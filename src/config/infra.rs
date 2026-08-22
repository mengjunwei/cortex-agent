//! 基础设施配置段 — `[db]` / `[redis]` / `[log]` / `[server]`

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
