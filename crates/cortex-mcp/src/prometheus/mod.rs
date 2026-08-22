//! Prometheus 只读查询工具集（PromQL 即时/区间查询 + 结构探查）。
//!
//! 客户端用 [`prometheus_http_query`]（Prometheus HTTP API 的事实标准 Rust 客户端，
//! 5.5M+ 下载、持续维护、reqwest 0.13 与本 crate 对齐）——官方只提供 Go 客户端，
//! Rust 侧无官方查询库。查询/元数据全部走服务端 HTTP API，零命令行依赖。
//!
//! 只读性由构造保证：PromQL 本身只读，且只调用查询/元数据端点，从不触碰
//! `/api/v1/admin/*`。输出与 db_* / influx_* 同款 JSON 信封 `{"v":1,"ok":...}`，
//! 错误码复用 influx 模块的封闭列表（CONNECTION_FAILED / AUTH_FAILED /
//! QUERY_REJECTED / QUERY_ERROR / SERVER_ERROR / TIMEOUT / INTERNAL，hint 必填）。
//!
//! 资源上限：行数（点数）默认 100（`PROM_MAX_ROWS`，硬上限 1000）；单查超时
//! 默认 30s（`PROM_TIMEOUT_SECS`，硬上限 300s），同时作为 HTTP 客户端超时与
//! 服务端求值预算传入。
//!
//! # 退出码约定
//!
//! PROM_* 配置无效或启动自检失败（最小查询）→ 进程以 **exit code 2** 退出
//! （stderr 中文说明），cortex 的 MCP 探活立即转红，下次重新拉起进程自愈。

pub mod config;

pub use config::PromEnv;

use std::sync::Arc;
use std::time::Instant;

use prometheus_http_query::{Client as PromClient, Error as PromError};
use serde::Serialize;
use serde_json::{Map, Value};

// 错误码契约与 db_* / influx_* 共用同一封闭列表（定义在 influx 模块）
use crate::influx::code;

/// 元数据清单的行数上限：比查询行数上限宽，但仍有界。
fn cap() -> usize {
    config::HARD_MAX_ROWS
}

#[derive(Clone)]
pub struct PromTools(Arc<Inner>);

struct Inner {
    env: PromEnv,
    client: PromClient,
}

impl PromTools {
    /// 构建 + 启动自检（最小查询 `1`，验证 URL / 鉴权 / 查询链路）。
    /// Err（中文，操作者可见）→ main exit 2。
    pub async fn start(env: PromEnv) -> Result<PromTools, String> {
        let mut builder = reqwest::Client::builder().timeout(env.query_timeout);
        if let Some(token) = &env.token {
            let v = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| "PROM_TOKEN 含非法 HTTP 头字符".to_string())?;
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, v);
            builder = builder.default_headers(headers);
        }
        let http = builder.build().map_err(|e| format!("HTTP 客户端构建失败: {e}"))?;
        let client =
            PromClient::from(http, &env.url).map_err(|e| format!("PROM_URL 无法解析: {e}"))?;
        let tools = PromTools(Arc::new(Inner { client, env }));
        let url = tools.0.env.url.clone();
        let resp = tools
            .0
            .client
            .query("1")
            .get_raw()
            .await
            .map_err(|e| format!("Prometheus 启动自检失败（PROM_URL={url}）: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Prometheus 自检读取响应失败: {e}"))?;
        let healthy = status.is_success()
            && serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("status").and_then(Value::as_str).map(|s| s == "success"))
                .unwrap_or(false);
        if !healthy {
            return Err(format!(
                "Prometheus 启动自检失败（PROM_URL={url}，HTTP {status}，如经网关鉴权请检查 PROM_TOKEN）: {}",
                shorten(&body)
            ));
        }
        Ok(tools)
    }

    /// prom_query：单条 PromQL 查询。
    /// - `start`+`end`+`step` 三者齐备 → 区间查询（逐点展开成行）
    /// - 三者全缺 → 即时查询（`time` 可选，缺省为当前时刻）
    /// - 只给部分 → QUERY_REJECTED
    pub async fn query(
        &self,
        q: &str,
        time: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
        step: Option<f64>,
        limit: Option<u64>,
    ) -> String {
        let inner = &self.0;
        let t0 = Instant::now();
        let max = inner.env.max_rows;
        let limit = limit.unwrap_or(max as u64).clamp(1, max as u64) as usize;

        if q.trim().is_empty() {
            return err_envelope(
                code::QUERY_REJECTED,
                "query is empty",
                "provide a PromQL expression, e.g. \"up\" or \"rate(http_requests_total[5m])\"",
            );
        }
        let timeout_secs = inner.env.query_timeout.as_secs() as i64;

        // 参数组合校验（在解析具体值之前，先给方向性的提示）
        let range_given = start.is_some() || end.is_some() || step.is_some();
        let range_complete = start.is_some() && end.is_some() && step.is_some();
        if range_given && !range_complete {
            return err_envelope(
                code::QUERY_REJECTED,
                "range query needs start, end and step together",
                "provide all three (start + end + step) for a range query, or none of them \
                 for an instant query",
            );
        }

        // 表达式查询走 get_raw + 自解析：crate 0.9.0 的类型化反序列化对
        // scalar 结果有 bug（"invalid type: map, expected f64"），且不支持
        // string 结果类型；Prometheus 查询响应格式极简且稳定，自解析更稳。
        let raw = if range_complete {
            // 区间查询
            let (Ok(s), Ok(e)) = (parse_ts(start.unwrap()), parse_ts(end.unwrap())) else {
                return err_envelope(
                    code::QUERY_REJECTED,
                    "start/end must be unix seconds or RFC3339 (e.g. 1735660800 or 2024-12-31T16:00:00Z)",
                    "fix the start/end format",
                );
            };
            let Some(step) = step else { unreachable!("range_complete guarantees step") };
            if !(step.is_finite() && step > 0.0) {
                return err_envelope(
                    code::QUERY_REJECTED,
                    "step must be a positive number of seconds",
                    "e.g. 15 (seconds), or 0.5 for half-second resolution",
                );
            }
            if e < s {
                return err_envelope(
                    code::QUERY_REJECTED,
                    "end is before start",
                    "swap the time bounds or fix the values",
                );
            }
            inner
                .client
                .query_range(q, s as i64, e as i64, step)
                .timeout(timeout_secs)
                .get_raw()
                .await
        } else {
            // 即时查询
            let mut b = inner.client.query(q).timeout(timeout_secs);
            if let Some(t) = time.map(str::trim).filter(|s| !s.is_empty()) {
                match parse_ts(t) {
                    Ok(ts) => b = b.at(ts as i64),
                    Err(_) => {
                        return err_envelope(
                            code::QUERY_REJECTED,
                            "time must be unix seconds or RFC3339",
                            "e.g. 1735660800 or 2024-12-31T16:00:00Z",
                        )
                    }
                }
            }
            b.get_raw().await
        };

        match raw {
            Err(e) => err_envelope_map(&e),
            Ok(resp) => match raw_rows(resp, limit).await {
                Err(envelope) => envelope,
                Ok((rows, truncated)) => {
                    let mut warnings = Vec::new();
                    if truncated {
                        warnings.push(Warning {
                            code: "TRUNCATED",
                            message: format!("result capped at {limit} rows (one row per point)"),
                        });
                    }
                    query_envelope(rows, truncated, t0.elapsed(), warnings)
                }
            },
        }
    }

    /// prom_schema：无参 → 指标名清单；带 `metric` → 该指标的 type/help/unit
    /// 与 label 键集合。
    pub async fn schema(&self, metric: Option<&str>) -> String {
        let inner = &self.0;
        let t0 = Instant::now();
        let metric = metric.map(str::trim).filter(|s| !s.is_empty());

        let Some(metric) = metric else {
            return match inner.client.label_values("__name__").get().await {
                Err(e) => err_envelope_map(&e),
                Ok(mut names) => {
                    let truncated = names.len() > cap();
                    names.sort_unstable();
                    names.truncate(cap());
                    MetricsEnvelope {
                        v: 1,
                        ok: true,
                        metric_count: names.len(),
                        metrics: names,
                        warnings: truncated_warn(truncated),
                        meta: SchemaMeta {
                            duration_ms: ms(t0.elapsed()),
                            connection: "prometheus",
                        },
                    }
                    .to_json()
                }
            };
        };

        // 指标明细：元数据（type/help/unit）+ 全时间窗的 label 键并集
        let meta = inner
            .client
            .metric_metadata()
            .metric(metric)
            .get()
            .await;
        let series = match inner
            .client
            .series(&[prometheus_http_query::Selector::new().eq("__name__", metric)])
        {
            Err(e) => Err(e),
            Ok(b) => {
                b.start(0)
                    .end(chrono::Utc::now().timestamp())
                    .get()
                    .await
            }
        };

        match (meta, series) {
            (Err(e), _) | (_, Err(e)) => err_envelope_map(&e),
            (Ok(meta_map), Ok(series)) => {
                let (metric_type, help, unit) = meta_map
                    .get(metric)
                    .and_then(|v| v.first())
                    .map(|m| {
                        (
                            format!("{:?}", m.metric_type()).to_lowercase(),
                            m.help().to_string(),
                            m.unit().to_string(),
                        )
                    })
                    .unwrap_or_else(|| ("unknown".into(), String::new(), String::new()));
                // label 键并集（去掉 __name__ 本身）
                let mut keys: Vec<String> = series
                    .iter()
                    .flat_map(|s| s.keys().cloned())
                    .filter(|k| k != "__name__")
                    .collect();
                keys.sort_unstable();
                keys.dedup();
                if metric_type == "unknown" && keys.is_empty() {
                    return err_envelope(
                        code::QUERY_ERROR,
                        &format!("metric \"{metric}\" not found"),
                        "call prom_schema with no arguments to list available metric names",
                    );
                }
                MetricDetail {
                    v: 1,
                    ok: true,
                    metric,
                    metric_type,
                    help,
                    unit,
                    labels: keys,
                    meta: SchemaMeta {
                        duration_ms: ms(t0.elapsed()),
                        connection: "prometheus",
                    },
                }
                .to_json()
            }
        }
    }
}

// —— 表达式查询响应解析（get_raw 自研路径） ——

/// 读取原始响应 → (rows, truncated)；Err 为完整错误信封 JSON。
/// 错误分层：传输失败 → CONNECTION_FAILED；HTTP 非 2xx / 非 JSON → SERVER_ERROR
/// （多为代理拦了，提示查 PROM_TOKEN）；API status=error → 按错误类型映射。
async fn raw_rows(resp: reqwest::Response, limit: usize) -> Result<(Vec<Value>, bool), String> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| {
            err_envelope(
                code::CONNECTION_FAILED,
                &format!("cannot read response: {e}"),
                "check the network path to the Prometheus server",
            )
        })?;
    // 先看 JSON body 再看 HTTP 码：Prometheus 的 bad_data/timeout 错误也伴随
    // HTTP 400/503，错误语义以 body 的 status/errorType 为准。
    let parsed: Option<Value> = serde_json::from_str(&body).ok();
    if let Some(v) = &parsed {
        if v.get("status").and_then(Value::as_str) == Some("error") {
            let et = v.get("errorType").and_then(Value::as_str).unwrap_or("");
            let msg = v.get("error").and_then(Value::as_str).unwrap_or("query failed");
            let (c, hint) = match et {
                "timeout" | "canceled" => (
                    code::TIMEOUT,
                    "narrow the time range or increase the step to reduce query work",
                ),
                "internal" | "unavailable" => {
                    (code::SERVER_ERROR, "server-side failure; retry shortly")
                }
                _ => (
                    code::QUERY_ERROR,
                    "check the PromQL syntax; call prom_schema first to discover metric \
                     and label names",
                ),
            };
            return Err(err_envelope(c, msg, hint));
        }
    }
    if !status.is_success() || parsed.is_none() {
        // 非 JSON 或非 2xx 且无 API 错误体：多为代理/网关拦截
        return Err(err_envelope(
            code::SERVER_ERROR,
            &format!("unexpected response (HTTP {status}): {}", shorten(&body)),
            "verify PROM_URL is a Prometheus API endpoint; if a gateway sits in front, \
             check PROM_TOKEN",
        ));
    }
    let v = parsed.unwrap_or_default();
    rows_from_data(v.get("data").unwrap_or(&Value::Null), limit).map_err(|m| {
        err_envelope(
            code::INTERNAL,
            &m,
            "unexpected response shape; the server version may differ",
        )
    })
}

/// data 对象 → 统一「一行一个点」（vector/matrix/scalar/string 全支持）。
fn rows_from_data(data: &Value, limit: usize) -> Result<(Vec<Value>, bool), String> {
    let rt = data
        .get("resultType")
        .and_then(Value::as_str)
        .ok_or("missing resultType")?;
    let mut rows: Vec<Value> = Vec::new();
    let mut total: usize = 0;
    let mut add = |labels: &Value, ts: f64, s: &str| {
        total += 1;
        if rows.len() < limit {
            rows.push(point_row(labels, ts, s));
        }
    };
    match rt {
        "vector" => {
            for item in data
                .get("result")
                .and_then(Value::as_array)
                .ok_or("vector result is not an array")?
            {
                let Some((ts, s)) = pair_parts(item.get("value")) else {
                    return Err("vector sample is not a [timestamp, value] pair".into());
                };
                add(item.get("metric").unwrap_or(&Value::Null), ts, s);
            }
        }
        "matrix" => {
            for series in data
                .get("result")
                .and_then(Value::as_array)
                .ok_or("matrix result is not an array")?
            {
                for pair in series
                    .get("values")
                    .and_then(Value::as_array)
                    .ok_or("matrix series has no values array")?
                {
                    let Some((ts, s)) = pair_parts(Some(pair)) else {
                        return Err("matrix sample is not a [timestamp, value] pair".into());
                    };
                    add(series.get("metric").unwrap_or(&Value::Null), ts, s);
                }
            }
        }
        "scalar" | "string" => {
            let Some((ts, s)) = pair_parts(data.get("result")) else {
                return Err("scalar/string result is not a [timestamp, value] pair".into());
            };
            add(&Value::Null, ts, s);
        }
        other => return Err(format!("unsupported resultType \"{other}\"")),
    }
    let truncated = total > rows.len();
    Ok((rows, truncated))
}

/// [timestamp, value] 对 → (f64 秒, 值字符串)。Prometheus 的值一律是字符串
/// （保精度），NaN/±Inf 也是字符串。
fn pair_parts(pair: Option<&Value>) -> Option<(f64, &str)> {
    let arr = pair?.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    let ts = arr[0].as_f64()?;
    let s = arr[1].as_str()?;
    Some((ts, s))
}

/// 一个样本点 → 一行：labels 平铺 + value + RFC3339 时间。
fn point_row(labels: &Value, ts: f64, s: &str) -> Value {
    let mut obj = Map::new();
    if let Some(m) = labels.as_object() {
        for (k, v) in m {
            if let Some(sv) = v.as_str() {
                obj.insert(k.clone(), Value::String(sv.to_string()));
            }
        }
    }
    obj.insert("value".into(), prom_value(s));
    obj.insert("time".into(), Value::String(ts_rfc3339(ts)));
    Value::Object(obj)
}

/// Prometheus 值字符串 → JSON：数值字符串转数字（保精度语义内），NaN/±Inf 与
/// 其他非数值原样保留为字符串（serde_json 无法序列化非有限 f64）。
fn prom_value(s: &str) -> Value {
    if let Ok(n) = s.parse::<f64>() {
        if n.is_finite() {
            return serde_json::Number::from_f64(n)
                .map(Value::Number)
                .unwrap_or(Value::Null);
        }
    }
    Value::String(s.to_string())
}

/// 服务器正文截断（防止把整页 HTML 塞进信封）
fn shorten(s: &str) -> String {
    s.chars().take(300).collect()
}

/// unix 秒（可能带小数）→ RFC3339（UTC，带 Z）
fn ts_rfc3339(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9).round() as u32;
    chrono::DateTime::from_timestamp(secs, nanos.min(999_999_999))
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .unwrap_or_else(|| ts.to_string())
}

/// 时间参数解析：unix 秒（可带小数）或 RFC3339。
fn parse_ts(s: &str) -> Result<f64, ()> {
    if let Ok(n) = s.parse::<f64>() {
        if n.is_finite() {
            return Ok(n);
        }
        return Err(());
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp() as f64 + d.timestamp_subsec_nanos() as f64 / 1e9)
        .map_err(|_| ())
}

fn ms(d: std::time::Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

fn truncated_warn(truncated: bool) -> Vec<Warning> {
    if truncated {
        vec![Warning {
            code: "TRUNCATED",
            message: format!("list capped at {} entries", cap()),
        }]
    } else {
        Vec::new()
    }
}

/// crate 错误 → 信封错误码。
/// - `Prometheus`（服务器 JSON error）：timeout→TIMEOUT，internal/unavailable→
///   SERVER_ERROR，其余（bad_data/execution 等）→ QUERY_ERROR
/// - `Client`（传输/反序列化）：错误链里找 reqwest::Error 判超时；连不上 →
///   CONNECTION_FAILED；无 reqwest 源（多为代理返回非 JSON，如 401 页面）→
///   SERVER_ERROR 并提示检查 PROM_TOKEN
fn err_envelope_map(e: &PromError) -> String {
    match e {
        PromError::Prometheus(pe) => {
            let (c, hint) = if pe.is_timeout() {
                (
                    code::TIMEOUT,
                    "narrow the time range or increase the step to reduce query work",
                )
            } else if pe.is_internal() || pe.is_unavailable() {
                (code::SERVER_ERROR, "server-side failure; retry shortly")
            } else {
                (
                    code::QUERY_ERROR,
                    "check the PromQL syntax; call prom_schema first to discover metric \
                     and label names",
                )
            };
            err_envelope(c, pe.message(), hint)
        }
        PromError::Client(_) => {
            // 沿错误链下钻找 reqwest::Error（字段是 pub(crate)，只能靠 source() 链）
            let mut src: Option<&dyn std::error::Error> = Some(e);
            while let Some(s) = src {
                if let Some(re) = s.downcast_ref::<reqwest::Error>() {
                    return if re.is_timeout() {
                        err_envelope(
                            code::TIMEOUT,
                            &format!("request timed out: {e}"),
                            "narrow the time range or increase the step to reduce query work",
                        )
                    } else {
                        err_envelope(
                            code::CONNECTION_FAILED,
                            &format!("cannot reach Prometheus: {e}"),
                            "check that PROM_URL points to a running Prometheus server",
                        )
                    };
                }
                src = s.source();
            }
            err_envelope(
                code::SERVER_ERROR,
                &format!("unexpected response: {e}"),
                "verify PROM_URL is a Prometheus API endpoint; if a gateway sits in front, \
                 check PROM_TOKEN",
            )
        }
        _ => err_envelope(
            code::INTERNAL,
            &format!("{e}"),
            "unexpected client error; see the message above",
        ),
    }
}

// —— JSON 信封 v1（字段序即序列化序；契约与 db_* / influx_* 对齐，只增不改） ——

trait ToJson {
    fn to_json(self) -> String;
}

#[derive(Serialize)]
struct QueryEnvelope {
    v: u8,
    ok: bool,
    rows: Vec<Value>,
    meta: QueryMeta,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<Warning>,
}

#[derive(Serialize)]
struct QueryMeta {
    row_count: usize,
    truncated: bool,
    duration_ms: u64,
    connection: &'static str,
}

#[derive(Serialize)]
struct Warning {
    code: &'static str,
    message: String,
}

impl ToJson for QueryEnvelope {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

fn query_envelope(rows: Vec<Value>, truncated: bool, d: std::time::Duration, warnings: Vec<Warning>) -> String {
    QueryEnvelope {
        v: 1,
        ok: true,
        meta: QueryMeta {
            row_count: rows.len(),
            truncated,
            duration_ms: ms(d),
            connection: "prometheus",
        },
        rows,
        warnings,
    }
    .to_json()
}

#[derive(Serialize)]
struct ErrEnvelope {
    v: u8,
    ok: bool,
    error: ErrBody,
}

#[derive(Serialize)]
struct ErrBody {
    code: &'static str,
    message: String,
    hint: String,
}

fn err_envelope(code: &'static str, message: &str, hint: &str) -> String {
    serde_json::to_string(&ErrEnvelope {
        v: 1,
        ok: false,
        error: ErrBody {
            code,
            message: message.into(),
            hint: hint.into(),
        },
    })
    .unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
}

#[derive(Serialize)]
struct SchemaMeta {
    duration_ms: u64,
    connection: &'static str,
}

#[derive(Serialize)]
struct MetricsEnvelope {
    v: u8,
    ok: bool,
    metric_count: usize,
    metrics: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<Warning>,
    meta: SchemaMeta,
}

impl ToJson for MetricsEnvelope {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[derive(Serialize)]
struct MetricDetail<'a> {
    v: u8,
    ok: bool,
    metric: &'a str,
    metric_type: String,
    help: String,
    unit: String,
    labels: Vec<String>,
    meta: SchemaMeta,
}

impl ToJson for MetricDetail<'_> {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— 纯函数单测 ——

    #[test]
    fn prom_value_mapping() {
        assert_eq!(prom_value("1.5"), serde_json::json!(1.5));
        assert_eq!(prom_value("0"), serde_json::json!(0.0));
        assert_eq!(prom_value("NaN"), serde_json::json!("NaN"));
        assert_eq!(prom_value("+Inf"), serde_json::json!("+Inf"));
        assert_eq!(prom_value("-Inf"), serde_json::json!("-Inf"));
    }

    #[test]
    fn ts_rfc3339_conversion() {
        assert_eq!(ts_rfc3339(1735660800.0), "2024-12-31T16:00:00Z");
        assert!(ts_rfc3339(1735660800.25).starts_with("2024-12-31T16:00:00"));
        assert_eq!(ts_rfc3339(0.0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn parse_ts_unix_and_rfc3339() {
        assert_eq!(parse_ts("1735660800").unwrap(), 1735660800.0);
        assert_eq!(parse_ts("1735660800.5").unwrap(), 1735660800.5);
        assert_eq!(parse_ts("2024-12-31T16:00:00Z").unwrap(), 1735660800.0);
        assert_eq!(
            parse_ts("2024-12-31T16:00:00.500Z").unwrap(),
            1735660800.5
        );
        assert!(parse_ts("yesterday").is_err());
        assert!(parse_ts("NaN").is_err());
    }

    #[test]
    fn rows_from_data_matrix_respects_limit() {
        let data = serde_json::json!({
            "resultType": "matrix",
            "result": [
                {"metric": {"__name__": "up", "job": "j"},
                 "values": [[1735660800, "1"], [1735660815, "1"], [1735660830, "1"]]}
            ]
        });
        let (rows, truncated) = rows_from_data(&data, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(truncated);
        let (rows, truncated) = rows_from_data(&data, 5).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(!truncated);
        // 行形状：labels 平铺 + value 数字 + time RFC3339
        assert_eq!(rows[0]["__name__"], "up");
        assert_eq!(rows[0]["job"], "j");
        assert_eq!(rows[0]["value"], 1.0);
        assert_eq!(rows[0]["time"], "2024-12-31T16:00:00Z");
    }

    #[test]
    fn rows_from_data_vector_scalar_string() {
        let data = serde_json::json!({
            "resultType": "vector",
            "result": [
                {"metric": {"__name__": "up"}, "value": [1735660800, "0"]},
                {"metric": {"__name__": "up"}, "value": [1735660815, "1"]}
            ]
        });
        let (rows, truncated) = rows_from_data(&data, 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!truncated);

        let data = serde_json::json!({"resultType": "scalar", "result": [1735660800, "42"]});
        let (rows, _) = rows_from_data(&data, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["value"], 42.0);
        assert_eq!(rows[0]["time"], "2024-12-31T16:00:00Z");

        // string 结果类型（crate 不支持，自解析覆盖）
        let data = serde_json::json!({"resultType": "string", "result": [1735660800, "hello"]});
        let (rows, _) = rows_from_data(&data, 100).unwrap();
        assert_eq!(rows[0]["value"], "hello");

        // 非数值/非有限值原样保留
        let data = serde_json::json!({"resultType": "scalar", "result": [1735660800, "NaN"]});
        let (rows, _) = rows_from_data(&data, 100).unwrap();
        assert_eq!(rows[0]["value"], "NaN");

        // 未知 resultType → 错误
        let data = serde_json::json!({"resultType": "weird", "result": []});
        assert!(rows_from_data(&data, 100).is_err());
    }

    // —— 手搓最小 HTTP mock（路由按路径前缀匹配，query 在 URL 里） ——

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 返回 (url, token)。带 token 时校验请求带 Bearer 头。
    async fn spawn_mock(token: Option<&str>) -> String {
        let token = token.map(str::to_string);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                let mut buf = [0u8; 8192];
                let mut req = Vec::new();
                loop {
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(h) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..h]).to_string();
                        let cl = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if req.len() >= h + 4 + cl {
                            break;
                        }
                    }
                }
                let req_text = String::from_utf8_lossy(&req).to_string();
                let lower = req_text.to_ascii_lowercase();
                // 所有 mock 端点都要求鉴权头（当配置了 token 时）
                if let Some(t) = &token {
                    assert!(
                        lower.contains(&format!("authorization: bearer {t}")),
                        "missing bearer auth: {req_text}"
                    );
                }
                let path = req_text.split(' ').nth(1).unwrap_or_default();
                let (status, body) = if path.starts_with("/api/v1/query?") {
                    if path.contains("1%2B1") {
                        // 标量结果（crate 类型化路径不支持，走自解析）
                        (
                            "200 OK",
                            r#"{"status":"success","data":{"resultType":"scalar","result":[1735660800,"2"]}}"#.to_string(),
                        )
                    } else if path.contains("query=nope") {
                        (
                            "400 Bad Request",
                            r#"{"status":"error","errorType":"bad_data","error":"invalid expression"}"#.to_string(),
                        )
                    } else {
                        (
                            "200 OK",
                            r#"{"status":"success","data":{"resultType":"vector","result":[
                                {"metric":{"__name__":"up","job":"prometheus"},"value":[1735660800,"1"]}]}}"#
                                .to_string(),
                        )
                    }
                } else if path.starts_with("/api/v1/label/") && path.contains("/values") {
                    (
                        "200 OK",
                        r#"{"status":"success","data":["up","prometheus_tsdb_head_series"]}"#.to_string(),
                    )
                } else if path.starts_with("/api/v1/series") {
                    // 按 match 选择器里的指标名过滤（mock 不做 PromQL，只做字符串包含）
                    if path.contains("does_not_exist") {
                        ("200 OK", r#"{"status":"success","data":[]}"#.to_string())
                    } else {
                        (
                            "200 OK",
                            r#"{"status":"success","data":[{"__name__":"up","job":"prometheus","instance":"127.0.0.1:9090"}]}"#.to_string(),
                        )
                    }
                } else if path.starts_with("/api/v1/metadata") {
                    (
                        "200 OK",
                        r#"{"status":"success","data":{"up":[{"type":"gauge","help":"Up 1=alive","unit":""}]}}"#.to_string(),
                    )
                } else {
                    ("404 Not Found", r#"{"status":"error","errorType":"not_found","error":"nope"}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        format!("http://{addr}")
    }

    fn env_for(url: &str, token: Option<&str>) -> PromEnv {
        PromEnv {
            url: url.to_string(),
            token: token.map(str::to_string),
            max_rows: 100,
            query_timeout: std::time::Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn full_path_query_and_schema() {
        let url = spawn_mock(Some("tok-1")).await;
        let tools = PromTools::start(env_for(&url, Some("tok-1"))).await.unwrap();

        // 即时查询：信封形状 + 行内容 + labels 平铺
        let out = tools.query("up", None, None, None, None, None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["ok"], true);
        assert_eq!(v["meta"]["connection"], "prometheus");
        assert_eq!(v["meta"]["row_count"], 1);
        assert_eq!(v["rows"][0]["__name__"], "up");
        assert_eq!(v["rows"][0]["job"], "prometheus");
        assert_eq!(v["rows"][0]["value"], 1.0);
        assert_eq!(v["rows"][0]["time"], "2024-12-31T16:00:00Z");

        // 标量查询（crate 0.9.0 类型化路径的坏场景，自解析覆盖）
        let out = tools.query("1+1", None, None, None, None, None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["meta"]["row_count"], 1);
        assert_eq!(v["rows"][0]["value"], 2.0, "{out}");

        // 区间查询参数不齐 → 拒绝
        let out = tools.query("up", None, Some("1735660800"), None, None, None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "QUERY_REJECTED");

        // 坏时间格式 → 拒绝
        let out = tools
            .query("up", Some("yesterday"), None, None, None, None)
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "QUERY_REJECTED");

        // 空 query → 拒绝
        let out = tools.query("  ", None, None, None, None, None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "QUERY_REJECTED");

        // 指标清单
        let out = tools.schema(None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["metric_count"], 2);
        assert_eq!(v["metrics"][0], "prometheus_tsdb_head_series"); // 已排序
        assert_eq!(v["meta"]["connection"], "prometheus");

        // 指标明细：type/help + label 键并集（去掉 __name__）
        let out = tools.schema(Some("up")).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["metric"], "up");
        assert_eq!(v["metric_type"], "gauge");
        assert_eq!(v["help"], "Up 1=alive");
        assert_eq!(v["labels"], serde_json::json!(["instance", "job"]));
    }

    #[tokio::test]
    async fn server_error_maps_to_query_error() {
        let url = spawn_mock(None).await;
        let tools = PromTools::start(env_for(&url, None)).await.unwrap();
        let out = tools.query("nope", None, None, None, None, None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "QUERY_ERROR");
        assert_eq!(v["error"]["message"], "invalid expression");
        assert!(!v["error"]["hint"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_metric_reports_query_error() {
        let url = spawn_mock(None).await;
        let tools = PromTools::start(env_for(&url, None)).await.unwrap();
        let out = tools.schema(Some("does_not_exist")).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "QUERY_ERROR");
        assert!(v["error"]["hint"].as_str().unwrap().contains("prom_schema"));
    }

    #[test]
    fn envelope_shapes_are_stable() {
        // 信封字段序契约快照（只增不改）
        let e = QueryEnvelope {
            v: 1,
            ok: true,
            rows: vec![],
            meta: QueryMeta {
                row_count: 0,
                truncated: false,
                duration_ms: 1,
                connection: "prometheus",
            },
            warnings: vec![],
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.starts_with(r#"{"v":1,"ok":true,"rows":"#));
    }
}
