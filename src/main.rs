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

fn main() -> anyhow::Result<()> {
    // ── 沙箱 helper 模式(必须在一切初始化之前,含 tokio runtime)──
    // bwrap 命令链 `bwrap <fs> -- cortex-agent --sandbox-exec-inner --restrict-network -- cmd`
    // 会在沙箱内 re-exec 本二进制:这里以 argv[1] 拦截,零初始化开销直接进入 helper
    // (NNP+seccomp → fork → reaper 循环,永不返回)。对齐 codex codex-linux-sandbox
    // 单二进制自嵌模式——部署零额外文件,不存在"helper 漏部署降级"。
    // 放在 #[tokio::main] 之外:helper 是纯阻塞式 fork/waitpid 循环,不需要 runtime;
    // 也避免在多线程 runtime 里 fork(虽有 exec 兜底,能不碰就不碰)。
    #[cfg(target_os = "linux")]
    {
        let mut argv = std::env::args_os();
        let _argv0 = argv.next();
        if argv.next().as_deref()
            == Some(std::ffi::OsStr::new(
                cortex_agent::infra::sandbox::sandbox_exec::INNER_FLAG,
            ))
        {
            cortex_agent::infra::sandbox::sandbox_exec::run_inner(argv);
        }
    }

    cortex_agent::server_main()
}
