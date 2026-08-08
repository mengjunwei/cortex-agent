//! shell 命令的 OS 级强制沙箱（Linux: bubblewrap / macOS: seatbelt）。
//!
//! 仅在 Linux/macOS 编译。按 [`SandboxMode`] 构建 [`SandboxPolicy`] 真实强制文件系统/网络约束。
//! Windows 上 adk-sandbox 的 enforcer 是空壳，shell 执行降级为策略层（见
//! `shell_command::execute_command` 的 cfg 分支），真强制待 D 阶段。
//!
//! ⚠️ 本模块代码在 Windows 上不编译（cfg 排除）。adk-sandbox API 用法 + bwrap/seatbelt
//!    policy 完备性需在 Linux/macOS 上 `cargo check` + 实跑验证迭代。
//!
//! ## 安全边界（不做资源限制）
//!
//! adk-sandbox 的 `LinuxEnforcer` 只用 bwrap 的 namespace（文件系统 bind / 网络 / PID），
//! **不带 cgroup / seccomp**；`ExecRequest.memory_limit_mb` 仅 `WasmBackend` 强制、进程后端忽略。
//! 故本沙箱只做"安全边界"（防乱写/删系统文件、防外泄），**不限内存/CPU**（防失控需另加 cgroup 层）。
//!
//! ## policy 规则
//!
//! - 系统必需路径只读（`/usr /bin /lib* /etc /dev /proc /sys /tmp /sbin /opt`）；砍 `/var`
//!   （服务数据）与整个 `/home`（改用 `$HOME` 精确暴露）
//! - 当前用户 `$HOME` 只读（cargo/npm/python 依赖 `~/.cargo` 等 cache）
//! - session 工作目录：read-only 模式只读，其余读写（**唯一可写处**）
//! - `session/.git` 只读覆盖（workspace-write 下，靠 bwrap 后挂载覆盖语义）
//! - 截图目录 / skill 目录只读（`readonly_extra`，存在才加）
//! - 网络：bwrap 只能全开/全断；开是为了装包，curl/wget 等外联工具由 `safety` 层拦死
//! - TMPDIR 引导到 session 内（`.cortex-tmp`），临时文件不落 host `/tmp`

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::time::Duration;

use adk_sandbox::sandbox::{SandboxPolicyBuilder, get_enforcer};
use adk_sandbox::{ExecRequest, Language, ProcessBackend, ProcessConfig, SandboxBackend};

use crate::domain::permissions::{PermissionPolicy, SandboxMode};

/// 在 OS 强制沙箱内执行 shell 命令。
///
/// `readonly_extra` 是额外只读可见路径（截图目录、skill 目录等），函数内会 `exists()` 检查，
/// 不存在的跳过——避免 adk-sandbox `canonicalize` 不存在路径报 `PolicyViolation` 导致命令全废。
///
/// 返回 `Some((exit_code, combined_output, elapsed_ms))`。enforcer 不可用（如未装 bwrap）或
/// 执行失败时返回 `None`，调用方 fail-closed（拒绝执行，不静默降级到非沙箱）。
///
/// TODO(Linux 验证):
/// - `.git` 保护依赖 bwrap 后挂载覆盖语义 + `allowed_paths` 顺序（session 在前、.git 在后），
///   adk-sandbox 无文档保证，需实跑验证 `.git` 确实只读、其余 session 目录仍可写。
/// - 系统只读路径清单可能要按发行版/工具链补全。
pub async fn execute_sandboxed(
    cmd: &str,
    workspace: &Path,
    readonly_extra: &[&Path],
    snapshot: Option<&Path>,
    policy: PermissionPolicy,
    timeout: Duration,
) -> Option<(i32, String, u128)> {
    let enforcer = match get_enforcer() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("[shell_sandbox] enforcer 不可用，返回 None 由调用方 fail-closed 拒绝: {e}");
            return None;
        }
    };

    let mut builder = SandboxPolicyBuilder::new();
    // 0. 整个宿主根只读 bind —— 保证沙箱内有可用的 /bin/sh、动态 loader、库、/proc 等。
    //    此前只逐路径 bind(/usr /bin /lib...),但 adk-sandbox 的 canonicalize 会把
    //    /bin→/usr/bin,沙箱根里没有 /bin,bwrap execvp sh 报 "No such file or directory"。
    //    整根 ro 已实测可行:sh 能跑 + workspace 可写(rw 叠加在 ro 根上) + 工作区外不可写
    //    (Read-only file system)。下方 /usr /lib 等 bind 随之冗余但无害。
    //    已知折中(F7):整根 ro 让沙箱命令可读整盘(含 /etc/shadow 等);但写入仍只限 session
    //    workspace,且 agent 进程本就以运行用户身份能读这些 —— 安全性显著优于 DangerFullAccess
    //    (后者可随意写)。逐路径方案因 adk-sandbox 缺 proc/dev/symlink 重建而不可用。
    builder = builder.allow_read(std::path::Path::new("/"));
    // 1. 系统必需路径只读（让 sh / 命令 / 库 / temp 可访问）。exists 过滤：adk-sandbox
    //    canonicalize 不存在路径会 PolicyViolation 导致全部命令 fail-closed，而 /lib32、
    //    /lib64 在非 multilib 发行版、/opt 在精简系统可能不存在——必须按实际存在性过滤。
    //    （exists() 对 symlink 返回 true，不会误删 /bin → /usr/bin 这类 symlink 路径。）
    for sys in system_readonly_paths() {
        let p = std::path::Path::new(sys);
        if p.exists() {
            builder = builder.allow_read(p);
        }
    }
    // 2. 用户 $HOME 下的包管理器/工具链 cache 只读（精确暴露，不 bind 整个 $HOME）。
    //    安全：整目录 bind $HOME 会暴露 ~/.aws/.ssh/.kube/.env 等凭证——配合开网即可外传。
    //    仅暴露 cargo/npm/pip/rustup 等 cache 子目录（对齐 codex 默认不挂 $HOME 的安全姿态）。
    //    命令装包/编译依赖这些 cache；缺失的子目录 exists 跳过（避免 canonicalize 失败）。
    if let Ok(home) = std::env::var("HOME") {
        let home_path = std::path::Path::new(&home);
        for sub in home_cache_subdirs() {
            let p = home_path.join(sub);
            if p.exists() {
                builder = builder.allow_read(p.as_path());
            }
        }
    }
    // 3. session 工作目录：read-only 模式只读，其余读写（唯一可写处）。
    match policy.sandbox_mode {
        SandboxMode::ReadOnly => {
            builder = builder.allow_read(workspace);
        }
        SandboxMode::WorkspaceWrite | SandboxMode::DangerFullAccess => {
            builder = builder.allow_read_write(workspace);
            // 3b. .git 保护：session 可写但 session/.git 只读。顺序在 workspace 之后，
            //     靠 bwrap 后挂载覆盖；.git 不存在则跳过（否则 canonicalize 失败）。
            let git_dir = workspace.join(".git");
            if git_dir.exists() {
                builder = builder.allow_read(git_dir.as_path());
            }
        }
    }
    // 4. 额外只读路径（截图目录、skill 目录）：存在才加。
    for p in readonly_extra {
        if p.exists() {
            builder = builder.allow_read(*p);
        }
    }
    // 5. 网络：bwrap 只能全开/全断（Linux 无域名级）。默认关网（config network_access=false）；
    //    显式开启（装包等）才 allow_network。curl/wget 等外联命令另由 safety 层命令名拦截 +
    //    用户审批兜底（非网络层强隔离）。
    if policy.network_access {
        builder = builder.allow_network();
    }
    // 6. sh -c 需要 fork/exec
    builder = builder.allow_process_spawn();
    let sandbox_policy = builder.build();

    let backend = ProcessBackend::with_sandbox(ProcessConfig::default(), enforcer, sandbox_policy);

    // ExecRequest 无 cwd 字段：在 code 里 cd 到 workspace（路径用单引号包裹防空格）。
    // 先 source 会话 shell 快照（PATH/venv，只读挂载于 readonly_extra），失败静默不阻断；
    // 受管变量（TMPDIR/XDG_*）已在捕获时剔除，不会覆盖下方的 workspace 重定向。
    let source_prefix = crate::infra::shell_snapshot::source_prefix(snapshot);
    let code = format!("{source_prefix}cd '{}' && {}", workspace.display(), cmd);
    // 注入白名单环境变量（与非沙箱 execute_command 一致）。adk-sandbox 会清空继承环境，
    // 不注入则沙箱内无 PATH/HOME，命令会 "command not found"。
    let mut env = std::collections::HashMap::new();
    for key in crate::tools::shell_command::ENV_WHITELIST {
        if let Ok(val) = std::env::var(key) {
            env.insert((*key).to_string(), val);
        }
    }
    // TMPDIR + XDG 引导到 session 内（仅可写模式）：宿主 /tmp、$HOME 及宿主 XDG 在沙箱内
    // 均只读（被 ro-bind / 带入）。pip/npm/cargo 尊重 TMPDIR；LibreOffice/Qt/Chromium 等据
    // XDG 目录定位 runtime/config/cache（LibreOffice 单实例管道路径派生自 XDG_RUNTIME_DIR），
    // 缺可写 XDG 会报 "no valid pipe path found"。统一重定向到 session/.cortex-tmp 下的子目录：
    // ① 集中在既有临时区，不新增 workspace 顶层目录；② 语义正确（XDG_RUNTIME_DIR 本就该是
    // tmpfs/ephemeral）。插在白名单透传之后，覆盖只读的宿主值。ReadOnly 模式下 workspace 整体
    // 只读，临时文件本就不该写，跳过注入（保留宿主值）。
    if !matches!(policy.sandbox_mode, SandboxMode::ReadOnly) {
        let session_tmp = workspace.join(".cortex-tmp");
        if std::fs::create_dir_all(&session_tmp).is_ok() {
            env.insert(
                "TMPDIR".to_string(),
                session_tmp.to_string_lossy().into_owned(),
            );
            for (key, sub) in [
                ("XDG_RUNTIME_DIR", "xdg-runtime"),
                ("XDG_CONFIG_HOME", "xdg-config"),
                ("XDG_DATA_HOME", "xdg-data"),
                ("XDG_CACHE_HOME", "xdg-cache"),
            ] {
                let dir = session_tmp.join(sub);
                if std::fs::create_dir_all(&dir).is_ok() {
                    env.insert(key.to_string(), dir.to_string_lossy().into_owned());
                }
            }
        }
    }
    let request = ExecRequest {
        language: Language::Command,
        code,
        stdin: None,
        timeout,
        memory_limit_mb: None,
        env,
    };

    match backend.execute(request).await {
        Ok(result) => {
            let mut combined = result.stdout;
            if !result.stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&result.stderr);
            }
            Some((result.exit_code, combined, result.duration.as_millis()))
        }
        Err(e) => {
            tracing::error!("[shell_sandbox] 沙箱执行失败: {e}");
            None
        }
    }
}

/// 命令执行所需的系统只读路径（让 sh / 常用命令 / 库 / temp 可访问）。
///
/// 砍掉 `/var`（服务数据：postgres/redis/docker 等，命令一般不读）与整个 `/home`
/// （改用 `$HOME` 精确暴露，见 [`execute_sandboxed`]，避免泄露其他用户文件）。
fn system_readonly_paths() -> &'static [&'static str] {
    &[
        "/usr", "/bin", "/lib", "/lib64", "/lib32", "/etc", "/dev", "/proc", "/sys", "/tmp",
        "/sbin", "/opt",
    ]
}

/// 用户 `$HOME` 下需暴露给沙箱的 cache/工具链子目录（只读）。
///
/// 仅暴露包管理器/编译器 cache，**不 bind 整个 `$HOME`**——避免泄露 `~/.aws`/`~/.ssh`/
/// `~/.kube`/`.env` 等凭证（对齐 codex 默认不挂 `$HOME` 的安全姿态）。命令装包/编译依赖
/// 这些 cache；调用方对每个子目录 `exists()` 过滤后再 `allow_read`，缺失的跳过。
fn home_cache_subdirs() -> &'static [&'static str] {
    &[
        ".cargo", // Rust crates registry / git
        ".rustup", // Rust 工具链
        ".npm",   // npm cache
        ".cache/pip", // pip wheel cache
        ".cache/uv",  // uv cache
        ".cache/go-build", // Go 构建 cache
        "go/pkg/mod", // Go 模块 cache（默认 GOPATH=~/go）
        ".m2",    // Maven
        ".gradle", // Gradle
        ".pyenv", // pyenv 版本
    ]
}
