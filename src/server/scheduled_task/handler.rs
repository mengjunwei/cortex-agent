//! 定时任务 REST API（`/api/scheduled-tasks/*`）。
//!
//! 权限与现有体系一致：全部需登录（`AuthUser`）；CRUD 按 `user_id` 归属校验，admin 可管所有；
//! 删除走 API Token 守卫（Bearer 认证仅允许删除会话，删任务被拒，对齐 GraphQL
//! `reject_api_token_delete`）。关键写操作 + 触发结果写审计。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{Value, json};

use super::runner_core;
use super::scheduler::{SchedulerEngine, validate_cron};
use crate::domain::scheduled_task::ScheduledTaskInput;
use crate::domain::auth::AuthUser as AuthUserModel;
use crate::server::AppState;
use crate::server::auth::AuthUser;
use crate::server::{assistant, response};
use crate::server::response::code;

// ============ 请求 DTO ============

#[derive(Debug, Deserialize)]
pub struct ParseScheduleRequest {
    /// 自然语言调度描述（如"每天早上9点"）
    pub schedule_nl: String,
    /// 时区（可选，默认 Asia/Shanghai）
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub assistant_id: String,
    pub name: String,
    pub instruction: String,
    /// cron 表达式（与 schedule_nl 二选一；都填时以 schedule_cron 为准）
    #[serde(default)]
    pub schedule_cron: Option<String>,
    /// 自然语言调度（无 schedule_cron 时由后端转 cron）
    #[serde(default)]
    pub schedule_nl: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTaskRequest {
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub instruction: Option<String>,
    #[serde(default)]
    pub schedule_cron: Option<String>,
    #[serde(default)]
    pub schedule_nl: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

// ============ 内部工具 ============

fn err_resp(code: i32, msg: impl Into<String>) -> Response {
    Json(response::err(code, msg)).into_response()
}

fn store_unavailable() -> Response {
    err_resp(code::DATABASE, "定时任务存储不可用（数据库未启用）")
}

fn engine_unavailable() -> Response {
    err_resp(code::DATABASE, "定时任务调度器未初始化")
}

/// 判定请求是否经 `Authorization: Bearer`（API Token）认证。用于删除守卫：
/// API Token 仅允许删除会话，删任务被拒（对齐 GraphQL `reject_api_token_delete`）。
fn via_api_token(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split_once(' ').map(|(s, _)| s.eq_ignore_ascii_case("bearer")).unwrap_or(false))
        .unwrap_or(false)
}

/// 归属校验：返回 true 表示可访问（归属人 或 admin）。
fn can_access(task_owner: &str, user: &AuthUserModel) -> bool {
    user.is_admin || task_owner == user.user_id
}

/// 校验助手对当前用户可见（创建/更新任务的前置；执行时也会再校验一次）。
async fn check_assistant_readable(
    state: &AppState,
    assistant_id: &str,
    user: &AuthUserModel,
) -> Result<(), Response> {
    let Some(store) = state.assistant_store.clone() else {
        return Err(err_resp(code::DATABASE, "助手存储不可用"));
    };
    match store.get(assistant_id).await {
        Ok(Some(a)) => {
            if assistant::can_read(&a, &user.user_id, user.is_admin) {
                Ok(())
            } else {
                Err(err_resp(code::BUSINESS, "无权使用该助手"))
            }
        }
        Ok(None) => Err(err_resp(code::NOT_FOUND, "助手不存在")),
        Err(e) => Err(err_resp(code::DATABASE, format!("加载助手失败: {e}"))),
    }
}

// ============ NL → cron ============

/// `POST /parse-schedule` — 自然语言转 cron（预览确认后由前端创建任务）。
pub async fn parse_schedule(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Json(body): Json<ParseScheduleRequest>,
) -> Response {
    let nl = body.schedule_nl.trim();
    if nl.is_empty() {
        return err_resp(code::INVALID_PARAMS, "调度描述不能为空");
    }
    let tz = body
        .timezone
        .clone()
        .unwrap_or_else(|| "Asia/Shanghai".to_string());

    match nl_to_cron(&state, &user.user_id, nl, &tz).await {
        Ok(cron) => {
            let preview = SchedulerEngine::preview(&cron, &tz, 3).unwrap_or_default();
            let human = humanize_cron(&cron, &tz);
            Json(response::ok(json!({
                "cron": cron,
                "timezone": tz,
                "human": human,
                "next_runs": preview,
            })))
            .into_response()
        }
        Err(msg) => err_resp(code::PARSE_ERROR, msg),
    }
}

/// 用默认模型把自然语言调度转成 cron 表达式（5 段：分 时 日 月 周）。
/// `pub(crate)`：会话内 manage_scheduled_task 工具复用。
pub(crate) async fn nl_to_cron(
    state: &AppState,
    user_id: &str,
    nl: &str,
    tz: &str,
) -> Result<String, String> {
    use tokio_stream::StreamExt;
    let Some(store) = state.model_provider_store.clone() else {
        return Err("模型供应商存储不可用".to_string());
    };
    let model = crate::llm::make_model_by_id(&store, None, user_id)
        .map_err(|e| format!("解析默认模型失败: {e}"))?;

    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %A").to_string();
    let prompt = format!(
        "你是 cron 表达式转换器。把用户的中文调度描述转成标准 5 段 cron 表达式（分 时 日 月 周）。\n\
         当前时间：{now}（时区 {tz}）。\n\
         规则：\n\
         - 只输出 cron 表达式本身，不要任何解释、代码块或额外文字。\n\
         - \"每天早上9点\" → `0 9 * * *`；\"每5分钟\" → `*/5 * * * *`；\"每周一上午8点半\" → `30 8 * * 1`；\
         \"每月1号凌晨2点\" → `0 2 1 * *`；\"每小时\" → `0 * * * *`。\n\
         - 无法识别为周期调度时，输出 `INVALID`。\n\
         用户描述：{nl}"
    );

    let request = adk_rust::LlmRequest {
        model: model.name().to_string(),
        contents: vec![
            adk_rust::Content::new("system").with_text("你只输出 cron 表达式或 INVALID，仅此而已。"),
            adk_rust::Content::new("user").with_text(&prompt),
        ],
        tools: std::collections::HashMap::new(),
        config: Some(adk_rust::GenerateContentConfig {
            temperature: Some(0.0),
            max_output_tokens: Some(64),
            ..Default::default()
        }),
        previous_response_id: None,
    };

    let mut text = String::new();
    match model.generate_content(request, false).await {
        Ok(mut stream) => {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(resp) => {
                        if let Some(c) = &resp.content {
                            for p in &c.parts {
                                if let Some(t) = p.text() {
                                    text.push_str(t);
                                }
                            }
                        }
                        if resp.turn_complete {
                            break;
                        }
                    }
                    Err(e) => return Err(format!("模型调用失败: {e}")),
                }
            }
        }
        Err(e) => return Err(format!("模型调用失败: {e}")),
    }

    // 提取 cron：去代码块/空白，取第一个非空行。
    let cleaned = text
        .trim()
        .trim_start_matches("```cron")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("invalid") {
        return Err("无法识别为周期调度，请换个说法（如\"每天早上9点\"、\"每5分钟\"）".to_string());
    }
    // 校验合法性（顺带规范化空格）。
    validate_cron(&cleaned, tz).map_err(|e| e.to_string())?;
    Ok(cleaned)
}

/// 生成 cron 的中文可读描述（简单规则；复杂表达式回退原文）。
/// `pub(crate)`：会话内工具复用。
pub(crate) fn humanize_cron(cron: &str, _tz: &str) -> String {
    let f: Vec<&str> = cron.split_whitespace().collect();
    if f.len() != 5 {
        return cron.to_string();
    }
    let (min, hour, dom, mon, dow) = (f[0], f[1], f[2], f[3], f[4]);
    let wd = |s: &str| -> String {
        match s {
            "0" | "7" => "周日",
            "1" => "周一",
            "2" => "周二",
            "3" => "周三",
            "4" => "周四",
            "5" => "周五",
            "6" => "周六",
            _ => s,
        }
        .to_string()
    };
    if dom == "*" && mon == "*" && dow == "*" {
        if min == "*" && hour == "*" {
            "每分钟".to_string()
        } else if hour == "*" {
            format!("每小时第 {min} 分")
        } else {
            format!("每天 {hour}:{:0>2}", min)
        }
    } else if dom == "*" && mon == "*" {
        format!("每{} {:02}:{:02}", wd(dow), hour.parse::<u32>().unwrap_or(0), min.parse::<u32>().unwrap_or(0))
    } else if dow == "*" && mon == "*" {
        format!("每月 {} 号 {:02}:{:02}", dom, hour.parse::<u32>().unwrap_or(0), min.parse::<u32>().unwrap_or(0))
    } else {
        cron.to_string()
    }
}

// ============ CRUD ============

/// `POST /` — 创建任务。
pub async fn create_task(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateTaskRequest>,
) -> Response {
    let Some(store) = state.scheduled_task_store.clone() else {
        return store_unavailable();
    };
    let Some(engine) = state.scheduler() else {
        return engine_unavailable();
    };

    if let Err(r) = check_assistant_readable(&state, &body.assistant_id, &user).await {
        return r;
    }
    if body.name.trim().is_empty() || body.instruction.trim().is_empty() {
        return err_resp(code::INVALID_PARAMS, "任务名与指令不能为空");
    }

    let tz = body.timezone.clone().unwrap_or_else(|| "Asia/Shanghai".to_string());
    // cron：优先显式 schedule_cron，否则 NL 转换。
    let cron = match (body.schedule_cron.clone(), body.schedule_nl.clone()) {
        (Some(c), _) if !c.trim().is_empty() => c.trim().to_string(),
        (_, Some(nl)) if !nl.trim().is_empty() => {
            match nl_to_cron(&state, &user.user_id, nl.trim(), &tz).await {
                Ok(c) => c,
                Err(msg) => return err_resp(code::PARSE_ERROR, msg),
            }
        }
        _ => return err_resp(code::INVALID_PARAMS, "需提供 schedule_cron 或 schedule_nl"),
    };
    if let Err(e) = validate_cron(&cron, &tz) {
        return err_resp(code::PARSE_ERROR, e.to_string());
    }

    let input = ScheduledTaskInput {
        assistant_id: body.assistant_id.clone(),
        name: body.name.trim().to_string(),
        instruction: body.instruction.trim().to_string(),
        schedule_cron: cron,
        timezone: tz,
    };

    let mut task = match store.insert(&user.user_id, &input).await {
        Ok(t) => t,
        Err(e) => return err_resp(code::DATABASE, format!("创建任务失败: {e}")),
    };

    // 启停：默认启用则注册调度；显式 enabled=false 则保持停用（不注册）。
    let enabled = body.enabled.unwrap_or(true);
    if enabled {
        if let Err(e) = engine.register_job(&task).await {
            let _ = store.delete(&task.id).await;
            return err_resp(code::DATABASE, format!("注册调度失败: {e}"));
        }
    } else if let Err(e) = store.set_enabled(&task.id, false).await {
        tracing::warn!("[scheduled] 初始停用设置失败: {e}");
    }
    // 重新读取（拿到回填的 scheduler_job_id / next_run_at / enabled）。
    if let Ok(Some(t)) = store.get(&task.id).await {
        task = t;
    }

    audit(&state, &user, "create_scheduled_task", &task.id, true, &task.name);
    let name = assistant_name(&state, &task.assistant_id).await;
    Json(response::ok(serde_json::to_value(task.to_dto(&name)).unwrap_or_default())).into_response()
}

/// `GET /` — 列表（归属人；admin 全部）。
/// 分页查询参数（page 从 1 开始；page_size 缺省 10，上限 100）。
#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

impl PageQuery {
    fn norm(&self) -> (i64, i64) {
        let page = self.page.unwrap_or(1).max(1);
        let size = self.page_size.unwrap_or(10).clamp(1, 100);
        (page, size)
    }
}

/// `GET /` — 列表（分页）。归属过滤：admin 看全部，否则只看自己。
pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Query(q): Query<PageQuery>,
) -> Response {
    let Some(store) = state.scheduled_task_store.clone() else {
        return store_unavailable();
    };
    let (page, page_size) = q.norm();
    match store
        .list_for_owner_paged(&user.user_id, user.is_admin, page, page_size)
        .await
    {
        Ok((tasks, total)) => {
            // 批量取助手名，避免 N+1。
            let names = assistant_names(&state, &tasks.iter().map(|t| t.assistant_id.clone()).collect::<Vec<_>>()).await;
            let dtos: Vec<Value> = tasks
                .iter()
                .map(|t| {
                    let n = names.get(&t.assistant_id).cloned().unwrap_or_default();
                    serde_json::to_value(t.to_dto(&n)).unwrap_or_default()
                })
                .collect();
            Json(response::ok(json!({ "tasks": dtos, "total": total, "page": page, "page_size": page_size }))).into_response()
        }
        Err(e) => err_resp(code::DATABASE, format!("查询任务失败: {e}")),
    }
}

/// `GET /{id}` — 详情。
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Some(store) = state.scheduled_task_store.clone() else {
        return store_unavailable();
    };
    match store.get(&id).await {
        Ok(Some(task)) => {
            if !can_access(&task.user_id, &user) {
                return err_resp(code::BUSINESS, "无权访问该任务");
            }
            let name = assistant_name(&state, &task.assistant_id).await;
            Json(response::ok(serde_json::to_value(task.to_dto(&name)).unwrap_or_default())).into_response()
        }
        Ok(None) => err_resp(code::NOT_FOUND, "任务不存在"),
        Err(e) => err_resp(code::DATABASE, format!("查询任务失败: {e}")),
    }
}

/// `PATCH /{id}` — 更新（名称/指令/助手/cron/时区/启停）。
pub async fn update_task(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdateTaskRequest>,
) -> Response {
    let Some(store) = state.scheduled_task_store.clone() else {
        return store_unavailable();
    };
    let Some(engine) = state.scheduler() else {
        return engine_unavailable();
    };
    let Some(mut task) = (match store.get(&id).await {
        Ok(t) => t,
        Err(e) => return err_resp(code::DATABASE, format!("查询任务失败: {e}")),
    }) else {
        return err_resp(code::NOT_FOUND, "任务不存在");
    };
    if !can_access(&task.user_id, &user) {
        return err_resp(code::BUSINESS, "无权修改该任务");
    }

    // 解析新值（缺省沿用旧值）。
    let new_assistant = body.assistant_id.clone().unwrap_or_else(|| task.assistant_id.clone());
    let new_name = body.name.clone().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| task.name.clone());
    let new_instruction = body.instruction.clone().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| task.instruction.clone());
    let new_tz = body.timezone.clone().unwrap_or_else(|| task.timezone.clone());
    let cron_changed = body.schedule_cron.is_some() || body.schedule_nl.is_some();
    let new_cron = match (body.schedule_cron.clone(), body.schedule_nl.clone()) {
        (Some(c), _) if !c.trim().is_empty() => c.trim().to_string(),
        (_, Some(nl)) if !nl.trim().is_empty() => {
            match nl_to_cron(&state, &user.user_id, nl.trim(), &new_tz).await {
                Ok(c) => c,
                Err(msg) => return err_resp(code::PARSE_ERROR, msg),
            }
        }
        _ => task.schedule_cron.clone(),
    };

    if let Err(r) = check_assistant_readable(&state, &new_assistant, &user).await {
        return r;
    }
    if let Err(e) = validate_cron(&new_cron, &new_tz) {
        return err_resp(code::PARSE_ERROR, e.to_string());
    }

    let input = ScheduledTaskInput {
        assistant_id: new_assistant,
        name: new_name,
        instruction: new_instruction,
        schedule_cron: new_cron.clone(),
        timezone: new_tz.clone(),
    };
    if let Err(e) = store.update_fields(&id, &input).await {
        return err_resp(code::DATABASE, format!("更新任务失败: {e}"));
    }

    // 启停变更。
    let new_enabled = body.enabled.unwrap_or(task.enabled);
    if let Err(e) = store.set_enabled(&id, new_enabled).await {
        return err_resp(code::DATABASE, format!("更新启停失败: {e}"));
    }

    // 调度器同步：cron/时区变了 或 启停变了 → remove + (enabled ? add)。
    let schedule_changed = cron_changed || new_tz != task.timezone;
    if schedule_changed || new_enabled != task.enabled {
        if let Err(e) = engine.remove_job(&id).await {
            tracing::warn!("[scheduled] 更新时移除旧 job 失败: {e}");
        }
        if new_enabled {
            task.assistant_id = input.assistant_id.clone();
            task.schedule_cron = new_cron.clone();
            task.timezone = new_tz.clone();
            if let Err(e) = engine.register_job(&task).await {
                return err_resp(code::DATABASE, format!("重新注册调度失败: {e}"));
            }
        }
    }

    if let Ok(Some(t)) = store.get(&id).await {
        task = t;
    }
    audit(&state, &user, "update_scheduled_task", &id, true, &task.name);
    let name = assistant_name(&state, &task.assistant_id).await;
    Json(response::ok(serde_json::to_value(task.to_dto(&name)).unwrap_or_default())).into_response()
}

/// `DELETE /{id}` — 删除（API Token 拒绝）。
pub async fn delete_task(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    // API Token 删除守卫（对齐 reject_api_token_delete：Bearer 仅允许删会话）。
    if via_api_token(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(response::err(code::BUSINESS, "API Token 认证不支持删除定时任务，请使用账号登录")),
        )
            .into_response();
    }
    let Some(store) = state.scheduled_task_store.clone() else {
        return store_unavailable();
    };
    let Some(engine) = state.scheduler() else {
        return engine_unavailable();
    };
    let Some(task) = (match store.get(&id).await {
        Ok(t) => t,
        Err(e) => return err_resp(code::DATABASE, format!("查询任务失败: {e}")),
    }) else {
        return err_resp(code::NOT_FOUND, "任务不存在");
    };
    if !can_access(&task.user_id, &user) {
        return err_resp(code::BUSINESS, "无权删除该任务");
    }

    if let Err(e) = engine.remove_job(&id).await {
        tracing::warn!("[scheduled] 删除时移除 job 失败: {e}");
    }
    match store.delete(&id).await {
        Ok(true) => {
            audit(&state, &user, "delete_scheduled_task", &id, true, &task.name);
            Json(response::ok(json!({ "deleted": true, "id": id }))).into_response()
        }
        Ok(false) => err_resp(code::NOT_FOUND, "任务不存在"),
        Err(e) => err_resp(code::DATABASE, format!("删除任务失败: {e}")),
    }
}

/// `GET /{id}/runs` — 近 30 天运行历史（分页）。
pub async fn list_runs(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let Some(store) = state.scheduled_task_store.clone() else {
        return store_unavailable();
    };
    let Some(task) = (match store.get(&id).await {
        Ok(t) => t,
        Err(e) => return err_resp(code::DATABASE, format!("查询任务失败: {e}")),
    }) else {
        return err_resp(code::NOT_FOUND, "任务不存在");
    };
    if !can_access(&task.user_id, &user) {
        return err_resp(code::BUSINESS, "无权访问该任务");
    }
    let Some(ss) = state.session_settings_store.clone() else {
        return err_resp(code::DATABASE, "会话配置存储不可用");
    };
    let (page, page_size) = q.norm();
    match ss.list_scheduled_runs_paged(&id, page, page_size).await {
        Ok((runs, total)) => Json(response::ok(json!({ "runs": runs, "total": total, "page": page, "page_size": page_size }))).into_response(),
        Err(e) => err_resp(code::DATABASE, format!("查询运行历史失败: {e}")),
    }
}

/// `POST /{id}/run-now` — 立即手动触发一次（调试）。
pub async fn run_now(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Some(store) = state.scheduled_task_store.clone() else {
        return store_unavailable();
    };
    let Some(task) = (match store.get(&id).await {
        Ok(t) => t,
        Err(e) => return err_resp(code::DATABASE, format!("查询任务失败: {e}")),
    }) else {
        return err_resp(code::NOT_FOUND, "任务不存在");
    };
    if !can_access(&task.user_id, &user) {
        return err_resp(code::BUSINESS, "无权操作该任务");
    }
    // 停用任务不允许手动触发（前端按钮也已禁用，此处为服务端兜底）。
    if !task.enabled {
        return err_resp(code::BUSINESS, "任务已停用，请先启用");
    }

    let state2 = state.clone();
    let tid = id.clone();
    tokio::spawn(async move {
        runner_core::run_scheduled_task(state2, &tid, "manual").await;
    });
    audit(&state, &user, "run_scheduled_task_now", &id, true, &task.name);
    Json(response::ok(json!({ "triggered": true, "id": id }))).into_response()
}

// ============ 辅助 ============

fn assistant_name_blocking_warn() -> String {
    String::new()
}

async fn assistant_name(state: &AppState, assistant_id: &str) -> String {
    match state.assistant_store.clone() {
        Some(s) => s
            .get(assistant_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.name)
            .unwrap_or_default(),
        None => assistant_name_blocking_warn(),
    }
}

async fn assistant_names(state: &AppState, ids: &[String]) -> std::collections::HashMap<String, String> {
    match state.assistant_store.clone() {
        Some(s) => s
            .get_batch(ids)
            .await
            .map(|m| m.into_iter().map(|(k, a)| (k, a.name)).collect())
            .unwrap_or_default(),
        None => Default::default(),
    }
}

fn audit(state: &AppState, user: &AuthUserModel, op: &str, target: &str, success: bool, detail: &str) {
    crate::domain::audit::spawn_record(
        state.audit_store.as_ref(),
        crate::domain::audit::AuditEntry {
            user_id: user.user_id.clone(),
            actor: if user.name.is_empty() { user.user_id.clone() } else { user.name.clone() },
            source: "scheduled_task".to_string(),
            operation: op.to_string(),
            target_id: target.to_string(),
            success,
            detail: detail.to_string(),
            ip: String::new(),
        },
    );
}

/// 注册路由（挂到 `/api/scheduled-tasks`）。
pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/scheduled-tasks", post(create_task).get(list_tasks))
        .route("/api/scheduled-tasks/parse-schedule", post(parse_schedule))
        .route("/api/scheduled-tasks/{id}", get(get_task).patch(update_task).delete(delete_task))
        .route("/api/scheduled-tasks/{id}/runs", get(list_runs))
        .route("/api/scheduled-tasks/{id}/run-now", post(run_now))
}
