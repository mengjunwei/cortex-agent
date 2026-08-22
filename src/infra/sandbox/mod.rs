//! 沙箱家族 — Shell 命令隔离执行、环境快照、工作区容灾、代码编译验证
//!
//! | 模块 | 说明 |
//! |------|------|
//! | [`shell_sandbox`] | bwrap 沙箱封装：shell_command 写仅 session + 读白名单 |
//! | [`sandbox_exec`] | seccomp helper 内嵌实现(Linux;由 main 入口 `--sandbox-exec-inner` 分发) |
//! | [`shell_snapshot`] | 会话级 shell 环境快照（捕获 PATH/venv，每条命令 source，避免重复探测） |
//! | [`workspace_snapshot`] | 沙箱工作目录会话亲和容灾（tar.zst 快照 + 原子解包 + tar slipping 防护） |
//! | [`code_exec`] | adk-code 封装：RustExecutor 完整管线执行（验证 Layer 3） |

pub mod code_exec;
/// 沙箱 seccomp helper 内嵌实现(Linux;由 main 入口 `--sandbox-exec-inner` 分发)。
#[cfg(target_os = "linux")]
pub mod sandbox_exec;
pub mod shell_sandbox;
pub mod shell_snapshot;
pub mod workspace_snapshot;
