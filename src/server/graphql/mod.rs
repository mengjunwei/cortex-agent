//! GraphQL 接入层
//!
//! 将原 REST 业务逻辑统一暴露为单一 GraphQL 入口 `POST /api/graphql`。
//!
//! ## 设计要点
//!
//! 1. **JSON 标量透传**：所有入参/返回值使用 `JSON`（即 `serde_json::Value`），
//!    业务 handler 统一返回 [`super::response`] 生成的标准信封
//!    `{ code, message, data }`（`code == 0` 表示成功）。前端 `gql()` 解包后
//!    拿到 `{ data, code, message }`，调用点按 `code` 判定成败。
//! 2. **保留路由**：`/api/run_sse`（SSE 流式）、
//!    `/api/health`、`/api/v1/monitor/health`（健康检查）不迁移，仍在 [super] 中以 REST 暴露。
//! 3. **AppState 注入**：通过 `Schema::build().data(state)` 把 `Arc<AppState>` 注入
//!    GraphQL Context，resolver 内通过 `ctx.data::<Arc<AppState>>()` 取用。

use std::sync::Arc;

use async_graphql::{
    Context, EmptySubscription, InputValueResult, Object, Scalar, ScalarType, Schema,
    Value as GqlValue,
};

use crate::server::AppState;
use crate::domain::skill::SkillScope;

use serde_json::json;

/// JSON 标量包装类型 — 用于 GraphQL 入参/返回值的透传
///
/// 实现思路：通过 serde 在 `serde_json::Value` 与 `async_graphql::Value` 之间互转。
/// async-graphql 的 `Value` 同时实现了 `Serialize` / `Deserialize`，
/// 因此 `serde_json::to_value` / `serde_json::from_value` 可正确完成转换。
#[derive(Debug, Clone, Default)]
pub struct Json(pub serde_json::Value);

#[Scalar(name = "JSON")]
impl ScalarType for Json {
    fn parse(value: GqlValue) -> InputValueResult<Self> {
        let v = serde_json::to_value(&value).unwrap_or_default();
        Ok(Json(v))
    }

    fn to_value(&self) -> GqlValue {
        serde_json::from_value(self.0.clone()).unwrap_or(GqlValue::Null)
    }
}

/// GraphQL Schema 类型别名
pub type GqlSchema = Schema<QueryRoot, MutationRoot, EmptySubscription>;

/// 构建 GraphQL Schema，将 AppState 注入 Context
pub fn build_schema(state: Arc<AppState>) -> GqlSchema {
    Schema::build(QueryRoot, MutationRoot, EmptySubscription)
        .data(state)
        .finish()
}

/// 从 GraphQL Context 取出 AppState 引用
fn state_of<'a>(ctx: &'a Context<'_>) -> &'a AppState {
    // 生命周期：data_unchecked 返回 &'a T，其中 'a 绑定到 ctx 的生命周期。
    // AppState 由 Schema 持有（Arc），存活期 >= Schema >= 请求处理周期，安全。
    ctx.data_unchecked::<Arc<AppState>>()
}

/// 从 GraphQL Context 取出当前用户 id（graphql_handler 通过 `request.data(user_id)` 注入）。
/// 记忆等按用户隔离的接口用此 id；未注入（旧路径）时回退 "user"。
fn current_user_id(ctx: &Context<'_>) -> String {
    ctx.data::<String>()
        .ok()
        .cloned()
        .unwrap_or_else(|| "user".to_string())
}

/// GraphQL Context 里的认证上下文。
///
/// **必须用单一 struct 注入**：async-graphql 的 context 按 `TypeId` 索引数据，同类型只能存一个。
/// 若把 `is_admin` 和 `via_api_token` 分别 `.data(bool)` 注入，后者会覆盖前者，
/// 导致 `reject_api_token_delete` 读到的是 `is_admin` 而非 `via_api_token`——管理员被误判成
/// API Token 请求、删除操作被拒（已修过的 bug）。合并为一个 struct 即各自独立可读。
#[derive(Clone, Copy, Default)]
pub struct GqlAuthCtx {
    /// 当前用户是否管理员
    pub is_admin: bool,
    /// 是否经 Authorization: Bearer（账户 API Token）认证，而非 Cookie 登录
    pub via_api_token: bool,
}

/// 从 GraphQL Context 取出当前用户是否管理员。
///
/// 用于「完全访问」(danger-full-access) 等特权能力的后端强制：仅管理员可设。
/// 未注入/未登录时回退 false——特权能力 fail-closed，宁可误判非管理员也不放行。
fn current_is_admin(ctx: &Context<'_>) -> bool {
    ctx.data::<GqlAuthCtx>()
        .map(|a| a.is_admin)
        .unwrap_or(false)
}

/// 删除类操作的权限守卫：**API Token 认证的请求仅允许删除会话**，其他删除一律拒绝。
///
/// 判定依据：`graphql_handler` 注入的 [`GqlAuthCtx::via_api_token`]（请求通过 Authorization:
/// Bearer 成功认证 = 账户 API Token 程序化访问）。账号登录（Cookie JWT）与未登录不受此限。
/// 返回 `Some(Json)` 表示被拒，resolver 应直接 `return` 该响应。
fn reject_api_token_delete(ctx: &Context<'_>) -> Option<Json> {
    let via_api_token = ctx
        .data::<GqlAuthCtx>()
        .map(|a| a.via_api_token)
        .unwrap_or(false);
    if via_api_token {
        Some(Json(super::response::err(
            super::response::code::BUSINESS,
            "API Token 认证仅支持删除会话；删除助手/模型/供应商/MCP/知识库请使用账号登录",
        )))
    } else {
        None
    }
}

/// 入参反序列化失败的统一响应：错误码 `PARSE_ERROR`（1002）
fn parse_err(e: serde_json::Error) -> Json {
    Json(super::response::err(
        super::response::code::PARSE_ERROR,
        format!("参数解析失败: {}", e),
    ))
}

// =============================================================================
//  Query
// =============================================================================

pub struct QueryRoot;

#[allow(clippy::too_many_arguments)]
#[Object]
impl QueryRoot {
    // -------- 通用 --------

    /// 可用模型列表（含默认模型 id，按归属隔离）
    async fn models(&self, ctx: &Context<'_>) -> Json {
        Json(super::models(state_of(ctx), &current_user_id(ctx), current_is_admin(ctx)).await)
    }

    /// 设备目录（厂商 + 设备类型）
    async fn catalog(&self, ctx: &Context<'_>) -> Json {
        Json(super::catalog(state_of(ctx)).await)
    }

    // -------- 助手 --------

    /// 全部助手：普通用户=自己创建的；管理员=全部（含归属管理员的内置助手）
    async fn assistants(&self, ctx: &Context<'_>) -> Json {
        Json(
            super::assistant::list_assistants(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 单个助手详情：私有仅归属人/管理员可读，否则 404
    async fn assistant(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::assistant::get_assistant(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 广场列表（公开助手，脱敏）
    async fn explore_assistants(&self, ctx: &Context<'_>) -> Json {
        Json(super::assistant::list_explore(state_of(ctx)).await)
    }

    /// 按分享口令查询助手（公开）
    async fn assistant_by_token(&self, ctx: &Context<'_>, token: String) -> Json {
        Json(super::assistant::get_by_token(state_of(ctx), &token).await)
    }

    /// 可勾选工具列表
    async fn tools(&self, ctx: &Context<'_>) -> Json {
        Json(super::assistant::list_tools(state_of(ctx)).await)
    }

    // -------- 会话 --------

    /// 会话列表（分页 / 关键词 / agent_type / kind / assistant_id 筛选）
    async fn sessions(
        &self,
        ctx: &Context<'_>,
        page: Option<i32>,
        page_size: Option<i32>,
        keyword: Option<String>,
        agent_type: Option<String>,
        kind: Option<i32>,
        assistant_id: Option<String>,
    ) -> Json {
        Json(
            super::session::list_sessions(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                super::session::SessionListParams {
                    page: page.map(|v| v.max(1) as usize),
                    page_size: page_size.map(|v| v.clamp(1, 100) as usize),
                    keyword,
                    agent_type,
                    kind: kind.map(|v| v as i16),
                    assistant_id,
                },
            )
            .await,
        )
    }

    /// 会话历史（含消息序列、待确认项、绑定模型）
    async fn session_history(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::session::get_session_history(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
            )
            .await,
        )
    }

    /// 会话级思考级别（默认 high）
    async fn session_thinking_level(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::session::get_session_thinking_level(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
            )
            .await,
        )
    }

    /// 会话级审批方式（沙箱模式 + 审批策略，未设置 → 全局 [shell] 默认）
    async fn session_permission_policy(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::session::get_session_permission_policy(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
            )
            .await,
        )
    }

    // -------- 记忆 --------

    /// 全部记忆（管理页；普通用户仅自己，管理员看全部）
    async fn memories(&self, ctx: &Context<'_>) -> Json {
        Json(
            super::memory::list_memories(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 待确认记忆建议（卡片；普通用户仅自己，管理员看全部）
    async fn memory_proposals(&self, ctx: &Context<'_>) -> Json {
        Json(
            super::memory::list_memory_proposals(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    // -------- 知识库 --------

    /// 知识库实例列表（多 provider：Dify 外挂 + 内置）：普通用户=自己的+公开；管理员=全部
    async fn kb_instances(&self, ctx: &Context<'_>) -> Json {
        Json(
            super::knowledge_instances::kb_instance_list(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// Provider 配置 schema（驱动前端动态表单）
    async fn kb_provider_schema(&self, ctx: &Context<'_>) -> Json {
        Json(super::knowledge_instances::kb_provider_schema(state_of(ctx)).await)
    }

    /// 某实例的文档列表（路由到对应 provider）：校验可读
    async fn kb_instance_documents(&self, ctx: &Context<'_>, input: Json) -> Json {
        let params: super::knowledge::KbInstanceDocsQuery = match serde_json::from_value(input.0) {
            Ok(p) => p,
            Err(e) => return parse_err(e),
        };
        Json(
            super::knowledge::kb_instance_documents(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                params,
            )
            .await,
        )
    }

    /// 某实例文档的分段预览：校验可读
    async fn kb_instance_segments(
        &self,
        ctx: &Context<'_>,
        instance_id: String,
        doc_id: String,
    ) -> Json {
        Json(
            super::knowledge::kb_instance_segments(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &instance_id,
                &doc_id,
            )
            .await,
        )
    }

    // -------- 设备检索 --------

    /// 设备语义检索：取调用者可见的第一个启用知识库实例
    async fn device_search(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: super::device::DeviceSearchRequest = match serde_json::from_value(input.0) {
            Ok(r) => r,
            Err(e) => return parse_err(e),
        };
        Json(super::device::device_search(state_of(ctx), &current_user_id(ctx), req).await)
    }

    // -------- 监控 --------

    /// 列出所有监控插件
    async fn monitor_plugins(&self, ctx: &Context<'_>) -> Json {
        Json(super::monitor::list(state_of(ctx)).await)
    }

    /// 获取插件详情
    async fn monitor_plugin(&self, ctx: &Context<'_>, plugin_id: String) -> Json {
        Json(super::monitor::get_plugin(state_of(ctx), &plugin_id).await)
    }

    /// 获取插件版本历史
    async fn monitor_plugin_versions(&self, ctx: &Context<'_>, plugin_id: String) -> Json {
        Json(super::monitor::list_versions(state_of(ctx), &plugin_id).await)
    }

    /// 获取插件 OID 列表（带缓存）
    async fn monitor_oids(&self, ctx: &Context<'_>, plugin_id: String) -> Json {
        Json(super::monitor_get_oids(state_of(ctx), &plugin_id).await)
    }

    /// 计算监控结果
    async fn monitor_calculate(
        &self,
        ctx: &Context<'_>,
        plugin_id: String,
        oid_values: Json,
    ) -> Json {
        Json(super::monitor_calculate(state_of(ctx), &plugin_id, &oid_values.0).await)
    }

    // -------- 模型供应商 --------

    /// 供应商列表（含嵌套模型，按归属隔离）
    async fn model_providers(&self, ctx: &Context<'_>) -> Json {
        Json(
            super::model_provider::list_providers(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    // -------- MCP Server --------

    /// MCP Server 列表（含健康状态，按归属隔离：普通用户=自己的；管理员=全部）
    async fn mcp_servers(
        &self,
        ctx: &Context<'_>,
        page: Option<usize>,
        page_size: Option<usize>,
        keyword: Option<String>,
    ) -> Json {
        Json(
            super::mcp::list_servers_paged(
                state_of(ctx),
                page,
                page_size,
                keyword,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 单个 MCP Server 详情（归属人/管理员可见，否则 404）
    async fn mcp_server(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::mcp::get_server(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// MCP 工具清单查询（仅归属人/管理员可见自己的 server）
    async fn mcp_tools(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(
            super::mcp::list_tools(
                state_of(ctx),
                input.0,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// Shell 权限规则列表
    async fn shell_rules(&self, ctx: &Context<'_>) -> Json {
        let state = state_of(ctx);
        match &state.shell_rule_store {
            Some(store) => match store.list().await {
                Ok(rules) => Json(super::response::ok(
                    serde_json::to_value(&rules).unwrap_or_default(),
                )),
                Err(e) => Json(super::response::err(
                    super::response::code::DATABASE,
                    format!("查询失败: {e}"),
                )),
            },
            None => Json(super::response::ok(json!([]))),
        }
    }

    // -------- Skill --------

    /// Skill 目录(已加载的 skill 列表,供管理页展示)
    async fn skills(&self, ctx: &Context<'_>) -> Json {
        let state = state_of(ctx);
        match &state.skill_service {
            Some(svc) => {
                let skills = svc
                    .list_skills()
                    .into_iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "description": s.description,
                            "short_description": s.short_description,
                            "scope": match s.scope {
                                SkillScope::Builtin => "builtin",
                                SkillScope::User => "user",
                            },
                        })
                    })
                    .collect::<Vec<_>>();
                Json(super::response::ok(json!({ "skills": skills })))
            }
            None => Json(super::response::err(
                super::response::code::BUSINESS,
                "Skill 服务未初始化",
            )),
        }
    }
}

// =============================================================================
//  Mutation
// =============================================================================

pub struct MutationRoot;

#[Object]
impl MutationRoot {
    // -------- 会话 --------

    /// 创建会话
    async fn create_session(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(super::session::create_session(state_of(ctx), &current_user_id(ctx), input.0).await)
    }

    /// 删除会话
    async fn delete_session(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::session::delete_session(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
            )
            .await,
        )
    }

    /// 重命名会话
    async fn rename_session(&self, ctx: &Context<'_>, id: String, title: String) -> Json {
        Json(
            super::session::rename_session(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
                &title,
            )
            .await,
        )
    }

    /// 更新会话绑定的模型
    async fn update_session_model(
        &self,
        ctx: &Context<'_>,
        id: String,
        model_id: Option<String>,
    ) -> Json {
        Json(
            super::session::update_session_model(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
                model_id.as_deref().unwrap_or(""),
            )
            .await,
        )
    }

    /// 更新会话级思考级别（low/medium/high/xhigh/max）
    async fn update_session_thinking_level(
        &self,
        ctx: &Context<'_>,
        id: String,
        level: String,
    ) -> Json {
        Json(
            super::session::update_session_thinking_level(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
                &level,
            )
            .await,
        )
    }

    /// 更新会话级审批方式（沙箱模式 + 审批策略）
    async fn update_session_permission_policy(
        &self,
        ctx: &Context<'_>,
        id: String,
        sandbox_mode: String,
        approval_policy: String,
    ) -> Json {
        // 后端强制：完全访问（danger-full-access）仅管理员可设。前端隐藏只是 UX，
        // 直接调 GraphQL 可绕过——必须在此拦截，非管理员一律拒绝。
        if sandbox_mode.trim()
            == crate::permissions::SandboxMode::DangerFullAccess.codex_id()
            && !current_is_admin(ctx)
        {
            return Json(super::response::err(
                super::response::code::BUSINESS,
                "完全访问模式仅管理员可用".to_string(),
            ));
        }
        Json(
            super::session::update_session_permission_policy(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &id,
                &sandbox_mode,
                &approval_policy,
            )
            .await,
        )
    }

    // -------- 记忆 --------

    /// 手动新增记忆（管理页）
    async fn create_memory(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(super::memory::create_memory(state_of(ctx), &current_user_id(ctx), input.0).await)
    }

    /// 编辑记忆正文/类型
    async fn update_memory(&self, ctx: &Context<'_>, id: String, input: Json) -> Json {
        Json(
            super::memory::update_memory(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                id,
                input.0,
            )
            .await,
        )
    }

    /// 删除记忆
    async fn delete_memory(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::memory::delete_memory(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                id,
            )
            .await,
        )
    }

    /// 采纳记忆建议（转正写入 memories）
    async fn accept_memory_proposal(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::memory::accept_memory_proposal(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                id,
            )
            .await,
        )
    }

    /// 忽略记忆建议
    async fn reject_memory_proposal(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::memory::reject_memory_proposal(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                id,
            )
            .await,
        )
    }

    // -------- 流式控制 --------

    /// 取消正在运行的 Agent 任务
    async fn cancel_run(&self, ctx: &Context<'_>, thread_id: String) -> Json {
        Json(
            super::sse::cancel(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &thread_id,
            )
            .await,
        )
    }

    /// 运行中追加输入（steer，对齐 codex StartOrSteer）：会话忙时把用户消息注入当前
    /// run（下轮模型请求前生效）；返回 `steered:false` = 无活跃 run，前端回退正常发送。
    async fn steer_run(
        &self,
        ctx: &Context<'_>,
        thread_id: String,
        messages: Json,
        run_id: Option<String>,
    ) -> Json {
        Json(
            super::sse::steer(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &thread_id,
                messages.0,
                run_id,
            )
            .await,
        )
    }

    // -------- 助手 --------

    /// 创建自定义助手
    async fn create_assistant(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(
            super::assistant::create_assistant(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &input.0,
            )
            .await,
        )
    }

    /// AI 智能生成助手草稿（不落库，只返回四字段供前端填充表单）
    async fn generate_assistant(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(
            super::assistant::generate_assistant(state_of(ctx), &input.0, &current_user_id(ctx))
                .await,
        )
    }

    /// 更新自定义助手
    async fn update_assistant(&self, ctx: &Context<'_>, id: String, input: Json) -> Json {
        Json(
            super::assistant::update_assistant(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
                &input.0,
            )
            .await,
        )
    }

    /// 删除自定义助手（force 省略/false=仅预检返回影响清单；force=true=执行级联清理+删除）
    async fn delete_assistant(&self, ctx: &Context<'_>, id: String, force: Option<bool>) -> Json {
        if let Some(denied) = reject_api_token_delete(ctx) {
            return denied;
        }
        Json(
            super::assistant::delete_assistant(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
                force.unwrap_or(false),
            )
            .await,
        )
    }

    /// 复制助手为自定义副本
    async fn duplicate_assistant(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::assistant::duplicate_assistant(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 生成/续用分享口令
    async fn share_assistant(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::assistant::share_enable(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 关闭分享口令
    async fn unshare_assistant(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::assistant::share_disable(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// Fork 公开/分享助手到本地
    async fn fork_assistant(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(super::assistant::fork_assistant(state_of(ctx), &id, &current_user_id(ctx)).await)
    }

    /// 绑定/解绑助手的知识库实例（内置助手配置知识库用；kb_instance_id 空串视为解绑）
    async fn bind_assistant_kb_instance(
        &self,
        ctx: &Context<'_>,
        assistant_id: String,
        kb_instance_id: Option<String>,
    ) -> Json {
        // 空串 / 纯空白视为解绑（传 None）
        let kb = kb_instance_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Json(
            super::assistant::set_kb_instance(
                state_of(ctx),
                &assistant_id,
                kb.as_deref(),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 导入助手 JSON
    async fn import_assistant(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(super::assistant::import_one(state_of(ctx), &current_user_id(ctx), &input.0).await)
    }

    /// 导出助手为 JSON
    async fn export_assistant(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::assistant::export_one(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 查看助手环境变量明文（需二次输入当前登录用户密码确认）
    async fn reveal_assistant_env_vars(
        &self,
        ctx: &Context<'_>,
        id: String,
        password: String,
    ) -> Json {
        Json(
            super::assistant::reveal_env_vars(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
                &password,
            )
            .await,
        )
    }

    // -------- 知识库 --------

    /// FAQ 学习（生成候选）：校验实例可读
    async fn kb_learn(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: super::knowledge::KbLearnRequest = match serde_json::from_value(input.0) {
            Ok(r) => r,
            Err(e) => return parse_err(e),
        };
        Json(
            super::knowledge::kb_learn(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                req,
            )
            .await,
        )
    }

    /// FAQ 重生成：校验实例可读
    async fn kb_learn_regenerate(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: super::knowledge::KbRegenerateRequest = match serde_json::from_value(input.0) {
            Ok(r) => r,
            Err(e) => return parse_err(e),
        };
        Json(
            super::knowledge::kb_learn_regenerate(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                req,
            )
            .await,
        )
    }

    /// FAQ 提交（写入）：校验实例归属
    async fn kb_learn_commit(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: super::knowledge::KbCommitRequest = match serde_json::from_value(input.0) {
            Ok(r) => r,
            Err(e) => return parse_err(e),
        };
        Json(
            super::knowledge::kb_learn_commit(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                req,
            )
            .await,
        )
    }

    /// 创建知识库实例（归属=当前用户）
    async fn kb_instance_create(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: super::knowledge_instances::KbInstanceCreateRequest =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::knowledge_instances::kb_instance_create(
                state_of(ctx),
                &current_user_id(ctx),
                req,
            )
            .await,
        )
    }

    /// 更新知识库实例（secret 字段留空保留原值）：校验归属
    async fn kb_instance_update(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: super::knowledge_instances::KbInstanceUpdateRequest =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::knowledge_instances::kb_instance_update(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                req,
            )
            .await,
        )
    }

    /// 删除知识库实例（force 省略/false=仅预检返回影响清单；force=true=执行解绑+删除）：校验归属
    async fn kb_instance_delete(&self, ctx: &Context<'_>, id: String, force: Option<bool>) -> Json {
        if let Some(denied) = reject_api_token_delete(ctx) {
            return denied;
        }
        Json(
            super::knowledge_instances::kb_instance_delete(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                id,
                force.unwrap_or(false),
            )
            .await,
        )
    }

    /// 测试知识库实例连通性：校验归属
    async fn kb_instance_test(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::knowledge_instances::kb_instance_test(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                id,
            )
            .await,
        )
    }

    /// 上传文档到指定实例：校验归属
    async fn kb_instance_upload(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: super::knowledge::KbInstanceDocUploadRequest =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::knowledge::kb_instance_upload(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                req,
            )
            .await,
        )
    }

    /// 删除指定实例的文档：校验归属
    async fn kb_instance_delete_document(
        &self,
        ctx: &Context<'_>,
        instance_id: String,
        doc_id: String,
    ) -> Json {
        Json(
            super::knowledge::kb_instance_delete_document(
                state_of(ctx),
                &current_user_id(ctx),
                current_is_admin(ctx),
                &instance_id,
                &doc_id,
            )
            .await,
        )
    }

    // -------- 监控 --------

    /// 注册监控插件
    async fn register_monitor_plugin(&self, ctx: &Context<'_>, input: Json) -> Json {
        // 监控插件为全局运行时配置：多用户(auth 启用)模式下仅管理员可管理；
        // 单用户(no-auth)模式无身份概念，放行（保持既有行为）。
        if state_of(ctx).auth.is_some() && !current_is_admin(ctx) {
            return Json(super::response::err(
                super::response::code::BUSINESS,
                "仅管理员可管理监控插件".to_string(),
            ));
        }
        let req: super::monitor::RegisterRequest = match serde_json::from_value(input.0) {
            Ok(r) => r,
            Err(e) => return parse_err(e),
        };
        Json(super::monitor::register(state_of(ctx), req).await)
    }

    /// 注销监控插件
    async fn unregister_monitor_plugin(&self, ctx: &Context<'_>, plugin_id: String) -> Json {
        // 监控插件为全局运行时配置：多用户(auth 启用)模式下仅管理员可管理；
        // 单用户(no-auth)模式无身份概念，放行（保持既有行为）。
        if state_of(ctx).auth.is_some() && !current_is_admin(ctx) {
            return Json(super::response::err(
                super::response::code::BUSINESS,
                "仅管理员可管理监控插件".to_string(),
            ));
        }
        Json(super::monitor::unregister(state_of(ctx), &plugin_id).await)
    }

    /// 回滚插件版本
    async fn rollback_monitor_plugin(
        &self,
        ctx: &Context<'_>,
        plugin_id: String,
        version: i32,
    ) -> Json {
        // 监控插件为全局运行时配置：多用户(auth 启用)模式下仅管理员可管理；
        // 单用户(no-auth)模式无身份概念，放行（保持既有行为）。
        if state_of(ctx).auth.is_some() && !current_is_admin(ctx) {
            return Json(super::response::err(
                super::response::code::BUSINESS,
                "仅管理员可管理监控插件".to_string(),
            ));
        }
        if version < 0 {
            return Json(super::response::err(
                super::response::code::INVALID_PARAMS,
                "version 不能为负",
            ));
        }
        Json(super::monitor::rollback(state_of(ctx), &plugin_id, version as u32).await)
    }

    // -------- 模型供应商 --------

    /// 新建供应商
    async fn create_model_provider(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: crate::domain::model_provider::dto::CreateProviderRequest =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::model_provider::create_provider(state_of(ctx), req, &current_user_id(ctx)).await,
        )
    }

    /// 编辑供应商（不含密钥）
    async fn update_model_provider(&self, ctx: &Context<'_>, id: String, input: Json) -> Json {
        let req: crate::domain::model_provider::dto::UpdateProviderRequest =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::model_provider::update_provider(
                state_of(ctx),
                &id,
                req,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 删除供应商（force 省略/false=仅预检返回影响清单；force=true=执行级联清理+删除）
    async fn delete_model_provider(
        &self,
        ctx: &Context<'_>,
        id: String,
        force: Option<bool>,
    ) -> Json {
        if let Some(denied) = reject_api_token_delete(ctx) {
            return denied;
        }
        Json(
            super::model_provider::delete_provider(
                state_of(ctx),
                &id,
                force.unwrap_or(false),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 重置供应商 API Key
    async fn reset_model_provider_key(&self, ctx: &Context<'_>, id: String, input: Json) -> Json {
        let req: crate::domain::model_provider::dto::ResetKeyRequest = match serde_json::from_value(input.0)
        {
            Ok(r) => r,
            Err(e) => return parse_err(e),
        };
        Json(
            super::model_provider::reset_key(
                state_of(ctx),
                &id,
                req,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 新建模型
    async fn create_model(&self, ctx: &Context<'_>, provider_id: String, input: Json) -> Json {
        let req: crate::domain::model_provider::dto::CreateModelRequest =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::model_provider::create_model(
                state_of(ctx),
                &provider_id,
                req,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 编辑模型
    async fn update_model(&self, ctx: &Context<'_>, id: String, input: Json) -> Json {
        let req: crate::domain::model_provider::dto::UpdateModelRequest =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::model_provider::update_model(
                state_of(ctx),
                &id,
                req,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 删除模型（force 省略/false=仅预检返回影响清单；force=true=执行级联清理+删除）
    async fn delete_model(&self, ctx: &Context<'_>, id: String, force: Option<bool>) -> Json {
        if let Some(denied) = reject_api_token_delete(ctx) {
            return denied;
        }
        Json(
            super::model_provider::delete_model(
                state_of(ctx),
                &id,
                force.unwrap_or(false),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 设为默认模型
    async fn set_default_model(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::model_provider::set_default(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 设为默认 embedding 模型（知识库内置 provider 用，按用户作用域唯一）
    async fn set_embedding_default_model(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::model_provider::set_embedding_default(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 批量探测模型存活（全并发，单模型 30s 超时，按归属隔离）
    async fn probe_models(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: crate::domain::model_provider::dto::ProbeModelsInput =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::model_provider::probe_models(
                state_of(ctx),
                req,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    // -------- MCP Server --------

    /// 新建 MCP Server（归属=当前用户）
    async fn create_mcp_server(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: crate::domain::mcp::dto::CreateMcpServerInput =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(super::mcp::create_server(state_of(ctx), req, &current_user_id(ctx)).await)
    }

    /// 编辑 MCP Server（归属人/管理员，否则 404）
    async fn update_mcp_server(&self, ctx: &Context<'_>, id: String, input: Json) -> Json {
        let req: crate::domain::mcp::dto::UpdateMcpServerInput =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(
            super::mcp::update_server(
                state_of(ctx),
                &id,
                req,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 删除 MCP Server（force 省略/false=仅预检返回影响清单；force=true=执行清理+删除；归属人/管理员）
    async fn delete_mcp_server(&self, ctx: &Context<'_>, id: String, force: Option<bool>) -> Json {
        if let Some(denied) = reject_api_token_delete(ctx) {
            return denied;
        }
        Json(
            super::mcp::delete_server(
                state_of(ctx),
                &id,
                force.unwrap_or(false),
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 手动探测 MCP Server（强制重连 + 工具发现；归属人/管理员）
    async fn probe_mcp_server(&self, ctx: &Context<'_>, id: String) -> Json {
        Json(
            super::mcp::probe_server(
                state_of(ctx),
                &id,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 批量设置 MCP 服务状态（ids 为 null 时表示全选匹配项；按归属隔离）
    async fn batch_set_mcp_status(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(
            super::mcp::batch_set_status(
                state_of(ctx),
                input.0,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 批量删除 MCP 服务（ids 为 null 时表示全选匹配项；按归属隔离）
    async fn batch_delete_mcp_servers(&self, ctx: &Context<'_>, input: Json) -> Json {
        if let Some(denied) = reject_api_token_delete(ctx) {
            return denied;
        }
        Json(
            super::mcp::batch_delete_servers(
                state_of(ctx),
                input.0,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    /// 批量探测 MCP 服务（仅支持指定 ID 列表；按归属隔离）
    async fn batch_probe_mcp_servers(&self, ctx: &Context<'_>, input: Json) -> Json {
        Json(
            super::mcp::batch_probe_servers(
                state_of(ctx),
                input.0,
                &current_user_id(ctx),
                current_is_admin(ctx),
            )
            .await,
        )
    }

    // -------- Shell 权限规则 --------

    /// 创建 Shell 权限规则
    async fn create_shell_rule(&self, ctx: &Context<'_>, input: Json) -> Json {
        let state = state_of(ctx);
        // 全局规则（影响所有用户的 shell 审批）：多用户(auth 启用)模式下仅管理员可改；
        // 单用户(no-auth)模式无身份概念，放行（保持既有行为）。
        if state.auth.is_some() && !current_is_admin(ctx) {
            return Json(super::response::err(
                super::response::code::BUSINESS,
                "仅管理员可管理 Shell 权限规则".to_string(),
            ));
        }
        match &state.shell_rule_store {
            Some(store) => {
                #[derive(serde::Deserialize)]
                struct CreateRuleInput {
                    pattern: String,
                    decision: i16,
                    priority: Option<i32>,
                }
                let req: CreateRuleInput = match serde_json::from_value(input.0) {
                    Ok(r) => r,
                    Err(e) => return parse_err(e),
                };
                let decision = crate::domain::shell_rules::RuleDecision::from_i16(req.decision);
                match store
                    .create(&req.pattern, decision, req.priority.unwrap_or(0))
                    .await
                {
                    Ok(rule) => Json(super::response::ok(
                        serde_json::to_value(&rule).unwrap_or_default(),
                    )),
                    Err(e) => Json(super::response::err(
                        super::response::code::DATABASE,
                        format!("创建失败: {e}"),
                    )),
                }
            }
            None => Json(super::response::err(
                super::response::code::BUSINESS,
                "DB 不可用",
            )),
        }
    }

    /// 删除 Shell 权限规则
    async fn delete_shell_rule(&self, ctx: &Context<'_>, id: String) -> Json {
        let state = state_of(ctx);
        // 全局规则（影响所有用户的 shell 审批）：多用户(auth 启用)模式下仅管理员可改；
        // 单用户(no-auth)模式无身份概念，放行（保持既有行为）。
        if state.auth.is_some() && !current_is_admin(ctx) {
            return Json(super::response::err(
                super::response::code::BUSINESS,
                "仅管理员可管理 Shell 权限规则".to_string(),
            ));
        }
        match &state.shell_rule_store {
            Some(store) => match store.delete(&id).await {
                Ok(true) => Json(super::response::ok(json!({"deleted": true}))),
                Ok(false) => Json(super::response::err(
                    super::response::code::NOT_FOUND,
                    "规则不存在",
                )),
                Err(e) => Json(super::response::err(
                    super::response::code::DATABASE,
                    format!("删除失败: {e}"),
                )),
            },
            None => Json(super::response::err(
                super::response::code::BUSINESS,
                "DB 不可用",
            )),
        }
    }

    // -------- Skill --------

    /// 热重载 Skill 目录(重新扫描磁盘并替换内存 catalog)
    async fn reload_skills(&self, ctx: &Context<'_>) -> Json {
        let state = state_of(ctx);
        match &state.skill_service {
            Some(svc) => match svc.reload() {
                Ok(()) => Json(super::response::ok(json!({ "reloaded": true }))),
                Err(e) => Json(super::response::err(
                    super::response::code::BUSINESS,
                    format!("skill 重载失败: {e}"),
                )),
            },
            None => Json(super::response::err(
                super::response::code::BUSINESS,
                "Skill 服务未初始化",
            )),
        }
    }
}
