//! Linux bubblewrap sandbox enforcer.
//!
//! Uses `bwrap` with user namespaces to create isolated filesystem and
//! process environments without requiring root privileges.
//!
//! ## How It Works
//!
//! 1. Bubblewrap arguments are generated from the [`SandboxPolicy`]
//! 2. The original command is wrapped: `bwrap <args> -- <program> <args...>`
//! 3. The kernel enforces namespace-based isolation on the child process
//!
//! ## Key bwrap Arguments
//!
//! - `--die-with-parent` — kill child when parent exits
//! - `--unshare-pid` — isolate process ID namespace
//! - `--unshare-net` — isolate network namespace (no network access)
//! - `--ro-bind <src> <dest>` — read-only filesystem bind mount
//! - `--bind <src> <dest>` — read-write filesystem bind mount
//! - `--tmpfs <dest>` + `--remount-ro <dest>` — mask a path (deny r/w, block creation)
//! - `--bind-try /dev/shm /dev/shm` — restore POSIX shared memory after `--dev`
//!   (LibreOffice/Chrome/Qt IPC; absent → "no valid pipe path found")
//! - `--new-session` — new session for process isolation
//! - `--cap-drop ALL` — drop every Linux capability after namespace/mount setup
//!   (closes the remount / setuid-helper escape class; applied right before `--`)
//!
//! ## Bind source/destination split
//!
//! `--ro-bind <src> <dest>` takes the host path to bind (`src`) and where it
//! appears inside the sandbox (`dest`). Policy paths are used for `dest`
//! verbatim, while `src` is the canonicalized host path. Binding the
//! canonicalized path at the canonicalized location (previous behavior)
//! leaves the sandbox without `/bin` on hosts where `/bin → /usr/bin`, and
//! `execvp("sh")` fails with ENOENT.
//!
//! ## Seccomp helper chaining
//!
//! bwrap cannot install per-thread seccomp filters itself. When the policy
//! sets `seccomp_restrict_network` and provides a `seccomp_helper` binary,
//! `wrap_command` binds the helper read-only into the sandbox and chains:
//! `bwrap <fs-args> -- <helper> [--inner-flag] --restrict-network -- <program> <args...>`.
//! The helper applies PR_SET_NO_NEW_PRIVS + seccomp in the child before exec
//! (see `linux_seccomp`), so network denial is enforced at the syscall level
//! even when the isolated netns is escaped via setuid helpers. The optional
//! inner flag supports self-embedding hosts (cortex re-execs its own main
//! executable; `--sandbox-exec-inner` dispatches before any initialization,
//! mirroring codex's single-binary `codex-linux-sandbox`).

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use super::{AccessMode, AllowedPath, SandboxEnforcer, SandboxPolicy, WrappedCommand};
use crate::error::SandboxError;

/// Linux bubblewrap sandbox enforcer.
///
/// Wraps child processes with `bwrap` arguments to enforce namespace-based
/// filesystem, network, and process isolation.
///
/// # Example
///
/// ```rust,ignore
/// use adk_sandbox::sandbox::linux::LinuxEnforcer;
/// use adk_sandbox::sandbox::{SandboxEnforcer, SandboxPolicyBuilder};
/// use std::ffi::OsString;
///
/// let enforcer = LinuxEnforcer::new();
/// enforcer.probe()?;
///
/// let policy = SandboxPolicyBuilder::new()
///     .allow_read("/usr/lib")
///     .allow_read_write("/tmp/work")
///     .build();
///
/// let wrapped = enforcer.wrap_command(
///     "python3".as_ref(),
///     &[OsString::from("-c"), OsString::from("print('hello')")],
///     &policy,
/// )?;
/// // wrapped.program == "bwrap"
/// // wrapped.args == ["--new-session", "--die-with-parent", "--unshare-user",
/// //                   "--unshare-pid", "--unshare-net",
/// //                   "--ro-bind", "/usr/lib", "/usr/lib",
/// //                   "--bind", "/tmp/work", "/tmp/work",
/// //                   "--", "python3", "-c", "print('hello')"]
/// ```
pub struct LinuxEnforcer;

impl LinuxEnforcer {
    /// Creates a new Linux bubblewrap enforcer.
    pub fn new() -> Self {
        Self
    }

    /// Generates bubblewrap arguments from the policy.
    ///
    /// Always starts with `--new-session`, `--die-with-parent`,
    /// `--unshare-user` and `--unshare-pid`.
    /// The returned arguments do NOT include the `--` separator or the
    /// original program/args — those are appended by `wrap_command`.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use adk_sandbox::sandbox::linux::LinuxEnforcer;
    /// use adk_sandbox::sandbox::SandboxPolicyBuilder;
    ///
    /// let policy = SandboxPolicyBuilder::new()
    ///     .allow_read("/usr/lib")
    ///     .build();
    ///
    /// let args = LinuxEnforcer::generate_args(&policy);
    /// assert_eq!(args[0], "--new-session");
    /// assert_eq!(args[1], "--die-with-parent");
    /// assert!(args.contains(&"--unshare-user".to_string()));
    /// assert!(args.contains(&"--unshare-net".to_string()));
    /// ```
    pub fn generate_args(policy: &SandboxPolicy) -> Vec<String> {
        // generate_args 是无失败面的 pub API:单条 canonicalize 失败时回退原路径
        // (bind src 与 dst 相同),由 bwrap 在运行期报错,而不是静默丢掉全部 bind。
        let canonicalized: Vec<AllowedPath> = policy
            .allowed_paths
            .iter()
            .map(|entry| match std::fs::canonicalize(&entry.path) {
                Ok(canonical) => AllowedPath { path: canonical, mode: entry.mode },
                Err(_) => entry.clone(),
            })
            .collect();
        // rw_bind_pairs 同理:src canonicalize 失败回退原路径,dst 非绝对直接跳过。
        let rw_pairs: Vec<(PathBuf, PathBuf)> = policy
            .rw_bind_pairs
            .iter()
            .filter(|pair| pair.dst.is_absolute())
            .map(|pair| {
                let src = std::fs::canonicalize(&pair.src).unwrap_or_else(|_| pair.src.clone());
                (src, pair.dst.clone())
            })
            .collect();
        Self::generate_args_from_paths(
            &policy.allowed_paths,
            &canonicalized,
            &policy.masked_paths,
            &policy.tmpfs_paths,
            &rw_pairs,
            policy.allow_network,
            policy.allow_process_spawn,
            /*as_pid_1*/ false,
        )
    }

    /// Internal: generates args from original + canonicalized path pairs.
    ///
    /// `orig` entries supply the bind destination (sandbox-visible path, kept
    /// verbatim); `canonical` entries supply the bind source (host path).
    /// See the module docs "Bind source/destination split" for why these
    /// differ.
    ///
    /// `as_pid_1`: run the command (typically the seccomp helper) as PID 1
    /// of the sandbox PID namespace, so orphaned descendants reparent to it
    /// and its `waitpid(-1)` reaper loop can collect them (aligned with
    /// codex #38396 "Reap orphaned processes in Linux sandboxes").
    // 内部 argv 生成 helper:5 个并行挂载清单切片 + 2 开关,语义各自独立,
    // 聚成 struct 反而割裂对应关系(同 cortex 主仓 ChildAgentFactory::new 惯例)。
    #[allow(clippy::too_many_arguments)]
    fn generate_args_from_paths(
        orig: &[AllowedPath],
        canonical: &[AllowedPath],
        masked: &[PathBuf],
        tmpfs_paths: &[PathBuf],
        rw_pairs: &[(PathBuf, PathBuf)],
        allow_network: bool,
        _allow_process_spawn: bool,
        as_pid_1: bool,
    ) -> Vec<String> {
        debug_assert_eq!(orig.len(), canonical.len());
        let mut args = Vec::with_capacity(16);

        // Always: new session (setsid) — detaches the sandboxed process from
        // the caller's controlling terminal/session so terminal signals do not
        // reach it. Aligned with codex, which passes --new-session
        // unconditionally; it does NOT restrict process spawning.
        args.push("--new-session".to_string());

        // Always: terminate child when parent exits
        args.push("--die-with-parent".to_string());

        // Always: request a user namespace explicitly rather than relying on
        // bubblewrap's auto-enable behavior, which is skipped when the caller
        // runs as uid 0 (aligned with codex). Without it, a root-run sandbox
        // keeps full root capabilities and the copied mounts are not locked:
        // a `mount -o remount,rw` from inside could bypass every read-only
        // bind and mask. The probe already exercises --unshare-user, so any
        // environment that passes the probe supports the flag.
        args.push("--unshare-user".to_string());

        // Always: isolate PID namespace
        args.push("--unshare-pid".to_string());

        // Network isolation
        if !allow_network {
            args.push("--unshare-net".to_string());
        }

        // `allow_process_spawn` no longer gates any flag: --new-session is
        // unconditional (it never was a spawn restriction) and sh -c always
        // needs fork/exec. The field is kept for API compatibility.

        // Run the wrapped command as PID 1 of the sandbox PID namespace so
        // orphans reparent to it (only meaningful when the command is the
        // seccomp helper, which runs a waitpid(-1) reaper loop). Requires
        // bwrap >= 0.5; probed once per process — an older bwrap would
        // abort on the unknown flag and break every network-denied command,
        // so the orphan reaping quietly degrades instead.
        if as_pid_1 && bwrap_supports_as_pid_1() {
            args.push("--as-pid-1".to_string());
        }

        // Filesystem bind mounts: src = canonicalized host path, dest = the
        // path as the policy requested it (symlinks like /bin stay usable).
        // bwrap mount destinations must be absolute — a relative policy path
        // (e.g. a workspace derived from a relative data_dir) falls back to
        // the canonical form, which is always absolute.
        for (entry, canon) in orig.iter().zip(canonical) {
            let src = canon.path.to_string_lossy().to_string();
            let dst = if entry.path.is_absolute() {
                entry.path.to_string_lossy().to_string()
            } else {
                src.clone()
            };
            match entry.mode {
                AccessMode::ReadOnly => {
                    args.push("--ro-bind".to_string());
                    args.push(src);
                    args.push(dst);
                }
                AccessMode::ReadWrite => {
                    args.push("--bind".to_string());
                    args.push(src);
                    args.push(dst);
                }
            }
        }

        // Writable binds with split source/destination: host `src` mounted at
        // sandbox `dst`. Emitted AFTER the allowed_paths loop — bwrap applies
        // mounts in argv order and later mounts win, so these overlay any
        // earlier view of dst (e.g. a full-root --ro-bind / /). Unlike
        // tmpfs_paths the writes reach the real host directory and persist
        // beyond the sandbox invocation (per-session host scratch dir at
        // /tmp, codex workspace-write's host /tmp bind has the same shape).
        for (src, dst) in rw_pairs {
            args.push("--bind".to_string());
            args.push(src.to_string_lossy().to_string());
            args.push(dst.to_string_lossy().to_string());
        }

        // Devtmpfs + procfs: /dev/null /dev/zero /dev/urandom + /proc
        // (ro-bind 带入的 /dev /proc 只读; --dev/--proc 创建新的可写实例)
        args.push("--dev".to_string());
        args.push("/dev".to_string());
        // /dev/shm: --dev 只建 null/zero/full/random/urandom/tty，不含 POSIX 共享内存。
        // LibreOffice/Chrome/Qt 等用 shm_open() 做 IPC（LibreOffice 单实例管道即依赖此），
        // 缺失会报 "no valid pipe path found"。--bind-try：宿主无 /dev/shm 时跳过而非失败，
        // 且不与 generate_args 的 --bind 计数冲突（属性测试按精确串 "--bind" 计数，"--bind-try"
        // 不匹配）。对齐 codex linux-sandbox/src/bwrap.rs 的 --bind-try /dev/shm /dev/shm。
        args.push("--bind-try".to_string());
        args.push("/dev/shm".to_string());
        args.push("/dev/shm".to_string());
        args.push("--proc".to_string());
        args.push("/proc".to_string());

        // Masked paths: read-only empty tmpfs overlaid AFTER all binds.
        // Existing directory contents are hidden, writes fail with EROFS,
        // and — because tmpfs is a fresh mount — the path cannot be removed
        // or re-created with different content inside the sandbox. Aligned
        // with codex's protected-metadata masking (`.git`/`.codex` under a
        // writable root, append_empty_directory_args):
        // `--perms 555 --tmpfs <path> --remount-ro <path>`. The explicit
        // --perms 555 keeps the traversal bit for any uid inside the user
        // namespace (a default tmpfs can end up owned by nobody). The
        // destination keeps the policy's path form verbatim (same rule as
        // binds): when the workspace path contains symlinks, canonicalizing
        // the mask target would mount the overlay at a location that does
        // not exist inside the sandbox.
        for path in masked {
            // 挂载点必须绝对:原始形态非绝对(相对 workspace 派生)时退回 canonical。
            // 与 bind dst 的回退规则一致,保证 mask 与 workspace 覆盖在同一形态。
            let dst = if path.is_absolute() {
                path.to_string_lossy().to_string()
            } else {
                match std::fs::canonicalize(path) {
                    Ok(c) => c.to_string_lossy().to_string(),
                    // wrap_command 已过滤不可解析路径;此处兜底(仅 generate_args 直调)
                    Err(_) => continue,
                }
            };
            args.push("--perms".to_string());
            args.push("555".to_string());
            args.push("--tmpfs".to_string());
            args.push(dst.clone());
            args.push("--remount-ro".to_string());
            args.push(dst);
        }

        // Writable private tmpfs paths, overlaid AFTER all binds and masks.
        // Emitted after the masked block so a policy that masks a parent and
        // tmpfs-mounts a child keeps the child writable (bwrap applies mounts
        // in argv order; later mounts win). Unlike the mask block this stays
        // writable — a fresh tmpfs with no host contents, so writes are
        // ephemeral and never reach the host filesystem. Aligned with codex's
        // default WorkspaceWrite policy, which mounts /tmp and $TMPDIR as
        // writable roots. The explicit --perms 1777 keeps the sticky bit and
        // world-writability that tools expect from /tmp (a default tmpfs can
        // end up owned by `nobody` inside the user namespace).
        for path in tmpfs_paths {
            if !path.is_absolute() {
                tracing::warn!(path = %path.display(), "tmpfs path is not absolute; skipping");
                continue;
            }
            let dst = path.to_string_lossy().to_string();
            args.push("--perms".to_string());
            args.push("1777".to_string());
            args.push("--tmpfs".to_string());
            args.push(dst);
        }

        // Always drop every Linux capability from the sandbox process. bwrap
        // applies this after namespace/mount setup and right before exec'ing
        // the command, so the mounts above are unaffected. Inside a user
        // namespace the process otherwise holds a full capability set *in that
        // namespace* — enough to remount over the ro-bind/mask overlays or
        // unshare a fresh mount namespace; cap-drop closes that class for both
        // root and non-root callers (aligned with codex, which pushes
        // `--cap-drop ALL` immediately before `--`).
        args.push("--cap-drop".to_string());
        args.push("ALL".to_string());

        args
    }
}

impl Default for LinuxEnforcer {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxEnforcer for LinuxEnforcer {
    fn name(&self) -> &str {
        "bubblewrap"
    }

    fn probe(&self) -> Result<(), SandboxError> {
        // Check that bwrap binary exists
        let result = std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match result {
            Ok(status) if status.success() => {
                // Also check user namespaces are available
                // The root bind is not optional. bwrap gives the new namespace an empty
                // root, so without it `/bin/true` does not exist inside and execvp fails —
                // which this probe then reported as "user namespaces are not available",
                // pointing operators at a sysctl that was never the problem. The check
                // failed on every host, so the enforcer was never selectable on Linux.
                let ns_check = std::process::Command::new("bwrap")
                    .args(["--ro-bind", "/", "/", "--unshare-user", "--", "/bin/true"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();

                match ns_check {
                    Ok(s) if s.success() => Ok(()),
                    _ => Err(SandboxError::EnforcerUnavailable {
                        enforcer: "bubblewrap".to_string(),
                        message: "user namespaces are not available. Check that \
                                  `kernel.unprivileged_userns_clone` sysctl is set to 1."
                            .to_string(),
                    }),
                }
            }
            Ok(_) => Err(SandboxError::EnforcerUnavailable {
                enforcer: "bubblewrap".to_string(),
                message: "bwrap binary found but returned an error. \
                          Verify installation is complete."
                    .to_string(),
            }),
            Err(e) => Err(SandboxError::EnforcerUnavailable {
                enforcer: "bubblewrap".to_string(),
                message: format!(
                    "bwrap binary not found: {e}. Install bubblewrap: \
                     `apt install bubblewrap` (Debian/Ubuntu) or \
                     `dnf install bubblewrap` (Fedora/RHEL)."
                ),
            }),
        }
    }

    fn wrap_command(
        &self,
        program: &OsStr,
        args: &[OsString],
        policy: &SandboxPolicy,
    ) -> Result<WrappedCommand, SandboxError> {
        // Warn if domain-level network rules are present — bubblewrap can't enforce them
        if !policy.allow_network && !policy.network_rules.is_empty() {
            tracing::warn!(
                rules_count = policy.network_rules.len(),
                "bubblewrap does not support per-domain network filtering; \
                 network_rules will be ignored and all network access will be blocked"
            );
        }

        // 1. Canonicalize all paths in the policy
        let canonicalized_paths = canonicalize_paths(&policy.allowed_paths)?;

        // 2. Filter masked paths down to absolute + host-resolvable ones; the
        //    overlay mounts at the policy's ORIGINAL path form (same rule as
        //    bind destinations — a canonicalized mask target would land at a
        //    location that does not exist inside the sandbox when the
        //    enclosing workspace path contains symlinks).
        let masked_paths: Vec<PathBuf> = policy
            .masked_paths
            .iter()
            .filter(|p| {
                let absolute = p.is_absolute();
                if !absolute {
                    tracing::warn!(path = %p.display(), "masked path is not absolute; skipping");
                }
                absolute
            })
            .filter(|p| {
                let resolvable = std::fs::canonicalize(p).is_ok();
                if !resolvable {
                    tracing::warn!(
                        path = %p.display(),
                        "masked path does not resolve on host; skipping"
                    );
                }
                resolvable
            })
            .cloned()
            .collect();

        // 2b. rw_bind_pairs: src canonicalize 失败跳过(告警)而非整命令失败——
        //     与 allowed_paths 的 fail-closed 不同,这类挂载是便利性挂载
        //     (如可能被并发清理的会话 tmp 目录),缺失时命令仍应在更小的
        //     视图下运行。dst 非绝对同样跳过。
        let rw_pairs: Vec<(PathBuf, PathBuf)> = policy
            .rw_bind_pairs
            .iter()
            .filter_map(|pair| {
                if !pair.dst.is_absolute() {
                    tracing::warn!(
                        dst = %pair.dst.display(),
                        "rw bind destination is not absolute; skipping"
                    );
                    return None;
                }
                match std::fs::canonicalize(&pair.src) {
                    Ok(src) => Some((src, pair.dst.clone())),
                    Err(e) => {
                        tracing::warn!(
                            src = %pair.src.display(),
                            error = %e,
                            "rw bind source does not resolve on host; skipping"
                        );
                        None
                    }
                }
            })
            .collect();

        // 3. Generate bwrap args. When a seccomp helper is configured and
        //    network is denied, the helper binary is bound read-only into the
        //    sandbox and the command chain becomes
        //    `bwrap <args> -- <helper> --restrict-network -- <program> <args>`.
        let mut helper_mount = None;
        let seccomp_helper = if policy.seccomp_restrict_network && !policy.allow_network {
            match &policy.seccomp_helper {
                Some(helper) => match std::fs::canonicalize(helper) {
                    Ok(canonical) => {
                        helper_mount = Some(canonical.clone());
                        Some(canonical)
                    }
                    Err(e) => {
                        // Missing/unresolvable helper: fall back to no seccomp
                        // (netns isolation still enforced) rather than failing
                        // every command. Log loudly — this is a deployment gap.
                        tracing::warn!(
                            helper = %helper.display(),
                            error = %e,
                            "seccomp helper not found; falling back to netns-only network isolation"
                        );
                        None
                    }
                },
                None => {
                    tracing::warn!(
                        "seccomp_restrict_network set but no seccomp_helper configured; ignoring"
                    );
                    None
                }
            }
        } else {
            None
        };

        // The helper must be visible inside the sandbox at its own host path
        // (its own argv[0]/dynamic-loader resolution depends on it).
        let mut allowed = policy.allowed_paths.clone();
        let mut canonical_allowed = canonicalized_paths;
        if let Some(helper) = &helper_mount {
            allowed.push(super::AllowedPath { path: helper.clone(), mode: super::AccessMode::ReadOnly });
            canonical_allowed.push(super::AllowedPath { path: helper.clone(), mode: super::AccessMode::ReadOnly });
        }

        let bwrap_args = Self::generate_args_from_paths(
            &allowed,
            &canonical_allowed,
            &masked_paths,
            &policy.tmpfs_paths,
            &rw_pairs,
            policy.allow_network,
            policy.allow_process_spawn,
            /*as_pid_1*/ seccomp_helper.is_some(),
        );

        // 4. Build the wrapped command: bwrap <args> -- <helper invocation> <program> <original_args...>
        // Helper invocation is `<helper> --restrict-network -- <program> <args>` for
        // the legacy standalone helper binary, or
        // `<helper> <inner_flag> --restrict-network -- <program> <args>` when the
        // host binary self-embeds the helper (cortex re-execs its own main
        // executable; argv dispatch happens before any initialization).
        let mut wrapped_args: Vec<OsString> = bwrap_args.into_iter().map(OsString::from).collect();
        wrapped_args.push(OsString::from("--"));
        if let Some(helper) = seccomp_helper {
            wrapped_args.push(OsString::from(helper));
            if let Some(flag) = &policy.seccomp_helper_inner_flag {
                wrapped_args.push(OsString::from(flag.as_str()));
            }
            wrapped_args.push(OsString::from("--restrict-network"));
            wrapped_args.push(OsString::from("--"));
        }
        wrapped_args.push(program.to_owned());
        wrapped_args.extend_from_slice(args);

        Ok(WrappedCommand { program: OsString::from("bwrap"), args: wrapped_args })
    }
}

/// Whether the installed `bwrap` supports `--as-pid-1` (bwrap >= 0.5).
///
/// Cached per process. `--help` output contains the flag string when
/// supported; a missing/old bwrap yields false. Failing closed here would
/// break every network-denied command on older hosts, so callers degrade
/// to launching without PID-1 reaping instead.
fn bwrap_supports_as_pid_1() -> bool {
    use std::sync::OnceLock;
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let output = std::process::Command::new("bwrap")
            .arg("--help")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                text.contains("--as-pid-1") || String::from_utf8_lossy(&out.stderr).contains("--as-pid-1")
            }
            Err(_) => false,
        }
    })
}

/// Canonicalizes all paths in the policy, logging warnings for changed paths.
fn canonicalize_paths(paths: &[AllowedPath]) -> Result<Vec<AllowedPath>, SandboxError> {    let mut result = Vec::with_capacity(paths.len());

    for entry in paths {
        let canonical = std::fs::canonicalize(&entry.path).map_err(|e| {
            SandboxError::PolicyViolation(format!(
                "failed to canonicalize allowed path '{}': {e}",
                entry.path.display()
            ))
        })?;

        if canonical != entry.path {
            tracing::warn!(
                original = %entry.path.display(),
                resolved = %canonical.display(),
                "allowed path resolved to a different location (possible symlink)"
            );
        }

        result.push(AllowedPath { path: canonical, mode: entry.mode });
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxPolicyBuilder;

    #[test]
    fn test_generate_args_deny_all() {
        let policy = SandboxPolicyBuilder::new().build();
        let args = LinuxEnforcer::generate_args(&policy);

        assert_eq!(args[0], "--new-session");
        assert_eq!(args[1], "--die-with-parent");
        assert_eq!(args[2], "--unshare-user");
        assert_eq!(args[3], "--unshare-pid");
        assert!(args.contains(&"--unshare-net".to_string()));
        // No bind mounts
        assert!(!args.contains(&"--ro-bind".to_string()));
        assert!(!args.contains(&"--bind".to_string()));
    }

    #[test]
    fn test_generate_args_rw_bind_pair_overlays_root() {
        // 整盘只读 + 会话宿主目录挂到 /tmp:--bind 必须晚于 --ro-bind / /
        // (bwrap 后挂载生效),/tmp 才是可写宿主目录而非只读视图。
        // src 在测试环境不存在 → generate_args 回退原路径,dst 保留原样。
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/")
            .bind_read_write_at("/srv/session-42/tmp", "/tmp")
            .build();
        let args = LinuxEnforcer::generate_args(&policy);

        let idx = |flag: &str, operand: &str| -> Option<usize> {
            args.windows(2).position(|w| w[0] == flag && w[1] == operand)
        };
        let ro_root = idx("--ro-bind", "/").expect("full-root ro bind");
        let tmp_bind = idx("--bind", "/srv/session-42/tmp").expect("session tmp rw bind");
        assert!(tmp_bind > ro_root, "rw pair must be emitted after the root bind");
        assert_eq!(args[tmp_bind + 2], "/tmp", "dst form is preserved verbatim");
    }

    #[test]
    fn test_generate_args_always_unshares_user() {
        // --unshare-user 必须无条件出现:root 运行的调用者下 bwrap 自动 userns
        // 被跳过,缺此 flag 会让 ro-bind/mask 可被 remount 绕过(对齐 codex)。
        for policy in [
            SandboxPolicyBuilder::new().build(),
            SandboxPolicyBuilder::new().allow_process_spawn().build(),
            SandboxPolicyBuilder::new().allow_network().allow_process_spawn().build(),
        ] {
            let args = LinuxEnforcer::generate_args(&policy);
            assert!(
                args.contains(&"--unshare-user".to_string()),
                "--unshare-user must be unconditional: {args:?}"
            );
            assert!(
                args.contains(&"--new-session".to_string()),
                "--new-session must be unconditional: {args:?}"
            );
        }
    }

    #[test]
    fn test_generate_args_read_only_path() {
        let policy = SandboxPolicyBuilder::new().allow_read("/usr/lib").build();
        let args = LinuxEnforcer::generate_args(&policy);

        let ro_idx = args.iter().position(|a| a == "--ro-bind").unwrap();
        assert_eq!(args[ro_idx + 1], "/usr/lib");
        assert_eq!(args[ro_idx + 2], "/usr/lib");
        assert!(!args.contains(&"--bind".to_string()));
    }

    #[test]
    fn test_generate_args_read_write_path() {
        let policy = SandboxPolicyBuilder::new().allow_read_write("/tmp/work").build();
        let args = LinuxEnforcer::generate_args(&policy);

        let bind_idx = args.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(args[bind_idx + 1], "/tmp/work");
        assert_eq!(args[bind_idx + 2], "/tmp/work");
        assert!(!args.contains(&"--ro-bind".to_string()));
    }

    #[test]
    fn test_bind_keeps_symlink_destination() {
        // /bin on most distros is a symlink to /usr/bin. The bind source must
        // be the canonical host path, but the destination must stay /bin —
        // otherwise the sandbox root has no /bin and execvp("sh") fails.
        let policy = SandboxPolicyBuilder::new().allow_read("/bin").build();
        let args = LinuxEnforcer::generate_args(&policy);

        let ro_idx = args.iter().position(|a| a == "--ro-bind").expect("ro-bind present");
        let src = &args[ro_idx + 1];
        let dst = &args[ro_idx + 2];
        assert_eq!(dst, "/bin", "destination must be the requested path verbatim");
        // src 是宿主 canonical 路径(/bin 符号链接解析后的 /usr/bin);不硬编码,
        // 只验证 src == dst 或二者不同时 src 确实存在(符号链接宿主)。
        if src != dst {
            assert!(
                std::path::Path::new(src).exists(),
                "canonical source {src} must exist on host"
            );
        }
    }

    #[test]
    fn test_generate_args_masks_paths_after_binds() {
        // masked path 覆盖必须排在全部 bind 之后(bwrap 后挂载覆盖语义),
        // 形如 --tmpfs <p> --remount-ro <p>。
        let masked = if let Ok(c) = std::fs::canonicalize("/tmp") {
            c.join("cortex_mask_test_dir")
        } else {
            std::path::PathBuf::from("/tmp/cortex_mask_test_dir")
        };
        std::fs::create_dir_all(&masked).unwrap();
        let policy = SandboxPolicyBuilder::new()
            .allow_read_write("/tmp")
            .mask_path(masked.clone())
            .build();
        let args = LinuxEnforcer::generate_args(&policy);

        let masked_str = masked.to_string_lossy().to_string();
        // --perms 555 --tmpfs <p> --remount-ro <p> 序列完整出现(对齐 codex
        // append_empty_directory_args,--perms 555 保证 userns 内任意 uid 可遍历)
        let perms_idx = args.iter().position(|a| a == "--perms").expect("--perms present");
        assert_eq!(args[perms_idx + 1], "555");
        assert_eq!(args[perms_idx + 2], "--tmpfs");
        assert_eq!(args[perms_idx + 3], masked_str);
        assert_eq!(args[perms_idx + 4], "--remount-ro");
        assert_eq!(args[perms_idx + 5], masked_str);
        // masked 覆盖在所有 --bind/--ro-bind 之后
        let last_bind_idx = args.iter().rposition(|a| a == "--bind" || a == "--ro-bind")
            .expect("some bind present");
        assert!(perms_idx > last_bind_idx, "mask must overlay after binds");
        let _ = std::fs::remove_dir(&masked);
    }

    #[test]
    fn test_generate_args_emits_absolute_mask_even_if_missing_on_host() {
        // generate_args 是无失败面 API:绝对路径(即使宿主不存在)原样 emit,
        // 由 bwrap 运行期报错——静默跳过会让 mask 保护悄悄失效。不可解析
        // 路径的过滤发生在 wrap_command(那里有 warn 日志)。此测试与
        // test_wrap_command_missing_mask... 互补,覆盖两层各自的语义。
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/usr")
            .mask_path("/nonexistent/masked/path")
            .build();
        let args = LinuxEnforcer::generate_args(&policy);
        let tmpfs_idx = args.iter().position(|a| a == "--tmpfs").expect("mask emitted");
        assert_eq!(args[tmpfs_idx + 1], "/nonexistent/masked/path");
        assert!(args.iter().any(|a| a == "--ro-bind"));
    }

    #[test]
    fn test_generate_args_writable_tmpfs_overlays_bind() {
        // /tmp 先 ro-bind(平台只读根),writable_tmpfs("/tmp") 在其后覆盖成
        // 私有可写 tmpfs(--perms 1777 --tmpfs /tmp,无 --remount-ro)。
        // LibreOffice oosplash 对 /tmp 做 access(W_OK),只读 bind 会触发
        // "no valid pipe path found"。
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/tmp")
            .writable_tmpfs("/tmp")
            .build();
        let args = LinuxEnforcer::generate_args(&policy);

        let tmpfs_idx = args
            .iter()
            .position(|a| a == "--tmpfs")
            .expect("writable tmpfs must be emitted");
        assert_eq!(args[tmpfs_idx - 1], "1777", "sticky+world-writable like host /tmp");
        assert_eq!(args[tmpfs_idx + 1], "/tmp");
        // 可写 tmpfs 不跟 --remount-ro(masked 的只读覆盖才有);--tmpfs /tmp
        // 是最后一段 args,后面允许直接结束
        assert_ne!(args.get(tmpfs_idx + 2).map(String::as_str), Some("--remount-ro"));
        // 覆盖必须排在 /tmp 的 ro-bind 之后(bwrap 后挂载生效)
        let ro_bind_idx = args
            .iter()
            .position(|a| a == "--ro-bind")
            .expect("/tmp ro-bind present");
        assert!(tmpfs_idx > ro_bind_idx, "tmpfs overlay must come after binds");
    }

    #[test]
    fn test_generate_args_skips_relative_tmpfs_path() {
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/usr")
            .writable_tmpfs("relative/tmp")
            .build();
        let args = LinuxEnforcer::generate_args(&policy);
        assert!(!args.contains(&"--tmpfs".to_string()));
    }

    #[test]
    fn test_generate_args_network_allowed() {
        let policy = SandboxPolicyBuilder::new().allow_network().build();
        let args = LinuxEnforcer::generate_args(&policy);

        assert!(!args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn test_generate_args_network_denied() {
        let policy = SandboxPolicyBuilder::new().build();
        let args = LinuxEnforcer::generate_args(&policy);

        assert!(args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn test_generate_args_process_spawn_allowed() {
        // --new-session 是 setsid 隔离固定件,不再受 allow_process_spawn 控制
        // (它本就不限制 spawn;sh -c 总是需要 fork/exec)。
        let policy = SandboxPolicyBuilder::new().allow_process_spawn().build();
        let args = LinuxEnforcer::generate_args(&policy);

        assert!(args.contains(&"--new-session".to_string()));
    }

    #[test]
    fn test_generate_args_process_spawn_denied() {
        let policy = SandboxPolicyBuilder::new().build();
        let args = LinuxEnforcer::generate_args(&policy);

        assert!(args.contains(&"--new-session".to_string()));
    }

    #[test]
    fn test_generate_args_starts_with_new_session_then_die_with_parent() {
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/tmp")
            .allow_network()
            .allow_process_spawn()
            .build();
        let args = LinuxEnforcer::generate_args(&policy);

        assert_eq!(args[0], "--new-session");
        assert_eq!(args[1], "--die-with-parent");
    }

    #[test]
    fn test_generate_args_ends_with_cap_drop_all() {
        // cap-drop must be the last flags: wrap_command appends `--` right
        // after these args, so bwrap applies it after all namespace/mount
        // setup and immediately before exec'ing the command (codex parity).
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/tmp")
            .allow_network()
            .build();
        let args = LinuxEnforcer::generate_args(&policy);

        let n = args.len();
        assert!(n >= 2);
        assert_eq!(args[n - 2], "--cap-drop");
        assert_eq!(args[n - 1], "ALL");
    }

    #[test]
    fn test_generate_args_no_empty_strings() {
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/usr/lib")
            .allow_read_write("/tmp")
            .allow_network()
            .build();
        let args = LinuxEnforcer::generate_args(&policy);

        for arg in &args {
            assert!(!arg.is_empty(), "found empty string in bwrap args");
        }
    }

    #[test]
    fn test_generate_args_restores_dev_shm() {
        // LibreOffice/Chrome/Qt need POSIX shm (/dev/shm) for IPC; --dev /dev alone
        // does not provide it. Verify the enforcer emits --bind-try /dev/shm /dev/shm
        // after --dev /dev, regardless of policy.
        let policy = SandboxPolicyBuilder::new().allow_network().allow_process_spawn().build();
        let args = LinuxEnforcer::generate_args(&policy);

        let dev_idx = args.iter().position(|a| a == "--dev").expect("--dev present");
        // --dev /dev is a flag + single path arg.
        assert_eq!(args[dev_idx + 1], "/dev");

        // Find the --bind-try pair for /dev/shm.
        let found = args.windows(3).any(|w| {
            w[0] == "--bind-try" && w[1] == "/dev/shm" && w[2] == "/dev/shm"
        });
        assert!(found, "--bind-try /dev/shm /dev/shm must be emitted, got: {args:?}");

        // It must come after --dev so it overlays the fresh devtmpfs, not the ro-bind.
        let bind_try_idx = args
            .iter()
            .position(|a| a == "--bind-try")
            .expect("--bind-try present");
        assert!(bind_try_idx > dev_idx, "--bind-try /dev/shm must follow --dev /dev");
    }

    #[test]
    fn test_name() {
        let enforcer = LinuxEnforcer::new();
        assert_eq!(enforcer.name(), "bubblewrap");
    }

    #[test]
    fn test_wrap_command_nonexistent_path_fails() {
        let enforcer = LinuxEnforcer::new();
        let policy =
            SandboxPolicyBuilder::new().allow_read("/nonexistent/path/that/does/not/exist").build();

        let result = enforcer.wrap_command(OsStr::new("echo"), &[], &policy);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, SandboxError::PolicyViolation(_)),
            "expected PolicyViolation, got: {err:?}"
        );
    }

    #[test]
    fn test_wrap_command_no_mask_emits_no_tmpfs() {
        let enforcer = LinuxEnforcer::new();
        let policy = SandboxPolicyBuilder::new().allow_read("/usr").build();
        let wrapped = enforcer.wrap_command(OsStr::new("echo"), &[], &policy).unwrap();
        let args: Vec<String> = wrapped.args.iter().map(|a| a.to_string_lossy().into()).collect();
        assert!(!args.iter().any(|a| a == "--tmpfs"));
    }

    #[test]
    fn test_wrap_command_seccomp_helper_chain() {
        // helper 存在且禁网:命令链为 bwrap <args> -- <helper> --restrict-network -- echo
        let helper = std::env::current_exe().unwrap(); // 任意存在的可执行文件充当 helper
        let enforcer = LinuxEnforcer::new();
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/usr")
            .seccomp_restrict_network_with(&helper)
            .build();
        let wrapped = enforcer.wrap_command(OsStr::new("echo"), &["hi".into()], &policy).unwrap();
        let args: Vec<String> = wrapped.args.iter().map(|a| a.to_string_lossy().into()).collect();

        let sep_idx = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(
            args[sep_idx + 1],
            helper.canonicalize().unwrap().to_string_lossy(),
            "helper must be the first command after the fs-args separator"
        );
        assert_eq!(args[sep_idx + 2], "--restrict-network");
        assert_eq!(args[sep_idx + 3], "--", "second separator before user command");
        assert_eq!(args[sep_idx + 4], "echo");
        assert_eq!(args[sep_idx + 5], "hi");
        // helper 自身被 ro-bind 进沙箱(canonical 路径 src 与 dst)
        let helper_str = helper.canonicalize().unwrap().to_string_lossy().to_string();
        assert!(
            args.windows(3).any(|w| w[0] == "--ro-bind" && w[1] == helper_str && w[2] == helper_str),
            "helper must be ro-bound into the sandbox: {args:?}"
        );
        // --as-pid-1:helper 当 PID ns 的 PID 1 收孤儿(宿主 bwrap 支持时;
        // 本测试只断言"支持时出现/不支持时缺省",不假设宿主装了哪版 bwrap)
        if bwrap_supports_as_pid_1() {
            assert!(
                args.iter().any(|a| a == "--as-pid-1"),
                "--as-pid-1 must be emitted for the helper chain on bwrap >= 0.5"
            );
        } else {
            assert!(!args.iter().any(|a| a == "--as-pid-1"));
        }
    }

    #[test]
    fn test_wrap_command_self_embedded_helper_chain() {
        // 自嵌模式(cortex 主二进制):inner_flag 插在 helper 程序之后、--restrict-network 之前
        let helper = std::env::current_exe().unwrap();
        let enforcer = LinuxEnforcer::new();
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/usr")
            .seccomp_restrict_network_self_embedded(&helper, "--sandbox-exec-inner")
            .build();
        let wrapped = enforcer.wrap_command(OsStr::new("echo"), &[], &policy).unwrap();
        let args: Vec<String> = wrapped.args.iter().map(|a| a.to_string_lossy().into()).collect();

        let sep_idx = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(
            args[sep_idx + 1],
            helper.canonicalize().unwrap().to_string_lossy(),
            "self-embedded helper is the host executable itself"
        );
        assert_eq!(args[sep_idx + 2], "--sandbox-exec-inner", "inner flag before --restrict-network");
        assert_eq!(args[sep_idx + 3], "--restrict-network");
        assert_eq!(args[sep_idx + 4], "--");
        assert_eq!(args[sep_idx + 5], "echo");
    }

    #[test]
    fn test_wrap_command_seccomp_helper_skipped_when_network_allowed() {
        // allow_network 时不需要 helper(无禁网可兜底),也不挂 helper
        let helper = std::env::current_exe().unwrap();
        let enforcer = LinuxEnforcer::new();
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/usr")
            .allow_network()
            .seccomp_restrict_network_with(&helper)
            .build();
        let wrapped = enforcer.wrap_command(OsStr::new("echo"), &[], &policy).unwrap();
        let args: Vec<String> = wrapped.args.iter().map(|a| a.to_string_lossy().into()).collect();
        assert!(!args.iter().any(|a| a == "--restrict-network"));
    }

    #[test]
    fn test_wrap_command_missing_helper_warns_and_skips() {
        // helper 不存在:warn + 退回纯 netns 隔离(命令链无 helper),不报错
        let enforcer = LinuxEnforcer::new();
        let policy = SandboxPolicyBuilder::new()
            .allow_read("/usr")
            .seccomp_restrict_network_with("/nonexistent/cortex-sandbox-exec")
            .build();
        let wrapped = enforcer.wrap_command(OsStr::new("echo"), &[], &policy).unwrap();
        let args: Vec<String> = wrapped.args.iter().map(|a| a.to_string_lossy().into()).collect();
        assert!(!args.iter().any(|a| a == "--restrict-network"));
        assert_eq!(wrapped.program, "bwrap");
    }
}
