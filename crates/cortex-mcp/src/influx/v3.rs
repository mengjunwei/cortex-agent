//! InfluxDB v3 客户端适配（官方 `influxdb3-client`：查询走 Arrow Flight gRPC，
//! 元数据/健康检查走 HTTP）。
//!
//! v3 的查询语言是 SQL（默认）或 InfluxQL；一条 MCP 进程绑定**一个** database
//! （`INFLUX_DATABASE`，官方客户端的查询不携带 per-query db）。只读防线：
//! 语句首关键字白名单 + 单语句限制（v3 的 SQL 引擎本身也不支持写操作，
//! 白名单是纵深防御）。

use futures_util::stream::StreamExt;
use influxdb3_client::{Client, ClientConfig, Error as V3Error, QueryResult, QueryType};
use serde_json::{json, Map, Value};

use super::{code, ToolError};

/// v3 语句首关键字白名单（只读）
const READONLY_KEYWORDS: &[&str] = &["select", "show", "with", "describe", "desc", "explain"];

pub(crate) struct V3Client {
    client: Client,
    database: String,
    /// SHOW DATABASES 的 Flight 路径在 3-core 未实现（见 [`V3Client::databases`]），
    /// 走 HTTP v1 InfluxQL 端点，需要独立的 HTTP 客户端。
    http: reqwest::Client,
    url: String,
    token: String,
}

impl V3Client {
    pub(crate) async fn connect(url: &str, token: &str, database: &str) -> Result<Self, String> {
        let cfg = ClientConfig::builder()
            .host(url.to_string())
            .token(token.to_string())
            .database(database.to_string())
            .build()
            .map_err(|e| format!("InfluxDB 3 客户端配置失败: {e}"))?;
        let client = Client::new(cfg)
            .await
            .map_err(|e| format!("InfluxDB 3 客户端构建失败: {e}"))?;
        Ok(V3Client {
            client,
            database: database.to_string(),
            http: reqwest::Client::new(),
            url: url.to_string(),
            token: token.to_string(),
        })
    }

    /// 启动自检：ping（HTTP，带鉴权）+ 最小 SQL 查询（触发 Flight 建连、
    /// 验证 database 与 token）。Err 中文给操作者 → main exit 2。
    pub(crate) async fn health(&self) -> Result<(), String> {
        self.client
            .ping()
            .await
            .map_err(|e| format!("InfluxDB 3 ping 失败（检查 INFLUX_URL 是否可达）: {e}"))?;
        self.client
            .sql("SELECT 1")
            .await
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "InfluxDB 3 查询自检失败（database \"{}\"，检查 INFLUX_TOKEN 与 INFLUX_DATABASE）: {e}",
                    self.database
                )
            })?;
        Ok(())
    }

    pub(crate) fn database(&self) -> &str {
        &self.database
    }

    /// 执行查询并做行数截断。流式取批次、够量即停（不把全表拉进内存）。
    /// 返回 (行 JSON, 是否截断)。
    pub(crate) async fn query(
        &self,
        dialect: &str,
        q: &str,
        limit: usize,
    ) -> Result<(Vec<Value>, bool), ToolError> {
        let language = if dialect == "influxql" {
            QueryType::InfluxQL
        } else {
            QueryType::Sql
        };
        let mut stream = self
            .client
            .query(q, language)
            .stream()
            .await
            .map_err(map_v3)?;
        // 多取一行探测截断：凑满 limit+1 行 ⇒ 必有第 limit+1 行之后的内容（截断）；
        // 流自然结束 ⇒ total 精确，total <= limit 即未截断。
        let fetch_to = limit.saturating_add(1);
        let mut batches = Vec::new();
        let mut schema = None;
        let mut got: usize = 0;
        while let Some(batch) = stream.next().await {
            let batch = batch.map_err(map_v3)?;
            if schema.is_none() {
                schema = Some(batch.schema());
            }
            got += batch.num_rows();
            batches.push(batch);
            if got >= fetch_to {
                break; // 够量即停：后续批次不再拉取（Flight 流可随时丢弃）
            }
        }
        let Some(schema) = schema else {
            return Ok((Vec::new(), false)); // 空结果
        };
        let result = QueryResult::new(schema, batches);
        let total = result.num_rows();
        let mut rows = Vec::with_capacity(total.min(limit));
        for row in result {
            let row = row.map_err(map_v3)?;
            if rows.len() >= limit {
                break;
            }
            let mut obj = Map::with_capacity(row.len());
            for (name, v) in row.columns().iter().zip(row.values()) {
                obj.insert(name.clone(), value_to_json(v));
            }
            rows.push(Value::Object(obj));
        }
        Ok((rows, total > limit))
    }

    /// 库清单（InfluxQL `SHOW DATABASES`）。Flight 路径在 3-core 报
    /// "This feature is not implemented"，但 HTTP v1 InfluxQL 端点
    /// （`POST /api/v3/query_influxql`）支持且返回干净 JSON——服务端原生
    /// HTTP 接口，不依赖任何命令行工具。
    pub(crate) async fn databases(&self) -> Result<Vec<String>, ToolError> {
        let resp = self
            .http
            .post(format!("{}/api/v3/query_influxql", self.url))
            .query(&[("db", self.database.as_str()), ("format", "jsonl")])
            .bearer_auth(&self.token)
            .json(&json!({"q": "SHOW DATABASES"}))
            .send()
            .await
            .map_err(map_http)?;
        let status = resp.status();
        let body = resp.text().await.map_err(map_http)?;
        if !status.is_success() {
            let code = match status.as_u16() {
                401 | 403 => code::AUTH_FAILED,
                400 => code::QUERY_ERROR,
                _ => code::SERVER_ERROR,
            };
            return Err(ToolError {
                code,
                message: format!("SHOW DATABASES failed: HTTP {status}: {}", shorten(&body)),
                hint: "check the token and that the server is InfluxDB 3.x".into(),
            });
        }
        parse_databases_json(&body).map_err(|e| ToolError {
            code: code::INTERNAL,
            message: e,
            hint: "unexpected SHOW DATABASES response shape; the server version may differ".into(),
        })
    }

    /// 配置库下的表清单（information_schema）。
    pub(crate) async fn tables(&self) -> Result<Vec<String>, ToolError> {
        let sql = "SELECT table_name FROM information_schema.tables \
                   WHERE table_schema = 'iox' ORDER BY table_name";
        let (rows, _) = self.query("sql", sql, 1000).await?;
        first_column_strings(&rows)
    }

    /// 单表列明细（列名 + 类型）。
    pub(crate) async fn columns(&self, table: &str) -> Result<Vec<(String, String)>, ToolError> {
        let esc = table.replace('\'', "''");
        let sql = format!(
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_schema = 'iox' AND table_name = '{esc}' ORDER BY ordinal_position"
        );
        let (rows, _) = self.query("sql", &sql, 1000).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let obj = match r.as_object() {
                Some(o) => o,
                None => continue,
            };
            let name = obj.values().next().and_then(Value::as_str);
            let ty = obj.values().nth(1).and_then(Value::as_str);
            if let Some((n, t)) = name.zip(ty) {
                out.push((n.to_string(), t.to_string()));
            }
        }
        Ok(out)
    }
}

/// v3 只读护栏：首关键字白名单 + 单语句。Ok(())=放行；Err(why,detail)=拒绝
/// （why 是给模型的提示，英文）。
pub(crate) fn readonly_reject(q: &str) -> Result<(), (&'static str, String)> {
    let mut s = q;
    // 跳过前导空白与注释（-- 行注释、/* */ 块注释）
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        } else if let Some(rest) = t.strip_prefix("/*") {
            match rest.find("*/") {
                Some(i) => s = &rest[i + 2..],
                None => return Err(("remove the unterminated /* comment", t.chars().take(60).collect())),
            }
        } else {
            s = t;
            break;
        }
    }
    if s.is_empty() {
        return Err(("provide a read-only statement (SELECT / SHOW / WITH / DESCRIBE / EXPLAIN)", String::new()));
    }
    let first: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase();
    if !READONLY_KEYWORDS.contains(&first.as_str()) {
        return Err((
            "this tool is read-only; start the statement with SELECT / SHOW / WITH / DESCRIBE / EXPLAIN",
            format!("statement starts with \"{first}\""),
        ));
    }
    // 单语句：分号后还有非注释内容 → 拒绝
    if let Some((_, after)) = s.split_once(';') {
        if !after.trim().is_empty() {
            return Err((
                "send one statement at a time",
                "content found after the first \";\"".into(),
            ));
        }
    }
    Ok(())
}

fn first_column_strings(rows: &[Value]) -> Result<Vec<String>, ToolError> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if let Some(o) = r.as_object() {
            if let Some(s) = o.values().next().and_then(Value::as_str) {
                out.push(s.to_string());
            }
        }
    }
    Ok(out)
}

/// `SHOW DATABASES` 的 HTTP 响应：`[{"iox::database":"mnet","deleted":false},...]`
fn parse_databases_json(body: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| format!("cannot parse SHOW DATABASES response: {e}"))?;
    let arr = v
        .as_array()
        .ok_or("cannot parse SHOW DATABASES response: not a JSON array")?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(name) = item.get("iox::database").and_then(Value::as_str) else {
            continue;
        };
        if item.get("deleted").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// HTTP 传输层错误（databases() 专用）：超时→TIMEOUT，其余→CONNECTION_FAILED
fn map_http(e: reqwest::Error) -> ToolError {
    if e.is_timeout() {
        ToolError {
            code: code::TIMEOUT,
            message: format!("request timed out: {e}"),
            hint: "check server load, then retry".into(),
        }
    } else {
        ToolError {
            code: code::CONNECTION_FAILED,
            message: format!("cannot reach InfluxDB 3: {e}"),
            hint: "check that the server is running and reachable from this process".into(),
        }
    }
}

/// 服务器错误正文截断（防止把整页 HTML 塞进信封）
fn shorten(s: &str) -> String {
    s.chars().take(300).collect()
}

fn value_to_json(v: &influxdb3_client::Value) -> Value {
    use influxdb3_client::Value as V;
    match v {
        V::Bool(b) => Value::Bool(*b),
        V::I8(x) => Value::from(*x),
        V::I16(x) => Value::from(*x),
        V::I32(x) => Value::from(*x),
        V::I64(x) => Value::from(*x),
        V::U8(x) => Value::from(*x),
        V::U16(x) => Value::from(*x),
        V::U32(x) => Value::from(*x),
        V::U64(x) => Value::from(*x),
        V::F32(x) => Value::from(*x),
        V::F64(x) => Value::from(*x),
        V::String(s) => Value::String(s.clone()),
        V::Binary(b) => Value::String(format!("<{} bytes>", b.len())),
        V::Timestamp(ns) => Value::String(rfc3339_nanos(*ns)),
        V::Null => Value::Null,
    }
}

/// 纳秒 epoch → RFC3339（UTC，带 Z）。转换失败（超范围）回退原数值字符串。
fn rfc3339_nanos(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .unwrap_or_else(|| ns.to_string())
}

fn map_v3(e: V3Error) -> ToolError {
    match &e {
        V3Error::Http(h) if h.is_timeout() => ToolError {
            code: code::TIMEOUT,
            message: format!("request timed out: {h}"),
            hint: "narrow the time range or aggregate to reduce query work".into(),
        },
        V3Error::Http(h) => ToolError {
            code: code::CONNECTION_FAILED,
            message: format!("cannot reach InfluxDB 3: {h}"),
            hint: "check that the server is running and reachable from this process".into(),
        },
        V3Error::Transport(t) => ToolError {
            code: code::CONNECTION_FAILED,
            message: format!("cannot open query channel: {t}"),
            hint: "check the server address/port; InfluxDB 3 serves queries on the same port (default 8181)".into(),
        },
        V3Error::Flight(s) => {
            let code_dbg = format!("{:?}", s.code());
            match code_dbg.as_str() {
                "Unauthenticated" | "PermissionDenied" => ToolError {
                    code: code::AUTH_FAILED,
                    message: s.message().to_string(),
                    hint: "check the token value and that it has read access to the database".into(),
                },
                "Unavailable" => ToolError {
                    code: code::SERVER_ERROR,
                    message: s.message().to_string(),
                    hint: "server unavailable; retry shortly".into(),
                },
                _ => ToolError {
                    code: code::QUERY_ERROR,
                    message: s.message().to_string(),
                    hint: "the server rejected the query; check syntax and table/column names".into(),
                },
            }
        }
        V3Error::Server { code: c, message } => match c {
            401 | 403 => ToolError {
                code: code::AUTH_FAILED,
                message: message.clone(),
                hint: "check the token value and that it has read access to the database".into(),
            },
            400 => ToolError {
                code: code::QUERY_ERROR,
                message: message.clone(),
                hint: "the server rejected the query; check syntax and table/column names".into(),
            },
            c if *c >= 500 => ToolError {
                code: code::SERVER_ERROR,
                message: message.clone(),
                hint: "server-side failure; retry later or narrow the query".into(),
            },
            _ => ToolError {
                code: code::QUERY_ERROR,
                message: format!("HTTP {c}: {message}"),
                hint: "unexpected response; verify the server is InfluxDB 3.x".into(),
            },
        },
        V3Error::Timeout(_) => ToolError {
            code: code::TIMEOUT,
            message: e.to_string(),
            hint: "narrow the time range or aggregate to reduce query work".into(),
        },
        V3Error::Config(m) => ToolError {
            code: code::INTERNAL,
            message: m.clone(),
            hint: "client configuration issue; check INFLUX_* environment variables".into(),
        },
        _ => ToolError {
            code: code::INTERNAL,
            message: e.to_string(),
            hint: "unexpected client error; see the message above".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_readonly_statements() {
        for q in [
            "SELECT * FROM cpu",
            "  select time, value from cpu where time > now() - interval '1 hour'",
            "show databases",
            "SHOW MEASUREMENTS",
            "with t as (select 1) select * from t",
            "DESCRIBE cpu",
            "desc cpu",
            "EXPLAIN SELECT 1",
            "-- leading comment\nSELECT 1",
            "/* block */ SELECT 1",
            "SELECT 1;",
        ] {
            assert!(readonly_reject(q).is_ok(), "should allow: {q}");
        }
    }

    #[test]
    fn rejects_writes_and_multi_statements() {
        for q in [
            "INSERT INTO cpu VALUES (1)",
            "delete from cpu",
            "DROP TABLE cpu",
            "create table x (a int)",
            "update cpu set v = 1",
            "copy cpu to '/tmp/x'",
            "SELECT 1; DROP TABLE cpu",
            "",
            "   ",
        ] {
            assert!(readonly_reject(q).is_err(), "should reject: {q}");
        }
    }

    #[test]
    fn rfc3339_conversion() {
        // 2024-01-01T00:00:00Z = 1704067200s
        assert!(rfc3339_nanos(1_704_067_200_000_000_000).starts_with("2024-01-01T00:00:00"));
        assert_eq!(rfc3339_nanos(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn databases_json_parsing() {
        // 3-core 实测响应形状（HTTP v1 InfluxQL 端点，format=jsonl）
        let body = r#"[{"iox::database":"_internal","deleted":false},{"iox::database":"mnet","deleted":false},{"iox::database":"old","deleted":true}]"#;
        assert_eq!(
            parse_databases_json(body).unwrap(),
            vec!["_internal".to_string(), "mnet".to_string()]
        );
        assert!(parse_databases_json("not json").is_err());
        assert!(parse_databases_json("{}").is_err());
    }
}
