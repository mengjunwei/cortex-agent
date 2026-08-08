//! 日志与遥测初始化模块 — 统一基于 `tracing` 生态
//!
//! 之前使用 `log4rs` 抢占 `log` crate 的全局 logger，与 `adk-telemetry`
//! 内部安装的 `tracing-log` LogTracer 冲突，会在初始化时 panic
//! （`SetLoggerError`）。这里改为：
//!
//! - `tracing-subscriber` 作为唯一 subscriber，统一接收 `tracing` + `log` 事件
//!   （依赖 `tracing-log` feature，由 tracing-subscriber 内部处理 LogTracer 安装，
//!   **绝不**手动调用 `LogTracer::init`，否则会和 subscriber 的 `init()` 重复注册 panic）
//! - 控制台层：开发模式输出到 stdout
//! - 文件层：生产模式按天滚动写入 `{log_path}/nm_agent.log.*`
//! - OTLP 层：通过 `adk_telemetry::build_otlp_layer` 接入，导出到 OpenObserve
//! - 噪音过滤：reqwest / hyper / sqlx / diesel / fred 等限制到 warn
//!
//! ## 用法
//!
//! ```ignore
//! let _guard = init_logging(&cfg.log, "cortex-agent", "http://127.0.0.1:5081")?;
//! // _guard 持有 tracing-appender 的 non-blocking worker，drop 时会 flush 文件
//! ```

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LogConfig;

/// 初始化日志 + OTLP 遥测，返回文件 worker 守卫（必须保活到 main 退出，否则文件可能丢日志）。
///
/// - `log_cfg`：来自配置文件的日志配置（level / path / debug 标志）
/// - `service_name`：上报到 OTLP 的 `service.name` 资源属性
/// - `otlp_endpoint`：OTLP gRPC 端点，例如 `http://127.0.0.1:5081`
///
/// 行为：
/// - 始终注册 `EnvFilter`（优先读 `RUST_LOG`，否则按 `log_cfg.level`），并把
///   reqwest / hyper / sqlx / fred / diesel 等噪音库压到 warn
/// - 始终注册 OTLP layer（向 OpenObserve 推送 spans + metrics）
/// - `log_cfg.debug = true` 时增加控制台 fmt 层；否则增加按天滚动文件层
/// - 用 `try_init()` 替代 `init()`：避免重复初始化（例如集成测试反复调用）时 panic
pub fn init_logging(
    log_cfg: &LogConfig,
    service_name: &str,
) -> anyhow::Result<Option<WorkerGuard>> {
    let env_filter = build_env_filter(&log_cfg.level);

    // OTLP layer 条件注册：otlp_enabled=false（部署机无 OTLP 后端）时完全不构建，
    // 避免无谓向 otlp_endpoint 导出失败。用 Option<Layer> 保持 registry 类型统一
    // （tracing-subscriber 为 Option<L: Layer> 实现了 no-op Layer）。
    let otlp_layer = if log_cfg.otlp_enabled {
        Some(
            adk_telemetry::build_otlp_layer(service_name, &log_cfg.otlp_endpoint)
                .map_err(|e| anyhow::anyhow!("build OTLP layer failed: {e}"))?,
        )
    } else {
        None
    };

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(otlp_layer);

    if log_cfg.debug {
        // 控制台输出（开发模式）
        let console_layer = fmt::layer()
            .with_target(true)
            .with_thread_ids(false)
            .with_line_number(true)
            .with_ansi(true);
        registry
            .with(console_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("init tracing subscriber failed: {e}"))?;
        Ok(None)
    } else {
        // 文件输出（生产模式）：按天滚动，文件名前缀 nm_agent.log
        let file_appender = rolling::daily(&log_cfg.path, "nm_agent.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let file_layer = fmt::layer()
            .with_target(true)
            .with_line_number(true)
            .with_ansi(false)
            .with_writer(non_blocking);
        registry
            .with(file_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("init tracing subscriber failed: {e}"))?;
        Ok(Some(guard))
    }
}

/// 构建 EnvFilter：
/// - 优先读 `RUST_LOG` 环境变量
/// - 否则用配置里的级别作为默认值
/// - 始终把噪音库降到 warn
fn build_env_filter(default_level: &str) -> EnvFilter {
    let default_level = match default_level.to_uppercase().as_str() {
        "TRACE" => "trace",
        "DEBUG" => "debug",
        "WARN" => "warn",
        "ERROR" => "error",
        _ => "info",
    };

    let base = format!(
        "{default_level},reqwest=warn,hyper=warn,hyper_util=warn,sqlx=warn,sqlx_core=warn,fred=warn,diesel=warn,h2=warn,rustls=warn,tower=warn"
    );

    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(base))
}
