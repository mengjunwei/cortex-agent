// Adapted from nyetdb v0.3.1 — https://github.com/stasmarkin/nyetdb
// Copyright (c) Stas Markin. Licensed under MIT OR Apache-2.0 (copies in
// crates/cortex-mcp/third-party/nyetdb/). Ported for cortex-mcp: MongoDB /
// ClickHouse / Redis engines and `nyet doctor` diagnostics are NOT ported.

//! Engines: IO adapters behind the `Engine` trait (D2). Engines know their
//! drivers (sqlx) and nothing about clap; the cli layer maps `EngineError` onto
//! contract codes and wraps execution in a timeout. The one thing they take
//! from `output` is the pure `schema` model (`Schema`/`SchemaTable`/... plus
//! `build_table`, which owns the pk/unique presentation rules) — the contract
//! shape they fill in, so the engines cannot drift.

use super::guardrail::{CostEstimate, Guardrail};
use super::output::{
    build_table, KeyPart, Schema, SchemaColumn, SchemaFk, SchemaIndex, SchemaTable,
};
use super::validator::Origin;
use futures_util::TryStreamExt;
use serde_json::Value;
use sqlx::mysql::{MySqlConnectOptions, MySqlRow, MySqlSslMode};
use sqlx::postgres::{PgConnectOptions, PgRow, PgSslMode};
use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::{Column, ColumnOrigin, ConnectOptions, Connection, Row, TypeInfo, ValueRef};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// The SERVER stopped short of what nyet asked for, so this result is
    /// incomplete for a reason the row count cannot show. Only MongoDB sets it
    /// (its 16 MiB reply cap can cut a batch before the row limit is reached);
    /// the SQL engines stream rows and detect truncation by counting them, so
    /// they leave it false. The cli ORs it into `meta.truncated` — an
    /// incomplete answer that reads as complete is the worst failure a read
    /// tool has (UX-1).
    pub truncated: bool,
    /// Where each column came from, as the DRIVER reported it — the raw
    /// material for the cli's net B (`validator::check_origins`). Same length
    /// and order as `columns`; a missing entry counts as `Origin::Unknown`
    /// (fail closed) on a connection with a PII policy.
    pub origins: Vec<Origin>,
}

/// Translate the driver's column metadata into the pure `Origin` the validator
/// judges. One place for every engine whose driver reports provenance at all
/// (the sqlx three), so they cannot drift. MongoDB, ClickHouse and Redis get no
/// provenance from the wire and do not come through here.
fn origins_of<C: Column>(columns: &[C]) -> Vec<Origin> {
    columns
        .iter()
        .map(|c| match c.origin() {
            ColumnOrigin::Table(t) => Origin::Table {
                table: t.table.to_string(),
                column: t.name.to_string(),
            },
            ColumnOrigin::Expression => Origin::Expression,
            ColumnOrigin::Unknown => Origin::Unknown,
        })
        .collect()
}

// Debug: test assertions unwrap on it; the fields are curated messages/hints
// with no secrets (never the password or the url).
#[derive(Debug)]
pub enum EngineError {
    /// The database could not be reached/opened (-> CONNECTION_FAILED, exit 6).
    Connect { message: String, hint: String },
    /// The database accepted the connection but rejected the query
    /// (-> DB_ERROR, exit 7).
    Db { message: String, hint: String },
    /// **Layer 1 refused, from inside the engine** (-> NYET + `reason`, exit 5).
    ///
    /// Only Redis produces it, and only because its classification needs the
    /// server: `COMMAND INFO` is what says whether a command reads, so the
    /// verdict cannot be reached before connecting. It is a REFUSAL, not a
    /// database error, and it has to arrive at the agent as one — a `NYET` with
    /// its reason and exit 5, like every other layer-1 verdict. Mapping it to
    /// `Db` would teach an agent that "the query was wrong" when the answer is
    /// "nyet does not run that".
    Refused {
        /// From the closed contract list (`crate::redis::DenyReason`).
        reason: &'static str,
        message: String,
        hint: String,
    },
    /// The server aborted the query on its own timeout (-> TIMEOUT, exit 8).
    /// Kept distinct from `Db` so a server-side statement_timeout and the
    /// cli's own tokio timeout both map to exit 8 (deterministic exit code).
    Timeout { message: String, hint: String },
}

/// Floor below which the connect handshake is never cut.
const CONNECT_DEADLINE_FLOOR_MS: u64 = 10_000;

/// Deadline for the TCP+TLS+auth handshake of a server engine, shared by
/// Postgres and MySQL so the two never drift. Its ONLY job is to bound a HUNG
/// connect (blackhole / dropped SYN) so nyet does not hang for the full outer
/// timeout — it is deliberately NOT the query timeout. A legitimate connect over
/// WAN/TLS/auth can take seconds, so we never cut below a 10s floor: a small
/// query timeout (e.g. `--timeout 1`, or the server-timeout tests' 300ms) must
/// still be able to connect — the SERVER's statement_timeout cancels the heavy
/// query on its own. For a large query timeout we stay a hair under it so a hung
/// connect is still classified CONNECTION_FAILED (exit 6), not the outer TIMEOUT.
fn connect_deadline(statement_timeout_ms: u64) -> Duration {
    Duration::from_millis(
        statement_timeout_ms
            .saturating_sub(250)
            .max(CONNECT_DEADLINE_FLOOR_MS),
    )
}

/// Client-side query-phase timeout: the query ran past the effective per-query
/// budget (the in-process tokio timer that wraps the fetch loop, AFTER a
/// successful connect). Distinct from a server-cancelled query only in wording;
/// both are `EngineError::Timeout` (exit 8). For SQLite this is the ONLY query
/// bound (no server timeout); for Postgres/MySQL it backstops the server-side
/// statement_timeout so the exit code is deterministic whichever fires.
fn client_timeout(query_timeout_ms: u64) -> EngineError {
    EngineError::Timeout {
        message: format!(
            "the query did not finish within the {}s timeout",
            query_timeout_ms / 1000
        ),
        hint: "narrow the query (WHERE / LIMIT), or raise --timeout or timeout_secs \
               in the config"
            .to_string(),
    }
}

/// The outcome of a guarded `execute`.
#[derive(Debug)]
pub enum QueryOutcome {
    /// The query ran. `estimate` is `None` when the guardrail was off or the
    /// database refused to plan the statement (fail open, see `Plan::Failed`);
    /// the cli reads the verdict off it — the engines never decide policy, they
    /// only obey `Guardrail::refuses`.
    Ran {
        result: ResultSet,
        estimate: Option<CostEstimate>,
    },
    /// The plan estimate was over the threshold: NOTHING was executed. `value`
    /// is the offending number the guardrail measured.
    Refused { estimate: CostEstimate, value: f64 },
    /// Planning itself outran the guardrail's budget: NOTHING was executed
    /// (fail closed — see `Plan::TooSlow`).
    PlanTooSlow { budget_ms: u64 },
    /// Net B refused the RESULT (PII): the query ran, but a result column's
    /// reported provenance is protected or unprovable, so no row is released.
    /// A variant rather than a check the caller may forget: rows leave the
    /// engine layer only through this enum, and every arm must be matched
    /// (finding 6).
    PiiRefused(Box<super::validator::Refusal>),
}

/// What the guardrail's EXPLAIN produced. The two failure modes are deliberately
/// NOT the same:
///
/// - `Failed` — the database refused to plan the statement (a role that may
///   `SELECT` a view but lacks `SHOW VIEW`, a form the server dislikes). The
///   agent cannot make this happen on an ARBITRARY query, and the query itself
///   would often succeed, so failing it would be a regression caused purely by
///   the guard: **fail open** (run, warn `GUARDRAIL_SKIPPED`). The error is kept
///   because `nyet explain`, which has no query to fall back on, reports it.
/// - `TooSlow` — planning outran the budget. Planning time IS agent-controlled
///   (PostgreSQL const-folds `IMMUTABLE` expressions at plan time; a MySQL
///   EXPLAIN over `information_schema` takes tens of seconds), so this one must
///   **fail closed**, or "make planning slow" becomes the way to switch the
///   guardrail off.
/// - `Broken` — the guardrail's own PLUMBING failed (savepoint, the statement
///   timeout it lends the EXPLAIN, the rollback, the restore). That is not "we
///   could not plan it": the session is in an unknown state — possibly an
///   aborted transaction, possibly still carrying the short EXPLAIN timeout — so
///   the error is surfaced and the query is NOT run on it.
///
/// `TooSlow` is TERMINAL: once planning has outrun the budget, no plumbing error
/// may change that verdict, because no plumbing is attempted at all — the
/// connection may still be busy with the planning we gave up on, and any
/// politeness (`ROLLBACK`, `COM_QUIT`, a graceful close) would queue behind it
/// until the query's own deadline fired and turned the refusal into a bare
/// TIMEOUT. Both "no plan" verdicts therefore hand the connection back to be
/// DROPPED (see `discard`), which costs one connection and saves the answer.
enum Plan {
    Got(CostEstimate),
    Failed(EngineError),
    TooSlow,
    Broken(EngineError),
}

impl Plan {
    /// What `nyet explain` makes of it: the plan is the answer there, so a
    /// database error surfaces (exit 7) and only the budget produces "no plan"
    /// (`Ok(None)` -> `verdict: no_estimate` plus a warning).
    fn into_answer(self) -> Result<Option<CostEstimate>, EngineError> {
        match self {
            Plan::Got(estimate) => Ok(Some(estimate)),
            Plan::Failed(e) | Plan::Broken(e) => Err(e),
            Plan::TooSlow => Ok(None),
        }
    }

    /// Is the connection unfit for a polite goodbye? `TooSlow` may leave the
    /// backend still planning, and `Broken` leaves the session in an unknown
    /// state; in both cases the socket is dropped instead of chatted with.
    fn discard(&self) -> bool {
        matches!(self, Plan::TooSlow | Plan::Broken(_))
    }

    /// Classify one guarded EXPLAIN. A server-side statement timeout during the
    /// EXPLAIN arrives as `EngineError::Timeout` (Postgres 57014, MySQL 3024 /
    /// MariaDB 1969) — that IS the budget firing, so it is TooSlow, not Failed.
    fn of(result: Result<Result<CostEstimate, EngineError>, tokio::time::error::Elapsed>) -> Plan {
        match result {
            Ok(Ok(estimate)) => Plan::Got(estimate),
            Ok(Err(EngineError::Timeout { .. })) | Err(_) => Plan::TooSlow,
            Ok(Err(e)) => Plan::Failed(e),
        }
    }
}

/// How long the guardrail's own EXPLAIN may take. The guardrail must not eat the
/// budget of the query it guards (a MySQL EXPLAIN over information_schema has
/// been seen taking 17.9s, turning a would-be refusal into a TIMEOUT), and past
/// this point the planning cost is itself evidence that the statement is
/// expensive — so exceeding it REFUSES the query (`Plan::TooSlow`).
const EXPLAIN_BUDGET_MS: u64 = 5_000;

/// The SERVER-side cap lent to the guardrail's EXPLAIN: `min(cap + grace,
/// timeout - reserve) - grace`. If planning alone ate the query's budget the
/// query could not have finished either, and the guardrail's "planning is itself
/// that expensive" beats a bare TIMEOUT — but only if it gets to answer first,
/// which is what the reserve buys. At the smallest legal timeout (1 s) this is
/// 300 ms; from 10 s up it is the flat cap.
///
/// **Applicability: `query_timeout_ms >= 1000`** — the minimum both the CLI and
/// the config enforce (`--timeout` / `timeout_secs` are >= 1 second). Below
/// ~200 ms the reserve eats everything and the budget collapses to 1 ms, so the
/// "strictly nested" property would stop holding; nothing can reach that today,
/// and a future sub-second input must revisit these three constants.
fn explain_budget_ms(query_timeout_ms: u64) -> u64 {
    EXPLAIN_BUDGET_MS
        .saturating_add(EXPLAIN_GRACE_MS)
        .min(query_timeout_ms.saturating_sub(EXPLAIN_RESERVE_MS))
        .max(1)
        .saturating_sub(EXPLAIN_GRACE_MS)
        .max(1)
}

/// The guardrail's OWN client deadline, DERIVED from the cap the server was
/// actually given rather than recomputed from the timeout. That is the point:
/// the cap is armed at one call site (it rides in the `BEGIN`) and the deadline
/// is awaited at another, and "the server cuts a grace period earlier than the
/// client" must not depend on both places doing the same arithmetic on the same
/// input. Here it holds by construction — the deadline cannot exist without the
/// budget it is derived from. (The two were once equal, and the race was then
/// decided by luck: the e2e flaked one run in three, and the losing branch fell
/// OPEN.)
fn explain_deadline_ms(budget_ms: u64) -> u64 {
    budget_ms.saturating_add(EXPLAIN_GRACE_MS)
}

/// How much later than the SERVER's cap the client deadline fires. The two used
/// to be set to the same value, and the race was decided by luck: when the
/// client won, the abandoned future left a dirty connection and the verdict was
/// silently downgraded to fail-open (the e2e flaked one run in three). The grace
/// makes the clean server-side cancellation the normal outcome and keeps the
/// tokio timer as what it is meant to be — a backstop for a server that ignores
/// its own cap.
const EXPLAIN_GRACE_MS: u64 = 500;

/// What the guardrail leaves of the query's deadline for itself to answer in.
/// Its own deadline must fire STRICTLY before the query's, or the outer timer
/// wins the race and the answer is a bare TIMEOUT instead of the refusal.
const EXPLAIN_RESERVE_MS: u64 = 200;

/// How long a graceful close may wait for the server to close the socket after
/// Terminate/COM_QUIT before the socket is simply dropped. Generous for a real
/// server (which closes in one round trip) and short enough that a pooler which
/// never closes it cannot cost the caller an answer already in hand.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Run the guardrail's EXPLAIN under its budget. The SERVER is capped at
/// `budget_ms` by the caller, so the usual outcome is a clean server-side error
/// rather than an abandoned future — an abandoned one keeps burning the backend
/// and its late error surfaces on the NEXT statement (observed on MySQL).
async fn budgeted_plan(
    deadline_ms: u64,
    plan: impl std::future::Future<Output = Result<CostEstimate, EngineError>>,
) -> Plan {
    Plan::of(tokio::time::timeout(Duration::from_millis(deadline_ms), plan).await)
}

/// The one planned abstraction of the project (D5). Fetches at most
/// `fetch_limit` rows; the caller passes limit+1 to detect truncation.
// The lint fires only because src/lib.rs makes this trait technically public;
// it is implemented and used inside this crate alone (the lib target exists for
// the fuzz targets), so no downstream ever needs a `Send` bound on the futures.
#[allow(async_fn_in_trait)]
pub trait Engine {
    /// Run the query, unless the guardrail's plan estimate says it is a monster
    /// (then nothing is executed). The EXPLAIN runs in the SAME read-only
    /// session — no second connect — inside the query deadline and its own
    /// (shorter) budget.
    async fn execute(
        &self,
        sql: &str,
        fetch_limit: u64,
        guardrail: &Guardrail,
    ) -> Result<QueryOutcome, EngineError>;

    /// The query plan and whatever estimate this engine publishes, without
    /// executing anything (`nyet explain`). **Never ANALYZE** — that would run
    /// the statement.
    ///
    /// `Ok(None)` means planning outran the guardrail's budget — the same budget
    /// and the same server-side cap the guarded query path uses, so `explain`
    /// answers what `query` would decide instead of grinding for the full
    /// timeout and reporting a cheerful "ok" for a statement `query` refuses.
    /// A database error still surfaces (exit 7): here the plan IS the answer,
    /// there is no query to fall back on.
    async fn estimate(&self, sql: &str) -> Result<Option<CostEstimate>, EngineError>;

    /// Introspect the schema through the same read-only session as a query.
    /// `table` is the agent's `[table]` argument: `Some` selects one object
    /// (empty result = not found, the cli turns that into DB_ERROR), `None`
    /// lists everything — with details only while the object count stays
    /// within `output::DETAIL_LIMIT`.
    async fn schema(&self, table: Option<&str>) -> Result<Schema, EngineError>;

}

fn db_error(e: sqlx::Error) -> EngineError {
    EngineError::Db {
        message: format!("the database returned an error: {}", error_text(&e)),
        hint: "check the query against the actual schema, e.g. \
               SELECT name, sql FROM sqlite_master WHERE type = 'table'"
            .to_string(),
    }
}

/// For database-level errors prefer the driver's bare message over
/// sqlx's wrapper text.
fn error_text(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => db.message().to_string(),
        other => other.to_string(),
    }
}

fn error_parts(e: EngineError) -> (String, String) {
    match e {
        EngineError::Connect { message, hint }
        | EngineError::Db { message, hint }
        | EngineError::Refused { message, hint, .. }
        | EngineError::Timeout { message, hint } => (message, hint),
    }
}

/// Per-object accumulator while catalog rows (one query per aspect) are
/// grouped back together by table.
struct TableParts {
    kind: &'static str,
    columns: Vec<SchemaColumn>,
    pk: Vec<String>,
    indexes: Vec<SchemaIndex>,
    fks: Vec<SchemaFk>,
    /// False when the server may have withheld columns (a column-level GRANT),
    /// which makes `build_table` drop every key touching an invisible column.
    full_columns: bool,
}

impl TableParts {
    fn new(kind: &'static str, full_columns: bool) -> Self {
        TableParts {
            kind,
            columns: Vec::new(),
            pk: Vec::new(),
            indexes: Vec::new(),
            fks: Vec::new(),
            full_columns,
        }
    }
}

/// Do we answer with names only? Past the limit an unfiltered listing would
/// burn the agent's context; naming a table always gets full detail.
fn over_detail_limit(table: Option<&str>, count: usize) -> bool {
    table.is_none() && count > super::output::DETAIL_LIMIT
}

/// The names-only answer past the detail limit.
fn listing(objects: Vec<(String, TableParts)>) -> Schema {
    sorted(
        objects
            .into_iter()
            .map(|(name, p)| SchemaTable {
                name,
                kind: p.kind,
                columns: None,
                indexes: Vec::new(),
                fks: Vec::new(),
                ..SchemaTable::default()
            })
            .collect(),
    )
}

/// The full answer: the collected parts through the shared presentation rules.
fn assemble(objects: Vec<(String, TableParts)>) -> Schema {
    sorted(
        objects
            .into_iter()
            .map(|(name, p)| {
                build_table(
                    name,
                    p.kind,
                    p.columns,
                    &p.pk,
                    p.indexes,
                    p.fks,
                    p.full_columns,
                )
            })
            .collect(),
    )
}

/// Tables are ordered by their display name — the contract's deterministic
/// order (snapshot-testable), which is NOT the catalog's grouping key for
/// PostgreSQL (that one is schema-first).
fn sorted(mut tables: Vec<SchemaTable>) -> Schema {
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    Schema {
        tables,
        na: None,
        databases: Vec::new(),
    }
}

/// SQLite via sqlx, opened with `mode=ro` (file-level read-only — layer 2:
/// even a write that slipped past the validator fails in the database).
pub struct Sqlite {
    pub path: PathBuf,
    /// The effective per-query wall budget in ms. SQLite has no server-side
    /// timeout, so this in-process deadline (wrapping the fetch) is the only
    /// query bound — the cli no longer wraps `execute` in an outer timeout.
    pub query_timeout_ms: u64,
}

impl Sqlite {
    /// Open the file read-only (layer 2), with the pre-check that turns
    /// sqlite's opaque "unable to open database file" into a real reason.
    /// Shared by `execute` and `schema`.
    async fn open(&self) -> Result<sqlx::SqliteConnection, EngineError> {
        // Explicit pre-check: sqlite's own "unable to open database file"
        // (code 14) does not say why. Relative paths resolve against the
        // process cwd (documented in README). Off the async thread: a
        // synchronous stat on a hung filesystem (NFS) would block the
        // single-threaded runtime and defeat the caller's timeout.
        let stat_path = self.path.clone();
        let metadata = tokio::task::spawn_blocking(move || std::fs::metadata(&stat_path))
            .await
            .map_err(|e| EngineError::Connect {
                message: format!("cannot open SQLite database {}: {e}", self.path.display()),
                hint: "check `path` for this connection in the config".to_string(),
            })?;
        match metadata {
            Err(e) => {
                return Err(EngineError::Connect {
                    message: format!("cannot open SQLite database {}: {e}", self.path.display()),
                    hint: "check `path` for this connection in the config; a relative \
                           path resolves against the current directory"
                        .to_string(),
                })
            }
            Ok(md) if md.is_dir() => {
                return Err(EngineError::Connect {
                    message: format!(
                        "cannot open SQLite database {}: it is a directory",
                        self.path.display()
                    ),
                    hint: "point `path` in the config at the database file itself".to_string(),
                })
            }
            Ok(_) => {}
        }
        SqliteConnectOptions::new()
            .filename(&self.path)
            .read_only(true)
            .connect()
            .await
            .map_err(|e| EngineError::Connect {
                message: format!(
                    "cannot open SQLite database {}: {}",
                    self.path.display(),
                    error_text(&e)
                ),
                hint: "check that the file is a readable SQLite database".to_string(),
            })
    }
}

impl Engine for Sqlite {
    /// The guardrail is not applicable here and the parameter is ignored on
    /// purpose: SQLite's `EXPLAIN QUERY PLAN` publishes no cost or row estimate
    /// at all, so `guardrail::resolve` accepts nothing but `off` for this engine
    /// (a config error otherwise, exit 3). Running the EXPLAIN anyway would burn
    /// a round trip to learn nothing.
    async fn execute(
        &self,
        sql: &str,
        fetch_limit: u64,
        _guardrail: &Guardrail,
    ) -> Result<QueryOutcome, EngineError> {
        let mut conn = self.open().await?;
        // Bound the QUERY phase (not the local file open above) on the effective
        // per-query budget: sqlite has no server-side timeout, so this in-process
        // deadline is the only query bound. On expiry the fetch future is dropped
        // and we report Timeout (exit 8); the sqlite worker thread may keep
        // grinding until the process exits (the cli calls shutdown_background),
        // so on the timeout path we do NOT await the connection afterwards.
        let deadline = Duration::from_millis(self.query_timeout_ms);
        match tokio::time::timeout(deadline, async move {
            let mut columns: Vec<String> = Vec::new();
            let mut origins: Vec<Origin> = Vec::new();
            let mut rows: Vec<Vec<Value>> = Vec::new();
            {
                // AssertSqlSafe is sqlx's marker for "audited dynamic SQL":
                // running caller-supplied SQL is nyet's whole job, and the audit
                // is the validator + the read-only open mode.
                let mut stream = sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).fetch(&mut conn);
                while (rows.len() as u64) < fetch_limit {
                    match stream.try_next().await.map_err(db_error)? {
                        Some(row) => {
                            if columns.is_empty() {
                                columns =
                                    row.columns().iter().map(|c| c.name().to_string()).collect();
                                origins = origins_of(row.columns());
                            }
                            rows.push(decode_row(&row)?);
                        }
                        None => break,
                    }
                }
            }
            // No rows -> no column names from the stream; ask the prepared
            // statement so table output can still print a header. Best effort:
            // a prepare failure leaves columns empty.
            if rows.is_empty() && columns.is_empty() {
                use sqlx::{Executor, SqlSafeStr, Statement};
                let sql_str = sqlx::AssertSqlSafe(sql.to_string()).into_sql_str();
                if let Ok(statement) = conn.prepare(sql_str).await {
                    columns = statement
                        .columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect();
                    origins = origins_of(statement.columns());
                }
            }
            let _ = conn.close().await;
            Ok::<QueryOutcome, EngineError>(QueryOutcome::Ran {
                result: ResultSet {
                    columns,
                    rows,
                    origins,
                    truncated: false,
                },
                estimate: None,
            })
        })
        .await
        {
            Ok(r) => r,
            Err(_elapsed) => Err(client_timeout(self.query_timeout_ms)),
        }
    }

    /// No budget games here: SQLite has no server-side cap to lend and its
    /// planner publishes no numbers anyway, so the only bound is the ordinary
    /// query timeout.
    async fn estimate(&self, sql: &str) -> Result<Option<CostEstimate>, EngineError> {
        let mut conn = self.open().await?;
        let deadline = Duration::from_millis(self.query_timeout_ms);
        match tokio::time::timeout(deadline, sqlite_plan(&mut conn, sql)).await {
            Ok(r) => {
                let _ = conn.close().await;
                r.map(Some)
            }
            Err(_elapsed) => Err(client_timeout(self.query_timeout_ms)),
        }
    }

    async fn schema(&self, table: Option<&str>) -> Result<Schema, EngineError> {
        let mut conn = self.open().await?;
        let deadline = Duration::from_millis(self.query_timeout_ms);
        // Same shape as execute: on expiry the future is dropped and the
        // connection is NOT awaited (the worker may still be busy).
        match tokio::time::timeout(deadline, sqlite_schema(&mut conn, table)).await {
            Ok(r) => {
                let _ = conn.close().await;
                r
            }
            Err(_elapsed) => Err(client_timeout(self.query_timeout_ms)),
        }
    }

}

/// SQLite plan: `EXPLAIN QUERY PLAN` — the human-readable plan and nothing
/// else (SQLite publishes no cost/row estimates, hence no guardrail here).
///
/// **Constant prefix + the SQL the validator already accepted, and never
/// ANALYZE:** `EXPLAIN QUERY PLAN` only plans, while SQLite's `EXPLAIN ANALYZE`
/// (and every other engine's) would RUN the statement — the one thing a
/// guardrail must never do.
async fn sqlite_plan(
    conn: &mut sqlx::SqliteConnection,
    sql: &str,
) -> Result<CostEstimate, EngineError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
        .fetch_all(&mut *conn)
        .await
        .map_err(db_error)?;
    let (columns, values) = plan_table(&rows, decode_row)?;
    Ok(super::guardrail::sqlite_estimate(&columns, &values))
}

/// A tabular plan (SQLite / MySQL) -> (column names, decoded values), for the
/// pure parsers in `guardrail`. Column names come from the first row; an empty
/// plan yields empty everything (which reads as "no estimate").
fn plan_table<R: Row>(
    rows: &[R],
    decode: fn(&R) -> Result<Vec<Value>, EngineError>,
) -> Result<(Vec<String>, Vec<Vec<Value>>), EngineError> {
    let columns = rows.first().map_or_else(Vec::new, |r| {
        r.columns().iter().map(|c| c.name().to_string()).collect()
    });
    let values = rows.iter().map(decode).collect::<Result<_, _>>()?;
    Ok((columns, values))
}

/// SQLite introspection: `sqlite_master` for the object list, then the
/// table-valued pragmas for the details.
///
/// **The `[table]` argument never reaches SQL.** It is compared in Rust against
/// the names the catalog returned, and every pragma below is called with the
/// name that came BACK from the catalog, passed as a bound parameter — so
/// neither `users; DROP TABLE x` nor `users'--` can be anything but a name that
/// matches nothing. (`pragma_table_xinfo(?)` is the table-valued form of
/// `PRAGMA table_xinfo`; unlike the statement form it takes bind parameters,
/// which is why it is used here.)
async fn sqlite_schema(
    conn: &mut sqlx::SqliteConnection,
    table: Option<&str>,
) -> Result<Schema, EngineError> {
    let rows = sqlx::query("SELECT name, type FROM sqlite_master WHERE type IN ('table','view')")
        .fetch_all(&mut *conn)
        .await
        .map_err(db_error)?;
    let mut objects: Vec<(String, TableParts)> = Vec::new();
    for row in rows {
        let name: String = row.try_get("name").map_err(db_error)?;
        let kind: String = row.try_get("type").map_err(db_error)?;
        // sqlite_sequence / sqlite_stat1 / autoindexes: engine bookkeeping.
        if name.starts_with("sqlite_") {
            continue;
        }
        // SQLite resolves identifiers ASCII-case-insensitively, so `nyet schema
        // db USERS` must find `users` — exactly like `SELECT * FROM USERS`.
        if table.is_some_and(|t| !t.eq_ignore_ascii_case(&name)) {
            continue;
        }
        objects.push((
            name,
            // SQLite has no privileges: the pragma always lists every column.
            TableParts::new(if kind == "view" { "view" } else { "table" }, true),
        ));
    }
    if over_detail_limit(table, objects.len()) {
        return Ok(listing(objects));
    }
    for (name, parts) in &mut objects {
        let (columns, pk) = sqlite_columns(conn, name).await?;
        parts.columns = columns;
        parts.pk = pk;
        // A view has no indexes or foreign keys.
        if parts.kind == "table" {
            parts.indexes = sqlite_indexes(conn, name).await?;
            parts.fks = sqlite_fks(conn, name).await?;
        }
    }
    Ok(assemble(objects))
}

/// Columns in ordinal order + the primary-key column names in key order.
/// `type` is the declared type, verbatim (empty for an untyped column).
///
/// `table_xinfo`, not `table_info`: the latter hides GENERATED columns, which
/// are perfectly readable columns an agent must know about. Its `hidden` marks
/// them (2 = VIRTUAL, 3 = STORED, 0 = ordinary); only `1` — a virtual-table
/// hidden column, not selectable by name — is dropped.
async fn sqlite_columns(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Result<(Vec<SchemaColumn>, Vec<String>), EngineError> {
    let rows = sqlx::query(
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden \
         FROM pragma_table_xinfo(?) ORDER BY cid",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    let mut columns = Vec::new();
    let mut pk: Vec<(i64, String)> = Vec::new();
    for row in rows {
        let name: String = row.try_get("name").map_err(db_error)?;
        // Lenient on the rest: a pragma column nyet cannot decode must not
        // fail the whole introspection (D3 — no panics, no dead ends).
        let ty: String = row.try_get("type").unwrap_or_default();
        let notnull: i64 = row.try_get("notnull").unwrap_or(0);
        let default: Option<String> = row.try_get("dflt_value").unwrap_or(None);
        let position: i64 = row.try_get("pk").unwrap_or(0);
        if row.try_get::<i64, _>("hidden").unwrap_or(0) == 1 {
            continue;
        }
        if position > 0 {
            pk.push((position, name.clone()));
        }
        columns.push(SchemaColumn {
            name,
            ty,
            // As declared: SQLite's rowid-alias PRIMARY KEY (`id INTEGER
            // PRIMARY KEY`) carries no NOT NULL, so it reads back nullable
            // here — build_table normalizes a pk column to false so the three
            // engines agree (see docs/dev/DEV.md).
            nullable: notnull == 0,
            pk: false,
            unique: false,
            default,
            // A catalog IS the schema: no provenance marker (that is a
            // MongoDB question, where the fields are inferred).
            source: None,
            seen: None,
            pii: None,
        });
    }
    pk.sort_by_key(|(position, _)| *position);
    Ok((columns, pk.into_iter().map(|(_, name)| name).collect()))
}

async fn sqlite_indexes(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Result<Vec<SchemaIndex>, EngineError> {
    let rows = sqlx::query(
        "SELECT name, \"unique\", origin, \"partial\" FROM pragma_index_list(?) ORDER BY seq",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    let mut indexes = Vec::new();
    for row in rows {
        let name: String = row.try_get("name").map_err(db_error)?;
        let unique: i64 = row.try_get("unique").unwrap_or(0);
        let origin: String = row.try_get("origin").unwrap_or_default();
        // A partial index (`CREATE UNIQUE INDEX ... WHERE ...`) enforces
        // uniqueness only over the rows its predicate matches, so it is
        // reported as an ordinary index — claiming `unique` would promise the
        // agent a key that does not hold for the whole table.
        let partial: i64 = row.try_get("partial").unwrap_or(0);
        // origin 'pk' backs the PRIMARY KEY: redundant with the pk flags.
        if origin == "pk" {
            continue;
        }
        let parts = sqlx::query("SELECT name FROM pragma_index_info(?) ORDER BY seqno")
            .bind(&name)
            .fetch_all(&mut *conn)
            .await
            .map_err(db_error)?;
        // NULL for an expression key (`CREATE INDEX ... (lower(x))`): kept as an
        // Expression part so the key arity survives (the pragma has no text for
        // it, hence None).
        let columns: Vec<KeyPart> = parts
            .iter()
            .map(|r| match r.try_get::<Option<String>, _>("name") {
                Ok(Some(name)) => KeyPart::Named(name),
                _ => KeyPart::Expression(None),
            })
            .collect();
        if columns.is_empty() {
            continue;
        }
        indexes.push(SchemaIndex {
            name,
            columns,
            unique: unique != 0 && partial == 0,
        });
    }
    Ok(indexes)
}

async fn sqlite_fks(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Result<Vec<SchemaFk>, EngineError> {
    let rows = sqlx::query(
        "SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?) ORDER BY id, seq",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    // Rows are one column each, grouped by `id` (a composite key spans several).
    let mut fks: Vec<(i64, SchemaFk)> = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("id").map_err(db_error)?;
        let ref_table: String = row.try_get("table").map_err(db_error)?;
        let from: String = row.try_get("from").map_err(db_error)?;
        let to: Option<String> = row.try_get("to").unwrap_or(None);
        match fks.last_mut() {
            Some((last, fk)) if *last == id => {
                fk.columns.push(from);
                fk.ref_columns.extend(to);
            }
            _ => fks.push((
                id,
                SchemaFk {
                    columns: vec![from],
                    ref_table,
                    ref_columns: to.into_iter().collect(),
                },
            )),
        }
    }
    // `REFERENCES orgs` without a column list points at the parent's primary
    // key; SQLite reports those columns as NULL, so resolve them. A parent
    // with no declared primary key (an implicit rowid reference, or a parent
    // that does not exist) leaves `ref_columns` empty — reported as-is and
    // documented, not invented.
    for (_, fk) in &mut fks {
        if fk.ref_columns.is_empty() {
            fk.ref_columns = sqlite_columns(conn, &fk.ref_table).await?.1;
        }
    }
    Ok(fks.into_iter().map(|(_, fk)| fk).collect())
}

/// PostgreSQL via sqlx. Layer 2 (DESIGN §3) is server-enforced: the
/// connection is opened with `-c default_transaction_read_only=on -c
/// statement_timeout=<ms>` and every read runs inside an explicit
/// `BEGIN READ ONLY` transaction — a write that slipped past the validator is
/// refused by the database itself (SQLSTATE 25006), and a runaway query is
/// killed by the server timeout (57014 -> EngineError::Timeout).
pub struct Postgres {
    /// The `url` from the config (no password embedded by convention).
    pub url: String,
    /// Resolved from the connection's `password` by the cli; never logged/printed.
    pub password: Option<String>,
    /// Server-side statement_timeout, from the effective per-query timeout.
    pub statement_timeout_ms: u64,
    /// The effective per-query wall budget in ms: the in-process deadline that
    /// wraps the query phase (AFTER connect), backstopping the server-side
    /// statement_timeout so a runaway query is TIMEOUT (exit 8) whichever fires.
    pub query_timeout_ms: u64,
    /// When an SSH tunnel is up, `(127.0.0.1, local_port)` to connect through
    /// instead of the url's host/port. User/dbname/params from the url stay.
    pub host_override: Option<(String, u16)>,
    /// Test-only override for the connect handshake deadline (ms). Production
    /// (the cli) passes `None` -> `connect_deadline(statement_timeout_ms)`; the
    /// hung-connect tests pass `Some(short)` so they finish fast without the 10s
    /// production floor.
    pub connect_timeout_ms: Option<u64>,
    /// Resolve column PROVENANCE for the result (net B). Set by the cli only
    /// when the connection has a PII policy, because it costs one extra
    /// DESCRIBE round trip: on the FETCH path sqlx asks Postgres not to resolve
    /// origins, so `PgColumn::origin()` comes back `Unknown` for real table
    /// columns (only the oid+attnum are filled in). Preparing the statement
    /// FIRST resolves the names and caches them on the connection, so the
    /// following fetch reports `Table(table, column)` — verified against
    /// postgres:16-alpine, see docs/dev/DEV.md.
    pub resolve_column_origins: bool,
}

/// Redirect host+port to the tunnel's local end while keeping every other
/// connect option (user, dbname, params, password) from the url. Overriding
/// `PgConnectOptions` is more robust than rewriting the url string. Pure — no
/// IO — so it is unit-tested without a database.
///
/// Also rewrites `sslmode` for the tunnel leg, because the url's mode describes
/// a hop that no longer exists — the connection now goes to 127.0.0.1:
///
/// - everything below `require` (`disable`, `allow`, `prefer` — the last one is
///   sqlx's default, so this is also "the url said nothing") becomes `disable`:
///   the ssh hop already encrypts, so the TLS round trip would buy nothing.
/// - `verify-full` is downgraded to `verify-ca`, the ONLY step it must lose:
///   sqlx checks the hostname for `verify-full` alone (`accept_invalid_hostnames
///   = !VerifyFull`), and the certificate names the real host while the socket
///   goes to 127.0.0.1. `verify-ca` itself is KEPT — it authenticates the chain
///   without looking at the hostname, so it survives the tunnel intact.
/// - `require` stays `require`: it encrypts without authenticating, which is
///   what the url asked for, and some servers refuse plaintext outright, so
///   forcing `disable` on them made the connection impossible (Yandex MDB's
///   odyssey answers `SSL is required`).
///
/// The DIRECT path (`None`) is left untouched, so the `sslmode` from the url is
/// honored by sqlx's rustls backend (prefer/require/verify-ca/verify-full).
fn apply_host_override(
    opts: PgConnectOptions,
    host_override: &Option<(String, u16)>,
) -> PgConnectOptions {
    match host_override {
        Some((host, port)) => {
            let mode = match opts.get_ssl_mode() {
                PgSslMode::Disable | PgSslMode::Allow | PgSslMode::Prefer => PgSslMode::Disable,
                PgSslMode::VerifyCa | PgSslMode::VerifyFull => PgSslMode::VerifyCa,
                _ => PgSslMode::Require,
            };
            opts.host(host).port(*port).ssl_mode(mode)
        }
        None => opts,
    }
}

impl Postgres {
    /// The ONE owner of "how large a `statement_timeout` this server accepts":
    /// PostgreSQL rejects anything above INT_MAX ms (~24.8 days) at connect, so
    /// an unclamped value turns a generous timeout into CONNECTION_FAILED.
    /// Every place that fills `statement_timeout_ms` goes through here — the
    /// cli builds the engine with it, and shrinks it through it too.
    pub fn clamp_statement_timeout(ms: u64) -> u64 {
        ms.min(i32::MAX as u64)
    }

    /// Build the connect options (layer 2 + the tunnel override) and run the
    /// handshake under its own generous deadline. Shared by `execute` and
    /// `schema`, so introspection gets the same read-only, timeout-capped
    /// session as a query.
    async fn connect(&self) -> Result<sqlx::PgConnection, EngineError> {
        // Never echo the url on a parse error: it may embed credentials.
        let opts: PgConnectOptions = self.url.parse().map_err(|_| EngineError::Connect {
            message: "the `url` for this connection is not a valid PostgreSQL URL".to_string(),
            hint: "use the form postgres://user@host:port/dbname; keep the password in \
                   this connection's `password`, not in the url"
                .to_string(),
        })?;
        // Layer 2: the SERVER enforces read-only and the timeout, independent
        // of the client. `.options()` becomes libpq `-c key=value` startup
        // options (statement_timeout in bare milliseconds).
        let opts = opts
            .options([
                ("default_transaction_read_only", "on".to_string()),
                ("statement_timeout", self.statement_timeout_ms.to_string()),
            ])
            .application_name("nyet");
        let opts = match &self.password {
            Some(pw) => opts.password(pw),
            // No password configured: try trust/peer auth (local dev). An auth
            // failure surfaces as CONNECTION_FAILED with a hint below.
            None => opts,
        };
        // If a tunnel is up, connect to its local end (127.0.0.1:<port>)
        // instead of the url's host — everything else from the url is kept.
        let opts = apply_host_override(opts, &self.host_override);
        // Bound connect on its OWN generous deadline so a hung TCP handshake
        // (firewall blackhole: SYN accepted, handshake never completes) is
        // CONNECTION_FAILED (exit 6) instead of hanging — see connect_deadline
        // (it is NOT the query timeout; a legit connect may take seconds).
        let deadline = self
            .connect_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| connect_deadline(self.statement_timeout_ms));
        match tokio::time::timeout(deadline, opts.connect()).await {
            Ok(r) => r.map_err(pg_connect_error),
            Err(_elapsed) => Err(EngineError::Connect {
                message: "the connection to the PostgreSQL database did not complete in time"
                    .to_string(),
                hint: "check the host/port in `url` and that the server is reachable \
                       (a firewall may be dropping the connection)"
                    .to_string(),
            }),
        }
    }

}

impl Engine for Postgres {
    async fn execute(
        &self,
        sql: &str,
        fetch_limit: u64,
        guardrail: &Guardrail,
    ) -> Result<QueryOutcome, EngineError> {
        let mut conn = self.connect().await?;

        // Bound the QUERY phase on the effective per-query budget (connect above
        // has its OWN generous deadline). Keeping the two timers separate means a
        // slow/hung connect is always CONNECTION_FAILED (exit 6) and only a slow
        // QUERY is TIMEOUT (exit 8), deterministic regardless of --timeout size.
        // Complements the server statement_timeout (57014); whichever fires, 8.
        let deadline = Duration::from_millis(self.query_timeout_ms);
        let phase = tokio::time::timeout(deadline, async move {
            // The guardrail's EXPLAIN runs HERE: same read-only session (no
            // second connect), before a single row of the real query is read —
            // so when it is going to run, its savepoint and its server-side
            // budget are armed in the same message as the BEGIN.
            let budget_ms = explain_budget_ms(self.query_timeout_ms);
            let arm = guardrail.plans().then_some(budget_ms);
            if let Err(e) = Postgres::begin_read_only(&mut conn, arm).await {
                // Arming shares the fate of `Plan::Broken`: after a partially
                // applied BEGIN+SAVEPOINT+SET the session state is unknown, so
                // the socket is dropped rather than chatted with.
                drop(conn);
                return (Err(e), None);
            }

            let estimate = match guardrail.plans() {
                false => None,
                true => {
                    // The SAME `budget_ms` the server was armed with above, so
                    // the client deadline is derived from it, never recomputed.
                    let plan =
                        pg_guarded_plan(&mut conn, sql, budget_ms, self.statement_timeout_ms).await;
                    // THE decision about this connection, here as everywhere
                    // (`Plan::discard`): anything the guardrail could not leave
                    // clean is DROPPED, never closed politely — a graceful
                    // ROLLBACK/close queues behind the planning we abandoned (or
                    // behind whatever broke) until the query deadline turns the
                    // refusal into a bare TIMEOUT.
                    if plan.discard() {
                        drop(conn);
                        return (
                            match plan {
                                Plan::Broken(e) => Err(e),
                                // TooSlow is the only other discarding verdict.
                                _ => Ok(QueryOutcome::PlanTooSlow { budget_ms }),
                            },
                            None,
                        );
                    }
                    match plan {
                        Plan::Got(estimate) => Some(estimate),
                        // Failed: the database would not plan it — fail OPEN
                        // (the query runs and reports its own error, if any).
                        _ => None,
                    }
                }
            };
            if let Some(value) = estimate.as_ref().and_then(|e| guardrail.refuses(e)) {
                return (
                    Ok(QueryOutcome::Refused {
                        // Some by construction: refuses() answered about this one.
                        estimate: estimate.expect("the refused estimate"),
                        value,
                    }),
                    Some(conn),
                );
            }

            // Net B needs the column origins the fetch path does not resolve;
            // preparing first fills the connection's origin cache (and the
            // statement cache, so the fetch below reuses this very PARSE). Best
            // effort: if it fails the origins stay Unknown and the cli refuses
            // the result — fail closed, never a silent pass.
            if self.resolve_column_origins {
                use sqlx::{Executor, SqlSafeStr};
                let sql_str = sqlx::AssertSqlSafe(sql.to_string()).into_sql_str();
                let _ = conn.prepare(sql_str).await;
            }
            let mut columns: Vec<String> = Vec::new();
            let mut origins: Vec<Origin> = Vec::new();
            let mut rows: Vec<Vec<Value>> = Vec::new();
            let fetched = {
                let mut stream = sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).fetch(&mut conn);
                loop {
                    if (rows.len() as u64) >= fetch_limit {
                        break Ok(());
                    }
                    match stream.try_next().await {
                        Ok(Some(row)) => {
                            if columns.is_empty() {
                                columns =
                                    row.columns().iter().map(|c| c.name().to_string()).collect();
                                origins = origins_of(row.columns());
                            }
                            match decode_pg_row(&row) {
                                Ok(r) => rows.push(r),
                                Err(e) => break Err(e),
                            }
                        }
                        Ok(None) => break Ok(()),
                        Err(e) => break Err(pg_error(e)),
                    }
                }
            };
            // Empty result -> no columns from the stream; ask the prepared
            // statement so table/csv output still has a header (best effort).
            if fetched.is_ok() && rows.is_empty() && columns.is_empty() {
                use sqlx::{Executor, SqlSafeStr, Statement};
                let sql_str = sqlx::AssertSqlSafe(sql.to_string()).into_sql_str();
                if let Ok(statement) = conn.prepare(sql_str).await {
                    columns = statement
                        .columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect();
                    origins = origins_of(statement.columns());
                }
            }
            let answer = fetched.map(|()| QueryOutcome::Ran {
                result: ResultSet {
                    columns,
                    rows,
                    origins,
                    truncated: false,
                },
                estimate,
            });
            (answer, Some(conn))
        })
        .await;
        pg_finish(phase, self.query_timeout_ms).await
    }

    async fn estimate(&self, sql: &str) -> Result<Option<CostEstimate>, EngineError> {
        let mut conn = self.connect().await?;
        let deadline = Duration::from_millis(self.query_timeout_ms);
        let phase = tokio::time::timeout(deadline, async move {
            // `explain` always plans, so the savepoint and the budget are always
            // armed with the BEGIN (see the `execute` twin for the drop rule).
            let budget_ms = explain_budget_ms(self.query_timeout_ms);
            if let Err(e) = Postgres::begin_read_only(&mut conn, Some(budget_ms)).await {
                drop(conn);
                return (Err(e), None);
            }
            // The guarded path, exactly as `query` runs it — so a statement
            // whose planning `query` refuses is not blessed with an "ok" here.
            let plan = pg_guarded_plan(&mut conn, sql, budget_ms, self.statement_timeout_ms).await;
            // A connection the guardrail could not leave clean is dropped, never
            // chatted with (`Plan::discard` — see DEV.md).
            let keep = match plan.discard() {
                true => {
                    drop(conn);
                    None
                }
                false => Some(conn),
            };
            (plan.into_answer(), keep)
        })
        .await;
        pg_finish(phase, self.query_timeout_ms).await
    }

    async fn schema(&self, table: Option<&str>) -> Result<Schema, EngineError> {
        let mut conn = self.connect().await?;
        let deadline = Duration::from_millis(self.query_timeout_ms);
        let phase = tokio::time::timeout(deadline, async move {
            // `schema` never plans: no savepoint, no borrowed budget, so a failed
            // BEGIN leaves a session whose state IS known and can be closed
            // politely.
            if let Err(e) = Postgres::begin_read_only(&mut conn, None).await {
                return (Err(e), Some(conn));
            }
            let schema = pg_schema(&mut conn, table).await;
            (schema, Some(conn))
        })
        .await;
        pg_finish(phase, self.query_timeout_ms).await
    }

}

impl Postgres {
    /// Layer 2, client half: an explicit read-only transaction (belt and
    /// suspenders over the connection's `default_transaction_read_only`) — the
    /// read runs inside it, a smuggled write fails. Shared by execute/schema,
    /// mirroring `Mysql::begin_read_only`.
    ///
    /// `arm` is the guardrail's server-side budget: when the caller is about to
    /// plan, the savepoint and that budget travel in the SAME message as the
    /// BEGIN (one round trip instead of three — `pg_guarded_plan` then only
    /// EXPLAINs and repairs). The simple-query protocol runs a `;`-separated
    /// string in one round trip, in order, stopping at the first error — so
    /// `BEGIN READ ONLY` still takes effect before anything else can, and the
    /// only statement that ever carries agent text (the query itself) still
    /// travels alone, as a prepared statement.
    async fn begin_read_only(
        conn: &mut sqlx::PgConnection,
        arm: Option<u64>,
    ) -> Result<(), EngineError> {
        use sqlx::Executor;
        let sql = match arm {
            None => "BEGIN READ ONLY".to_string(),
            Some(budget_ms) => format!(
                "BEGIN READ ONLY; SAVEPOINT {PG_GUARDRAIL_SAVEPOINT}; \
                 SET LOCAL statement_timeout = {budget_ms}"
            ),
        };
        conn.execute(sqlx::AssertSqlSafe(sql))
            .await
            .map_err(pg_error)?;
        Ok(())
    }
}

/// The savepoint the guardrail plans inside. Named once: it is set by
/// `begin_read_only` and rolled back by `pg_guarded_plan`.
const PG_GUARDRAIL_SAVEPOINT: &str = "nyet_guardrail";

/// PostgreSQL plan: `EXPLAIN (FORMAT JSON)` — the JSON tree carries the
/// planner's `Total Cost` and `Plan Rows` on the top node, which is what the
/// guardrail compares (parsing is pure, in `guardrail`).
///
/// **Constant prefix + the SQL the validator already accepted, and NEVER
/// ANALYZE:** `EXPLAIN` alone only plans the statement, `EXPLAIN ANALYZE` would
/// execute it — the one thing a guardrail must not do.
async fn pg_plan(conn: &mut sqlx::PgConnection, sql: &str) -> Result<CostEstimate, EngineError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {sql}")))
        .fetch_one(&mut *conn)
        .await
        .map_err(pg_error)?;
    // The column is `json` on every supported server; a text-returning one (or
    // any shape we did not expect) must not fail the run — an unreadable plan
    // is simply "no estimate" (the cli warns GUARDRAIL_SKIPPED and runs on).
    let plan = row.try_get::<Value, _>(0).unwrap_or_else(|_| {
        row.try_get::<String, _>(0)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null)
    });
    Ok(super::guardrail::postgres_estimate(plan))
}

/// The guardrail's EXPLAIN on PostgreSQL, wrapped in the two things that keep
/// the SESSION healthy and the budget honest (both review findings, verified
/// live):
///
/// - **`SAVEPOINT` + `ROLLBACK TO SAVEPOINT` around it.** A failing EXPLAIN
///   aborts the transaction, and every later statement then dies with "current
///   transaction is aborted" — so the fail-open path did not actually work on
///   Postgres: `SELECT * FROM nope_x` reported that instead of "relation does
///   not exist". Rolling back to the savepoint restores the transaction AND
///   (savepoints being transactional) the `statement_timeout` the arming set, so
///   one mechanism undoes everything.
/// - **`SET LOCAL statement_timeout = <budget>`.** The SERVER stops planning at
///   the budget instead of us abandoning a future that keeps burning the backend.
///   The error comes back as 57014 -> `EngineError::Timeout` -> `Plan::TooSlow`.
///
/// The ARMING half (savepoint + budget) rode in the caller's `BEGIN READ ONLY`
/// message, and the repair half is one message too, so the scaffolding around
/// the EXPLAIN costs ONE round trip instead of four. Both groups are collapsible for the same
/// reason: within a group every statement fails the same way to the caller
/// (arming -> the session was never set up; repair -> `Plan::Broken`, the
/// session is unusable either way), so nothing is lost by not knowing which one
/// the server tripped on.
async fn pg_guarded_plan(
    conn: &mut sqlx::PgConnection,
    sql: &str,
    budget_ms: u64,
    restore_ms: u64,
) -> Plan {
    use sqlx::Executor;
    // The client deadline sits a grace period BEHIND the server cap (and inside
    // the query's own deadline), so the normal way out is the server's 57014.
    // `budget_ms` is the very value the caller armed the server with, not a
    // second computation of it — see `explain_deadline_ms`.
    let plan = budgeted_plan(explain_deadline_ms(budget_ms), pg_plan(conn, sql)).await;
    if matches!(plan, Plan::TooSlow) {
        // TERMINAL — and nothing is attempted on the connection afterwards, so
        // no plumbing error can rewrite the verdict. When OUR deadline is what
        // fired, the backend is still planning and the connection is busy: the
        // repair below would queue behind the very work we gave up on and burn
        // the query's remaining time, which is how this case answered TIMEOUT
        // instead of a refusal. The caller drops the socket.
        return Plan::TooSlow;
    }
    // One round trip, and the two halves are inseparable anyway: the rollback
    // clears an aborted transaction and (savepoints being transactional) already
    // restores statement_timeout, while the explicit SET is the belt-and-
    // suspenders half — if the rollback somehow left the budget in place, the
    // query would be cut short at the EXPLAIN's budget. Failing either is a
    // broken session: running the query under an unknown timeout would report a
    // TIMEOUT that is nobody's actual setting, so it is surfaced, never fallen
    // open on. (TooSlow cannot reach here — it returned above.)
    if let Err(e) = conn
        .execute(sqlx::AssertSqlSafe(format!(
            "ROLLBACK TO SAVEPOINT {PG_GUARDRAIL_SAVEPOINT}; \
             SET LOCAL statement_timeout = {restore_ms}"
        )))
        .await
    {
        return Plan::Broken(pg_error(e));
    }
    plan
}

/// Read-only: nothing to persist, so rollback (cheaper than commit) and close
/// the connection gracefully. Best effort — the answer is already in hand.
///
/// TWO rules make this safe, and both are load-bearing:
///
/// 1. The whole goodbye is BOUNDED. On a TLS connection `close()` is not the
///    fire-and-forget half-close it looks like: sqlx's rustls socket runs
///    `complete_io` on shutdown, which READS, waiting for the peer's
///    `close_notify` — and a pooler may never send one (Yandex MDB's odyssey
///    neither answers nor closes the socket). The `ROLLBACK` above waits on the
///    server too. Either can stall forever.
/// 2. Callers run this OUTSIDE the query deadline (see `pg_finish`). A bound
///    alone is not enough: inside the deadline the grace is only
///    `min(remaining, CLOSE_GRACE)`, so a small `timeout_secs` would still let
///    the outer timer discard an answer we already have.
async fn pg_close_read_only(conn: sqlx::PgConnection) {
    let _ = tokio::time::timeout(CLOSE_GRACE, async move {
        use sqlx::Executor;
        let mut conn = conn;
        let _ = conn.execute("ROLLBACK").await;
        let _ = conn.close().await;
    })
    .await;
}

/// Unwrap a bounded query phase, then say goodbye to its connection AFTER the
/// deadline. The phase hands back the connection it wants closed politely
/// (`None` for one it deliberately dropped — see `Plan::discard`), and the
/// close cannot cost the caller the answer, because the deadline is already
/// spent by then. `Elapsed` stays TIMEOUT, exactly as before.
async fn pg_finish<T>(
    phase: Result<
        (Result<T, EngineError>, Option<sqlx::PgConnection>),
        tokio::time::error::Elapsed,
    >,
    query_timeout_ms: u64,
) -> Result<T, EngineError> {
    match phase {
        Ok((answer, conn)) => {
            if let Some(conn) = conn {
                pg_close_read_only(conn).await;
            }
            answer
        }
        Err(_elapsed) => Err(client_timeout(query_timeout_ms)),
    }
}

/// The shared WHERE tail of the four pg_catalog queries. No agent text: the
/// `[table]` argument arrives as the bound `$1`/`$2` (name / schema), and the
/// system schemas are excluded by literals. `'pg\_%'` escapes the LIKE
/// wildcard, so it means the literal prefix `pg_` (pg_catalog, pg_toast,
/// pg_temp_*) — user schemas cannot start with `pg_` (reserved).
///
/// **The privilege checks are the security half (SECURITY).** pg_catalog is
/// world-readable, so without them `nyet schema` would hand the agent every
/// table of every schema the role cannot even see — including DEFAULT
/// expressions, which are literal data (secrets get parked in defaults). With
/// them the answer matches what the role could actually SELECT, the way
/// MySQL's information_schema already filters itself. `has_any_column_privilege`,
/// not `has_table_privilege`: a `GRANT SELECT (col) ON t` makes `SELECT col
/// FROM t` work, so the table must be introspectable too (the columns query
/// then hides the columns that were not granted).
///
/// A bare `[table]` also matches its lowercase form: PostgreSQL folds
/// unquoted identifiers to lowercase, so `nyet schema pg ORGS` must find
/// `orgs` — as `SELECT * FROM ORGS` would. (If both `ORGS` and `orgs` exist,
/// both are returned; qualify or quote to pin one down.)
const PG_FILTER: &str = "n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%' \
     AND has_schema_privilege(n.oid, 'USAGE') AND has_any_column_privilege(c.oid, 'SELECT') \
     AND ($1::text IS NULL OR c.relname = $1 OR c.relname = lower($1)) \
     AND ($2::text IS NULL OR n.nspname = $2 OR n.nspname = lower($2))";

/// Ordinary + partitioned + foreign tables, views and materialized views. A
/// foreign table reads like a table (it just lives elsewhere), so a role with
/// SELECT on one must find it here. (information_schema cannot answer this: it
/// has no index catalog — hence pg_catalog throughout.)
const PG_RELKINDS: &str = "c.relkind IN ('r','p','f','v','m')";

/// `public` is on the default search_path, so its objects read as bare names;
/// everything else is schema-qualified — which is also the form the `[table]`
/// argument accepts back.
fn pg_display(schema: &str, name: &str) -> String {
    if schema == "public" {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// PostgreSQL introspection: four pg_catalog queries (objects, columns,
/// constraints, indexes) grouped back together by table. Every one of them is a
/// constant string plus the two bound filter parameters.
async fn pg_schema(
    conn: &mut sqlx::PgConnection,
    table: Option<&str>,
) -> Result<Schema, EngineError> {
    let (schema_filter, name_filter) = match table {
        // The `[table]` argument is split on the first dot by the one rule
        // `nyet sample` also applies to it. Both halves are BOUND as parameters
        // below, never interpolated; an unqualified name matches in every
        // non-system schema.
        Some(t) => {
            let (schema, name) = super::sample::split_qualified(t);
            (schema, Some(name))
        }
        None => (None, None),
    };
    // AssertSqlSafe: the SQL below is entirely nyet's own constant text (the
    // agent's argument travels as bind parameters), it is dynamic only because
    // the shared WHERE tail is composed with format!.
    let objects = sqlx::query(sqlx::AssertSqlSafe(format!(
        // full_sel: table-wide SELECT, so the columns query cannot have held
        // anything back. Without it the role got in through a column-level
        // GRANT and every key over an invisible column must be dropped.
        "SELECT n.nspname::text AS schema, c.relname::text AS name, c.relkind::text AS kind, \
         has_table_privilege(c.oid, 'SELECT') AS full_sel \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE {PG_RELKINDS} AND {PG_FILTER} ORDER BY 1, 2"
    )))
    .bind(name_filter)
    .bind(schema_filter)
    .fetch_all(&mut *conn)
    .await
    .map_err(pg_error)?;

    // Keyed by (schema, name), NOT by display name: `public."a.b"` and table
    // `b` in schema `a` share a display name and would otherwise merge into one
    // object. The display name is applied only on the way out (pg_objects).
    let mut parts: BTreeMap<(String, String), TableParts> = BTreeMap::new();
    for row in &objects {
        let kind: String = row.try_get("kind").map_err(pg_error)?;
        let kind = if kind == "v" || kind == "m" {
            "view"
        } else {
            "table"
        };
        let full_sel: bool = row.try_get("full_sel").unwrap_or(false);
        parts.insert(pg_key(row)?, TableParts::new(kind, full_sel));
    }
    if over_detail_limit(table, parts.len()) {
        return Ok(listing(pg_objects(parts)));
    }

    let columns = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT n.nspname::text AS schema, c.relname::text AS name, a.attname::text AS column, \
         format_type(a.atttypid, a.atttypmod) AS type, a.attnotnull AS notnull, \
         COALESCE(pg_get_expr(d.adbin, d.adrelid), \
                  CASE WHEN a.attidentity <> '' THEN 'generated as identity' END) AS \"default\" \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE a.attnum > 0 AND NOT a.attisdropped \
         AND has_column_privilege(c.oid, a.attnum, 'SELECT') \
         AND {PG_RELKINDS} AND {PG_FILTER} \
         ORDER BY 1, 2, a.attnum"
    )))
    .bind(name_filter)
    .bind(schema_filter)
    .fetch_all(&mut *conn)
    .await
    .map_err(pg_error)?;
    for row in &columns {
        let Some(entry) = parts.get_mut(&pg_key(row)?) else {
            continue;
        };
        entry.columns.push(SchemaColumn {
            name: row.try_get("column").map_err(pg_error)?,
            ty: row.try_get("type").map_err(pg_error)?,
            nullable: !row.try_get::<bool, _>("notnull").map_err(pg_error)?,
            pk: false,
            unique: false,
            default: row.try_get("default").map_err(pg_error)?,
            pii: None,
            source: None,
            seen: None,
        });
    }

    // Primary keys and foreign keys in one pass over pg_constraint; the column
    // names come back as ordered arrays (conkey/confkey are attnum vectors).
    let constraints = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT n.nspname::text AS schema, c.relname::text AS name, con.contype::text AS contype, \
         (SELECT array_agg(att.attname::text ORDER BY u.ord) \
            FROM unnest(con.conkey) WITH ORDINALITY AS u(attnum, ord) \
            JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = u.attnum) AS cols, \
         fns.nspname::text AS ref_schema, ft.relname::text AS ref_table, \
         (SELECT array_agg(att.attname::text ORDER BY u.ord) \
            FROM unnest(con.confkey) WITH ORDINALITY AS u(attnum, ord) \
            JOIN pg_attribute att ON att.attrelid = con.confrelid AND att.attnum = u.attnum) AS ref_cols \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_class ft ON ft.oid = con.confrelid \
         LEFT JOIN pg_namespace fns ON fns.oid = ft.relnamespace \
         WHERE con.contype IN ('p','f') AND {PG_FILTER} ORDER BY 1, 2, con.conname"
    )))
    .bind(name_filter)
    .bind(schema_filter)
    .fetch_all(&mut *conn)
    .await
    .map_err(pg_error)?;
    for row in &constraints {
        let Some(entry) = parts.get_mut(&pg_key(row)?) else {
            continue;
        };
        let contype: String = row.try_get("contype").map_err(pg_error)?;
        let cols: Vec<String> = row
            .try_get::<Option<Vec<String>>, _>("cols")
            .ok()
            .flatten()
            .unwrap_or_default();
        if contype == "p" {
            entry.pk = cols;
            continue;
        }
        let (Ok(ref_schema), Ok(ref_table)) = (
            row.try_get::<String, _>("ref_schema"),
            row.try_get::<String, _>("ref_table"),
        ) else {
            continue;
        };
        entry.fks.push(SchemaFk {
            columns: cols,
            ref_table: pg_display(&ref_schema, &ref_table),
            ref_columns: row
                .try_get::<Option<Vec<String>>, _>("ref_cols")
                .ok()
                .flatten()
                .unwrap_or_default(),
        });
    }

    // Indexes, one row per key column, for real tables only (a materialized
    // view is reported as a view, and a view never carries indexes on any
    // engine). The PRIMARY KEY index is skipped (the pk flags carry it); a
    // unique index over a single column is folded into that column's `unique`
    // flag by build_table. Expression keys have attnum 0, so pg_get_indexdef
    // supplies their text. `unique` is claimed only for a valid, unconditional
    // index: a partial one (indpred) holds for its predicate rows only, and an
    // invalid one (a failed CREATE INDEX CONCURRENTLY) enforces nothing.
    let indexes = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT n.nspname::text AS schema, c.relname::text AS name, i.relname::text AS idx, \
         (ix.indisunique AND ix.indpred IS NULL AND ix.indisvalid) AS is_unique, \
         a.attname::text AS col, \
         pg_get_indexdef(ix.indexrelid, k.ord::int, true) AS expr \
         FROM pg_index ix \
         JOIN pg_class c ON c.oid = ix.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         CROSS JOIN LATERAL unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) \
         LEFT JOIN pg_attribute a ON a.attrelid = ix.indrelid AND a.attnum = k.attnum \
         WHERE NOT ix.indisprimary AND k.ord <= ix.indnkeyatts \
         AND c.relkind IN ('r','p') AND {PG_FILTER} \
         ORDER BY 1, 2, 3, k.ord"
    )))
    .bind(name_filter)
    .bind(schema_filter)
    .fetch_all(&mut *conn)
    .await
    .map_err(pg_error)?;
    for row in &indexes {
        let Some(entry) = parts.get_mut(&pg_key(row)?) else {
            continue;
        };
        let index_name: String = row.try_get("idx").map_err(pg_error)?;
        // attnum 0 -> no column, an expression: pg_get_indexdef spells it out.
        let part = match row.try_get::<Option<String>, _>("col").map_err(pg_error)? {
            Some(name) => KeyPart::Named(name),
            None => KeyPart::Expression(Some(row.try_get("expr").map_err(pg_error)?)),
        };
        let unique: bool = row.try_get("is_unique").map_err(pg_error)?;
        push_index_column(&mut entry.indexes, index_name, part, unique);
    }

    Ok(assemble(pg_objects(parts)))
}

/// The catalog grouping key of a row: (schema, name).
fn pg_key(row: &PgRow) -> Result<(String, String), EngineError> {
    Ok((
        row.try_get("schema").map_err(pg_error)?,
        row.try_get("name").map_err(pg_error)?,
    ))
}

/// Grouped parts -> the display names the contract shows.
fn pg_objects(parts: BTreeMap<(String, String), TableParts>) -> Vec<(String, TableParts)> {
    parts
        .into_iter()
        .map(|((schema, name), p)| (pg_display(&schema, &name), p))
        .collect()
}

/// Catalog rows arrive one key column at a time, ordered by index name — so a
/// row either extends the index being built or starts a new one. Shared by the
/// Postgres and MySQL introspection (both group the same way).
fn push_index_column(indexes: &mut Vec<SchemaIndex>, name: String, part: KeyPart, unique: bool) {
    match indexes.last_mut() {
        Some(last) if last.name == name => last.columns.push(part),
        _ => indexes.push(SchemaIndex {
            name,
            columns: vec![part],
            unique,
        }),
    }
}

fn decode_pg_row(row: &PgRow) -> Result<Vec<Value>, EngineError> {
    (0..row.len()).map(|i| decode_pg_column(row, i)).collect()
}

/// Decode one PostgreSQL cell into JSON by its wire type. Types real tables
/// are full of are handled explicitly; anything else falls back to a text
/// decode and, failing that, a clear DB_ERROR (never a panic — D3).
///
/// Representation choices (DEV.md): numeric -> string (exact, no f64
/// rounding), timestamp/date/time -> ISO-ish string, uuid -> string,
/// json/jsonb -> structured JSON as-is, bytea -> lowercase hex (as SQLite
/// BLOB), NULL -> null.
fn decode_pg_column(row: &PgRow, i: usize) -> Result<Value, EngineError> {
    let raw = row.try_get_raw(i).map_err(pg_error)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let ty = raw.type_info().name().to_string();
    use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
    use sqlx::types::{BigDecimal, Uuid};
    // None => no typed decoder for this type (text family / exotic). Some(Err)
    // => a typed decoder that could not represent the value ('NaN'::numeric has
    // no BigDecimal, 'infinity'::timestamptz has no chrono value). Both go to
    // the text fallback: a text decode recovers text-compatible types, and
    // anything else gets a "::text-cast" DB_ERROR — never pg_error's misleading
    // "check the schema" hint (the schema is fine; the value just isn't JSON-able).
    let typed: Option<Result<Value, sqlx::Error>> = match ty.as_str() {
        "BOOL" => Some(row.try_get::<bool, _>(i).map(Value::from)),
        "INT2" => Some(row.try_get::<i16, _>(i).map(|v| Value::from(v as i64))),
        "INT4" => Some(row.try_get::<i32, _>(i).map(|v| Value::from(v as i64))),
        "INT8" => Some(row.try_get::<i64, _>(i).map(Value::from)),
        "FLOAT4" => Some(row.try_get::<f32, _>(i).map(|v| number_or_string(v as f64))),
        "FLOAT8" => Some(row.try_get::<f64, _>(i).map(number_or_string)),
        "NUMERIC" => Some(
            row.try_get::<BigDecimal, _>(i)
                .map(|d| Value::String(d.to_string())),
        ),
        "UUID" => Some(
            row.try_get::<Uuid, _>(i)
                .map(|u| Value::String(u.to_string())),
        ),
        "JSON" | "JSONB" => Some(row.try_get::<Value, _>(i)),
        "TIMESTAMP" => Some(
            row.try_get::<NaiveDateTime, _>(i)
                .map(|t| Value::String(t.to_string())),
        ),
        "TIMESTAMPTZ" => Some(
            row.try_get::<DateTime<Utc>, _>(i)
                .map(|t| Value::String(t.to_rfc3339())),
        ),
        "DATE" => Some(
            row.try_get::<NaiveDate, _>(i)
                .map(|d| Value::String(d.to_string())),
        ),
        "TIME" => Some(
            row.try_get::<NaiveTime, _>(i)
                .map(|t| Value::String(t.to_string())),
        ),
        // ponytail: bytea -> hex string; dedicated binary handling can land if
        // agents actually query blobs. Same convention as the SQLite engine.
        "BYTEA" => Some(row.try_get::<Vec<u8>, _>(i).map(|b| Value::String(hex(&b)))),
        // Text family (TEXT/VARCHAR/CHAR/NAME/UNKNOWN) and everything exotic
        // (arrays, inet, ranges, ...): straight to the text fallback. ponytail:
        // arrays and exotic types come back only when text-decodable; otherwise
        // the query gets a ::text-cast DB_ERROR — add per-type arms if agents
        // need them structured.
        _ => None,
    };
    match typed {
        Some(Ok(v)) => Ok(v),
        _ => decode_pg_text_fallback(row, i, &ty),
    }
}

fn decode_pg_text_fallback(row: &PgRow, i: usize, ty: &str) -> Result<Value, EngineError> {
    match row.try_get::<String, _>(i) {
        Ok(s) => Ok(Value::String(s)),
        Err(_) => Err(EngineError::Db {
            message: format!("nyet cannot serialize a value of PostgreSQL type {ty} to JSON"),
            hint: "cast the column to text in the query (e.g. col::text) and retry".to_string(),
        }),
    }
}

/// JSON has no NaN/Infinity; fall back to a string for non-finite floats.
fn number_or_string(x: f64) -> Value {
    serde_json::Number::from_f64(x)
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(x.to_string()))
}

/// Connection/auth failures -> CONNECTION_FAILED (exit 6). The driver's
/// message names the failing user on auth errors but never the password. A TLS
/// handshake/cert failure gets a TLS-specific hint (pointing at sslmode and the
/// server certificate) instead of the misleading "check host/creds" one.
fn pg_connect_error(e: sqlx::Error) -> EngineError {
    EngineError::Connect {
        message: format!(
            "cannot connect to the PostgreSQL database: {}",
            error_text(&e)
        ),
        hint: if is_tls_error(&e) {
            tls_hint()
        } else {
            "check the host/port in `url` and the credentials; set `password` on this \
             connection to where the password lives"
                .to_string()
        },
    }
}

/// True when a DIRECT server connection's transport is NOT guaranteed encrypted
/// and verified: the url's `sslmode`/`ssl-mode` is below `require`/`REQUIRED`
/// (absent -> the sqlx default `prefer`/`preferred`, which uses TLS only if the
/// server offers it and otherwise silently falls back to plaintext). Static —
/// it parses the url only, no server round-trip — so over-warning against a
/// server that happens to negotiate TLS is accepted: we report the *guarantee*,
/// not the runtime outcome. SQLite and unparsable urls -> false (the cli gates
/// this on a server engine, and a bad url fails later at connect anyway).
pub fn transport_below_require(engine: &str, url: &str) -> bool {
    match engine {
        "postgres" => url.parse::<PgConnectOptions>().is_ok_and(|o| {
            matches!(
                o.get_ssl_mode(),
                PgSslMode::Disable | PgSslMode::Allow | PgSslMode::Prefer
            )
        }),
        "mysql" | "mariadb" => url.parse::<MySqlConnectOptions>().is_ok_and(|o| {
            matches!(
                o.get_ssl_mode(),
                MySqlSslMode::Disabled | MySqlSslMode::Preferred
            )
        }),
        // Read off the url text rather than through the driver: parsing a
        // MongoDB url is async (SRV) and this runs before any runtime exists
        // (D9). `tls`/`ssl` are synonyms; `mongodb+srv://` turns TLS on by
        // default, which an explicit `false` can still switch off. Erring
        // toward "insecure" is the safe direction for a warning.
        "mongodb" => {
            let url = url.to_ascii_lowercase();
            if url.contains("tls=false") || url.contains("ssl=false") {
                return true;
            }
            !(url.starts_with("mongodb+srv://")
                || url.contains("tls=true")
                || url.contains("ssl=true"))
        }
        // ClickHouse's transport is decided by the SCHEME, not by a parameter:
        // `http://` is plaintext and `https://` is TLS with the webpki roots.
        // There is no `prefer`-shaped middle to get wrong, so the check is the
        // scheme itself — and an unparsable url errs toward "insecure", which
        // is the safe direction for a warning.
        "clickhouse" => !url.to_ascii_lowercase().starts_with("https://"),
        // Redis decides TLS by scheme too: `rediss://` (two s) is TLS,
        // `redis://` is plaintext. An unparsable url errs toward "insecure".
        "redis" | "valkey" => !url.to_ascii_lowercase().starts_with("rediss://"),
        _ => false,
    }
}

/// A TLS handshake / certificate-verification failure (`sqlx::Error::Tls`).
/// Its Display describes the cert/handshake problem and never carries the url
/// or password, so it is safe to surface via `error_text`.
fn is_tls_error(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Tls(_))
}

/// Shared hint for a TLS failure. Reachable from the TUNNEL leg too since an
/// explicit `require`+ survives it (`apply_host_override`), which is why the
/// text also names dropping the mode: over ssh the hop is already encrypted, so
/// that is a real fix there rather than a downgrade of the only protection.
/// Neutral by design otherwise: `sqlx::Error::Tls` covers BOTH "the server does
/// not support TLS" (so require/verify-* cannot be satisfied) and "the
/// certificate failed verification" — without reliably distinguishing them, so
/// the hint names both causes rather than always advising to relax the mode.
fn tls_hint() -> String {
    "TLS could not be established: the server may not support TLS, or its certificate \
     failed verification — check the server's TLS config and the sslmode/ssl-mode in `url` \
     (for a private CA point sslrootcert=/ssl-ca= at its certificate; over an ssh tunnel \
     the hop is already encrypted, so dropping sslmode/ssl-mode from the url is also an option)"
        .to_string()
}

/// Query-time errors. The server's own statement_timeout (57014) maps to
/// TIMEOUT (exit 8), matching the cli's tokio timeout, so the exit code is
/// deterministic regardless of which fires. 57014 is query_canceled generally,
/// so a MANUAL cancel from another session (pg_cancel_backend) also lands here
/// as TIMEOUT — expected case is our statement_timeout, the manual cancel is
/// rare and TIMEOUT is a reasonable classification for it. Everything else is
/// DB_ERROR.
fn pg_error(e: sqlx::Error) -> EngineError {
    if let sqlx::Error::Database(db) = &e {
        if db.code().as_deref() == Some("57014") {
            return EngineError::Timeout {
                message: "the query exceeded the timeout and was cancelled by the server"
                    .to_string(),
                hint: "narrow the query (WHERE / LIMIT), or raise --timeout or timeout_secs \
                       in the config"
                    .to_string(),
            };
        }
    }
    EngineError::Db {
        message: format!("the database returned an error: {}", error_text(&e)),
        hint: "check the query against the actual schema, e.g. SELECT table_name FROM \
               information_schema.tables WHERE table_schema = 'public'"
            .to_string(),
    }
}

/// MySQL/MariaDB via sqlx. Layer 2 (DESIGN §3): each read runs inside an
/// explicit `START TRANSACTION READ ONLY`, so a write that slipped past the
/// validator is refused by the database (ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION),
/// and a server-side statement timeout cancels a runaway query.
///
/// The server timeout variable differs by flavor and the two are mutually
/// exclusive (each server rejects the other's name with ER_UNKNOWN_SYSTEM_VARIABLE
/// 1193): MySQL uses `max_execution_time` (milliseconds, SELECT-only), MariaDB
/// uses `max_statement_time` (seconds). We set BOTH and swallow the wrong-flavor
/// 1193 on each, so the real server always gets a server-side cap regardless of
/// the config `engine` label — the tokio timeout only bounds the client, it does
/// not stop a runaway server scan. Both timeout SQLSTATEs (3024 / 1969) map to
/// EngineError::Timeout so the exit code is deterministic (like Postgres 57014).
pub struct Mysql {
    /// The `url` from the config (no password embedded by convention).
    pub url: String,
    /// Resolved from the connection's `password` by the cli; never logged/printed.
    pub password: Option<String>,
    /// The per-query wall budget in ms (MySQL `max_execution_time`; the MariaDB
    /// `max_statement_time` is the same budget in seconds).
    pub statement_timeout_ms: u64,
    /// The effective per-query wall budget in ms: the in-process deadline that
    /// wraps the query phase (AFTER connect), backstopping the server-side
    /// max_execution_time/max_statement_time so a runaway query is TIMEOUT
    /// (exit 8) whichever fires.
    pub query_timeout_ms: u64,
    /// When an SSH tunnel is up, `(127.0.0.1, local_port)` to connect through.
    pub host_override: Option<(String, u16)>,
    /// Test-only override for the connect handshake deadline (ms); see the same
    /// field on `Postgres`. Production passes `None`.
    pub connect_timeout_ms: Option<u64>,
    /// The config's `engine = "mariadb"` label — a HINT only: it decides which
    /// of the two mutually exclusive timeout variables is tried FIRST, never
    /// which one is honored. A mislabelled server is still capped, it just pays
    /// one extra round trip once per connection (see `TimeoutVar`).
    pub mariadb: bool,
}

/// Which server-side statement-timeout variable THIS server accepts. The two
/// spellings are mutually exclusive — MySQL has `max_execution_time`
/// (milliseconds), MariaDB `max_statement_time` (seconds), and each answers
/// ER_UNKNOWN_SYSTEM_VARIABLE (1193) for the other's name (verified live on
/// mysql:8.4 and mariadb:11.4).
///
/// The engine used to send BOTH on every call, so half of the six timeout round
/// trips a guarded query paid were known-doomed. Now the flavor is learned ONCE
/// per connection, from the very 1193 that used to be paid over and over: the
/// config label picks which name to try first, so a correctly labelled server
/// never sees a 1193 at all, and a mislabelled one is still capped by the
/// fallback — the label cannot silently disable the server cap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TimeoutVar {
    /// Not asked yet: try the label's name first, then the other.
    Unknown,
    /// MySQL: milliseconds.
    MaxExecutionTime,
    /// MariaDB: seconds.
    MaxStatementTime,
    /// Neither name exists here (an exotic proxy). No server cap is possible, so
    /// nothing more is tried on this connection — the cli's own deadline is the
    /// backstop, exactly as it was when both SETs were swallowed.
    Neither,
}

impl TimeoutVar {
    /// The names still worth trying, in order.
    fn candidates(self, mariadb: bool) -> &'static [TimeoutVar] {
        const MYSQL_FIRST: &[TimeoutVar] =
            &[TimeoutVar::MaxExecutionTime, TimeoutVar::MaxStatementTime];
        const MARIA_FIRST: &[TimeoutVar] =
            &[TimeoutVar::MaxStatementTime, TimeoutVar::MaxExecutionTime];
        match self {
            TimeoutVar::Unknown if mariadb => MARIA_FIRST,
            TimeoutVar::Unknown => MYSQL_FIRST,
            TimeoutVar::MaxExecutionTime => &MYSQL_FIRST[..1],
            TimeoutVar::MaxStatementTime => &MARIA_FIRST[..1],
            TimeoutVar::Neither => &[],
        }
    }

    /// The SET statement for this name, or `None` for the two non-names.
    fn set(self, timeout_ms: u64) -> Option<String> {
        match self {
            // (>= 1; 0 = "no limit".)
            TimeoutVar::MaxExecutionTime => Some(format!(
                "SET SESSION max_execution_time = {}",
                timeout_ms.max(1)
            )),
            // MariaDB counts SECONDS, so round UP: rounding down turned a 1600ms
            // guardrail budget into a 1s server cap and refused queries whose
            // planning was inside the budget.
            TimeoutVar::MaxStatementTime => Some(format!(
                "SET SESSION max_statement_time = {}",
                timeout_ms.div_ceil(1000).max(1)
            )),
            TimeoutVar::Unknown | TimeoutVar::Neither => None,
        }
    }
}

/// Redirect host+port to the tunnel's local end while keeping user/db/params
/// from the url, and rewrite `ssl-mode` for the tunnel leg exactly like the
/// Postgres twin: `PREFERRED` (sqlx's default) and `DISABLED` stay disabled (the
/// ssh hop already encrypts); `VERIFY_IDENTITY` drops to `VERIFY_CA` because it
/// is the only mode that checks the hostname (`accept_invalid_hostnames =
/// !VerifyIdentity`) and the cert names the real host, not 127.0.0.1; `VERIFY_CA`
/// is kept as is (chain authentication survives the tunnel); `REQUIRED` stays,
/// so a server that refuses plaintext is still reachable. The DIRECT path
/// (`None`) is left untouched, so the `ssl-mode` from the url is honored by
/// sqlx's rustls backend. Pure — unit-tested.
fn apply_mysql_host_override(
    opts: MySqlConnectOptions,
    host_override: &Option<(String, u16)>,
) -> MySqlConnectOptions {
    match host_override {
        Some((host, port)) => {
            let mode = match opts.get_ssl_mode() {
                MySqlSslMode::Disabled | MySqlSslMode::Preferred => MySqlSslMode::Disabled,
                MySqlSslMode::VerifyCa | MySqlSslMode::VerifyIdentity => MySqlSslMode::VerifyCa,
                _ => MySqlSslMode::Required,
            };
            opts.host(host).port(*port).ssl_mode(mode)
        }
        None => opts,
    }
}

impl Mysql {
    /// The `Postgres::clamp_statement_timeout` twin: `max_execution_time` is an
    /// unsigned 32-bit millisecond value (`max_statement_time` is its MariaDB
    /// seconds-typed sibling), and a value outside that range is a hard server
    /// error on the SET, not a capped query.
    pub fn clamp_statement_timeout(ms: u64) -> u64 {
        ms.min(u32::MAX as u64)
    }

    /// Connect options (password + tunnel override) and the handshake under its
    /// own generous deadline. Shared by `execute` and `schema`.
    async fn connect(&self) -> Result<sqlx::MySqlConnection, EngineError> {
        // Never echo the url on a parse error: it may embed credentials.
        let opts: MySqlConnectOptions = self.url.parse().map_err(|_| EngineError::Connect {
            message: "the `url` for this connection is not a valid MySQL URL".to_string(),
            hint: "use the form mysql://user@host:port/dbname; put the password in the \
                   this connection's `password`, not in the url"
                .to_string(),
        })?;
        let opts = match &self.password {
            Some(pw) => opts.password(pw),
            None => opts,
        };
        let opts = apply_mysql_host_override(opts, &self.host_override);
        // Bound connect on its own generous deadline so a hung TCP handshake is
        // CONNECTION_FAILED (exit 6) instead of hanging — same as Postgres (see
        // connect_deadline; it is NOT the query timeout).
        let deadline = self
            .connect_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| connect_deadline(self.statement_timeout_ms));
        match tokio::time::timeout(deadline, opts.connect()).await {
            Ok(r) => r.map_err(mysql_connect_error),
            Err(_elapsed) => Err(EngineError::Connect {
                message: "the connection to the MySQL database did not complete in time"
                    .to_string(),
                hint: "check the host/port in `url` and that the server is reachable \
                       (a firewall may be dropping the connection)"
                    .to_string(),
            }),
        }
    }

    /// Layer 2 for MySQL/MariaDB: an explicit read-only transaction plus the
    /// server-side statement timeout, in ONE round trip (sqlx negotiates
    /// CLIENT_MULTI_STATEMENTS, and a `;`-separated COM_QUERY runs in order,
    /// stopping at the first error). Shared by `execute` and `schema`.
    ///
    /// The transaction comes FIRST on purpose: it is the layer that must exist
    /// before anything else can run, and the cap that follows it still lands
    /// before the first agent statement. If the flavor guess is wrong the SET
    /// costs one extra round trip (see `set_statement_timeout`) — the read-only
    /// transaction is already open by then and is never re-sent.
    async fn begin_read_only(
        &self,
        conn: &mut sqlx::MySqlConnection,
        var: &mut TimeoutVar,
    ) -> Result<(), EngineError> {
        self.set_statement_timeout(
            conn,
            Some("START TRANSACTION READ ONLY"),
            self.statement_timeout_ms,
            var,
        )
        .await
    }

    /// The server-side statement cap, in the ONE spelling this server accepts.
    /// Also used to lend the guardrail's EXPLAIN a shorter cap of its own and to
    /// give it back afterwards.
    ///
    /// `lead` is an optional constant statement sent in the same round trip,
    /// ahead of the SET (`begin_read_only`'s transaction). It runs at most once:
    /// a retry with the other flavor's name must not repeat it.
    ///
    /// Only the flavor mismatch (1193) is swallowed; any other error is the
    /// caller's, which owns and closes the connection.
    async fn set_statement_timeout(
        &self,
        conn: &mut sqlx::MySqlConnection,
        lead: Option<&str>,
        timeout_ms: u64,
        var: &mut TimeoutVar,
    ) -> Result<(), EngineError> {
        use sqlx::Executor;
        let mut lead = lead;
        for candidate in var.candidates(self.mariadb) {
            let Some(set) = candidate.set(timeout_ms) else {
                continue;
            };
            let stmt = match lead {
                Some(first) => format!("{first}; {set}"),
                None => set,
            };
            match conn.execute(sqlx::AssertSqlSafe(stmt)).await {
                Ok(_) => {
                    *var = *candidate;
                    return Ok(());
                }
                // Wrong flavor: this name does not exist here. The lead ran
                // before it (the server stops AT the failing statement, not
                // before it), so the retry must not send it again.
                Err(e) if is_unknown_var(&e) => lead = None,
                Err(e) => return Err(mysql_error(e)),
            }
        }
        *var = TimeoutVar::Neither;
        // Neither name exists (or was already known not to): no server cap is
        // possible here, but the lead still MUST run — it is layer 2, not part
        // of the cap. Unreachable today (every phase starts from a fresh
        // `Unknown`, so the loop always sends the lead with its first attempt);
        // it is here so that reusing a learned flavor across phases can never
        // silently drop the read-only transaction.
        if let Some(first) = lead {
            conn.execute(sqlx::AssertSqlSafe(first.to_string()))
                .await
                .map_err(mysql_error)?;
        }
        Ok(())
    }

    /// The guardrail's EXPLAIN with the SERVER capped at the budget too. Without
    /// that cap the abandoned EXPLAIN keeps running and its late error surfaces
    /// as the failure of the NEXT statement (observed: a 3024 from a dropped
    /// EXPLAIN reported against the query that followed). MariaDB's
    /// `max_statement_time` has second granularity, so the server cap is the
    /// budget rounded UP to a second and the tokio deadline stays the finer of
    /// the two.
    async fn guarded_plan(
        &self,
        conn: &mut sqlx::MySqlConnection,
        sql: &str,
        query_timeout_ms: u64,
        var: &mut TimeoutVar,
    ) -> Plan {
        // One budget, computed once: the deadline below is derived from the
        // value the SERVER was actually given (see `explain_deadline_ms`).
        let budget_ms = explain_budget_ms(query_timeout_ms);
        if let Err(e) = self.set_statement_timeout(conn, None, budget_ms, var).await {
            return Plan::Broken(e);
        }
        let plan = budgeted_plan(explain_deadline_ms(budget_ms), mysql_plan(conn, sql)).await;
        if matches!(plan, Plan::TooSlow) {
            // TERMINAL, and the connection may still be busy with the planning
            // we abandoned: no restore, no politeness (see the Postgres twin).
            return Plan::TooSlow;
        }
        // Restoring the query's own cap is part of the plumbing: if it fails the
        // query would run under the EXPLAIN's short budget, so fail closed.
        match self
            .set_statement_timeout(conn, None, self.statement_timeout_ms, var)
            .await
        {
            Ok(()) => plan,
            Err(e) => Plan::Broken(e),
        }
    }
}

/// Read-only: rollback (nothing to persist) and close gracefully — a proper
/// COM_QUIT rather than a dropped socket. Best effort, like the Postgres twin,
/// bounded and run outside the query deadline for exactly the same two reasons
/// (see `pg_close_read_only` and `mysql_finish`).
async fn mysql_close_read_only(conn: sqlx::MySqlConnection) {
    let _ = tokio::time::timeout(CLOSE_GRACE, async move {
        use sqlx::Executor;
        let mut conn = conn;
        let _ = conn.execute("ROLLBACK").await;
        let _ = conn.close().await;
    })
    .await;
}

/// The MySQL twin of `pg_finish`: unwrap the bounded query phase, then close its
/// connection once the deadline can no longer take the answer away.
async fn mysql_finish<T>(
    phase: Result<
        (Result<T, EngineError>, Option<sqlx::MySqlConnection>),
        tokio::time::error::Elapsed,
    >,
    query_timeout_ms: u64,
) -> Result<T, EngineError> {
    match phase {
        Ok((answer, conn)) => {
            if let Some(conn) = conn {
                mysql_close_read_only(conn).await;
            }
            answer
        }
        Err(_elapsed) => Err(client_timeout(query_timeout_ms)),
    }
}

impl Engine for Mysql {
    async fn execute(
        &self,
        sql: &str,
        fetch_limit: u64,
        guardrail: &Guardrail,
    ) -> Result<QueryOutcome, EngineError> {
        let mut conn = self.connect().await?;

        // Bound the QUERY phase (everything below, after a successful connect)
        // on the effective per-query budget; connect above has its own generous
        // deadline. Split timers => a slow/hung connect is CONNECTION_FAILED
        // (exit 6) and only a slow QUERY is TIMEOUT (exit 8), deterministic
        // regardless of --timeout size. Backstops the server-side
        // max_execution_time/max_statement_time; whichever fires, exit 8.
        let deadline = Duration::from_millis(self.query_timeout_ms);
        let phase = tokio::time::timeout(deadline, async move {
            // Which timeout variable this server accepts, learned once and
            // reused by every SET of this connection (see `TimeoutVar`).
            let mut var = TimeoutVar::Unknown;
            if let Err(e) = self.begin_read_only(&mut conn, &mut var).await {
                return (Err(e), Some(conn));
            }

            // Guardrail: same read-only session, before the real query runs
            // (see the Postgres twin).
            let budget_ms = explain_budget_ms(self.query_timeout_ms);
            let estimate = match guardrail.plans() {
                false => None,
                true => {
                    let plan = self
                        .guarded_plan(&mut conn, sql, self.query_timeout_ms, &mut var)
                        .await;
                    // The same single decision as the Postgres twin.
                    if plan.discard() {
                        drop(conn);
                        return (
                            match plan {
                                Plan::Broken(e) => Err(e),
                                _ => Ok(QueryOutcome::PlanTooSlow { budget_ms }),
                            },
                            None,
                        );
                    }
                    match plan {
                        Plan::Got(estimate) => Some(estimate),
                        _ => None,
                    }
                }
            };
            if let Some(value) = estimate.as_ref().and_then(|e| guardrail.refuses(e)) {
                return (
                    Ok(QueryOutcome::Refused {
                        estimate: estimate.expect("the refused estimate"),
                        value,
                    }),
                    Some(conn),
                );
            }

            // ponytail: the fetch loop / empty-columns-via-prepare / rollback+close
            // tail below is structurally the same as Postgres's (only the
            // row-decode and error-map fns differ). Extract a shared `stream_rows`
            // helper if a third server engine lands — two copies isn't worth a
            // generic yet.
            let mut columns: Vec<String> = Vec::new();
            let mut origins: Vec<Origin> = Vec::new();
            let mut rows: Vec<Vec<Value>> = Vec::new();
            let fetched = {
                let mut stream = sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).fetch(&mut conn);
                loop {
                    if (rows.len() as u64) >= fetch_limit {
                        break Ok(());
                    }
                    match stream.try_next().await {
                        Ok(Some(row)) => {
                            if columns.is_empty() {
                                columns =
                                    row.columns().iter().map(|c| c.name().to_string()).collect();
                                origins = origins_of(row.columns());
                            }
                            match decode_mysql_row(&row) {
                                Ok(r) => rows.push(r),
                                Err(e) => break Err(e),
                            }
                        }
                        Ok(None) => break Ok(()),
                        Err(e) => break Err(mysql_error(e)),
                    }
                }
            };
            // Empty result -> ask the prepared statement for columns (best effort).
            if fetched.is_ok() && rows.is_empty() && columns.is_empty() {
                use sqlx::{Executor, SqlSafeStr, Statement};
                let sql_str = sqlx::AssertSqlSafe(sql.to_string()).into_sql_str();
                if let Ok(statement) = conn.prepare(sql_str).await {
                    columns = statement
                        .columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect();
                    origins = origins_of(statement.columns());
                }
            }
            let answer = fetched.map(|()| QueryOutcome::Ran {
                result: ResultSet {
                    columns,
                    rows,
                    origins,
                    truncated: false,
                },
                estimate,
            });
            (answer, Some(conn))
        })
        .await;
        mysql_finish(phase, self.query_timeout_ms).await
    }

    async fn estimate(&self, sql: &str) -> Result<Option<CostEstimate>, EngineError> {
        let mut conn = self.connect().await?;
        let deadline = Duration::from_millis(self.query_timeout_ms);
        let phase = tokio::time::timeout(deadline, async move {
            let mut var = TimeoutVar::Unknown;
            if let Err(e) = self.begin_read_only(&mut conn, &mut var).await {
                return (Err(e), Some(conn));
            }
            // The guarded path, exactly as `query` runs it (see the pg twin).
            let plan = self
                .guarded_plan(&mut conn, sql, self.query_timeout_ms, &mut var)
                .await;
            // A connection the guardrail could not leave clean is dropped.
            let keep = match plan.discard() {
                true => {
                    drop(conn);
                    None
                }
                false => Some(conn),
            };
            (plan.into_answer(), keep)
        })
        .await;
        mysql_finish(phase, self.query_timeout_ms).await
    }

    async fn schema(&self, table: Option<&str>) -> Result<Schema, EngineError> {
        let mut conn = self.connect().await?;
        let deadline = Duration::from_millis(self.query_timeout_ms);
        let phase = tokio::time::timeout(deadline, async move {
            let mut var = TimeoutVar::Unknown;
            if let Err(e) = self.begin_read_only(&mut conn, &mut var).await {
                return (Err(e), Some(conn));
            }
            let schema = mysql_schema(&mut conn, table).await;
            (schema, Some(conn))
        })
        .await;
        mysql_finish(phase, self.query_timeout_ms).await
    }

}

async fn mysql_plan(
    conn: &mut sqlx::MySqlConnection,
    sql: &str,
) -> Result<CostEstimate, EngineError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
        .fetch_all(&mut *conn)
        .await
        .map_err(mysql_error)?;
    let (columns, values) = plan_table(&rows, decode_mysql_row)?;
    Ok(super::guardrail::mysql_estimate(&columns, &values))
}

/// MySQL/MariaDB introspection: four information_schema queries scoped to the
/// connection's own database (`DATABASE()`), grouped back together by table.
/// The `[table]` argument is bound (`?`), never interpolated — it is bound
/// twice because MySQL placeholders are positional.
async fn mysql_schema(
    conn: &mut sqlx::MySqlConnection,
    table: Option<&str>,
) -> Result<Schema, EngineError> {
    let objects = sqlx::query(
        "SELECT TABLE_NAME AS name, TABLE_TYPE AS kind FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND (? IS NULL OR TABLE_NAME = ?) ORDER BY TABLE_NAME",
    )
    .bind(table)
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(mysql_error)?;
    let mut parts: BTreeMap<String, TableParts> = BTreeMap::new();
    for row in &objects {
        let name: String = row.try_get("name").map_err(mysql_error)?;
        let kind: String = row.try_get("kind").map_err(mysql_error)?;
        let kind = match kind.as_str() {
            "BASE TABLE" => "table",
            "VIEW" => "view",
            // SYSTEM VIEW / SEQUENCE / anything else: not a readable relation
            // the agent asked about.
            _ => continue,
        };
        // information_schema.COLUMNS is privilege-filtered by the server, but
        // it never says WHETHER it withheld anything — so the key filter always
        // runs. With full privileges every named part is visible and nothing is
        // dropped; the cost is only paid by a column-granted account.
        parts.insert(name, TableParts::new(kind, false));
    }
    if over_detail_limit(table, parts.len()) {
        return Ok(listing(parts.into_iter().collect()));
    }

    let columns = sqlx::query(
        "SELECT TABLE_NAME AS name, COLUMN_NAME AS col, COLUMN_TYPE AS type, \
         IS_NULLABLE AS nullable, COLUMN_DEFAULT AS def, EXTRA AS extra \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND (? IS NULL OR TABLE_NAME = ?) \
         ORDER BY TABLE_NAME, ORDINAL_POSITION",
    )
    .bind(table)
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(mysql_error)?;
    for row in &columns {
        let key: String = row.try_get("name").map_err(mysql_error)?;
        let Some(entry) = parts.get_mut(&key) else {
            continue;
        };
        let extra: String = row.try_get("extra").unwrap_or_default();
        let default: Option<String> = row.try_get("def").unwrap_or(None);
        entry.columns.push(SchemaColumn {
            name: row.try_get("col").map_err(mysql_error)?,
            ty: row.try_get("type").map_err(mysql_error)?,
            nullable: row.try_get::<String, _>("nullable").unwrap_or_default() != "NO",
            pk: false,
            unique: false,
            // MySQL reports auto-increment in EXTRA, not COLUMN_DEFAULT; surface
            // it as the default so the agent sees the column is auto-assigned.
            default: default.or_else(|| {
                extra
                    .to_lowercase()
                    .contains("auto_increment")
                    .then(|| "auto_increment".to_string())
            }),
            pii: None,
            source: None,
            seen: None,
        });
    }

    let indexes = sqlx::query(
        "SELECT TABLE_NAME AS name, INDEX_NAME AS idx, NON_UNIQUE AS non_unique, \
         COLUMN_NAME AS col FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = DATABASE() AND (? IS NULL OR TABLE_NAME = ?) \
         ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(table)
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(mysql_error)?;
    for row in &indexes {
        let key: String = row.try_get("name").map_err(mysql_error)?;
        let Some(entry) = parts.get_mut(&key) else {
            continue;
        };
        let index_name: String = row.try_get("idx").map_err(mysql_error)?;
        // NULL for a functional key part (MySQL 8 `((lower(x)))`): kept as an
        // Expression part so the key arity survives. STATISTICS.EXPRESSION
        // would hold its text, but MySQL 8 has that column and MariaDB does
        // not — no text (None) works on both.
        let part = match row.try_get::<Option<String>, _>("col") {
            Ok(Some(name)) => KeyPart::Named(name),
            _ => KeyPart::Expression(None),
        };
        // The primary key is always named PRIMARY; its index is redundant with
        // the pk column flags. (A pk part is always a real column.)
        if index_name == "PRIMARY" {
            if let KeyPart::Named(name) = part {
                entry.pk.push(name);
            }
            continue;
        }
        let unique = row.try_get::<i64, _>("non_unique").unwrap_or(1) == 0;
        push_index_column(&mut entry.indexes, index_name, part, unique);
    }

    let fks = sqlx::query(
        // A foreign key may point at another database; qualify the parent then,
        // the way PostgreSQL qualifies anything outside `public`.
        "SELECT TABLE_NAME AS name, CONSTRAINT_NAME AS con, COLUMN_NAME AS col, \
         IF(REFERENCED_TABLE_SCHEMA = DATABASE(), REFERENCED_TABLE_NAME, \
            CONCAT(REFERENCED_TABLE_SCHEMA, '.', REFERENCED_TABLE_NAME)) AS ref_table, \
         REFERENCED_COLUMN_NAME AS ref_col \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_NAME IS NOT NULL \
         AND (? IS NULL OR TABLE_NAME = ?) \
         ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
    )
    .bind(table)
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(mysql_error)?;
    // Rows are one key column each, ordered by table+constraint: a row either
    // extends the fk being built or starts a new one. Grouped in a plain Vec
    // first (like sqlite_fks), then attached — no bookkeeping across borrows.
    let mut grouped: Vec<(String, String, SchemaFk)> = Vec::new();
    for row in &fks {
        let key: String = row.try_get("name").map_err(mysql_error)?;
        let constraint: String = row.try_get("con").map_err(mysql_error)?;
        let column: String = row.try_get("col").map_err(mysql_error)?;
        let ref_table: String = row.try_get("ref_table").map_err(mysql_error)?;
        let ref_column: String = row.try_get("ref_col").map_err(mysql_error)?;
        match grouped.last_mut() {
            Some((last_table, last_con, fk)) if *last_table == key && *last_con == constraint => {
                fk.columns.push(column);
                fk.ref_columns.push(ref_column);
            }
            _ => grouped.push((
                key,
                constraint,
                SchemaFk {
                    columns: vec![column],
                    ref_table,
                    ref_columns: vec![ref_column],
                },
            )),
        }
    }
    for (key, _, fk) in grouped {
        if let Some(entry) = parts.get_mut(&key) {
            entry.fks.push(fk);
        }
    }

    Ok(assemble(parts.into_iter().collect()))
}

fn decode_mysql_row(row: &MySqlRow) -> Result<Vec<Value>, EngineError> {
    (0..row.len())
        .map(|i| decode_mysql_column(row, i))
        .collect()
}

/// Decode one MySQL/MariaDB cell into JSON by its wire type (names from
/// sqlx's `MySqlTypeInfo`). Representation (DEV.md): signed ints -> number,
/// unsigned ints & BIT -> number (BIGINT UNSIGNED as u64, may exceed i64),
/// DECIMAL -> string (exact), FLOAT/DOUBLE -> number, text/ENUM -> string,
/// DATE/DATETIME/TIMESTAMP/TIME -> string, binary/BLOB -> lowercase hex,
/// JSON -> structured JSON, NULL -> null. Anything else falls back to a text
/// decode and, failing that, a clear ::CHAR-cast DB_ERROR (never a panic, D3).
fn decode_mysql_column(row: &MySqlRow, i: usize) -> Result<Value, EngineError> {
    let raw = row.try_get_raw(i).map_err(mysql_error)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let ty = raw.type_info().name().to_string();
    use sqlx::mysql::types::MySqlTime;
    use sqlx::types::chrono::{NaiveDate, NaiveDateTime};
    use sqlx::types::BigDecimal;
    // Some(Err) (a typed decoder that could not represent the value) and None
    // (no typed arm) both fall through to the text fallback — same shape as
    // decode_pg_column.
    let typed: Option<Result<Value, sqlx::Error>> = match ty.as_str() {
        "BOOLEAN" | "TINYINT" | "SMALLINT" | "INT" | "MEDIUMINT" | "BIGINT" | "YEAR" => {
            Some(row.try_get::<i64, _>(i).map(Value::from))
        }
        // Unsigned ints and BIT are all uint-decodable; BIGINT UNSIGNED can
        // exceed i64, so decode as u64 (serde_json Number holds it exactly).
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "INT UNSIGNED" | "MEDIUMINT UNSIGNED"
        | "BIGINT UNSIGNED" | "BIT" => Some(row.try_get::<u64, _>(i).map(Value::from)),
        "FLOAT" => Some(row.try_get::<f32, _>(i).map(|v| number_or_string(v as f64))),
        "DOUBLE" => Some(row.try_get::<f64, _>(i).map(number_or_string)),
        "DECIMAL" => Some(
            row.try_get::<BigDecimal, _>(i)
                .map(|d| Value::String(d.to_string())),
        ),
        "JSON" => Some(row.try_get::<Value, _>(i)),
        "DATE" => Some(
            row.try_get::<NaiveDate, _>(i)
                .map(|d| Value::String(d.to_string())),
        ),
        "DATETIME" | "TIMESTAMP" => Some(
            row.try_get::<NaiveDateTime, _>(i)
                .map(|t| Value::String(t.to_string())),
        ),
        // MySqlTime, not chrono::NaiveTime: MySQL TIME spans -838:59:59..838:59:59
        // (a duration, can be negative / exceed 24h), which NaiveTime cannot hold
        // — decoding a normal such column as NaiveTime would DB_ERROR.
        "TIME" => Some(
            row.try_get::<MySqlTime, _>(i)
                .map(|t| Value::String(t.to_string())),
        ),
        "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" => {
            Some(row.try_get::<String, _>(i).map(Value::String))
        }
        // ponytail: binary/blob -> lowercase hex; same convention as Postgres
        // bytea and SQLite BLOB. A dedicated representation can land if needed.
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
            Some(row.try_get::<Vec<u8>, _>(i).map(|b| Value::String(hex(&b))))
        }
        // SET, GEOMETRY and anything exotic: text fallback, then a ::CHAR-cast
        // DB_ERROR if that fails too.
        _ => None,
    };
    match typed {
        Some(Ok(v)) => Ok(v),
        _ => decode_mysql_text_fallback(row, i, &ty),
    }
}

fn decode_mysql_text_fallback(row: &MySqlRow, i: usize, ty: &str) -> Result<Value, EngineError> {
    match row.try_get::<String, _>(i) {
        Ok(s) => Ok(Value::String(s)),
        Err(_) => Err(EngineError::Db {
            message: format!("nyet cannot serialize a value of MySQL type {ty} to JSON"),
            hint: "cast the column to text in the query (e.g. CAST(col AS CHAR)) and retry"
                .to_string(),
        }),
    }
}

/// Connection/auth failures -> CONNECTION_FAILED (exit 6). The driver names
/// the failing user on auth errors but never the password.
fn mysql_connect_error(e: sqlx::Error) -> EngineError {
    EngineError::Connect {
        message: format!("cannot connect to the MySQL database: {}", error_text(&e)),
        hint: if is_tls_error(&e) {
            tls_hint()
        } else {
            "check the host/port in `url` and the credentials; set `password` on this \
             connection to where the password lives"
                .to_string()
        },
    }
}

/// Query-time errors. The server statement timeout maps to TIMEOUT (exit 8) so
/// the exit code is deterministic: MySQL raises 3024 (max_execution_time) and
/// MariaDB 1969 (max_statement_time). Everything else is DB_ERROR.
fn mysql_error(e: sqlx::Error) -> EngineError {
    if let Some(n) = mysql_err_number(&e) {
        if n == 3024 || n == 1969 {
            return EngineError::Timeout {
                message: "the query exceeded the timeout and was cancelled by the server"
                    .to_string(),
                hint: "narrow the query (WHERE / LIMIT), or raise --timeout or timeout_secs \
                       in the config"
                    .to_string(),
            };
        }
    }
    EngineError::Db {
        message: format!("the database returned an error: {}", error_text(&e)),
        hint: "check the query against the actual schema, e.g. SHOW TABLES or \
               SELECT table_name FROM information_schema.tables WHERE table_schema = DATABASE()"
            .to_string(),
    }
}

/// The MySQL/MariaDB error number (1193 = unknown system variable, 3024 /
/// 1969 = statement timeout), or None for non-database errors.
fn mysql_err_number(e: &sqlx::Error) -> Option<u16> {
    e.as_database_error()
        .and_then(|db| db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
        .map(|m| m.number())
}

/// ER_UNKNOWN_SYSTEM_VARIABLE (1193): the server does not know the timeout
/// variable we tried (wrong flavor) — swallowed, tokio timeout is the backstop.
fn is_unknown_var(e: &sqlx::Error) -> bool {
    mysql_err_number(e) == Some(1193)
}

fn decode_row(row: &SqliteRow) -> Result<Vec<Value>, EngineError> {
    (0..row.len()).map(|i| decode_column(row, i)).collect()
}

/// Decode by the value's storage class. SQLite values are NULL/INTEGER/
/// REAL/TEXT/BLOB; declared column types (DATE, BOOLEAN, ...) fall through
/// to the closest JSON-able form.
fn decode_column(row: &SqliteRow, i: usize) -> Result<Value, EngineError> {
    let raw = row.try_get_raw(i).map_err(db_error)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let value = match raw.type_info().name() {
        "INTEGER" | "BOOLEAN" => row.try_get::<i64, _>(i).map(Value::from),
        // number_or_string: SQLite can store infinities (e.g. 9e999), which
        // JSON cannot — shared with the Postgres float path so the non-finite
        // handling never drifts between engines.
        "REAL" | "NUMERIC" => row.try_get::<f64, _>(i).map(number_or_string),
        // ponytail: blobs come back as a lowercase hex string; a dedicated
        // representation can land if agents actually query binary data.
        "BLOB" => row.try_get::<Vec<u8>, _>(i).map(|b| Value::String(hex(&b))),
        // TEXT and declared date/time types. Decoded as bytes + lossy UTF-8:
        // sqlite does not enforce encoding, and one broken cell must not
        // fail the whole query.
        _ => row
            .try_get::<Vec<u8>, _>(i)
            .map(|b| Value::String(String::from_utf8_lossy(&b).into_owned())),
    };
    value.map_err(db_error)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

