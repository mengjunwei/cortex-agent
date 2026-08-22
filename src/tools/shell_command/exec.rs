//! Shell 命令执行管道 — spawn/流式读/超时/输出解码(从 mod.rs 拆出)。

use super::*;

pub(super) async fn execute_command(
    root: &std::path::Path,
    cmd: &str,
    timeout_ms: u64,
    cancel_token: &CancellationToken,
    snapshot: Option<&std::path::Path>,
    extra_env: &[(String, String)],
) -> Value {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return json!({ "ok": false, "error": "Command cannot be empty" });
    }

    // Unix: source 会话 shell 快照（PATH/venv），失败静默不阻断。Windows 无等价机制（venv 激活走
    // activate.bat），跳过。snapshot 为 None 时无前缀，行为与原先一致。
    let prefix = if cfg!(unix) {
        crate::infra::sandbox::shell_snapshot::source_prefix(snapshot)
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
    // 助手级环境变量：白名单之后注入（可覆盖 PATH 等，等同真实 shell 语义）。
    // 让 skill 脚本等能经 os.environ['KEY'] 读到助手配置的变量。
    for (key, val) in extra_env {
        command.env(key, val);
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
pub(super) async fn read_capped_into<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    buf: &mut Vec<u8>,
    cap: usize,
) {
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

/// 沙箱拒绝特征检测 + hint 生成（仅沙箱执行分支调用，即 ReadOnly/WorkspaceWrite 模式）。
///
/// 非零退出 + 输出含 EROFS / 只读根派生报错等沙箱典型特征时，在结果尾部追加一段英文 hint
/// 告知模型**当前模式下**的出路。对齐 Claude Code 的做法——命令因沙箱被拒而失败时把
/// 违规详情追加到失败输出里，模型一眼看出是沙箱挡的、下一步该干什么，而不是对着
/// "Read-only file system" / "no valid pipe path found" 反复试错浪费轮次。
///
/// 文案按 sandbox 模式区分：WorkspaceWrite 下 $TMPDIR/$HOME 已重定向可写（指引往那写）；
/// ReadOnly 下**没有任何可写位置**（指引告知用户换模式），不能给错方向让模型再撞一次。
///
/// 特征串刻意收窄（EROFS 类）：`Permission denied` 等歧义报错不命中，避免误报污染正常失败。
pub(super) fn truncate_str(s: &str, max: usize) -> String {
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
pub(super) fn decode_console_output(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    let (cow, _encoding, _had_errors) = encoding_rs::GBK.decode(raw);
    cow.into_owned()
}

