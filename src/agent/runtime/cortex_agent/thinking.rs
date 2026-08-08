//! 思考参数（thinking / effort / reasoning_effort）兜底 —— 模型不支持时去参数重试。
//!
//! 双协议键：`anthropic.effort` 与 `openai.reasoning_effort`（见 `llm::make_gen_config_from`）。
//! 部分模型收到这类参数会以「参数错误」首事件返回，此处识别该错误并清除参数后重试一次，
//! 让任务继续而非中断。

use adk_rust::GenerateContentConfig;

/// 配置是否带思考参数（双协议键任一存在即视为带）
pub(super) fn config_has_thinking(config: &Option<GenerateContentConfig>) -> bool {
    config.as_ref().is_some_and(|c| {
        c.extensions
            .get("anthropic")
            .and_then(|v| v.get("effort"))
            .is_some()
            || c.extensions
                .get("openai")
                .and_then(|v| v.get("reasoning_effort"))
                .is_some()
    })
}

/// 清掉配置里的思考参数（兜底重试 + 让后续轮次也走模型默认）
pub(super) fn clear_thinking_from_config(config: &mut Option<GenerateContentConfig>) {
    if let Some(c) = config.as_mut() {
        if let Some(a) = c
            .extensions
            .get_mut("anthropic")
            .and_then(|v| v.as_object_mut())
        {
            a.remove("effort");
        }
        if let Some(o) = c
            .extensions
            .get_mut("openai")
            .and_then(|v| v.as_object_mut())
        {
            o.remove("reasoning_effort");
        }
    }
}

/// 错误是否像「思考参数不支持」——error_message 含相关关键词
pub(super) fn looks_like_thinking_param_error(chunk: &adk_rust::LlmResponse) -> bool {
    let Some(msg) = chunk.error_message.as_deref() else {
        return false;
    };
    let m = msg.to_lowercase();
    m.contains("reasoning_effort")
        || m.contains("output_config")
        || m.contains("effort")
        || m.contains("thinking")
}
