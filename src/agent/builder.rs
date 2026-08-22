//! 自定义助手构建器 + 会话级 Agent 分发器
//!
//! 主要功能：
//! - [`build_custom_agent`]：从 [`Assistant`] 构建通用 Agent（内置/自定义统一走此路径）
//! - [`build_agent_for_session`]：会话运行时入口
//!
//! 分发规则：所有助手（含内置）一律走 `build_custom_agent`。内置助手的配置
//! （system_prompt / enabled_tools / max_tokens）在 seed 时写入 DB（见 `seed_builtin`），
//! 运行期与自定义助手同路径——不再有忽略 DB 配置的专用 builder。

use std::sync::Arc;

use adk_rust::agent::Agent;
use tokio_util::sync::CancellationToken;

use crate::agent::workspace;
use crate::config::AppConfig;
use crate::domain::assistant::models::Assistant;
use crate::domain::device_catalog::CatalogCache;
use crate::domain::knowledge::KnowledgeManager;
use crate::permissions::PermissionPolicy;
use crate::infra::db::DbPool;
use crate::infra::object_store::ObjectStore;
use crate::infra::redis::SharedRedisPool;
use crate::llm::{make_gen_config_from, make_model_and_meta, make_model_by_id};
use crate::domain::model_provider::store::ModelProviderStore;
use crate::domain::monitor::PluginManager;

use crate::agent::cortex::CortexAgentBuilder;

/// `build_custom_agent` / `build_agent_for_session` 的返回：装箱 agent + 供 SSE 只读轮询的
/// 句柄（装箱前从具体 CortexAgent 取出：上下文预算快照 + 子 agent token 用量累加器，
/// SSE 层随 CONTEXT_USAGE 上报）。
type AgentWithHandles = (
    Arc<dyn Agent>,
    Option<crate::agent::cortex::SharedBudget>,
    Option<crate::agent::cortex::ChildUsageTotal>,
);
use crate::tools::shell_command::ShellToolDeps;

/// 知识库检索工具（search_kb）：**绑定知识库即常驻注入**，不走 `enabled_tools` 白名单。
///
/// UI 没有 search_kb 的勾选入口（它不是「可选工具」，而是绑定知识库后的固有能力），
/// 故与 code 文件工具同属"条件常驻"模式——只要 `kb_instance_id` 非空就注入。
/// 仍需 KnowledgeManager 可用 + 会话归属人有可用模型（query_understanding 用它解析查询），
/// 模型不可用时跳过并告警（不阻断 agent 构建）。
fn push_search_kb_tool(
    builder: CortexAgentBuilder,
    kb_instance_id: Option<&str>,
    knowledge: Option<&Arc<KnowledgeManager>>,
    model_store: Option<&ModelProviderStore>,
    user_id: &str,
) -> CortexAgentBuilder {
    let Some(kb_id) = kb_instance_id.filter(|s| !s.is_empty()) else {
        return builder;
    };
    let Some(km) = knowledge else {
        return builder;
    };
    // 查询理解模型按会话归属人解析（隔离 API Key）
    let model = model_store.and_then(|s| make_model_by_id(s, None, user_id).ok());
    if let Some(m) = model {
        let qu =
            Arc::new(crate::agent::query_understanding::QueryUnderstandingService::new(m, 500));
        builder.tool(Arc::new(crate::tools::device_command::create_search_tool(
            km.clone(),
            qu,
            Some(kb_id.to_string()),
        )))
    } else {
        tracing::warn!(
            "[custom] search_kb 跳过：模型不可用，无法初始化 query_understanding（kb_instance_id={kb_id}）"
        );
        builder
    }
}

/// 按 `enabled_tools` 白名单注入「真正可选」的工具（设备目录检索、shell 命令）。
///
/// 注：`search_kb` 不再走此白名单——已改为绑定知识库即常驻注入，见 [`push_search_kb_tool`]。
/// 旧数据 `enabled_tools` 里残留的 `"search_kb"` 命中后落到默认分支，无副作用。
fn push_tool_for_key(
    builder: CortexAgentBuilder,
    key: &str,
    catalog: Option<Arc<CatalogCache>>,
    shell_deps: Option<&Arc<ShellToolDeps>>,
) -> CortexAgentBuilder {
    match key {
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
    skill_service: Option<Arc<crate::domain::skill::SkillService>>,
    memory_proposal_store: Option<&std::sync::Arc<crate::domain::memory::MemoryProposalStore>>,
    model_id_override: Option<&str>,
    user_id: &str,
    shell_deps: Option<Arc<ShellToolDeps>>,
    skill_bodies: Option<&str>,
    memory_block: Option<&str>,
    session_thinking_level: Option<&str>,
    policy: PermissionPolicy,
    cancel_token: CancellationToken,
    child_event_sink: Option<Arc<dyn crate::agent::cortex::ChildEventSink>>,
    async_message_sink: Option<
        Arc<dyn crate::tools::send_user_message_async::AsyncUserMessageSink>,
    >,
    model_store_arc: Option<Arc<ModelProviderStore>>,
    steer_port: Option<Arc<crate::infra::run_registry::SteerPort>>,
    session_window: Option<crate::agent::cortex::SharedWindowState>,
    // 顶层会话的应用状态（manage_scheduled_task 工具用）。子 agent 为 None。
    app_state: Option<Arc<crate::server::AppState>>,
) -> anyhow::Result<AgentWithHandles> {
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
    let (model, resolved) = make_model_and_meta(model_store, effective_model.as_deref(), user_id)?;

    let gen_cfg = make_gen_config_from(
        assistant.max_tokens,
        assistant.temperature,
        assistant.top_p,
        session_thinking_level,
    );

    // 注入 skill 目录(分层注入,不拼进 instruction)
    // 助手级 skill 白名单：空 = 全部可见(传 None)；非空 = 硬隔离仅列出的可见(传 Some)。
    let skill_allowed: Option<&[String]> = if assistant.enabled_skills.is_empty() {
        None
    } else {
        Some(assistant.enabled_skills.as_slice())
    };
    let skill_catalog = if let Some(svc) = skill_service.as_ref() {
        // catalog 预算按模型真实 context_window 缩放（0/负值回退默认），
        // 避免硬编码 128k 导致小窗口模型溢出、超大窗口模型被无谓截断。
        let cw = resolved
            .context_window
            .filter(|&w| w > 0)
            .map(|w| w as usize);
        let catalog = svc.render_catalog_block_filtered(
            cfg.skill.catalog_token_budget_pct,
            cw.unwrap_or(0),
            skill_allowed,
        );
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
    // 模型名先取（model 随即 move 进 builder，下方 context_window warn 需要引用）
    let model_name_for_warn = model.name().to_string();
    let mut builder = crate::agent::cortex::CortexAgentBuilder::new(&agent_name)
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
    // steer 队列消费句柄（运行中提交的用户消息；root run 由 SSE 层注入，未注入则不消费）
    if let Some(port) = steer_port {
        builder = builder.steer_port(port);
    }
    // 会话级软着陆窗口状态（root run 由 SSE 层按 thread_id 注入，remind/borrow flag
    // 跨 run 存活；子 agent 不注入 → 各自 per-run 独立窗口）
    if let Some(ws) = session_window {
        builder = builder.window_state(ws);
    }
    // 多智能体 V2：`[agents]` 角色配置 + 会话思考级别（MultiAgentMode Auto 推导用）
    builder = builder
        .agents_config(cfg.agents.clone())
        .session_thinking_level(session_thinking_level.map(str::to_string));
    // spawn model 覆盖解析器（对齐 codex apply_requested_spawn_agent_model_overrides：
    // spawn 的 model 参数 / default_subagent_model 按 user_id 从 DB 解析，解析不了继承父。
    // 闭包需 'static，故用 AgentContext 的 Arc store 而非本函数的 &ModelProviderStore）
    if let Some(store) = model_store_arc {
        let uid = user_id.to_string();
        builder = builder.model_resolver(std::sync::Arc::new(move |id| {
            crate::llm::make_model_by_id(&store, Some(id), &uid).ok()
        }));
    }

    // 工作目录（绝对路径）注入 environment 层：模型据此定位、把产物写到工作区，避免盲写他处。
    // 优先 canonicalize 绝对路径（模型/工具跨 cwd 取用更稳）；目录尚不存在则回退展示值。
    if let Some(cwd) = workspace_root.as_ref().map(|p| {
        std::fs::canonicalize(p.as_path())
            .map(|c| c.to_string_lossy().to_string())
            .unwrap_or_else(|_| p.display().to_string())
    }) {
        builder = builder.workspace_cwd(cwd);
    }

    // 动态压缩阈值：注入模型 context_window。
    // None / 0 / 负值（DB 字段，管理员可填错）一律忽略 → 走 fallback_context_window，
    // 避免 0 导致每轮死磕压缩、负值(i32→usize 巨大)导致压缩永久禁用。
    // 缺失/非法时打 warn 暴露：静默 fallback 会把 1M 窗口模型压到 128K 闸门，
    // 用户侧表现为「还剩一半上下文却突然压缩」（占用种子对旧窗口仅 50%）。
    match resolved.context_window {
        Some(w) if w > 0 => {
            builder = builder.context_window(w as usize);
        }
        _ => {
            tracing::warn!(
                "[custom] 模型 \"{}\" (id={:?}) 的 context_window 未配置或非法（{:?}），\
                 压缩闸门回落 fallback={}——请到模型管理补配真实窗口",
                model_name_for_warn,
                effective_model,
                resolved.context_window,
                cfg.context.fallback_context_window
            );
        }
    }
    // 压缩专用便宜模型（None=用主模型压缩）
    if let Some(cid) = cfg
        .context
        .compact_model_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        match make_model_by_id(model_store, Some(cid), user_id) {
            Ok(cm) => builder = builder.compact_model(cm),
            Err(e) => tracing::warn!(
                "[custom] compact_model_id={} 解析失败，回退主模型压缩: {e}",
                cid
            ),
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

    // 知识库检索：绑定知识库即注入 search_kb（非可选工具，不走 enabled_tools 白名单；
    // UI 无此工具勾选入口，配了知识库就该能用——与下方 code 文件工具同属"条件常驻"）。
    builder = push_search_kb_tool(
        builder,
        assistant.kb_instance_id.as_deref(),
        knowledge.as_ref(),
        Some(model_store),
        user_id,
    );

    // 注册助手声明的「可选工具」(不再收窄 — skill 不再绑定到 assistant)
    for key in &assistant.enabled_tools {
        builder = push_tool_for_key(builder, key.as_str(), catalog.clone(), shell_deps.as_ref());
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

    // 注册 read_skill 工具(常驻,让 LLM 主动拉取 skill 正文;与 $name 提及同款渲染,带 <path>)。
    // 传入助手级白名单：非白名单 skill 在工具内按「不存在」处理(硬隔离)。
    if let Some(svc) = skill_service.as_ref() {
        let allowed_owned: Option<Vec<String>> = if assistant.enabled_skills.is_empty() {
            None
        } else {
            Some(assistant.enabled_skills.clone())
        };
        builder = builder.tool(Arc::new(crate::tools::skill_read::create_read_skill_tool(
            svc.clone(),
            cfg.skill.max_inject_chars,
            allowed_owned,
        )));
    }

    // 文件操作工具(常驻,对所有 custom 助手可用)。
    // UI 没有"按助手配置工具"的入口,而这些读/编辑能力是基础工具——且已被 workspace 根路径
    // 关住、经 resolve_safe_path 防目录逃逸/符号链接攻击,故不走 enabled_tools 白名单,
    // 只要有会话 workspace 就注册。shell_command 等强能力工具仍保持白名单。
    // 只读工具(read_file/glob/grep)额外放开 shell_command 的 readonly_extra
    // (skill 目录等),让模型能直接读 skill 脚本/文档——否则只能改用 head/cat 绕开。
    // 写工具(edit_file/create_file)仍只允许 workspace 根,只读目录不可写。
    let extra_read_roots: Vec<std::path::PathBuf> = shell_deps
        .as_ref()
        .map(|d| d.readonly_extra.clone())
        .unwrap_or_default();
    if let Some(root) = workspace_root.as_ref() {
        builder = builder.tool(Arc::new(crate::tools::code::create_read_file_tool(
            root.clone(),
            extra_read_roots.clone(),
        )));
        builder = builder.tool(Arc::new(crate::tools::code::create_glob_tool(
            root.clone(),
            extra_read_roots.clone(),
        )));
        builder = builder.tool(Arc::new(crate::tools::code::create_grep_tool(
            root.clone(),
            extra_read_roots.clone(),
        )));
        builder = builder.tool(Arc::new(crate::tools::code::create_edit_file_tool(
            root.clone(),
        )));
        builder = builder.tool(Arc::new(crate::tools::code::create_create_file_tool(
            root.clone(),
        )));
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

    // 注册 manage_scheduled_task 工具（常驻顶层会话，让 LLM 在对话中创建/管理定时任务）。
    // 仅顶层会话注入 app_state（子 agent blueprint 克隆不带 → 子 agent 无此工具）。
    if let Some(state) = app_state {
        builder = builder.tool(Arc::new(
            crate::tools::manage_scheduled_task::create_manage_scheduled_task_tool(
                state.clone(),
                assistant.id.clone(),
            ),
        ));
    }

    // 注册 send_user_message_async 工具（常驻；长任务中途给用户发进度/阻塞提问，
    // 立即返回、回复异步到达——对齐 codex。SSE 入口才注册：sink 指向本次 run 的
    // SSE 流，非 SSE 入口无用户面前端。子 agent 经 blueprint 克隆本工具时 sink
    // 随之继承（同样到达用户面），与 codex「任意 agent 可发异步用户消息」一致。）
    if let Some(sink) = async_message_sink {
        builder = builder.tool(Arc::new(
            crate::tools::send_user_message_async::create_send_user_message_async_tool(sink),
        ));
    }

    let agent = builder
        .build()
        .map_err(|e| anyhow::anyhow!("创建自定义助手 Agent 失败: {}", e))?;
    // 装箱前取出 budget 只读句柄，供 SSE 层推 token 用量（对齐 codex token 显示）
    let budget = Some(agent.budget());
    // 同款取出子 agent 用量累加器只读句柄（SSE 随 CONTEXT_USAGE 上报子 agent 花费）
    let child_usage = Some(agent.child_usage_total());
    Ok((Arc::new(agent) as Arc<dyn Agent>, budget, child_usage))
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
    pub skill_service: Option<Arc<crate::domain::skill::SkillService>>,
    /// 记忆建议存储（propose_memory 工具写入；DB 不可用时为 None → 不注册该工具）
    pub memory_proposal_store: Option<std::sync::Arc<crate::domain::memory::MemoryProposalStore>>,
    /// 模型供应商存储（LLM 解析的唯一数据源；取代历史 `GLOBAL_STORE`）。
    /// DB 不可用时为 None，agent 构建时若调用 `require_model_store` 会返回错误。
    pub model_provider_store: Option<Arc<ModelProviderStore>>,
    /// 对象存储(S3/RustFS);截图工具结果上传用。未启用时为 None → 截图走普通截断
    pub object_store: Option<Arc<ObjectStore>>,
    /// 应用状态（'static Arc）。供 manage_scheduled_task 等需要访问业务 store/调度器/模型
    /// 的工具使用。仅在顶层会话注入（SSE/定时任务入口）；子 agent（blueprint 克隆）不带 → 不注册该工具。
    pub app_state: Option<Arc<crate::server::AppState>>,
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
    /// 会话归属人 user_id：模型/记忆/知识库等按此隔离（管理员跨用户访问会话时为归属者）
    pub user_id: String,
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
    pub child_event_sink: Option<Arc<dyn crate::agent::cortex::ChildEventSink>>,
    /// 异步用户消息出口（send_user_message_async 工具的 SSE 转发 sink）；
    /// None=不注册该工具（非 SSE 入口无用户面前端）
    pub async_message_sink: Option<Arc<dyn crate::tools::send_user_message_async::AsyncUserMessageSink>>,
    /// steer 队列消费句柄（运行中提交的用户消息注入当前 run）；None=本 run 不支持 steer
    /// （非 SSE 入口 / 子 agent）。对齐 codex TurnInputMode::StartOrSteer 的消费端。
    pub steer_port: Option<Arc<crate::infra::run_registry::SteerPort>>,
    /// 会话级软着陆窗口状态（remind/borrow flag 跨 run 存活，仅压缩开新窗时复位）；
    /// None=子 agent / 非 SSE run（各自独立窗口，per-run 生命周期）。
    pub session_window:
        Option<crate::agent::cortex::SharedWindowState>,
}

/// 会话运行时入口：内置/自定义助手统一路由到 [`build_custom_agent`]。
///
/// 统一接收 `AgentContext`（共享基础设施）+ `AgentRequest`（单次请求参数），
/// 避免函数签名随参数增加不断膨胀。
/// 返回 (Agent, 预算句柄)；预算句柄含 crate-private 类型，
/// 本函数仅供 crate 内 SSE 层调用，故允许 private_interfaces。
#[allow(private_interfaces)]
pub async fn build_agent_for_session(
    ctx: &AgentContext<'_>,
    assistant: &Assistant,
    req: AgentRequest,
) -> anyhow::Result<AgentWithHandles> {
    // 模型供应商存储：所有后续 LLM 解析的唯一数据源（取代历史 GLOBAL_STORE）。
    // DB 不可用则 agent 构建失败。
    let model_store = ctx.require_model_store()?;

    let AgentRequest {
        model_id,
        user_id,
        mcp_toolsets,
        shell_deps,
        skill_bodies,
        memory_block,
        session_thinking_level,
        policy,
        cancel_token,
        workspace_mode,
        child_event_sink,
        async_message_sink,
        steer_port,
        session_window,
    } = req;
    // 沙箱根路径：仅 Sandbox 模式有，供 code 工具（read_file/edit_file/…）注册用
    let workspace_root: Option<Arc<std::path::PathBuf>> = match &workspace_mode {
        workspace::WorkspaceMode::Sandbox(p) => Some(Arc::new(p.clone())),
        workspace::WorkspaceMode::ChatOnly => None,
    };

    // 内置/自定义统一：一律走 build_custom_agent（使用 DB 中的配置）。
    // 内置助手的 system_prompt / enabled_tools / max_tokens 在 seed 时写入 DB（见 seed_builtin）。
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
        &user_id,
        shell_deps,
        skill_bodies.as_deref(),
        memory_block.as_deref(),
        session_thinking_level.as_deref(),
        policy,
        cancel_token,
        child_event_sink,
        async_message_sink,
        ctx.model_provider_store.clone(),
        steer_port,
        session_window,
        ctx.app_state.clone(),
    )
}
