//! `send_user_message_async` 工具 — 长任务中途向用户发一条可见消息。
//!
//! 对齐 codex `send_user_message_async`：模型在多步任务进行中向用户推送简短的
//! 进度更新 / 重要通知 / 阻塞提问，工具**立即返回** `{"accepted":true}`，不等待
//! 用户回应；用户的回复以后续 user message 异步到达（运行中经 steer 注入下一轮，
//! 或用户在 run 结束后正常输入）。
//!
//! 出口：SSE 层实现 [`AsyncUserMessageSink`] 把消息转成 `ASYNC_USER_MESSAGE` 事件
//! 推给前端（整条推送，区别于 turn 末尾的 TEXT_MESSAGE_* 流）。工具注册时闭包捕获
//! sink；子 agent 经 AgentBlueprint 克隆父工具集时 sink（Arc）随之继承——任何层级
//! 的 agent 都可直接向用户面说话（与 codex 语义一致）。

use std::sync::Arc;

use adk_rust::tool::FunctionTool;
use adk_rust::{
    ToolContext,
    serde_json::{Value, json},
};
use schemars::JsonSchema;
use serde::Serialize;

/// 异步用户消息出口。SSE 层实现转 `ASYNC_USER_MESSAGE` 事件；默认 Noop 丢弃。
pub trait AsyncUserMessageSink: Send + Sync {
    fn emit(&self, message: String);
}

/// 空 sink：非 SSE 场景 / 测试用，丢弃所有消息。
pub struct NoopAsyncUserMessageSink;
impl AsyncUserMessageSink for NoopAsyncUserMessageSink {
    fn emit(&self, _message: String) {}
}

#[derive(Debug, Serialize, JsonSchema)]
struct SendUserMessageAsyncParams {
    /// 要发给用户的消息：简短、自包含（进度更新 / 重要通知 / 阻塞提问）。
    message: String,
}

/// 构造 send_user_message_async 工具。
pub fn create_send_user_message_async_tool(sink: Arc<dyn AsyncUserMessageSink>) -> FunctionTool {
    FunctionTool::new(
        "send_user_message_async",
        "长任务运行中途向用户发送一条简短可见消息（进度更新、重要通知、或需要用户决策的\
         阻塞提问）。立即返回、不等待回复；用户的回复会以后续对话消息异步到达，无需轮询。\
         适用场景：预计还要较长时间时告知进度、汇报阶段性发现、或任务被卡住需要用户拍板。\
         不要用它替代最终回复——turn 结束时的正常输出才是最终答案。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let sink = sink.clone();
            async move {
                let message = match args["message"].as_str() {
                    Some(s) => s.trim(),
                    _ => {
                        return Ok(json!({
                            "accepted": false,
                            "error": "message must be a non-empty string"
                        }))
                    }
                };
                if message.is_empty() {
                    return Ok(json!({
                        "accepted": false,
                        "error": "message must be a non-empty string"
                    }));
                }
                let message = message.to_string();
                sink.emit(message);
                Ok(json!({ "accepted": true }))
            }
        },
    )
    .with_parameters_schema::<SendUserMessageAsyncParams>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CollectSink(Mutex<Vec<String>>);
    impl AsyncUserMessageSink for CollectSink {
        fn emit(&self, message: String) {
            self.0.lock().unwrap().push(message);
        }
    }

    fn call_tool(tool: &FunctionTool, args: Value) -> Value {
        use adk_rust::Tool;
        // SimpleToolContext：测试用空上下文（本工具不读 ctx）
        let ctx: Arc<dyn ToolContext> =
            Arc::new(adk_rust::tool::SimpleToolContext::new("test"));
        let fut = tool.execute(ctx, args);
        futures::executor::block_on(fut).unwrap_or_else(|e| {
            panic!("tool call failed: {e:?}");
        })
    }

    #[test]
    fn emits_message_and_accepts() {
        let sink = Arc::new(CollectSink::default());
        let tool = create_send_user_message_async_tool(sink.clone());
        let out = call_tool(
            &tool,
            json!({ "message": "  分析到一半，预计还需 2 分钟  " }),
        );
        assert_eq!(out, json!({ "accepted": true }));
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            ["分析到一半，预计还需 2 分钟"]
        );
    }

    #[test]
    fn rejects_empty_or_missing_message() {
        let sink = Arc::new(CollectSink::default());
        let tool = create_send_user_message_async_tool(sink.clone());
        let out = call_tool(&tool, json!({ "message": "   " }));
        assert_eq!(out["accepted"], json!(false));
        assert!(out["error"].as_str().unwrap().contains("non-empty"));
        let out = call_tool(&tool, json!({}));
        assert_eq!(out["accepted"], json!(false));
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn noop_sink_swallows() {
        let sink = Arc::new(NoopAsyncUserMessageSink);
        let tool = create_send_user_message_async_tool(sink);
        let out = call_tool(&tool, json!({ "message": "hi" }));
        assert_eq!(out, json!({ "accepted": true }));
    }
}
