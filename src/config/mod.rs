//! 应用配置模块 — 从 TOML 文件加载并解析全局配置
//!
//! 配置文件结构见 `config/config.toml`（`AppConfig::load` 只读 TOML，不再支持
//! 环境变量覆盖业务配置项；启动配置路径由 `CORTEX_AGENT_CONFIG` / `--config` 指定）。
//!
//! 注：LLM 模型、知识库均不从配置文件读取，统一由 DB 管理
//! （见 `model_provider`、`domain/knowledge` 模块）；历史 `[dify]` 配置段已移除，
//! Dify 现作为运行时 KB provider 通过 GraphQL 配置。

use anyhow::Context;
use serde::Deserialize;

/// PostgreSQL 数据库连接配置（`[db]` 段）
#[derive(Debug, Clone, Deserialize)]
pub struct DbConfig {
    /// 数据库类型：postgres/mysql
    pub db_type: String,
    /// 数据库主机地址
    pub host: String,
    /// 数据库端口
    pub port: u16,
    /// 数据库密码
    pub password: String,
    /// 数据库用户名
    pub user: String,
    /// 数据库名称
    pub db: String,
    /// 连接超时（秒），默认 10
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    /// 语句执行超时（秒），默认 30
    #[serde(default = "default_statement_timeout")]
    pub statement_timeout: u64,
    /// 连接池最大连接数，默认 10
    #[serde(default = "default_pool_max_size")]
    pub pool_max_size: u32,
    /// 连接池获取连接超时（秒），默认 5
    #[serde(default = "default_pool_timeout")]
    pub pool_timeout: u64,
}

fn default_connect_timeout() -> u64 {
    10
}
fn default_statement_timeout() -> u64 {
    30
}
fn default_pool_max_size() -> u32 {
    10
}
fn default_pool_timeout() -> u64 {
    5
}

impl DbConfig {
    /// 生成数据库连接URL字符串（包含超时参数）
    pub fn url(&self) -> String {
        let db_type = self.db_type.to_lowercase();

        // URL 编码用户名和密码，处理特殊字符
        let user = urlencoding::encode(&self.user);
        let password = urlencoding::encode(&self.password);

        let connection_url = match db_type.as_str() {
            "postgres" | "postgresql" => {
                format!(
                    "postgres://{}:{}@{}:{}/{}?connect_timeout={}&statement_timeout={}",
                    user,
                    password,
                    self.host,
                    self.port,
                    self.db,
                    self.connect_timeout,
                    self.statement_timeout * 1000 // PostgreSQL 使用毫秒
                )
            }
            "mysql" => {
                format!(
                    "mysql://{}:{}@{}:{}/{}?connect_timeout={}&wait_timeout={}",
                    user,
                    password,
                    self.host,
                    self.port,
                    self.db,
                    self.connect_timeout,
                    self.statement_timeout
                )
            }
            _ => {
                panic!("不支持的数据库类型: {}", self.db_type)
            }
        };

        tracing::info!(
            "[db] 生成的连接字符串: postgres://{}:***@{}:{}/{}",
            user,
            self.host,
            self.port,
            self.db
        );
        connection_url
    }
}

/// Redis 连接配置（`[redis]` 段）
#[derive(Debug, Clone, Deserialize)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub password: String,
}

impl RedisConfig {
    /// 生成 Redis 连接 URL（密码为空时不包含认证信息）
    ///
    /// 密码做 URL 编码，处理 `@`、`:`、`/`、`#`、`*` 等特殊字符，
    /// 避免 URL 解析器误解析（与数据库 URL 处理方式一致）。
    pub fn url(&self) -> String {
        if self.password.is_empty() {
            format!("redis://{}:{}/", self.host, self.port)
        } else {
            let password = urlencoding::encode(&self.password);
            format!("redis://:{}@{}:{}/", password, self.host, self.port)
        }
    }
}

/// 日志配置（`[log]` 段）
#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    /// 是否为 debug 模式（true=控制台输出，false=文件输出）
    pub debug: bool,
    /// 日志文件目录
    pub path: String,
    /// 日志级别：DEBUG / INFO / ERROR
    pub level: String,
    /// 是否启用 OTLP 遥测导出（上报到 OpenObserve 等）。默认 true。
    /// 部署机无 OTLP 后端时设 false——避免无谓地向 otlp_endpoint 导出失败。
    #[serde(default = "default_otlp_enabled")]
    pub otlp_enabled: bool,
    /// OTLP gRPC 端点（如 `http://127.0.0.1:5081`）。默认本机 OpenObserve。
    #[serde(default = "default_otlp_endpoint")]
    pub otlp_endpoint: String,
}

fn default_otlp_enabled() -> bool {
    true
}

fn default_otlp_endpoint() -> String {
    "http://127.0.0.1:5081".to_string()
}

/// HTTP 服务器配置（`[server]` 段）
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// 监听端口，默认 8090
    #[serde(default = "default_port")]
    pub port: String,
}

fn default_port() -> String {
    "8090".to_string()
}

/// 知识库配置（`[kb]` 段）— 多 provider 知识库的全局设置。
///
/// Qdrant 连接（内置 provider 的向量后端）+ 内置实例默认切片/检索参数
/// （前端新建内置实例时作表单默认值）。
#[derive(Debug, Clone, Deserialize)]
pub struct KbConfig {
    /// Qdrant gRPC 地址
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    /// Qdrant API Key（无鉴权留空）
    #[serde(default)]
    pub qdrant_api_key: String,
    #[serde(default = "default_kb_chunk_size")]
    pub default_chunk_size: usize,
    #[serde(default = "default_kb_chunk_overlap")]
    pub default_chunk_overlap: usize,
    #[serde(default = "default_kb_top_k")]
    pub default_top_k: usize,
    #[serde(default = "default_kb_similarity")]
    pub default_similarity_threshold: f64,
}

impl Default for KbConfig {
    fn default() -> Self {
        Self {
            qdrant_url: default_qdrant_url(),
            qdrant_api_key: String::new(),
            default_chunk_size: default_kb_chunk_size(),
            default_chunk_overlap: default_kb_chunk_overlap(),
            default_top_k: default_kb_top_k(),
            default_similarity_threshold: default_kb_similarity(),
        }
    }
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".to_string()
}
fn default_kb_chunk_size() -> usize {
    1024
}
fn default_kb_chunk_overlap() -> usize {
    100
}
fn default_kb_top_k() -> usize {
    6
}
fn default_kb_similarity() -> f64 {
    0.35
}

/// Context 治理配置（`[context]` 段）
///
/// 上下文压缩对齐 codex：**仅按模型 context_window 触发**（软闸 ×0.9 / 硬闸 ×0.95 / 提醒 ×0.15，
/// 比例固化为常量见 `cortex_agent`），不再按轮数 / 单轮 token 阈值压缩。
/// 本段只保留与窗口压缩无关或为其兜底的通用项。
#[derive(Debug, Clone, Deserialize)]
pub struct ContextConfig {
    /// token 估算：字符/token 比率，默认 4（中英文混合合理值）
    #[serde(default = "default_chars_per_token")]
    pub chars_per_token: u32,
    /// 工具输出截断阈值（字节），默认 48KB
    #[serde(default = "default_tool_max_output_bytes")]
    pub tool_max_output_bytes: usize,
    /// 模型未配 context_window 时的回退窗口（token），默认 128000
    #[serde(default = "default_fallback_context_window")]
    pub fallback_context_window: usize,
    /// 压缩专用便宜模型 id（None=用主模型），默认 None
    #[serde(default)]
    pub compact_model_id: Option<String>,
    /// spawn_agent 最大嵌套深度（顶层=0，每 spawn 子 agent +1），防失控递归，默认 3
    #[serde(default = "default_max_spawn_depth")]
    pub max_spawn_depth: u32,
    /// 同时运行的最大子 agent 数（对齐 codex AgentExecutionLimiter），0=不限，默认 4
    #[serde(default = "default_max_concurrent_children")]
    pub max_concurrent_children: usize,
}

fn default_chars_per_token() -> u32 {
    4
}
fn default_tool_max_output_bytes() -> usize {
    48 * 1024
}
fn default_fallback_context_window() -> usize {
    128_000
}
fn default_max_spawn_depth() -> u32 {
    3
}
fn default_max_concurrent_children() -> usize {
    // 3：兼容弱模型（如 mimo-v2.5-pro）的稳定并行上限——实测 6 个长任务会让主 agent 复读退化。
    // 强模型（如 glm-5.2）靠并发排队分批仍可完成更多子任务。
    3
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            chars_per_token: default_chars_per_token(),
            tool_max_output_bytes: default_tool_max_output_bytes(),
            fallback_context_window: default_fallback_context_window(),
            compact_model_id: None,
            max_spawn_depth: default_max_spawn_depth(),
            max_concurrent_children: default_max_concurrent_children(),
        }
    }
}

/// 安全配置（`[security]` 段）
///
/// 用于敏感数据静态加密。当前仅用于模型供应商 API Key 的 AES-256-GCM 加密。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SecurityConfig {
    /// AES-256 加密密钥（支持 base64 编码的 32 字节，或任意长度口令自动补齐/截断到 32 字节）。
    /// 可通过环境变量 `MODEL_AES_KEY` 覆盖。
    /// 若留空，启动时会随机生成临时密钥（重启后已加密的 Key 将无法解密，仅用于首次体验）。
    #[serde(default)]
    pub aes_key: String,
}

/// Skill（Agent 技能）配置（`[skill]` 段）
///
/// Skill 能力随数据库启用而始终启用（无独立开关）。详见 docs/design/skill-management.md §8。
///
/// 物化根目录由 `AppConfig.data_dir` 统一管理（`{data_dir}/skills`），此处不再保留 root_dir。
#[derive(Debug, Clone, Deserialize)]
pub struct SkillConfig {
    /// 单个 skill body 注入到对话的最大字符数
    #[serde(default = "default_skill_max_inject_chars")]
    pub max_inject_chars: usize,
    /// skill 目录占上下文窗口的百分比(默认 2)
    #[serde(default = "default_catalog_token_budget_pct")]
    pub catalog_token_budget_pct: u8,
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            max_inject_chars: default_skill_max_inject_chars(),
            catalog_token_budget_pct: default_catalog_token_budget_pct(),
        }
    }
}

fn default_skill_max_inject_chars() -> usize {
    1500
}
fn default_catalog_token_budget_pct() -> u8 {
    2
}

/// 默认 OIDC scope
fn default_oidc_scope() -> String {
    "openid profile email".to_string()
}

/// 默认 JWT 有效期：24 小时
fn default_jwt_ttl() -> i64 {
    86_400
}

/// 默认 Cookie 名称
fn default_cookie_name() -> String {
    "cortex_session".to_string()
}

/// 单个身份认证提供商配置（`[[auth.providers]]` 数组项）
///
/// 支持同类型多实例（如两个 OIDC），通过 `kind:name` 复合键区分。
/// `client_secret_enc` 约定以 `enc:` 前缀标识已用 AesCodec 加密；
/// 不带前缀时视为明文（仅限开发调试，生产环境务必加密）。
#[derive(Debug, Clone, Deserialize)]
pub struct AuthProviderConfig {
    /// 提供商类型：`feishu` / `wechat` / `oidc`
    pub kind: String,
    /// 实例展示名称（前端展示 + 实例键的一部分）
    pub name: String,
    /// OAuth client_id / app_id
    pub client_id: String,
    /// OAuth client_secret（密文，`enc:` 前缀表示 AesCodec 加密）
    #[serde(default)]
    pub client_secret_enc: String,
    /// 回调 URI（需与第三方后台配置一致）
    pub redirect_uri: String,

    // ---- OIDC 专用（feishu/wechat 忽略）----
    /// OIDC issuer URL（用于 `/.well-known/openid-configuration` 自动发现）
    #[serde(default)]
    pub issuer: String,
    /// 手动覆盖 authorize endpoint（留空时走 OIDC discovery）
    #[serde(default)]
    pub authorize_url: String,
    /// 手动覆盖 token endpoint
    #[serde(default)]
    pub token_url: String,
    /// 手动覆盖 userinfo endpoint
    #[serde(default)]
    pub userinfo_url: String,
    /// OIDC scope（默认 `openid profile email`）
    #[serde(default = "default_oidc_scope")]
    pub scope: String,
}

/// 认证配置（`[auth]` 段）
///
/// 支持配置多个身份提供商并存，全部注册到 ProviderRegistry。
/// 认证**始终启用**（数据库可用时即生效，不可关闭），避免 `enabled` 开关被误触关闭。
/// `providers` 为空时仅本地用户名密码登录（首个注册用户自动成为管理员）。
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// JWT 签名密钥（HS256，至少 32 字节）
    #[serde(default)]
    pub jwt_secret: String,
    /// JWT 有效期（秒），默认 86400
    #[serde(default = "default_jwt_ttl")]
    pub token_ttl_secs: i64,
    /// 会话 Cookie 名称
    #[serde(default = "default_cookie_name")]
    pub cookie_name: String,
    /// 身份提供商列表（支持多实例并存）
    #[serde(default)]
    pub providers: Vec<AuthProviderConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: String::new(),
            token_ttl_secs: default_jwt_ttl(),
            cookie_name: default_cookie_name(),
            providers: Vec::new(),
        }
    }
}

/// 代码助手配置（`[workspace]` 段）— session 沙箱与工具开关
///
/// 注：原 Git workspace（clone/pull）基础设施已移除，
/// 代码助手统一使用 session 级临时沙箱目录（`{data_dir}/workspaces/sessions/{session_id}/`）。
/// `data_dir` 由 `AppConfig.data_dir` 统一管理，此处不再保留独立的 data_dir 字段。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    /// 是否为代码助手会话自动创建临时沙箱目录
    ///
    /// 开启后，代码助手会话在首次运行时会在 `{data_dir}/workspaces/sessions/{session_id}/`
    /// 创建一个临时目录作为沙箱，会话删除时同步清理。
    /// 关闭则降级为 T0 聊天档（纯对话，无文件工具）。
    #[serde(default = "default_enable_session_sandbox")]
    pub enable_session_sandbox: bool,
}

fn default_enable_session_sandbox() -> bool {
    true
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            enable_session_sandbox: default_enable_session_sandbox(),
        }
    }
}

/// Shell 命令工具配置（`[shell]` 段）
#[derive(Debug, Clone, Deserialize)]
pub struct ShellConfig {
    /// 命令执行默认超时（毫秒）
    #[serde(default = "default_shell_default_timeout_ms")]
    pub default_timeout_ms: u64,
    /// 命令执行超时上限（毫秒）
    #[serde(default = "default_shell_max_timeout_ms")]
    pub max_timeout_ms: u64,
    /// 审批等待超时（秒，用户不响应时自动拒绝）
    #[serde(default = "default_shell_approval_timeout_secs")]
    pub approval_timeout_secs: u64,
    /// 文件系统沙箱模式（对齐 codex `sandbox_mode`，默认 workspace-write）。
    /// B 阶段为策略层（决定命令分类/审批）；C/D 阶段升级为真 OS 强制。
    #[serde(default)]
    pub sandbox_mode: crate::domain::permissions::SandboxMode,
    /// 命令审批策略（对齐 codex `approval_policy`，默认 unless-trusted）。
    #[serde(default)]
    pub approval_policy: crate::domain::permissions::ApprovalPolicy,
    /// 是否允许命令访问网络（默认 false，对齐 codex：默认禁网，需联网装包时显式开启）。
    /// 填充 prompt 的 `{{ network_access }}` 占位符，由 OS 沙箱强制（Linux bwrap unshare-net）。
    /// 安全：bwrap 网络只能全开/全断（无域级隔离），默认关网是防凭证外泄的关键一道闸——
    /// 真正防外泄依赖「默认关网 + safety 层命令名拦截 + 用户审批」多层叠加。
    #[serde(default)]
    pub network_access: bool,
}

impl ShellConfig {
    /// 聚合为 [`crate::domain::permissions::PermissionPolicy`]，便于在会话/工具间传递。
    pub fn permission_policy(&self) -> crate::domain::permissions::PermissionPolicy {
        crate::domain::permissions::PermissionPolicy::new(
            self.sandbox_mode,
            self.approval_policy,
            self.network_access,
        )
    }
}

fn default_shell_default_timeout_ms() -> u64 {
    30000
}
fn default_shell_max_timeout_ms() -> u64 {
    120000
}
fn default_shell_approval_timeout_secs() -> u64 {
    120
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: default_shell_default_timeout_ms(),
            max_timeout_ms: default_shell_max_timeout_ms(),
            approval_timeout_secs: default_shell_approval_timeout_secs(),
            sandbox_mode: Default::default(),
            approval_policy: Default::default(),
            network_access: false,
        }
    }
}

/// MCP 配置（`[mcp]` 段）— 预配置 MCP 服务器种子
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// 预配置 MCP 服务器列表（启动时 upsert 到 DB）
    #[serde(default)]
    pub seeds: Vec<McpSeedConfig>,
}

/// 单个 MCP 种子配置
#[derive(Debug, Clone, Deserialize)]
pub struct McpSeedConfig {
    /// 唯一标识（slug，用于 DB upsert 匹配）
    pub slug: String,
    /// 显示名称
    pub name: String,
    /// 传输方式：1=stdio, 2=streamable_http
    #[serde(default = "default_mcp_transport")]
    pub transport: i16,
    /// stdio: 命令路径; http: URL
    pub endpoint: String,
    /// 启动参数（JSON 数组字符串）
    #[serde(default = "default_mcp_args")]
    pub args: String,
    /// 单次工具调用超时（秒），缺省 60
    #[serde(default)]
    pub tool_timeout_secs: Option<i64>,
}

fn default_mcp_transport() -> i16 {
    1
}
fn default_mcp_args() -> String {
    "[]".to_string()
}

/// 助手配置 — 对应 `config.toml` 的 `[assistant]` 段（当前无配置项，保留占位以便后续扩展）
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AssistantConfig {}

/// 对象存储配置(`[object_storage]` 段)— S3 兼容,接 RustFS / MinIO / AWS S3
///
/// 用于截图 / 上传图 / artifact / 沙箱快照的共享存储(6+ 节点负载均衡场景)。
/// 详见 `docs/superpowers/specs/2026-08-04-object-storage-ha-design.md`。
#[derive(Debug, Clone, Deserialize)]
pub struct ObjectStorageConfig {
    /// 是否启用对象存储(默认 true)。生产多节点必须启用并配齐连接参数。
    #[serde(default = "default_os_enabled")]
    pub enabled: bool,
    /// S3 endpoint,如 `http://rustfs:9000`
    #[serde(default)]
    pub endpoint: String,
    /// region(默认 us-east-1)
    #[serde(default = "default_os_region")]
    pub region: String,
    /// bucket 名
    #[serde(default)]
    pub bucket: String,
    /// access key(敏感,不入日志)
    #[serde(default)]
    pub access_key: String,
    /// secret key(敏感,不入日志)
    #[serde(default)]
    pub secret_key: String,
    /// path-style 访问(RustFS/MinIO 用 true;AWS S3 虚拟主机风格用 false),默认 true
    #[serde(default = "default_os_path_style")]
    pub path_style: bool,
    /// presigned URL 有效期(秒),默认 7 天
    #[serde(default = "default_os_presign_ttl")]
    pub presign_ttl_secs: u64,
}

impl Default for ObjectStorageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: String::new(),
            region: default_os_region(),
            bucket: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            path_style: true,
            presign_ttl_secs: default_os_presign_ttl(),
        }
    }
}

fn default_os_enabled() -> bool {
    true
}
fn default_os_region() -> String {
    "us-east-1".to_string()
}
fn default_os_path_style() -> bool {
    true
}
fn default_os_presign_ttl() -> u64 {
    7 * 24 * 3600
}

/// 应用全局配置根结构 — 对应 `config.toml` 的所有配置段
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub db: DbConfig,
    pub redis: RedisConfig,
    pub log: LogConfig,
    #[serde(default)]
    pub kb: KbConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub skill: SkillConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub assistant: AssistantConfig,
    /// 对象存储(S3/RustFS)配置
    #[serde(default)]
    pub object_storage: ObjectStorageConfig,
    /// 统一数据根目录，所有本地持久化数据的子目录都派生自此
    ///
    /// 派生目录（由 `AppConfig` 的 helper 方法计算，使用时自动创建）：
    /// - `{data_dir}/skills` — Skill 物化文件
    /// - `{data_dir}/workspaces/sessions/{session_id}` — 代码助手会话沙箱
    /// - `{data_dir}/artifacts` — ADK Artifact 持久化
    /// - `{data_dir}/screenshots` — 浏览器截图
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

fn default_data_dir() -> String {
    "./data".to_string()
}

/// 路径段安全校验:拒空 / `/` / `\` / `..` / `:`。
///
/// 用于 [`AppConfig::workspace_session_dir`] / [`AppConfig::screenshot_session_dir`] 派生目录时
/// 净化 `session_id` 等外部(客户端可控)输入,防 `Path::join` 路径穿越导致任意目录删除/写。
pub fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 256
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains("..")
        && !s.contains(':')
}

impl AppConfig {
    // ── 目录派生 helper ──
    // 所有子目录统一从 data_dir 派生，避免散落的硬编码路径

    /// Skill 物化根目录：`{data_dir}/skills`
    pub fn skill_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("skills")
    }

    /// 代码助手会话沙箱目录：`{data_dir}/workspaces/sessions/{session_id}`
    ///
    /// `session_id` 经 [`is_safe_path_segment`] 净化,异常(含 `/` `\` `..`)时落到专用
    /// 安全目录,绝不 join 原值——防 `Path::join` 路径穿越(任意目录删除/写)。
    pub fn workspace_session_dir(&self, session_id: &str) -> std::path::PathBuf {
        let seg = if is_safe_path_segment(session_id) {
            session_id
        } else {
            "__invalid_session__"
        };
        std::path::Path::new(&self.data_dir)
            .join("workspaces")
            .join("sessions")
            .join(seg)
    }

    /// Artifact 持久化目录：`{data_dir}/artifacts`
    pub fn artifact_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("artifacts")
    }

    /// 用户上传的图片附件目录：`{data_dir}/uploads`（与会话多模态输入配套）
    pub fn uploads_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("uploads")
    }

    /// 从 TOML 文件加载配置
    ///
    /// 加载流程：
    /// 1. 读取并解析 TOML 文件
    /// 2. 应用环境变量覆盖（LLM / Dify 相关配置）
    pub fn load(path: &str) -> Result<Self, anyhow::Error> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("读取配置文件 {} 失败", path))?;

        let cfg: AppConfig = toml::from_str(&content).with_context(|| "解析配置文件失败")?;

        Ok(cfg)
    }
}
