//! `manage_scheduled_task` 工具 — 会话内自然语言创建/管理定时任务。
//!
//! 常驻注册在每个 custom agent（同 propose_memory，不受 enabled_tools 白名单约束）。
//! 用户在对话中说「帮我每天早上9点出一份XX报表」时，模型调用本工具创建任务。
//! user_id / assistant_id 从 ToolContext + 闭包捕获（对齐 propose_memory 惯例）。
//!
//! 支持动作：create / update / list / toggle / delete / run_now。
//! NL 转 cron 复用 server 层的 [`crate::server::scheduled_task::handler::nl_to_cron`]。

use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::Serialize;

use crate::server::AppState;
use crate::server::scheduled_task::scheduler::validate_cron;
use crate::server::scheduled_task::{handler, runner_core};
use crate::domain::scheduled_task::ScheduledTaskInput;

#[derive(Debug, Serialize, JsonSchema)]
struct ManageScheduledTaskParams {
    /// Action: create / update / list / toggle / delete / run_now
    action: String,
    /// [create/update] Task name (short, e.g. "daily sales report"). Unchanged on update if omitted.
    #[serde(default)]
    name: Option<String>,
    /// [create/update] Instruction: what the agent should do each run. Unchanged on update if omitted.
    #[serde(default)]
    instruction: Option<String>,
    /// [create/update] Natural-language schedule (e.g. "every day at 9am"). Mutually exclusive with schedule_cron.
    #[serde(default)]
    schedule_nl: Option<String>,
    /// [create/update] Standard 5-field cron. Provide directly when known, skipping NL conversion.
    #[serde(default)]
    schedule_cron: Option<String>,
    /// [update/toggle/delete/run_now] Target task id (obtain via list).
    #[serde(default)]
    task_id: Option<String>,
    /// [toggle] Target state: true=enable false=disable
    #[serde(default)]
    enabled: Option<bool>,
}

/// 构造 manage_scheduled_task 工具。
///
/// - `state`：应用依赖（拿 store / scheduler / 模型解析）。
/// - `assistant_id`：当前助手 id（创建任务默认绑定当前助手）。
pub fn create_manage_scheduled_task_tool(state: Arc<AppState>, assistant_id: String) -> FunctionTool {
    FunctionTool::new(
        "manage_scheduled_task",
        "Manage scheduled tasks (run an assistant automatically on a recurring schedule, e.g. \"daily report\"). \
         Use this when the user wants something done periodically/periodic. \
         Actions: create / update / list / toggle / delete / run_now. \
         - create: needs name, instruction (what to do each run), and schedule_nl (e.g. \"every day at 9am\") or schedule_cron (standard 5-field cron). Binds the current assistant. \
         - update: edit an EXISTING task. When the user wants to change a task's time/instruction/name, first call list to get the task_id, then call update with task_id plus only the fields to change (schedule_nl or schedule_cron / instruction / name). NEVER create-then-delete to modify a task. \
         - toggle: enable/disable (task_id + enabled). delete: remove (task_id). run_now: trigger once immediately (task_id). \
         After create/update, report the next run time. Each run produces an independent session, viewable on the task detail page.",
        move |ctx: Arc<dyn ToolContext>, args: Value| {
            let state = state.clone();
            let assistant_id = assistant_id.clone();
            async move {
                let user_id = ctx.user_id().to_string();
                let is_admin = false; // 工具层无 admin 上下文；归属校验按 user_id（agent 只能管自己的任务）
                let action = args["action"].as_str().unwrap_or("").to_lowercase();

                let Some(store) = state.scheduled_task_store.clone() else {
                    return Ok(json!({ "ok": false, "message": "定时任务功能不可用（数据库未启用）" }));
                };

                match action.as_str() {
                    "create" => {
                        let Some(engine) = state.scheduler() else {
                            return Ok(json!({ "ok": false, "message": "调度器未初始化" }));
                        };
                        let name = match args["name"].as_str() {
                            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                            _ => return Ok(json!({ "ok": false, "message": "缺少任务名 name" })),
                        };
                        let instruction = match args["instruction"].as_str() {
                            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                            _ => return Ok(json!({ "ok": false, "message": "缺少执行指令 instruction" })),
                        };
                        let tz = "Asia/Shanghai".to_string();
                        let cron = match (
                            args["schedule_cron"].as_str().filter(|s| !s.trim().is_empty()),
                            args["schedule_nl"].as_str().filter(|s| !s.trim().is_empty()),
                        ) {
                            (Some(c), _) => c.trim().to_string(),
                            (None, Some(nl)) => {
                                match handler::nl_to_cron(&state, &user_id, nl, &tz).await {
                                    Ok(c) => c,
                                    Err(msg) => return Ok(json!({ "ok": false, "message": msg })),
                                }
                            }
                            _ => return Ok(json!({ "ok": false, "message": "需提供 schedule_nl 或 schedule_cron" })),
                        };
                        if let Err(e) = validate_cron(&cron, &tz) {
                            return Ok(json!({ "ok": false, "message": e.to_string() }));
                        }

                        let input = ScheduledTaskInput {
                            assistant_id: assistant_id.clone(),
                            name: name.clone(),
                            instruction: instruction.clone(),
                            schedule_cron: cron.clone(),
                            timezone: tz.clone(),
                        };
                        let task = match store.insert(&user_id, &input).await {
                            Ok(t) => t,
                            Err(e) => return Ok(json!({ "ok": false, "message": format!("创建失败: {e}") })),
                        };
                        if let Err(e) = engine.register_job(&task).await {
                            let _ = store.delete(&task.id).await;
                            return Ok(json!({ "ok": false, "message": format!("注册调度失败: {e}") }));
                        }
                        tool_audit(&state, &user_id, "create_scheduled_task", &task.id, &task.name);
                        let next = engine_next(&task);
                        let human = handler::humanize_cron(&cron, &tz);
                        Ok(json!({
                            "ok": true,
                            "task_id": task.id,
                            "message": format!("定时任务「{}」已创建：{}（cron: {}）。下次运行：{}。结果会生成独立会话，可在定时任务详情页查看。", name, human, cron, next),
                        }))
                    }
                    "list" => {
                        match store.list_for_owner(&user_id, false).await {
                            Ok(tasks) => {
                                if tasks.is_empty() {
                                    return Ok(json!({ "ok": true, "message": "你还没有定时任务。", "tasks": [] }));
                                }
                                let items: Vec<Value> = tasks.iter().map(|t| {
                                    json!({
                                        "task_id": t.id,
                                        "name": t.name,
                                        "instruction": t.instruction,
                                        "cron": t.schedule_cron,
                                        "enabled": t.enabled,
                                        "next_run_at": t.next_run_at.map(|x| x.to_rfc3339()),
                                        "last_run_status": t.last_run_status.map(|s| s.as_i16()),
                                    })
                                }).collect();
                                Ok(json!({ "ok": true, "tasks": items, "message": format!("共 {} 个定时任务。", items.len()) }))
                            }
                            Err(e) => Ok(json!({ "ok": false, "message": format!("查询失败: {e}") })),
                        }
                    }
                    "update" => {
                        let Some(engine) = state.scheduler() else {
                            return Ok(json!({ "ok": false, "message": "调度器未初始化" }));
                        };
                        let tid = args["task_id"].as_str().unwrap_or("");
                        let Some(target) = (match store.get(tid).await {
                            Ok(t) => t,
                            // DB 故障 ≠ 任务不存在：分开报错，避免模型向用户转述错误结论
                            Err(e) => return Ok(json!({ "ok": false, "message": format!("查询任务失败: {e}") })),
                        }) else {
                            return Ok(json!({ "ok": false, "message": "任务不存在（先 list 拿到 task_id）" }));
                        };
                        if target.user_id != user_id && !is_admin {
                            return Ok(json!({ "ok": false, "message": "无权操作该任务" }));
                        }

                        // 缺省沿用旧值；只覆盖传入的字段。
                        let new_name = args["name"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| target.name.clone());
                        let new_instruction = args["instruction"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| target.instruction.clone());
                        let tz = target.timezone.clone();
                        let cron_changed = args["schedule_cron"].as_str().is_some() || args["schedule_nl"].as_str().is_some();
                        let new_cron = match (
                            args["schedule_cron"].as_str().filter(|s| !s.trim().is_empty()),
                            args["schedule_nl"].as_str().filter(|s| !s.trim().is_empty()),
                        ) {
                            (Some(c), _) => c.trim().to_string(),
                            (None, Some(nl)) => match handler::nl_to_cron(&state, &user_id, nl, &tz).await {
                                Ok(c) => c,
                                Err(msg) => return Ok(json!({ "ok": false, "message": msg })),
                            },
                            _ => target.schedule_cron.clone(),
                        };
                        if let Err(e) = validate_cron(&new_cron, &tz) {
                            return Ok(json!({ "ok": false, "message": e.to_string() }));
                        }

                        let input = ScheduledTaskInput {
                            assistant_id: target.assistant_id.clone(),
                            name: new_name.clone(),
                            instruction: new_instruction,
                            schedule_cron: new_cron.clone(),
                            timezone: tz.clone(),
                        };
                        if let Err(e) = store.update_fields(tid, &input).await {
                            return Ok(json!({ "ok": false, "message": format!("更新失败: {e}") }));
                        }

                        // cron 变了 → 调度器 remove + 重新注册（沿用 enabled 状态）。
                        if cron_changed {
                            let _ = engine.remove_job(tid).await;
                            if target.enabled {
                                if let Err(e) = engine.register_job(&target).await {
                                    return Ok(json!({ "ok": false, "message": format!("重新注册调度失败: {e}") }));
                                }
                            }
                        }
                        // 「下次运行」按新 cron 计算（无论启停都同步，停用任务算的是
                        // 启用后的首跑时间——否则停用分支用旧 cron，文案自相矛盾）。
                        let next = crate::server::scheduled_task::runner_core::next_occurrence(&new_cron, &tz)
                            .map(|t| {
                                let tz2: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::Asia::Shanghai);
                                t.with_timezone(&tz2).format("%Y-%m-%d %H:%M").to_string()
                            })
                            .unwrap_or_else(|| "未知".to_string());
                        let human = handler::humanize_cron(&new_cron, &tz);
                        tool_audit(&state, &user_id, "update_scheduled_task", tid, &new_name);
                        Ok(json!({
                            "ok": true,
                            "task_id": tid,
                            "message": format!("任务「{}」已更新：{}（cron: {}）。下次运行：{}。", new_name, human, new_cron, next),
                        }))
                    }
                    "toggle" => {
                        let tid = args["task_id"].as_str().unwrap_or("");
                        let Some(target) = (match store.get(tid).await {
                            Ok(t) => t,
                            Err(e) => return Ok(json!({ "ok": false, "message": format!("查询任务失败: {e}") })),
                        }) else {
                            return Ok(json!({ "ok": false, "message": "任务不存在" }));
                        };
                        if target.user_id != user_id && !is_admin {
                            return Ok(json!({ "ok": false, "message": "无权操作该任务" }));
                        }
                        let Some(engine) = state.scheduler() else {
                            return Ok(json!({ "ok": false, "message": "调度器未初始化" }));
                        };
                        let new_enabled = args["enabled"].as_bool().unwrap_or(!target.enabled);
                        // 幂等守卫（对齐 REST update_task）：状态未变时不重复注册——
                        // 重复 register_job 会叠加新 job 行而旧 job 未删，每 cron 触发两次。
                        if new_enabled == target.enabled {
                            return Ok(json!({ "ok": true, "message": format!("任务「{}」已处于{}状态，无需重复操作。", target.name, if new_enabled { "启用" } else { "停用" }) }));
                        }
                        if let Err(e) = store.set_enabled(tid, new_enabled).await {
                            return Ok(json!({ "ok": false, "message": format!("更新失败: {e}") }));
                        }
                        let mut t2 = target.clone();
                        t2.enabled = new_enabled;
                        let _ = engine.set_enabled(&t2).await;
                        tool_audit(&state, &user_id, "toggle_scheduled_task", tid, &target.name);
                        let status_text = if new_enabled { "启用" } else { "停用" };
                        Ok(json!({ "ok": true, "message": format!("任务「{}」已{}", target.name, status_text) }))
                    }
                    "delete" => {
                        let tid = args["task_id"].as_str().unwrap_or("");
                        let Some(target) = (match store.get(tid).await {
                            Ok(t) => t,
                            Err(e) => return Ok(json!({ "ok": false, "message": format!("查询任务失败: {e}") })),
                        }) else {
                            return Ok(json!({ "ok": false, "message": "任务不存在" }));
                        };
                        if target.user_id != user_id && !is_admin {
                            return Ok(json!({ "ok": false, "message": "无权操作该任务" }));
                        }
                        if let Some(engine) = state.scheduler() {
                            let _ = engine.remove_job(tid).await;
                        }
                        match store.delete(tid).await {
                            Ok(true) => {
                                tool_audit(&state, &user_id, "delete_scheduled_task", tid, &target.name);
                                Ok(json!({ "ok": true, "message": format!("任务「{}」已删除。", target.name) }))
                            }
                            _ => Ok(json!({ "ok": false, "message": "删除失败" })),
                        }
                    }
                    "run_now" => {
                        let tid = args["task_id"].as_str().unwrap_or("");
                        let Some(target) = (match store.get(tid).await {
                            Ok(t) => t,
                            Err(e) => return Ok(json!({ "ok": false, "message": format!("查询任务失败: {e}") })),
                        }) else {
                            return Ok(json!({ "ok": false, "message": "任务不存在" }));
                        };
                        if target.user_id != user_id && !is_admin {
                            return Ok(json!({ "ok": false, "message": "无权操作该任务" }));
                        }
                        if !target.enabled {
                            return Ok(json!({ "ok": false, "message": "任务已停用，请先启用" }));
                        }
                        let state2 = state.clone();
                        let tid2 = tid.to_string();
                        tokio::spawn(async move {
                            runner_core::run_scheduled_task(state2, &tid2, "manual").await;
                        });
                        tool_audit(&state, &user_id, "run_scheduled_task_now", tid, &target.name);
                        Ok(json!({ "ok": true, "message": format!("已触发任务「{}」立即执行一次，稍后在定时任务详情页查看结果。", target.name) }))
                    }
                    _ => Ok(json!({ "ok": false, "message": format!("未知动作 action={}（支持 create/update/list/toggle/delete/run_now）", action) })),
                }
            }
        },
    )
    .with_parameters_schema::<ManageScheduledTaskParams>()
}

/// 取任务的下次运行时间展示文本。
fn engine_next(task: &crate::domain::scheduled_task::ScheduledTask) -> String {
    crate::server::scheduled_task::runner_core::next_occurrence(&task.schedule_cron, &task.timezone)
        .map(|t| {
            let tz: chrono_tz::Tz = task.timezone.parse().unwrap_or(chrono_tz::Asia::Shanghai);
            t.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_else(|| "未知".to_string())
}

/// 工具层写操作审计：与 REST handler 的 `scheduled_task` source 区分（`scheduled_task_tool`），
/// 「agent 在会话内创建/改/删了定时任务」这条安全事实必须可查（对齐 skill 删除审查纪律）。
/// user_id 直接落库（工具层无登录用户名，只有 agent 会话的归属人）。
fn tool_audit(state: &Arc<AppState>, user_id: &str, op: &str, target_id: &str, detail: &str) {
    crate::domain::audit::spawn_record(
        state.audit_store.as_ref(),
        crate::domain::audit::AuditEntry {
            user_id: user_id.to_string(),
            actor: user_id.to_string(),
            source: "scheduled_task_tool".to_string(),
            operation: op.to_string(),
            target_id: target_id.to_string(),
            success: true,
            detail: detail.to_string(),
            ip: String::new(),
        },
    );
}
