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

use crate::permissions::{PermissionPolicy, SandboxMode};
use crate::domain::shell_rules::{RuleDecision, ShellRuleStore};

pub mod approval;
pub mod events;
mod safety;

use approval::{ApprovalDecision, ShellApprovalRegistry};
use events::ToolEventSink;
use safety::{Safety, classify_with_sandbox};

/// stdout/stderr 单边最大字符数（截断防 token 爆炸）
/// 参考 codex: 默认 10KB,中间截断(保留头+尾)
const MAX_OUTPUT_CHARS: usize = 10_000;
/// 默认超时（毫秒）
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 沙箱可见性如实描述（进 shell 工具 description，模型可据此直接用 $VIRTUAL_ENV、
/// 不再满盘 find / 找 venv）。与 shell_sandbox v1.17.1 姿态对齐：整盘只读 +
/// 凭证目录 mask + /tmp 会话持久目录。改挂载姿态时必须同步这里（测试断言内容）。
const SANDBOX_VISIBILITY_NOTE: &str = "The session workspace is the only writable area on the \
host: write all generated files/scripts there (the shell's default cwd). In workspace-write \
sandbox mode, $TMPDIR and $HOME are redirected to writable per-session directories, and /tmp \
maps to a session-persistent directory (writes survive across commands within the session); \
in read-only mode nothing on disk is writable. Everything else on the host filesystem — \
including skill directories, system directories, and any Python venv that was active in the \
user's shell at session start (run its interpreter directly, e.g. $VIRTUAL_ENV/bin/python) — \
is readable read-only, except credential directories (~/.ssh, ~/.gnupg, ~/.aws, ~/.azure, \
~/.kube, ~/.docker, /etc/ssh) which are hidden. You may RUN a skill's bundled scripts, but \
never modify them.";

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

/// 助手 env_vars 注入黑名单：可劫持子进程加载/启动的变量，注入沙箱会破坏隔离边界。
///
/// 助手 env_vars 经 [`ShellToolDeps::extra_env`] 注入子进程环境（白名单之后，可覆盖宿主值）。
/// 下列变量若被助手任意设置，会让沙箱内的 sh/python/node 等加载攻击者提供的 .so/.py（如
/// LD_PRELOAD 指向 session 可写目录），等于绕过沙箱——故在注入前剥离。`PYTHONPATH`/
/// `PYTHONHOME` 本就在白名单（透传宿主值），这里剥离助手侧的覆盖，让它们回归宿主值。
pub(crate) const ENV_DENYLIST: &[&str] = &[
    // 动态链接器（glibc / macOS dyld）
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    // 解释器路径/选项注入
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONSTARTUP",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5LIB",
    "PERL5OPT",
    "PERLLIB",
    "RUBYLIB",
    "RUBYOPT",
    // shell 启动脚本注入
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
    "IFS",
    // 其他平台 / 运行时
    "SHLIB_PATH",
    "LIBPATH",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
];

/// 剥离 [`ENV_DENYLIST`] 命中的键（大小写敏感——加载器/解释器只认规范大写名）。
/// 逐个 warn 记录被丢的键，便于排查「为何我的 LD_PRELOAD 没生效」。
pub fn sanitize_extra_env(extra: Vec<(String, String)>) -> Vec<(String, String)> {
    extra
        .into_iter()
        .filter(|(k, _)| {
            if ENV_DENYLIST.contains(&k.as_str()) {
                tracing::warn!(
                    "[shell_command] 助手 env_var '{k}' 在注入黑名单中（可劫持子进程），已剥离"
                );
                false
            } else {
                true
            }
        })
        .collect()
}

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
    /// 事件推送抽象（SSE 传输层实现），斩断 tools→server 依赖
    pub event_sink: Arc<dyn ToolEventSink>,
    /// 当前 session_id（用于 SSE 事件）
    pub session_id: String,
    /// ADK 会话服务：artifact 事件持久化用（落 system 事件 state_delta，
    /// 刷新页面后 get_session_history 据此恢复文件卡片）。None=DB 未启用，仅实时卡片。
    pub session_service: Option<std::sync::Arc<dyn adk_rust::session::SessionService>>,
    /// 权限规则存储（DB 不可用时为 None）
    pub rule_store: Option<Arc<ShellRuleStore>>,
    /// 已执行命令历史（防循环）
    pub cmd_history: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// 权限策略（沙箱模式 + 审批策略 + 网络开关），驱动命令分类/审批决策与 prompt 注入。
    pub policy: PermissionPolicy,
    /// 沙箱内额外只读可见路径（截图目录、skill 目录等）。execute_sandboxed 会 exists 检查后
    /// 加入 allow_read 白名单——不存在的不加（避免 adk-sandbox canonicalize 失败导致 PolicyViolation）。
    pub readonly_extra: Vec<PathBuf>,
    /// 沙箱内 mask（空 tmpfs 覆盖，内容不可见）的路径。用于 skill 白名单硬隔离：整盘
    /// 只读（bwrap --ro-bind / /）下 readonly_extra 收窄无效，须把白名单外的 skill
    /// 子目录 mask 掉，否则 `cat <skill_dir>/<被隐藏skill>/SKILL.md` 可绕过隔离。
    pub masked_paths: Vec<PathBuf>,
    /// 会话级 shell 环境快照路径（节点本地）。每条命令执行前 `source` 它，把用户交互式 shell 的
    /// PATH/venv（VIRTUAL_ENV 等）带进来——避免每条命令重复探测环境、venv 未激活导致找不到解释器。
    /// None=未启用（无沙箱 / 快照构建失败 / Windows）。见 `infra::sandbox::shell_snapshot`。
    pub shell_snapshot: Option<PathBuf>,
    /// skill 根目录：用于 shell_command 的"禁止写入 skill 源码"检查（cat >/cp/python open('w') 等
    /// 写向此目录的命令直接拒，引导模型改用 edit_file/create_file 或告知用户）。None=未启用 skill。
    pub skill_dir: Option<PathBuf>,
    /// 助手级环境变量：会话执行时注入子进程环境（白名单之后，可覆盖 PATH 等——等同真实 shell 语义）。
    /// 由 SSE 层从 `Assistant.env_vars` 填充；skill 脚本等可经 `os.environ['KEY']` 读取。
    pub extra_env: Vec<(String, String)>,
    /// 取消令牌：用户点"停止"时 cancel，request_approval / execute_command 的 select! 立即解锁
    /// （对齐 codex exec.rs 的 ExecExpiration::Cancellation 模式）。
    pub cancel_token: CancellationToken,
    /// 最近推送的 artifact 路径 → 时间戳（同路径 10 秒内去重，避免脚本重复打印时前端刷卡片）。
    pub recent_artifacts: Arc<tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
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
    let re =
        RE.get_or_init(|| Regex::new(r"\[\[ARTIFACT:([^|\]]+)\|([^|\]]*)\|([^|\]]*)\]\]").unwrap());
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
    // 1) 每个标记发一个 FileArtifact 事件（同路径 10 秒内去重：脚本重复打印时不刷卡片）
    let mut recent = deps.recent_artifacts.lock().await;
    let now = std::time::Instant::now();
    // 清理过期条目（>60s）
    recent.retain(|_, t| now.duration_since(*t).as_secs() < 60);
    for cap in re.captures_iter(&output) {
        let path = cap.get(1).unwrap().as_str().trim().to_string();
        let title = cap.get(2).unwrap().as_str().trim().to_string();
        let mime = cap.get(3).unwrap().as_str().trim().to_string();
        if path.is_empty() {
            continue;
        }
        // 同路径 30 秒内已推送过 → 跳过（删了重建等场景：30s 后自然放行）
        if let Some(last) = recent.get(&path) {
            if now.duration_since(*last).as_secs() < 30 {
                continue;
            }
        }
        recent.insert(path.clone(), now);
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let size = std::fs::metadata(deps.sandbox_dir.join(&path))
            .map(|m| m.len())
            .unwrap_or(0);
        // 持久化载荷先按借用构造（json! 内部 to_value(&v)），原值随后 move 进事件 sink
        let artifact_state = json!({
            "path": path, "filename": filename, "title": title,
            "mime": mime, "size": size,
        });
        deps.event_sink
            .send_file_artifact(path, filename, title, mime, size)
            .await;
        // 持久化 artifact 标记：落一条 system 事件（state_delta["app:artifact"]，对齐
        // app:model_switched 时间线标记模式），刷新页面后 collect_history_messages
        // 据此恢复文件卡片。state_delta 不进 LLM 回放上下文，不污染对话；
        // 落库失败仅告警（实时卡片已推，不影响本轮展示）。
        if let Some(svc) = &deps.session_service {
            let mut event = adk_rust::Event::new(&uuid::Uuid::now_v7().to_string());
            event.author = "system".to_string();
            event
                .actions
                .state_delta
                .insert("app:artifact".to_string(), artifact_state);
            if let Err(e) = svc.append_event(&deps.session_id, event).await {
                tracing::warn!(
                    "[artifact] 会话 {} artifact 事件落库失败: {}",
                    deps.session_id,
                    e
                );
            }
        }
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
         {SANDBOX_VISIBILITY_NOTE}\n\
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
                    Err(e) => {
                        return Ok(
                            json!({ "ok": false, "error": format!("invalid arguments: {e}") }),
                        );
                    }
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

    // 灾难级黑名单硬拦：先于用户 Allow 规则——rm -rf / 等破坏宿主形态没有
    // 「用户已信任」的合理场景，规则可豁免审批提示（NeedsPrompt 层的工具级命令），
    // 但不得豁免灾难拦截（两级黑名单的上级，见 safety::CATASTROPHIC）。
    if safety::is_catastrophic_command(cmd) {
        tracing::warn!("[shell_command] 拒绝灾难级命令（不可被 Allow 规则豁免）: {cmd}");
        return json!({
            "ok": false, "blocked": true, "command": cmd,
            "reason": "blocked: catastrophic command (cannot be overridden by user rules)"
        });
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
                    return json!({ "ok": false, "blocked": true, "command": cmd, "reason": "matched a user rule: this command is denied" });
                }
                RuleDecision::Ask => { /* 继续走下面的审批流程 */ }
            }
        }
    }

    // 再查硬编码 safelist/dangerous，并叠加权限策略（SandboxMode/ApprovalPolicy）。
    // A′ 门控：git/npm/cargo 只读白名单仅在命令真正进 OS 沙箱时成立——仓库配置
    // （.git/config 的 core.pager/diff.*.command 等）可让只读子命令执行任意 helper，
    // 沙箱内 helper 只能以沙箱权限跑（与 python x.py 同级），白名单可接受；
    // DangerFullAccess / 无沙箱平台裸跑时前提不成立 → 降级审批。
    let os_sandboxed = cfg!(any(target_os = "linux", target_os = "macos"))
        && !matches!(deps.policy.sandbox_mode, SandboxMode::DangerFullAccess);
    let level = classify_with_sandbox(cmd, os_sandboxed);
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
                "reason": "blocked: dangerous command"
            });
        }
        Safety::NeedsPrompt => match decide_with_policy(&deps.policy) {
            PromptDecision::Execute => { /* 直接执行（如 danger-full-access）*/ }
            PromptDecision::RequestApproval => match request_approval(deps, cmd).await {
                ApprovalOutcome::Approved => {}
                ApprovalOutcome::Rejected => {
                    return json!({
                        "ok": false, "denied": true, "command": cmd,
                        "reason": "user rejected this command"
                    });
                }
                ApprovalOutcome::Timeout => {
                    return json!({
                        "ok": false, "timeout": true, "command": cmd,
                        "reason": "approval timed out (no user response)"
                    });
                }
                ApprovalOutcome::Cancelled => {
                    return json!({
                        "ok": false, "cancelled": true, "command": cmd,
                        "reason": "cancelled by user"
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
    use crate::permissions::{ApprovalPolicy, SandboxMode};
    match policy.sandbox_mode {
        SandboxMode::ReadOnly => {
            PromptDecision::Reject("read-only sandbox mode: non-read-only commands are forbidden")
        }
        SandboxMode::DangerFullAccess => PromptDecision::Execute,
        SandboxMode::WorkspaceWrite => match policy.approval_policy {
            // 自动批准（定时任务无人值守）：直接执行，仍受 dangerous 硬编码阻断兜底。
            ApprovalPolicy::Auto => PromptDecision::Execute,
            ApprovalPolicy::Never => PromptDecision::Reject(
                "approval_policy=never: commands requiring approval are denied outright",
            ),
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

    // 通过 ToolEventSink 推送审批请求（SSE 层实现复用 to_sse_data 序列化路径）
    deps.event_sink
        .send_approval_request(
            approval_id.clone(),
            command.to_string(),
            deps.session_id.clone(),
        )
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

fn sandbox_denial_hint(
    exit_code: i32,
    combined: &str,
    cwd: &std::path::Path,
    mode: &crate::permissions::SandboxMode,
) -> String {
    if exit_code == 0 {
        return String::new();
    }
    const MARKERS: &[&str] = &[
        "Read-only file system",
        "read-only file system",
        "EROFS",                                // errno 名，部分工具直呼其名
        "no valid pipe path found",             // LibreOffice：XDG_RUNTIME_DIR 派生管道写失败
        "Attempt to write a readonly database", // SQLite EROFS 映射
        // seccomp 兜底新增的拒绝形态:命中禁网规则的 syscall(connect/bind 等)返回
        // EPERM(对齐 codex denial.rs 关键词)。刻意不含 ENOENT/"No such file"——
        // 那在普通命令失败里太常见,误报会让模型把真实缺文件误判为沙箱限制
        "Operation not permitted",
        "operation not permitted",
        "EPERM",
    ];
    if !MARKERS.iter().any(|m| combined.contains(m)) {
        return String::new();
    }
    match mode {
        crate::permissions::SandboxMode::ReadOnly => {
            "\n---\n[sandbox] The error above is characteristic of the OS sandbox blocking a \
write. The sandbox is in READ-ONLY mode: NO location on disk is writable, not even the \
working directory. Retrying the same command with different paths will keep failing. Tell \
the user exactly what you need to write and where, and ask them to switch the sandbox mode \
to workspace-write (or perform the write themselves) instead of retrying."
                .to_string()
        }
        _ => format!(
            "\n---\n[sandbox] The error above is characteristic of the OS sandbox blocking a write \
or a network syscall (only writable locations inside the sandbox: (1) the working directory {cwd}, \
(2) $TMPDIR and (3) $HOME — both already redirected to writable per-session directories \
under .cortex-tmp/, and (4) /tmp — a session-persistent directory; network is disabled at both \
the namespace and syscall level, so retrying network commands will keep failing with EPERM). \
Do NOT retry the same command with \
randomly guessed paths. Instead: write outputs under the working directory or $TMPDIR, or \
point the tool at an explicit writable dir it supports (e.g. HOME=\"$TMPDIR\", --tmpdir=, \
or XDG_* env vars). If the task genuinely requires writing outside the sandbox or network \
access, tell the user instead of retrying.",
            cwd = cwd.display(),
        ),
    }
}

/// 按权限策略 + 平台分发命令执行。
///
/// Linux/macOS + 真沙箱模式（非 danger-full-access）→ [`crate::infra::sandbox::shell_sandbox::execute_sandboxed`]
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
        if !matches!(deps.policy.sandbox_mode, SandboxMode::DangerFullAccess) {
            // 沙箱执行也监听 cancel：取消时立即返回（前端不卡到 sandbox timeout）。
            // 沙箱内的进程由 ExecRequest.timeout 兜底回收。
            let readonly: Vec<&std::path::Path> = deps
                .readonly_extra
                .iter()
                .map(std::path::PathBuf::as_path)
                .collect();
            let masked: Vec<&std::path::Path> = deps
                .masked_paths
                .iter()
                .map(std::path::PathBuf::as_path)
                .collect();
            let sandbox_fut = crate::infra::sandbox::shell_sandbox::execute_sandboxed(
                crate::infra::sandbox::shell_sandbox::SandboxExec {
                    cmd,
                    workspace: cwd,
                    // HOME cache symlink 桥仅限会话工作区内:workdir 指到工作区外(skill 目录等)
                    // 时不往外部目录塞绝对路径 symlink(污染源码目录),代价只是该次 cache 冷启动。
                    // 两侧必须先 canonicalize:sandbox_dir 派生自 data_dir(默认相对 "./data"),
                    // 而 resolve_workdir 可能返回绝对路径——词法 starts_with 会误判(绝对 workdir
                    // 误判在外→桥失效;含 .. 的相对 workdir 词法通过→逃逸出工作区建 symlink)。
                    // canonicalize 失败(目录刚消失)保守 false:不桥接只冷 cache,无污染风险。
                    bridge_home_cache: match (
                        std::fs::canonicalize(cwd),
                        std::fs::canonicalize(deps.sandbox_dir.as_path()),
                    ) {
                        (Ok(c), Ok(w)) => c.starts_with(w),
                        _ => false,
                    },
                    readonly_extra: &readonly,
                    masked_paths: &masked,
                    snapshot: deps.shell_snapshot.as_deref(),
                    policy: deps.policy,
                    timeout: Duration::from_millis(timeout_ms),
                    extra_env: &deps.extra_env,
                },
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
                    // 沙箱拒绝特征命中时在结果尾部追加可写位置 hint（对齐 Claude Code
                    // 「失败输出注入违规详情」），避免模型对着报错盲试浪费轮次
                    let hint =
                        sandbox_denial_hint(exit_code, &combined, cwd, &deps.policy.sandbox_mode);
                    // 与 execute_command 输出结构对齐：补 Wall time 行 + duration_ms 字段
                    let result_text = format!(
                        "Exit code: {exit_code}\nWall time: {wall_secs:.1}s\nOutput:\n{combined_truncated}{hint}"
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
                        "error": "sandbox enforcement is unavailable or failed; refusing to run without a sandbox in workspace-write/read-only mode (check that bwrap/seatbelt is installed)",
                    });
                }
            }
        }
    }
    // Windows / DangerFullAccess 分支：非沙箱直接执行（仍 source 快照，Unix 上取 PATH/venv）。
    execute_command(
        cwd,
        cmd,
        timeout_ms,
        &deps.cancel_token,
        deps.shell_snapshot.as_deref(),
        &deps.extra_env,
    )
    .await
}


mod exec;
use exec::*;
#[cfg(test)]
mod tests;
