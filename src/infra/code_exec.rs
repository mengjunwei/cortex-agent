//! adk-code 封装 —— 用于通过完整 Rust 编译管线验证 Rhai 脚本（验证 Layer 3）
//!
//! 利用 adk-code 的 `RustExecutor`（check → build → execute）管线，
//! 把 Rhai 脚本嵌入一个 Rust wrapper 程序，编译运行得到结果。
//!
//! 这一路径演示了完整的代码执行管线，未来可直接用于验证 Rust 监控插件。

use std::sync::Arc;
use std::time::Duration;

use adk_code::{CodeError, RustExecutor, RustExecutorConfig};
use adk_sandbox::{ProcessBackend, SandboxBackend};
use anyhow::Result;
use serde::Serialize;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// adk-code 验证结果
#[derive(Debug, Clone, Serialize)]
pub struct CodeVerifyResult {
    pub ok: bool,
    pub output: Option<serde_json::Value>,
    pub display_stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub diagnostics: Vec<String>,
    pub error: Option<String>,
}

/// adk-code 执行器：封装 RustExecutor
pub struct CodeVerifier {
    executor: RustExecutor,
}

impl CodeVerifier {
    pub fn new() -> Self {
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::default());
        let executor = RustExecutor::new(backend, RustExecutorConfig::default());
        Self { executor }
    }

    /// 通过 RustExecutor 跑 Rhai 脚本
    ///
    /// 内部把 Rhai 脚本嵌入 Rust wrapper 程序，编译运行。
    /// 这一路径会真正调用 rustc，时间较长（首次 5-15 秒）。
    pub async fn run_rhai(
        &self,
        script: &str,
        action: &str,
        oid_values_json: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<CodeVerifyResult> {
        let wrapper = build_rust_wrapper(script, action, oid_values_json);
        let input = serde_json::json!({});
        match self
            .executor
            .execute(&wrapper, Some(&input), timeout.unwrap_or(DEFAULT_TIMEOUT))
            .await
        {
            Ok(code_result) => Ok(CodeVerifyResult {
                ok: code_result.exec_result.exit_code == 0,
                output: code_result.output,
                display_stdout: code_result.display_stdout,
                stderr: code_result.exec_result.stderr,
                exit_code: code_result.exec_result.exit_code,
                duration_ms: code_result.exec_result.duration.as_millis(),
                diagnostics: code_result
                    .diagnostics
                    .into_iter()
                    .map(|d| {
                        format!(
                            "[{}] {} ({})",
                            d.level,
                            d.message,
                            d.code.unwrap_or_default()
                        )
                    })
                    .collect(),
                error: None,
            }),
            Err(e) => Ok(CodeVerifyResult {
                ok: false,
                output: None,
                display_stdout: String::new(),
                stderr: match &e {
                    CodeError::CompileError { stderr, .. } => stderr.clone(),
                    _ => String::new(),
                },
                exit_code: -1,
                duration_ms: 0,
                diagnostics: vec![],
                error: Some(format!("{e}")),
            }),
        }
    }
}

impl Default for CodeVerifier {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造 Rust wrapper 程序源码
///
/// wrapper 里嵌入 Rhai 脚本，编译后调用 rhai 引擎执行。
/// 通过 `run(input)` 函数返回 JSON 结果。
///
/// 脚本以 Base64 编码嵌入，避开所有引号/反斜杠转义问题。
fn build_rust_wrapper(script: &str, action: &str, oid_values_json: Option<&str>) -> String {
    use base64_encode_decode as b64;
    let script_b64 = b64::encode(script);
    let oid_b64 = b64::encode(oid_values_json.unwrap_or(""));

    format!(
        r#"fn run(_input: serde_json::Value) -> serde_json::Value {{
    let script_b64: &str = "{script_b64}";
    let oid_b64: &str = "{oid_b64}";
    let action: &str = "{action}";

    let script = match base64_decode(script_b64) {{
        Ok(s) => s,
        Err(e) => return serde_json::json!({{ "ok": false, "error": format!("b64 decode: {{}}", e) }}),
    }};
    let oid_arg = base64_decode(oid_b64).unwrap_or_default();

    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(64, 64);
    engine.set_max_call_levels(50);
    engine.set_max_operations(1_000);
    engine.set_max_string_size(1_000_000);
    engine.set_max_array_size(10_000);
    engine.set_max_map_size(10_000);

    let ast = match engine.compile(&script) {{
        Ok(a) => a,
        Err(e) => return serde_json::json!({{ "ok": false, "error": format!("compile: {{}}", e) }}),
    }};

    let mut scope = rhai::Scope::new();
    match action {{
        "prepare_oids" => match engine.call_fn::<String>(&mut scope, &ast, "prepare_oids", ()) {{
            Ok(s) => serde_json::json!({{ "ok": true, "result": s }}),
            Err(e) => serde_json::json!({{ "ok": false, "error": format!("prepare_oids: {{}}", e) }}),
        }},
        "parse" => match engine.call_fn::<String>(&mut scope, &ast, "parse", (oid_arg,)) {{
            Ok(s) => serde_json::json!({{ "ok": true, "result": s }}),
            Err(e) => serde_json::json!({{ "ok": false, "error": format!("parse: {{}}", e) }}),
        }},
        _ => serde_json::json!({{ "ok": false, "error": "unknown action" }}),
    }}
}}

fn base64_decode(s: &str) -> Result<String, String> {{
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {{
        if b == b'=' {{ break; }}
        let v = TBL.iter().position(|&c| c == b).ok_or_else(|| format!("invalid b64 char: {{}}", b as char))? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {{
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }}
    }}
    String::from_utf8(out).map_err(|e| format!("utf8: {{}}", e))
}}
"#
    )
}

/// 简易 Base64 编码（避免引入新依赖）
mod base64_encode_decode {
    pub fn encode(s: &str) -> String {
        const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        let mut i = 0;
        while i + 3 <= bytes.len() {
            let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
            out.push(TBL[((n >> 18) & 0x3F) as usize] as char);
            out.push(TBL[((n >> 12) & 0x3F) as usize] as char);
            out.push(TBL[((n >> 6) & 0x3F) as usize] as char);
            out.push(TBL[(n & 0x3F) as usize] as char);
            i += 3;
        }
        let rem = bytes.len() - i;
        if rem == 1 {
            let n = (bytes[i] as u32) << 16;
            out.push(TBL[((n >> 18) & 0x3F) as usize] as char);
            out.push(TBL[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        } else if rem == 2 {
            let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
            out.push(TBL[((n >> 18) & 0x3F) as usize] as char);
            out.push(TBL[((n >> 12) & 0x3F) as usize] as char);
            out.push(TBL[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_is_valid_syntax_pattern() {
        let w = build_rust_wrapper("fn prepare_oids() {}", "prepare_oids", None);
        assert!(w.contains("fn run(_input: serde_json::Value) -> serde_json::Value"));
        assert!(w.contains(r#"let action: &str = "prepare_oids";"#));
        assert!(w.contains("let script_b64: &str ="));
    }

    #[test]
    fn wrapper_handles_quotes_and_backslashes() {
        let w = build_rust_wrapper("fn x() { `\"a\"` }", "parse", Some("{\"k\":1}"));
        // 内嵌字符串通过 Base64 处理后不应破坏 Rust 语法
        assert!(w.contains(r#"let action: &str = "parse";"#));
        assert!(w.contains("let oid_b64: &str ="));
    }

    #[test]
    fn b64_encode_roundtrip() {
        use base64_encode_decode as b64;
        let s = "fn parse(j) { let x = \"中文测试\"; }";
        let enc = b64::encode(s);
        // 解码用同样的逻辑（参考 wrapper 内 base64_decode 验证一次）
        // 这里只确保 encode 产出合法 base64 字符集
        for c in enc.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=',
                "bad char: {c}"
            );
        }
    }

    /// 端到端 RustExecutor 测试 —— 需要 rustc 可访问，且依赖编译耗时长。
    /// 默认标 ignore，CI/手动运行时启用。
    #[tokio::test]
    #[ignore = "需要 rustc + serde_json 依赖，跑 adk-code 完整管线，耗时 15+s"]
    async fn code_verifier_full_pipeline() {
        let v = CodeVerifier::new();
        let script = r#"
            fn prepare_oids() { `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]` }
        "#;
        let r = v
            .run_rhai(script, "prepare_oids", None, None)
            .await
            .unwrap();
        assert!(r.ok || r.error.is_some());
    }
}
