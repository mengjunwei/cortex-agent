//! 独立 Rhai 脚本执行器（供 adk-sandbox 隔离调用）
//!
//! 这是 cortex-agent 的一个**附属二进制**，编译产物 `rhai-runner.exe`。
//! 主进程通过 `adk_sandbox::ProcessBackend` 以 `Language::Command` 方式 spawn 它，
//! 在子进程内运行 LLM 生成的 Rhai 脚本，确保死循环 / 爆内存等缺陷不影响主进程。
//!
//! host function 注册与安全限制**直接复用主 crate**（`cortex_agent::monitor`），
//! 杜绝"进程内（L1）注册了某函数、子进程（L2）漏注册"导致行为不一致的隐患。
//!
//! ## 协议
//!
//! stdin：UTF-8 JSON，格式如下：
//! ```json
//! {
//!   "script": "fn prepare_oids() { ... } fn parse(j) { ... }",
//!   "action": "prepare_oids" | "parse",
//!   "oid_values_json": "..."  // 仅 action=parse 时使用
//! }
//! ```
//!
//! stdout：UTF-8 JSON 结果（最后一行）
//! ```json
//! { "result": "..." }
//! { "error": "..." }
//! ```

use std::io::Read;

use rhai::{Engine, Scope};
use serde::{Deserialize, Serialize};

use cortex_agent::monitor::{apply_safety_limits, register_host_functions};

#[derive(Debug, Deserialize)]
struct RunnerRequest {
    script: String,
    action: String,
    #[serde(default)]
    oid_values_json: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RunnerResponse {
    Ok { result: String },
    Err { error: String },
}

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        emit_err("failed to read stdin");
        return;
    }

    let req: RunnerRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            emit_err(&format!("invalid stdin json: {e}"));
            return;
        }
    };

    let mut engine = Engine::new();
    apply_safety_limits(&mut engine);
    register_host_functions(&mut engine);

    let ast = match engine.compile(&req.script) {
        Ok(a) => a,
        Err(e) => {
            emit_err(&format!("compile: {e}"));
            return;
        }
    };

    let mut scope = Scope::new();
    let result: String = match req.action.as_str() {
        "prepare_oids" => match engine.call_fn::<String>(&mut scope, &ast, "prepare_oids", ()) {
            Ok(s) => s,
            Err(e) => {
                emit_err(&format!("prepare_oids: {e}"));
                return;
            }
        },
        "parse" => {
            let arg = req.oid_values_json.unwrap_or_default();
            match engine.call_fn::<String>(&mut scope, &ast, "parse", (arg,)) {
                Ok(s) => s,
                Err(e) => {
                    emit_err(&format!("parse: {e}"));
                    return;
                }
            }
        }
        other => {
            emit_err(&format!("unknown action: {other}"));
            return;
        }
    };

    println!(
        "{}",
        serde_json::to_string(&RunnerResponse::Ok { result }).unwrap()
    );
}

fn emit_err(msg: &str) {
    println!(
        "{}",
        serde_json::to_string(&RunnerResponse::Err {
            error: msg.to_string()
        })
        .unwrap()
    );
}
