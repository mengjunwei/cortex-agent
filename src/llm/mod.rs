//! LLM 层 — 自定义模型客户端实现与配置
//!
//! 绕过 ADK 框架限制（如 ToolCallBuffer 截断问题），直接对接 OpenAI Compatible API。

pub mod anthropic_custom;
mod factory;

pub use factory::*;
pub mod openai;

use std::sync::atomic::{AtomicU64, Ordering};

/// 全局单调合成工具调用 id 计数器。
///
/// 弱供应商/文本标签来源的 FunctionCall 可能全程不发 id（id=None 或空串）。这些 FC 需要补一个
/// 占位 id，否则后续回填的 FunctionResponse 拿到空 id，`normalize_function_pairs` 会把它当孤立
/// FR 误删 → 触发严格模式（Anthropic/OpenAI）400。
///
/// 关键不变量：**跨轮、跨 run、跨协议全局唯一**。若用 `call_{idx}` / `call_{iteration}_{n}` 这类
/// 局部序号，同一 id 会在不同迭代/不同运行里重复——压缩/历史拼接后 normalize 会把同 id 的旧
/// FC 与新 FC 混淆（删除/错配）。用全局单调原子计数器保证每个合成 id 只出现一次。
static SYNTHETIC_CALL_ID: AtomicU64 = AtomicU64::new(0);

/// 生成全局唯一合成工具调用 id（`call_s{n}`，n 单调递增）。
///
/// 仅在 FC 真实 id 缺失（None/空）时调用；真实 id 优先保留。`call_s` 前缀与供应商真实 id
/// （`call_xxx`、`toolu_xxx`）区分，便于日志辨认是合成占位。
pub fn next_synthetic_call_id() -> String {
    format!(
        "call_s{}",
        SYNTHETIC_CALL_ID.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_anthropic_endpoint() {
        // GLM 的 Anthropic 端点应被拦截
        let r = validate_openai_compatible_base_url("https://open.bigmodel.cn/api/anthropic");
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("Anthropic"));
        assert!(msg.contains("paas/v4"), "应给出正确端点提示: {msg}");
    }

    #[test]
    fn rejects_anthropic_with_trailing_slash() {
        let r = validate_openai_compatible_base_url("https://open.bigmodel.cn/api/anthropic/");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_messages_endpoint() {
        let r = validate_openai_compatible_base_url("https://api.anthropic.com/v1/messages");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_empty_base_url() {
        assert!(validate_openai_compatible_base_url("").is_err());
        assert!(validate_openai_compatible_base_url("   ").is_err());
    }

    #[test]
    fn accepts_openai_compatible_endpoints() {
        // OpenAI 官方
        assert!(validate_openai_compatible_base_url("https://api.openai.com/v1").is_ok());
        // DeepSeek
        assert!(validate_openai_compatible_base_url("https://api.deepseek.com/v1").is_ok());
        // GLM OpenAI 兼容
        assert!(
            validate_openai_compatible_base_url("https://open.bigmodel.cn/api/paas/v4").is_ok()
        );
        // Ollama 本地
        assert!(validate_openai_compatible_base_url("http://localhost:11434/v1").is_ok());
        // 通义千问
        assert!(
            validate_openai_compatible_base_url(
                "https://dashscope.aliyuncs.com/compatible-mode/v1"
            )
            .is_ok()
        );
    }

    #[test]
    fn case_insensitive_check() {
        // 大写变体也要拦截
        let r = validate_openai_compatible_base_url("https://open.bigmodel.cn/API/Anthropic");
        assert!(r.is_err());
    }

    #[test]
    fn make_gen_config_defaults_anti_repetition_penalty() {
        // 默认生成配置应携带保守的重复惩罚，从源头压低 LLM degeneration 概率
        let cfg = make_gen_config(1024);
        assert_eq!(
            cfg.frequency_penalty,
            Some(0.4),
            "默认应设 frequency_penalty=0.4"
        );
        assert_eq!(
            cfg.presence_penalty,
            Some(0.3),
            "默认应设 presence_penalty=0.3"
        );
    }

    #[test]
    fn make_gen_config_from_defaults_anti_repetition_penalty() {
        let cfg = make_gen_config_from(None, None, None, None);
        assert_eq!(cfg.frequency_penalty, Some(0.4));
        assert_eq!(cfg.presence_penalty, Some(0.3));
    }
}
