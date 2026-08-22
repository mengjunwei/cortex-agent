//! 组合根（Composition Root）— 装配跨切服务并构造 [`AppDeps`]。
//!
//! 本模块是项目**唯一**的依赖装配点（见架构 §3 Q6、§5）。所有跨业务簇的共享服务
//! 都在 [`build_app_deps`] 中初始化，通过 [`AppDeps`] 显式注入，
//! **禁止** 进程级全局变量（见 §5.4，取代了历史 `model_provider::GLOBAL_STORE`）。
//!
//! ## 装配顺序
//!
//! 装配顺序敏感（后置依赖前置）：
//!
//! 1. 数据库连接池（致命：失败即退出）
//! 2. 文档元数据存储 + 知识管理器 + 设备目录缓存（基础设施，带降级）
//! 3. Session / Artifact / Memory（adk-rust 三大服务，各自带降级）
//! 4. ModelProviderStore（DB 模型解析，必须早于 query_understanding）
//! 5. QueryUnderstanding（依赖模型解析）
//! 6. Plugin / Browser / Redis / Session*Store / Auth / Assistant / MCP / Skill
//! 7. run_registry / brainstorm semaphore（路由级运行时状态）

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::AppConfig;
use crate::domain::auth::AuthService;
use crate::domain::device_catalog::CatalogCache;
use crate::domain::knowledge::KnowledgeManager;
use crate::domain::mcp::McpManager;
use crate::domain::memory::{MemoryProposalStore, MemoryStore};
use crate::domain::session::SessionSettingsStore;
use crate::infra::db::DbPool;
use crate::infra::redis::SharedRedisPool;
use crate::domain::monitor::PluginManager;
use crate::domain::skill::SkillService;
use adk_rust::session::SessionService;

mod init;

// ========================================================================
//  AppDeps
// ========================================================================

/// 全局应用依赖 — 通过 Axum `State` 与 GraphQL `Context` 共享给所有 handler / resolver。
///
/// 持有所有共享服务的 `Arc` 引用。装配逻辑集中在 [`build_app_deps`]，
/// 不再使用进程级全局变量（取代了历史 `model_provider::GLOBAL_STORE`）。
///
/// ## 设计原则
///
/// - 跨业务簇（≥2 个无关模块）读取的服务才进 `AppDeps`；仅单簇内使用的依赖
///   不应放入（见架构 §5.3「新增依赖时的判定流程」）。
/// - 字段以 `Option<Arc<...>>` 形式承载可降级服务（DB 不可用时为 `None`）。
/// - 当字段数 ≥ 10 时应按业务簇拆子 struct（架构 §5.3 Level 3）。
pub struct AppDeps {
    pub config: AppConfig,
    pub adk_session_service: Arc<dyn SessionService>,
    pub artifact_service: Option<Arc<dyn adk_rust::artifact::ArtifactService>>,
    pub memory_service: Option<Arc<dyn adk_rust::Memory>>,
    /// 会话运行注册表：per-session 活跃 run（唯一，忙拒绝）+ steer 队列（运行中提交的
    /// 输入，agent 主循环下轮模型请求前注入）。取代旧 `cancellation_tokens` 裸 map——
    /// 后者 insert 即覆盖，并发 run 时旧 run 的取消入口直接丢失。进程内存态（重启清空）。
    pub run_registry: Arc<crate::infra::run_registry::RunRegistry>,
    /// 会话级 token 用量累计最大值（thread_id → 已观测最大 total_tokens）。
    ///
    /// 对齐 codex 的会话级 `token_info` 累计语义：SSE 的 `EventSink`/`budget` 都是
    /// per-run（每轮新建），无跨轮状态；若每轮从头算 usage，跨轮会「回退」。
    /// 这里在进程级维护每个会话的累计最大值，跨轮持久、仅在压缩（`on_compaction`）时清零，
    /// 供 `emit_usage` 上报单调值。进程内存（重启清空）；按 thread_id(UUID) 隔离，条目极小故不主动回收。
    pub session_token_usage: Arc<Mutex<HashMap<String, u64>>>,
    /// 会话级软着陆窗口状态（thread_id → 窗口快照句柄）。
    ///
    /// 对齐 codex `SessionState.auto_compact_window`：软着陆的 remind/borrow flag
    /// 「每窗一次」跨 run（用户轮次）存活，仅压缩开新窗时复位。agent 侧无 AppState
    /// 访问权，由 SSE 层按 thread_id 取句柄经 builder 注入；子 agent 不注入。
    /// 进程内存（重启清空，最多多一次 remind/borrow，可接受）。
    pub session_window_state:
        Arc<Mutex<HashMap<String, crate::agent::cortex::SharedWindowState>>>,
    pub knowledge_manager: Arc<KnowledgeManager>,
    pub catalog: Arc<CatalogCache>,
    pub query_understanding: Arc<crate::agent::query_understanding::QueryUnderstandingService>,
    /// 监控插件管理器（内置 Rhai 引擎，进程内运行）
    pub plugin_manager: Arc<PluginManager>,
    /// 数据库连接池（供 lookup_device_id 等工具使用）
    pub db_pool: Option<DbPool>,
    /// 模型供应商存储（DB 模型管理的唯一数据源；DB 不可用时为 None）
    pub model_provider_store: Option<Arc<crate::domain::model_provider::store::ModelProviderStore>>,
    /// 会话级配置合并存储（session_settings：标题/模型/思考级别/沙箱审批/助手绑定）
    pub session_settings_store: Option<Arc<SessionSettingsStore>>,
    /// Redis 连接池（供 snmp_test_collect 等工具使用）
    pub redis_pool: Option<SharedRedisPool>,
    /// OID 缓存（高性能 API 使用）
    pub oid_cache: Arc<dashmap::DashMap<String, bytes::Bytes>>,
    /// 认证（SSO）服务（未启用 / 初始化失败时为 None）
    pub auth: Option<Arc<AuthService>>,
    /// 自定义助手存储（DB 不可用时为 None，前端只读内置 seed）
    pub assistant_store: Option<Arc<crate::domain::assistant::AssistantStore>>,
    /// MCP Server 管理器（连接池 + 健康探测；DB 不可用时为 None）
    pub mcp_manager: Option<Arc<McpManager>>,
    /// 新版文件系统 Skill 服务(Codex 风格,渐进式披露)
    pub skill_service: Option<Arc<SkillService>>,
    /// Shell 命令审批注册表（全局共享）
    pub shell_approval_registry: Arc<crate::server::shell_approval::ShellApprovalRegistry>,
    /// Shell 权限规则存储（DB 不可用时为 None）
    pub shell_rule_store: Option<Arc<crate::domain::shell_rules::ShellRuleStore>>,
    /// 跨会话记忆存储（已确认记忆，注入 prompt；DB 不可用时为 None）
    pub memory_store: Option<Arc<MemoryStore>>,
    /// 记忆建议存储（agent 通过 propose_memory 产出，待用户确认；DB 不可用时为 None）
    pub memory_proposal_store: Option<Arc<MemoryProposalStore>>,
    /// 审计日志存储（DB 不可用时为 None，调用方静默跳过）
    pub audit_store: Option<Arc<crate::domain::audit::AuditStore>>,
    /// 对象存储(S3/RustFS)客户端;未启用或初始化失败时为 None
    pub object_store: Option<Arc<crate::infra::object_store::ObjectStore>>,
    /// 定时任务业务存储（DB 不可用时为 None）
    pub scheduled_task_store: Option<Arc<crate::domain::scheduled_task::ScheduledTaskStore>>,
    /// 定时任务调度引擎（tokio-cron-scheduler）。OnceCell：AppState 在 server::run 才 Arc 化，
    /// 引擎又依赖 Arc<AppState>，故 bootstrap 建空 OnceCell、server::run 里 set 启动后的引擎。
    /// handler 经 `scheduler()` 访问，未初始化返回 None（降级，不影响其它功能）。
    pub scheduler: Arc<tokio::sync::OnceCell<Arc<crate::server::scheduled_task::SchedulerEngine>>>,
}

impl AppDeps {
    /// 访问调度引擎（未启动返回 None）。
    pub fn scheduler(&self) -> Option<Arc<crate::server::scheduled_task::SchedulerEngine>> {
        self.scheduler.get().cloned()
    }
}

impl AppDeps {
    /// 取得模型供应商存储；DB 不可用时返回错误（调用方据此降级或报错）。
    ///
    /// 取代了历史 `model_provider::global_store()` 全局访问。
    pub fn require_model_store(
        &self,
    ) -> anyhow::Result<&Arc<crate::domain::model_provider::store::ModelProviderStore>> {
        self.model_provider_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("模型供应商存储未初始化，请检查数据库是否启用并完成模型配置")
        })
    }
}

// ========================================================================
//  装配入口
// ========================================================================

/// 装配所有跨切服务，构造 [`AppDeps`]。
///
/// 装配顺序见模块级文档。任一非关键步骤失败均降级（记日志、置 `None`），
/// 仅数据库连接 / 模型解析等致命错误向上抛出。
pub async fn build_app_deps(cfg: AppConfig) -> anyhow::Result<AppDeps> {
    // ── 1. 数据库连接池（致命）──
    // `init_db` 返回 `DbPool`（非 Option）；doc_meta_store / catalog 直接使用，
    // 之后再包成 `Option<DbPool>` 供后续依赖 DB 的服务（带降级）使用。
    let db_pool = crate::infra::db::init_db(&cfg.db).await?;

    // ── 2. 知识库相关存储（多 provider 新表）+ 设备目录缓存 ──
    let document_store = Arc::new(
        crate::domain::knowledge::document_store::DocumentStore::new(db_pool.clone()).await?,
    );
    let chunk_store =
        Arc::new(crate::domain::knowledge::chunk_store::ChunkStore::new(db_pool.clone()).await?);
    let kb_instance_store = Arc::new(
        crate::domain::knowledge::kb_instance_store::KbInstanceStore::new(db_pool.clone()).await?,
    );
    let catalog = match CatalogCache::new(db_pool.clone()).await {
        Ok(c) => {
            tracing::info!("[bootstrap] CatalogCache 初始化成功");
            c
        }
        Err(e) => {
            tracing::warn!(
                "[bootstrap] CatalogCache 初始化失败({})，设备运维将使用空目录",
                e
            );
            CatalogCache::new_empty(db_pool.clone())
        }
    };

    // 之后所有依赖 DB 的服务统一以 `Option<DbPool>` 形式判空（DB 不可用时降级为 None）。
    let db_pool: Option<DbPool> = Some(db_pool);

    // boot 期密钥轮换：把历史密钥加密的密文 re-wrap 到活动密钥（幂等，仅多密钥时实质生效）。
    // 必须在各 store 构造（缓存装载）之前，使缓存从已轮换数据装载。失败非致命——多密钥
    // 解密在运行时仍可解历史密文，故仅告警并继续启动。详见 [`crate::security::reencrypt`]。
    if let Some(ref pool) = db_pool {
        if let Err(e) = crate::security::reencrypt::reencrypt_all(pool).await {
            tracing::error!(
                "[bootstrap] re-encrypt 扫描失败（已跳过；历史密文运行时仍可多密钥解密）: {e}"
            );
        }
    }

    // ── 3. adk session service（PostgreSQL，失败降级 InMemory）──
    let adk_session_service: Arc<dyn SessionService> = match init::init_session_service(&cfg).await {
        Ok(s) => {
            tracing::info!("[infra] adk session service: PostgreSQL 持久化");
            s
        }
        Err(e) => {
            tracing::warn!(
                "[infra] Postgres session 初始化失败({})，降级为 InMemory",
                e
            );
            Arc::new(adk_rust::session::InMemorySessionService::new())
        }
    };

    // ── artifact service（文件系统，失败降级 InMemory）──
    let artifact_dir = cfg.artifact_dir();
    std::fs::create_dir_all(&artifact_dir).ok();
    let artifact_service: Option<Arc<dyn adk_rust::artifact::ArtifactService>> =
        match adk_rust::artifact::FileArtifactService::new(&artifact_dir) {
            Ok(s) => {
                tracing::info!("[infra] artifact service: File({})", artifact_dir.display());
                Some(Arc::new(s))
            }
            Err(e) => {
                tracing::warn!(
                    "[infra] FileArtifactService 初始化失败({})，降级为 InMemory",
                    e
                );
                Some(Arc::new(adk_rust::artifact::InMemoryArtifactService::new()))
            }
        };

    // ── 对象存储(S3/RustFS)—— 截图/上传图/artifact/沙箱快照共用 ──
    let object_store = if cfg.object_storage.enabled {
        match crate::infra::object_store::ObjectStore::new(&cfg.object_storage).await {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::error!(
                    "[infra] 对象存储初始化失败({}) — 截图/上传图/沙箱快照将不可用",
                    e
                );
                None
            }
        }
    } else {
        tracing::warn!("[infra] 对象存储未启用(enabled=false)");
        None
    };

    // ── memory service（Redis，失败降级 InMemory）──
    let memory_service: Option<Arc<dyn adk_rust::Memory>> = match init::init_memory_service(&cfg).await {
        Ok(s) => {
            tracing::info!("[infra] memory service: Redis 持久化");
            Some(s)
        }
        Err(e) => {
            tracing::warn!("[infra] Redis memory 初始化失败({})，降级为 InMemory", e);
            Some(Arc::new(adk_rust::memory::MemoryServiceAdapter::new(
                Arc::new(adk_rust::memory::InMemoryMemoryService::new()),
                "cortex-agent",
                "user",
            )))
        }
    };

    // ── 4. 模型供应商存储（DB 模型管理唯一数据源；不再注册全局）──
    let model_provider_store = match &db_pool {
        Some(pool) => {
            match crate::domain::model_provider::store::ModelProviderStore::new(pool.clone()).await {
                Ok(store) => Some(store),
                Err(e) => {
                    tracing::warn!(
                        "[bootstrap] 模型供应商存储初始化失败({})，模型解析将不可用",
                        e
                    );
                    None
                }
            }
        }
        None => {
            tracing::warn!("[bootstrap] 数据库未启用，模型供应商管理不可用");
            None
        }
    };

    // ── 4.5 知识管理器（多 provider 路由；依赖 model_provider_store 解析 embedding 模型）──
    let kb_codec = crate::security::crypto::AesCodec::from_secrets();
    let knowledge_manager = {
        let model_store = model_provider_store
            .clone()
            .ok_or_else(|| anyhow::anyhow!("模型供应商存储未初始化，无法装配知识库路由"))?;
        Arc::new(KnowledgeManager::new(
            Arc::new(cfg.clone()),
            kb_instance_store.clone(),
            model_store,
            document_store.clone(),
            chunk_store.clone(),
            kb_codec,
        ))
    };

    // ── 5. query_understanding（依赖模型解析；模型不可用则启动失败）──
    // boot 期无用户上下文，用系统桶（user_id=""）模型兜底；系统桶空（如管理员删了种子模型）
    // 时回退任意已启用模型，避免启动失败（见 make_model_boot）。运行时按请求归属人解析的
    // QueryUnderstandingService 在 device_search / agent 构建时另行创建（隔离 API Key）。
    let query_understanding = {
        let store = model_provider_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("模型供应商存储未初始化，无法初始化 query_understanding 服务")
        })?;
        let model = crate::llm::make_model_boot(store)?;
        Arc::new(crate::agent::query_understanding::QueryUnderstandingService::new(model, 500))
    };

    // ── 6. plugin manager ──
    let plugin_manager = if let Some(ref pool) = db_pool {
        let plugin_store =
            Arc::new(crate::domain::monitor::plugin_store::PluginStore::new(pool.clone()).await?);
        let mgr = PluginManager::with_store(plugin_store);
        if let Err(e) = mgr.load_from_db().await {
            tracing::warn!("[bootstrap] 从数据库加载插件失败: {e}");
        }
        Arc::new(mgr)
    } else {
        Arc::new(PluginManager::new())
    };

    // ── OID 缓存（高性能 API 使用）──
    let oid_cache = Arc::new(dashmap::DashMap::with_capacity(10000));

    // ── Redis 连接池（供 snmp_test_collect 等工具使用）──
    let redis_url = cfg.redis.url();
    let redis_pool = match crate::infra::redis::init_redis(redis_url).await {
        Ok(pool) => {
            tracing::info!("[infra] Redis 连接池初始化成功");
            Some(pool)
        }
        Err(e) => {
            tracing::warn!("[infra] Redis 连接池初始化失败({})，SNMP 采集工具不可用", e);
            None
        }
    };

    // ── 会话级配置合并存储（session_settings）──
    let session_settings_store = match &db_pool {
        Some(pool) => match SessionSettingsStore::new(pool.clone()).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("[bootstrap] 会话级配置存储初始化失败({})", e);
                None
            }
        },
        None => None,
    };

    // ── 跨会话记忆存储（memories + memory_proposals）──
    let memory_store = match &db_pool {
        Some(pool) => match MemoryStore::new(pool.clone()).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("[bootstrap] 记忆存储初始化失败({})", e);
                None
            }
        },
        None => None,
    };
    let memory_proposal_store = match &db_pool {
        Some(pool) => match MemoryProposalStore::new(pool.clone()).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("[bootstrap] 记忆建议存储初始化失败({})", e);
                None
            }
        },
        None => None,
    };

    // ── 认证服务（始终启用，数据库可用即生效；首个注册用户自动管理员）──
    // 认证不可关闭：已移除 enabled 开关，避免被误触关闭。DB 不可用时降级为 None。
    let auth_service = match &db_pool {
        Some(pool) => match init::init_auth_service(&cfg, pool.clone(), redis_pool.clone()).await {
            Ok(svc) => {
                let p = cfg.auth.providers.len();
                tracing::info!(
                    "[infra] 认证服务初始化成功（本地登录已启用{})",
                    if p > 0 {
                        format!("，{p} 个 SSO provider")
                    } else {
                        String::new()
                    }
                );
                Some(svc)
            }
            Err(e) => {
                tracing::warn!("[infra] 认证服务初始化失败({})，登录功能不可用", e);
                None
            }
        },
        None => {
            tracing::warn!("[infra] 数据库未启用，认证服务不可用");
            None
        }
    };

    // ── 助手存储 ──
    // env_vars 静态加密 codec（密钥内置代码，与 KnowledgeManager / model_provider 共享 APP_SECRETS）
    let assistant_codec = crate::security::crypto::AesCodec::from_secrets();
    let assistant_store = match &db_pool {
        Some(pool) => {
            match crate::domain::assistant::AssistantStore::new(pool.clone(), assistant_codec).await
            {
                Ok(s) => {
                    tracing::info!("[infra] 自定义助手存储初始化成功");
                    Some(s)
                }
                Err(e) => {
                    tracing::warn!(
                        "[infra] 自定义助手存储初始化失败({})，自定义助手功能不可用",
                        e
                    );
                    None
                }
            }
        }
        None => {
            tracing::warn!("[infra] 数据库未启用，自定义助手存储不可用");
            None
        }
    };

    // ── MCP 管理器（Store + 连接池 + 健康探测）──
    let mcp_manager = match &db_pool {
        Some(pool) => match crate::domain::mcp::store::McpServerStore::new(pool.clone()).await {
            Ok(store) => match McpManager::new(store, cfg.mcp.stdio_inherit_env).await {
                Ok(mgr) => {
                    tracing::info!("[infra] MCP 管理器初始化成功");
                    Some(mgr)
                }
                Err(e) => {
                    tracing::warn!("[infra] MCP 管理器初始化失败({})", e);
                    None
                }
            },
            Err(e) => {
                tracing::warn!("[infra] MCP Store 初始化失败({})", e);
                None
            }
        },
        None => {
            tracing::warn!("[infra] 数据库未启用，MCP 管理功能不可用");
            None
        }
    };

    // ── MCP 种子服务器 upsert（从 config 自动注册预配置 MCP 服务器）──
    if !cfg.mcp.seeds.is_empty() {
        if let Some(ref mgr) = mcp_manager {
            let seeds = &cfg.mcp.seeds;
            tracing::info!("[infra] 开始 upsert {} 个 MCP 种子服务器", seeds.len());
            for seed in seeds {
                if let Err(e) = init::upsert_mcp_seed(mgr, seed).await {
                    tracing::warn!("[infra] MCP 种子 upsert 失败 (slug={}): {}", seed.slug, e);
                }
            }
        } else {
            tracing::warn!(
                "[infra] MCP 管理器不可用，跳过 {} 个种子服务器",
                cfg.mcp.seeds.len()
            );
        }
    }

    // ── 启动 MCP 健康探测（须在种子 upsert 之后：首轮探测读 DB，种子未入库则读到旧 endpoint）──
    if let Some(ref mgr) = mcp_manager {
        mgr.start_probe_loop();
    }

    // ── Skill 服务(文件系统,Codex 风格,渐进式披露)──
    let skill_service = match SkillService::new(cfg.skill_dir()) {
        Ok(svc) => {
            tracing::info!("[infra] Skill 服务初始化成功");
            Some(Arc::new(svc))
        }
        Err(e) => {
            tracing::warn!("[infra] Skill 服务初始化失败({})", e);
            None
        }
    };

    // ── Shell 权限规则存储 ──
    let shell_rule_store = match &db_pool {
        Some(pool) => match crate::domain::shell_rules::ShellRuleStore::new(pool.clone()).await {
            Ok(s) => {
                tracing::info!("[infra] Shell 权限规则存储初始化成功");
                Some(s)
            }
            Err(e) => {
                tracing::warn!("[infra] Shell 权限规则存储初始化失败: {}", e);
                None
            }
        },
        None => None,
    };

    // ── 审计日志存储 ──
    let audit_store = match &db_pool {
        Some(pool) => match crate::domain::audit::AuditStore::new(pool.clone()).await {
            Ok(s) => {
                tracing::info!("[infra] 审计日志存储初始化成功");
                Some(s)
            }
            Err(e) => {
                tracing::warn!("[infra] 审计日志存储初始化失败: {}", e);
                None
            }
        },
        None => None,
    };

    // 探测运行环境（python/node/npm 全局包/git），缓存供 system prompt 注入。
    // 让 agent 启动即知可用 runtime，避免「pip install 失败 → 改用 node」式试错。
    crate::agent::cortex::env_probe::init().await;

    // ── 定时任务业务存储 ──
    let scheduled_task_store = match &db_pool {
        Some(pool) => {
            let s = crate::domain::scheduled_task::ScheduledTaskStore::new(pool.clone());
            tracing::info!("[infra] 定时任务存储初始化成功");
            Some(s)
        }
        None => None,
    };

    Ok(AppDeps {
        config: cfg,
        adk_session_service,
        artifact_service,
        memory_service,
        run_registry: Arc::new(crate::infra::run_registry::RunRegistry::new()),
        session_token_usage: Arc::new(Mutex::new(HashMap::new())),
        session_window_state: Arc::new(Mutex::new(HashMap::new())),
        knowledge_manager,
        catalog,
        query_understanding,
        plugin_manager,
        db_pool,
        model_provider_store,
        session_settings_store,
        redis_pool,
        oid_cache,
        auth: auth_service,
        assistant_store,
        mcp_manager,
        skill_service,
        shell_approval_registry: crate::server::shell_approval::ShellApprovalRegistry::new(),
        shell_rule_store,
        memory_store,
        memory_proposal_store,
        audit_store,
        object_store,
        scheduled_task_store,
        // 调度引擎在 server::run 中启动（此时 AppState 尚未 Arc 化，无法持有自身引用）。
        scheduler: Arc::new(tokio::sync::OnceCell::new()),
    })
}

