//! 邮件发送工具 —— 基于 lettre 异步 SMTP。
//!
//! 用 lettre（而非手搓 TLS）正是为了规避 rustls「服务器关连接未发 close_notify」被当成
//! 发送失败的问题（QQ 企业邮/163 等常见）：lettre 在收到 250 入队确认后即返回成功，
//! 连接关闭阶段的错误不会冒泡为发送失败。

use rmcp::schemars;
use serde::Deserialize;

/// SMTP 账号配置（启动时从 env 读入）。
#[derive(Clone)]
pub struct EmailConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
}

impl EmailConfig {
    /// 从环境变量构建。`SMTP_HOST` / `SMTP_USERNAME` / `SMTP_PASSWORD` 任一缺失 → `None`
    /// （调用方据此禁用 send_email 工具）。
    ///
    /// - `SMTP_PORT`：默认 465。465 → 隐式 TLS；其它端口（如 587）→ STARTTLS。
    /// - `SMTP_FROM`：默认等于 `SMTP_USERNAME`。
    pub fn from_env() -> Option<Self> {
        let host = std::env::var("SMTP_HOST").ok()?;
        let username = std::env::var("SMTP_USERNAME").ok()?;
        let password = std::env::var("SMTP_PASSWORD").ok()?;
        let port: u16 = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(465);
        let from = std::env::var("SMTP_FROM").unwrap_or_else(|_| username.clone());
        Some(Self {
            host,
            port,
            username,
            password,
            from,
        })
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendEmailInput {
    /// Recipient addresses, comma-separated (e.g. a@x.com, b@y.com)
    pub to: String,
    /// Email subject
    pub subject: String,
    /// Plain-text body
    pub body: String,
    /// Optional HTML body (when both are given, clients render HTML and keep plain text as fallback)
    #[serde(default)]
    pub html: Option<String>,
    /// Optional CC addresses, comma-separated
    #[serde(default)]
    pub cc: Option<String>,
    /// Optional BCC addresses, comma-separated
    #[serde(default)]
    pub bcc: Option<String>,
    /// Optional list of absolute paths to local files to attach
    #[serde(default)]
    pub attachments: Option<Vec<String>>,
}

/// 实际发送。
pub async fn send(cfg: &EmailConfig, i: SendEmailInput) -> anyhow::Result<String> {
    use lettre::message::header::ContentType;
    use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    // —— 邮件头 ——
    let from: Mailbox = cfg
        .from
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid SMTP_FROM address: {e}"))?;
    let mut builder = Message::builder().from(from).subject(i.subject.clone());

    for addr in split_addrs(&i.to) {
        let m: Mailbox = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid recipient address '{addr}': {e}"))?;
        builder = builder.to(m);
    }
    if let Some(cc) = i.cc.as_deref().and_then(nonempty) {
        for addr in split_addrs(cc) {
            let m: Mailbox = addr
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid CC address '{addr}': {e}"))?;
            builder = builder.cc(m);
        }
    }
    if let Some(bcc) = i.bcc.as_deref().and_then(nonempty) {
        for addr in split_addrs(bcc) {
            let m: Mailbox = addr
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid BCC address '{addr}': {e}"))?;
            builder = builder.bcc(m);
        }
    }

    // —— 正文：multipart/alternative（纯文本 + 可选 HTML）——
    let mut alt = MultiPart::alternative().singlepart(SinglePart::plain(i.body.clone()));
    if let Some(html) = i.html.as_deref().and_then(nonempty) {
        alt = alt.singlepart(SinglePart::html(html.to_string()));
    }

    // —— 外层 mixed：正文 + 附件 ——
    let mut mixed = MultiPart::mixed().multipart(alt);
    if let Some(atts) = i.attachments.as_deref() {
        for path in atts {
            let p = std::path::Path::new(path);
            let filename = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
                .to_string();
            let content =
                std::fs::read(p).map_err(|e| anyhow::anyhow!("failed to read attachment '{path}': {e}"))?;
            // lettre 用自己的 ContentType（包裹 mime）；Attachment::body 返回 SinglePart
            let mime = mime_guess::from_path(p).first_or_octet_stream();
            let ct = ContentType::parse(mime.essence_str())
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
            mixed = mixed.singlepart(Attachment::new(filename).body(content, ct));
        }
    }

    let email = builder.multipart(mixed)?;

    // —— 传输：465 = 隐式 TLS（relay），其余 = STARTTLS ——
    let transport_builder = if cfg.port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
    }
    .map_err(|e| anyhow::anyhow!("failed to build SMTP transport ({}:{}): {e}", cfg.host, cfg.port))?;

    let transport = transport_builder
        .port(cfg.port)
        .credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()))
        .build();

    transport
        .send(email)
        .await
        .map_err(|e| anyhow::anyhow!("SMTP send failed ({}:{}): {e}", cfg.host, cfg.port))?;

    Ok(format!("sent successfully via {}:{}", cfg.host, cfg.port))
}

/// 逗号分隔的地址列表 → 去空白、去空项的迭代器。
fn split_addrs(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty())
}

/// 非空字符串返回 Some，否则 None。
fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::{nonempty, split_addrs};

    #[test]
    fn split_addrs_handles_commas_and_whitespace() {
        let v: Vec<_> = split_addrs("a@x.com, b@y.com ,, c@z.com").collect();
        assert_eq!(v, vec!["a@x.com", "b@y.com", "c@z.com"]);
        assert!(split_addrs("").collect::<Vec<_>>().is_empty());
    }

    #[test]
    fn nonempty_trims_and_detects_blank() {
        assert_eq!(nonempty("  hi  "), Some("hi"));
        assert_eq!(nonempty("   "), None);
        assert_eq!(nonempty(""), None);
    }
}
