//! adk-sandbox 封装 —— 用于在隔离子进程中运行 Rhai 脚本（验证 Layer 2）
//!
//! 通过 spawn `rhai-runner` 二进制 + stdin/stdout JSON 协议，
//! 在独立子进程中执行 LLM 生成的 Rhai 脚本。子进程崩溃 / 死循环 / 爆内存
//! 都不会影响主进程。
//!
//! `rhai-runner` 二进制由本项目 `src/bin/rhai_runner.rs` 编译产生。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adk_sandbox::{ExecRequest, Language, ProcessBackend, SandboxBackend};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Rhai runner 调用动作
#[derive(Debug, Clone, Copy)]
pub enum RunnerAction {
    PrepareOids,
    Parse,
}

impl RunnerAction {
    fn as_str(&self) -> &'static str {
        match self {
            RunnerAction::PrepareOids => "prepare_oids",
            RunnerAction::Parse => "parse",
        }
    }
}

/// 传给 rhai-runner 的请求
#[derive(Debug, Serialize)]
struct RunnerRequest<'a> {
    script: &'a str,
    action: &'a str,
    oid_values_json: Option<&'a str>,
}

/// rhai-runner 返回的响应
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RunnerResponse {
    Ok { result: String },
    Err { error: String },
}

/// 沙箱验证结果
#[derive(Debug, Clone, Serialize)]
pub struct SandboxVerifyResult {
    pub ok: bool,
    pub result: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub error: Option<String>,
}

/// 沙箱执行器：封装 adk-sandbox ProcessBackend + rhai-runner
pub struct SandboxVerifier {
    backend: Arc<dyn SandboxBackend>,
    runner_path: PathBuf,
}

impl SandboxVerifier {
    /// 创建新的沙箱执行器
    ///
    /// `runner_path` 为 `rhai-runner` 可执行文件路径。
    /// 若传 None，则尝试从当前可执行文件同级目录查找。
    pub fn new(runner_path: Option<PathBuf>) -> Result<Self> {
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::default());
        let runner_path = match runner_path {
            Some(p) => p,
            None => locate_runner()?,
        };
        Ok(Self {
            backend,
            runner_path,
        })
    }

    /// 在隔离子进程中执行 Rhai 脚本
    pub async fn run(
        &self,
        script: &str,
        action: RunnerAction,
        oid_values_json: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<SandboxVerifyResult> {
        let req_payload = RunnerRequest {
            script,
            action: action.as_str(),
            oid_values_json,
        };
        let stdin = serde_json::to_string(&req_payload)?;

        let env = HashMap::new();
        let runner_path_str = self.runner_path.to_string_lossy().replace('\\', "/");
        // 注意：adk-sandbox 在 Windows 上使用 `cmd /C <code>`、在 Unix 上使用 `sh -c <code>`。
        // 这里不要在 code 中再加字面引号 —— std::process::Command 在传递参数给 cmd/sh 时
        // 会自己做正确的转义；额外加引号会导致 cmd /C 看到双层引号从而识别失败。
        let exec_req = ExecRequest {
            language: Language::Command,
            code: runner_path_str,
            stdin: Some(stdin),
            timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
            memory_limit_mb: None,
            env,
        };

        match self.backend.execute(exec_req).await {
            Ok(exec_result) => {
                let resp_parsed: Result<RunnerResponse, _> = serde_json::from_str(
                    exec_result
                        .stdout
                        .trim()
                        .split('\n')
                        .next_back()
                        .unwrap_or(""),
                );
                match resp_parsed {
                    Ok(RunnerResponse::Ok { result }) => Ok(SandboxVerifyResult {
                        ok: true,
                        result,
                        stdout: exec_result.stdout,
                        stderr: exec_result.stderr,
                        exit_code: exec_result.exit_code,
                        duration_ms: exec_result.duration.as_millis(),
                        error: None,
                    }),
                    Ok(RunnerResponse::Err { error }) => Ok(SandboxVerifyResult {
                        ok: false,
                        result: String::new(),
                        stdout: exec_result.stdout,
                        stderr: exec_result.stderr,
                        exit_code: exec_result.exit_code,
                        duration_ms: exec_result.duration.as_millis(),
                        error: Some(error),
                    }),
                    Err(e) => Ok(SandboxVerifyResult {
                        ok: false,
                        result: String::new(),
                        stdout: exec_result.stdout,
                        stderr: exec_result.stderr,
                        exit_code: exec_result.exit_code,
                        duration_ms: exec_result.duration.as_millis(),
                        error: Some(format!("解析 runner 响应失败: {e}")),
                    }),
                }
            }
            Err(e) => Ok(SandboxVerifyResult {
                ok: false,
                result: String::new(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                duration_ms: 0,
                error: Some(format!("sandbox 后端错误: {e}")),
            }),
        }
    }
}

/// 定位 rhai-runner 可执行文件
///
/// 查找顺序：
/// 1. CARGO_BIN_EXE_rhai_runner 环境变量（cargo test 自动设置）
/// 2. 当前可执行文件同目录下 `rhai-runner(.exe)` —— 生产部署：与主程序并列
/// 3. 当前 exe 上溯两级（target/&lt;profile&gt;/）—— cargo test 场景：deps/..
/// 4. 项目 target 目录兜底 —— CARGO_MANIFEST_DIR/target/&lt;profile&gt;
fn locate_runner() -> Result<PathBuf> {
    // 1. cargo test 自动注入的环境变量
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_rhai_runner") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Ok(path);
        }
    }

    // 2. 当前 exe 同目录（生产部署）
    if let Ok(curr_exe) = std::env::current_exe() {
        if let Some(parent) = curr_exe.parent() {
            let candidate = with_exe_extension(parent.join("rhai-runner"));
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        // 3. 上溯两级：deps -> target/<profile>
        if let Some(profile_dir) = curr_exe.parent().and_then(|p| p.parent()) {
            let candidate = with_exe_extension(profile_dir.join("rhai-runner"));
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    // 4. 项目目录兜底（编译期常量，开发环境最稳）
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    for profile in ["debug", "release"] {
        let candidate = with_exe_extension(
            PathBuf::from(manifest_dir)
                .join("target")
                .join(profile)
                .join("rhai-runner"),
        );
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow::anyhow!(
        "未找到 rhai-runner 可执行文件。请先 `cargo build --bin rhai-runner`。"
    ))
    .with_context(|| "locate rhai-runner failed")
}

#[cfg(windows)]
fn with_exe_extension(base: PathBuf) -> PathBuf {
    base.with_extension("exe")
}

#[cfg(not(windows))]
fn with_exe_extension(base: PathBuf) -> PathBuf {
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        fn prepare_oids() { `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]` }
        fn parse(j) {
            let m = parse_json(j);
            let n = get_num(m, ".1.3.6.1.2.1.1.3.0");
            if n.is_none() { return `[{"success":false,"errors":["x"]}]`; }
            `[{"success":true,"value":{"number":${n.unwrap()}}}]`
        }
    "#;

    /// 此测试依赖 rhai-runner 二进制存在。
    /// 运行前请执行 `cargo build --bin rhai-runner`。
    #[tokio::test]
    async fn sandbox_prepare_oids_roundtrip() {
        let verifier = match SandboxVerifier::new(None) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("跳过沙箱测试（rhai-runner 未编译）: {e}");
                return;
            }
        };
        let res = verifier
            .run(SAMPLE, RunnerAction::PrepareOids, None, None)
            .await
            .unwrap();
        assert!(res.ok, "stderr={}", res.stderr);
        let v: serde_json::Value = serde_json::from_str(&res.result).unwrap();
        assert_eq!(v[0]["method"], "get");
    }

    #[tokio::test]
    async fn sandbox_parse_roundtrip() {
        let verifier = match SandboxVerifier::new(None) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("跳过沙箱测试（rhai-runner 未编译）: {e}");
                return;
            }
        };
        let input =
            r#"{".1.3.6.1.2.1.1.3.0":{"oid_value_type":2,"value_str":"","value_num":123456}}"#;
        let res = verifier
            .run(SAMPLE, RunnerAction::Parse, Some(input), None)
            .await
            .unwrap();
        assert!(res.ok, "stderr={}", res.stderr);
        let v: serde_json::Value = serde_json::from_str(&res.result).unwrap();
        assert_eq!(v[0]["success"], true);
        assert_eq!(v[0]["value"]["number"], 123456.0);
    }
}
