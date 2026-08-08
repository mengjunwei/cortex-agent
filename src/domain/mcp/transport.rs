//! MCP 传输层适配
//!
//! 封装 rmcp 的 stdio / streamable_http 连接细节，对上层暴露统一入口 [`connect`]。
//!
//! - stdio：`TokioChildProcess` + `tokio::process::Command`（逐 arg 传递，避免 shell 注入）
//! - http：`StreamableHttpClientTransport::from_config`（支持自定义请求头）
//!
//! 连接成功后返回 `RunningService<RoleClient, ()>`，供 [`crate::domain::mcp::manager`] 缓存复用。
//! 使用 `().serve(transport)` 由 rmcp 内部完成协议握手与默认 ClientInfo 通告。

use std::collections::HashMap;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, transport::StreamableHttpClientTransport};
use tokio::process::Command;

use crate::domain::mcp::enums::TransportKind;
use crate::domain::mcp::models::McpServer;
use crate::error::AppError;

/// 连接超时（覆盖 stdio 进程启动握手 / http TLS 建连）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 按传输方式建立连接，返回已初始化（握手完成）的 RunningService
pub async fn connect(server: &McpServer) -> Result<RunningService<RoleClient, ()>, AppError> {
    match server.transport {
        TransportKind::Stdio => connect_stdio(server).await,
        TransportKind::StreamableHttp => connect_http(server).await,
    }
}

async fn connect_stdio(server: &McpServer) -> Result<RunningService<RoleClient, ()>, AppError> {
    let mut cmd = Command::new(&server.endpoint);
    for arg in &server.args {
        cmd.arg(arg);
    }
    for (k, v) in &server.env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());

    let child = TokioChildProcess::new(cmd)
        .map_err(|e| AppError::BusinessError(format!("启动 stdio MCP 进程失败: {e}")))?;

    let running = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(child))
        .await
        .map_err(|_| {
            AppError::NetworkError(format!(
                "stdio MCP 连接超时（{:.0}s）：{}",
                CONNECT_TIMEOUT.as_secs_f64(),
                server.endpoint
            ))
        })?
        .map_err(|e| {
            AppError::NetworkError(format!("stdio MCP 握手失败 (slug={}): {e}", server.slug))
        })?;
    Ok(running)
}

async fn connect_http(server: &McpServer) -> Result<RunningService<RoleClient, ()>, AppError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(server.endpoint.clone());
    if !server.headers.is_empty() {
        let headers = build_http_headers(&server.headers)?;
        config = config.custom_headers(headers);
    }

    let transport = StreamableHttpClientTransport::<reqwest::Client>::from_config(config);
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
}
