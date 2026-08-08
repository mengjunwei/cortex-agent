//! `shell_command` 工具 — 统一的命令执行能力（custom agent 使用，沙箱内执行）。
//!
//! 设计参考 codex `shell_command`（`codex-rs/core/src/tools/handlers/shell_spec.rs`）。
//!
//! 三层安全策略：
//! - safelist 自动放行（ls/cat/python/git status 等）
//! - dangerous 自动阻断（rm -rf / / sudo / mkfs 等）
//! - 其余需用户审批（通过 SSE → 前端弹窗 → HTTP 回调）
//!
//! 命令在 session 沙箱目录（`{data_dir}/workspaces/sessions/{session_id}/`）执行。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::permissions::PermissionPolicy;
use crate::domain::shell_rules::{RuleDecision, ShellRuleStore};
use crate::server::shell_approval::{ApprovalDecision, ShellApprovalRegistry};
use crate::server::sse::SseEventMsg;

mod safety;
use safety::{Safety, classify};

/// stdout/stderr 单边最大字符数（截断防 token 爆炸）
/// 参考 codex: 默认 10KB,中间截断(保留头+尾)
const MAX_OUTPUT_CHARS: usize = 10_000;
/// 默认超时（毫秒）
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 命令执行环境变量白名单（env_clear 后只注入这些）。
///
/// shell 命令执行（沙箱与非沙箱）共用，保证两条路径环境一致——否则沙箱内（adk-sandbox 会
/// 清空继承环境）会因缺 PATH/HOME 等导致 "command not found"。
pub(crate) const ENV_WHITELIST: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "SystemRoot",
    "SYSTEMROOT",
    "WINDIR",
    "APPDATA",
    "LOCALAPPDATA",
    "PYTHONPATH",
    "PYTHONHOME",
    "VIRTUAL_ENV",
    "PSMODULEPATH",
    "COMSPEC",
    "PATHEXT",
    "USERNAME",
    "USERDOMAIN",
    "PROGRAMFILES",
    "PROGRAMDATA",
    // XDG 目录：LibreOffice/Qt/Chromium 等据此定位 runtime/config/cache（LibreOffice 单实例
    // 管道路径即派生自 XDG_RUNTIME_DIR）。沙箱内由 shell_sandbox 重定向到 workspace 可写
    // 子目录（见 execute_sandboxed）——宿主值在沙箱里只读，直接透传会导致 "no valid pipe
    // path found"。非沙箱/DangerFullAccess 路径直接透传宿主值。
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    // run_report.py 访问 URL：设了它脚本就确定性地 print 出报表访问 URL（见 monitor-report skill）。
    "MARVELNET_REPORT_BASE_URL",
    "MARVELNET_REPORT_OUTPUT_DIR",
];

/// shell_command 工具的运行时依赖
pub struct ShellToolDeps {
    /// session 沙箱目录（命令的 cwd）
    pub sandbox_dir: Arc<PathBuf>,
    /// 超时上限（毫秒），来自配置
    pub max_timeout_ms: u64,
    /// 审批等待超时（秒）
    pub approval_timeout_secs: u64,
    /// 审批注册表（全局共享，存在 AppState）
    pub approval_registry: Arc<ShellApprovalRegistry>,
    /// SSE 事件发送端（session 级，每次对话新建）
    pub sse_tx: tokio::sync::mpsc::Sender<axum::response::sse::Event>,
    /// 当前 session_id（用于 SSE 事件）
    pub session_id: String,
    /// 权限规则存储（DB 不可用时为 None）
    pub rule_store: Option<Arc<ShellRuleStore>>,
    /// 已执行命令历史（防循环）
    pub cmd_history: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// 权限策略（沙箱模式 + 审批策略 + 网络开关），驱动命令分类/审批决策与 prompt 注入。
    pub policy: PermissionPolicy,
    /// 沙箱内额外只读可见路径（截图目录、skill 目录等）。execute_sandboxed 会 exists 检查后
    /// 加入 allow_read 白名单——不存在的不加（避免 adk-sandbox canonicalize 失败导致 PolicyViolation）。
    pub readonly_extra: Vec<PathBuf>,
    /// 会话级 shell 环境快照路径（节点本地）。每条命令执行前 `source` 它，把用户交互式 shell 的
    /// PATH/venv（VIRTUAL_ENV 等）带进来——避免每条命令重复探测环境、venv 未激活导致找不到解释器。
    /// None=未启用（无沙箱 / 快照构建失败 / Windows）。见 `infra::shell_snapshot`。
    pub shell_snapshot: Option<PathBuf>,
    /// skill 根目录：用于 shell_command 的"禁止写入 skill 源码"检查（cat >/cp/python open('w') 等
    /// 写向此目录的命令直接拒，引导模型改用 edit_file/create_file 或告知用户）。None=未启用 skill。
    pub skill_dir: Option<PathBuf>,
    /// 取消令牌：用户点"停止"时 cancel，request_approval / execute_command 的 select! 立即解锁
    /// （对齐 codex exec.rs 的 ExecExpiration::Cancellation 模式）。
    pub cancel_token: CancellationToken,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct ShellCommandParams {
    /// Shell command to execute
    pub command: String,
    /// Working directory. Defaults to the session sandbox. Use the <path> from a skill injection to run commands in the skill's directory.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Timeout in milliseconds (default 30000)
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// 扫描 shell 输出里的 `[[ARTIFACT:path|title|mime]]` 标记 → 发 `FileArtifact` SSE 事件 +
/// 从展示文本里剥掉标记。让报表等产物自动变成前端文件卡片(无需模型贴链接)。
///
/// `path` 是相对会话工作区(cwd)的路径;filename 由 path 推导,size 由 stat 工作区文件得到。
/// 标记由 run_report.py 等脚本在生成文件后打印。
async fn emit_artifacts_and_strip(deps: &ShellToolDeps, result: &mut Value) {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\[\[ARTIFACT:([^|\]]+)\|([^|\]]*)\|([^|\]]*)\]\]").unwrap()
    });
    // clone 出 owned 字符串,避免后续可变借用 result 与不可变借用冲突
    let Some(output) = result
        .get("output")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return;
    };
    if !re.is_match(&output) {
        return;
    }
    // 1) 每个标记发一个 FileArtifact 事件
    for cap in re.captures_iter(&output) {
        let path = cap.get(1).unwrap().as_str().trim().to_string();
        let title = cap.get(2).unwrap().as_str().trim().to_string();
        let mime = cap.get(3).unwrap().as_str().trim().to_string();
        if path.is_empty() {
            continue;
        }
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let size = std::fs::metadata(deps.sandbox_dir.join(&path))
            .map(|m| m.len())
            .unwrap_or(0);
        let ev = SseEventMsg::FileArtifact {
            path,
            filename,
            title,
            mime,
            size,
        };
        let _ = deps
            .sse_tx
            .send(axum::response::sse::Event::default().data(ev.to_sse_data()))
            .await;
    }
    // 2) 从展示输出里剥掉标记(避免把 [[ARTIFACT:...]] 当文本回显)
    if let Some(obj) = result.as_object_mut() {
        if let Some(Value::String(out)) = obj.get_mut("output") {
            *out = re.replace_all(out, "").to_string();
        }
    }
}

pub fn create_shell_command_tool(deps: Arc<ShellToolDeps>) -> FunctionTool {
    let (shell_name, shell_hint) = if cfg!(target_os = "windows") {
        (
            "PowerShell",
            "You are running on Windows. Use PowerShell/cmd commands (e.g. `type` instead of `cat`, `dir` instead of `ls`, `Select-String` instead of `grep`). Do NOT use Unix-only commands like cat/head/sed/awk. NOTE: `python`/`python3` may be a Microsoft Store stub that fails with 'Python was not found' — use `py` (the Windows Python launcher) to run .py scripts instead.",
        )
    } else {
        (
            "sh",
            "You are running on Linux/macOS. Standard Unix shell commands are available.",
        )
    };
    let description = format!(
        "Execute a shell command via {}.\n{}\n\
         Safe commands (ls/dir, cat/type, grep/Select-String, git status, etc.) execute immediately.\n\
         Interpreter inline code (`python -c`/`bash -c`/`node -e`) requires user approval; running a script file (`python x.py`) executes immediately.\n\
         Dangerous commands (rm -rf /, sudo, mkfs) are blocked.\n\
         Other commands require user approval via a popup dialog.\n\
         stdout/stderr are truncated to 10000 chars each.\n\
         Use the `workdir` parameter to run commands in the skill's directory (the <path> from a <skill> injection).\n\
         The session workspace is the only writable area: write all generated files/scripts there (the shell's default cwd). Skill and system directories are read-only — you may RUN a skill's bundled scripts, but never modify them.\n\
         IMPORTANT: Do NOT repeat commands you have already run. Read previous tool results.",
        shell_name, shell_hint
    );
    FunctionTool::new(
        "shell_command",
        &description,
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let deps = deps.clone();
            async move {
                let params: ShellCommandParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("参数错误: {e}") })),
                };
                let mut result = execute_shell_command(&deps, &params).await;
                emit_artifacts_and_strip(&deps, &mut result).await;
                Ok(crate::tools::redact::redact_secrets(result))
            }
        },
    )
    .with_parameters_schema::<ShellCommandParams>()
}

async fn execute_shell_command(deps: &ShellToolDeps, params: &ShellCommandParams) -> Value {
    let cmd = params.command.trim();
    if cmd.is_empty() {
        return json!({ "ok": false, "error": "Command cannot be empty" });
    }

    // 防循环:检查是否已执行过相同命令
    {
        let mut history = deps.cmd_history.lock().await;
        let count = history.iter().filter(|c| c.as_str() == cmd).count();
        if count >= 2 {
            return json!({
                "ok": false,
                "error": "DUPLICATE_COMMAND: You have already run this exact command twice. The result is in your context. Do NOT run it again. Look at previous tool results and proceed with the next step.",
                "duplicate_count": count
            });
        }
        history.push(cmd.to_string());
        // 保留最近 50 条,防内存增长
        if history.len() > 50 {
            let drop_count = history.len() - 50;
            history.drain(0..drop_count);
        }
    }

    // skill 源码写保护：禁止向 skill 根目录写入（早于用户规则，硬拦）。
    // edit_file/create_file 已被 workspace 根关住够不着 skill 目录；这里堵 shell_command 这条路
    // （cat >/cp/mv/tee/mkdir/sed -i/python open('w') 等），把模型引导回"用 edit_file 或告知用户"。
    if let Some(skill_dir) = &deps.skill_dir {
        if let Some(target) = safety::detect_write_into(cmd, skill_dir) {
            tracing::warn!("[shell_command] 拒绝写入 skill 目录({target}): {cmd}");
            return json!({
                "ok": false, "blocked": true, "command": cmd,
                "reason": "Skill source is read-only: modifying its scripts/references/assets is forbidden. If the user needs a capability the skill does not provide, explicitly tell the user it is unsupported and propose a fallback (produce the output with the skill's existing capabilities, or use edit_file/create_file to generate a standalone artifact inside the session workspace). Do NOT patch the skill's bundled scripts.",
                "write_target": target,
            });
        }
    }

    // 先查用户自定义规则（DB glob 匹配）
    if let Some(store) = &deps.rule_store {
        if let Some(decision) = store.match_command(cmd).await {
            match decision {
                RuleDecision::Allow => {
                    let timeout_ms = params
                        .timeout_ms
                        .unwrap_or(DEFAULT_TIMEOUT_MS)
                        .min(deps.max_timeout_ms);
                    let cwd = resolve_workdir(&deps.sandbox_dir, &params.workdir);
                    return dispatch_execution(deps, &cwd, cmd, timeout_ms).await;
                }
                RuleDecision::Deny => {
                    return json!({ "ok": false, "blocked": true, "command": cmd, "reason": "规则匹配: 此命令被禁止" });
                }
                RuleDecision::Ask => { /* 继续走下面的审批流程 */ }
            }
        }
    }

    // 再查硬编码 safelist/dangerous，并叠加权限策略（SandboxMode/ApprovalPolicy）
    let level = classify(cmd);
    // Windows 无 OS 沙盒兜底：除纯只读命令外，其余 Allowed 命令（解释器/构建/文本工具等，
    // 可执行任意代码或有副作用）降级为 NeedsPrompt 走审批——safety 层无法检查其载荷。
    // Linux/macOS 有 bwrap/seatbelt OS 沙箱，Allowed 可自动执行（修高危③）。
    #[cfg(target_os = "windows")]
    let level = match level {
        Safety::Allowed if !safety::is_pure_readonly(cmd) => Safety::NeedsPrompt,
        other => other,
    };
    match level {
        Safety::Dangerous => {
            return json!({
                "ok": false, "blocked": true, "command": cmd,
                "reason": "危险命令被阻止"
            });
        }
        Safety::NeedsPrompt => match decide_with_policy(&deps.policy) {
            PromptDecision::Execute => { /* 直接执行（如 danger-full-access）*/ }
            PromptDecision::RequestApproval => match request_approval(deps, cmd).await {
                ApprovalOutcome::Approved => {}
                ApprovalOutcome::Rejected => {
                    return json!({
                        "ok": false, "denied": true, "command": cmd,
                        "reason": "用户拒绝了此命令"
                    });
                }
                ApprovalOutcome::Timeout => {
                    return json!({
                        "ok": false, "timeout": true, "command": cmd,
                        "reason": "审批超时（用户未响应）"
                    });
                }
                ApprovalOutcome::Cancelled => {
                    return json!({
                        "ok": false, "cancelled": true, "command": cmd,
                        "reason": "用户取消了操作"
                    });
                }
            },
            PromptDecision::Reject(reason) => {
                return json!({
                    "ok": false, "denied": true, "command": cmd,
                    "reason": reason
                });
            }
        },
        Safety::Allowed => {}
    }

    let timeout_ms = params
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(deps.max_timeout_ms);

    let cwd = resolve_workdir(&deps.sandbox_dir, &params.workdir);
    dispatch_execution(deps, &cwd, cmd, timeout_ms).await
}

/// 解析工作目录：优先用 LLM 指定的 workdir，否则用沙箱目录
fn resolve_workdir(sandbox: &std::path::Path, workdir: &Option<String>) -> std::path::PathBuf {
    match workdir {
        Some(wd) if !wd.trim().is_empty() => {
            let p = std::path::PathBuf::from(wd);
            if p.is_absolute() && p.exists() {
                p
            } else {
                sandbox.join(wd)
            }
        }
        _ => sandbox.to_path_buf(),
    }
}

enum ApprovalOutcome {
    Approved,
    Rejected,
    Timeout,
    Cancelled,
}

/// 权限策略对「需审批」命令的裁决。
enum PromptDecision {
    /// 直接执行（如 danger-full-access 模式）
    Execute,
    /// 走用户审批流程
    RequestApproval,
    /// 直接拒绝并回填原因
    Reject(&'static str),
}

/// 根据 [`PermissionPolicy`] 决定一条 NeedsPrompt 命令的命运。
///
/// - read-only：拒绝（禁止非只读命令）
/// - workspace-write：按 approval_policy（never→拒绝，其余→审批）
/// - danger-full-access：直接执行（仍受 dangerous 硬编码阻断兜底）
fn decide_with_policy(policy: &PermissionPolicy) -> PromptDecision {
    use crate::domain::permissions::{ApprovalPolicy, SandboxMode};
    match policy.sandbox_mode {
        SandboxMode::ReadOnly => PromptDecision::Reject("read-only 沙箱模式：禁止非只读命令"),
        SandboxMode::DangerFullAccess => PromptDecision::Execute,
        SandboxMode::WorkspaceWrite => match policy.approval_policy {
            ApprovalPolicy::Never => {
                PromptDecision::Reject("approval_policy=never：需审批的命令直接拒绝")
            }
            ApprovalPolicy::OnRequest
            | ApprovalPolicy::UnlessTrusted
            | ApprovalPolicy::OnRequestRuleRequestPermission => PromptDecision::RequestApproval,
        },
    }
}

async fn request_approval(deps: &ShellToolDeps, command: &str) -> ApprovalOutcome {
    let approval_id = uuid::Uuid::now_v7().to_string();

    let rx = deps
        .approval_registry
        .register(&approval_id, &deps.session_id)
        .await;

    // 通过 SseEventMsg::ShellApprovalRequest 构造事件，复用统一序列化路径（to_sse_data）
    let ev = SseEventMsg::ShellApprovalRequest {
        approval_id: approval_id.clone(),
        command: command.to_string(),
        session_id: deps.session_id.clone(),
    };
    let _ = deps
        .sse_tx
        .send(axum::response::sse::Event::default().data(ev.to_sse_data()))
        .await;

    // 同时监听「审批结果（带超时）」和「用户取消」：任一触发即解锁，避免点停止后 agent 卡死。
    // 对齐 codex：cancel 信号以 select! 抢占方式打断阻塞等待，而非协作式轮询。
    tokio::select! {
        res = tokio::time::timeout(Duration::from_secs(deps.approval_timeout_secs), rx) => match res {
            Ok(Ok(ApprovalDecision::Approved)) => ApprovalOutcome::Approved,
            Ok(Ok(ApprovalDecision::Rejected)) => ApprovalOutcome::Rejected,
            Ok(Err(_)) => ApprovalOutcome::Timeout,
            Err(_) => {
                deps.approval_registry.remove(&approval_id).await;
                ApprovalOutcome::Timeout
            }
        },
        _ = deps.cancel_token.cancelled() => {
            // 用户点停止：清理 pending（双保险，cancel 接口也会 cancel_session）
            deps.approval_registry.remove(&approval_id).await;
            ApprovalOutcome::Cancelled
        }
    }
}

async fn execute_command(
    root: &std::path::Path,
    cmd: &str,
    timeout_ms: u64,
    cancel_token: &CancellationToken,
    snapshot: Option<&std::path::Path>,
) -> Value {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return json!({ "ok": false, "error": "Command cannot be empty" });
    }

    // Unix: source 会话 shell 快照（PATH/venv），失败静默不阻断。Windows 无等价机制（venv 激活走
    // activate.bat），跳过。snapshot 为 None 时无前缀，行为与原先一致。
    let prefix = if cfg!(unix) {
        crate::infra::shell_snapshot::source_prefix(snapshot)
    } else {
        String::new()
    };
    let final_cmd: String = if prefix.is_empty() {
        cmd.to_string()
    } else {
        format!("{prefix}{cmd}")
    };
    let cmd = final_cmd.as_str();

    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "windows") {
        (
            "powershell",
            vec!["-NoProfile", "-NonInteractive", "-Command", cmd],
        )
    } else {
        ("sh", vec!["-c", cmd])
    };

    let start = std::time::Instant::now();
    let mut command = tokio::process::Command::new(program);
    command.args(&args);
    command.current_dir(root);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);

    command.env_clear();
    for key in ENV_WHITELIST {
        if let Ok(val) = std::env::var(key) {
            command.env(key, val);
        }
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return json!({ "ok": false, "error": format!("Failed to spawn: {e}") });
        }
    };
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    let timeout = Duration::from_millis(timeout_ms);
    // 增量 cap 读取 stdout/stderr（修高危⑤，对标 codex exec.rs read_output + append_capped）：
    // 累计到 CAP_BYTES 后丢弃超额但继续读到 EOF（防管道写端背压死锁），内存上限恒定 ≤2×CAP。
    // 刻意不 spawn 读 task：select! 落选分支的 future 被 drop 即取消、管道读端随之释放——
    // spawn detach 的读 task 在孙进程持有管道写端时会永久泄漏（kill_on_drop 只杀直接子进程）。
    const CAP_BYTES: usize = 1_048_576; // 1 MiB 单边
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    // 并发「读 stdout + 读 stderr + 等进程退出」。读 future 用 pin 让它可被多个分支 poll。
    // 必须与 wait 并发读：OS 管道缓冲仅 ~64KB，子进程写满后 write() 阻塞无法退出，
    // 先 wait 再读会死锁到 timeout（修 R2 候选1）。早退分支 future drop 即释放管道读端，无泄漏。
    // read_fut 持有 buf 的可变借用；块内只算 status，块结束 read_fut drop、借用释放后，
    // 块外再 decode buf（修 E0502 借用冲突）。
    let status = {
        let read_fut = async {
            tokio::join!(
                read_capped_into(stdout_pipe, &mut stdout_buf, CAP_BYTES),
                read_capped_into(stderr_pipe, &mut stderr_buf, CAP_BYTES),
            );
        };
        tokio::pin!(read_fut);
        let st = tokio::select! {
            res = tokio::time::timeout(timeout, child.wait()) => match res {
                Ok(Ok(s)) => {
                    // 进程已退出，管道可能仍有未读数据；给短 grace 读尽（孙进程持管道写端时读不到
                    // EOF，grace 后放弃，buf 保留已读真尾，无泄漏不挂死）。
                    tokio::select! {
                        _ = &mut read_fut => {}
                        _ = tokio::time::sleep(Duration::from_millis(2_000)) => {
                            tracing::debug!("[shell_command] 子进程已退出但管道未关闭（疑孙进程持有），grace 超时放弃剩余输出");
                        }
                    }
                    s
                }
                Ok(Err(e)) => {
                    return json!({ "ok": false, "error": format!("Execution failed: {e}") });
                }
                Err(_) => {
                    return json!({ "ok": false, "error": format!("Timed out (>{timeout_ms}ms)"), "timed_out": true });
                }
            },
            _ = &mut read_fut => {
                // 管道全部读到 EOF（子进程已关闭输出），仍需 wait 收割进程（防僵尸 + 拿 exit code）。
                match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => return json!({ "ok": false, "error": format!("Execution failed: {e}") }),
                    Err(_) => return json!({ "ok": false, "error": "Timed out waiting for process exit after pipe EOF", "timed_out": true }),
                }
            },
            _ = cancel_token.cancelled() => {
                return json!({ "ok": false, "error": "Cancelled by user", "cancelled": true });
            }
        };
        st
    };

    // 块结束 read_fut 已 drop，buf 可变借用释放，安全 decode。
    let stdout_raw = decode_console_output(&stdout_buf);
    let stderr_raw = decode_console_output(&stderr_buf);

    let mut combined = String::with_capacity(stdout_raw.len() + stderr_raw.len() + 64);
    combined.push_str(&stdout_raw);
    if !stderr_raw.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr_raw);
    }
    // buf 超 CAP 时已被 read_capped_into 保尾为真尾窗口；truncate_str 再按字符上限截断（保头）。
    let combined_truncated = truncate_str(&combined, MAX_OUTPUT_CHARS);
    let exit_code = status.code().unwrap_or(-1);
    let wall_time = start.elapsed().as_secs_f64();

    let result_text = format!(
        "Exit code: {exit_code}\nWall time: {wall_time:.1}s\nOutput:\n{combined_truncated}"
    );

    json!({
        "ok": status.success(),
        "exit_code": exit_code,
        "output": result_text,
        "duration_ms": start.elapsed().as_millis(),
    })
}

/// 增量 cap 读取到外部 buf（修高危⑤，对标 codex `exec.rs` 的 `read_output` + `append_capped`）。
///
/// 未超 `cap` 时顺序追加；超 `cap` 后进入「保尾」模式——buf 始终是「当前流的最后 keep_tail 字节」
/// （真尾滚动窗口），保住命令结尾输出（最终错误/汇总通常在最后）。**不插入任何分隔标记**。
/// 注意：buf 内是**字节**，UTF-8 切分发生在字符边界上（decode 时按 lossy 处理），不乱码。
async fn read_capped_into<R: tokio::io::AsyncRead + Unpin>(mut r: R, buf: &mut Vec<u8>, cap: usize) {
    use tokio::io::AsyncReadExt;
    let mut tmp = [0u8; 8192];
    loop {
        match r.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                let data = &tmp[..n];
                if buf.len() + n <= cap {
                    buf.extend_from_slice(data);
                    continue;
                }
                // 保尾：buf 保留尾部 keep_tail 字节窗口——新数据到达时，先挤出等量最旧字节再追加，
                // 使 buf 始终是"当前流的最后 keep_tail 字节"（真尾）。挤出用批量 drain，非逐字节 O(n²)。
                let keep_tail = cap / 2;
                let mut remaining = data;
                while !remaining.is_empty() {
                    let need = buf.len() + remaining.len() - keep_tail;
                    if need > 0 {
                        // 需挤出 need 字节：buf 太满。一次 drain 掉 need（可能含部分 head，但此时已超 cap，
                        // 真尾优先），再从 remaining 取可填充部分。
                        let drain = need.min(buf.len());
                        buf.drain(0..drain);
                    }
                    let take = (keep_tail - buf.len()).min(remaining.len());
                    if take == 0 {
                        break;
                    }
                    buf.extend_from_slice(&remaining[..take]);
                    remaining = &remaining[take..];
                }
            }
            Err(_) => break,
        }
    }
}

/// 按权限策略 + 平台分发命令执行。
///
/// Linux/macOS + 真沙箱模式（非 danger-full-access）→ [`crate::infra::shell_sandbox::execute_sandboxed`]
/// （OS 强制），enforcer 不可用或执行失败时降级；Windows / DangerFullAccess → [`execute_command`]
/// （非沙箱直接执行）。
async fn dispatch_execution(
    deps: &ShellToolDeps,
    cwd: &std::path::Path,
    cmd: &str,
    timeout_ms: u64,
) -> Value {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use crate::domain::permissions::SandboxMode;
        if !matches!(deps.policy.sandbox_mode, SandboxMode::DangerFullAccess) {
            // 沙箱执行也监听 cancel：取消时立即返回（前端不卡到 sandbox timeout）。
            // 沙箱内的进程由 ExecRequest.timeout 兜底回收。
            let readonly: Vec<&std::path::Path> = deps
                .readonly_extra
                .iter()
                .map(std::path::PathBuf::as_path)
                .collect();
            let sandbox_fut = crate::infra::shell_sandbox::execute_sandboxed(
                cmd,
                cwd,
                &readonly,
                deps.shell_snapshot.as_deref(),
                deps.policy,
                Duration::from_millis(timeout_ms),
            );
            let result = tokio::select! {
                r = sandbox_fut => r,
                _ = deps.cancel_token.cancelled() => {
                    tracing::info!("[shell_command] 沙箱命令被用户取消");
                    return json!({ "ok": false, "error": "Cancelled by user", "cancelled": true });
                }
            };
            match result {
                Some((exit_code, combined, duration_ms)) => {
                    let combined_truncated = truncate_str(&combined, MAX_OUTPUT_CHARS);
                    let wall_secs = (duration_ms as f64) / 1000.0;
                    // 与 execute_command 输出结构对齐：补 Wall time 行 + duration_ms 字段
                    let result_text = format!(
                        "Exit code: {exit_code}\nWall time: {wall_secs:.1}s\nOutput:\n{combined_truncated}"
                    );
                    return json!({
                        "ok": exit_code == 0,
                        "exit_code": exit_code,
                        "output": result_text,
                        "duration_ms": duration_ms,
                    });
                }
                None => {
                    // enforcer 不可用（如未装 bwrap）或执行失败：fail-closed，拒绝执行。
                    // 安全特性不能静默降级为非沙箱——否则 workspace-write 会失去 OS 约束。
                    tracing::error!(
                        "[shell_command] 沙箱强制不可用/失败，fail-closed 拒绝执行: {}",
                        cmd
                    );
                    return json!({
                        "ok": false,
                        "error": "沙箱强制不可用或执行失败；workspace-write/read-only 模式拒绝无沙箱执行（请检查 bwrap/seatbelt 是否安装）",
                    });
                }
            }
        }
    }
    // Windows / DangerFullAccess 分支：非沙箱直接执行（仍 source 快照，Unix 上取 PATH/venv）。
    execute_command(cwd, cmd, timeout_ms, &deps.cancel_token, deps.shell_snapshot.as_deref()).await
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // 中间截断:保留前 40% + 后 40%,中间 20% 丢弃(参考 codex head_tail_buffer)
        let head_size = max * 2 / 5;
        let tail_size = max * 2 / 5;
        let total_lines = s.lines().count();
        let head: String = s.chars().take(head_size).collect();
        let tail: String = s
            .chars()
            .rev()
            .take(tail_size)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let omitted = s.len() - head_size - tail_size;
        format!(
            "Warning: truncated output (original {} bytes, {} lines)\n\n{}\n\n... [{} bytes omitted] ...\n\n{}",
            s.len(),
            total_lines,
            head,
            omitted,
            tail
        )
    }
}

/// 解码命令输出字节：优先 UTF-8，失败则按 GBK 解码。
///
/// Windows 命令（如 `dir`）输出默认 GBK（cp936），`from_utf8_lossy` 会把中文（如"目录"
/// 表头）解成乱码 "Ŀ¼"，导致模型看不懂工具结果而误判任务完成。先试 UTF-8，非 UTF-8
/// 则按 GBK 解码（Windows 中文系统默认）。
fn decode_console_output(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    let (cow, _encoding, _had_errors) = encoding_rs::GBK.decode(raw);
    cow.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::permissions::{ApprovalPolicy, PermissionPolicy, SandboxMode};
    use crate::tools::code::tests_helpers::TmpWs;

    #[tokio::test]
    async fn runs_echo_successfully() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = execute_command(&root, "echo hello", 5000, &CancellationToken::new(), None).await;
        assert_eq!(r["ok"], true, "echo should succeed: {:?}", r);
        let output = r["output"].as_str().unwrap_or("");
        assert!(
            output.contains("hello"),
            "output should contain hello: {output}"
        );
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let cmd = if cfg!(target_os = "windows") {
            "cmd /C exit 42"
        } else {
            "exit 42"
        };
        let r = execute_command(&root, cmd, 5000, &CancellationToken::new(), None).await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn times_out_long_running_command() {
        let ws = TmpWs::new();
        let root = ws.canon();
        // 用 PowerShell 原生 Start-Sleep 保证稳定睡眠 > 超时阈值。
        // 旧用例 `ping -n 10 127.0.0.1 > nul` 在 `powershell -Command` 下不可靠：
        // `>` 重定向 + 外部 ping 解析会让进程提前结束，走不到超时分支、timed_out 字段缺失。
        let cmd = if cfg!(target_os = "windows") {
            "Start-Sleep -Seconds 10"
        } else {
            "sleep 10"
        };
        let r = execute_command(&root, cmd, 500, &CancellationToken::new(), None).await;
        assert_eq!(r["ok"], false, "应超时失败: {:?}", r);
        assert_eq!(r["timed_out"], true);
    }

    #[tokio::test]
    async fn execute_empty_command_fails() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = execute_command(&root, "   ", 5000, &CancellationToken::new(), None).await;
        assert_eq!(r["ok"], false);
    }

    #[test]
    fn truncate_str_keeps_short() {
        assert_eq!(truncate_str("short", 100), "short");
    }

    #[tokio::test]
    async fn read_capped_keeps_small_output_intact() {
        // 小输出原样保留
        let data = b"hello world, this is under the cap";
        let mut buf = Vec::new();
        read_capped_into(&data[..], &mut buf, 1024).await;
        assert_eq!(buf, data);
    }

    #[tokio::test]
    async fn read_capped_keeps_true_tail_when_over_cap() {
        // 超 cap：buf 保真尾（最后是最后写入的字节）
        let cap = 100usize;
        let mut buf = Vec::new();
        // 写入一段 > cap 的字节流
        let data: Vec<u8> = (0..250u32).map(|i| (i % 26) as u8 + b'a').collect();
        let mut slice: &[u8] = &data;
        read_capped_into(&mut slice, &mut buf, cap).await;
        let keep_tail = cap / 2;
        assert!(buf.len() <= cap.max(keep_tail), "buf 不应超过 cap");
        // 最后一个字节应等于输入的最后一个字节（真尾）
        assert_eq!(*buf.last().unwrap(), *data.last().unwrap(), "应保住真尾");
    }

    #[test]
    fn truncate_str_cuts_long() {
        let long = "x".repeat(100);
        let t = truncate_str(&long, 10);
        assert!(t.len() < long.len());
        assert!(t.contains("truncated"));
    }

    #[test]
    fn readonly_rejects_needs_prompt() {
        let p = PermissionPolicy::new(SandboxMode::ReadOnly, ApprovalPolicy::UnlessTrusted, false);
        assert!(matches!(decide_with_policy(&p), PromptDecision::Reject(_)));
    }

    #[test]
    fn danger_full_access_executes_needs_prompt() {
        let p = PermissionPolicy::new(SandboxMode::DangerFullAccess, ApprovalPolicy::Never, false);
        assert!(matches!(decide_with_policy(&p), PromptDecision::Execute));
    }

    #[test]
    fn workspace_write_never_rejects() {
        let p = PermissionPolicy::new(SandboxMode::WorkspaceWrite, ApprovalPolicy::Never, false);
        assert!(matches!(decide_with_policy(&p), PromptDecision::Reject(_)));
    }

    #[test]
    fn workspace_write_unless_trusted_requests_approval() {
        let p = PermissionPolicy::new(
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::UnlessTrusted,
            false,
        );
        assert!(matches!(
            decide_with_policy(&p),
            PromptDecision::RequestApproval
        ));
    }
}
