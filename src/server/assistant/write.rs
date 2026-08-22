//! 助手写操作 — create / update / delete / 复制 / 分享 / fork / 导入导出 / env 明文查看 / AI 生成。
//!
//! 从 assistant.rs 拆出;读操作(list/get/explore/tools)与 DTO/权限校验留在 mod.rs。

use super::*;

// ===========================================================================
// Mutation resolvers
// ===========================================================================

/// 创建自定义助手
pub async fn create_assistant(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    input: &Value,
) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let req: WriteAssistantRequest = match serde_json::from_value(input.clone()) {
        Ok(r) => r,
        Err(e) => return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}")),
    };
    let input_data = match req.to_input() {
        Ok(i) => i,
        Err((c, m)) => return business_err_obj(c, m),
    };
    // 跨实体校验：kb_instance_id 必须对调用者可见（防绑定他人私有知识库，运行时跨用户读）
    if let Err(v) = validate_kb_readable(
        state,
        input_data.kb_instance_id.as_deref(),
        user_id,
        is_admin,
    )
    .await
    {
        return v;
    }
    // creator = 当前登录用户（新助手归属真实 user_id）
    match store.create_custom(&input_data, user_id).await {
        Ok(id) => {
            tracing::info!(target: "assistant", "create_assistant name={} → id={}", input_data.name, id);
            response::ok(json!({ "id": id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "create_assistant 失败: {e}");
            response::err(code::DATABASE, format!("创建失败: {e}"))
        }
    }
}

/// 更新自定义助手
pub async fn update_assistant(
    state: &AppState,
    id: &str,
    user_id: &str,
    is_admin: bool,
    input_val: &Value,
) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let req: WriteAssistantRequest = match serde_json::from_value(input_val.clone()) {
        Ok(r) => r,
        Err(e) => return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}")),
    };
    let input_data = match req.to_input() {
        Ok(i) => i,
        Err((c, m)) => return business_err_obj(c, m),
    };
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, user_id, is_admin) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "update_assistant 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }
    // 跨实体校验：kb_instance_id 必须对调用者可见（防绑定他人私有知识库，运行时跨用户读）
    if let Err(v) = validate_kb_readable(
        state,
        input_data.kb_instance_id.as_deref(),
        user_id,
        is_admin,
    )
    .await
    {
        return v;
    }
    match store.update_custom(id, &input_data).await {
        Ok(true) => response::ok(json!({ "updated": true })),
        Ok(false) => response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "update_assistant {id} 失败: {e}");
            response::err(code::DATABASE, format!("更新失败: {e}"))
        }
    }
}

/// 删除自定义助手（两步合一）
///
/// - `force=false`（默认）：dry-run 预检，返回引用影响清单，不删除
/// - `force=true`：单个事务内级联清理所有引用（保留引用方主体），再删除助手
pub async fn delete_assistant(
    state: &AppState,
    id: &str,
    user_id: &str,
    is_admin: bool,
    force: bool,
) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    // 权限/存在性校验：无论是否 force，都先确认助手存在且可写
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, user_id, is_admin) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "delete_assistant 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }

    if !force {
        // 预检：返回影响清单，不删除
        match store.impact_of_delete(id).await {
            Ok(impact) => response::ok(json!({
                "deleted": false,
                "impact": {
                    "sessions": impact.sessions,
                    "memories": impact.memories,
                    "memory_proposals": impact.memory_proposals,
                },
                "summary": summarize_assistant_impact(&impact),
            })),
            Err(e) => {
                tracing::error!(target: "assistant", "delete_assistant 预检 {id} 失败: {e}");
                response::err(code::DATABASE, format!("预检失败: {e}"))
            }
        }
    } else {
        // 执行：事务内级联清理 + 删除
        match store.delete_with_cleanup(id).await {
            Ok(res) if res.deleted => response::ok(json!({
                "deleted": true,
                "cleanup": {
                    "sessions_unbound": res.sessions_unbound,
                    "memories_downgraded": res.memories_downgraded,
                    "proposals_removed": res.proposals_removed,
                },
            })),
            Ok(_) => response::err(code::NOT_FOUND, "助手不存在或为内置（不可删除）"),
            Err(e) => {
                tracing::error!(target: "assistant", "delete_assistant {id} 失败: {e}");
                response::err(code::DATABASE, format!("删除失败: {e}"))
            }
        }
    }
}

/// 把预检影响计数转成人类可读摘要，供前端确认框直接展示。
fn summarize_assistant_impact(
    impact: &crate::domain::assistant::store::AssistantDeletionImpact,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if impact.sessions > 0 {
        parts.push(format!(
            "{} 个会话将解除助手绑定（回退默认助手）",
            impact.sessions
        ));
    }
    if impact.memories > 0 {
        parts.push(format!(
            "{} 条助手级记忆将降级为用户级（不丢失）",
            impact.memories
        ));
    }
    if impact.memory_proposals > 0 {
        parts.push(format!("{} 条记忆建议将被清理", impact.memory_proposals));
    }
    if parts.is_empty() {
        "无关联数据，可直接删除".to_string()
    } else {
        parts.join("；")
    }
}

/// 设置助手绑定的知识库实例（builtin/custom 均可）
///
/// 设备命令类助手靠 kb_instance_id 注入 search_kb，属运行时配置；
/// 通过此接口单独更新 kb_instance_id。写权限由 assert_kb_writable 鉴权。
pub async fn set_kb_instance(
    state: &AppState,
    assistant_id: &str,
    kb_instance_id: Option<&str>,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    // 归属校验：仅归属人/管理员可改助手绑定的知识库（防他人篡改绑定）
    let a = match store.get(assistant_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "set_kb_instance get {assistant_id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    };
    if let Err((c, m)) = assert_kb_writable(&a, user_id, is_admin) {
        return response::err(c, m);
    }
    // 跨实体校验：待绑定知识库实例必须对调用者可见（私有仅归属人/管理员；公开人人可读），
    // 防止绑定他人私有知识库（运行时 search_kb 会读取，写时不校验则可跨用户读他人私有知识库）。
    if let Err(v) = validate_kb_readable(state, kb_instance_id, user_id, is_admin).await {
        return v;
    }
    match store.set_kb_instance(assistant_id, kb_instance_id).await {
        Ok(true) => response::ok(json!({ "updated": true })),
        Ok(false) => response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "set_kb_instance {assistant_id} 失败: {e}");
            response::err(code::DATABASE, format!("更新失败: {e}"))
        }
    }
}

/// 复制助手为自定义副本。
///
/// 归属校验（[`caller_owns`]）：仅本人 / 管理员可复制 custom 源——因为副本会继承源助手的
/// env_vars（含密钥），这是「自有副本」语义，与 fork（公开助手跨用户复制、不携带密钥）故意不同。
/// 内置助手归属管理员 `marvelnet`，仅管理员可复制；普通用户应改用 fork 内置/公开助手。
pub async fn duplicate_assistant(
    state: &AppState,
    id: &str,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    // 归属校验：仅归属人/管理员可复制（副本继承源 env_vars，属「自有副本」语义）。
    // builtin 源归属管理员，仅管理员可复制；普通用户应改用 fork。
    match store.get(id).await {
        Ok(Some(a)) => {
            if !caller_owns(&a, user_id, is_admin) {
                return response::err(code::BUSINESS, "无权复制他人创建的助手");
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "duplicate 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }
    // 副本归属 = 当前用户（真实 user_id）
    match store.duplicate_builtin(id, user_id).await {
        Ok(new_id) => {
            tracing::info!(target: "assistant", "duplicate {id} → {new_id} (user_id={user_id})");
            response::ok(json!({ "id": new_id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "duplicate {id} 失败: {e}");
            let code_v = match &e {
                crate::error::AppError::BusinessError(_) => code::BUSINESS,
                _ => code::DATABASE,
            };
            business_err_obj(code_v, format!("{}", e))
        }
    }
}

/// 生成/续用分享口令
pub async fn share_enable(state: &AppState, id: &str, user_id: &str, is_admin: bool) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, user_id, is_admin) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "share_enable 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }
    match store.ensure_share_token(id).await {
        Ok(token) => {
            tracing::info!(target: "assistant", "share_enable {id} → token={token}");
            response::ok(json!({ "share_token": token }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "share_enable {id} 失败: {e}");
            response::err(code::DATABASE, format!("生成口令失败: {e}"))
        }
    }
}

/// 关闭分享口令
pub async fn share_disable(state: &AppState, id: &str, user_id: &str, is_admin: bool) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, user_id, is_admin) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "share_disable 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }
    match store.clear_share_token(id).await {
        Ok(_) => response::ok(json!({ "cleared": true })),
        Err(e) => {
            tracing::error!(target: "assistant", "share_disable {id} 失败: {e}");
            response::err(code::DATABASE, format!("关闭分享失败: {e}"))
        }
    }
}

/// Fork 公开/分享助手到本地
pub async fn fork_assistant(state: &AppState, id: &str, user_id: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    // fork 副本归属 = 当前用户
    match store.fork(id, user_id).await {
        Ok(new_id) => {
            tracing::info!(target: "assistant", "fork {id} → {new_id}");
            response::ok(json!({ "id": new_id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "fork {id} 失败: {e}");
            let code_v = match &e {
                crate::error::AppError::BusinessError(_) => code::BUSINESS,
                _ => code::DATABASE,
            };
            business_err_obj(code_v, format!("{}", e))
        }
    }
}

/// 导出助手为 JSON
pub async fn export_one(state: &AppState, id: &str, user_id: &str, is_admin: bool) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_exportable(&a, user_id, is_admin) {
                return business_err_obj(c, m);
            }
            let visibility_str = match a.visibility {
                Visibility::Private => "private",
                Visibility::Shared => "shared",
                Visibility::Builtin => "builtin",
            };
            response::ok(json!({
                "schema": "cortex-agent.assistant.v1",
                "name": a.name,
                "description": a.description,
                "avatar": a.avatar,
                "kind": "custom",
                "agent_type": "custom",
                "visibility": visibility_str,
                "system_prompt": a.system_prompt,
                "model_id": a.model_id,
                "temperature": a.temperature,
                "top_p": a.top_p,
                "max_tokens": a.max_tokens,
                "enabled_tools": a.enabled_tools,
                // skill 白名单随导出（跨实例 skill 名同构；漏掉会让副本静默放宽为全部可见）
                "enabled_skills": a.enabled_skills,
                // MCP 绑定随导出：import 端 WriteAssistantRequest.enabled_mcps 能接收，
                // 漏掉会让同实例导出→导入（复制助手）静默丢失 MCP 工具。
                "enabled_mcps": a.enabled_mcps,
                "knowledge_enabled": a.knowledge_enabled,
                "kb_instance_id": a.kb_instance_id,
                "greeting": a.greeting,
            }))
        }
        Ok(None) => response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "export_one {id} 失败: {e}");
            response::err(code::DATABASE, format!("导出失败: {e}"))
        }
    }
}

/// 导入助手 JSON
pub async fn import_one(state: &AppState, user_id: &str, v: &Value) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let schema = v.get("schema").and_then(|x| x.as_str()).unwrap_or("");
    if schema != "cortex-agent.assistant.v1" {
        return business_err_obj(
            code::INVALID_PARAMS,
            "schema 不兼容（仅支持 cortex-agent.assistant.v1）",
        );
    }
    let mut cleaned = v.clone();
    if let Some(obj) = cleaned.as_object_mut() {
        obj.remove("schema");
        obj.remove("kind");
        obj.remove("agent_type");
        obj.remove("visibility");
    }
    let mut req: WriteAssistantRequest = match serde_json::from_value(cleaned) {
        Ok(r) => r,
        Err(e) => {
            return business_err_obj(code::INVALID_PARAMS, format!("导入数据格式错误: {e}"));
        }
    };
    req.visibility = 0;
    if req.name.trim().is_empty() {
        req.name = "导入助手".to_string();
    }
    if req.avatar.trim().is_empty() {
        req.avatar = "🤖".to_string();
    }
    let input_data = match req.to_input() {
        Ok(i) => i,
        Err((c, m)) => return business_err_obj(c, m),
    };
    // 导入副本归属 = 当前用户
    match store.create_custom(&input_data, user_id).await {
        Ok(new_id) => {
            tracing::info!(target: "assistant", "import_one → id={new_id}");
            response::ok(json!({ "id": new_id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "import_one 失败: {e}");
            response::err(code::DATABASE, format!("导入失败: {e}"))
        }
    }
}

// 静态引用以避免未使用告警
const _KIND_REF: AssistantKind = AssistantKind::Builtin;
const _AGENT_TYPE_REF: AgentType = AgentType::Custom;

// ===========================================================================
// 环境变量明文查看（二次密码确认）
// ===========================================================================

/// 查看助手环境变量明文。
///
/// 鉴权：
/// - **认证启用**：二次输入当前登录用户密码（防 CSRF / 他人趁开着的会话偷看）+ 归属校验
///   （[`caller_owns`]：本人 / 管理员）。
/// - **认证未启用**：单用户本地部署，无密码可校验、也无用户隔离 → 直接放行（否则存量
///   env_vars 永久冻结无法编辑）。
///
/// 解密失败显式返回错误（[`crate::domain::assistant::store::EnvVarsReveal::Unreadable`]），
/// 绝不静默成空——否则前端解锁拿到空、一保存就会覆盖原密文永久丢密钥。
pub async fn reveal_env_vars(
    state: &AppState,
    assistant_id: &str,
    user_id: &str,
    is_admin: bool,
    password: &str,
) -> Value {
    // 认证启用：二次密码确认（DB 错误用专门 code，区别于「密码错误」）
    if let Some(auth) = &state.auth {
        if user_id.is_empty() {
            return response::err(code::UNAUTHORIZED, "未登录");
        }
        match auth.verify_user_password(user_id, password).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    target: "assistant",
                    "reveal_env_vars 密码/账号校验未通过 user_id={user_id} assistant_id={assistant_id}"
                );
                return response::err(code::UNAUTHORIZED, "密码错误或账号已禁用");
            }
            Err(e) => {
                tracing::error!(target: "assistant", "reveal_env_vars 密码校验异常: {e}");
                return response::err(code::DATABASE, format!("校验失败: {e}"));
            }
        }
    }
    // else: 认证未启用（单用户本地）→ 跳过密码，直接进入归属/取值

    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };

    // 归属校验（多用户隔离；内置助手归属管理员 marvelnet）
    match store.get(assistant_id).await {
        Ok(Some(a)) => {
            if !caller_owns(&a, user_id, is_admin) {
                return response::err(code::BUSINESS, "无权查看该助手的环境变量");
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "reveal_env_vars 查询 {assistant_id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }

    // 三态取值：NotFound / Unreadable / Ok
    match store.reveal_env_vars(assistant_id).await {
        Ok(crate::domain::assistant::store::EnvVarsReveal::Ok(map)) => {
            tracing::info!(
                target: "assistant",
                "reveal_env_vars 成功 user_id={user_id} assistant_id={assistant_id} ({} 个变量)",
                map.len()
            );
            response::ok(json!({ "env_vars": map }))
        }
        Ok(crate::domain::assistant::store::EnvVarsReveal::Unreadable) => response::err(
            code::BUSINESS,
            "环境变量无法解密（加密密钥可能已变更），请联系管理员；切勿在此状态保存以免覆盖",
        ),
        Ok(crate::domain::assistant::store::EnvVarsReveal::NotFound) => {
            response::err(code::NOT_FOUND, "助手不存在")
        }
        Err(e) => {
            tracing::error!(target: "assistant", "reveal_env_vars {assistant_id} 失败: {e}");
            response::err(code::DATABASE, format!("查询失败: {e}"))
        }
    }
}

// ===========================================================================
// AI 智能生成助手草稿
// ===========================================================================

/// 依据用户模糊需求描述，让 LLM 自动生成助手的 name/description/system_prompt/greeting 四字段。
///
/// 只返回草稿，不落库；前端拿到后填充到编辑表单，用户可再编辑后再保存。
pub async fn generate_assistant(state: &AppState, input: &Value, user_id: &str) -> Value {
    #[derive(Deserialize)]
    struct Req {
        prompt: String,
        #[serde(default)]
        model_id: Option<String>,
    }

    let req: Req = match serde_json::from_value(input.clone()) {
        Ok(r) => r,
        Err(e) => return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}")),
    };
    if req.prompt.trim().is_empty() {
        return response::err(code::INVALID_PARAMS, "prompt 不能为空");
    }

    let model_id = req.model_id.as_deref().filter(|s| !s.trim().is_empty());
    let model = match state.require_model_store() {
        Ok(store) => match crate::llm::make_model_by_id(store, model_id, user_id) {
            Ok(m) => m,
            Err(e) => return response::err(code::LLM, format!("创建模型失败: {e}")),
        },
        Err(e) => return response::err(code::LLM, format!("创建模型失败: {e}")),
    };

    match crate::agent::assistant_generator::generate(model, &req.prompt).await {
        Ok(draft) => response::ok(json!({
            "name": draft.name,
            "description": draft.description,
            "system_prompt": draft.system_prompt,
            "greeting": draft.greeting,
        })),
        Err(e) => response::err(code::LLM, format!("生成失败: {e}")),
    }
}

