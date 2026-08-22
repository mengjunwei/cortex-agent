//! cortex 沙箱 seccomp helper(嵌入主二进制,Linux 生效)。
//!
//! 单二进制自嵌(对齐 codex `codex-linux-sandbox` 的单文件两阶段模式,差异仅在
//! codex 靠同一 argv 自嵌,这里靠主二进制 argv 拦截):bwrap 命令链为
//! `bwrap <fs-args> -- cortex-agent --sandbox-exec-inner --restrict-network -- <cmd> <args...>`
//!
//! 主程序 [`run_sandbox_exec_inner`](crate::infra::run_sandbox_exec_inner) 在入口
//! 最前(bintool/日志/配置初始化之前)检测到 `--sandbox-exec-inner` 即进入本函数:
//!
//! 1. 应用 `PR_SET_NO_NEW_PRIVS` + seccomp 过滤器(per-thread,本进程 exec/fork
//!    的后代全部继承;bwrap 外的服务主进程不受影响——那是另一个进程实例)
//! 2. fork 出目标命令,本进程留在沙箱 PID namespace 里当"reaper":
//!    `waitpid(-1)` 循环收尸所有后代(含 sh 退出后的孤儿;codex #38396)
//! 3. 转发 SIGHUP/INT/QUIT/TERM 到命令进程组(外层以 SIGTERM 优雅终止时——如
//!    bwrap --die-with-parent 触发——命令进程组立刻收到而非等 SIGKILL 兜底)
//! 4. wait status 原样透出退出码/信号(信号死 self-raise 同信号,bwrap 外层
//!    链路能正确上报 killed-by-signal)
//!
//! 因为复用主二进制,部署零额外文件、永不"漏部署降级"(旧独立 helper 方案的
//! 降级路径随之消失;enforcer 对不存在的 helper 路径仍保留 warn-降级代码作防御)。

#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;

/// 沙箱 helper 模式的 argv 标志(bwrap `--` 后第一个参数)。
pub const INNER_FLAG: &str = "--sandbox-exec-inner";

/// 需要转发给命令进程组的信号(对齐 codex FORWARDED_SIGNALS)。
const FORWARDED_SIGNALS: &[libc::c_int] =
    &[libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// 目标命令 pid(sigaction 处理器里读取)。
static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// helper 入口:参数为 `--restrict-network -- <cmd> <args...>`(INNER_FLAG 已被
/// 调用方消费)。永不返回(终以目标命令的退出码/信号退出)。
pub fn run_inner(mut args: impl Iterator<Item = std::ffi::OsString>) -> ! {
    // 参数协议:--restrict-network -- <cmd> <args...>
    match args.next().as_deref() {
        Some(flag) if flag == "--restrict-network" => {}
        other => {
            eprintln!(
                "cortex sandbox-exec: expected --restrict-network as first argument, got {other:?}"
            );
            std::process::exit(125);
        }
    }
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        eprintln!("cortex sandbox-exec: expected -- before the user command");
        std::process::exit(125);
    }
    let Some(program) = args.next() else {
        eprintln!("cortex sandbox-exec: missing user command after --");
        std::process::exit(125);
    };
    let rest: Vec<std::ffi::OsString> = args.collect();

    // 1. 应用 seccomp:失败 fail-closed(退出而非裸跑——不能让命令在无过滤下执行)
    if let Err(e) = adk_sandbox::sandbox::linux_seccomp::apply_seccomp_filter(
        adk_sandbox::sandbox::linux_seccomp::NetworkFilterMode::Restricted,
    ) {
        eprintln!("cortex sandbox-exec: failed to apply seccomp filter: {e}");
        std::process::exit(126);
    }

    // 2. fork + reaper 循环(对齐 codex linux_run_main 的 inner stage)。
    //    fork 前屏蔽转发信号,避免 fork 竞态窗口内丢信号;子进程 exec 前恢复。
    unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        for sig in FORWARDED_SIGNALS {
            libc::sigaddset(&mut blocked, *sig);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()) < 0 {
            eprintln!("cortex sandbox-exec: failed to block signals before fork");
            std::process::exit(126);
        }

        let pid = libc::fork();
        if pid < 0 {
            eprintln!(
                "cortex sandbox exec: fork failed: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(126);
        }

        if pid == 0 {
            // 子进程:恢复信号屏蔽后 exec 目标命令(seccomp 随之继承)。
            // 刻意不 setsid(对齐 codex):命令必须留在 helper 的进程组里,
            // 外层超时 kill(-pgid, SIGKILL) 才能直达命令;信号转发也依赖同组。
            // exec 失败按惯例 127。
            let mut previous: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &mut previous);
            for sig in FORWARDED_SIGNALS {
                libc::sigdelset(&mut previous, *sig);
            }
            libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
            let err = std::process::Command::new(&program).args(&rest).exec();
            eprintln!(
                "cortex sandbox-exec: exec '{}' failed: {err}",
                program.to_string_lossy()
            );
            std::process::exit(127);
        }

        // 父进程(本进程):装信号转发器后恢复屏蔽,再进 reaper 循环。
        let child_pid = pid;
        install_signal_forwarders(child_pid);
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &mut previous);
        for sig in FORWARDED_SIGNALS {
            libc::sigdelset(&mut previous, *sig);
        }
        libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());

        loop {
            let mut status: libc::c_int = 0;
            let reaped = libc::waitpid(-1, &mut status, 0);
            if reaped == child_pid {
                exit_with_wait_status(status);
            }
            if reaped >= 0 {
                continue; // 其他后代(孤儿)——收掉继续
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                eprintln!("cortex sandbox-exec: waitpid failed: {err}");
                std::process::exit(1);
            }
        }
    }
}

/// 转发信号到命令进程组(先杀组再杀 pid,覆盖命令尚未/已经改变进程组归属的形态)。
extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(-pid, sig);
            libc::kill(pid, sig);
        }
    }
}

fn install_signal_forwarders(pid: libc::pid_t) {
    use std::sync::atomic::Ordering::SeqCst;
    CHILD_PID.store(pid, SeqCst);
    for sig in FORWARDED_SIGNALS {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = forward_signal as *const () as libc::sighandler_t;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(*sig, &action, std::ptr::null_mut());
        }
    }
}

/// 用 wait status 原样退出(对齐 codex exit_with_wait_status):正常退出透传
/// 退出码;信号死恢复默认处理器后 self-raise 同信号(让父链路拿到真实信号死因)。
fn exit_with_wait_status(status: libc::c_int) -> ! {
    unsafe {
        if libc::WIFEXITED(status) {
            std::process::exit(libc::WEXITSTATUS(status));
        }
        if libc::WIFSIGNALED(status) {
            let sig = libc::WTERMSIG(status);
            libc::signal(sig, libc::SIG_DFL);
            libc::kill(libc::getpid(), sig);
        }
    }
    std::process::exit(1);
}
