//! cortex-agent Library
//!
//! 提供 cortex-agent 的核心功能。
#![allow(clippy::collapsible_if)]

pub mod bootstrap;
pub mod config;
pub mod domain;
pub mod error;
pub mod permissions;
pub mod prompts;
pub mod security;

pub mod agent;
pub mod infra;
pub mod llm;
pub mod server;
pub mod tools;

/// 服务主入口(从 main.rs 内联主体迁出):打印横幅 → 解析 --config → 日志/OTLP →
/// 装配 AppDeps → 启动 HTTP。main.rs 只负责沙箱 helper 模式的 argv 拦截
/// (`--sandbox-exec-inner`,见 `infra::sandbox::sandbox_exec`)后转调本函数——服务逻辑
/// 进 lib 使 helper 与服务共享同一二进制(单文件自嵌,对齐 codex)。
#[tokio::main]
pub async fn server_main() -> anyhow::Result<()> {
    println!("============================================");
    println!("  cortex-agent");
    println!("============================================");
    println!();

    let m = clap::Command::new("cortex-agent")
        .author("DevOps Team")
        .version("1.0")
        .about("cortex-agent")
        .arg(
            clap::Arg::new("config")
                .long("config")
                .short('c')
                .help("config path")
                .default_value("./config/config_1.toml"),
        );

    let conf_arg = m.get_matches();
    let conf = conf_arg.get_one::<String>("config").unwrap();
    let real_conf_file = if let Ok(val) = std::env::var("CORTEX_AGENT_CONFIG") {
        val
    } else {
        conf.to_string()
    };
    let cfg = config::AppConfig::load(&real_conf_file)?;

    // ── 初始化日志 + OTLP 遥测 ──
    // 统一用 tracing-subscriber：debug 走控制台、生产走滚动文件。
    // OTLP layer 由 [log].otlp_enabled 控制（默认 true）；部署机无 OpenObserve 时
    // config 设 false 即关，不再向 5081 导出。OTLP 后端的 Authorization / organization /
    // stream-name 通过环境变量 OTEL_EXPORTER_OTLP_HEADERS 注入（调试在 .zed/debug.json
    // 的 env 里设，生产在启动脚本里设）。
    let _log_guard = infra::log_util::init_logging(&cfg.log, "cortex-agent")?;
    tracing::info!("日志与 OTLP 遥测初始化完成");
    tracing::info!("项目配置加载完成");

    // ── 装配所有跨切服务（DB / 知识库 / 设备目录 / Session / 模型供应商 / ...）──
    // 集中在 bootstrap 模块（架构 §3 Q6、§5），不内联在 server::run。
    tracing::info!("开始装配应用依赖...");
    let deps = bootstrap::build_app_deps(cfg).await?;
    tracing::info!("应用依赖装配完成");

    // ── 启动 HTTP 服务器（路由注册 + Schema 注入 + TCP 监听）──
    tracing::info!("HTTP 服务启动中...");
    server::run(deps).await?;

    adk_telemetry::shutdown_telemetry();
    Ok(())
}
