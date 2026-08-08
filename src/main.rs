//! # cortex-agent
//!
//! 基于 RAG（检索增强生成）架构的网络设备运维智能助手，核心能力包括：
//!
//! - **助手驱动的多智能体**：会话绑定一个助手（内置 / 自定义），运行时按助手记录
//!   分发到对应 Agent（设备命令、监控插件、浏览器、通用问答、配置头脑风暴、智能路由）
//! - **设备命令查询**：按厂商/设备类型检索配置命令，生成结构化命令帮助
//! - **监控插件生成**：根据需求生成 Rhai 监控插件代码，三层自动校验后注册
//! - **知识库检索**：通过 Dify 知识库 API 实现语义检索，支持厂商/设备类型过滤
//! - **查询理解**：用 LLM 从自然语言中提取结构化检索条件（厂商、设备类型、关键词）
//! - **会话管理**：基于 PostgreSQL 持久化多轮对话，支持历史回放与上下文压缩
//! - **知识沉淀**：从完整对话中自动提取 FAQ 并写入知识库，实现持续学习
//! - **认证（SSO + 本地）**：可选启用飞书 / 微信 / OIDC 单点登录与本地账号登录
//!
//! ## 启动流程
//!
//! 组合根分离后（架构 §3 Q6），启动流程极简：
//!
//! 1. 解析命令行 + 加载配置（`--config` 或 `CORTEX_AGENT_CONFIG`，默认 `config/config_1.toml`）
//! 2. 初始化日志（debug → 控制台；生产 → 滚动文件）+ OTLP 遥测（上报 OpenObserve）
//! 3. [`bootstrap::build_app_deps`]：装配所有跨切服务（DB / 知识库 / 设备目录 /
//!    Session / Artifact / Memory / ModelProvider / QueryUnderstanding / Plugin /
//!    Browser / Redis / Session*Store / Auth / Assistant / MCP / Skill），构造 `AppDeps`
//! 4. [`server::run`]：注册路由 + 启动 Axum HTTP 服务
//!
//! ## 主要依赖
//!
//! | 组件 | 用途 |
//! |------|------|
//! | `adk-rust` | Agent 开发框架（Agent / Runner / Session / Tool / Memory / Skill） |
//! | `axum` + `async-graphql` | HTTP 服务器框架 + GraphQL 单入口 |
//! | `diesel` + `diesel-async` | PostgreSQL 异步 ORM（文档元数据 / 助手 / 供应商等映射） |
//! | `bb8-redis` | Redis 连接池（Agent 长期记忆 + 认证黑名单） |
//! | `reqwest` | HTTP 客户端（调用 Dify / LLM / OAuth endpoint） |
//! | `rhai` | 嵌入式脚本引擎（监控插件运行时） |
//!
//! > 模型（LLM）配置不再从配置文件读取，统一由数据库「模型供应商」管理
//! > （见 [`model_provider`](cortex_agent::model_provider) 模块），启动时由
//! > [`bootstrap::build_app_deps`] 装配后通过 `AppDeps` 注入（不再使用进程级全局）。

use clap::{Arg, Command};
use std::env;

use cortex_agent::bootstrap;
use cortex_agent::config::AppConfig;
use cortex_agent::server;

use adk_telemetry::shutdown_telemetry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("============================================");
    println!("  cortex-agent");
    println!("============================================");
    println!();

    let m = Command::new("cortex-agent")
        .author("DevOps Team")
        .version("1.0")
        .about("cortex-agent")
        .arg(
            Arg::new("config")
                .long("config")
                .short('c')
                .help("config path")
                .default_value("./config/config_1.toml"),
        );

    let conf_arg = m.get_matches();
    let conf = conf_arg.get_one::<String>("config").unwrap();
    let real_conf_file = if let Ok(val) = env::var("CORTEX_AGENT_CONFIG") {
        val
    } else {
        conf.to_string()
    };
    let cfg = AppConfig::load(&real_conf_file)?;

    // ── 初始化日志 + OTLP 遥测 ──
    // 统一用 tracing-subscriber：debug 走控制台、生产走滚动文件。
    // OTLP layer 由 [log].otlp_enabled 控制（默认 true）；部署机无 OpenObserve 时
    // config 设 false 即关，不再向 5081 导出。OTLP 后端的 Authorization / organization /
    // stream-name 通过环境变量 OTEL_EXPORTER_OTLP_HEADERS 注入（调试在 .zed/debug.json
    // 的 env 里设，生产在启动脚本里设）。
    let _log_guard = cortex_agent::infra::log_util::init_logging(&cfg.log, "cortex-agent")?;
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

    shutdown_telemetry();
    Ok(())
}
