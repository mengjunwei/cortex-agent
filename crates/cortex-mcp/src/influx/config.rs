//! env 配置解析：一条 MCP 进程 = 一个 InfluxDB 服务（连接参数经环境变量注入
//! 子进程，由 cortex 侧 AES-GCM 加密落库，不进 LLM 上下文）。
//!
//! 三态语义（与 DB_* 一致）：
//! - `Ok(None)`：INFLUX_* 完全未配置（进程照常 serve，工具调用时返回「未配置」提示）
//! - `Ok(Some)`：配置有效
//! - `Err(msg)`：配置错误 —— main 直接 exit 2，探活立刻红（错误文本给操作者看，中文）
//!
//! | 变量 | 必填 | 说明 |
//! |---|---|---|
//! | `INFLUX_URL` | 是 | 服务地址，如 `http://127.0.0.1:8086`（v3 默认端口 8181） |
//! | `INFLUX_TOKEN` | 是 | API token（只读权限为佳） |
//! | `INFLUX_VERSION` | 否 | `2`（默认）或 `3` |
//! | `INFLUX_ORG` | v2 必填 | org 名 |
//! | `INFLUX_DATABASE` | v3 必填 | 数据库名（兼作 influx_schema 的默认库） |
//! | `INFLUX_BUCKET` | 否 | v2 默认 bucket：influx_schema 省略 bucket 参数时使用 |
//! | `INFLUX_MAX_ROWS` | 否 | 行数上限，默认 100 / 硬上限 1000 |
//! | `INFLUX_TIMEOUT_SECS` | 否 | 单条查询超时，默认 30s / 硬上限 300s |

use std::time::Duration;

/// 行数上限默认值与硬上限（与 DB_* 对齐）
pub const DEFAULT_MAX_ROWS: usize = 100;
pub const HARD_MAX_ROWS: usize = 1000;
/// 单条查询超时默认值（秒）与硬上限
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const HARD_TIMEOUT_SECS: u64 = 300;

/// InfluxDB 大版本（决定查询语言与 API 面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfluxVersion {
    V2,
    V3,
}

impl InfluxVersion {
    pub fn label(self) -> &'static str {
        match self {
            InfluxVersion::V2 => "influxdb2",
            InfluxVersion::V3 => "influxdb3",
        }
    }
}

/// InfluxDB 工具配置。
#[derive(Debug, Clone)]
pub struct InfluxEnv {
    pub version: InfluxVersion,
    /// 服务地址（已去掉尾部 `/`；token 绝不内嵌，日志安全）
    pub url: String,
    /// API token。只出现在请求头，绝不进日志/信封。
    pub token: String,
    /// v2：org 名（查询必填）
    pub org: Option<String>,
    /// v3：数据库名
    pub database: Option<String>,
    /// v2：默认 bucket（仅 influx_schema 省参时兜底；查询语言里 bucket 由 Flux 自带）
    pub default_bucket: Option<String>,
    /// 行数上限：默认 100，硬上限 1000
    pub max_rows: usize,
    /// 单条查询墙钟超时
    pub query_timeout: Duration,
}

impl InfluxEnv {
    /// 从进程环境变量构建（生产入口）。
    pub fn from_env() -> Result<Option<InfluxEnv>, String> {
        Self::from_getter(|k| std::env::var(k).ok())
    }

    /// 从任意取值器构建（单测入口，避免 env::set_var 竞态）。
    /// 取值器返回 None 表示「未设置」；Some("") 表示「显式设了空值」。
    pub fn from_getter<F>(get: F) -> Result<Option<InfluxEnv>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let trimmed = |k: &str| get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        // 任一核心变量出现即视为「想配置」；缺失其余 → Err（fail loud，不静默禁用）
        let core_set = trimmed("INFLUX_URL").is_some()
            || trimmed("INFLUX_TOKEN").is_some()
            || trimmed("INFLUX_ORG").is_some()
            || trimmed("INFLUX_DATABASE").is_some();
        if !core_set {
            return Ok(None);
        }

        let url = trimmed("INFLUX_URL").ok_or("缺少 INFLUX_URL（如 http://127.0.0.1:8086）")?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("INFLUX_URL 非法: {url}（需以 http:// 或 https:// 开头）"));
        }
        let url = url.trim_end_matches('/').to_string();
        let token = trimmed("INFLUX_TOKEN").ok_or("缺少 INFLUX_TOKEN")?;

        let version = match trimmed("INFLUX_VERSION").as_deref() {
            None | Some("2") | Some("v2") | Some("V2") => InfluxVersion::V2,
            Some("3") | Some("v3") | Some("V3") => InfluxVersion::V3,
            Some(other) => {
                return Err(format!("INFLUX_VERSION 非法: {other}（可选 2 | 3）"));
            }
        };

        let org = trimmed("INFLUX_ORG");
        let database = trimmed("INFLUX_DATABASE");
        match version {
            InfluxVersion::V2 if org.is_none() => {
                return Err("INFLUX_VERSION=2 需要 INFLUX_ORG（InfluxDB 2 的 org 名）".into());
            }
            InfluxVersion::V3 if database.is_none() => {
                return Err("INFLUX_VERSION=3 需要 INFLUX_DATABASE（InfluxDB 3 的数据库名）".into());
            }
            _ => {}
        }

        Ok(Some(InfluxEnv {
            version,
            url,
            token,
            org,
            database,
            default_bucket: trimmed("INFLUX_BUCKET"),
            max_rows: parse_max_rows(trimmed("INFLUX_MAX_ROWS"))?,
            query_timeout: parse_timeout(trimmed("INFLUX_TIMEOUT_SECS"))?,
        }))
    }
}

fn parse_max_rows(v: Option<String>) -> Result<usize, String> {
    match v {
        None => Ok(DEFAULT_MAX_ROWS),
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|_| format!("INFLUX_MAX_ROWS 非法: {s}（需为正整数）"))?;
            if n == 0 {
                return Err("INFLUX_MAX_ROWS 非法: 0（需 ≥ 1）".into());
            }
            Ok(n.min(HARD_MAX_ROWS))
        }
    }
}

fn parse_timeout(v: Option<String>) -> Result<Duration, String> {
    match v {
        None => Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        Some(s) => {
            let n: u64 = s
                .parse()
                .map_err(|_| format!("INFLUX_TIMEOUT_SECS 非法: {s}（需为正整数秒）"))?;
            if n == 0 {
                return Err("INFLUX_TIMEOUT_SECS 非法: 0（需 ≥ 1）".into());
            }
            Ok(Duration::from_secs(n.min(HARD_TIMEOUT_SECS)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let m: HashMap<&str, String> =
            map.iter().map(|(k, v)| (*k, v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn empty_env_is_none() {
        assert!(InfluxEnv::from_getter(getter(&[])).unwrap().is_none());
    }

    #[test]
    fn v2_happy_path_with_defaults() {
        let env = InfluxEnv::from_getter(getter(&[
            ("INFLUX_URL", "http://127.0.0.1:8086/"),
            ("INFLUX_TOKEN", "t"),
            ("INFLUX_ORG", "resolink"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(env.version, InfluxVersion::V2);
        assert_eq!(env.url, "http://127.0.0.1:8086"); // 尾部 / 已剥
        assert_eq!(env.max_rows, DEFAULT_MAX_ROWS);
        assert_eq!(env.query_timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(env.org.as_deref(), Some("resolink"));
    }

    #[test]
    fn v3_requires_database() {
        let err = InfluxEnv::from_getter(getter(&[
            ("INFLUX_URL", "http://127.0.0.1:8181"),
            ("INFLUX_TOKEN", "t"),
            ("INFLUX_VERSION", "3"),
        ]))
        .unwrap_err();
        assert!(err.contains("INFLUX_DATABASE"), "{err}");
    }

    #[test]
    fn v2_requires_org() {
        let err = InfluxEnv::from_getter(getter(&[
            ("INFLUX_URL", "http://x"),
            ("INFLUX_TOKEN", "t"),
        ]))
        .unwrap_err();
        assert!(err.contains("INFLUX_ORG"), "{err}");
    }

    #[test]
    fn token_alone_means_configured_and_errors() {
        let err = InfluxEnv::from_getter(getter(&[("INFLUX_TOKEN", "t")])).unwrap_err();
        assert!(err.contains("INFLUX_URL"), "{err}");
    }

    #[test]
    fn bad_url_scheme_rejected() {
        let err = InfluxEnv::from_getter(getter(&[
            ("INFLUX_URL", "ftp://x"),
            ("INFLUX_TOKEN", "t"),
            ("INFLUX_ORG", "o"),
        ]))
        .unwrap_err();
        assert!(err.contains("http"), "{err}");
    }

    #[test]
    fn clamps_apply() {
        let env = InfluxEnv::from_getter(getter(&[
            ("INFLUX_URL", "http://x"),
            ("INFLUX_TOKEN", "t"),
            ("INFLUX_ORG", "o"),
            ("INFLUX_MAX_ROWS", "99999"),
            ("INFLUX_TIMEOUT_SECS", "9999"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(env.max_rows, HARD_MAX_ROWS);
        assert_eq!(env.query_timeout, Duration::from_secs(HARD_TIMEOUT_SECS));
    }

    #[test]
    fn version_aliases() {
        for (raw, expect) in [("3", InfluxVersion::V3), ("v3", InfluxVersion::V3)] {
            let env = InfluxEnv::from_getter(getter(&[
                ("INFLUX_URL", "http://x"),
                ("INFLUX_TOKEN", "t"),
                ("INFLUX_DATABASE", "d"),
                ("INFLUX_VERSION", raw),
            ]))
            .unwrap()
            .unwrap();
            assert_eq!(env.version, expect);
        }
        let err = InfluxEnv::from_getter(getter(&[
            ("INFLUX_URL", "http://x"),
            ("INFLUX_TOKEN", "t"),
            ("INFLUX_ORG", "o"),
            ("INFLUX_VERSION", "4"),
        ]))
        .unwrap_err();
        assert!(err.contains("INFLUX_VERSION"), "{err}");
    }
}
