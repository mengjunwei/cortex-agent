//! InfluxDB 时序数据库只读查询工具集（v2 Flux / v3 SQL・InfluxQL 统一工具面）。
//!
//! 查询语言由服务版本决定：v2 = Flux（`POST /api/v2/query`，annotated CSV，
//! reqwest 直连 —— 官方无 Rust 客户端）；v3 = SQL|InfluxQL（官方
//! `influxdb3-client`，Arrow Flight）。MCP 工具面两个入口：
//! [`InfluxTools::query`]（influx_query）与 [`InfluxTools::schema`]（influx_schema），
//! 输出与 db_* 同款 JSON 信封 `{"v":1,"ok":...}`，错误码契约见 [`code`]
//! （封闭列表：CONNECTION_FAILED / AUTH_FAILED / QUERY_REJECTED / QUERY_ERROR /
//! SERVER_ERROR / TIMEOUT / INTERNAL，hint 必填）。
//!
//! 只读防线（不完美但诚实，纵深防御请配只读 token）：
//! - v2 Flux：函数级黑名单（`to` / `http.*` / `sql.*` / `socket.*` 等副作用函数）
//! - v3：语句首关键字白名单（SELECT/SHOW/WITH/DESCRIBE/EXPLAIN）+ 单语句
//! - 两者都有行数上限（INFLUX_MAX_ROWS，默认 100 / 硬上限 1000）、
//!   v2 响应体 8 MiB 上限、单查超时（INFLUX_TIMEOUT_SECS，默认 30s / 硬上限 300s）
//!
//! # 退出码约定
//!
//! INFLUX_* 配置无效或启动自检失败（/health、token 验证、最小查询）→ 进程以
//! **exit code 2** 退出（stderr 中文说明），cortex 的 MCP 探活立即转红，下次
//! 重新拉起进程自愈。

pub mod config;
mod v2;
mod v3;

pub use config::InfluxEnv;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

/// 错误码契约：封闭列表，只增不改（模型按 code 分支，改语义 = 破坏契约）。
pub mod code {
    pub const CONNECTION_FAILED: &str = "CONNECTION_FAILED";
    pub const AUTH_FAILED: &str = "AUTH_FAILED";
    /// 本地护栏拒绝（只读黑名单/白名单、方言不匹配、空查询）
    pub const QUERY_REJECTED: &str = "QUERY_REJECTED";
    /// 服务器判定查询非法
    pub const QUERY_ERROR: &str = "QUERY_ERROR";
    pub const SERVER_ERROR: &str = "SERVER_ERROR";
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const INTERNAL: &str = "INTERNAL";
}

/// 工具层错误（模型可见，一律英文；hint 必填 —— 没有可行动提示的错误不发）。
pub(crate) struct ToolError {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
}

enum Backend {
    V2(v2::V2Client),
    // V3Client（官方客户端，含 Flight 通道）远大于 V2Client：装箱抹平差异
    V3(Box<v3::V3Client>),
}

/// 统一工具门面：server.rs 只见 query/schema 两个入口 + 启动自检。
///
/// Arc：ToolServer 需要 Clone（rmcp 注册面）；方法全是 &self，共享无副作用。
#[derive(Clone)]
pub struct InfluxTools(Arc<Inner>);

struct Inner {
    env: InfluxEnv,
    backend: Backend,
}

impl InfluxTools {
    /// 构建 + 启动自检（v2：/health + token 验证；v3：ping + 最小查询）。
    /// Err（中文，操作者可见）→ main exit 2。
    pub async fn start(env: InfluxEnv) -> Result<InfluxTools, String> {
        let backend = match env.version {
            config::InfluxVersion::V2 => {
                let Some(org) = env.org.clone() else {
                    return Err("INFLUX_VERSION=2 需要 INFLUX_ORG".into());
                };
                let c = v2::V2Client::new(&env.url, &env.token, &org, env.query_timeout)?;
                c.health().await?;
                Backend::V2(c)
            }
            config::InfluxVersion::V3 => {
                let Some(db) = env.database.clone() else {
                    return Err("INFLUX_VERSION=3 需要 INFLUX_DATABASE".into());
                };
                let c = v3::V3Client::connect(&env.url, &env.token, &db).await?;
                c.health().await?;
                Backend::V3(Box::new(c))
            }
        };
        Ok(InfluxTools(Arc::new(Inner { env, backend })))
    }

    /// influx_query：单条只读查询（JSON 信封输出）。
    /// dialect：v2 固定 flux（默认）；v3 = sql（默认）| influxql。
    pub async fn query(&self, q: &str, dialect: Option<&str>, limit: Option<u64>) -> String {
        let inner = &self.0;
        let max = inner.env.max_rows;
        let limit = limit.unwrap_or(max as u64).clamp(1, max as u64) as usize;
        let t0 = Instant::now();
        let dialect = dialect
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase());

        match &inner.backend {
            Backend::V2(c) => {
                let d = dialect.unwrap_or_else(|| "flux".into());
                if d != "flux" {
                    return err_envelope(
                        code::QUERY_REJECTED,
                        &format!("dialect \"{d}\" is not available on this server"),
                        "this tool is connected to InfluxDB 2.x: use Flux (the default dialect), \
                         e.g. from(bucket: \"m\") |> range(start: -1h)",
                    );
                }
                if q.trim().is_empty() {
                    return err_envelope(
                        code::QUERY_REJECTED,
                        "query is empty",
                        "provide a Flux query, e.g. from(bucket: \"m\") |> range(start: -1h) \
                         |> filter(fn: (r) => r._measurement == \"cpu\")",
                    );
                }
                if let Some(f) = v2::flux_denied_function(q) {
                    return err_envelope(
                        code::QUERY_REJECTED,
                        &format!("Flux function \"{f}()\" is not allowed: this tool is read-only"),
                        &format!(
                            "remove the {f}() call. If \"{f}\" only appears inside a string \
                             literal, reword the literal so it is not directly followed by \"(\"."
                        ),
                    );
                }
                let timeout = inner.env.query_timeout;
                match tokio::time::timeout(timeout, c.query(q)).await {
                    Err(_) => err_envelope(
                        code::TIMEOUT,
                        &format!("query exceeded the {}s budget", timeout.as_secs()),
                        "narrow the time range (range start/stop) or aggregate \
                         (e.g. aggregateWindow) to reduce work",
                    ),
                    Ok(Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                    Ok(Ok(raw)) => {
                        let mut parsed = v2::parse_annotated_csv(&raw.body, limit);
                        if let Some(se) = parsed.server_error.take() {
                            return err_envelope(
                                code::QUERY_ERROR,
                                &se,
                                "the server failed the query mid-stream; check \
                                 measurement/field names and the time range",
                            );
                        }
                        let truncated = parsed.truncated || raw.byte_truncated;
                        let mut warnings = Vec::new();
                        if truncated {
                            let note = if raw.byte_truncated {
                                format!(
                                    "result capped at {limit} rows or 8 MiB of CSV, \
                                     whichever hit first"
                                )
                            } else {
                                format!("result capped at {limit} rows")
                            };
                            warnings.push(Warning {
                                code: "TRUNCATED",
                                message: note,
                            });
                        }
                        query_envelope(parsed.rows, truncated, t0.elapsed(), "influxdb2", warnings)
                    }
                }
            }
            Backend::V3(c) => {
                let d = dialect.unwrap_or_else(|| "sql".into());
                if d != "sql" && d != "influxql" {
                    return err_envelope(
                        code::QUERY_REJECTED,
                        &format!("dialect \"{d}\" is not available on this server"),
                        "this tool is connected to InfluxDB 3.x: use \"sql\" (the default) \
                         or \"influxql\"",
                    );
                }
                if q.trim().is_empty() {
                    return err_envelope(
                        code::QUERY_REJECTED,
                        "query is empty",
                        "provide a read-only SQL statement (or an InfluxQL query with \
                         dialect=\"influxql\")",
                    );
                }
                if let Err((why, detail)) = v3::readonly_reject(q) {
                    return err_envelope(code::QUERY_REJECTED, &detail, why);
                }
                let timeout = inner.env.query_timeout;
                match tokio::time::timeout(timeout, c.query(&d, q, limit)).await {
                    Err(_) => err_envelope(
                        code::TIMEOUT,
                        &format!("query exceeded the {}s budget", timeout.as_secs()),
                        "narrow the time range or aggregate to reduce query work",
                    ),
                    Ok(Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                    Ok(Ok((rows, truncated))) => {
                        let mut warnings = Vec::new();
                        if truncated {
                            warnings.push(Warning {
                                code: "TRUNCATED",
                                message: format!("result capped at {limit} rows"),
                            });
                        }
                        query_envelope(rows, truncated, t0.elapsed(), "influxdb3", warnings)
                    }
                }
            }
        }
    }

    /// influx_schema：无参 → bucket（v2）/ database（v3）清单；
    /// 只有 bucket → measurement（v2）/ table（v3）清单；
    /// bucket + measurement → 字段与 tag（v2）/ 列与类型（v3）。
    pub async fn schema(&self, bucket: Option<&str>, measurement: Option<&str>) -> String {
        let inner = &self.0;
        let t0 = Instant::now();
        // 闭包对 Option<&str> 的生命周期推断不通过，用普通函数
        fn norm(s: Option<&str>) -> Option<&str> {
            s.map(str::trim).filter(|s| !s.is_empty())
        }
        let bucket = norm(bucket);
        let measurement = norm(measurement);
        // v2：measurement 明细省略 bucket 时用 INFLUX_BUCKET 兜底（进 match 前解析，
        // 避免 async 递归）
        let bucket = match (&inner.backend, bucket, measurement) {
            (Backend::V2(_), None, Some(_)) => match inner.env.default_bucket.as_deref() {
                Some(b) => Some(b),
                None => {
                    return err_envelope(
                        code::QUERY_REJECTED,
                        "measurement detail requires a bucket",
                        "pass bucket=... explicitly, or set INFLUX_BUCKET so it can be omitted",
                    );
                }
            },
            (_, b, _) => b,
        };

        match &inner.backend {
            Backend::V2(c) => {
                let timeout = inner.env.query_timeout;
                match (bucket, measurement) {
                    // 预解析已保证 v2 不会以 (None, Some) 到达这里；防御性兜底不 panic
                    (None, Some(_)) => err_envelope(
                        code::INTERNAL,
                        "measurement detail without a bucket should have been resolved earlier",
                        "this is a bug in the schema tool; report it",
                    ),
                    (None, None) => {
                        match tokio::time::timeout(timeout, c.buckets()).await {
                            Err(_) => err_envelope(
                                code::TIMEOUT,
                                "bucket listing timed out",
                                "check server load, then retry",
                            ),
                            Ok(Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                            Ok(Ok(buckets)) => {
                                let rows: Vec<BucketRow> = buckets
                                    .into_iter()
                                    .map(|b| BucketRow {
                                        name: b.name,
                                        retention: b.retention,
                                    })
                                    .collect();
                                BucketsEnvelope {
                                    v: 1,
                                    ok: true,
                                    bucket_count: rows.len(),
                                    buckets: rows,
                                    meta: SchemaMeta {
                                        duration_ms: ms(t0.elapsed()),
                                        connection: "influxdb2",
                                    },
                                }
                                .to_json()
                            }
                        }
                    }
                    (Some(b), None) => {
                        let flux = v2::flux_measurements(b);
                        match tokio::time::timeout(timeout, c.scalar_column(&flux, cap())).await {
                            Err(_) => err_envelope(
                                code::TIMEOUT,
                                "measurement listing timed out",
                                "retry; if it persists, the bucket may be very large",
                            ),
                            Ok(Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                            Ok(Ok((names, truncated))) => MeasurementsEnvelope {
                                v: 1,
                                ok: true,
                                bucket: b,
                                measurement_count: names.len(),
                                measurements: names,
                                warnings: truncated_warn(truncated),
                                meta: SchemaMeta {
                                    duration_ms: ms(t0.elapsed()),
                                    connection: "influxdb2",
                                },
                            }
                            .to_json(),
                        }
                    }
                    (Some(b), Some(m)) => {
                        let fields = tokio::time::timeout(
                            timeout,
                            c.scalar_column(&v2::flux_field_keys(b, m), cap()),
                        )
                        .await;
                        let fields = match fields {
                            Err(_) => {
                                return err_envelope(
                                    code::TIMEOUT,
                                    "field listing timed out",
                                    "retry; if it persists, narrow to a different measurement",
                                )
                            }
                            Ok(r) => r,
                        };
                        let tags = tokio::time::timeout(
                            timeout,
                            c.scalar_column(&v2::flux_tag_keys(b, m), cap()),
                        )
                        .await;
                        let tags = match tags {
                            Err(_) => {
                                return err_envelope(
                                    code::TIMEOUT,
                                    "tag listing timed out",
                                    "retry; if it persists, narrow to a different measurement",
                                )
                            }
                            Ok(r) => r,
                        };
                        match (fields, tags) {
                            (Ok((fields, f_trunc)), Ok((mut tags, t_trunc))) => {
                                tags.retain(|t| !v2::SYSTEM_COLUMNS.contains(&t.as_str()));
                                MeasurementDetail {
                                    v: 1,
                                    ok: true,
                                    bucket: b,
                                    measurement: m,
                                    fields,
                                    tags,
                                    warnings: truncated_warn(f_trunc || t_trunc),
                                    meta: SchemaMeta {
                                        duration_ms: ms(t0.elapsed()),
                                        connection: "influxdb2",
                                    },
                                }
                                .to_json()
                            }
                            (Err(e), _) | (_, Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                        }
                    }
                }
            }
            Backend::V3(c) => match (bucket, measurement) {
                (None, None) => {
                    let timeout = inner.env.query_timeout;
                    match tokio::time::timeout(timeout, c.databases()).await {
                        Err(_) => err_envelope(
                            code::TIMEOUT,
                            "database listing timed out",
                            "check server load, then retry",
                        ),
                        Ok(Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                        Ok(Ok(names)) => DatabasesEnvelope {
                            v: 1,
                            ok: true,
                            database_count: names.len(),
                            databases: names,
                            meta: SchemaMeta {
                                duration_ms: ms(t0.elapsed()),
                                connection: "influxdb3",
                            },
                        }
                        .to_json(),
                    }
                }
                (db, m) => {
                    // v3：一条 MCP 进程绑定一个 database（官方客户端查询不带
                    // per-query db）。显式给了别的库名 → 拒绝并说明。
                    if let Some(db) = db {
                        if !db.eq_ignore_ascii_case(c.database()) {
                            return err_envelope(
                                code::QUERY_REJECTED,
                                &format!(
                                    "database \"{db}\" is not served by this process \
                                     (bound to \"{}\")",
                                    c.database()
                                ),
                                "this MCP process is bound to one INFLUX_DATABASE; add \
                                 another MCP server entry for other databases",
                            );
                        }
                    }
                    let timeout = inner.env.query_timeout;
                    match m {
                        None => {
                            match tokio::time::timeout(timeout, c.tables()).await {
                                Err(_) => err_envelope(
                                    code::TIMEOUT,
                                    "table listing timed out",
                                    "check server load, then retry",
                                ),
                                Ok(Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                                Ok(Ok(names)) => TablesEnvelope {
                                    v: 1,
                                    ok: true,
                                    database: c.database().to_string(),
                                    table_count: names.len(),
                                    tables: names,
                                    meta: SchemaMeta {
                                        duration_ms: ms(t0.elapsed()),
                                        connection: "influxdb3",
                                    },
                                }
                                .to_json(),
                            }
                        }
                        Some(table) => {
                            let cols =
                                tokio::time::timeout(timeout, c.columns(table)).await;
                            match cols {
                                Err(_) => err_envelope(
                                    code::TIMEOUT,
                                    "column listing timed out",
                                    "check server load, then retry",
                                ),
                                Ok(Err(e)) => err_envelope(e.code, &e.message, &e.hint),
                                Ok(Ok(columns)) => TableDetail {
                                    v: 1,
                                    ok: true,
                                    database: c.database().to_string(),
                                    table,
                                    columns: columns
                                        .into_iter()
                                        .map(|(name, data_type)| ColumnInfo { name, data_type })
                                        .collect(),
                                    meta: SchemaMeta {
                                        duration_ms: ms(t0.elapsed()),
                                        connection: "influxdb3",
                                    },
                                }
                                .to_json(),
                            }
                        }
                    }
                }
            },
        }
    }
}

/// 元数据清单（measurement 列表等）的行数上限：比查询行数上限宽，但仍有界。
fn cap() -> usize {
    config::HARD_MAX_ROWS
}

fn ms(d: Duration) -> u64 {
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

// —— JSON 信封 v1（字段序即序列化序；契约与 db_* 对齐，只增不改） ——

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

fn query_envelope(
    rows: Vec<Value>,
    truncated: bool,
    d: Duration,
    connection: &'static str,
    warnings: Vec<Warning>,
) -> String {
    QueryEnvelope {
        v: 1,
        ok: true,
        meta: QueryMeta {
            row_count: rows.len(),
            truncated,
            duration_ms: ms(d),
            connection,
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
struct BucketRow {
    name: String,
    retention: String,
}

#[derive(Serialize)]
struct BucketsEnvelope {
    v: u8,
    ok: bool,
    bucket_count: usize,
    buckets: Vec<BucketRow>,
    meta: SchemaMeta,
}

impl ToJson for BucketsEnvelope {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[derive(Serialize)]
struct MeasurementsEnvelope<'a> {
    v: u8,
    ok: bool,
    bucket: &'a str,
    measurement_count: usize,
    measurements: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<Warning>,
    meta: SchemaMeta,
}

impl ToJson for MeasurementsEnvelope<'_> {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[derive(Serialize)]
struct MeasurementDetail<'a> {
    v: u8,
    ok: bool,
    bucket: &'a str,
    measurement: &'a str,
    fields: Vec<String>,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<Warning>,
    meta: SchemaMeta,
}

impl ToJson for MeasurementDetail<'_> {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[derive(Serialize)]
struct DatabasesEnvelope {
    v: u8,
    ok: bool,
    database_count: usize,
    databases: Vec<String>,
    meta: SchemaMeta,
}

impl ToJson for DatabasesEnvelope {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[derive(Serialize)]
struct TablesEnvelope {
    v: u8,
    ok: bool,
    database: String,
    table_count: usize,
    tables: Vec<String>,
    meta: SchemaMeta,
}

impl ToJson for TablesEnvelope {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[derive(Serialize)]
struct ColumnInfo {
    name: String,
    data_type: String,
}

#[derive(Serialize)]
struct TableDetail<'a> {
    v: u8,
    ok: bool,
    database: String,
    table: &'a str,
    columns: Vec<ColumnInfo>,
    meta: SchemaMeta,
}

impl ToJson for TableDetail<'_> {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|_| "{\"v\":1,\"ok\":false}".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::influx::config::InfluxVersion;

    fn env_for(url: &str) -> InfluxEnv {
        InfluxEnv {
            version: InfluxVersion::V2,
            url: url.to_string(),
            token: "test-token".into(),
            org: Some("resolink".into()),
            database: None,
            default_bucket: Some("mnet".into()),
            max_rows: 100,
            query_timeout: StdDuration::from_secs(5),
        }
    }

    /// 手搓最小 HTTP/1.1 mock：按路由表应答（GET /health、GET /api/v2/buckets、
    /// POST /api/v2/query）。校验查询请求的鉴权头（hyper 发小写头名，做大小写
    /// 不敏感匹配）与 JSON 体。
    async fn spawn_mock() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                // 串行处理即可（reqwest 每请求一连接）
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
                let (status, ctype, body) = if req_text.starts_with("GET /health") {
                    (
                        "200 OK",
                        "application/json",
                        "{\"status\":\"pass\"}".to_string(),
                    )
                } else if req_text.starts_with("GET /api/v2/buckets") {
                    (
                        "200 OK",
                        "application/json",
                        "{\"buckets\":[{\"name\":\"mnet\",\"retentionRules\":[{\"type\":\"expire\",\"everySeconds\":16416000}]},{\"name\":\"_tasks\",\"retentionRules\":[]}]}".to_string(),
                    )
                } else if req_text.starts_with("POST /api/v2/query") {
                    assert!(
                        lower.contains("authorization: token test-token"),
                        "missing v2 Token auth header: {req_text}"
                    );
                    assert!(req_text.contains("\"query\""), "missing query body");
                    (
                        "200 OK",
                        "application/csv",
                        "#datatype,string,long,dateTime:RFC3339,double,string,string\n\
                         #group,false,false,false,false,true,true\n\
                         #default,_result,,,,,\n\
                         ,result,table,_time,_value,_field,_measurement\n\
                         ,,0,2024-01-01T00:00:00Z,1.5,inflow_value,task_line_port_rate_prod\n\
                         ,,0,2024-01-01T00:01:00Z,2.5,inflow_value,task_line_port_rate_prod\n"
                            .to_string(),
                    )
                } else {
                    ("404 Not Found", "text/plain", "nope".to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn v2_full_path_query_and_schema() {
        let url = spawn_mock().await;
        let tools = InfluxTools::start(env_for(&url)).await.unwrap();

        // 查询：信封形状 + 行内容
        let out = tools
            .query(
                "from(bucket:\"mnet\") |> range(start:-1h)",
                None,
                None,
            )
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["ok"], true);
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);
        assert_eq!(v["rows"][0]["_value"], 1.5);
        assert_eq!(v["rows"][0]["_measurement"], "task_line_port_rate_prod");
        assert_eq!(v["meta"]["row_count"], 2);
        assert_eq!(v["meta"]["connection"], "influxdb2");

        // schema 无参：bucket 清单（系统桶 _tasks 被过滤）
        let out = tools.schema(None, None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["bucket_count"], 1);
        assert_eq!(v["buckets"][0]["name"], "mnet");
        assert_eq!(v["buckets"][0]["retention"], "190d");

        // 只读护栏：to() 拒绝，信封 ok=false + hint
        let out = tools
            .query("from(bucket:\"m\") |> to(bucket:\"x\")", None, None)
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "QUERY_REJECTED");
        assert!(v["error"]["hint"].as_str().unwrap().contains("to()"));

        // 方言不匹配拒绝
        let out = tools.query("SELECT 1", Some("sql"), None).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "QUERY_REJECTED");

        // 省略 bucket + INFLUX_BUCKET 兜底（走 query 端点 → measurements 解析）
        let out = tools.schema(None, Some("task_line_port_rate_prod")).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn v2_measurement_without_bucket_or_default_is_rejected() {
        let url = spawn_mock().await;
        let mut env = env_for(&url);
        env.default_bucket = None;
        let tools = InfluxTools::start(env).await.unwrap();
        let out = tools.schema(None, Some("m")).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "QUERY_REJECTED");
        assert!(v["error"]["hint"].as_str().unwrap().contains("INFLUX_BUCKET"));
    }
}
