//! 自定义助手构建器 + 会话级 Agent 分发器
//!
//! 主要功能：
//! - [`build_custom_agent`]：从 [`Assistant`] 构建通用 LlmAgent
//! - [`build_agent_for_session`]：会话运行时入口，根据助手记录路由到内置或自定义
//!
//! 分发规则：
//! - `Custom` 助手 → `build_custom_agent`（使用 DB 中的配置）
//! - `Builtin` + 有效 agent_type → `build_builtin`（内置专用 Agent）

use std::sync::Arc;

use adk_rust::agent::Agent;
use tokio_util::sync::CancellationToken;

use crate::agent::{device_command, monitor_plugin, runtime::workspace};
use crate::config::AppConfig;
use crate::domain::assistant::enums::AssistantKind;
use crate::domain::assistant::models::Assistant;
use crate::domain::device_catalog::CatalogCache;
use crate::domain::knowledge::KnowledgeManager;
use crate::domain::permissions::PermissionPolicy;
use crate::infra::db::DbPool;
use crate::infra::object_store::ObjectStore;
use crate::infra::redis::SharedRedisPool;
use crate::llm::{make_gen_config_from, make_model_and_meta, make_model_by_id};
use crate::model_provider::store::ModelProviderStore;
use crate::monitor::PluginManager;

use crate::agent::runtime::cortex_agent::CortexAgentBuilder;
use crate::tools::shell_command::ShellToolDeps;

/// 按 `enabled_tools` 白名单注入工具
#[allow(clippy::too_many_arguments)]
fn push_tool_for_key(
    builder: CortexAgentBuilder,
    key: &str,
    assistant: &Assistant,
    knowledge: Option<Arc<KnowledgeManager>>,
    catalog: Option<Arc<CatalogCache>>,
    model_store: Option<&ModelProviderStore>,
    shell_deps: Option<&Arc<ShellToolDeps>>,
) -> CortexAgentBuilder {
    match key {
        "search_kb" if assistant.kb_instance_id.is_some() && knowledge.is_some() => {
            let km = knowledge.clone().unwrap();
            let model = model_store.and_then(|s| make_model_by_id(s, None).ok());
            if let Some(m) = model {
                let qu = Arc::new(
                    crate::agent::query_understanding::QueryUnderstandingService::new(m, 500),
                );
                builder.tool(Arc::new(crate::tools::device_command::create_search_tool(
                    km,
                    qu,
                    assistant.kb_instance_id.clone(),
                )))
            } else {
                tracing::warn!(
                    "[custom] search_kb 跳过：模型不可用，无法初始化 query_understanding"
                );
                builder
            }
        }
        "query_device_catalog" if catalog.is_some() => builder.tool(Arc::new(
            crate::tools::device_command::create_catalog_tool(catalog.clone().unwrap()),
        )),
        "shell_command" if shell_deps.is_some() => builder.tool(Arc::new(
            crate::tools::shell_command::create_shell_command_tool(shell_deps.unwrap().clone()),
        )),
        _ => builder,
    }
}

/// 规范化 Agent 名称：全角空格转半角 + trim。
///
/// 统一处理中文名里常见的全角空格 `　`、前后导/尾随空格，
/// 保证注册名干净稳定。
fn normalize_agent_name(name: &str) -> String {
    name.replace('\u{3000}', " ") // 全角空格 → 半角
        .trim()
        .to_string()
}

/// 从 [`Assistant`] 构建自定义 Agent（使用 DB 中的配置）。
#[allow(clippy::too_many_arguments)]
#[allow(private_interfaces)]
pub fn build_custom_agent(
    cfg: &AppConfig,
    model_store: &ModelProviderStore,
    assistant: &Assistant,
    knowledge: Option<Arc<KnowledgeManager>>,
    catalog: Option<Arc<CatalogCache>>,
    mcp_toolsets: Vec<Arc<dyn adk_rust::Toolset>>,
    object_store: Option<Arc<ObjectStore>>,
    workspace_root: Option<Arc<std::path::PathBuf>>,
    skill_service: Option<Arc<crate::skill::SkillService>>,
    memory_proposal_store: Option<&std::sync::Arc<crate::domain::memory::MemoryProposalStore>>,
    model_id_override: Option<&str>,
    shell_deps: Option<Arc<ShellToolDeps>>,
    skill_bodies: Option<&str>,
    memory_block: Option<&str>,
    session_thinking_level: Option<&str>,
    policy: PermissionPolicy,
    cancel_token: CancellationToken,
    child_event_sink: Option<Arc<dyn crate::agent::runtime::cortex_agent::ChildEventSink>>,
) -> anyhow::Result<(Arc<dyn Agent>, Option<crate::agent::runtime::cortex_agent::SharedBudget>)> {
    let effective_model = model_id_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            if assistant.model_id.is_empty() {
                None
            } else {
                Some(assistant.model_id.clone())
            }
        });
    let (model, resolved) = make_model_and_meta(model_store, effective_model.as_deref())?;

    let gen_cfg = make_gen_config_from(
        assistant.max_tokens,
        assistant.temperature,
        assistant.top_p,
        session_thinking_level,
    );

    // 注入 skill 目录(分层注入,不拼进 instruction)
    let skill_catalog = if let Some(svc) = skill_service.as_ref() {
        let catalog = svc.render_catalog_block(cfg.skill.catalog_token_budget_pct);
        if catalog.is_empty() {
            None
        } else {
            Some(catalog)
        }
    } else {
        None
    };

    let agent_name = normalize_agent_name(&assistant.name);
    tracing::info!(
        "[custom] 注册 Agent name=\"{}\" bytes={:x?} (原始=\"{}\")",
        agent_name,
        agent_name.as_bytes(),
        assistant.name
    );
    // max_iterations 沿用 CortexAgent 默认值（80），不再硬编码 20——复杂多步任务易被过早截断
    let mut builder = crate::agent::runtime::cortex_agent::CortexAgentBuilder::new(&agent_name)
        .description(&assistant.description)
        .model(model)
        .generate_content_config(gen_cfg)
        .policy(policy)
        .cancel_token(cancel_token)
        .context_config(cfg.context.clone());
    // 子 agent 活动事件出口（SSE 转发；未注入则 builder 默认 Noop 不转发）
    if let Some(sink) = child_event_sink {
        builder = builder.child_event_sink(sink);
    }

    // 动态压缩阈值：注入模型 context_window。
    // None / 0 / 负值（DB 字段，管理员可填错）一律忽略 → 走 fallback_context_window，
    // 避免 0 导致每轮死磕压缩、负值(i32→usize 巨大)导致压缩永久禁用。
    if let Some(w) = resolved.context_window.filter(|&w| w > 0).map(|w| w as usize) {
        builder = builder.context_window(w);
    }
    // 压缩专用便宜模型（None=用主模型压缩）
    if let Some(cid) = cfg
        .context
        .compact_model_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        match make_model_by_id(model_store, Some(cid)) {
            Ok(cm) => builder = builder.compact_model(cm),
            Err(e) => tracing::warn!("[custom] compact_model_id={} 解析失败，回退主模型压缩: {e}", cid),
        }
    }

    // 用户配置的 system prompt（未填则不设 instruction — CortexAgent 内部以 BASE_INSTRUCTION
    // 作为通用基线始终注入，无需在此拼接）。CortexAgent 统一接管「特化指令 + BASE 基线」的注入，
    // 自定义 / 内置助手走同一套，像工具注入一样。
    let user_prompt = assistant.system_prompt.trim();
    if !user_prompt.is_empty() {
        builder = builder.instruction(user_prompt);
    }

    if let Some(ref catalog) = skill_catalog {
        builder = builder.skill_catalog(catalog);
    }

    // 用户 @ 提及的 skill 正文：注入 system prompt 侧（不进 user message，避免污染持久化/前端回显）
    if let Some(bodies) = skill_bodies {
        if !bodies.is_empty() {
            builder = builder.skill_bodies(bodies);
        }
    }

    // 跨会话记忆（用户的习惯/坑）：注入 stable prefix（紧贴人设，命中缓存）
    if let Some(mem) = memory_block {
        if !mem.is_empty() {
            builder = builder.memory_block(mem);
        }
    }

    // 注册助手声明的工具(不再收窄 — skill 不再绑定到 assistant)
    for key in &assistant.enabled_tools {
        builder = push_tool_for_key(
            builder,
            key.as_str(),
            assistant,
            knowledge.clone(),
            catalog.clone(),
            Some(model_store),
            shell_deps.as_ref(),
        );
    }

    // MCP Server 工具集注入（命名空间隔离：mcp__{slug}__{tool}）
    for ts in mcp_toolsets {
        let wrapped = crate::tools::wrap_toolset_with_truncation(
            Some(ts),
            cfg.context.tool_max_output_bytes,
            object_store.clone(),
        );
        if let Some(w) = wrapped {
            builder = builder.toolset(w);
        }
    }

    // 注册 read_skill 工具(常驻,让 LLM 主动拉取 skill 正文;与 $name 提及同款渲染,带 <path>)
    if let Some(svc) = skill_service.as_ref() {
        builder = builder.tool(Arc::new(crate::tools::skill_read::create_read_skill_tool(
            svc.clone(),
            cfg.skill.max_inject_chars,
        )));
    }

    // 文件操作工具(常驻,对所有 custom 助手可用)。
    // UI 没有"按助手配置工具"的入口,而这些读/编辑能力是基础工具——且已被 workspace 根路径
    // 关住、经 resolve_safe_path 防目录逃逸/符号链接攻击,故不走 enabled_tools 白名单,
    // 只要有会话 workspace 就注册。shell_command 等强能力工具仍保持白名单。
    if let Some(root) = workspace_root.as_ref() {
        builder = builder.tool(Arc::new(crate::tools::code::create_read_file_tool(root.clone())));
        builder = builder
            .tool(Arc::new(crate::tools::code::create_list_directory_tool(root.clone())));
        builder = builder.tool(Arc::new(crate::tools::code::create_grep_tool(root.clone())));
        builder = builder.tool(Arc::new(crate::tools::code::create_edit_file_tool(root.clone())));
        builder = builder
            .tool(Arc::new(crate::tools::code::create_create_file_tool(root.clone())));
    }

    // 注册 propose_memory 工具（常驻，让 LLM 主动提议记忆建议；用户在卡片上确认才记入长期记忆）
    if let Some(store) = memory_proposal_store {
        builder = builder.tool(Arc::new(
            crate::tools::propose_memory::create_propose_memory_tool(
                store.clone(),
                assistant.id.clone(),
            ),
        ));
    }

    let agent = builder
        .build()
        .map_err(|e| anyhow::anyhow!("创建自定义助手 Agent 失败: {}", e))?;
    // 装箱前取出 budget 只读句柄，供 SSE 层推 token 用量（对齐 codex token 显示）
    let budget = Some(agent.budget());
    Ok((Arc::new(agent) as Arc<dyn Agent>, budget))
}

/// 会话级共享上下文（基础设施依赖，生命周期与 AppState 一致）
pub struct AgentContext<'a> {
    pub cfg: &'a AppConfig,
    pub knowledge_manager: Arc<KnowledgeManager>,
    pub catalog: Arc<CatalogCache>,
    pub db_pool: Option<DbPool>,
    pub redis_pool: Option<SharedRedisPool>,
    pub plugin_manager: Option<Arc<PluginManager>>,
    /// Skill 服务(新版文件系统;未初始化时为 None → 无 skill 注入)
    pub skill_service: Option<Arc<crate::skill::SkillService>>,
    /// 记忆建议存储（propose_memory 工具写入；DB 不可用时为 None → 不注册该工具）
    pub memory_proposal_store: Option<std::sync::Arc<crate::domain::memory::MemoryProposalStore>>,
    /// 模型供应商存储（LLM 解析的唯一数据源；取代历史 `GLOBAL_STORE`）。
    /// DB 不可用时为 None，agent 构建时若调用 `require_model_store` 会返回错误。
    pub model_provider_store: Option<Arc<ModelProviderStore>>,
    /// 对象存储(S3/RustFS);截图工具结果上传用。未启用时为 None → 截图走普通截断
    pub object_store: Option<Arc<ObjectStore>>,
}

impl<'a> AgentContext<'a> {
    /// 取得模型供应商存储；DB 不可用时返回错误。
    pub fn require_model_store(&self) -> anyhow::Result<&Arc<ModelProviderStore>> {
        self.model_provider_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("模型供应商存储未初始化，请检查数据库是否启用并完成模型配置")
        })
    }
}

/// 会话级请求参数（单次对话级别的可变配置）
pub struct AgentRequest {
    pub model_id: Option<String>,
    pub workspace_mode: workspace::WorkspaceMode,
    pub mcp_toolsets: Vec<Arc<dyn adk_rust::Toolset>>,
    pub shell_deps: Option<Arc<ShellToolDeps>>,
    /// 用户在输入里 @ 提及的 skill 正文（注入 system prompt，不污染 user message）
    pub skill_bodies: Option<String>,
    /// 跨会话记忆块（用户习惯/坑，注入 stable prefix；由 SSE 用 user_id+assistant_id 预拉渲染）
    pub memory_block: Option<String>,
    /// 会话级思考级别（low/medium/high/xhigh/max；None=默认 high）
    pub session_thinking_level: Option<String>,
    /// 会话级审批策略（沙箱模式 + 审批策略 + 网络开关）；sse 按「会话级优先、回退全局 [shell]」算好后传入
    pub policy: PermissionPolicy,
    /// 本次运行的取消令牌（用户点停止时 cancel）；透传到 CortexAgent，run() 内工具执行 select! 监听
    pub cancel_token: CancellationToken,
    /// 子 agent 活动事件出口（SSE 转发 sink）；None=Noop，子 agent 活动不转发前端
    pub child_event_sink: Option<Arc<dyn crate::agent::runtime::cortex_agent::ChildEventSink>>,
}

/// 会话运行时入口：根据助手记录路由到对应 Agent
///
/// 统一接收 `AgentContext`（共享基础设施）+ `AgentRequest`（单次请求参数），
/// 避免函数签名随参数增加不断膨胀。
/// 返回 (Agent, 预算句柄)；内置 Agent 无预算 → None。预算句柄含 crate-private 类型，
/// 本函数仅供 crate 内 SSE 层调用，故允许 private_interfaces。
#[allow(private_interfaces)]
pub async fn build_agent_for_session(
    ctx: &AgentContext<'_>,
    assistant: &Assistant,
    req: AgentRequest,
) -> anyhow::Result<(Arc<dyn Agent>, Option<crate::agent::runtime::cortex_agent::SharedBudget>)> {
    // 模型供应商存储：所有后续 LLM 解析的唯一数据源（取代历史 GLOBAL_STORE）。
    // DB 不可用则 agent 构建失败。
    let model_store = ctx.require_model_store()?;

    // 会话级审批策略（Copy）：先取出再 destructure。自定义助手路径传给 build_custom_agent；
    // 内置助手路径不使用（内置助手维持 config 默认，少执行 shell）。
    let policy = req.policy;
    let AgentRequest {
        model_id,
        mcp_toolsets,
        shell_deps,
        skill_bodies,
        memory_block,
        session_thinking_level,
        cancel_token,
        workspace_mode,
        child_event_sink,
        ..
    } = req;
    // 沙箱根路径：仅 Sandbox 模式有，供 code 工具（read_file/edit_file/…）注册用
    let workspace_root: Option<Arc<std::path::PathBuf>> = match &workspace_mode {
        workspace::WorkspaceMode::Sandbox(p) => Some(Arc::new(p.clone())),
        workspace::WorkspaceMode::ChatOnly => None,
    };

    // 内置助手：走 build_builtin（各 agent_type 专用 Agent）
    if assistant.kind == AssistantKind::Builtin {
        let key = assistant.agent_type.dispatch_key();
        tracing::info!("[dispatch] builtin+{} → legacy", key);
        // 内置 Agent 非 CortexAgent，无 budget 句柄 → 返回 None（前端不显示 token 用量）
        let agent = build_builtin(
            ctx.cfg,
            model_store,
            key,
            ctx.knowledge_manager.clone(),
            ctx.catalog.clone(),
            model_id.as_deref(),
            session_thinking_level.as_deref(),
            ctx.db_pool.clone(),
            ctx.redis_pool.clone(),
            ctx.plugin_manager.clone(),
            assistant.kb_instance_id.as_deref(),
            cancel_token,
        )?;
        return Ok((agent, None));
    }

    // 自定义助手：build_custom_agent（使用 DB 中的配置）
    build_custom_agent(
        ctx.cfg,
        model_store,
        assistant,
        Some(ctx.knowledge_manager.clone()),
        Some(ctx.catalog.clone()),
        mcp_toolsets,
        ctx.object_store.clone(),
        workspace_root,
        ctx.skill_service.clone(),
        ctx.memory_proposal_store.as_ref(),
        model_id.as_deref(),
        shell_deps,
        skill_bodies.as_deref(),
        memory_block.as_deref(),
        session_thinking_level.as_deref(),
        policy,
        cancel_token,
        child_event_sink,
    )
}

/// 构建内置 Agent
#[allow(clippy::too_many_arguments)]
fn build_builtin(
    cfg: &AppConfig,
    model_store: &ModelProviderStore,
    agent_type_key: &str,
    knowledge_manager: Arc<KnowledgeManager>,
    catalog: Arc<CatalogCache>,
    model_id: Option<&str>,
    thinking_level: Option<&str>,
    db_pool: Option<DbPool>,
    redis_pool: Option<SharedRedisPool>,
    plugin_manager: Option<Arc<PluginManager>>,
    kb_instance_id: Option<&str>,
    cancel_token: CancellationToken,
) -> anyhow::Result<Arc<dyn Agent>> {
    match agent_type_key {
        "device_command" => {
            let agent = device_command::build_device_command_agent_with_model(
                model_store,
                knowledge_manager,
                catalog,
                model_id,
                thinking_level,
                kb_instance_id,
                cancel_token,
            )?;
            Ok(Arc::new(agent))
        }
        "monitor_plugin" => monitor_plugin::build_monitor_plugin_agent_with_model(
            cfg,
            model_store,
            model_id,
            thinking_level,
            db_pool,
            redis_pool,
            plugin_manager,
            cancel_token,
        ),
        _ => {
            anyhow::bail!("不支持的内置 agent_type: {}", agent_type_key)
        }
    }
}
