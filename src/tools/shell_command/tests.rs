//! shell_command 测试 — 从 mod.rs 拆出。

use super::*;
use crate::permissions::{ApprovalPolicy, PermissionPolicy, SandboxMode};
use crate::tools::code::tests_helpers::TmpWs;

#[tokio::test]
async fn runs_echo_successfully() {
    let ws = TmpWs::new();
    let root = ws.canon();
    let r = execute_command(
        &root,
        "echo hello",
        5000,
        &CancellationToken::new(),
        None,
        &[],
    )
    .await;
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
    let r = execute_command(&root, cmd, 5000, &CancellationToken::new(), None, &[]).await;
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
    let r = execute_command(&root, cmd, 500, &CancellationToken::new(), None, &[]).await;
    assert_eq!(r["ok"], false, "应超时失败: {:?}", r);
    assert_eq!(r["timed_out"], true);
}

#[tokio::test]
async fn execute_empty_command_fails() {
    let ws = TmpWs::new();
    let root = ws.canon();
    let r = execute_command(&root, "   ", 5000, &CancellationToken::new(), None, &[]).await;
    assert_eq!(r["ok"], false);
}

#[test]
fn truncate_str_keeps_short() {
    assert_eq!(truncate_str("short", 100), "short");
}

#[test]
fn sandbox_denial_hint_triggers_on_erofs_markers() {
    let cwd = std::path::Path::new("/ws");
    let ws_write = SandboxMode::WorkspaceWrite;
    for marker in [
        "cp: cannot create file '/root/x': Read-only file system",
        "soffice: no valid pipe path found",
        "sqlite3: Attempt to write a readonly database",
    ] {
        let hint = sandbox_denial_hint(1, marker, cwd, &ws_write);
        assert!(hint.starts_with("\n---"), "应命中特征: {marker}");
        assert!(hint.contains("/ws"), "hint 应包含 cwd");
        assert!(hint.contains("$TMPDIR"));
    }
}

#[test]
fn sandbox_denial_hint_readonly_points_to_mode_switch_not_tmpdir() {
    // ReadOnly：无任何可写位置，hint 不能指引写 $TMPDIR（那里也没重定向、照样 EROFS）
    let hint = sandbox_denial_hint(
        1,
        "cp: cannot create file: Read-only file system",
        std::path::Path::new("/ws"),
        &SandboxMode::ReadOnly,
    );
    assert!(hint.contains("READ-ONLY mode"));
    assert!(
        !hint.contains("$TMPDIR"),
        "ReadOnly 下不应指引写 $TMPDIR: {hint}"
    );
    assert!(hint.contains("workspace-write"));
}

#[test]
fn sandbox_denial_hint_silent_on_success_or_unrelated_failure() {
    let cwd = std::path::Path::new("/ws");
    let ws_write = SandboxMode::WorkspaceWrite;
    // 成功退出：即使输出含特征串也不追加
    assert_eq!(
        sandbox_denial_hint(0, "Read-only file system", cwd, &ws_write),
        ""
    );
    // 普通失败（无沙箱特征）：不追加，避免误报污染
    assert_eq!(
        sandbox_denial_hint(1, "Permission denied", cwd, &ws_write),
        ""
    );
    assert_eq!(
        sandbox_denial_hint(1, "command not found", cwd, &ws_write),
        ""
    );
}

// 可见性描述如实化（v1.17.1 整盘只读姿态）：模型据此直接用 $VIRTUAL_ENV、
// 不再满盘 find / 找 venv。改挂载姿态时此测试同步改。
#[test]
fn sandbox_visibility_note_states_venv_credentials_and_persistence() {
    assert!(SANDBOX_VISIBILITY_NOTE.contains("$VIRTUAL_ENV/bin/python"));
    assert!(SANDBOX_VISIBILITY_NOTE.contains("~/.ssh"));
    assert!(SANDBOX_VISIBILITY_NOTE.contains("/etc/ssh"));
    assert!(SANDBOX_VISIBILITY_NOTE.contains("readable read-only"));
    assert!(SANDBOX_VISIBILITY_NOTE.contains("session-persistent"));
    // "Everything else is read-only" 在整盘只读下必须成立——不许再出现
    // "not mounted"/缺失路径的旧描述
    assert!(!SANDBOX_VISIBILITY_NOTE.contains("not mounted"));
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

#[test]
fn workspace_write_auto_executes() {
    // 无人值守定时任务：auto 档直接放行需审批命令（仍受 dangerous 硬编码阻断兜底）。
    let p = PermissionPolicy::new(SandboxMode::WorkspaceWrite, ApprovalPolicy::Auto, false);
    assert!(matches!(decide_with_policy(&p), PromptDecision::Execute));
}

#[test]
fn approval_policy_auto_roundtrip() {
    assert_eq!(ApprovalPolicy::Auto.codex_id(), "auto");
    assert_eq!(
        ApprovalPolicy::from_codex_id("auto"),
        Some(ApprovalPolicy::Auto)
    );
}
