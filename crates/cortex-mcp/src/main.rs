//! cortex-mcp —— cortex-agent 内置的 MCP 工具二进制（stdio）。
//!
//! 设计为**可扩展的工具集**：每个工具一个模块（`email.rs`、`db/`、未来的 `xxx.rs`），
//! `server.rs` 是工具注册表（`#[tool_router]` impl 里每加一个 `#[tool]` 方法即多一个工具）。
//! 各工具的配置从环境变量按需读取；未配置的工具不致崩溃，调用时返回「未配置」提示。
//!
//! 日志一律走 stderr，stdout 仅承载 MCP JSON-RPC。
//!
//! 退出码约定：DB_* 配置无效或数据库启动自检失败 → **exit 2**（stderr 说明原因），
//! cortex 的 MCP 探活立即转红，下次探测/使用时重新拉起进程，自愈。

mod db;
mod email;
mod influx;
mod prometheus;
mod server;

use rmcp::{transport::stdio, ServiceExt};
use server::ToolServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // 各工具按 env 自行启用；未配置 → None（工具调用时返回「未配置」提示）
    let email = email::EmailConfig::from_env();
    if email.is_none() {
        tracing::warn!("SMTP_* 未配置：send_email 工具将返回未配置提示");
    }

    // DB 三态：Ok(None)=未配置（继续 serve）；Err=配置错误；Some=自检通过
    let db = match db::DbEnv::from_env() {
        Ok(None) => {
            tracing::warn!("DB_* 未配置：db_* 工具将返回未配置提示");
            None
        }
        Err(msg) => {
            eprintln!("cortex-mcp: DB_* 配置无效: {msg}");
            std::process::exit(2);
        }
        Ok(Some(env)) => {
            let (engine_label, redacted) = (env.engine.label(), env.redacted_url.clone());
            tracing::info!(
                impl = "nyet",
                engine = engine_label,
                url = %redacted,
                max_rows = env.max_rows,
                timeout_secs = env.query_timeout.as_secs(),
                "DB 配置就绪，开始启动自检"
            );
            match db::DbTools::start(env).await {
                Ok(db) => Some(db),
                Err(e) => {
                    eprintln!(
                        "cortex-mcp: 数据库启动自检失败（{engine_label} {redacted}）: {e}"
                    );
                    std::process::exit(2);
                }
            }
        }
    };

    // InfluxDB 三态：Ok(None)=未配置（继续 serve）；Err=配置错误；Some=自检通过
    let influx = match influx::InfluxEnv::from_env() {
        Ok(None) => {
            tracing::warn!("INFLUX_* 未配置：influx_* 工具将返回未配置提示");
            None
        }
        Err(msg) => {
            eprintln!("cortex-mcp: INFLUX_* 配置无效: {msg}");
            std::process::exit(2);
        }
        Ok(Some(env)) => {
            let (version_label, target, url) = (
                env.version.label(),
                env.org
                    .clone()
                    .or_else(|| env.database.clone())
                    .unwrap_or_default(),
                env.url.clone(),
            );
            tracing::info!(
                version = version_label,
                url = %url,
                target,
                max_rows = env.max_rows,
                timeout_secs = env.query_timeout.as_secs(),
                "InfluxDB 配置就绪，开始启动自检"
            );
            match influx::InfluxTools::start(env).await {
                Ok(tools) => Some(tools),
                Err(e) => {
                    eprintln!("cortex-mcp: InfluxDB 启动自检失败（{version_label} {url}）: {e}");
                    std::process::exit(2);
                }
            }
        }
    };

    // Prometheus 三态：Ok(None)=未配置（继续 serve）；Err=配置错误；Some=自检通过
    let prom = match prometheus::PromEnv::from_env() {
        Ok(None) => {
            tracing::warn!("PROM_* 未配置：prom_* 工具将返回未配置提示");
            None
        }
        Err(msg) => {
            eprintln!("cortex-mcp: PROM_* 配置无效: {msg}");
            std::process::exit(2);
        }
        Ok(Some(env)) => {
            let url = env.url.clone();
            tracing::info!(
                url = %url,
                auth = env.token.is_some(),
                max_rows = env.max_rows,
                timeout_secs = env.query_timeout.as_secs(),
                "Prometheus 配置就绪，开始启动自检"
            );
            match prometheus::PromTools::start(env).await {
                Ok(tools) => Some(tools),
                Err(e) => {
                    eprintln!("cortex-mcp: Prometheus 启动自检失败（{url}）: {e}");
                    std::process::exit(2);
                }
            }
        }
    };

    let server = ToolServer {
        email,
        db,
        influx,
        prom,
        // 未来工具在此追加配置字段：calendar, sms, ...
    };
    tracing::info!(
        email_enabled = server.email.is_some(),
        db_enabled = server.db.is_some(),
        influx_enabled = server.influx.is_some(),
        prom_enabled = server.prom.is_some(),
        "cortex-mcp starting on stdio"
    );

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
