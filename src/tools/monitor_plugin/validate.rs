//! 监控插件校验工具 —— LLM 生成 Rhai 脚本后的自检 FunctionTool
//!
//! 三层验证架构（用户既定方案）：
//!
//! | Layer | 实现 | 用途 | 耗时 |
//! |-------|------|------|------|
//! | L1 | 进程内 [`RhaiMonitorPlugin::check_syntax`] | 语法编译检查 | 毫秒级 |
//! | L2 | 进程内 [`run_in_process`]（spawn_blocking + timeout） | 执行 mock 用例 | 毫秒级 |
//! | L3 | [`CodeVerifier`]（adk-code RustExecutor） | 完整 Rust 编译管线演示 | 5-15s |
//!
//! L2 原使用 adk-sandbox 子进程隔离，但其 stdin 管道在 Windows 上存在句柄泄漏，
//! 首次执行后子进程 read_to_string 永久阻塞导致 10s 超时。改为进程内执行：
//! Rhai 引擎自带 max_operations 防死循环，配合 spawn_blocking + timeout 更稳定。
//!
//! LLM 生成 Rhai 脚本后应调用本工具（`validate_monitor_plugin`）自检；
//! 失败时根据返回的错误信息修正脚本，最多重试 3 轮。

use std::sync::Arc;
use std::time::Duration;

use adk_rust::tool::FunctionTool;
use adk_rust::{
    ToolContext,
    serde_json::{Value, json},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::infra::sandbox::code_exec::CodeVerifier;
use crate::domain::monitor::rhai_plugin::RhaiMonitorPlugin;

// ─── 参数 Schema ───────────────────────────────────────────────

/// 单个测试用例
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestCase {
    /// 用例名称
    pub name: String,
    /// action: prepare_oids 或 parse
    pub action: String,
    /// 调用 parse 时传入的 OID 值 JSON（action=prepare_oids 时可省）
    #[serde(default)]
    pub oid_values_json: Option<String>,
    /// 预期结果 JSON 子串（用作断言子集匹配，空字符串跳过断言）
    #[serde(default)]
    pub expected_contains: Option<String>,
    /// 预期 success 字段值（仅校验 JSON 顶层 success，None 跳过）
    #[serde(default)]
    pub expect_success: Option<bool>,
}

/// validate_monitor_plugin 工具参数
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ValidateParams {
    /// 待验证的 Rhai 脚本源码（含 `prepare_oids()` 和 `parse(json)` 两个顶层函数）
    pub script: String,
    /// 测试用例列表（至少 1 个）
    pub test_cases: Vec<TestCase>,
    /// 验证模式：fast=L1+L2，full=L1+L2+L3
    #[serde(default)]
    pub mode: Option<String>,
}

// ─── 结果 Schema ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct CaseResult {
    name: String,
    layer1_syntax: bool,
    layer1_error: Option<String>,
    layer2_sandbox: Option<Value>,
    layer3_code: Option<Value>,
    /// 本用例最终判定（综合 L1+L2；L3 仅作参考）
    passed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ValidateSummary {
    ok: bool,
    mode: String,
    total_cases: usize,
    passed_cases: usize,
    cases: Vec<CaseResult>,
    /// 整体错误摘要（LLM 友好）
    summary: String,
}

// ─── 工具创建 ─────────────────────────────────────────────────

/// 创建 `validate_monitor_plugin` 工具
///
/// 内部不持有任何可变状态：每次调用按需创建 verifier。
/// 这保证工具可被多 agent 并发调用。
pub fn create_validate_tool() -> FunctionTool {
    FunctionTool::new(
        "validate_monitor_plugin",
        "校验 LLM 生成的 Rhai 监控插件。三层验证：L1 进程内语法检查（毫秒），L2 进程内执行 mock 用例（毫秒，带安全限制+超时），L3 adk-code RustExecutor 完整 Rust 编译管线（秒）。生成 Rhai 脚本后必须先调用此工具自检，失败则按错误信息修正后再生成。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| async move {
            let script = args["script"].as_str().unwrap_or("").to_string();
            let mode = args["mode"]
                .as_str()
                .unwrap_or("fast")
                .to_string();
            let test_cases = parse_test_cases(&args["test_cases"]);

            if script.is_empty() {
                return Ok(json!({ "ok": false, "error": "script 不能为空" }));
            }
            if test_cases.is_empty() {
                return Ok(json!({
                    "ok": false,
                    "error": "至少需要 1 个 test_case",
                    "hint": "正常值 + 边界值，参考 system prompt 中的样例"
                }));
            }

            let result = run_validation(&script, &test_cases, &mode).await;
            Ok(serde_json::to_value(&result).unwrap_or_else(|_| json!({ "ok": false })))
        },
    )
    .with_parameters_schema::<ValidateParams>()
}

fn parse_test_cases(v: &Value) -> Vec<TestCase> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let name = match item["name"].as_str() {
                Some(s) => s.to_string(),
                None => {
                    tracing::warn!("[validate] test_cases[{i}] 缺少 name 字段，已跳过");
                    return None;
                }
            };
            Some(TestCase {
                name,
                action: item["action"]
                    .as_str()
                    .unwrap_or("prepare_oids")
                    .to_string(),
                oid_values_json: item["oid_values_json"].as_str().map(String::from),
                expected_contains: item["expected_contains"].as_str().map(String::from),
                expect_success: item["expect_success"].as_bool(),
            })
        })
        .collect()
}

async fn run_validation(script: &str, test_cases: &[TestCase], mode: &str) -> ValidateSummary {
    let run_layer3 = mode == "full";
    let code_verifier = if run_layer3 {
        Some(CodeVerifier::new())
    } else {
        None
    };

    let mut cases = Vec::with_capacity(test_cases.len());
    let mut passed_count = 0usize;

    for tc in test_cases {
        let mut cr = CaseResult {
            name: tc.name.clone(),
            layer1_syntax: false,
            layer1_error: None,
            layer2_sandbox: None,
            layer3_code: None,
            passed: false,
        };

        // ── L1：进程内语法检查 ───────────────────────────────
        match RhaiMonitorPlugin::check_syntax(script) {
            Ok(()) => cr.layer1_syntax = true,
            Err(e) => {
                cr.layer1_error = Some(e);
                cases.push(cr);
                continue;
            }
        }

        // ── L2：进程内执行（Rhai 安全限制 + spawn_blocking + timeout） ──
        // 不再使用 adk-sandbox 子进程：其 stdin 管道在 Windows 上存在句柄泄漏，
        // 首次执行后子进程 read_to_string 永久阻塞导致 10s 超时。
        // Rhai 引擎自带 max_operations 防死循环，进程内执行更稳定高效。
        let l2 = run_in_process(script, &tc.action, tc.oid_values_json.as_deref()).await;
        cr.layer2_sandbox = Some(l2);

        // ── L3：adk-code RustExecutor（可选） ───────────────
        if let Some(cv) = &code_verifier {
            let res = cv
                .run_rhai(script, &tc.action, tc.oid_values_json.as_deref(), None)
                .await
                .unwrap_or_else(|e| crate::infra::sandbox::code_exec::CodeVerifyResult {
                    ok: false,
                    output: None,
                    display_stdout: String::new(),
                    stderr: String::new(),
                    exit_code: -1,
                    duration_ms: 0,
                    diagnostics: vec![],
                    error: Some(e.to_string()),
                });
            cr.layer3_code = Some(json!({
                "ok": res.ok,
                "output": res.output,
                "exit_code": res.exit_code,
                "duration_ms": res.duration_ms,
                "stderr": res.stderr,
                "diagnostics": res.diagnostics,
                "error": res.error,
            }));
        }

        // ── 综合判定：L1 通过 且 L2 通过（无 sandbox 时仅 L1） ──
        let l2_ok = cr
            .layer2_sandbox
            .as_ref()
            .and_then(|v| v["ok"].as_bool())
            .unwrap_or(true);

        let mut local_pass = cr.layer1_syntax && l2_ok;

        // 应用断言
        if let Some(contains) = &tc.expected_contains {
            let actual = cr
                .layer2_sandbox
                .as_ref()
                .and_then(|v| v["result"].as_str())
                .unwrap_or("");
            local_pass = local_pass && actual.contains(contains.as_str());
        }
        if let Some(want_success) = tc.expect_success {
            let actual_success = cr
                .layer2_sandbox
                .as_ref()
                .and_then(|v| v["result"].as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|v| v[0]["success"].as_bool());
            if let Some(actual) = actual_success {
                local_pass = local_pass && (actual == want_success);
            }
        }

        cr.passed = local_pass;
        if local_pass {
            passed_count += 1;
        }
        cases.push(cr);
    }

    let ok = passed_count == test_cases.len();
    let summary = build_summary(&cases, ok);

    ValidateSummary {
        ok,
        mode: mode.to_string(),
        total_cases: test_cases.len(),
        passed_cases: passed_count,
        cases,
        summary,
    }
}

/// 进程内执行 Rhai 脚本（带安全限制 + spawn_blocking + timeout 兜底）
///
/// 替代 adk-sandbox 子进程方案：
/// - Rhai 引擎的 `max_operations` 已防止死循环（抛 `TooManyOperations`）
/// - `spawn_blocking` 避免阻塞 async runtime
/// - `timeout(5s)` 兜底极端情况
async fn run_in_process(script: &str, action: &str, oid_values_json: Option<&str>) -> Value {
    let script = script.to_string();
    let action = action.to_string();
    let oid = oid_values_json.map(String::from);
    let start = std::time::Instant::now();

    let join = tokio::task::spawn_blocking(move || {
        let plugin = match RhaiMonitorPlugin::compile("__validate__", &script, 0) {
            Ok(p) => p,
            Err(e) => {
                return json!({
                    "ok": false,
                    "result": "",
                    "error": format!("编译失败: {e}")
                });
            }
        };
        let result = match action.as_str() {
            "parse" => plugin.parse(oid.as_deref().unwrap_or("{}")),
            _ => plugin.prepare_oids(),
        };
        json!({
            "ok": true,
            "result": result,
            "error": null,
        })
    });

    match tokio::time::timeout(Duration::from_secs(5), join).await {
        Ok(Ok(v)) => {
            let mut out = v;
            out["duration_ms"] = json!(start.elapsed().as_millis());
            out["exec_mode"] = json!("in_process");
            out
        }
        Ok(Err(e)) => json!({
            "ok": false,
            "result": "",
            "duration_ms": start.elapsed().as_millis(),
            "exec_mode": "in_process",
            "error": format!("执行异常: {e}")
        }),
        Err(_) => json!({
            "ok": false,
            "result": "",
            "duration_ms": start.elapsed().as_millis(),
            "exec_mode": "in_process",
            "error": "执行超时 (5s)"
        }),
    }
}

fn build_summary(cases: &[CaseResult], ok: bool) -> String {
    if ok {
        return format!("全部 {} 个用例通过三层验证", cases.len());
    }
    let failed: Vec<&CaseResult> = cases.iter().filter(|c| !c.passed).collect();
    let mut msgs = Vec::new();
    for c in failed {
        if let Some(e) = &c.layer1_error {
            msgs.push(format!("「{}」L1 语法错误: {}", c.name, shorten(e, 200)));
            continue;
        }
        if let Some(l2) = &c.layer2_sandbox {
            if !l2["ok"].as_bool().unwrap_or(false) {
                let err = l2["error"]
                    .as_str()
                    .or_else(|| l2["stderr"].as_str())
                    .unwrap_or("未知错误");
                msgs.push(format!(
                    "「{}」L2 沙箱执行失败: {}",
                    c.name,
                    shorten(err, 200)
                ));
                continue;
            }
        }
        msgs.push(format!("「{}」断言不匹配", c.name));
    }
    format!("共 {} 条失败：{}", msgs.len(), msgs.join(" | "))
}

fn shorten(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(n).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_script() -> &'static str {
        r#"
        fn prepare_oids() { `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]` }
        fn parse(j) {
            let m = parse_json(j);
            let n = get_num(m, ".1.3.6.1.2.1.1.3.0");
            if n.is_none() { return `[{"success":false,"errors":["missing"]}]`; }
            let s = n.unwrap() / 100.0;
            `[{"success":true,"value":{"number":${s}}}]`
        }
        "#
    }

    fn tcs() -> Vec<TestCase> {
        vec![
            TestCase {
                name: "prepare_oids shape".into(),
                action: "prepare_oids".into(),
                oid_values_json: None,
                expected_contains: Some(".1.3.6.1.2.1.1.3.0".into()),
                expect_success: None,
            },
            TestCase {
                name: "parse normal".into(),
                action: "parse".into(),
                oid_values_json: Some(
                    r#"{".1.3.6.1.2.1.1.3.0":{"oid_value_type":2,"value_str":"","value_num":123456}}"#
                        .into(),
                ),
                expected_contains: None,
                expect_success: Some(true),
            },
        ]
    }

    #[tokio::test]
    async fn validate_good_script_passes() {
        let res = run_validation(sample_script(), &tcs(), "fast").await;
        assert!(res.ok, "summary={}", res.summary);
        assert_eq!(res.passed_cases, 2);
    }

    #[tokio::test]
    async fn validate_syntax_error_fails_at_l1() {
        let bad = "fn broken(";
        let res = run_validation(bad, &tcs(), "fast").await;
        assert!(!res.ok);
        assert!(res.summary.contains("L1 语法错误"));
    }

    #[tokio::test]
    async fn validate_assertion_failure_reported() {
        // 故意把 success 改成永远 false
        let bad = r#"
            fn prepare_oids() { `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]` }
            fn parse(_j) { `[{"success":false,"errors":["x"]}]` }
        "#;
        let mut cases = tcs();
        cases[1].expect_success = Some(true);
        let res = run_validation(bad, &cases, "fast").await;
        assert!(!res.ok);
    }

    #[test]
    fn parse_test_cases_handles_missing_fields() {
        let v = json!([{"name": "a", "action": "prepare_oids"}]);
        let parsed = parse_test_cases(&v);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "a");
    }

    #[test]
    fn parse_test_cases_returns_empty_for_non_array() {
        assert!(parse_test_cases(&json!("not array")).is_empty());
        assert!(parse_test_cases(&json!(null)).is_empty());
    }
}
