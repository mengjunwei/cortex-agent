//! 工具执行 —— 单工具超时 + panic 防护，以及工具调用上下文 `ToolCtx` 的实现。
//!
//! 单个工具的失败（未找到 / 超时 / panic / 返回 Err）一律转成 `{"error": ...}` JSON 回填模型，
//! 不让其终止整个 agent（对齐 codex「工具失败一律回填模型让其重试」）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use adk_rust::async_trait;
use adk_rust::serde_json::{Value, json};
use adk_rust::tokio::sync::Mutex;
use adk_rust::{Content, EventActions, InvocationContext, MemoryEntry, Result, Tool, ToolContext};
use tokio_util::sync::CancellationToken;

/// 执行单个工具：超时 + panic 防护（catch_unwind）。
///
/// 工具未找到、超时、panic、返回 Err 一律转成 `{"error": ...}` JSON 回填模型，不让单个
/// 工具的失败或崩溃终止整个 agent（对齐 codex「工具失败一律回填模型让其重试」）。
pub(super) async fn execute_one_tool_safe(
    tool_map: &HashMap<String, Arc<dyn Tool>>,
    parent_ctx: &Arc<dyn InvocationContext>,
    name: &str,
    args: &Value,
    id: &Option<String>,
    tool_timeout: Duration,
    cancel_token: &CancellationToken,
) -> Value {
    let Some(tool) = tool_map.get(name) else {
        return json!({ "error": format!("Tool '{name}' not found") });
    };
    let tc: Arc<dyn ToolContext> = Arc::new(ToolCtx::new(
        parent_ctx.clone(),
        id.clone().unwrap_or_default(),
    ));
    use futures::FutureExt;
    use std::panic::AssertUnwindSafe;
    // select! 同时 await「工具执行（带超时+panic 防护）」与「用户取消」。
    // cancel 时 tool.execute future drop → 取消（HTTP 请求 / MCP 调用 / shell 随之 abort）。
    // 对齐 codex parallel.rs:160 的 tool dispatch select!。
    let exec = tokio::time::timeout(
        tool_timeout,
        AssertUnwindSafe(tool.execute(tc, args.clone())).catch_unwind(),
    );
    tokio::select! {
        res = exec => match res {
            Ok(Ok(Ok(v))) => v,
            Ok(Ok(Err(e))) => json!({ "error": e.to_string() }),
            Ok(Err(panic_payload)) => {
                let msg = panic_payload
                    .downcast_ref::<&'static str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                tracing::error!("[cortex_agent] 工具 {name} 执行 panic: {msg}");
                json!({ "error": format!("Tool '{name}' panicked: {msg}") })
            }
            Err(_) => json!({ "error": "Tool timed out" }),
        },
        _ = cancel_token.cancelled() => {
            tracing::info!("[cortex_agent] 工具 {name} 被用户取消");
            json!({ "error": "cancelled by user" })
        }
    }
}

/// 工具调用上下文：包装父 `InvocationContext`，补 `function_call_id` 与独立 `EventActions`。
pub(super) struct ToolCtx {
    parent: Arc<dyn InvocationContext>,
    fc_id: String,
    actions: Mutex<EventActions>,
}

impl ToolCtx {
    fn new(parent: Arc<dyn InvocationContext>, fc_id: String) -> Self {
        Self {
            parent,
            fc_id,
            actions: Mutex::new(EventActions::default()),
        }
    }
}

#[async_trait]
impl adk_rust::ReadonlyContext for ToolCtx {
    fn invocation_id(&self) -> &str {
        self.parent.invocation_id()
    }
    fn agent_name(&self) -> &str {
        self.parent.agent_name()
    }
    fn user_id(&self) -> &str {
        self.parent.user_id()
    }
    fn app_name(&self) -> &str {
        self.parent.app_name()
    }
    fn session_id(&self) -> &str {
        self.parent.session_id()
    }
    fn branch(&self) -> &str {
        self.parent.branch()
    }
    fn user_content(&self) -> &Content {
        self.parent.user_content()
    }
}

#[async_trait]
impl adk_rust::CallbackContext for ToolCtx {
    fn artifacts(&self) -> Option<Arc<dyn adk_rust::Artifacts>> {
        self.parent.artifacts()
    }
    fn shared_state(&self) -> Option<Arc<adk_rust::SharedState>> {
        self.parent.shared_state()
    }
}

#[async_trait]
impl ToolContext for ToolCtx {
    fn function_call_id(&self) -> &str {
        &self.fc_id
    }
    fn actions(&self) -> EventActions {
        self.actions.blocking_lock().clone()
    }
    fn set_actions(&self, _a: EventActions) {}
    async fn search_memory(&self, q: &str) -> Result<Vec<MemoryEntry>> {
        if let Some(m) = self.parent.memory() {
            m.search(q).await
        } else {
            Ok(vec![])
        }
    }
    fn user_scopes(&self) -> Vec<String> {
        self.parent.user_scopes()
    }
}
