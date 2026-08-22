//! MCP 传输层适配
//!
//! 封装 rmcp 的 stdio / streamable_http 连接细节，对上层暴露统一入口 [`connect`]。
//!
//! - stdio：`TokioChildProcess` + `tokio::process::Command`（逐 arg 传递，避免 shell 注入）
//! - http：`StreamableHttpClientTransport`（支持自定义请求头；同源重定向自动跟随，
//!   兼容 Starlette/FastAPI 等框架的尾斜杠 307）
//!
//! 连接成功后返回 `RunningService<RoleClient, ()>`，供 [`crate::domain::mcp::manager`] 缓存复用。
//! 使用 `().serve(transport)` 由 rmcp 内部完成协议握手与默认 ClientInfo 通告。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use tokio::io::AsyncBufReadExt;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, transport::StreamableHttpClientTransport};
use tokio::process::Command;

use crate::domain::mcp::enums::TransportKind;
use crate::domain::mcp::models::McpServer;
use crate::error::AppError;

/// 连接超时（覆盖 stdio 进程启动握手 / http TLS 建连）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 按传输方式建立连接，返回已初始化（握手完成）的 RunningService
///
/// `inherit_env`：stdio 子进程是否继承宿主全量环境（见 [`resolve_child_env`]）。
pub async fn connect(
    server: &McpServer,
    inherit_env: bool,
) -> Result<RunningService<RoleClient, ()>, AppError> {
    match server.transport {
        TransportKind::Stdio => connect_stdio(server, inherit_env).await,
        TransportKind::StreamableHttp => connect_http(server).await,
    }
}

async fn connect_stdio(
    server: &McpServer,
    inherit_env: bool,
) -> Result<RunningService<RoleClient, ()>, AppError> {
    let mut cmd = Command::new(&server.endpoint);
    for arg in &server.args {
        cmd.arg(arg);
    }
    // 收紧模式：先 clear 再设（Command 语义：env_clear 连同覆盖表一起清，
    // 后设置的 env 才生效），子进程环境 = 白名单 ∪ server 显式 env。
    if !inherit_env {
        cmd.env_clear();
    }
    for (k, v) in resolve_child_env(inherit_env, std::env::vars(), &server.env) {
        cmd.env(k, v);
    }
    // stderr=piped 捕获子进程输出，握手失败时提取错误详情。
    // 关键：读端必须由常驻 drain 任务持有到子进程退出——若 connect 返回时 drop 读端，
    // 子进程随后写 stderr（tracing 日志）会拿到 EPIPE 直接退出（exit 1），
    // 表现为「连接成功后立刻 Transport closed」。
    let (child, stderr_opt) = TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AppError::BusinessError(format!("启动 stdio MCP 进程失败: {e}")))?;

    let tap = Arc::new(StderrTap::default());
    if let Some(stderr) = stderr_opt {
        tokio::spawn(drain_stderr(stderr, tap.clone()));
    }

    let result = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(child))
        .await
        .map_err(|_| {
            AppError::NetworkError(format!(
                "stdio MCP 连接超时（{:.0}s）：{}",
                CONNECT_TIMEOUT.as_secs_f64(),
                server.endpoint
            ))
        })
        .and_then(|r| r.map_err(|e| {
            AppError::NetworkError(format!("stdio MCP 握手失败 (slug={}): {e}", server.slug))
        }));

    match result {
        Ok(running) => Ok(running),
        Err(e) => {
            // 等子进程退出排空 stderr（最多 2s），再取缓冲内容提取错误详情
            for _ in 0..40 {
                if tap.closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let raw = {
                let b = tap.buf.lock().expect("stderr tap mutex poisoned");
                String::from_utf8_lossy(&b).into_owned()
            };
            let cleaned = extract_stderr_reason(&raw);
            if cleaned.is_empty() {
                Err(e)
            } else {
                // 取内层消息再包装：直接 format!("{e}") 会把 Display 前缀
                // （"网络请求错误: "）带进去，再包一层 NetworkError 就双重前缀了
                let inner = match &e {
                    AppError::NetworkError(m) => m.clone(),
                    _ => e.to_string(),
                };
                Err(AppError::NetworkError(format!("{inner} | 子进程: {cleaned}")))
            }
        }
    }
}

/// stderr 缓存上限（字节）：够装启动自检错误即可，防止异常子进程无限刷日志撑爆内存
const STDERR_TAP_MAX: usize = 8 * 1024;

/// 子进程 stderr 的共享读取端：drain 任务写入，握手失败路径读取
#[derive(Default)]
struct StderrTap {
    buf: std::sync::Mutex<Vec<u8>>,
    closed: std::sync::atomic::AtomicBool,
}

/// 常驻读取子进程 stderr：保活管道（防 EPIPE）+ 缓存内容 + debug 日志。
/// 读到 EOF/错误（子进程退出）时置 closed。
async fn drain_stderr(stderr: tokio::process::ChildStderr, tap: Arc<StderrTap>) {
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                tracing::debug!(target: "mcp_stdio_stderr", "{line}");
                let mut b = tap.buf.lock().expect("stderr tap mutex poisoned");
                tap_append(&mut b, &line);
            }
            Ok(None) | Err(_) => break,
        }
    }
    tap.closed.store(true, Ordering::Release);
}

/// 逐行写入 tap 缓冲（补回换行，保持原始多行格式供按行过滤），超上限静默丢弃。
fn tap_append(buf: &mut Vec<u8>, line: &str) {
    if buf.len() >= STDERR_TAP_MAX {
        return;
    }
    let remain = STDERR_TAP_MAX - buf.len();
    // 留 1 字节给换行
    let take = line.len().min(remain.saturating_sub(1));
    buf.extend_from_slice(&line.as_bytes()[..take]);
    buf.push(b'\n');
}

// ============================ stdio 子进程环境策略 ============================

/// Unix 基础环境白名单（对齐 codex `UNIX_CORE_ENV_VARS`）。
///
/// stdio MCP 子进程默认**不继承**宿主全量环境——cortex-agent 进程环境里的
/// LLM API key / DB 密码 / 对象存储凭证会被同一进程派生的所有 MCP server
/// 无差别读到（MCP server 是用户自配的第三方进程，不应默认获得宿主凭证）。
/// 只透传「进程能启动」所需的最小集合；server 自身的 `env` 配置仍显式注入
/// （优先级高于白名单，可覆盖）。
#[cfg(not(target_os = "windows"))]
const CORE_ENV_VARS: &[&str] = &[
    "PATH", "SHELL", "TMPDIR", "TEMP", "TMP", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME",
    "USER",
];

/// Windows 基础环境白名单（对齐 codex `WINDOWS_CORE_ENV_VARS`）：
/// 路径解析 / 系统根 / 用户上下文 / 程序目录 / AppData / 临时目录 / pwsh 提示。
#[cfg(target_os = "windows")]
const CORE_ENV_VARS: &[&str] = &[
    "PATH", "PATHEXT", "SHELL", "COMSPEC", "SYSTEMROOT", "SYSTEMDRIVE", "USERNAME", "USERDOMAIN",
    "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "PROGRAMFILES", "PROGRAMFILES(X86)", "PROGRAMW6432",
    "PROGRAMDATA", "LOCALAPPDATA", "APPDATA", "TEMP", "TMP", "TMPDIR", "POWERSHELL", "PWSH",
];

/// 计算 stdio MCP 子进程的显式环境覆盖表（供 `Command::env` 逐项设置）。
///
/// - `inherit_env=true`（兼容开关）：只返回 server 显式 env（子进程继承宿主全量环境
///   + 这些覆盖，即收紧前的旧行为）；
/// - `inherit_env=false`（默认，收紧）：返回「宿主环境 ∩ 白名单」∪ server 显式 env，
///   调用方须先 `env_clear()` 再应用返回值，宿主其余变量（含各类凭证）不进子进程。
///
/// 白名单匹配大小写不敏感（Windows 环境变量名不区分大小写；Unix 上罕见大小写变体
/// 保守放行，泄露面仅限白名单同名变量本身）。
fn resolve_child_env<I>(
    inherit_env: bool,
    host_env: I,
    server_env: &HashMap<String, String>,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out = HashMap::new();
    if !inherit_env {
        for (k, v) in host_env {
            if CORE_ENV_VARS.iter().any(|allowed| allowed.eq_ignore_ascii_case(&k)) {
                out.insert(k, v);
            }
        }
    }
    out.extend(server_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    out
}

#[cfg(test)]
mod env_tests {
    use super::*;

    fn server_env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn tight_mode_keeps_only_whitelist_plus_server_env() {
        // 平台分流:用户主目录变量名 Windows/Unix 不同(USERPROFILE vs HOME),
        // 白名单各自只含本平台那套——本测试须按平台取对应变量,否则跨平台必挂。
        #[cfg(not(target_os = "windows"))]
        let (home_key, home_val) = ("HOME", "/home/u");
        #[cfg(target_os = "windows")]
        let (home_key, home_val) = ("USERPROFILE", "C:\\Users\\u");

        let host = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            (home_key.to_string(), home_val.to_string()),
            ("OPENAI_API_KEY".to_string(), "sk-secret".to_string()),
            ("DATABASE_URL".to_string(), "postgres://pw@db".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "aws-secret".to_string()),
        ];
        let server = server_env(&[("GITHUB_TOKEN", "gh-1"), ("PATH", "/custom/bin")]);
        let out = resolve_child_env(false, host, &server);
        // 白名单透传 + server 显式项；宿主凭证全部不进子进程
        assert_eq!(out.get("PATH").unwrap(), "/custom/bin"); // server 显式覆盖白名单值
        assert_eq!(out.get(home_key).unwrap(), home_val);
        assert_eq!(out.get("GITHUB_TOKEN").unwrap(), "gh-1");
        assert!(!out.contains_key("OPENAI_API_KEY"));
        assert!(!out.contains_key("DATABASE_URL"));
        assert!(!out.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn tight_mode_matches_whitelist_case_insensitively() {
        // 平台分流:取本平台白名单内的小写变体(PATH 跨平台同名,path 必透传);
        // 第二项用本平台专有主目录变量(HOME 仅 Unix 白名单、USERPROFILE 仅 Windows)。
        #[cfg(not(target_os = "windows"))]
        let (home_key, home_val) = ("home", "/h");
        #[cfg(target_os = "windows")]
        let (home_key, home_val) = ("userprofile", "C:\\Users\\h");

        let host = vec![
            ("path".to_string(), "/bin".to_string()),
            (home_key.to_string(), home_val.to_string()),
        ];
        let out = resolve_child_env(false, host, &HashMap::new());
        assert_eq!(out.get("path").unwrap(), "/bin");
        assert_eq!(out.get(home_key).unwrap(), home_val);
    }

    #[test]
    fn inherit_mode_returns_only_server_overrides() {
        // 兼容模式 = 收紧前旧行为：子进程继承宿主全量（调用方不 env_clear），
        // 这里只返回 server 显式覆盖
        let host = vec![("PATH".to_string(), "/usr/bin".to_string())];
        let server = server_env(&[("MY_TOOL_HOME", "/opt/tool")]);
        let out = resolve_child_env(true, host, &server);
        assert_eq!(out, server_env(&[("MY_TOOL_HOME", "/opt/tool")]));
    }
}


async fn connect_http(server: &McpServer) -> Result<RunningService<RoleClient, ()>, AppError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(server.endpoint.clone());
    if !server.headers.is_empty() {
        let headers = build_http_headers(&server.headers)?;
        config = config.custom_headers(headers);
    }

    let transport = StreamableHttpClientTransport::with_client(build_http_client(), config);
    let running = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| {
            AppError::NetworkError(format!(
                "http MCP 连接超时（{:.0}s）：{}",
                CONNECT_TIMEOUT.as_secs_f64(),
                server.endpoint
            ))
        })?
        .map_err(|e| {
            AppError::NetworkError(format!("http MCP 握手失败 (slug={}): {e}", server.slug))
        })?;
    Ok(running)
}

/// 构建 http MCP 客户端。
///
/// rmcp 3.x 默认 client 显式禁用了重定向（防止自定义头被重放到重定向目标），
/// 但 FastAPI/Starlette 系 MCP 服务在 URL 缺尾斜杠时会回 307（如 /mcp → /mcp/），
/// 不跟随就直接握手失败。这里恢复"同源跟随"：host+port+scheme 一致的
/// 307/308 自动跟随，跨 host 或 scheme 降级（https→http 明文重放认证头）不跟随
/// ——头泄漏风险只存在于跨 host / 降级场景，安全性与 rmcp 默认持平。
/// 另：custom policy 没有默认跳数上限，同 host 乒乓重定向会无限循环，
/// 故同时按 reqwest 默认策略限制最多 10 跳。
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            // previous[0] 是初始 URL（非重定向产生）；host+port+scheme 全一致才跟随
            match attempt.previous().first() {
                Some(origin)
                    if attempt.url().host_str() == origin.host_str()
                        && attempt.url().port() == origin.port()
                        && attempt.url().scheme() == origin.scheme()
                        && attempt.previous().len() < 10 =>
                {
                    attempt.follow()
                }
                _ => attempt.stop(),
            }
        }))
        .build()
        .expect("构建 reqwest client 失败")
}

fn build_http_headers(
    headers: &HashMap<String, String>,
) -> Result<HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>, AppError> {
    let mut out = HashMap::with_capacity(headers.len());
    for (k, v) in headers {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| AppError::BusinessError(format!("非法 HTTP 头名 '{k}': {e}")))?;
        let value = reqwest::header::HeaderValue::from_str(v)
            .map_err(|e| AppError::BusinessError(format!("非法 HTTP 头值 '{k}': {e}")))?;
        out.insert(name, value);
    }
    Ok(out)
}

/// 从子进程 stderr 提取关键错误信息，去掉 ANSI 码、日志噪声，并脱敏。
///
/// 处理流程：
/// 1. 剥离 ANSI 转义序列（`\x1b[...m`）
/// 2. 跳过 INFO/WARN/DEBUG 级别的日志行（只保留 ERROR 和实际错误内容）
/// 3. 脱敏 URL 中的密码和 key=value 形式的敏感字段
/// 4. 截断到 300 字符
fn extract_stderr_reason(raw: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    // 1. 剥离 ANSI 转义序列
    static RE_ANSI: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-9;]*m").expect("invalid regex")
    });

    // 2. 匹配结构化日志行的时间戳+级别前缀（如 "2026-08-18T10:52:07.017594Z  INFO xxx:"）
    static RE_LOG_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\d{4}-\d{2}-\d{2}T[\d:.]+Z\s+(INFO|WARN|DEBUG|TRACE)\s+\S+:\s*")
            .expect("invalid regex")
    });

    // 3. 脱敏：URL 中的密码
    static RE_URL_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\w+://[^:]+:)[^@]+(@)").expect("invalid regex")
    });

    // 4. 脱敏：key=value 形式的敏感字段
    static RE_KV_SECRET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(password|secret|token|passwd|pgpassword|mysql_pwd)\s*[=:]\s*\S+")
            .expect("invalid regex")
    });

    let mut result_lines = Vec::new();
    for line in raw.lines() {
        // 剥离 ANSI
        let clean = RE_ANSI.replace_all(line, "");
        let clean = clean.trim();
        if clean.is_empty() {
            continue;
        }
        // 跳过 INFO/WARN/DEBUG/TRACE 级别日志（保留 ERROR 和无前缀的错误文本）
        if let Some(caps) = RE_LOG_PREFIX.captures(clean) {
            let level = &caps[1];
            if level != "ERROR" {
                continue; // 跳过非 ERROR 级别
            }
        }
        // 脱敏
        let safe = RE_URL_PASSWORD.replace_all(clean, "${1}***${2}");
        let safe = RE_KV_SECRET.replace_all(&safe, "$1=***");
        result_lines.push(safe.into_owned());
    }

    let joined = result_lines.join(" | ");
    // 按字符截断：错误文案含中文，字节切片会切在 UTF-8 边界上 panic
    if joined.chars().count() > 300 {
        let truncated: String = joined.chars().take(297).collect();
        format!("{truncated}...")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_http_headers_parses_valid() {
        let mut m = HashMap::new();
        m.insert("X-Custom".into(), "value123".into());
        m.insert("Authorization".into(), "Bearer abc".into());
        let h = build_http_headers(&m).unwrap();
        assert_eq!(h.len(), 2);
        // HeaderName 不实现 Borrow<str>，需用 as_str() 比较
        let mut found_custom = false;
        let mut found_auth = false;
        for (k, v) in &h {
            if k.as_str() == "x-custom" {
                assert_eq!(v, "value123");
                found_custom = true;
            }
            if k.as_str() == "authorization" {
                assert_eq!(v, "Bearer abc");
                found_auth = true;
            }
        }
        assert!(found_custom, "X-Custom header missing");
        assert!(found_auth, "Authorization header missing");
    }

    #[test]
    fn build_http_headers_rejects_invalid_name() {
        let mut m = HashMap::new();
        m.insert("bad header".into(), "v".into());
        assert!(build_http_headers(&m).is_err());
    }

    #[test]
    fn build_http_headers_rejects_invalid_value() {
        let mut m = HashMap::new();
        // 换行符是 HTTP 头值的非法字符
        m.insert("X-Bad".into(), "bad\nvalue".into());
        assert!(build_http_headers(&m).is_err());
    }

    #[test]
    fn extract_strips_ansi_and_noise() {
        // 模拟真实 stderr：每行一条日志，带 ANSI 码
        let input = "\
\x1b[2m2026-08-18T10:52:07.017594Z\x1b[0m \x1b[33m WARN\x1b[0m \x1b[2mcortex_mcp\x1b[0m:\x1b[0m SMTP_* 未配置：send_email 工具将返回未配置提示
\x1b[2m2026-08-18T10:52:07.017667Z\x1b[0m \x1b[32m INFO\x1b[0m \x1b[2mcortex_mcp\x1b[0m:\x1b[0m DB 配置就绪，开始启动自检
cortex-mcp: 数据库启动自检失败: password authentication failed for user \"master\"";
        let out = extract_stderr_reason(input);
        // INFO/WARN 行被跳过
        assert!(!out.contains("SMTP"), "WARN noise kept: {out}");
        assert!(!out.contains("DB 配置就绪"), "INFO noise kept: {out}");
        // 无前缀的错误行保留
        assert!(out.contains("数据库启动自检失败"), "error lost: {out}");
        assert!(out.contains("password authentication failed"), "error detail lost: {out}");
        // ANSI 码已剥离
        assert!(!out.contains("\x1b["), "ANSI codes remain: {out}");
    }

    #[test]
    fn extract_masks_url_password() {
        let input = "cortex-mcp: 数据库启动自检失败: postgres://master:s3cretP@ss@10.54.42.105:5432/marvelnet";
        let out = extract_stderr_reason(input);
        assert!(!out.contains("s3cretP@ss"), "password leaked: {out}");
        assert!(out.contains("master:***@"), "mask missing: {out}");
    }

    #[test]
    fn extract_masks_kv_password() {
        let input = "ERROR: connection failed password=hunter2 host=localhost";
        let out = extract_stderr_reason(input);
        assert!(!out.contains("hunter2"), "password leaked: {out}");
        assert!(out.contains("password=***"), "mask missing: {out}");
    }

    #[test]
    fn extract_keeps_error_level_logs() {
        let input = "\x1b[2m2026-08-18T10:00:00Z\x1b[0m \x1b[31m ERROR\x1b[0m myapp: fatal crash\nsome error without prefix";
        let out = extract_stderr_reason(input);
        assert!(out.contains("fatal crash"), "ERROR line lost: {out}");
        assert!(out.contains("some error without prefix"), "plain error lost: {out}");
    }

    #[test]
    fn extract_truncates_long_output() {
        let long_line = format!("error: {}", "x".repeat(500));
        let out = extract_stderr_reason(&long_line);
        assert!(out.len() <= 303, "not truncated: len={}", out.len()); // 300 + "..."
        assert!(out.ends_with("..."), "missing ellipsis: {out}");
    }

    #[test]
    fn extract_truncates_multibyte_without_panic() {
        // 中文错误文案：字节切片会切在 UTF-8 边界上 panic，必须按字符截断
        let long = "数据库连接失败".repeat(200);
        let out = extract_stderr_reason(&long);
        assert!(out.chars().count() <= 300, "not truncated: {} chars", out.chars().count());
        assert!(out.ends_with("..."), "missing ellipsis");
    }

    #[test]
    fn tap_append_preserves_line_breaks() {
        // 换行必须保留：否则多行拼成一行，按行过滤会整块误杀
        let mut buf = Vec::new();
        tap_append(&mut buf, "2026-08-18T10:52:07Z  INFO cortex_mcp: DB 配置就绪");
        tap_append(&mut buf, "cortex-mcp: 数据库启动自检失败: password authentication failed");
        assert_eq!(buf.iter().filter(|&&c| c == b'\n').count(), 2);
        let text = String::from_utf8(buf).unwrap();
        let kept = extract_stderr_reason(&text);
        assert!(!kept.contains("DB 配置就绪"), "INFO noise kept: {kept}");
        assert!(kept.contains("数据库启动自检失败"), "error lost: {kept}");
    }

    #[test]
    fn tap_append_caps_buffer() {
        let mut buf = Vec::new();
        for i in 0..1000 {
            tap_append(&mut buf, &format!("line-{i:04}"));
        }
        assert!(buf.len() <= STDERR_TAP_MAX, "cap exceeded: {}", buf.len());
        // 前 8KB 的行还在
        assert!(String::from_utf8_lossy(&buf).starts_with("line-0000"));
    }
}
