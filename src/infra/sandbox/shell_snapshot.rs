//! 会话级 shell 环境快照 —— 一次性捕获用户交互式 shell 环境（PATH、VIRTUAL_ENV 等），
//! 供沙箱内每条命令 `source`，避免每条命令重复探测环境、以及 venv 未激活导致找不到解释器。
//!
//! 设计参考 codex `shell_snapshot`（codex-rs/core/src/shell_snapshot.rs），但做减法：
//! - 只捕获**导出的环境变量**（`env -0`），不捕获函数/alias/shellopts。后者 sourcing 进沙箱
//!   有任意代码执行风险，且非本需求（venv/PATH）所需。
//! - 捕获输出**统一重建为 POSIX `export NAME='value'`**，与 source 时的 shell（sh）解耦：
//!   bash 的 `declare -x` / `$'...'` 不能被 dash/sh source，故不直接透传 `export -p`。
//!
//! 快照文件存 `{data_dir}/shell_snapshots/{session_id}.sh`（节点本地，不进 workspace tar，
//! 语义上 shell 环境属于节点而非工作区数据）。会话删除时由 `delete_session` 清理。
//!
//! ## 与沙箱环境变量的协调
//!
//! 沙箱在 `shell_sandbox` 里把 `TMPDIR`/`XDG_*`/`HOME` 重定向到 workspace 可写子目录。快照里若
//! 带回这些变量的宿主值（只读路径），source 后会覆盖重定向。故捕获时即剔除（`EXCLUDED_VARS`）：
//! source 快照只补充 PATH/venv 等，受管变量仍由进程环境注入、不被覆盖。非沙箱路径不受影响：
//! HOME 本就在 ENV_WHITELIST 中由宿主环境直接透传。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use tokio::process::Command;

/// 捕获 shell 的最长等待。`.bashrc` 卡在 `read` 等交互输入时（stdin 已置 null）由此时限兜底。
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

/// 快照文件保留期：超过则在建快照时顺手清理（best-effort，防目录无限增长）。
const SNAPSHOT_RETENTION: Duration = Duration::from_secs(60 * 60 * 24 * 7); // 7 天

const SNAPSHOT_DIR_NAME: &str = "shell_snapshots";

/// 受沙箱管理的环境变量：source 快照时不能让它们覆盖沙箱重定向（TMPDIR/XDG_*/HOME →
/// workspace 可写子目录）；PWD/OLDPWD 影响工作目录；SHLVL/_/PS* 是交互 shell 噪声。捕获时即剔除。
const EXCLUDED_VARS: &[&str] = &[
    "HOME",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "PS1",
    "PS2",
    "PS3",
    "PS4",
];

/// 快照目录：`{data_dir}/shell_snapshots`
fn snapshot_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(SNAPSHOT_DIR_NAME)
}

/// 快照文件路径。`session_id` 经 `is_safe_path_segment` 校验（拒 `/`、`\`、`..`、`:`、空格等，
/// 防路径穿越），不安全则返回 None。
pub fn snapshot_path(data_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if !crate::config::is_safe_path_segment(session_id) {
        tracing::warn!("[shell_snapshot] 不安全 session_id，拒绝: {session_id}");
        return None;
    }
    Some(snapshot_dir(data_dir).join(format!("{session_id}.sh")))
}

/// 构建会话 shell 快照：spawn 交互式 shell（自动 source rc），用 `env -0` 捕获导出变量，
/// 重建为 POSIX `export` 行并剔除受管变量，原子写入文件。返回**规范化路径**（与沙箱
/// `ro-bind` 的 canonicalize 对齐，确保 source 路径与挂载路径一致）。任一步失败返回 None
/// （优雅降级：无快照时命令按原有白名单环境执行，不报错）。
pub async fn build(data_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let path = snapshot_path(data_dir, session_id)?;
    let dir = snapshot_dir(data_dir);

    // 快照复用：同一 session 多轮请求共享快照文件，避免每请求都 spawn bash（捕获开销 +
    // bash 子进程可能被 SIGTSTP 停止、拖累父进程组的问题）。快照内容（PATH/venv 等）
    // 在 session 生命周期内基本不变；用户改了 rc 想生效，重启 session 即可。
    // 若文件存在且未过期（<1 小时），直接返回路径。
    if let Ok(meta) = tokio::fs::metadata(&path).await {
        let fresh = meta
            .modified()
            .ok()
            .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
            .is_some_and(|age| age < Duration::from_secs(3600));
        if fresh {
            tracing::debug!("[shell_snapshot] 复用已有快照: {}", path.display());
            return Some(path);
        }
    }

    if tokio::fs::create_dir_all(&dir).await.is_err() {
        tracing::warn!("[shell_snapshot] 创建快照目录失败: {}", dir.display());
        return None;
    }

    // 顺手清理过期快照（best-effort，失败不影响主流程）。
    let cleanup_dir = dir.clone();
    tokio::spawn(async move {
        let _ = cleanup_stale(&cleanup_dir).await;
    });

    let raw = match tokio::time::timeout(SNAPSHOT_TIMEOUT, capture_env()).await {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            tracing::warn!("[shell_snapshot] 捕获 shell 环境失败（无可用 shell 或 rc 报错）");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                "[shell_snapshot] 捕获 shell 环境超时（>{:?}），跳过快照",
                SNAPSHOT_TIMEOUT
            );
            return None;
        }
    };

    let filtered = reconstruct_exports(&raw);

    // 内容验证（对齐 codex validate_snapshot）：确保重建结果包含 header 和至少一行 export。
    // 不 spawn shell 验证（避免再次引入 SIGTSTP 风险），纯内容检查足以拦截空文件/损坏/全量被剔除。
    if !validate_snapshot_content(&filtered) {
        tracing::warn!(
            "[shell_snapshot] 快照内容验证失败（无有效 export 行），跳过: {}",
            path.display()
        );
        return None;
    }

    // 原子写：先 .tmp 再 rename，避免并发命令 source 到半截文件。
    let tmp = path.with_extension("sh.tmp");
    if tokio::fs::write(&tmp, filtered.as_bytes()).await.is_err() {
        tracing::warn!("[shell_snapshot] 写临时文件失败: {}", tmp.display());
        return None;
    }
    // 收紧权限：快照含完整环境变量，可能有 token/密钥（用户 rc 里 export 的）。
    // 默认 0o644 会让同机其他用户读到；改 0o600（仅属主）。沙箱命令以同一运行用户执行，仍可读。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
    }
    if tokio::fs::rename(&tmp, &path).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        tracing::warn!("[shell_snapshot] rename 失败: {}", path.display());
        return None;
    }
    tracing::info!(
        "[shell_snapshot] 已构建会话快照: {} ({} 字节)",
        path.display(),
        filtered.len()
    );

    // 规范化：与 adk-sandbox canonicalize_paths 对齐，避免 source 路径走符号链接而 ro-bind
    // 落在 canonical 导致沙箱内找不到文件。canonicalize 失败则退回原路径（path 此处 move）。
    match tokio::fs::canonicalize(&path).await {
        Ok(canonical) => Some(canonical),
        Err(_) => Some(path),
    }
}

/// 捕获 shell 环境的命令前缀：交互式启动让 `.bashrc`/`.zshrc` 完整执行（绕过其中的
/// `[[ $- != *i* ]] && return` 早退守卫），再 `env -0` 导出 null 分隔的 NAME=value 记录。
///
/// 优先用 `$SHELL`（zsh 用户走 .zshrc），否则 bash（Kylin/WSL 通用），最后 sh 兜底。
/// 任一 shell 成功即返回其 stdout。
async fn capture_env() -> Option<String> {
    let mut shells: Vec<String> = Vec::new();
    if let Ok(sh) = std::env::var("SHELL")
        && sh.ends_with("/zsh")
    {
        shells.push(sh);
    }
    shells.push("bash".to_string());
    shells.push("sh".to_string());

    for shell in &shells {
        match capture_with(shell).await {
            Some(out) => return Some(out),
            None => continue,
        }
    }
    None
}

/// 分离子进程的控制终端，防止父进程组收到 SIGTSTP 时 bash 子进程也被停止。
///
/// 在 `pre_exec` 中调用（fork 之后、exec 之前）：
/// - `setsid()` 创建新 session，子进程脱离父进程的 TTY（对齐 codex `detach_from_tty`）。
/// - 若 `setsid` 失败（已是 session leader，返回 EPERM），回退到 `setpgid(0, 0)` 创建独立进程组。
///
/// 不 detach 时：终端发 SIGTSTP（Ctrl+Z 或终端模拟器误触发）→ 父进程组全停（cortex-agent + bash）。
/// detach 后：bash 有独立 session/group，终端信号不传播，父进程正常继续。
#[cfg(unix)]
fn detach_from_tty() -> std::io::Result<()> {
    let result = unsafe { libc::setsid() };
    if result == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            // 已是 session leader，回退到独立进程组
            if unsafe { libc::setpgid(0, 0) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

/// 单个 shell 的捕获尝试。**无独立超时**——由 `build()` 的 `tokio::time::timeout` 统一兜底，
/// `kill_on_drop` 确保外层超时 drop 本 future 时杀掉子进程（否则 `.bashrc` 卡 `read` 会挂死）。
///
/// 判定成功用「stdout 非空」而非退出码：`.bashrc` 末尾报错会让 bash 非零退出，但 `env -0`
/// 往往已产出有效输出；按退出码判定会丢弃它并无谓回退到 `sh`。非 `NAME=value` 的噪声行
/// （提示符等）由 `reconstruct_exports` 过滤。
async fn capture_with(shell: &str) -> Option<String> {
    let mut cmd = Command::new(shell);
    cmd.arg("-ic")
        .arg("env -0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    // 脱离控制终端：防止终端 SIGTSTP 波及父进程组（对齐 codex shell_snapshot.rs pre_exec）。
    // SAFETY: pre_exec 闭包在 fork 之后、exec 之前运行，setsid/setpgid 不触碰共享状态。
    #[cfg(unix)]
    #[allow(unused_imports)] // CommandExt 在 cfg(unix) 下用于 pre_exec，编译器误报 unused
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(detach_from_tty);
    }

    match cmd.output().await {
        Ok(output) if !output.stdout.is_empty() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => None,
    }
}

/// 把 `env -0` 的原始输出重建为 POSIX 安全、可被任意 `sh` source 的 `export NAME='value'` 行。
///
/// - 优先按 `\0` 切分（`env -0`）；若输出不含 `\0`（老版本 `env` 忽略 `-0` 走换行），退回按行切分
///   （此时多行值会错位，但单行简单值——PATH/VIRTUAL_ENV 等绝大多数——仍正确）。
/// - 剔除受管变量（`EXCLUDED_VARS`）与非法变量名。
/// - 值用单引号包裹，内部单引号转义为 `'\''`（POSIX 通用），天然容纳换行/空格/特殊字符。
fn reconstruct_exports(raw: &str) -> String {
    let excluded: std::collections::HashSet<&str> = EXCLUDED_VARS.iter().copied().collect();

    let records: Vec<&str> = if raw.contains('\0') {
        raw.split('\0').collect()
    } else {
        raw.lines().collect()
    };

    let mut out = String::with_capacity(raw.len());
    out.push_str("# cortex shell snapshot — POSIX export lines, sourceable by sh\n");
    for rec in records {
        let Some((name, value)) = rec.split_once('=') else {
            continue; // 无 `=`（交互 shell 混入的提示符/空行）跳过
        };
        if !is_valid_env_name(name) || excluded.contains(name) {
            continue;
        }
        // export NAME='value'，内部单引号转义。单引号串内换行原样保留，POSIX sh 正确处理。
        let escaped = value.replace('\'', "'\\''");
        out.push_str("export ");
        out.push_str(name);
        out.push_str("='");
        out.push_str(&escaped);
        out.push_str("'\n");
    }
    out
}

/// 合法环境变量名：字母/下划线开头，后随字母/数字/下划线。
fn is_valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 验证快照内容（对齐 codex `validate_snapshot`）：纯内容检查，不 spawn shell（避免 SIGTSTP 风险）。
/// 检查：① 有 header 标记（reconstruct_exports 生成的第一行）② 至少一行 `export` 声明。
/// 任一不满足则内容不可靠（空文件/损坏/所有变量被剔除），应跳过。
fn validate_snapshot_content(content: &str) -> bool {
    let has_header = content.starts_with("# cortex shell snapshot");
    let has_export = content.lines().any(|l| l.starts_with("export "));
    has_header && has_export
}

/// 构造 `source` 前缀：`. '<path>' 2>/dev/null; `。失败（文件不可读/语法错）静默，不阻断命令。
/// 返回空串表示无快照（调用方据此跳过拼接）。
pub fn source_prefix(snapshot: Option<&Path>) -> String {
    match snapshot {
        Some(p) => {
            let quoted = shell_single_quote(&p.to_string_lossy());
            format!(". {quoted} 2>/dev/null; ")
        }
        None => String::new(),
    }
}

/// 单引号包裹一个字符串用于 shell：内部 `'` → `'\''`。路径受 is_safe_path_segment 约束，
/// 一般无单引号；转义仅作防御。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 删除会话快照文件（best-effort；不存在不算错）。由 `delete_session` 调用。
pub async fn delete(data_dir: &Path, session_id: &str) {
    let Some(path) = snapshot_path(data_dir, session_id) else {
        return;
    };
    match tokio::fs::remove_file(&path).await {
        Ok(_) => tracing::info!("[shell_snapshot] 已删除会话快照: {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("[shell_snapshot] 删除快照失败（可忽略）: {e}"),
    }
}

/// 清理超过保留期的快照文件（best-effort）。
async fn cleanup_stale(dir: &Path) -> std::io::Result<()> {
    let cutoff = SystemTime::now()
        .checked_sub(SNAPSHOT_RETENTION)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if mtime < cutoff {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstruct_handles_env0_output() {
        let raw = "PATH=/usr/bin:/home/u/venv/bin\0VIRTUAL_ENV=/home/u/venv\0HOME=/home/u\0PWD=/home/u\0SHLVL=2\0_=/usr/bin/env\0";
        let out = reconstruct_exports(raw);
        assert!(out.contains("export PATH='/usr/bin:/home/u/venv/bin'"));
        assert!(out.contains("export VIRTUAL_ENV='/home/u/venv'"));
        // 受管/噪声变量被剔除（HOME 受沙箱重定向管理，同 TMPDIR/XDG_* 一并剔除）
        assert!(!out.contains("export HOME"));
        assert!(!out.contains("export PWD"));
        assert!(!out.contains("export SHLVL"));
        assert!(!out.contains("export _="));
    }

    #[test]
    fn reconstruct_escapes_single_quotes_and_newlines() {
        // env -0 的值可含换行；单引号串内换行原样保留，内部单引号转义。
        let raw = "FOO=ab'c\nXY\0";
        let out = reconstruct_exports(raw);
        assert!(out.contains("export FOO='ab'\\''c\nXY'"), "got: {out}");
    }

    #[test]
    fn reconstruct_fallback_to_newline_split_when_no_null() {
        let raw = "PATH=/usr/bin\nVIRTUAL_ENV=/v\n"; // 老 env 不支持 -0
        let out = reconstruct_exports(raw);
        assert!(out.contains("export PATH='/usr/bin'"));
        assert!(out.contains("export VIRTUAL_ENV='/v'"));
    }

    #[test]
    fn reconstruct_skips_invalid_names_and_prompt_noise() {
        let raw = "bash-5.1$ \x01BAD=x\0GOOD=1\0";
        let out = reconstruct_exports(raw);
        assert!(out.contains("export GOOD='1'"));
        assert!(!out.contains("1BAD"));
        assert!(!out.contains("bash-5.1"));
    }

    #[test]
    fn reconstruct_strips_xdg_and_tmpdir() {
        let raw = "TMPDIR=/tmp\0XDG_RUNTIME_DIR=/run/user/0\0XDG_CONFIG_HOME=/x\0PATH=/p\0";
        let out = reconstruct_exports(raw);
        assert!(out.contains("export PATH='/p'"));
        assert!(!out.contains("TMPDIR"));
        assert!(!out.contains("XDG_RUNTIME_DIR"));
        assert!(!out.contains("XDG_CONFIG_HOME"));
    }

    #[test]
    fn source_prefix_quotes_path() {
        let p = Path::new("/data/snap/abc.sh");
        assert_eq!(
            source_prefix(Some(p)),
            ". '/data/snap/abc.sh' 2>/dev/null; "
        );
        assert_eq!(source_prefix(None), "");
    }

    #[test]
    fn source_prefix_escapes_embedded_quote() {
        let p = Path::new("/a/b'c.sh");
        // 内部单引号转义为 '\''
        assert_eq!(source_prefix(Some(p)), ". '/a/b'\\''c.sh' 2>/dev/null; ");
    }

    #[test]
    fn snapshot_path_rejects_unsafe_session_id() {
        let d = Path::new("/data");
        assert!(snapshot_path(d, "abc_123-1").is_some());
        // 路径穿越 / 特殊字符被拒
        assert!(snapshot_path(d, "../etc").is_none());
        assert!(snapshot_path(d, "a/b").is_none());
        assert!(snapshot_path(d, "a:b").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn build_end_to_end_with_real_shell() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        // 真实捕获：sh 一定存在，至少能拿到 PATH。用 std temp 目录作 data_dir（避免引入 tempfile 依赖）。
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let data_dir =
            std::env::temp_dir().join(format!("cortex-snap-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&data_dir).unwrap();
        let sid = "test_session_1";
        let path = build(&data_dir, sid).await;
        // bash/sh -ic 'env -0' 在多数环境可成功；若环境极简拿不到也允许 None（优雅降级）。
        if let Some(p) = path {
            assert!(p.exists(), "快照文件应存在");
            let content = std::fs::read_to_string(&p).unwrap();
            assert!(content.contains("export "), "应含 export 行: {content}");
            // PATH 一般存在且未被剔除
            assert!(content.contains("PATH="), "应捕获 PATH: {content}");
            // 受管变量不应泄漏
            assert!(!content.contains("TMPDIR="), "TMPDIR 应被剔除: {content}");
            // 权限应收紧到 0o600（含密钥的 env 不应组/其他可读）
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode();
                assert_eq!(
                    mode & 0o777,
                    0o600,
                    "快照文件权限应为 0o600，实际 {mode:#o}"
                );
            }
        }
        // 清理
        delete(&data_dir, sid).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn validate_accepts_good_content() {
        let good = "# cortex shell snapshot — POSIX export lines, sourceable by sh\nexport PATH='/usr/bin'\nexport HOME='/root'\n";
        assert!(validate_snapshot_content(good));
    }

    #[test]
    fn validate_rejects_empty_content() {
        assert!(!validate_snapshot_content(""));
    }

    #[test]
    fn validate_rejects_header_only() {
        let header_only = "# cortex shell snapshot — POSIX export lines, sourceable by sh\n";
        assert!(!validate_snapshot_content(header_only));
    }

    #[test]
    fn validate_rejects_no_header() {
        let no_header = "export PATH='/usr/bin'\n";
        assert!(!validate_snapshot_content(no_header));
    }

    #[test]
    fn validate_rejects_all_excluded() {
        // 所有变量被剔除后只剩 header，无 export 行
        let all_excluded = "# cortex shell snapshot — POSIX export lines, sourceable by sh\n";
        assert!(!validate_snapshot_content(all_excluded));
    }
}
