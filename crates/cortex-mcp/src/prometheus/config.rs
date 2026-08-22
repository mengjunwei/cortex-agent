//! env 配置解析：一条 MCP 进程 = 一个 Prometheus 服务（连接参数经环境变量注入
//! 子进程，由 cortex 侧 AES-GCM 加密落库，不进 LLM 上下文）。
//!
//! 三态语义（与 DB_* / INFLUX_* 一致）：
//! - `Ok(None)`：PROM_* 完全未配置（进程照常 serve，工具调用时返回「未配置」提示）
//! - `Ok(Some)`：配置有效
//! - `Err(msg)`：配置错误 —— main 直接 exit 2，探活立刻红（错误文本给操作者看，中文）
//!
//! | 变量 | 必填 | 说明 |
//! |---|---|---|
//! | `PROM_URL` | 是 | 服务地址，如 `http://127.0.0.1:9090`（可带路径前缀 `/prometheus`） |
//! | `PROM_TOKEN` | 否 | Bearer token（服务前面有网关鉴权时用；无鉴权服务省略） |
//! | `PROM_MAX_ROWS` | 否 | 行数上限，默认 100 / 硬上限 1000 |
//! | `PROM_TIMEOUT_SECS` | 否 | 单条查询超时，默认 30s / 硬上限 300s |

use std::time::Duration;

/// 行数上限默认值与硬上限（与 INFLUX_* / DB_* 对齐）
pub const DEFAULT_MAX_ROWS: usize = 100;
pub const HARD_MAX_ROWS: usize = 1000;
/// 单条查询超时默认值（秒）与硬上限
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const HARD_TIMEOUT_SECS: u64 = 300;

/// Prometheus 工具配置。
#[derive(Debug, Clone)]
pub struct PromEnv {
    /// 服务地址（已去掉尾部 `/`；token 绝不内嵌，日志安全）
    pub url: String,
    /// Bearer token（可选）。只进请求头，绝不进日志/信封。
    pub token: Option<String>,
    /// 行数上限：默认 100，硬上限 1000
    pub max_rows: usize,
    /// 单条查询墙钟超时（同时作为 HTTP 客户端超时）
    pub query_timeout: Duration,
}

impl PromEnv {
    /// 从进程环境变量构建（生产入口）。
    pub fn from_env() -> Result<Option<PromEnv>, String> {
        Self::from_getter(|k| std::env::var(k).ok())
    }

    /// 从任意取值器构建（单测入口，避免 env::set_var 竞态）。
    /// 取值器返回 None 表示「未设置」；Some("") 表示「显式设了空值」。
    pub fn from_getter<F>(get: F) -> Result<Option<PromEnv>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let trimmed = |k: &str| get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        // 任一核心变量出现即视为「想配置」；缺失其余 → Err（fail loud，不静默禁用）
        if trimmed("PROM_URL").is_none() && trimmed("PROM_TOKEN").is_none() {
            return Ok(None);
        }

        let url = trimmed("PROM_URL").ok_or("缺少 PROM_URL（如 http://127.0.0.1:9090）")?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("PROM_URL 非法: {url}（需以 http:// 或 https:// 开头）"));
        }
        let url = url.trim_end_matches('/').to_string();

        Ok(Some(PromEnv {
            url,
            token: trimmed("PROM_TOKEN"),
            max_rows: parse_max_rows(trimmed("PROM_MAX_ROWS"))?,
            query_timeout: parse_timeout(trimmed("PROM_TIMEOUT_SECS"))?,
        }))
    }
}

fn parse_max_rows(v: Option<String>) -> Result<usize, String> {
    match v {
        None => Ok(DEFAULT_MAX_ROWS),
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|_| format!("PROM_MAX_ROWS 非法: {s}（需为正整数）"))?;
            if n == 0 {
                return Err("PROM_MAX_ROWS 非法: 0（需 ≥ 1）".into());
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
                .map_err(|_| format!("PROM_TIMEOUT_SECS 非法: {s}（需为正整数秒）"))?;
            if n == 0 {
                return Err("PROM_TIMEOUT_SECS 非法: 0（需 ≥ 1）".into());
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
        assert!(PromEnv::from_getter(getter(&[])).unwrap().is_none());
    }

    #[test]
    fn happy_path_with_defaults() {
        let env = PromEnv::from_getter(getter(&[("PROM_URL", "http://127.0.0.1:9090/")]))
            .unwrap()
            .unwrap();
        assert_eq!(env.url, "http://127.0.0.1:9090"); // 尾部 / 已剥
        assert_eq!(env.token, None);
        assert_eq!(env.max_rows, DEFAULT_MAX_ROWS);
        assert_eq!(env.query_timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn token_alone_means_configured_and_errors() {
        let err = PromEnv::from_getter(getter(&[("PROM_TOKEN", "t")])).unwrap_err();
        assert!(err.contains("PROM_URL"), "{err}");
    }

    #[test]
    fn token_optional_pair_ok() {
        let env = PromEnv::from_getter(getter(&[
            ("PROM_URL", "http://gateway/prometheus"),
            ("PROM_TOKEN", "secret"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(env.token.as_deref(), Some("secret"));
        assert_eq!(env.url, "http://gateway/prometheus");
    }

    #[test]
    fn bad_url_scheme_rejected() {
        let err = PromEnv::from_getter(getter(&[("PROM_URL", "ftp://x")])).unwrap_err();
        assert!(err.contains("http"), "{err}");
    }

    #[test]
    fn clamps_apply() {
        let env = PromEnv::from_getter(getter(&[
            ("PROM_URL", "http://x"),
            ("PROM_MAX_ROWS", "99999"),
            ("PROM_TIMEOUT_SECS", "9999"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(env.max_rows, HARD_MAX_ROWS);
        assert_eq!(env.query_timeout, Duration::from_secs(HARD_TIMEOUT_SECS));
    }

    #[test]
    fn bad_numbers_rejected() {
        assert!(PromEnv::from_getter(getter(&[
            ("PROM_URL", "http://x"),
            ("PROM_MAX_ROWS", "abc"),
        ]))
        .is_err());
        assert!(PromEnv::from_getter(getter(&[
            ("PROM_URL", "http://x"),
            ("PROM_TIMEOUT_SECS", "0"),
        ]))
        .is_err());
    }
}
