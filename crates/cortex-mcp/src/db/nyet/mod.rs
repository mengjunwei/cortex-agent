// Adapted from nyetdb v0.3.1 — https://github.com/stasmarkin/nyetdb
// Copyright (c) Stas Markin. Licensed under MIT OR Apache-2.0 (copies in
// crates/cortex-mcp/third-party/nyetdb/). Ported for cortex-mcp: the CLI
// layer (aliases / config file / directory scoping / audit log / SSH tunnels
// / doctor / output formats) is replaced by this env-driven single-connection
// orchestration; MongoDB / ClickHouse / Redis engines are NOT ported.

//! nyetdb 移植版的 MCP 编排层：把 nyetdb CLI 的 `query` / `schema` / `sample`
//! / `explain` 四条命令流水线搬进一个 stdio MCP 服务进程。
//!
//! 与 CLI 原版的差异（语义不变，载体变了）：
//! - 失败不是 exit code，而是 [`output::error_json`] 信封字符串（code 契约照搬：
//!   NYET / CONNECTION_FAILED / DB_ERROR / TIMEOUT / CONFIG_INVALID）；
//! - 提示文本里的 `nyet query <alias>` 一律改写为本 MCP 的工具名
//!   （`db_query` / `db_schema`），密码永不进信封；
//! - 引擎是纯数据结构（每查一连接），每次调用按 env 重新拼装，
//!   `sample` 回退路径的预算收缩（set_query_timeout_ms）因此无需可变共享状态。
//!
//! 流水线（对齐 nyetdb run_attempt）：
//! validate（层1，含 net A PII）→ guardrail（EXPLAIN 代价预估）→
//! execute(limit+1 截断探测) → net B（结果溯源 PII）→ 信封。

#[allow(dead_code)] // 移植保留原库公开面（含测试所引），不逐项裁剪
mod engine;
#[allow(dead_code)] // 同上
mod guardrail;
#[allow(dead_code)] // 同上（jsonl/csv/table 渲染器随移植保留）
mod output;
#[allow(dead_code)] // 同上
mod sample;
#[allow(dead_code)] // 同上
mod validator;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::config::{DbEngine, DbEnv};
use engine::{Engine, Mysql, Postgres, QueryOutcome, Sqlite};
use guardrail::{CostEstimate, Guardrail};
use output::{Estimate, ExplainMeta, QueryMeta, SchemaMeta, Warning};
use validator::{PiiMode, PiiRules, Policy};

/// 信封里的 connection 名（CLI 原版是 alias；这里一条进程就是一个库）。
const CONNECTION: &str = "db";

/// 引擎枚举（nyetdb main.rs `Db` 的三引擎子集）。
enum Db {
    Sqlite(Sqlite),
    Postgres(Postgres),
    Mysql(Mysql),
}

impl Db {
    /// 行离开引擎层的唯一通道 —— net B（PII 结果溯源）在这层收口：
    /// 拒绝与打码都在行被格式化/进信封之前完成（nyetdb finding 6）。
    async fn execute(
        &self,
        sql: &str,
        fetch_limit: u64,
        guardrail: &Guardrail,
        pii: &validator::PiiRules,
        pii_exempt: &[usize],
    ) -> Result<(QueryOutcome, Vec<String>), engine::EngineError> {
        let mut outcome = match self {
            Db::Sqlite(e) => e.execute(sql, fetch_limit, guardrail).await,
            Db::Postgres(e) => e.execute(sql, fetch_limit, guardrail).await,
            Db::Mysql(e) => e.execute(sql, fetch_limit, guardrail).await,
        }?;
        let mut masked = Vec::new();
        if let QueryOutcome::Ran { result, .. } = &mut outcome {
            // sqlx 三引擎都在线路上报告列溯源（MySQL/SQLite 免费带回；
            // PostgreSQL 由 resolve_column_origins 预解析补齐）。
            match validator::check_origins(pii, &result.columns, &result.origins, pii_exempt) {
                Err(refusal) => {
                    return Ok((QueryOutcome::PiiRefused(Box::new(refusal)), Vec::new()))
                }
                Ok(indexes) => {
                    masked = indexes
                        .iter()
                        .map(|i| result.columns[*i].clone())
                        .collect::<Vec<_>>();
                    output::redact(&mut result.rows, &indexes);
                }
            }
        }
        Ok((outcome, masked))
    }

    async fn estimate(&self, sql: &str) -> Result<Option<CostEstimate>, engine::EngineError> {
        match self {
            Db::Sqlite(e) => e.estimate(sql).await,
            Db::Postgres(e) => e.estimate(sql).await,
            Db::Mysql(e) => e.estimate(sql).await,
        }
    }

    async fn schema(&self, table: Option<&str>) -> Result<output::Schema, engine::EngineError> {
        match self {
            Db::Sqlite(e) => e.schema(table).await,
            Db::Postgres(e) => e.schema(table).await,
            Db::Mysql(e) => e.schema(table).await,
        }
    }

    /// net B 需要 PostgreSQL 预解析列溯源（一次额外 DESCRIBE 往返）；
    /// MySQL/SQLite 线路免费带回，无开关。
    fn resolve_column_origins(&mut self) {
        if let Db::Postgres(pg) = self {
            pg.resolve_column_origins = true;
        }
    }

    /// `sample` 回退路径收缩下一次语句的预算（两趟共享一个超时，不新开）。
    fn set_query_timeout_ms(&mut self, ms: u64) {
        match self {
            Db::Sqlite(e) => e.query_timeout_ms = ms,
            Db::Postgres(e) => {
                e.query_timeout_ms = ms;
                e.statement_timeout_ms = Postgres::clamp_statement_timeout(ms);
            }
            Db::Mysql(e) => {
                e.query_timeout_ms = ms;
                e.statement_timeout_ms = Mysql::clamp_statement_timeout(ms);
            }
        }
    }
}

/// MCP 门面：env 配置 → 四个工具入口 + 启动自检。
pub struct NyetDb {
    env: DbEnv,
    policy: Policy,
    /// 有 PII 规则时，数据库错误文本可能引用单元格值 —— 与 DB 侧错误一并扣押。
    redact_db_errors: bool,
    insecure_transport: bool,
}

impl NyetDb {
    /// 从共享 env 配置构建；Err = 英文配置错误（main 据此 exit 2）。
    pub fn new(env: &DbEnv) -> Result<Self, String> {
        let pii_mode = PiiMode::parse(&env.pii_mode)
            .map_err(|m| format!("DB_PII_MODE invalid: {m}"))?;
        let pii = PiiRules::parse(&env.pii, pii_mode)
            .map_err(|m| format!("DB_PII invalid: {m}"))?;
        // 提前解析一次护栏：非法组合（如 sqlite 指定 cost）在这里就报，
        // 而不是拖到第一次 db_query。
        Guardrail::resolve(&env.engine.nyet_label(env.mariadb), env.guardrail_mode.as_deref(), env.guardrail_max_cost, env.guardrail_max_rows)?;
        let policy = match env.engine {
            DbEngine::MySql => Policy::mysql(&env.allow_functions, &env.deny_functions),
            DbEngine::Postgres => Policy::postgres(&env.allow_functions, &env.deny_functions),
            DbEngine::Sqlite => Policy::sqlite(&env.allow_functions, &env.deny_functions),
        }
        .with_pii(pii);
        let redact_db_errors = !policy.pii().is_empty();
        let insecure_transport = match env.engine {
            // sqlite 是本地文件，无传输可言
            DbEngine::Sqlite => false,
            _ => engine::transport_below_require(env.engine.label(), &env.nyet_url),
        };
        Ok(NyetDb {
            env: env.clone(),
            policy,
            redact_db_errors,
            insecure_transport,
        })
    }

    /// 每次调用现拼引擎（纯数据结构，连接是每查一条，无池可复用）。
    fn build_db(&self) -> Db {
        let timeout_ms = self.env.query_timeout.as_millis() as u64;
        let mut db = match (&self.env.engine, &self.env.sqlite_path) {
            (DbEngine::Sqlite, Some(path)) => Db::Sqlite(Sqlite {
                path: PathBuf::from(path),
                query_timeout_ms: timeout_ms,
            }),
            (DbEngine::Postgres, _) => Db::Postgres(Postgres {
                url: self.env.nyet_url.clone(),
                password: self.env.password.clone(),
                statement_timeout_ms: Postgres::clamp_statement_timeout(timeout_ms),
                query_timeout_ms: timeout_ms,
                host_override: None,
                connect_timeout_ms: None,
                resolve_column_origins: false,
            }),
            (DbEngine::MySql, _) => Db::Mysql(Mysql {
                url: self.env.nyet_url.clone(),
                password: self.env.password.clone(),
                statement_timeout_ms: Mysql::clamp_statement_timeout(timeout_ms),
                query_timeout_ms: timeout_ms,
                host_override: None,
                connect_timeout_ms: None,
                mariadb: self.env.mariadb,
            }),
            // sqlite 无 path 只会出现在配置解析放行了 sqlite:// 空路径的场合，
            // 由 db::config 保证不发生；这里按 url 形式兜底交给引擎报错。
            (DbEngine::Sqlite, None) => Db::Sqlite(Sqlite {
                path: PathBuf::new(),
                query_timeout_ms: timeout_ms,
            }),
        };
        if self.redact_db_errors {
            db.resolve_column_origins();
        }
        db
    }

    /// 启动自检：全流水线跑一遍 `SELECT 1`（层1 校验 + 层2 只读会话 + 连接）。
    /// Err = 英文原因（main 据此 exit 2 → 探活红 → 重拉自愈）。
    pub async fn probe(&self) -> Result<(), String> {
        let db = self.build_db();
        match db
            .execute("SELECT 1", 1, &Guardrail::OFF, self.policy.pii(), &[])
            .await
        {
            Ok((QueryOutcome::Ran { .. }, _)) => Ok(()),
            Ok((QueryOutcome::PiiRefused(r), _)) => Err(r.message),
            Ok(_) => Ok(()),
            Err(e) => {
                let (message, hint) = engine_failure_parts(e, false);
                Err(if hint.is_empty() {
                    message
                } else {
                    format!("{message} — {hint}")
                })
            }
        }
    }

    /// `db_query`：查询流水线，返回 JSON 信封。
    pub async fn query(&self, sql: &str, limit: Option<u64>) -> String {
        let limit = limit.unwrap_or(self.env.max_rows as u64).min(self.env.max_rows as u64);
        let db = self.build_db();
        let attempt = self.run_attempt(&db, sql, limit).await;
        let (rs, warnings, duration_ms) = match attempt {
            Ok(a) => a,
            Err(f) => return f.json(),
        };
        let mut rs = rs;
        let mut warnings = warnings;
        // 截断探测：取了 limit+1 —— 多出来的那行只说明「还有」，不进结果。
        let over_limit = rs.rows.len() as u64 > limit;
        let truncated = over_limit || rs.truncated;
        if over_limit {
            rs.rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        if truncated {
            warnings.push(Warning {
                code: "TRUNCATED",
                message: format!(
                    "result truncated to {limit} rows; add WHERE/LIMIT or raise the limit"
                ),
            });
        }
        push_shared_warnings(&mut warnings, &rs, self.insecure_transport);
        let meta = QueryMeta {
            row_count: rs.rows.len() as u64,
            truncated,
            duration_ms,
            connection: CONNECTION.to_string(),
        };
        output::query_json(&rs.columns, &rs.rows, &meta, &warnings)
    }

    /// `db_schema`：无 table → 表清单；有 table → 单表全量（列/键/索引/外键）。
    pub async fn schema(&self, table: Option<&str>) -> String {
        let db = self.build_db();
        let started = Instant::now();
        self.schema_result(db, table, started)
            .await
            .unwrap_or_else(|f: Failure| f.json())
    }

    async fn schema_result(
        &self,
        db: Db,
        table: Option<&str>,
        started: Instant,
    ) -> Result<String, Failure> {
        let schema = db
            .schema(table)
            .await
            .map_err(|e| engine_failure(e, self.redact_db_errors))?;
        let mut warnings = Vec::new();
        if table.is_none() && schema.tables.len() > output::DETAIL_LIMIT {
            warnings.push(Warning {
                code: "SCHEMA_TRUNCATED",
                message: format!(
                    "this database has more than {} tables and views, so only names and \
                     kinds are listed; name one table to get its full detail: \
                     db_schema with table = \"<name>\"",
                    output::DETAIL_LIMIT
                ),
            });
        }
        if let Some(sampled) = schema.tables.first().and_then(|t| t.sampled) {
            warnings.push(Warning {
                code: "SCHEMA_SAMPLED",
                message: format!(
                    "this engine does not publish column metadata, so this schema is a \
                     GUESS from the first {sampled} sampled row(s): names are as they \
                     appeared, types are inferred, and anything the sample did not show \
                     is missing. Verify a column before you rely on it"
                ),
            });
        }
        if self.insecure_transport {
            warnings.push(insecure_transport_warning());
        }
        let meta = SchemaMeta {
            table_count: schema.tables.len() as u64,
            duration_ms: elapsed_ms(started),
            connection: CONNECTION.to_string(),
        };
        Ok(output::schema_json(&schema, &meta, &warnings))
    }

    /// `db_sample`：随机抽样；护栏拒了随机排序时回退首 N 行（SAMPLE_FALLBACK）。
    pub async fn sample(&self, table: &str, limit: Option<u64>) -> String {
        let limit = limit
            .unwrap_or(sample::DEFAULT_ROWS)
            .min(self.env.max_rows as u64);
        let mut db = self.build_db();
        let (first, cheap) = statements(table, &self.env.engine, limit.saturating_add(1));
        let started = Instant::now();
        let mut attempt = self.run_attempt(&db, &first, limit).await;
        let mut fell_back = false;
        // 唯一重试的点：拒的是随机 ORDER BY（全表排序），不是这张表 ——
        // 换便宜的问题问，而不是把 agent 修不了的拒绝甩给它。两趟共享一个超时预算。
        if matches!(&attempt, Err(f) if f.is_expensive_query()) {
            let first_ms = elapsed_ms(started);
            if let (Some(cheap), Some(left)) = (&cheap, fallback_budget_ms(self.env.query_timeout, first_ms)) {
                db.set_query_timeout_ms(left.as_millis() as u64);
                attempt = self.run_attempt(&db, cheap, limit).await;
                fell_back = true;
            }
        }
        let (mut rs, mut warnings, mut duration_ms) = match attempt {
            Ok(a) => a,
            Err(f) => return sample_failure_hint(f, &self.env.engine, fell_back).json(),
        };
        if fell_back {
            // 被拒的那趟花的是真实时间（EXPLAIN 跑到了判决）；只报第二趟会
            // 把大部分等待藏起来。提示用 agent 自己的 limit 拼出可照抄的语句。
            duration_ms = duration_ms.saturating_add(elapsed_ms(started));
            let suggestion = statements(table, &self.env.engine, limit).0;
            warnings.insert(
                0,
                Warning {
                    code: "SAMPLE_FALLBACK",
                    message: format!(
                        "a random sample of this table was refused by this connection's \
                         guardrail as too expensive (drawing at random means sorting the whole \
                         table), so these are the FIRST rows the database returned, in its own \
                         storage order — typically the oldest or lowest-key ones. Do not read \
                         them as representative of the table. To insist on a real random draw, \
                         ask for it yourself: db_query with sql = {suggestion}"
                    ),
                },
            );
        }
        let over_limit = rs.rows.len() as u64 > limit;
        let truncated = over_limit || rs.truncated;
        if over_limit {
            rs.rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        if truncated {
            warnings.push(Warning {
                code: "TRUNCATED",
                message: format!(
                    "this is a sample of {limit} rows, not the table: there ARE more rows — \
                     pass a bigger limit for a bigger sample, or read the table itself with \
                     db_query and your own SELECT"
                ),
            });
        }
        push_shared_warnings(&mut warnings, &rs, self.insecure_transport);
        let meta = QueryMeta {
            row_count: rs.rows.len() as u64,
            truncated,
            duration_ms,
            connection: CONNECTION.to_string(),
        };
        output::query_json(&rs.columns, &rs.rows, &meta, &warnings)
    }

    /// `db_explain`：计划与代价预估 —— 不执行语句本身。
    pub async fn explain(&self, sql: &str) -> String {
        let db = self.build_db();
        let mut warnings = Vec::new();
        // 与 db_query 完全相同的层1：给写语句做计划在连接建立前就被拒。
        let (sql, is_query, mut vw, _) = match validate(sql, &self.policy) {
            Ok(v) => v,
            Err(f) => return f.json(),
        };
        warnings.append(&mut vw);
        let guardrail = match self.resolve_guardrail() {
            Ok(g) => g,
            Err(f) => return f.json(),
        };
        // 元数据语句（SHOW/DESCRIBE，或 agent 自己写的 EXPLAIN）没有计划可问：
        // 包一层 EXPLAIN 只会换来服务器的语法报错。就地诚实作答，不碰数据库。
        let started = Instant::now();
        let (plan, duration_ms) = match is_query {
            true => match db.estimate(&sql).await {
                Ok(p) => (p, elapsed_ms(started)),
                Err(e) => return engine_failure(e, self.redact_db_errors).json(),
            },
            false => {
                warnings.push(no_plan_warning());
                (None, 0)
            }
        };
        // 没拿到计划：要么本就不是查询，要么规划超了护栏预算 —— explain 与
        // query 共用同一预算，不能替 query 会拒的语句答「没问题」。
        let empty = plan.is_none();
        let plan = plan.unwrap_or_else(|| {
            if is_query {
                warnings.push(planning_too_slow_warning());
            }
            CostEstimate {
                plan: serde_json::Value::Array(Vec::new()),
                cost: None,
                rows: None,
                lower_bound: false,
            }
        });
        if is_query && !empty && guardrail.plans() && guardrail.check(&plan) == guardrail::Check::NoEstimate {
            warnings.push(guardrail_skipped_warning());
        }
        let estimate = guardrail.describe(plan);
        if self.insecure_transport {
            warnings.push(insecure_transport_warning());
        }
        let meta = ExplainMeta {
            duration_ms,
            connection: CONNECTION.to_string(),
        };
        output::explain_json(&estimate, &meta, &warnings)
    }

    /// 解析护栏配置（已在 new() 校验过；此处 Err 只作防御性兜底）。
    fn resolve_guardrail(&self) -> Result<Guardrail, Failure> {
        Guardrail::resolve(
            &self.env.engine.nyet_label(self.env.mariadb),
            self.env.guardrail_mode.as_deref(),
            self.env.guardrail_max_cost,
            self.env.guardrail_max_rows,
        )
        .map_err(|message| {
            Failure::config_invalid(&format!("DB_GUARDRAIL_* invalid: {message}"))
        })
    }

    /// 一次完整尝试（nyetdb run_attempt）：层1 → 护栏 → limit+1 执行 → net B。
    async fn run_attempt(
        &self,
        db: &Db,
        sql: &str,
        limit: u64,
    ) -> Result<(engine::ResultSet, Vec<Warning>, u64), Failure> {
        let (sql, is_query, mut warnings, pii_exempt) = validate(sql, &self.policy)?;
        // 层1.5：只有普通查询能包 EXPLAIN；SHOW/DESCRIBE 是无预估的元数据，直跑。
        let guardrail = match is_query {
            true => self.resolve_guardrail()?,
            false => Guardrail::OFF,
        };
        let started = Instant::now();
        let (outcome, masked) = db
            .execute(&sql, limit.saturating_add(1), &guardrail, self.policy.pii(), &pii_exempt)
            .await
            .map_err(|e| engine_failure(e, self.redact_db_errors))?;
        let duration_ms = elapsed_ms(started);
        if !masked.is_empty() {
            warnings.push(pii_masked_warning(&masked));
        }
        let (rows, estimate) = match outcome {
            // 护栏拒绝：什么都没执行；信封带上支撑判决的计划。
            QueryOutcome::Refused { estimate, value } => {
                let (message, hint) = guardrail_refusal(&guardrail, value);
                return Err(Failure {
                    code: "NYET",
                    reason: Some("EXPENSIVE_QUERY".to_string()),
                    message,
                    hint,
                    estimate: Some(Box::new(guardrail.describe(estimate))),
                });
            }
            // 规划本身跑超了护栏预算 —— fail closed：规划时间是 agent 可控的，
            // 「没按时出计划」不能成为关掉护栏的途径。
            QueryOutcome::PlanTooSlow { budget_ms } => {
                let (message, hint) = guardrail::planning_too_slow(CONNECTION, budget_ms);
                return Err(Failure {
                    code: "NYET",
                    reason: Some("EXPENSIVE_QUERY".to_string()),
                    message,
                    hint,
                    estimate: None,
                });
            }
            // net B 拒了结果：行存在但永不进信封。
            QueryOutcome::PiiRefused(refusal) => return Err(refusal_failure(*refusal)),
            QueryOutcome::Ran { result, estimate } => (result, estimate),
        };
        // 护栏开着却没拿到可判的数字 —— 按设计 fail open，但不许静默。
        if guardrail.plans()
            && estimate.is_none_or(|e| guardrail.check(&e) == guardrail::Check::NoEstimate)
        {
            warnings.push(guardrail_skipped_warning());
        }
        Ok((rows, warnings, duration_ms))
    }
}

/// 层1：validate 的 CLI 形状（Verdict → Failure / 展开字段）。
fn validate(
    query: &str,
    policy: &Policy,
) -> Result<(String, bool, Vec<Warning>, Vec<usize>), Failure> {
    match validator::validate(query, policy) {
        validator::Verdict::Deny {
            reason,
            message,
            hint,
        } => Err(refusal_failure(validator::Refusal {
            reason,
            message,
            hint,
        })),
        validator::Verdict::Allow {
            sql,
            warnings,
            is_query,
            pii_exempt,
        } => Ok((
            sql,
            is_query,
            warnings
                .into_iter()
                .map(|w| Warning {
                    code: w.code,
                    message: w.message,
                })
                .collect(),
            pii_exempt,
        )),
    }
}

/// 信封错误（code 契约见 docs/cortex-mcp.md §十二）。
struct Failure {
    code: &'static str,
    reason: Option<String>,
    message: String,
    hint: String,
    /// Box：Failure 是热路径 `Result` 的 Err 端（clippy result_large_err，
    /// 216B→96B）；唯一 Some 的地方在 Refused 分支，一次堆分配无感。
    estimate: Option<Box<Estimate>>,
}

impl Failure {
    fn config_invalid(message: &str) -> Failure {
        Failure {
            code: "CONFIG_INVALID",
            reason: None,
            message: message.to_string(),
            hint: "fix the DB_* environment variables for this MCP entry".to_string(),
            estimate: None,
        }
    }

    fn is_expensive_query(&self) -> bool {
        self.code == "NYET" && self.reason.as_deref() == Some("EXPENSIVE_QUERY")
    }

    fn json(&self) -> String {
        output::error_json(
            self.code,
            self.reason.as_deref(),
            &self.message,
            &self.hint,
            self.estimate.as_deref(),
        )
    }
}

/// nyetdb engine_failure 的信封版：EngineError → 错误码 +（可能扣押的）文本。
fn engine_failure(e: engine::EngineError, redact_db_errors: bool) -> Failure {
    match e {
        engine::EngineError::Connect { message, hint } => Failure {
            code: "CONNECTION_FAILED",
            reason: None,
            message,
            hint,
            estimate: None,
        },
        // 层1 拒绝不是数据库错误，永不扣押：它是本工具自己的话，不引用单元格。
        engine::EngineError::Refused {
            reason,
            message,
            hint,
        } => Failure {
            code: "NYET",
            reason: Some(reason.to_string()),
            message,
            hint,
            estimate: None,
        },
        engine::EngineError::Db { .. } if redact_db_errors => Failure {
            code: "DB_ERROR",
            reason: None,
            message: "the database rejected this query; its error text is withheld because \
                      this connection has a PII policy, and a database error message can \
                      quote the very cell values that caused it"
                .to_string(),
            hint: "check the query against the real schema with db_schema (types and column \
                   names are not withheld), and simplify it one clause at a time to find what \
                   the database dislikes; the full server message is in the database's own log"
                .to_string(),
            estimate: None,
        },
        engine::EngineError::Db { message, hint } => Failure {
            code: "DB_ERROR",
            reason: None,
            message,
            hint,
            estimate: None,
        },
        engine::EngineError::Timeout { message, hint } => Failure {
            code: "TIMEOUT",
            reason: None,
            message,
            hint,
            estimate: None,
        },
    }
}

/// probe 用的精简版：只要英文一句话。
fn engine_failure_parts(e: engine::EngineError, _redact: bool) -> (String, String) {
    match e {
        engine::EngineError::Connect { message, hint }
        | engine::EngineError::Db { message, hint }
        | engine::EngineError::Refused {
            message, hint, ..
        }
        | engine::EngineError::Timeout { message, hint } => (message, hint),
    }
}

/// 层1 拒绝（含 net B 的 PiiRefused）→ NYET + reason。
fn refusal_failure(r: validator::Refusal) -> Failure {
    Failure {
        code: "NYET",
        reason: Some(r.reason.as_str().to_string()),
        message: r.message,
        hint: r.hint,
        estimate: None,
    }
}

/// sample 语句与便宜回退（nyetdb statements 的三引擎子集）。
fn statements(table: &str, engine: &DbEngine, rows: u64) -> (String, Option<String>) {
    match engine {
        DbEngine::Sqlite => (sample::sqlite(table, rows), None),
        DbEngine::Postgres => (
            sample::postgres(table, rows, true),
            Some(sample::postgres(table, rows, false)),
        ),
        DbEngine::MySql => (
            sample::mysql(table, rows, true),
            Some(sample::mysql(table, rows, false)),
        ),
    }
}

/// sample 第二趟可用的时间：一个预算里剩下的，绝不新开 ——
/// 剩余不足 1s 时放弃回退，让护栏的拒绝连同提示一起作数。
fn fallback_budget_ms(timeout: Duration, spent_ms: u64) -> Option<Duration> {
    let left = timeout.as_millis() as u64 - spent_ms.min(timeout.as_millis() as u64);
    (left >= 1000).then(|| Duration::from_millis(left))
}

/// `sample` 的失败读在一个没写过这条语句的 agent 眼里：所有「收窄你的查询」
/// 都是死路（nyetdb D10）—— 改写为 MCP 工具面上真正可操作的选项。
fn sample_failure_hint(mut f: Failure, engine: &DbEngine, fell_back: bool) -> Failure {
    match (f.code, f.reason.as_deref()) {
        // 名字是最可能也最便宜的原因，但不是唯一原因（权限拒绝也长这样）。
        ("DB_ERROR", _) => {
            let qualify = match engine {
                DbEngine::Postgres => {
                    " (qualify it as schema.table when it is outside the search_path)"
                }
                _ => "",
            };
            f.hint = format!(
                "check the name first: db_schema lists the tables and views this connection \
                 can read{qualify}. If the name is right, the failure is about something \
                 else — {}",
                f.hint
            );
        }
        // 语句是工具自己写的，「加 WHERE」是给没见过的文本提建议；
        // agent 手里的是表名、limit，和自己写查询的选项。
        ("TIMEOUT", _) => {
            let cause = if fell_back {
                "the random draw was refused as too expensive and even a plain read of this \
                 table did not finish in what the refused attempt left of the timeout (both \
                 attempts of a sample share one budget)"
            } else {
                "a random draw sorts the whole table, which is what takes the time"
            };
            f.hint = format!(
                "{cause}: ask for fewer rows (a smaller limit), or read the table on your \
                 own terms with db_query and a filtered SELECT"
            );
        }
        ("NYET", Some("EXPENSIVE_QUERY")) => {
            let what = if fell_back {
                "the tool already retried without the random sort and the guardrail refused \
                 that too, so a plain read of this table is what it considers expensive"
            } else {
                "this connection's guardrail refused the random draw, which has to sort the \
                 whole table"
            };
            f.hint = format!(
                "{what}, and db_sample has no narrower form to offer — write the read \
                 yourself: db_query with sql = \"SELECT <the columns you need> FROM <table> \
                 LIMIT 10\", and {}",
                f.hint
            );
        }
        // 最常见的拒绝：sample 即 SELECT *，保护列在两种模式下都拒。
        // 「改列名」是对的方向，但这个命令做不到 —— 直说。
        ("NYET", Some("PII_COLUMN" | "PII_UNPROVABLE")) => {
            f.hint = format!(
                "{} — and db_sample cannot do that for you: it always writes SELECT *. Name \
                 the columns yourself: db_query with sql = \"SELECT <the columns you need> \
                 FROM <table> LIMIT 10\"",
                f.hint.trim_end_matches('.')
            );
        }
        _ => {}
    }
    f
}

/// 查询与 sample 共用的尾部告警：重名列 + 明文传输。
fn push_shared_warnings(warnings: &mut Vec<Warning>, rs: &engine::ResultSet, insecure: bool) {
    let duplicates = duplicate_columns(&rs.columns);
    if !duplicates.is_empty() {
        warnings.push(Warning {
            code: "DUPLICATE_COLUMNS",
            message: format!(
                "duplicate column name(s): {}; disambiguate with AS aliases — in this JSON \
                 output duplicates collapse to the last value",
                duplicates.join(", ")
            ),
        });
    }
    if insecure {
        warnings.push(insecure_transport_warning());
    }
}

fn duplicate_columns(columns: &[String]) -> Vec<&str> {
    let mut seen = std::collections::HashSet::new();
    let mut duplicates: Vec<&str> = Vec::new();
    for column in columns {
        if !seen.insert(column.as_str()) && !duplicates.contains(&column.as_str()) {
            duplicates.push(column.as_str());
        }
    }
    duplicates
}

/// `mode = "mask"`：必须告知遮的是哪些列，否则 `[REDACTED]` 会被当数据推理。
fn pii_masked_warning(columns: &[String]) -> Warning {
    Warning {
        code: "PII_MASKED",
        message: format!(
            "column(s) {} are protected by this connection's PII policy (mode = \"mask\"): \
             every value in them was replaced with \"{}\" before you saw it — the real \
             values, their type and their length are not in this answer, so do not treat \
             them as data, compare them or report them as such",
            columns
                .iter()
                .map(|c| format!("'{c}'"))
                .collect::<Vec<_>>()
                .join(", "),
            output::REDACTED
        ),
    }
}

/// 护栏开着却没拿到可判数字：fail open 按设计，但不许静默（nyetdb D10）。
fn guardrail_skipped_warning() -> Warning {
    Warning {
        code: "GUARDRAIL_SKIPPED",
        message: "the guardrail has no estimate it can trust for this query, so it was NOT \
                  checked against the connection's limit. Reasons differ by engine: the \
                  PostgreSQL planner does not bound a recursive CTE (its cost/rows are a LOWER \
                  bound, so only a plan already over the limit can be refused) and some plan \
                  shapes are unreadable. Bound the query yourself with WHERE/LIMIT or a \
                  smaller limit"
            .to_string(),
    }
}

/// 安全信号（不是拒绝）：传输未保证加密/验证。
fn insecure_transport_warning() -> Warning {
    Warning {
        code: "INSECURE_TRANSPORT",
        message: "this connection's transport is not guaranteed encrypted or verified \
                  (sslmode/ssl-mode below require); set DB_SSLMODE=required (transport \
                  encryption) or put sslmode=verify-full / ssl-mode=VERIFY_IDENTITY in the \
                  DB_URL for verification"
            .to_string(),
    }
}

/// `db_explain` 拿到的不是查询。
fn no_plan_warning() -> Warning {
    Warning {
        code: "NO_PLAN",
        message: "SHOW/DESCRIBE and an EXPLAIN you wrote yourself are metadata statements, \
                  not queries: there is no plan to estimate, so nothing was asked of the \
                  database — run the statement with db_query to get its result"
            .to_string(),
    }
}

/// 规划超了护栏预算：没有计划可给，也没有判决可下。
fn planning_too_slow_warning() -> Warning {
    Warning {
        code: "GUARDRAIL_SKIPPED",
        message: "planning this statement outran the guardrail's budget, so there is no plan \
                  to show and no verdict to give — db_query would refuse it for exactly that \
                  reason (EXPENSIVE_QUERY); simplify the statement (fewer joins, fewer \
                  computed expressions)"
            .to_string(),
    }
}

/// 护栏拒绝的文案（Guardrail::refusal 以 connection 名开头，这里换成工具语境）。
fn guardrail_refusal(guardrail: &Guardrail, value: f64) -> (String, String) {
    let (mut message, hint) = guardrail.refusal(CONNECTION, value);
    // CLI 文案引导去改配置文件；MCP 语境下配置在 env，改措辞不改语义。
    if let Some(at) = message.find(CONNECTION) {
        message.replace_range(at..at + CONNECTION.len(), "this connection");
    }
    (message, hint)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
