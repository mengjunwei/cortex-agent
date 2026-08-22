//! InfluxDB v2 HTTP 客户端（查询 + 元数据）+ Flux 只读护栏 + annotated CSV 解析。
//!
//! 官方无 Rust 客户端（InfluxData 仅维护 Go/Java/JS/Python 等），社区 influxdb2
//! 停更于 2024-07 且锁 reqwest 0.11，故直接实现 v2 REST：
//! - 查询：`POST /api/v2/query?org=...`，`Authorization: Token <token>`，JSON 体
//!   `{"query": <flux>, "dialect": {"annotations": ["datatype"]}}`，响应 annotated CSV
//! - bucket 清单：`GET /api/v2/buckets?org=...`
//! - measurement / 字段 / tag：Flux `influxdata/influxdb/schema` 包（同查询通道）
//!
//! 只读防线：Flux 是管道语言，无 SQL 那样的语句白名单，采用**函数级黑名单**
//! （`to` / `http.*` / `sql.*` / `socket.*` / `kafka.to` 等副作用函数），外加
//! 纵深防御建议：给工具配只读 token。

use std::time::Duration;

use futures_util::stream::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use reqwest::{Client, Response, StatusCode};
use serde_json::{Map, Value};

use super::{code, ToolError};

/// 响应体字节上限：防 runaway Flux 把上百 MB CSV 灌进内存/上下文。
/// 截断时置 truncated（TRUNCATED 警告），宁可少给也不 OOM。
const BODY_BYTE_CAP: usize = 8 * 1024 * 1024;

/// v2 查询的原始产物（解析前的 CSV 体 + 是否被字节上限截断）。
pub(crate) struct RawQuery {
    pub body: String,
    pub byte_truncated: bool,
}

/// annotated CSV 解析产物。
pub(crate) struct CsvOutcome {
    /// 每行一个 JSON 对象（列名 → 按 #dataType 类型化的值；常量列 `result` 省略）
    pub rows: Vec<Value>,
    /// 达到行数上限（或字节上限）而截断
    pub truncated: bool,
    /// 服务器在 200 响应中携带的 error 表（流中查询失败）
    pub server_error: Option<String>,
}

/// bucket 清单条目。
pub(crate) struct BucketInfo {
    pub name: String,
    /// 人类可读保留期（如 "190d"、"72h"、"inf"）
    pub retention: String,
}

pub(crate) struct V2Client {
    http: Client,
    url: String,
    token: String,
    org: String,
}

impl V2Client {
    pub(crate) fn new(
        url: &str,
        token: &str,
        org: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
        Ok(V2Client {
            http,
            url: url.to_string(),
            token: token.to_string(),
            org: org.to_string(),
        })
    }

    fn auth_header(&self) -> String {
        // v2 用 Token 方案（不是 Bearer）：`Authorization: Token <token>`
        format!("Token {}", self.token)
    }

    /// 启动自检：/health 探活（无鉴权）+ buckets 验证 token 与 org。
    /// Err 中文给操作者 → main exit 2。
    pub(crate) async fn health(&self) -> Result<(), String> {
        let resp = self
            .http
            .get(format!("{}/health", self.url))
            .send()
            .await
            .map_err(|e| format!("InfluxDB 健康检查失败（{}）: {e}", self.url))?;
        let status = resp.status();
        if status != StatusCode::OK {
            return Err(format!(
                "InfluxDB 健康检查异常（{} → HTTP {}）",
                self.url, status
            ));
        }
        // token/org 验证：能列出 bucket 即读权限可用
        let resp = self
            .http
            .get(format!("{}/api/v2/buckets", self.url))
            .query(&[("org", self.org.as_str())])
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| format!("InfluxDB 鉴权探查失败（{}）: {e}", self.url))?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(format!(
                "InfluxDB token 验证失败（HTTP {}）：检查 INFLUX_TOKEN 及其对 org \"{}\" 的读权限",
                resp.status(),
                self.org
            )),
            s => Err(format!(
                "InfluxDB 鉴权探查异常（HTTP {s}）：确认 {}/api/v2/buckets 可访问（INFLUX_VERSION=2 ？）",
                self.url
            )),
        }
    }

    /// 执行 Flux 查询，返回原始 annotated CSV（截断信息由调用方合并处理）。
    pub(crate) async fn query(&self, flux: &str) -> Result<RawQuery, ToolError> {
        let resp = self
            .http
            .post(format!("{}/api/v2/query", self.url))
            .query(&[("org", self.org.as_str())])
            .header(AUTHORIZATION, self.auth_header())
            .header(ACCEPT, "application/csv")
            .json(&serde_json::json!({
                "query": flux,
                "dialect": { "annotations": ["datatype"] }
            }))
            .send()
            .await
            .map_err(map_reqwest)?;
        let status = resp.status();
        if status != StatusCode::OK {
            return Err(status_error(resp).await);
        }
        // 流式读取 + 字节上限（行数上限管不住 SELECT * 全表这类响应体）
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
        let mut byte_truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest)?;
            if buf.len() + chunk.len() > BODY_BYTE_CAP {
                byte_truncated = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(RawQuery {
            body: String::from_utf8_lossy(&buf).into_owned(),
            byte_truncated,
        })
    }

    /// bucket 清单（GET /api/v2/buckets）。
    pub(crate) async fn buckets(&self) -> Result<Vec<BucketInfo>, ToolError> {
        let resp = self
            .http
            .get(format!("{}/api/v2/buckets", self.url))
            .query(&[("org", self.org.as_str())])
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(map_reqwest)?;
        let status = resp.status();
        if status != StatusCode::OK {
            return Err(status_error(resp).await);
        }
        let json: Value = resp.json().await.map_err(|e| ToolError {
            code: code::SERVER_ERROR,
            message: format!("cannot parse /api/v2/buckets response: {e}"),
            hint: "retry; if it persists, check the server version is InfluxDB 2.x".into(),
        })?;
        let mut out = Vec::new();
        if let Some(arr) = json.get("buckets").and_then(Value::as_array) {
            for b in arr {
                let name = b.get("name").and_then(Value::as_str).unwrap_or_default();
                if name.is_empty() || name.starts_with('_') {
                    continue; // _monitoring/_tasks 系统桶：对模型是噪声
                }
                out.push(BucketInfo {
                    name: name.to_string(),
                    retention: retention_of(b),
                });
            }
        }
        Ok(out)
    }

    /// 单列（_value）Flux 查询：schema.measurements / measurementFieldKeys /
    /// measurementTagKeys 的结果形态。返回 (值列表, 是否截断)。
    pub(crate) async fn scalar_column(&self, flux: &str, cap: usize) -> Result<(Vec<String>, bool), ToolError> {
        let raw = self.query(flux).await?;
        let parsed = parse_annotated_csv(&raw.body, cap);
        if let Some(se) = parsed.server_error {
            return Err(ToolError {
                code: code::QUERY_ERROR,
                message: se,
                hint: "the server rejected the metadata query; check bucket/measurement spelling"
                    .into(),
            });
        }
        let mut out = Vec::with_capacity(parsed.rows.len());
        for row in &parsed.rows {
            if let Some(s) = row.get("_value").and_then(Value::as_str) {
                out.push(s.to_string());
            }
        }
        Ok((out, parsed.truncated || raw.byte_truncated))
    }
}

// —— 元数据 Flux 构造（schema 包） ——

/// Flux 字符串字面量转义（bucket / measurement 名内插前必须转义）
pub(crate) fn flux_string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 注意 `start: 0`：schema 包的 field/tag keys 查询默认只扫近期窗口，
/// 老数据（历史 bucket / 回填）会被静默排除 —— 元数据查询必须全时段。
pub(crate) fn flux_measurements(bucket: &str) -> String {
    format!(
        "import \"influxdata/influxdb/schema\"\nschema.measurements(bucket: {}, start: 0)",
        flux_string_literal(bucket)
    )
}

pub(crate) fn flux_field_keys(bucket: &str, measurement: &str) -> String {
    format!(
        "import \"influxdata/influxdb/schema\"\nschema.measurementFieldKeys(bucket: {}, measurement: {}, start: 0)",
        flux_string_literal(bucket),
        flux_string_literal(measurement)
    )
}

pub(crate) fn flux_tag_keys(bucket: &str, measurement: &str) -> String {
    format!(
        "import \"influxdata/influxdb/schema\"\nschema.measurementTagKeys(bucket: {}, measurement: {}, start: 0)",
        flux_string_literal(bucket),
        flux_string_literal(measurement)
    )
}

/// `measurementTagKeys` 会把系统列（_start/_stop/_field/_measurement）一并
/// 当作 tag 返回 —— 对模型是噪声，过滤掉。
pub(crate) const SYSTEM_COLUMNS: &[&str] = &["_start", "_stop", "_field", "_measurement"];

// —— Flux 只读护栏 ——

/// Flux 副作用函数黑名单：命中（作为**函数调用**出现）返回函数名。
/// 匹配规则：名字前无标识符字符（`A-Za-z0-9_.`），名字后跳过空白紧跟 `(` ——
/// 因此 `total(`、`auto(`、`schema.to(...)` 不会误伤 `to`，`|> to(...)` 会命中。
pub(crate) fn flux_denied_function(flux: &str) -> Option<&'static str> {
    const DENY: &[&str] = &[
        "to",
        "experimental.to",
        "http.get",
        "http.post",
        "http.requests",
        "socket.from",
        "socket.to",
        "sql.from",
        "sql.to",
        "kafka.to",
    ];
    let lower = flux.to_ascii_lowercase();
    for name in DENY {
        let mut search = 0usize;
        while let Some(pos) = lower[search..].find(name) {
            let abs = search + pos;
            let before_ok = lower[..abs]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'));
            if before_ok {
                let after = lower[abs + name.len()..].trim_start();
                if after.starts_with('(') {
                    return Some(name);
                }
            }
            search = abs + name.len();
        }
    }
    None
}

// —— annotated CSV 解析 ——

/// 解析 v2 查询的 annotated CSV。纯函数，便于单测。
///
/// 格式要点：注释行 `#datatype,...` / `#group,...` / `#default,...`；表头行首列
/// 为空（annotation 列），如 `,result,table,_time,...`；多表结果会重复
/// 「注释块 + 表头」；查询失败时服务器在 200 响应里给 `,error,reference` 表。
/// `#datatype` 类型映射：long/unsigned → 数值，double → 浮点，boolean → 布尔，
/// dateTime / duration / string → 字符串；非 string 类型的空单元格 → null。
pub(crate) fn parse_annotated_csv(body: &str, max_rows: usize) -> CsvOutcome {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(body.as_bytes());

    let mut out = CsvOutcome {
        rows: Vec::new(),
        truncated: false,
        server_error: None,
    };
    let mut header: Vec<String> = Vec::new();
    let mut dtypes: Vec<String> = Vec::new(); // dtypes[i] ↔ header[i]
    let mut in_error_table = false;

    for r in rdr.records() {
        let r = match r {
            Ok(r) => r,
            // 尾部被字节上限截断的残行：丢弃已够用的部分
            Err(_) => break,
        };
        let first = r.get(0).unwrap_or("");
        if first == "#datatype" {
            dtypes = r.iter().skip(1).map(|s| s.to_string()).collect();
            continue;
        }
        if first.starts_with('#') {
            continue; // #group / #default：解析不需要
        }
        if first.is_empty() && r.get(1) == Some("error") {
            in_error_table = true;
            continue;
        }
        if in_error_table {
            if let Some(msg) = r.get(1) {
                if !msg.is_empty() {
                    out.server_error = Some(msg.to_string());
                }
            }
            continue;
        }
        if first.is_empty() && r.get(1) == Some("result") && r.get(2) == Some("table") {
            header = r.iter().skip(1).map(|s| s.to_string()).collect();
            continue;
        }
        if header.is_empty() {
            continue; // 没有表头的孤儿行（不应出现）
        }
        if out.rows.len() >= max_rows {
            out.truncated = true;
            break;
        }
        let mut obj = Map::with_capacity(header.len());
        for (i, name) in header.iter().enumerate() {
            // 常量列 `result`（恒为 _result）与空名列是 token 噪声，省略
            if name == "result" || name.is_empty() {
                continue;
            }
            let dt = dtypes.get(i).map(String::as_str).unwrap_or("string");
            obj.insert(name.clone(), convert_field(r.get(i + 1), dt));
        }
        out.rows.push(Value::Object(obj));
    }
    out
}

fn convert_field(raw: Option<&str>, dtype: &str) -> Value {
    let Some(s) = raw else { return Value::Null };
    if s.is_empty() && dtype != "string" {
        return Value::Null;
    }
    match dtype {
        "long" => s
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(s.to_string())),
        "unsigned" => s
            .parse::<u64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(s.to_string())),
        "double" => s
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(s.to_string())),
        "boolean" => match s {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(s.to_string()),
        },
        _ => Value::String(s.to_string()), // string / dateTime:* / duration / 未知
    }
}

// —— 错误映射 ——

fn map_reqwest(e: reqwest::Error) -> ToolError {
    if e.is_timeout() {
        ToolError {
            code: code::TIMEOUT,
            message: format!("request timed out: {e}"),
            hint: "narrow the time range (range start/stop) or aggregate to reduce query work"
                .into(),
        }
    } else {
        ToolError {
            code: code::CONNECTION_FAILED,
            message: format!("cannot reach InfluxDB: {e}"),
            hint: "check that the server is running and reachable from this process".into(),
        }
    }
}

/// 非 200 响应 → 工具错误（尽量带服务器 message）。
async fn status_error(resp: Response) -> ToolError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let server_msg = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let t = body.trim();
            if t.is_empty() {
                format!("HTTP {status}")
            } else {
                // 非 JSON 体（如反代 HTML）：掐头去尾取一小段，别把整页 HTML 灌给模型
                let mut s: String = t.chars().take(300).collect();
                if t.chars().count() > 300 {
                    s.push('…');
                }
                s
            }
        });
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ToolError {
            code: code::AUTH_FAILED,
            message: server_msg,
            hint: "check the token value and that it has read access to the configured org"
                .into(),
        },
        StatusCode::BAD_REQUEST => ToolError {
            code: code::QUERY_ERROR,
            message: server_msg,
            hint: "the server rejected the query; fix the syntax or check bucket/measurement/field names"
                .into(),
        },
        StatusCode::NOT_FOUND => ToolError {
            code: code::CONNECTION_FAILED,
            message: server_msg,
            hint: "endpoint not found; verify INFLUX_URL points to an InfluxDB 2.x server"
                .into(),
        },
        StatusCode::TOO_MANY_REQUESTS => ToolError {
            code: code::SERVER_ERROR,
            message: server_msg,
            hint: "rate limited; wait and retry with a narrower query".into(),
        },
        s if s.is_server_error() => ToolError {
            code: code::SERVER_ERROR,
            message: server_msg,
            hint: "InfluxDB server-side failure; retry later or narrow the query".into(),
        },
        s => ToolError {
            code: code::QUERY_ERROR,
            message: format!("unexpected HTTP {s}: {server_msg}"),
            hint: "unexpected response shape; verify the server is InfluxDB 2.x".into(),
        },
    }
}

/// retentionRules → 人类可读（expire 类型取 everySeconds）。
fn retention_of(bucket: &Value) -> String {
    let Some(rules) = bucket.get("retentionRules").and_then(Value::as_array) else {
        return "inf".into();
    };
    for r in rules {
        if r.get("type").and_then(Value::as_str) == Some("expire") {
            if let Some(secs) = r.get("everySeconds").and_then(Value::as_u64) {
                return humanize_secs(secs);
            }
        }
    }
    "inf".into()
}

fn humanize_secs(secs: u64) -> String {
    if secs == 0 {
        return "inf".into();
    }
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
#datatype,string,long,dateTime:RFC3339,double,string,string,string
#group,false,false,false,false,true,true,true
#default,_result,,,,,,
,result,table,_time,_value,_field,_measurement,host
,,0,2024-01-01T00:00:00Z,1.5,usage_user,cpu,servera
,,0,2024-01-01T00:01:00Z,2.5,usage_user,cpu,servera
,,1,2024-01-01T00:00:00Z,,usage_system,cpu,serverb
";

    #[test]
    fn parses_typed_rows_and_drops_constant_columns() {
        let out = parse_annotated_csv(SAMPLE, 100);
        assert_eq!(out.rows.len(), 3);
        assert!(!out.truncated);
        let r0 = out.rows[0].as_object().unwrap();
        // result 列被省略；table 是 long；空 double 是 null
        assert!(!r0.contains_key("result"));
        assert_eq!(r0["table"], Value::from(0));
        assert_eq!(r0["_value"], Value::from(1.5));
        assert_eq!(r0["_time"], "2024-01-01T00:00:00Z");
        assert_eq!(r0["host"], "servera");
        assert_eq!(out.rows[2].as_object().unwrap()["_value"], Value::Null);
    }

    #[test]
    fn parses_multiple_tables_with_repeated_headers() {
        let body = "\
#datatype,string,long,dateTime:RFC3339,double,string
,result,table,_time,_value,_field
,,0,2024-01-01T00:00:00Z,1,up
#datatype,string,long,dateTime:RFC3339,double,string,string
,result,table,_time,_value,_field,host
,,0,2024-01-01T00:00:00Z,2,up,h1
";
        let out = parse_annotated_csv(body, 100);
        assert_eq!(out.rows.len(), 2);
        assert!(!out.rows[0].as_object().unwrap().contains_key("host"));
        assert_eq!(out.rows[1].as_object().unwrap()["host"], "h1");
    }

    #[test]
    fn boolean_and_string_typing() {
        let body = "\
#datatype,string,long,string,boolean
,result,table,sensor,on
,,0,s1,true
,,0,s2,false
";
        let out = parse_annotated_csv(body, 100);
        assert_eq!(out.rows[0]["on"], Value::Bool(true));
        assert_eq!(out.rows[1]["on"], Value::Bool(false));
    }

    #[test]
    fn row_cap_truncates() {
        let out = parse_annotated_csv(SAMPLE, 2);
        assert_eq!(out.rows.len(), 2);
        assert!(out.truncated);
    }

    #[test]
    fn error_table_becomes_server_error() {
        let body = "\
,error,reference
,compilation failed: undefined identifier foo,
";
        let out = parse_annotated_csv(body, 100);
        assert!(out.rows.is_empty());
        assert_eq!(
            out.server_error.as_deref(),
            Some("compilation failed: undefined identifier foo")
        );
    }

    #[test]
    fn empty_and_annotation_only_body() {
        let out = parse_annotated_csv("", 100);
        assert!(out.rows.is_empty());
        let out = parse_annotated_csv("#group,false\n#default,_result\n", 100);
        assert!(out.rows.is_empty());
    }

    #[test]
    fn truncated_tail_garbage_is_dropped() {
        // 字节截断后最后一条 CSV 记录可能是残行（未闭合引号）：解析失败即止，已收行保留
        let mut body = SAMPLE.to_string();
        body.push_str(",,0,2024-01-01T00:02:00Z,\"unclosed");
        let out = parse_annotated_csv(&body, 100);
        assert!(out.rows.len() >= 3);
    }

    #[test]
    fn quoted_comma_field() {
        let body = "\
#datatype,string,long,string
,result,table,msg
,,0,\"a,b\"
";
        let out = parse_annotated_csv(body, 100);
        assert_eq!(out.rows[0]["msg"], "a,b");
    }

    #[test]
    fn flux_guardrail_deny() {
        for q in [
            "from(bucket:\"b\") |> range(start:-1h) |> to(bucket:\"out\")",
            "http.get(url:\"http://evil\")",
            "sql.from(driverName:\"x\")",
            "experimental.to(bucket:\"o\")",
            "socket.from(\"tcp://x\")",
            "kafka.to(brokers: [\"x\"])",
            "|> TO (bucket: \"x\")", // 大写也要挡
            "http.requests( default: {} )",
        ] {
            assert!(flux_denied_function(q).is_some(), "should deny: {q}");
        }
    }

    #[test]
    fn flux_guardrail_allows_reads() {
        for q in [
            "from(bucket:\"to-bucket\") |> range(start:-1h) |> filter(fn:(r) => r._measurement == \"total\")",
            "import \"influxdata/influxdb/schema\"\nschema.measurements(bucket: \"m\")",
            "from(bucket:\"m\") |> range(start: -1h) |> aggregateWindow(every: 5m, fn: mean)",
            "import \"http\"\nx = 1", // import 不带调用
            "from(bucket:\"m\") |> sort(columns:[\"_time\"], desc: true) |> limit(n:1)",
        ] {
            assert!(flux_denied_function(q).is_none(), "should allow: {q}");
        }
    }

    #[test]
    fn flux_string_literal_escapes() {
        assert_eq!(flux_string_literal("mnet"), "\"mnet\"");
        assert_eq!(flux_string_literal("a\"b"), "\"a\\\"b\"");
        assert_eq!(flux_string_literal("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn retention_humanize() {
        assert_eq!(humanize_secs(86_400), "1d");
        assert_eq!(humanize_secs(4560 * 3600), "190d");
        assert_eq!(humanize_secs(3_600), "1h");
        assert_eq!(humanize_secs(90), "90s");
        assert_eq!(humanize_secs(0), "inf");
    }
}
