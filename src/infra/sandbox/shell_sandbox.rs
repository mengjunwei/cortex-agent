//! shell 命令的 OS 级强制沙箱（Linux: bubblewrap / macOS: seatbelt）。
//!
//! 仅在 Linux/macOS 编译。按 [`SandboxMode`] 构建 [`SandboxPolicy`] 真实强制文件系统/网络约束。
//! Windows 上 adk-sandbox 的 enforcer 是空壳，shell 执行降级为策略层（见
//! `shell_command::execute_command` 的 cfg 分支），真强制待 D 阶段。
//!
//! ⚠️ 本模块代码在 Windows 上不编译（cfg 排除）。adk-sandbox API 用法 + bwrap/seatbelt
//!    policy 完备性需在 Linux/macOS 上 `cargo check` + 实跑验证迭代。
//!
//! ## 安全边界（资源限制仍不做）
//!
//! adk-sandbox 的 `LinuxEnforcer` 用 bwrap namespace（文件系统 bind / 网络 / PID）+
//! seccomp（禁网兜底、ptrace/io_uring 防护，经主二进制自嵌 `--sandbox-exec-inner`），
//! **不带 cgroup**；`ExecRequest.memory_limit_mb` 仅 `WasmBackend` 强制、进程后端忽略。
//! 故本沙箱防乱写/删系统文件、防外泄、防进程内省，**不限内存/CPU**（防失控需另加 cgroup 层）。
//!
//! ## policy 规则
//!
//! - 整盘只读根(`--ro-bind / /`,对齐 codex 默认策略姿态):root 部署下 venv/工具链
//!   可能装在宿主机任意路径,白名单永远追不完(线上事故:uv venv 换到哪个路径沙箱内
//!   都不可见)。codex 敢整盘读是因为它跑普通用户、文件权限兜底;cortex 后端常以
//!   root 跑,挂载层是唯一防线——因此**必须**叠加凭证目录 mask(见下条)
//! - 凭证目录 mask(`credential_mask_dirs`):`~/.ssh ~/.gnupg ~/.aws ~/.azure ~/.kube
//!   ~/.docker` + `/etc/ssh`,存在才 mask。目录级;文件级凭证(/etc/shadow、服务
//!   config.toml)不支持 mask,属已知残留面
//! - session 工作目录：read-only 模式只读，其余读写（**唯一可写处**；整盘只读 bind
//!   先发射、workspace 读写 bind 后发射,bwrap 后挂载生效）
//! - `session/.git` 保护:存在(非 symlink)→ 只读覆盖(可读不可写,git status/log 可用);
//!   不存在 → mask 挡创建(对齐 codex PROTECTED_METADATA 防重建语义);symlink → 不 bind
//!   (防 ro-bind src 被 canonicalize 越界)。已知限制:进程运行期"rm .git && git init"
//!   重建无 inotify 监控(codex 有),绕过窗口存在,后续增强。
//! - 截图目录 / skill 目录只读（`readonly_extra`，存在才加；整盘只读下天然可见,
//!   保留为显式清单供非整盘 enforcer 演进）
//! - 网络：bwrap netns 全开/全断 + **seccomp syscall 级兜底**(禁网时经主二进制
//!   自嵌 `cortex-agent --sandbox-exec-inner`:connect/bind/listen 等封死,socket 仅
//!   AF_UNIX;ptrace/io_uring 无条件封;单文件部署对齐 codex codex-linux-sandbox)。
//!   curl/wget 等外联工具另由 `safety` 层拦
//! - TMPDIR/XDG_*/HOME 引导到 session 内（`.cortex-tmp`），临时文件不落 host `/tmp`，
//!   依赖 `~/` 可写的工具不再撞只读根;`$HOME` 下 cache 子目录经 symlink 桥回宿主
//!   (整盘只读下目标天然可见,见 [`link_home_cache_into`])；硬编码 `/tmp` 且不读
//!   环境变量的工具（LibreOffice oosplash）由可写模式挂载的会话宿主目录
//!   `.cortex-tmp/tmp → /tmp` 承接（见 3z：跨命令持久、随会话清理、不绑宿主 /tmp）

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::time::Duration;

use adk_sandbox::sandbox::{SandboxPolicyBuilder, get_enforcer};
use adk_sandbox::{ExecRequest, Language, ProcessBackend, ProcessConfig, SandboxBackend};

use crate::permissions::{PermissionPolicy, SandboxMode};

/// [`execute_sandboxed`] 的参数集（字段数多，收进结构体防 clippy too_many_arguments）。
pub struct SandboxExec<'a> {
    /// 要执行的命令
    pub cmd: &'a str,
    /// session 工作目录（命令 cwd，也是唯一 rw 挂载点）
    pub workspace: &'a Path,
    /// 是否在新 HOME 下建 cache symlink 桥。仅当 cwd 是会话工作区（或其子目录）时传
    /// true——workdir 指到工作区外（如 skill 目录）时不往外部目录塞绝对路径 symlink
    /// （污染 skill 源码目录）；此时 HOME 仍重定向（直接指 .cortex-tmp，不另建 home
    /// 子目录），仅 cache 冷启动。
    pub bridge_home_cache: bool,
    /// 额外只读可见路径（截图目录、skill 目录等），函数内会 `exists()` 检查，不存在的
    /// 跳过——避免 adk-sandbox `canonicalize` 不存在路径报 `PolicyViolation` 导致命令全废。
    pub readonly_extra: &'a [&'a Path],
    /// mask（空 tmpfs 覆盖，内容不可见）的路径，同样 canonicalize 过滤。用于整盘只读
    /// 姿态下隐藏特定目录（如 skill 白名单外的 skill 子目录——readonly_extra 收窄在
    /// `--ro-bind / /` 面前是 no-op，只能靠 mask 覆盖）。
    pub masked_paths: &'a [&'a Path],
    /// 会话 shell 环境快照路径（None=未启用）
    pub snapshot: Option<&'a Path>,
    /// 权限策略（沙箱模式 + 审批策略 + 网络开关）
    pub policy: PermissionPolicy,
    /// 超时
    pub timeout: Duration,
    /// 助手级环境变量（白名单之后注入）
    pub extra_env: &'a [(String, String)],
}

/// 在 OS 强制沙箱内执行 shell 命令。
///
/// 返回 `Some((exit_code, combined_output, elapsed_ms))`。enforcer 不可用（如未装 bwrap）或
/// 执行失败时返回 `None`，调用方 fail-closed（拒绝执行，不静默降级到非沙箱）。
///
/// TODO(Linux 验证):
/// - `.git` 保护依赖 bwrap 后挂载覆盖语义 + `allowed_paths` 顺序（session 在前、.git 在后），
///   adk-sandbox 无文档保证，需实跑验证 `.git` 确实只读、其余 session 目录仍可写。
/// - 系统只读路径清单可能要按发行版/工具链补全。
pub async fn execute_sandboxed(
    SandboxExec {
        cmd,
        workspace,
        bridge_home_cache,
        readonly_extra,
        masked_paths,
        snapshot,
        policy,
        timeout,
        extra_env,
    }: SandboxExec<'_>,
) -> Option<(i32, String, u128)> {
    let enforcer = match get_enforcer() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                "[shell_sandbox] enforcer 不可用，返回 None 由调用方 fail-closed 拒绝: {e}"
            );
            return None;
        }
    };

    // workspace 统一 canonical 成绝对路径:bind/mask 的挂载点、shell 的 cd 目标、
    // TMPDIR/HOME 重定向、.git 保护全部派生自同一形态。data_dir 默认是相对路径
    // ("./data"),不统一的话:bwrap 挂载点拿到相对路径会直接报错(全部命令
    // fail-closed);.git mask 与 workspace bind 也可能落在两个不同形态的路径上。
    // canonicalize 失败(目录刚消失)返回 None 由调用方 fail-closed。
    let workspace_owned = match workspace.canonicalize() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("[shell_sandbox] workspace canonicalize 失败，fail-closed: {e}");
            return None;
        }
    };
    let workspace = workspace_owned.as_path();

    // readonly_extra / snapshot 同理统一 canonical:它们同样派生自 data_dir(可能相对),
    // 相对路径的 `source ./data/snapshot` 依赖 spawn 时 cwd(整盘只读下可见,但形态
    // 与挂载清单不一致仍易漂移)——统一成绝对形态,沙箱内必命中。
    // canonical 后:bind 挂载点与 source_prefix 使用的路径同源,沙箱内必命中。
    // 不存在的条目跳过(与下方 exists 过滤语义一致)。
    let readonly_extra_owned: Vec<std::path::PathBuf> = readonly_extra
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let readonly_extra: Vec<&Path> = readonly_extra_owned.iter().map(|p| p.as_path()).collect();
    // mask 目标同样 canonical（挂载点必须绝对；不存在=无需隐藏，天然跳过）。
    let masked_owned: Vec<std::path::PathBuf> = masked_paths
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let snapshot_owned = snapshot.and_then(|p| p.canonicalize().ok());
    let snapshot = snapshot_owned.as_deref();

    let mut builder = SandboxPolicyBuilder::new();
    // 0. 整盘只读(--ro-bind / /,对齐 codex 默认策略姿态):root 部署下 venv/工具链
    //    可能装在宿主机任意路径(/root/python3.13venv、/home/cortex/...、/srv/...),
    //    白名单永远追不完(线上事故:uv venv 换到哪个路径沙箱内都不可见,模型满盘
    //    find / 乱找)。整盘读让 shell 快照注入的 $VIRTUAL_ENV/PATH 天然可解析。
    //    codex 敢整盘读是因为它跑普通用户、文件权限兜底;cortex 后端常以 root 跑,
    //    挂载层是唯一防线——因此必须叠加 1z 的凭证目录 mask。workspace 的读写 bind
    //    在此之后加入,bwrap 后挂载生效,可写性不被整盘只读覆盖。
    builder = builder.allow_read(Path::new("/"));
    // 1z. 凭证目录 mask:整盘只读后 root 视角下 ~/.ssh、/etc/ssh 等全部可读,配合
    //     开网即可外传私钥。mask 把这些目录在沙箱内变成不可读的空覆盖(目录级;
    //     vendor mask 语义)。$HOME 下 cache 子目录与 .gitconfig 不 mask——
    //     link_home_cache_into 的 symlink 桥依赖其可见性。
    for p in credential_mask_dirs(std::env::var("HOME").ok().as_deref()) {
        builder = builder.mask_path(p);
    }
    // 1y. skill 白名单硬隔离 mask：整盘只读下被隐藏 skill 的目录天然全盘可见，
    //     只有 mask（空 tmpfs 覆盖）能把它们在沙箱内变成「目录存在但内容为空」。
    //     挂载清单只含白名单外的 skill 目录（调用方计算），mask 在所有 bind 之后
    //     发射（adk-sandbox 语义），后挂载生效。
    for p in &masked_owned {
        builder = builder.mask_path(p.as_path());
    }
    // 3. session 工作目录：read-only 模式只读，其余读写（唯一可写处）。
    match policy.sandbox_mode {
        SandboxMode::ReadOnly => {
            builder = builder.allow_read(workspace);
        }
        SandboxMode::WorkspaceWrite | SandboxMode::DangerFullAccess => {
            builder = builder.allow_read_write(workspace);
            // 3z. /tmp 挂成会话专属宿主目录:LibreOffice oosplash 等启动器把单实例
            //     管道路径硬编码在 /tmp 与 /var/tmp(start.cxx PIPEDEFAULTPATH),
            //     access(W_OK) 不过直接 "no valid pipe path found" 退出——TMPDIR/
            //     XDG 重定向对它们无效(不读任何环境变量)。旧方案私有 tmpfs 每次
            //     命令即焚,同会话跨命令不互通(硬编码 /tmp 的工具反复丢状态);改
            //     bind 宿主 workspace/.cortex-tmp/tmp 到 /tmp——跨命令持久、随会话
            //     工作区清理、不吃内存。**不 bind 宿主 /tmp**(codex workspace-write
            //     的做法):LibreOffice 单实例管道跨会话冲突 + 宿主其他用户的
            //     socket/文件泄漏。晚于 0 的整盘 ro-bind 发射,后挂载生效。
            let session_tmp_dir = workspace.join(".cortex-tmp").join("tmp");
            if std::fs::create_dir_all(&session_tmp_dir).is_ok() {
                builder = builder.bind_read_write_at(&session_tmp_dir, Path::new("/tmp"));
            }
            // 3b. .git 保护,对齐 codex PROTECTED_METADATA 语义:
            //     - 存在(目录/文件) → 只读覆盖(ro-bind 排在 workspace bind 之后)——
            //       模型仍可 git status/log,但不可改写
            //     - 不存在 → mask(宿主先建空目录,再 tmpfs+remount-ro 覆盖)——挡住
            //       "在可写 workspace 里新建 .git"的入口(旧实现直接跳过,git init
            //       即可建出沙箱外可见的 .git)。codex 用 synthetic mount target +
            //       inotify 实现同语义,这里用 mask 等价达成。
            //     - symlink → 不解析 bind(codex 对"保护路径穿过可写 symlink"fail-closed:
            //       ro-bind 的 src 会被 canonicalize 到 workspace 外的任意目标,沙箱内
            //       可读该目标——越界泄漏)。symlink 本体留在 workspace(可写,模型可改),
            //       但其目标不在挂载清单内,沙箱内不可见,无泄漏面。
            let git_dir = workspace.join(".git");
            match std::fs::symlink_metadata(&git_dir) {
                Ok(meta) if !meta.is_symlink() => {
                    builder = builder.allow_read(git_dir.as_path());
                }
                Ok(_) => { /* symlink:不 bind,目标不可见即无泄漏 */ }
                Err(_) => {
                    if std::fs::create_dir(&git_dir).is_ok() {
                        builder = builder.mask_path(git_dir.as_path());
                    }
                }
            }
        }
    }
    // 4. 额外只读路径（截图目录、skill 目录）：存在才加。
    for p in readonly_extra {
        if p.exists() {
            builder = builder.allow_read(p);
        }
    }
    // 5. 网络:bwrap netns 全开/全断(Linux 无域名级)。默认关网;显式开启(装包等)才
    //    allow_network。禁网时叠加 seccomp syscall 级兜底(connect/bind/listen 封死、
    //    socket 仅 AF_UNIX;ptrace/io_uring 无条件封)——对齐 codex 的双层禁网
    //    (netns + seccomp)。curl/wget 等外联命令另由 safety 层拦 + 用户审批。
    //    helper 为**主二进制自嵌**(cortex-agent --sandbox-exec-inner,对齐 codex
    //    codex-linux-sandbox 单文件两阶段):部署零额外文件。seccomp 仅 Linux,
    //    macOS enforcer 忽略该 policy 字段(Seatbelt 自带 network 过滤)。
    #[cfg(target_os = "linux")]
    if !policy.network_access {
        if let Ok(exe) = std::env::current_exe() {
            builder = builder.seccomp_restrict_network_self_embedded(
                exe,
                super::sandbox_exec::INNER_FLAG,
            );
        }
    } else {
        builder = builder.allow_network();
    }
    #[cfg(not(target_os = "linux"))]
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
    let source_prefix = super::shell_snapshot::source_prefix(snapshot);
    let code = format!("{source_prefix}cd '{}' && {}", workspace.display(), cmd);
    // 注入白名单环境变量（与非沙箱 execute_command 一致）。adk-sandbox 会清空继承环境，
    // 不注入则沙箱内无 PATH/HOME，命令会 "command not found"。
    let mut env = std::collections::HashMap::new();
    for key in crate::tools::shell_command::ENV_WHITELIST {
        if let Ok(val) = std::env::var(key) {
            env.insert((*key).to_string(), val);
        }
    }
    // 助手级环境变量：白名单之后注入（可覆盖 PATH 等，等同真实 shell 语义）。
    // 排在下方 TMPDIR/XDG 重定向之前——沙箱把临时目录钉在 session 内的安全语义不被助手变量打破。
    for (key, val) in extra_env {
        env.insert(key.clone(), val.clone());
    }
    // TMPDIR + XDG 引导到 session 内（仅可写模式）：宿主 $HOME 及宿主 XDG 在沙箱内
    // 均只读（被 ro-bind / 带入）。pip/npm/cargo 尊重 TMPDIR；Qt/Chromium 等据 XDG 目录
    // 定位 runtime/config/cache。注意:LibreOffice oosplash **不读任何环境变量**——其单
    // 实例管道路径硬编码 /tmp、/var/tmp(start.cxx PIPEDEFAULTPATH,access(W_OK) 不过即
    // "no valid pipe path found"),由 3z 的会话宿主目录(.cortex-tmp/tmp → /tmp)承接,
    // 勿再归因 XDG。
    // 统一重定向到 session/.cortex-tmp 下的子目录：
    // ① 集中在既有临时区，不新增 workspace 顶层目录；② 语义正确（XDG_RUNTIME_DIR 本就该是
    // tmpfs/ephemeral）。插在白名单透传之后，覆盖只读的宿主值。ReadOnly 模式下 workspace 整体
    // 只读，临时文件本就不该写，跳过注入（保留宿主值）。
    if !matches!(policy.sandbox_mode, SandboxMode::ReadOnly) {
        let session_tmp = workspace.join(".cortex-tmp");
        // canonicalize 成绝对路径再进 env：workspace 可能派生自相对 data_dir（默认 "./data"），
        // 而 spawn 的 shell 会先 cd 进 workspace——相对的 $HOME/$TMPDIR 在 cd 后全部悬空
        // （解析到 <workspace>/data/... 不存在，git 丢身份且报 ENOENT 而非 EROFS，hint 不触发）。
        // 建目录后 canonicalize 必然成功；极端失败回退原值（不劣于旧行为）。
        let session_tmp =
            std::fs::create_dir_all(&session_tmp).and_then(|_| session_tmp.canonicalize());
        if let Ok(session_tmp) = session_tmp {
            env.insert(
                "TMPDIR".to_string(),
                session_tmp.to_string_lossy().into_owned(),
            );
            // HOME 重定向到 session 内可写目录：宿主 HOME 在沙箱内只读（ro-bind / 带入），
            // git/npm 等往 ~/.xxx 写锁文件/配置会 EROFS。对齐 Claude Code「可写 = cwd +
            // 会话临时目录」的边界，HOME 一并落进 .cortex-tmp。cache/全局配置用 symlink
            // 桥回宿主只读挂载（cargo/npm 装包仍走宿主 cache 不冷启动、git 提交身份不丢），
            // 见 [`link_home_cache_into`]。HOME 已从 shell 快照 EXCLUDED_VARS 剔除，source
            // 不会覆盖。桥接时 HOME=.cortex-tmp/home；非桥接（cwd 在会话工作区外）HOME
            // 直接指 .cortex-tmp 本身——不为外部目录(skill 源码树等)新增 home 子目录，
            // 只复用已建的临时区。
            if bridge_home_cache {
                let session_home = session_tmp.join("home");
                if std::fs::create_dir_all(&session_home).is_ok() {
                    if let Ok(host_home) = std::env::var("HOME") {
                        link_home_cache_into(&host_home, &session_home);
                    }
                    env.insert(
                        "HOME".to_string(),
                        session_home.to_string_lossy().into_owned(),
                    );
                }
            } else {
                env.insert(
                    "HOME".to_string(),
                    session_tmp.to_string_lossy().into_owned(),
                );
            }
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

/// 整盘只读下必须 mask 的凭证目录（存在的绝对路径才返回）。
///
/// root 部署时挂载层是唯一防线，整盘读会把宿主凭证目录一并暴露给沙箱内进程
/// （配合开网即可外传私钥）。覆盖 SSH/GPG/云 CLI/k8s/docker 五类常见凭证目录。
/// 均为目录级 mask（vendor `masked_paths` 语义）；文件级凭证（/etc/shadow、服务
/// config.toml）不支持 mask，属已知残留面（见 architecture.md v1.17.1）。
/// 刻意不含 `$HOME/.cache` 等工具 cache——[`link_home_cache_into`] 的 symlink 桥
/// 依赖其可见性，cache 内也不含凭证。
fn credential_mask_dirs(home: Option<&str>) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from("/etc/ssh")];
    if let Some(home) = home.filter(|h| Path::new(h).is_absolute()) {
        for sub in [".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker"] {
            dirs.push(Path::new(home).join(sub));
        }
    }
    dirs.retain(|p| p.is_absolute() && p.exists());
    dirs
}

/// 用户 `$HOME` 下需暴露给沙箱的 cache/工具链子目录（只读）。
///
/// 仅暴露包管理器/编译器 cache，**不 bind 整个 `$HOME`**——避免泄露 `~/.aws`/`~/.ssh`/
/// `~/.kube`/`.env` 等凭证（对齐 codex 默认不挂 `$HOME` 的安全姿态）。命令装包/编译依赖
/// 这些 cache；调用方对每个子目录 `exists()` 过滤后再 `allow_read`，缺失的跳过。
fn home_cache_subdirs() -> &'static [&'static str] {
    &[
        ".cargo",          // Rust crates registry / git
        ".rustup",         // Rust 工具链
        ".npm",            // npm cache
        ".cache/pip",      // pip wheel cache
        ".cache/uv",       // uv cache
        ".cache/go-build", // Go 构建 cache
        "go/pkg/mod",      // Go 模块 cache（默认 GOPATH=~/go）
        ".m2",             // Maven
        ".gradle",         // Gradle
        ".pyenv",          // pyenv 版本
    ]
}

/// 把宿主 HOME 的 cache/全局配置子目录以 symlink 桥进沙箱 HOME（配合 HOME 重定向）。
///
/// HOME 重定向到 `.cortex-tmp/home` 后，`~/.cargo` 等不再指向宿主 cache 的只读挂载——
/// cargo/npm/pip 装包会变冷启动、git 丢提交身份（读 `~/.gitconfig`）。这里在新 HOME 下建
/// symlink 指回宿主路径（整盘只读 bind 下宿主路径天然只读可见），行为与重定向前一致：
/// - 已存在同名条目（前次命令建过）跳过，幂等；
/// - 宿主侧不存在该子目录跳过；
/// - 建链失败仅 debug 日志——缺 cache 只影响速度，不阻断命令。
///
/// ⚠️ 清单刻意与 `credential_mask_dirs` 不相交——mask 的目录若被桥进沙箱 HOME,
/// symlink 会解析到不可读的空覆盖,工具报错且难以归因。
fn link_home_cache_into(host_home: &str, session_home: &Path) {
    use std::os::unix::fs::symlink;
    let host = Path::new(host_home);
    // 除 cache 子目录外，git 全局配置也读 $HOME（重定向后丢身份会让 commit 挂掉）。
    let mut names: Vec<&str> = home_cache_subdirs().to_vec();
    names.push(".gitconfig");
    for sub in names {
        let src = host.join(sub);
        if !src.exists() {
            continue;
        }
        let dst = session_home.join(sub);
        // exists() 跟随 symlink（断链返回 false），symlink_metadata 能查到断链本身——
        // 两者都命中才算已存在，避免用目录/断链覆盖。
        if dst.exists() || std::fs::symlink_metadata(&dst).is_ok() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = symlink(&src, &dst) {
            tracing::debug!(
                "[shell_sandbox] HOME cache symlink 桥接失败 {} -> {}: {e}",
                dst.display(),
                src.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // credential_mask_dirs：整盘只读下只 mask 真实存在的凭证目录（绝对路径）。
    #[test]
    fn credential_masks_only_existing_dirs() {
        let host = std::env::temp_dir().join("cortex_sbx_test_credmask");
        let _ = std::fs::remove_dir_all(&host);
        std::fs::create_dir_all(host.join(".ssh")).unwrap();
        std::fs::create_dir_all(host.join(".kube")).unwrap();
        // .aws 不存在：不应出现
        let masks = credential_mask_dirs(Some(host.to_str().unwrap()));
        assert!(masks.contains(&host.join(".ssh")));
        assert!(masks.contains(&host.join(".kube")));
        assert!(!masks.contains(&host.join(".aws")));
        assert!(!masks.contains(&host.join(".cargo")), "cache 子目录不 mask");
        let _ = std::fs::remove_dir_all(&host);
    }

    // /etc/ssh 存在于一切常规部署；相对 HOME 形态整体跳过。
    #[test]
    fn credential_masks_always_include_etc_ssh_and_skip_relative_home() {
        let masks = credential_mask_dirs(Some("relative/home"));
        assert!(masks.contains(&std::path::PathBuf::from("/etc/ssh")));
        assert!(masks.iter().all(|p| p.is_absolute()));
        assert!(!masks.iter().any(|p| p.ends_with("relative")));

        let no_home = credential_mask_dirs(None);
        assert_eq!(no_home, vec![std::path::PathBuf::from("/etc/ssh")]);
    }

    // link_home_cache_into：HOME 重定向后把宿主 cache 子目录 symlink 桥进沙箱 HOME。
    // 幂等（重复调用不覆盖）+ 只桥存在的子目录 + .cache/pip 嵌套父目录自动建。
    #[test]
    fn links_existing_cache_subdirs_idempotently() {
        let host = std::env::temp_dir().join("cortex_sbx_test_host");
        let session = std::env::temp_dir().join("cortex_sbx_test_session");
        let _ = std::fs::remove_dir_all(&host);
        let _ = std::fs::remove_dir_all(&session);
        std::fs::create_dir_all(host.join(".cargo")).unwrap();
        std::fs::create_dir_all(host.join(".cache/pip")).unwrap();
        std::fs::write(host.join(".gitconfig"), "[user]\n").unwrap();
        // .npm 不存在：不应桥接
        std::fs::create_dir_all(&session).unwrap();

        link_home_cache_into(host.to_str().unwrap(), &session);
        // 幂等：再来一次不报错、不覆盖
        link_home_cache_into(host.to_str().unwrap(), &session);

        assert!(session.join(".cargo").is_dir());
        assert!(session.join(".cache/pip").is_dir());
        assert!(session.join(".gitconfig").is_file());
        assert!(!session.join(".npm").exists());
        // symlink 确实指回宿主源
        assert_eq!(
            std::fs::read_link(session.join(".cargo")).unwrap(),
            host.join(".cargo")
        );
        let _ = std::fs::remove_dir_all(&host);
        let _ = std::fs::remove_dir_all(&session);
    }
}
