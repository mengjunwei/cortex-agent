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
//! 7. cancellation_tokens / brainstorm semaphore（路由级运行时状态）

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::domain::auth::{ApiTokenStore, AuthService, JwtService, ProviderRegistry, UserStore};
use crate::domain::device_catalog::CatalogCache;
use crate::domain::knowledge::KnowledgeManager;
use crate::domain::mcp::McpManager;
use crate::domain::memory::{MemoryProposalStore, MemoryStore};
use crate::domain::session::SessionSettingsStore;
use crate::infra::db::DbPool;
use crate::infra::redis::SharedRedisPool;
use crate::monitor::PluginManager;
use crate::skill::SkillService;
use adk_rust::session::SessionService;

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
    pub cancellation_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
    pub knowledge_manager: Arc<KnowledgeManager>,
    pub catalog: Arc<CatalogCache>,
    pub query_understanding: Arc<crate::agent::query_understanding::QueryUnderstandingService>,
    /// 监控插件管理器（内置 Rhai 引擎，进程内运行）
    pub plugin_manager: Arc<PluginManager>,
    /// 数据库连接池（供 lookup_device_id 等工具使用）
    pub db_pool: Option<DbPool>,
    /// 模型供应商存储（DB 模型管理的唯一数据源；DB 不可用时为 None）
    pub model_provider_store: Option<Arc<crate::model_provider::store::ModelProviderStore>>,
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
}

impl AppDeps {
    /// 取得模型供应商存储；DB 不可用时返回错误（调用方据此降级或报错）。
    ///
    /// 取代了历史 `model_provider::global_store()` 全局访问。
    pub fn require_model_store(
        &self,
    ) -> anyhow::Result<&Arc<crate::model_provider::store::ModelProviderStore>> {
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

    // ── 3. adk session service（PostgreSQL，失败降级 InMemory）──
    let adk_session_service: Arc<dyn SessionService> = match init_session_service(&cfg).await {
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
    let memory_service: Option<Arc<dyn adk_rust::Memory>> = match init_memory_service(&cfg).await {
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
            match crate::model_provider::store::ModelProviderStore::new(pool.clone(), &cfg.security)
                .await
            {
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
    let kb_codec =
        crate::model_provider::crypto::codec_from_security(&cfg.security, "KnowledgeManager");
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
    let query_understanding = {
        let store = model_provider_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("模型供应商存储未初始化，无法初始化 query_understanding 服务")
        })?;
        let model = crate::llm::make_model(store)?;
        Arc::new(crate::agent::query_understanding::QueryUnderstandingService::new(model, 500))
    };

    // ── 6. plugin manager ──
    let plugin_manager = if let Some(ref pool) = db_pool {
        let plugin_store =
            Arc::new(crate::monitor::plugin_store::PluginStore::new(pool.clone()).await?);
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
        Some(pool) => match init_auth_service(&cfg, pool.clone(), redis_pool.clone()).await {
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
    let assistant_store = match &db_pool {
        Some(pool) => match crate::domain::assistant::AssistantStore::new(pool.clone()).await {
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
        },
        None => {
            tracing::warn!("[infra] 数据库未启用，自定义助手存储不可用");
            None
        }
    };

    // ── MCP 管理器（Store + 连接池 + 健康探测）──
    let mcp_manager = match &db_pool {
        Some(pool) => {
            match crate::domain::mcp::store::McpServerStore::new(pool.clone(), &cfg.security).await
            {
                Ok(store) => match McpManager::new(store).await {
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
            }
        }
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
                if let Err(e) = upsert_mcp_seed(mgr, seed).await {
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

    Ok(AppDeps {
        config: cfg,
        adk_session_service,
        artifact_service,
        memory_service,
        cancellation_tokens: Arc::new(Mutex::new(HashMap::new())),
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
    })
}

// ========================================================================
//  初始化辅助（从历史 server/mod.rs 迁入，集中到组合根）
// ========================================================================

async fn init_session_service(cfg: &AppConfig) -> anyhow::Result<Arc<dyn SessionService>> {
    tracing::info!("[infra] 初始化 PostgreSQL Session 服务...");
    tracing::debug!(
        "[infra] Session 数据库 URL: postgres://***@{}:{}/{}",
        cfg.db.host,
        cfg.db.port,
        cfg.db.db
    );

    let pg_service = adk_rust::session::PostgresSessionService::new(&cfg.db.url()).await?;
    tracing::info!("[infra] PostgreSQL Session 服务创建成功，开始迁移...");

    match tokio::time::timeout(std::time::Duration::from_secs(30), pg_service.migrate()).await {
        Ok(Ok(_)) => {
            tracing::info!("[infra] PostgreSQL Session 迁移完成");
        }
        Ok(Err(e)) => {
            tracing::warn!("[infra] PostgreSQL Session 迁移失败({})，继续启动", e);
        }
        Err(_) => {
            tracing::warn!("[infra] PostgreSQL Session 迁移超时(>1s)，继续启动");
        }
    }

    Ok(Arc::new(pg_service))
}

async fn init_memory_service(cfg: &AppConfig) -> anyhow::Result<Arc<dyn adk_rust::Memory>> {
    let redis_config = adk_rust::memory::redis::RedisMemoryConfig {
        url: cfg.redis.url(),
        ttl: None,
    };
    let redis_service = adk_rust::memory::redis::RedisMemoryService::new(redis_config).await?;
    let adapter = adk_rust::memory::MemoryServiceAdapter::new(
        Arc::new(redis_service),
        "cortex-agent",
        "user",
    );
    Ok(Arc::new(adapter))
}

/// 初始化认证（SSO）服务
///
/// 装配链路：
/// 1. `UserStore` — 建表 `users` / `user_identities`
/// 2. `AesCodec` — 解密 provider 的 `client_secret`（密钥派生逻辑与模型供应商一致，便于统一运维）
/// 3. `reqwest::Client` — OAuth 出站 HTTP（token / userinfo endpoint）
/// 4. `ProviderRegistry` — 遍历 `[[auth.providers]]` 全部实例化
/// 5. `JwtService` — HS256，密钥长度由其自身校验
/// 6. `AuthService` — 编排以上组件 + Redis 黑名单
///
/// 任一步骤失败均向上抛出错误，由调用方决定降级策略。
async fn init_auth_service(
    cfg: &AppConfig,
    pool: DbPool,
    redis: Option<SharedRedisPool>,
) -> anyhow::Result<Arc<AuthService>> {
    // 1. UserStore + ApiTokenStore（用户主表 / 身份绑定 / API Token；建表 DDL 在 schema.sql）
    let users = UserStore::new(pool.clone()).await?;
    let api_tokens = ApiTokenStore::new(pool).await?;

    // 2. AesCodec（与模型供应商共享同一密钥派生逻辑）
    let aes_raw = std::env::var("MODEL_AES_KEY").unwrap_or_else(|_| cfg.security.aes_key.clone());
    if aes_raw.trim().is_empty() {
        tracing::warn!(
            "[Auth] 未配置 [security].aes_key，已生成临时 AES 密钥。\
             重启后历史加密的 client_secret 将无法解密，生产环境请务必固定密钥。"
        );
    }
    let aes = crate::model_provider::crypto::AesCodec::from_passphrase(&aes_raw);

    // 3. HTTP 客户端（OAuth 调用第三方 token/userinfo endpoint）
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    // 4. ProviderRegistry（遍历 [[auth.providers]] 全部实例化）
    let registry = ProviderRegistry::from_config(&cfg.auth.providers, http, &aes)?;

    // 5. JwtService（密钥 < 32 字节时返回配置错误）。
    //    若未配置 jwt_secret 则生成临时密钥（重启后所有旧会话失效，仅适用开发环境）。
    let jwt_secret = if cfg.auth.jwt_secret.trim().is_empty() {
        let mut tmp = uuid::Uuid::now_v7().to_string();
        tmp.push_str(&uuid::Uuid::now_v7().to_string());
        tracing::warn!(
            "[Auth] 未配置 [auth].jwt_secret，已生成临时密钥。\
             重启后所有已签发的会话将失效，生产环境请务必固定 jwt_secret（≥32 字节）。"
        );
        tmp
    } else {
        cfg.auth.jwt_secret.clone()
    };
    let jwt = JwtService::new(&jwt_secret, cfg.auth.token_ttl_secs)?;

    // 6. AuthService（编排以上组件 + Redis 黑名单）
    let svc = AuthService::new(
        users,
        api_tokens,
        Arc::new(registry),
        Arc::new(jwt),
        redis,
        cfg.auth.token_ttl_secs,
        cfg.auth.cookie_name.clone(),
    );

    Ok(Arc::new(svc))
}

/// Upsert MCP 种子服务器到 DB（按 slug 匹配，存在则更新 endpoint/args，不存在则创建）
async fn upsert_mcp_seed(
    mgr: &crate::domain::mcp::McpManager,
    seed: &crate::config::McpSeedConfig,
) -> anyhow::Result<()> {
    use crate::infra::store_base::Store;
    use diesel_async::RunQueryDsl;

    let store = mgr.store();
    let mut conn = store.get_conn().await?;

    let id = uuid::Uuid::now_v7().to_string();
    let now = chrono::Utc::now();

    diesel::sql_query(
        r#"INSERT INTO mcp_servers (id, name, slug, transport, endpoint, args, env_enc, env_mask, headers_enc, headers_mask, status, tool_timeout_secs, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, '', '{}', '', '{}', 1, $8, $7, $7)
           ON CONFLICT (slug) DO UPDATE SET
             name = EXCLUDED.name,
             endpoint = EXCLUDED.endpoint,
             args = EXCLUDED.args,
             transport = EXCLUDED.transport,
             tool_timeout_secs = EXCLUDED.tool_timeout_secs,
             updated_at = EXCLUDED.updated_at"#,
    )
    .bind::<diesel::sql_types::VarChar, _>(&id)
    .bind::<diesel::sql_types::VarChar, _>(&seed.name)
    .bind::<diesel::sql_types::VarChar, _>(&seed.slug)
    .bind::<diesel::sql_types::SmallInt, _>(seed.transport)
    .bind::<diesel::sql_types::VarChar, _>(&seed.endpoint)
    .bind::<diesel::sql_types::Text, _>(&seed.args)
    .bind::<diesel::sql_types::Timestamptz, _>(now)
    .bind::<diesel::sql_types::Int4, _>(seed.tool_timeout_secs.unwrap_or(60) as i32)
    .execute(&mut *conn)
    .await?;

    tracing::info!(
        "[infra] MCP 种子 upsert 成功: slug={}, name={}",
        seed.slug,
        seed.name
    );
    Ok(())
}
