//! 工具注册表 —— cortex-mcp 的 MCP 服务端实现。
//!
//! # 如何新增一个工具
//! 1. 新建模块 `src/<tool>.rs`：定义配置结构（实现 `from_env`）、`XxxInput`（带
//!    `schemars::JsonSchema`）、以及 `pub async fn xxx(cfg, input) -> Result<String>`。
//! 2. 在 [`ToolServer`] 加一个同类型的 `Option<XxxConfig>` 字段（`None`=未配置/禁用）。
//! 3. 在下面的 `#[tool_router(server_handler)] impl ToolServer` 里加一个 `#[tool]` 方法，
//!    取出配置后委托给模块函数；未配置时返回提示。
//! 4. `main.rs` 里 `from_env()` 读取并填入 `ToolServer`。
//!
//! 这样新工具的「协议声明」与「业务逻辑」分离，注册表保持一目了然。
//!
//! 数据库工具是统一 4 工具面（db_query / db_schema / db_sample / db_explain），
//! 底层为 nyetdb 移植版，见 `db/mod.rs`。

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};
use serde::Deserialize;

use crate::db::DbTools;
use crate::email::{self, EmailConfig, SendEmailInput};
use crate::influx::InfluxTools;
use crate::prometheus::PromTools;

/// 未配置 DB_* 时的统一提示（模型可见，英文）。
const DB_NOT_CONFIGURED: &str = "Database tools not configured: set DB_URL, or DB_TYPE with \
                                 DB_HOST / DB_USER / DB_NAME environment variables";

/// 未配置 INFLUX_* 时的统一提示（模型可见，英文）。
const INFLUX_NOT_CONFIGURED: &str = "InfluxDB tools not configured: set INFLUX_URL, INFLUX_TOKEN \
                                     and INFLUX_ORG (v2) or INFLUX_DATABASE (v3) environment \
                                     variables";

/// 未配置 PROM_* 时的统一提示（模型可见，英文）。
const PROM_NOT_CONFIGURED: &str = "Prometheus tools not configured: set PROM_URL (optionally \
                                   PROM_TOKEN for gateway auth) environment variables";

// —— db 工具输入（协议层声明；实现在 db 模块） ——

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DbQueryInput {
    /// Read-only SQL: a SINGLE statement (SELECT / SHOW / EXPLAIN / DESCRIBE / DESC / WITH)
    pub sql: String,
    /// Max rows to return (default and ceiling come from DB_MAX_ROWS)
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DbSchemaInput {
    /// Table name for one table's detail (columns, keys, indexes, foreign keys);
    /// omit / leave empty to list all tables
    pub table: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DbSampleInput {
    /// The table to sample (may be schema-qualified on PostgreSQL)
    pub table: String,
    /// Max rows to draw (default 10 for the nyet impl; capped by DB_MAX_ROWS)
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DbExplainInput {
    /// The read-only SQL statement to plan (not executed)
    pub sql: String,
}

// —— influx 工具输入（协议层声明；实现在 influx 模块） ——

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InfluxQueryInput {
    /// Read-only query text. Flux for InfluxDB 2 (e.g. from(bucket:"m") |> range(start: -1h) |> filter(fn:(r) => r._measurement == "cpu")); SQL or InfluxQL for InfluxDB 3.
    pub query: String,
    /// Query language: "flux" (v2, default), "sql" (v3, default) or "influxql" (v3). Omit to use the server's default.
    pub dialect: Option<String>,
    /// Max rows to return (default and ceiling come from INFLUX_MAX_ROWS)
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InfluxSchemaInput {
    /// Bucket (v2) or database (v3) to inspect; omit to list all buckets/databases. When omitted at the measurement level, v2 falls back to INFLUX_BUCKET.
    pub bucket: Option<String>,
    /// Measurement (v2) or table (v3) to detail: lists fields and tags / columns with types. Requires bucket (or its default).
    pub measurement: Option<String>,
}

// —— prometheus 工具输入（协议层声明；实现在 prometheus 模块） ——

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromQueryInput {
    /// PromQL expression, e.g. "up", "rate(http_requests_total[5m])" or "avg by (job) (node_cpu_seconds_total)".
    pub query: String,
    /// Evaluation time for instant queries: unix seconds or RFC3339. Omit for now.
    pub time: Option<String>,
    /// Range query start: unix seconds or RFC3339. Must be given together with end and step.
    pub start: Option<String>,
    /// Range query end (inclusive): unix seconds or RFC3339.
    pub end: Option<String>,
    /// Range step as a number of seconds (float), e.g. 15 or 0.5.
    pub step: Option<f64>,
    /// Max rows (data points) to return; default and ceiling come from PROM_MAX_ROWS.
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromSchemaInput {
    /// Metric name to detail (type, help, label names); omit to list all metric names.
    pub metric: Option<String>,
}

/// 工具服务端。持有各工具的配置；`None` 表示该工具未配置（调用时返回提示，不崩溃）。
#[derive(Clone)]
pub struct ToolServer {
    /// 邮件发送配置。
    pub email: Option<EmailConfig>,
    /// 只读数据库工具（MySQL/PostgreSQL/SQLite，经 DB_* 环境变量配置；nyetdb 移植版）。
    pub db: Option<DbTools>,
    /// InfluxDB 时序只读查询工具（v2 Flux / v3 SQL・InfluxQL，经 INFLUX_* 环境变量配置）。
    pub influx: Option<InfluxTools>,
    /// Prometheus 时序只读查询工具（PromQL，经 PROM_* 环境变量配置）。
    pub prom: Option<PromTools>,
    // ── 未来工具配置在此追加 ──
    // pub calendar: Option<CalendarConfig>,
}

#[tool_router(server_handler)]
impl ToolServer {
    /// 通过配置的 SMTP 账号发送邮件（支持纯文本/HTML、抄送/密送、附件）。凭证走进程环境变量。
    /// 提示串模型可见，一律英文。
    #[tool(
        description = "Send an email via the configured SMTP account. Supports plain text and optional HTML body, CC/BCC, and file attachments. Requires env: SMTP_HOST, SMTP_USERNAME, SMTP_PASSWORD (optional: SMTP_PORT, SMTP_FROM)."
    )]
    async fn send_email(&self, Parameters(i): Parameters<SendEmailInput>) -> String {
        let Some(cfg) = &self.email else {
            return "Email tool not configured: missing SMTP_HOST / SMTP_USERNAME / SMTP_PASSWORD environment variables"
                .into();
        };
        match email::send(cfg, i).await {
            Ok(msg) => msg,
            Err(e) => format!("send failed: {e:#}"),
        }
    }

    /// 对配置的数据库执行单条只读 SQL，返回 JSON 信封。
    #[tool(
        description = "Run a single read-only SQL statement against the configured database. Read-only is enforced in layers (AST validation, read-only session), and single-statement only. Returns a JSON envelope {v,ok,rows,meta,warnings}. Requires env: DB_URL, or DB_TYPE with DB_HOST, DB_USER, DB_NAME (optional: DB_PASSWORD, DB_PORT, DB_SSLMODE, DB_MAX_ROWS, DB_QUERY_TIMEOUT_SECS)."
    )]
    async fn db_query(&self, Parameters(i): Parameters<DbQueryInput>) -> String {
        let Some(db) = &self.db else {
            return DB_NOT_CONFIGURED.into();
        };
        db.query(&i.sql, i.limit).await
    }

    /// 数据库结构：无 table 列出全部表；有 table 给出该表列/键/索引/外键。
    #[tool(
        description = "Inspect the database schema: with no table argument, list all tables and views; with a table name, show that table's columns, keys, indexes and foreign keys. Requires env: DB_URL, or DB_TYPE with DB_HOST, DB_USER, DB_NAME (optional: DB_PASSWORD, DB_PORT, DB_SSLMODE, DB_MAX_ROWS, DB_QUERY_TIMEOUT_SECS)."
    )]
    async fn db_schema(&self, Parameters(i): Parameters<DbSchemaInput>) -> String {
        let Some(db) = &self.db else {
            return DB_NOT_CONFIGURED.into();
        };
        let table = i.table.as_deref().map(str::trim).filter(|t| !t.is_empty());
        db.schema(table).await
    }

    /// 随机抽取表的前 N 行样本。
    #[tool(
        description = "Sample rows from a table: a small random draw (default 10 rows). The read-only validator and guardrail judge the generated statement exactly as they would judge your own SQL. Requires env: DB_URL, or DB_TYPE with DB_HOST, DB_USER, DB_NAME (optional: DB_PASSWORD, DB_PORT, DB_SSLMODE, DB_MAX_ROWS, DB_QUERY_TIMEOUT_SECS)."
    )]
    async fn db_sample(&self, Parameters(i): Parameters<DbSampleInput>) -> String {
        let Some(db) = &self.db else {
            return DB_NOT_CONFIGURED.into();
        };
        db.sample(&i.table, i.limit).await
    }

    /// 查看语句的执行计划与代价预估（不执行语句本身）。
    #[tool(
        description = "Show the query plan and cost estimate for a read-only SQL statement WITHOUT executing it. The verdict mirrors what db_query would decide (guardrail included). Requires env: DB_URL, or DB_TYPE with DB_HOST, DB_USER, DB_NAME (optional: DB_PASSWORD, DB_PORT, DB_SSLMODE, DB_MAX_ROWS, DB_QUERY_TIMEOUT_SECS)."
    )]
    async fn db_explain(&self, Parameters(i): Parameters<DbExplainInput>) -> String {
        let Some(db) = &self.db else {
            return DB_NOT_CONFIGURED.into();
        };
        db.explain(&i.sql).await
    }

    /// 对配置的 InfluxDB 执行单条只读时序查询（Flux / SQL / InfluxQL）。
    #[tool(
        description = "Run a single read-only query against the configured InfluxDB time-series server. InfluxDB 2: Flux (e.g. from(bucket:\"m\") |> range(start: -1h) |> filter(fn:(r) => r._measurement == \"cpu\")). InfluxDB 3: SQL (default) or InfluxQL. Write/side-effect functions are rejected. Keep time ranges bounded; results are row-capped. Returns a JSON envelope {v,ok,rows,meta,warnings}. Requires env: INFLUX_URL, INFLUX_TOKEN and INFLUX_ORG (v2) or INFLUX_DATABASE (v3); optional: INFLUX_VERSION, INFLUX_BUCKET, INFLUX_MAX_ROWS, INFLUX_TIMEOUT_SECS."
    )]
    async fn influx_query(&self, Parameters(i): Parameters<InfluxQueryInput>) -> String {
        let Some(influx) = &self.influx else {
            return INFLUX_NOT_CONFIGURED.into();
        };
        influx.query(&i.query, i.dialect.as_deref(), i.limit).await
    }

    /// InfluxDB 结构探查：bucket/数据库 → measurement/表 → 字段与 tag/列。
    #[tool(
        description = "Inspect the InfluxDB structure: with no arguments list buckets (v2) or databases (v3); with bucket only, list measurements (v2) or tables (v3); with both bucket and measurement, list fields and tags (v2) or columns with data types (v3). Use it before writing queries to discover names. Requires env: INFLUX_URL, INFLUX_TOKEN and INFLUX_ORG (v2) or INFLUX_DATABASE (v3); optional: INFLUX_VERSION, INFLUX_BUCKET, INFLUX_MAX_ROWS, INFLUX_TIMEOUT_SECS."
    )]
    async fn influx_schema(&self, Parameters(i): Parameters<InfluxSchemaInput>) -> String {
        let Some(influx) = &self.influx else {
            return INFLUX_NOT_CONFIGURED.into();
        };
        influx.schema(i.bucket.as_deref(), i.measurement.as_deref()).await
    }

    /// 对配置的 Prometheus 执行单条只读 PromQL 查询（即时或区间）。
    #[tool(
        description = "Run a single read-only PromQL query against the configured Prometheus server. Instant query: pass only the expression (optionally time as unix seconds or RFC3339). Range query: pass start + end + step (step in seconds) together — each data point becomes one row. Results are row-capped. Returns a JSON envelope {v,ok,rows,meta,warnings}. Requires env: PROM_URL (optional: PROM_TOKEN for gateway auth, PROM_MAX_ROWS, PROM_TIMEOUT_SECS)."
    )]
    async fn prom_query(&self, Parameters(i): Parameters<PromQueryInput>) -> String {
        let Some(prom) = &self.prom else {
            return PROM_NOT_CONFIGURED.into();
        };
        prom.query(
            &i.query,
            i.time.as_deref(),
            i.start.as_deref(),
            i.end.as_deref(),
            i.step,
            i.limit,
        )
        .await
    }

    /// Prometheus 结构探查：指标名清单 / 单指标 type・help・label 键。
    #[tool(
        description = "Inspect the Prometheus structure: with no arguments list all metric names; with a metric name, show its type, help text and label names. Use it before writing queries to discover metric and label names. Requires env: PROM_URL (optional: PROM_TOKEN for gateway auth, PROM_MAX_ROWS, PROM_TIMEOUT_SECS)."
    )]
    async fn prom_schema(&self, Parameters(i): Parameters<PromSchemaInput>) -> String {
        let Some(prom) = &self.prom else {
            return PROM_NOT_CONFIGURED.into();
        };
        prom.schema(i.metric.as_deref()).await
    }

    // ── 未来工具的 #[tool] 方法在此追加 ─────────────────────────────────
}
