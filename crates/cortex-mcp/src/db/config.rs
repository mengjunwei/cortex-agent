//! env 配置解析：一条 MCP 进程 = 一个数据库连接（连接参数经环境变量注入
//! 子进程，由 cortex 侧 AES-GCM 加密落库，不进 LLM 上下文）。
//!
//! 解析产物 [`DbEnv`] 喂 nyetdb 移植版（`DB_IMPL` 仅接受 `nyet`，显式设
//! 其他值报配置错误）。护栏 / PII / 函数黑白名单等 env 见 docs/cortex-mcp.md §十二。
//!
//! 三态语义（与旧版一致）：
//! - `Ok(None)`：DB_* 完全未配置（进程照常 serve，db 工具调用时返回「未配置」提示）
//! - `Ok(Some)`：配置有效
//! - `Err(msg)`：配置错误 —— main 直接 exit 2，探活立刻红（错误文本给操作者看，中文）

use std::time::Duration;

/// 行数上限默认值与硬上限
pub const DEFAULT_MAX_ROWS: usize = 100;
pub const HARD_MAX_ROWS: usize = 1000;
/// 单条 SQL 超时默认值（秒）与硬上限
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const HARD_TIMEOUT_SECS: u64 = 300;

/// 支持的数据库引擎
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbEngine {
    MySql,
    Postgres,
    Sqlite,
}

impl DbEngine {
    pub fn label(&self) -> &'static str {
        match self {
            DbEngine::MySql => "mysql",
            DbEngine::Postgres => "postgres",
            DbEngine::Sqlite => "sqlite",
        }
    }

    /// nyet 护栏解析用的引擎标签（`Guardrail::resolve` 按它决定默认模式：
    /// postgres=cost / mysql=rows / sqlite=off；mariadb 与 mysql 同为 rows，
    /// 仅影响引擎超时变量的尝试顺序）。
    pub fn nyet_label(&self, mariadb: bool) -> String {
        match self {
            DbEngine::MySql if mariadb => "mariadb".to_string(),
            DbEngine::MySql => "mysql".to_string(),
            DbEngine::Postgres => "postgres".to_string(),
            DbEngine::Sqlite => "sqlite".to_string(),
        }
    }

    fn from_type_str(s: &str) -> Result<Self, String> {
        match s {
            "mysql" => Ok(DbEngine::MySql),
            "postgres" | "postgresql" => Ok(DbEngine::Postgres),
            "sqlite" => Ok(DbEngine::Sqlite),
            other => Err(format!("DB_TYPE 不支持: {other}（可选 mysql | postgres | sqlite）")),
        }
    }
}

/// TLS 模式（DB_SSLMODE；仅 mysql/pg 生效，sqlite 忽略）。
/// `None` = 未显式设置（用引擎默认，等价 prefer）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslChoice {
    Disable,
    Prefer,
    Required,
}

/// 连接来源的中间形态（解析用，不进 [`DbEnv`]：两种 env 形态统一推导出
/// nyet 引擎参数与脱敏 URL 后即弃）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectSpec {
    /// DB_URL 形式（密码可内嵌，推导时剥出）
    Url { url: String },
    /// DB_TYPE 元组形式（无需 percent-encode）
    Server {
        host: String,
        port: u16,
        user: String,
        password: String,
        database: String,
    },
    /// sqlite：文件路径或 `:memory:`
    Sqlite { path: String },
}

/// 数据库工具配置。
#[derive(Debug, Clone)]
pub struct DbEnv {
    pub engine: DbEngine,
    /// nyet 引擎用的 URL：密码已剥离（内嵌密码拆到 `password`），
    /// DB_SSLMODE 显式设置时注入为 query 参数。sqlite 为空串。
    pub nyet_url: String,
    /// nyet 引擎用的显式密码（DB_URL 内嵌剥离或元组 DB_PASSWORD；可缺省）
    pub password: Option<String>,
    /// nyet 引擎用的 sqlite 路径（仅 sqlite 有值）
    pub sqlite_path: Option<String>,
    /// 日志用脱敏 URL（密码打码）
    pub redacted_url: String,
    /// 行数上限：默认 100，硬上限 1000
    pub max_rows: usize,
    /// 单条 SQL 墙钟超时
    pub query_timeout: Duration,
    // ---- nyet 专属 ----
    /// `DB_MARIADB=1`：mysql 服务器实为 MariaDB 的提示（仅影响超时变量尝试顺序）
    pub mariadb: bool,
    /// DB_GUARDRAIL_MODE：cost | rows | off（默认按引擎）
    pub guardrail_mode: Option<String>,
    /// DB_GUARDRAIL_MAX_COST（默认 1_000_000.0）
    pub guardrail_max_cost: Option<f64>,
    /// DB_GUARDRAIL_MAX_ROWS（默认 10_000_000）
    pub guardrail_max_rows: Option<u64>,
    /// DB_PII：`table.column` 逗号列表
    pub pii: Vec<String>,
    /// DB_PII_MODE：deny | mask（默认 deny）
    pub pii_mode: String,
    /// DB_SQL_ALLOW_FUNCTIONS / DB_SQL_DENY_FUNCTIONS：函数黑白名单
    pub allow_functions: Vec<String>,
    pub deny_functions: Vec<String>,
}

impl DbEnv {
    /// 从进程环境变量构建（生产入口）。
    pub fn from_env() -> Result<Option<DbEnv>, String> {
        Self::from_getter(|k| std::env::var(k).ok())
    }

    /// 从任意取值器构建（单测入口，避免 env::set_var 竞态）。
    /// 取值器返回 None 表示「未设置」；Some("") 表示「显式设了空值」。
    pub fn from_getter<F>(get: F) -> Result<Option<DbEnv>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let trimmed = |k: &str| get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        // DB_IMPL：唯一实现 nyet，显式选择也只接受 nyet（防误配）。
        if let Some(other) = trimmed("DB_IMPL") {
            if other != "nyet" {
                return Err(format!("DB_IMPL 非法: {other}（仅支持 nyet）"));
            }
        }
        let max_rows = parse_max_rows(trimmed("DB_MAX_ROWS"))?;
        let query_timeout = parse_timeout(trimmed("DB_QUERY_TIMEOUT_SECS"))?;
        let ssl = parse_sslmode_opt(trimmed("DB_SSLMODE"))?;

        let url = trimmed("DB_URL");
        let ty = trimmed("DB_TYPE");

        let (engine, spec, redacted_url) = if let Some(url) = url {
            let engine = engine_from_url(&url).ok_or_else(|| {
                format!("DB_URL 无法识别的协议: {url}（支持 mysql:// postgres:// sqlite://）")
            })?;
            if let Some(t) = &ty {
                let declared = DbEngine::from_type_str(t)?;
                if declared != engine {
                    return Err(format!(
                        "DB_URL 协议（{}）与 DB_TYPE（{t}）不一致",
                        engine.label()
                    ));
                }
            }
            match engine {
                DbEngine::Sqlite => {
                    let path = sqlite_path_from_url(&url);
                    (
                        engine,
                        ConnectSpec::Sqlite { path: path.clone() },
                        format!("sqlite://{path}"),
                    )
                }
                _ => (
                    engine,
                    ConnectSpec::Url { url: url.clone() },
                    redact_url(&url),
                ),
            }
        } else if let Some(t) = ty {
            build_from_tuple(&t, &get)?
        } else {
            // DB_URL / DB_TYPE 都未配置 → 未配置态
            return Ok(None);
        };

        // ---- nyet 专属 env（非法值一律在此报错：一条代码路径，可预测）----
        let mariadb = matches!(trimmed("DB_MARIADB").as_deref(), Some("1" | "true"));
        let guardrail_mode = match trimmed("DB_GUARDRAIL_MODE") {
            None => None,
            Some(s) if matches!(s.as_str(), "cost" | "rows" | "off") => Some(s),
            Some(s) => {
                return Err(format!(
                    "DB_GUARDRAIL_MODE 非法: {s}（可选 cost | rows | off）"
                ))
            }
        };
        let guardrail_max_cost = match trimmed("DB_GUARDRAIL_MAX_COST") {
            None => None,
            Some(s) => {
                let v: f64 = s
                    .parse()
                    .map_err(|_| format!("DB_GUARDRAIL_MAX_COST 非数字: {s}"))?;
                if v <= 0.0 {
                    return Err("DB_GUARDRAIL_MAX_COST 必须大于 0".into());
                }
                Some(v)
            }
        };
        let guardrail_max_rows = match trimmed("DB_GUARDRAIL_MAX_ROWS") {
            None => None,
            Some(s) => {
                let v: u64 = s
                    .parse()
                    .map_err(|_| format!("DB_GUARDRAIL_MAX_ROWS 非数字: {s}"))?;
                if v == 0 {
                    return Err("DB_GUARDRAIL_MAX_ROWS 不能为 0".into());
                }
                Some(v)
            }
        };
        let pii = csv_list(trimmed("DB_PII"));
        let pii_mode = match trimmed("DB_PII_MODE") {
            None => "deny".to_string(),
            Some(s) if matches!(s.as_str(), "deny" | "mask") => s,
            Some(s) => {
                return Err(format!("DB_PII_MODE 非法: {s}（可选 deny | mask）"));
            }
        };
        let allow_functions = csv_list(trimmed("DB_SQL_ALLOW_FUNCTIONS"));
        let deny_functions = csv_list(trimmed("DB_SQL_DENY_FUNCTIONS"));

        // ---- nyet 引擎参数推导（密码剥离 + sslmode 注入）----
        let (nyet_url, password, sqlite_path) = nyet_pieces(&engine, &spec, ssl);

        Ok(Some(DbEnv {
            engine,
            nyet_url,
            password,
            sqlite_path,
            redacted_url,
            max_rows,
            query_timeout,
            mariadb,
            guardrail_mode,
            guardrail_max_cost,
            guardrail_max_rows,
            pii,
            pii_mode,
            allow_functions,
            deny_functions,
        }))
    }
}

/// 解析 DB_MAX_ROWS：默认 100；0 或非数字 → Err；>1000 收敛到 1000。
fn parse_max_rows(v: Option<String>) -> Result<usize, String> {
    match v {
        None => Ok(DEFAULT_MAX_ROWS),
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|_| format!("DB_MAX_ROWS 非数字: {s}"))?;
            if n == 0 {
                return Err("DB_MAX_ROWS 不能为 0".into());
            }
            Ok(n.min(HARD_MAX_ROWS))
        }
    }
}

/// 解析 DB_QUERY_TIMEOUT_SECS：默认 30；0/非数字 → Err；>300 收敛到 300。
fn parse_timeout(v: Option<String>) -> Result<Duration, String> {
    match v {
        None => Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        Some(s) => {
            let n: u64 = s
                .parse()
                .map_err(|_| format!("DB_QUERY_TIMEOUT_SECS 非数字: {s}"))?;
            if n == 0 {
                return Err("DB_QUERY_TIMEOUT_SECS 不能为 0".into());
            }
            Ok(Duration::from_secs(n.min(HARD_TIMEOUT_SECS)))
        }
    }
}

/// 解析 DB_SSLMODE：disable | prefer | required（require 亦接受）。
/// 返回 None 表示未设置（与显式 prefer 等值，但 nyet 侧「未设置」不注入参数）。
fn parse_sslmode_opt(v: Option<String>) -> Result<Option<SslChoice>, String> {
    match v.as_deref() {
        None => Ok(None),
        Some("disable") => Ok(Some(SslChoice::Disable)),
        Some("prefer") => Ok(Some(SslChoice::Prefer)),
        Some("required") | Some("require") => Ok(Some(SslChoice::Required)),
        Some(other) => Err(format!(
            "DB_SSLMODE 非法: {other}（可选 disable | prefer | required）"
        )),
    }
}

/// 逗号列表 → Vec（trim、去空；空值/未设置 → 空 Vec）
fn csv_list(v: Option<String>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// URL scheme → 引擎
fn engine_from_url(url: &str) -> Option<DbEngine> {
    if url.starts_with("mysql://") {
        Some(DbEngine::MySql)
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Some(DbEngine::Postgres)
    } else if url.starts_with("sqlite://") || url.starts_with("sqlite::") {
        Some(DbEngine::Sqlite)
    } else {
        None
    }
}

/// sqlite URL → 文件路径（`sqlite:///abs.db` → `/abs.db`；
/// `sqlite::memory:` / `sqlite://:memory:` → `:memory:`）。
fn sqlite_path_from_url(url: &str) -> String {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    if rest == "memory:" {
        ":memory:".to_string()
    } else {
        rest.to_string()
    }
}

/// DB_TYPE + DB_HOST/DB_PORT/DB_USER/DB_PASSWORD/DB_NAME 元组形式。
fn build_from_tuple<F>(ty: &str, get: F) -> Result<(DbEngine, ConnectSpec, String), String>
where
    F: Fn(&str) -> Option<String>,
{
    let engine = DbEngine::from_type_str(ty)?;
    // 密码允许缺省（无密码场景），不要求显式设空
    let password = get("DB_PASSWORD").unwrap_or_default();

    match engine {
        DbEngine::MySql => {
            let host = required(&get, "DB_HOST")?;
            let user = required(&get, "DB_USER")?;
            let name = required(&get, "DB_NAME")?;
            let port = parse_port(get("DB_PORT"), 3306)?;
            let redacted = format!("mysql://{user}:***@{host}:{port}/{name}");
            Ok((
                engine,
                ConnectSpec::Server {
                    host,
                    port,
                    user,
                    password,
                    database: name,
                },
                redacted,
            ))
        }
        DbEngine::Postgres => {
            let host = required(&get, "DB_HOST")?;
            let user = required(&get, "DB_USER")?;
            let name = required(&get, "DB_NAME")?;
            let port = parse_port(get("DB_PORT"), 5432)?;
            let redacted = format!("postgres://{user}:***@{host}:{port}/{name}");
            Ok((
                engine,
                ConnectSpec::Server {
                    host,
                    port,
                    user,
                    password,
                    database: name,
                },
                redacted,
            ))
        }
        DbEngine::Sqlite => {
            // sqlite：DB_NAME 即 .db 文件路径（推荐绝对路径）或 :memory:
            let name = required(&get, "DB_NAME")?;
            Ok((
                engine,
                ConnectSpec::Sqlite { path: name.clone() },
                format!("sqlite://{name}"),
            ))
        }
    }
}

/// 必填项缺失 → Err（点名变量，便于界面排查）
fn required<F: Fn(&str) -> Option<String>>(get: F, key: &str) -> Result<String, String> {
    get(key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("缺少 {key}（DB_TYPE 元组形式的必填项）"))
}

/// 端口：默认值兜底；非数字 → Err
fn parse_port(v: Option<String>, default: u16) -> Result<u16, String> {
    match v {
        // 显式空值视同未设置（走引擎默认端口）
        None => Ok(default),
        Some(s) if s.trim().is_empty() => Ok(default),
        Some(s) => s
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("DB_PORT 非数字: {s}")),
    }
}

/// nyet 引擎参数：URL 形式剥离内嵌密码；DB_SSLMODE 显式设置时注入 query 参数；
/// 元组形式拼无密码 URL（密码单独传，天然免 percent-encode）。
fn nyet_pieces(
    engine: &DbEngine,
    spec: &ConnectSpec,
    ssl: Option<SslChoice>,
) -> (String, Option<String>, Option<String>) {
    match (&engine, spec) {
        (DbEngine::Sqlite, ConnectSpec::Sqlite { path }) => {
            (String::new(), None, Some(path.clone()))
        }
        (_, ConnectSpec::Url { url }) => {
            let (base, pw) = split_password(url);
            (inject_sslmode(&base, *engine, ssl), pw, None)
        }
        (DbEngine::MySql, ConnectSpec::Server { host, port, user, password, database }) => {
            let url = format!("mysql://{user}@{host}:{port}/{database}");
            (
                inject_sslmode(&url, *engine, ssl),
                (!password.is_empty()).then(|| password.clone()),
                None,
            )
        }
        (DbEngine::Postgres, ConnectSpec::Server { host, port, user, password, database }) => {
            let url = format!("postgres://{user}@{host}:{port}/{database}");
            (
                inject_sslmode(&url, *engine, ssl),
                (!password.is_empty()).then(|| password.clone()),
                None,
            )
        }
        // 组合由 from_getter 保证不出现（sqlite 必有 Sqlite spec，服务器引擎必有 Url/Server）
        _ => (String::new(), None, None),
    }
}

/// URL userinfo 里的密码剥出：`scheme://user:pass@rest` → `(scheme://user@rest, Some(pass))`。
/// 与 [`redact_url`] 同一套定位逻辑（rfind('@') + 首个 ':'）。
fn split_password(url: &str) -> (String, Option<String>) {
    let Some(scheme_end) = url.find("://") else {
        return (url.to_string(), None);
    };
    let rest_start = scheme_end + 3;
    let rest = &url[rest_start..];
    let host_start = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..host_start];
    if let Some(at) = authority.rfind('@') {
        if let Some(colon) = authority[..at].find(':') {
            let user = &authority[..colon];
            let password = &authority[colon + 1..at];
            return (
                // rest[at..] 自带 '@'，无需再补
                format!("{}{}{}", &url[..rest_start], user, &rest[at..]),
                Some(percent_decode(password)),
            );
        }
    }
    (url.to_string(), None)
}

/// DB_SSLMODE 注入为 URL query 参数（仅显式设置时；已有 query 用 `&` 接续）。
/// pg：`sslmode=disable|prefer|require`；mysql：`ssl-mode=DISABLED|PREFERRED|REQUIRED`。
fn inject_sslmode(url: &str, engine: DbEngine, ssl: Option<SslChoice>) -> String {
    let Some(ssl) = ssl else {
        return url.to_string();
    };
    let (key, value) = match (engine, ssl) {
        (DbEngine::Postgres, SslChoice::Disable) => ("sslmode", "disable"),
        (DbEngine::Postgres, SslChoice::Prefer) => ("sslmode", "prefer"),
        (DbEngine::Postgres, SslChoice::Required) => ("sslmode", "require"),
        (_, SslChoice::Disable) => ("ssl-mode", "DISABLED"),
        (_, SslChoice::Prefer) => ("ssl-mode", "PREFERRED"),
        (_, SslChoice::Required) => ("ssl-mode", "REQUIRED"),
    };
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{key}={value}")
}

/// userinfo 密码的 percent-decode（URL 形式下密码可能带 %XX 转义；
/// 引擎按明文密码覆写，故先解码。无法解码的 % 原样保留）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 日志脱敏：userinfo 中 `:` 之后的密码替换为 `***`。
pub(crate) fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest_start = scheme_end + 3;
    let rest = &url[rest_start..];
    // userinfo 在首个 `/` 之前、且含 `@` 才存在
    let host_start = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..host_start];
    if let Some(at) = authority.rfind('@') {
        if let Some(colon) = authority[..at].find(':') {
            let user = &authority[..colon];
            return format!("{}{}{}***{}", &url[..rest_start], user, ":", &rest[at..]);
        }
    }
    url.to_string()
}

// ============================================================================
//  单元测试（纯解析逻辑；端到端见 nyet 移植测试）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_masks_password() {
        assert_eq!(
            redact_url("mysql://u:secret@h:3306/db"),
            "mysql://u:***@h:3306/db"
        );
        assert_eq!(
            redact_url("postgres://bob:p@ss@h/db"),
            "postgres://bob:***@h/db"
        );
        // 无 userinfo 不动
        assert_eq!(redact_url("sqlite:///tmp/a.db"), "sqlite:///tmp/a.db");
    }

    #[test]
    fn config_from_url_forms() {
        let cases = [
            ("mysql://u:p@h:3307/db", DbEngine::MySql),
            ("postgres://u:p@h/db", DbEngine::Postgres),
            ("postgresql://u:p@h/db", DbEngine::Postgres),
            ("sqlite:///tmp/a.db", DbEngine::Sqlite),
        ];
        for (url, engine) in cases {
            let get = |k: &str| (k == "DB_URL").then(|| url.to_string());
            let cfg = DbEnv::from_getter(get).unwrap().unwrap_or_else(|| {
                panic!("应解析成功: {url}")
            });
            assert_eq!(cfg.engine, engine, "{url}");
        }
        // 未知协议
        let get = |k: &str| (k == "DB_URL").then(|| "redis://x".to_string());
        assert!(DbEnv::from_getter(get).is_err());
    }

    #[test]
    fn config_tuple_defaults_and_clamps() {
        let get = |k: &str| {
            match k {
                "DB_TYPE" => Some("mysql".into()),
                "DB_HOST" => Some("h".into()),
                "DB_USER" => Some("u".into()),
                "DB_PASSWORD" => Some("".into()),
                "DB_NAME" => Some("db".into()),
                _ => None,
            }
        };
        let cfg = DbEnv::from_getter(get).unwrap().unwrap();
        assert_eq!(cfg.engine, DbEngine::MySql);
        assert_eq!(cfg.max_rows, DEFAULT_MAX_ROWS);
        assert_eq!(cfg.query_timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(cfg.redacted_url, "mysql://u:***@h:3306/db");
        // 元组形式 → nyet 无密码 URL + 分离密码
        assert_eq!(cfg.nyet_url, "mysql://u@h:3306/db");
        assert_eq!(cfg.password, None);

        // 收敛
        let get2 = |k: &str| {
            match k {
                "DB_TYPE" => Some("postgres".into()),
                "DB_HOST" => Some("h".into()),
                "DB_USER" => Some("u".into()),
                "DB_NAME" => Some("db".into()),
                "DB_MAX_ROWS" => Some("5000".into()),
                "DB_QUERY_TIMEOUT_SECS" => Some("9999".into()),
                _ => None,
            }
        };
        let cfg2 = DbEnv::from_getter(get2).unwrap().unwrap();
        assert_eq!(cfg2.max_rows, HARD_MAX_ROWS);
        assert_eq!(cfg2.query_timeout, Duration::from_secs(HARD_TIMEOUT_SECS));

        // 非法数值
        let get3 = |k: &str| {
            match k {
                "DB_TYPE" => Some("mysql".into()),
                "DB_HOST" => Some("h".into()),
                "DB_USER" => Some("u".into()),
                "DB_NAME" => Some("db".into()),
                "DB_MAX_ROWS" => Some("abc".into()),
                _ => None,
            }
        };
        assert!(DbEnv::from_getter(get3).is_err());
    }

    #[test]
    fn config_missing_all_returns_none() {
        let get = |_: &str| None;
        assert!(DbEnv::from_getter(get).unwrap().is_none());
    }

    #[test]
    fn config_contradictory_url_and_type() {
        let get = |k: &str| {
            match k {
                "DB_URL" => Some("mysql://u:p@h/db".into()),
                "DB_TYPE" => Some("postgres".into()),
                _ => None,
            }
        };
        let err = DbEnv::from_getter(get).unwrap_err();
        assert!(err.contains("不一致"), "{err}");
    }

    #[test]
    fn config_required_tuple_fields() {
        // mysql 元组缺 DB_HOST → 点名报错
        let get = |k: &str| {
            match k {
                "DB_TYPE" => Some("mysql".into()),
                "DB_USER" => Some("u".into()),
                "DB_NAME" => Some("db".into()),
                _ => None,
            }
        };
        let err = DbEnv::from_getter(get).unwrap_err();
        assert!(err.contains("DB_HOST"), "{err}");
        // sqlite 忽略 HOST/USER，只要 DB_NAME
        let get2 = |k: &str| {
            match k {
                "DB_TYPE" => Some("sqlite".into()),
                "DB_NAME" => Some(":memory:".into()),
                _ => None,
            }
        };
        let cfg = DbEnv::from_getter(get2).unwrap().unwrap();
        assert_eq!(cfg.engine, DbEngine::Sqlite);
        assert_eq!(cfg.sqlite_path.as_deref(), Some(":memory:"));
    }

    #[test]
    fn sqlite_url_path_extraction() {
        assert_eq!(sqlite_path_from_url("sqlite:///tmp/a.db"), "/tmp/a.db");
        assert_eq!(sqlite_path_from_url("sqlite://rel.db"), "rel.db");
        assert_eq!(sqlite_path_from_url("sqlite::memory:"), ":memory:");
    }

    #[test]
    fn url_password_split_and_ssl_injection() {
        // 内嵌密码剥离
        let get = |k: &str| (k == "DB_URL").then(|| "mysql://u:sec%40ret@h/db".to_string());
        let cfg = DbEnv::from_getter(get).unwrap().unwrap();
        assert_eq!(cfg.nyet_url, "mysql://u@h/db");
        assert_eq!(cfg.password.as_deref(), Some("sec@ret"));

        // DB_SSLMODE 注入
        let get2 = |k: &str| match k {
            "DB_URL" => Some("postgres://u@h/db".to_string()),
            "DB_SSLMODE" => Some("required".into()),
            _ => None,
        };
        let cfg2 = DbEnv::from_getter(get2).unwrap().unwrap();
        assert_eq!(cfg2.nyet_url, "postgres://u@h/db?sslmode=require");

        // 已有 query 参数 → & 接续；mysql 大写形式
        let get3 = |k: &str| match k {
            "DB_URL" => Some("mysql://u@h/db?charset=utf8mb4".to_string()),
            "DB_SSLMODE" => Some("disable".into()),
            _ => None,
        };
        let cfg3 = DbEnv::from_getter(get3).unwrap().unwrap();
        assert_eq!(cfg3.nyet_url, "mysql://u@h/db?charset=utf8mb4&ssl-mode=DISABLED");

        // 未设置 → 不注入
        let get4 = |k: &str| (k == "DB_URL").then(|| "postgres://u@h/db".to_string());
        let cfg4 = DbEnv::from_getter(get4).unwrap().unwrap();
        assert_eq!(cfg4.nyet_url, "postgres://u@h/db");
    }

    #[test]
    fn db_impl_only_accepts_nyet_and_nyet_envs() {
        // 非法 DB_IMPL（未知值一律拒绝）
        for bad in ["fast", "postgres"] {
            let get = |k: &str| {
                match k {
                    "DB_URL" => Some("sqlite:///tmp/a.db".into()),
                    "DB_IMPL" => Some(bad.to_string()),
                    _ => None,
                }
            };
            let err = DbEnv::from_getter(get).unwrap_err();
            assert!(err.contains("仅支持 nyet"), "{bad}: {err}");
        }

        // 全套 nyet env
        let get2 = |k: &str| {
            match k {
                "DB_URL" => Some("mysql://u@h/db".into()),
                "DB_IMPL" => Some("nyet".into()),
                "DB_MARIADB" => Some("1".into()),
                "DB_GUARDRAIL_MODE" => Some("rows".into()),
                "DB_GUARDRAIL_MAX_COST" => Some("5000.5".into()),
                "DB_GUARDRAIL_MAX_ROWS" => Some("100000".into()),
                "DB_PII" => Some("users.email, orders.phone".into()),
                "DB_PII_MODE" => Some("mask".into()),
                "DB_SQL_DENY_FUNCTIONS" => Some("pg_sleep, dblink".into()),
                _ => None,
            }
        };
        let cfg = DbEnv::from_getter(get2).unwrap().unwrap();
        assert!(cfg.mariadb);
        assert_eq!(cfg.guardrail_mode.as_deref(), Some("rows"));
        assert_eq!(cfg.guardrail_max_cost, Some(5000.5));
        assert_eq!(cfg.guardrail_max_rows, Some(100000));
        assert_eq!(cfg.pii, vec!["users.email", "orders.phone"]);
        assert_eq!(cfg.pii_mode, "mask");
        assert_eq!(cfg.deny_functions, vec!["pg_sleep", "dblink"]);
        assert!(cfg.allow_functions.is_empty());

        // 非法值逐一报错
        for (k, v) in [
            ("DB_GUARDRAIL_MODE", "fast"),
            ("DB_GUARDRAIL_MAX_COST", "abc"),
            ("DB_GUARDRAIL_MAX_COST", "0"),
            ("DB_GUARDRAIL_MAX_ROWS", "-1"),
            ("DB_PII_MODE", "hide"),
            ("DB_SSLMODE", "maybe"),
        ] {
            let get = |kk: &str| {
                match kk {
                    "DB_URL" => Some("mysql://u@h/db".into()),
                    t if t == k => Some(v.to_string()),
                    _ => None,
                }
            };
            assert!(DbEnv::from_getter(get).is_err(), "{k}={v} 应报错");
        }
    }

    #[test]
    fn empty_pii_list_is_empty() {
        let get = |k: &str| {
            match k {
                "DB_URL" => Some("sqlite:///tmp/a.db".into()),
                "DB_PII" => Some(" , ".into()),
                _ => None,
            }
        };
        let cfg = DbEnv::from_getter(get).unwrap().unwrap();
        assert!(cfg.pii.is_empty());
        assert_eq!(cfg.pii_mode, "deny");
    }
}
