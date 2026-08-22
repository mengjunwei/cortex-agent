//! Linux seccomp 过滤器(网络隔离兜底 + 进程内省防护)。
//!
//! 对齐 codex `linux-sandbox/src/landlock.rs` 的
//! `install_network_seccomp_filter_on_current_thread`:
//! - 默认动作 Allow,命中规则返回 EPERM(工具报"operation not permitted"而非被 SIGSYS 杀死)
//! - 无条件禁 `ptrace`/`process_vm_readv`/`process_vm_writev`/`io_uring_*`
//! - 禁网时再封 connect/bind/listen/sendto 等,`socket`/`socketpair` 仅放行 AF_UNIX
//!
//! 应用方式与 codex 一致:**只在子进程内、exec 目标命令之前**调用
//! [`apply_seccomp_filter`](self)(`PR_SET_NO_NEW_PRIVS` + seccomp 均 per-thread,
//! 子进程 exec 后被目标命令继承,不影响父进程/其他线程)。配套 helper 由宿主
//! 应用自嵌提供(cortex: 主二进制以 `cortex-agent --sandbox-exec-inner` 在 bwrap
//! 内 re-exec 自己),bwrap 命令链为
//! `bwrap <fs-args> -- cortex-agent --sandbox-exec-inner --restrict-network -- <cmd> <args>`。

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// 网络过滤模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkFilterMode {
    /// 禁网:封 connect/bind/listen 等;socket/socketpair 仅放行 AF_UNIX。
    Restricted,
}

/// 无条件禁用的 syscall(与沙箱模式、网络开关无关)。
///
/// - ptrace / process_vm_*:防进程内省/注入沙箱外进程
/// - io_uring_*:io_uring 可绕过部分 /proc 与 seccomp 审计路径,内核攻击面大
/// - mount / umount2 / setns:纵深防御——userns 内进程对该 ns 持全量 capability,
///   可 remount 覆盖 ro-bind/mask 或 setns 加入宿主 namespace。bwrap 侧已
///   `--cap-drop ALL`(主闸),此处兜底同一攻击面(对齐 codex 同层封禁)
fn unconditional_denies(rules: &mut BTreeMap<i64, Vec<SeccompRule>>) {
    fn deny(rules: &mut BTreeMap<i64, Vec<SeccompRule>>, nr: i64) {
        // 空 rule vec = 无条件命中
        rules.insert(nr, vec![]);
    }
    deny(rules, libc::SYS_ptrace);
    deny(rules, libc::SYS_process_vm_readv);
    deny(rules, libc::SYS_process_vm_writev);
    deny(rules, libc::SYS_io_uring_setup);
    deny(rules, libc::SYS_io_uring_enter);
    deny(rules, libc::SYS_io_uring_register);
    deny(rules, libc::SYS_mount);
    deny(rules, libc::SYS_umount2);
    deny(rules, libc::SYS_setns);
}

/// 禁网 syscall 集合:connect/accept/bind/listen/shutdown/sendto/…
///
/// 刻意放行 `recvfrom`——cargo clippy 等工具用 socketpair + 子进程管理,
/// 封掉会误伤合法工具(对齐 codex 同处注释)。
fn restricted_network_denies(
    rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
) -> Result<(), seccompiler::Error> {
    fn deny(rules: &mut BTreeMap<i64, Vec<SeccompRule>>, nr: i64) {
        rules.insert(nr, vec![]);
    }
    deny(rules, libc::SYS_connect);
    deny(rules, libc::SYS_accept);
    deny(rules, libc::SYS_accept4);
    deny(rules, libc::SYS_bind);
    deny(rules, libc::SYS_listen);
    deny(rules, libc::SYS_getpeername);
    deny(rules, libc::SYS_getsockname);
    deny(rules, libc::SYS_shutdown);
    deny(rules, libc::SYS_sendto);
    deny(rules, libc::SYS_sendmmsg);
    deny(rules, libc::SYS_recvmmsg);
    deny(rules, libc::SYS_getsockopt);
    deny(rules, libc::SYS_setsockopt);

    // socket/socketpair 仅放行 AF_UNIX(arg0 == AF_UNIX,其余 domain EPERM)
    let unix_only = SeccompRule::new(vec![SeccompCondition::new(
        0, // 第一个参数(domain)
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )?])?;
    rules.insert(libc::SYS_socket, vec![unix_only.clone()]);
    rules.insert(libc::SYS_socketpair, vec![unix_only]);

    Ok(())
}

/// 在**当前线程**安装 seccomp 过滤器(调用方须为刚 fork 出的子进程)。
///
/// 流程对齐 codex:先 `PR_SET_NO_NEW_PRIVS`(seccomp 前置条件,兼防 setuid 提权),
/// 再 `apply_filter`。两者均只作用于当前线程及其 exec 后代,父进程(服务主进程)不受影响。
///
/// # Errors
/// prctl 或 seccomp 应用失败返回 `SandboxError::EnforcerFailed`(调用方 fail-closed)。
pub fn apply_seccomp_filter(
    mode: NetworkFilterMode,
) -> Result<(), crate::error::SandboxError> {
    // PR_SET_NO_NEW_PRIVS:seccomp 非 root 应用前置条件,同时封死 setuid 提权路径
    let nnp = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if nnp != 0 {
        return Err(crate::error::SandboxError::EnforcerFailed {
            enforcer: "seccomp".to_string(),
            message: format!(
                "PR_SET_NO_NEW_PRIVS failed: {}",
                std::io::Error::last_os_error()
            ),
        });
    }

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    unconditional_denies(&mut rules);
    // 单臂 match(而非 if let):单变体枚举上 if let 是 irrefutable pattern 告警;
    // match 写法在 NetworkFilterMode 未来加变体时强制显式处理(非穷尽=编译错)。
    match mode {
        NetworkFilterMode::Restricted => {
            restricted_network_denies(&mut rules).map_err(|e| {
                crate::error::SandboxError::EnforcerFailed {
                    enforcer: "seccomp".to_string(),
                    message: format!("failed to build network rules: {e}"),
                }
            })?;
        }
    }

    let arch = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        return Err(crate::error::SandboxError::EnforcerFailed {
            enforcer: "seccomp".to_string(),
            message: "unsupported architecture for seccomp filter".to_string(),
        });
    };

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                     // 默认放行
        SeccompAction::Errno(libc::EPERM as u32), // 命中规则返回 EPERM
        arch,
    )
    .map_err(|e| crate::error::SandboxError::EnforcerFailed {
        enforcer: "seccomp".to_string(),
        message: format!("failed to create seccomp filter: {e}"),
    })?;

    let prog: BpfProgram = filter.try_into().map_err(|e| {
        crate::error::SandboxError::EnforcerFailed {
            enforcer: "seccomp".to_string(),
            message: format!("failed to compile seccomp program: {e}"),
        }
    })?;

    // seccompiler 的 apply_filter 对当前线程调 seccomp(2)。本函数只应在
    // fork 出的子进程内、exec 目标命令之前调用(见模块文档)。
    seccompiler::apply_filter(&prog).map_err(|e| {
        crate::error::SandboxError::EnforcerFailed {
            enforcer: "seccomp".to_string(),
            message: format!("failed to apply seccomp filter: {e}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 过滤器构造可编译、规则表包含无条件禁用项(不真正 apply,测试进程自身不能装过滤器)
    #[test]
    fn rule_table_contains_unconditional_denies() {
        let mut rules = BTreeMap::new();
        unconditional_denies(&mut rules);
        for nr in [
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_setns,
        ] {
            assert!(rules.contains_key(&nr), "missing unconditional deny for {nr}");
            assert!(rules[&nr].is_empty(), "unconditional deny must match always");
        }
    }

    #[test]
    fn restricted_mode_adds_network_denies() {
        let mut rules = BTreeMap::new();
        unconditional_denies(&mut rules);
        restricted_network_denies(&mut rules).unwrap();
        assert!(rules.contains_key(&libc::SYS_connect));
        assert!(rules.contains_key(&libc::SYS_bind));
        // socket/socketpair 是条件规则(AF_UNIX 放行),vec 非空
        assert!(!rules[&libc::SYS_socket].is_empty());
        assert!(!rules[&libc::SYS_socketpair].is_empty());
    }
}
