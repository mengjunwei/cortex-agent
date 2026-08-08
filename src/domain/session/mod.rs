//! 会话级配置合并存储 — `session_settings` 一张大表
//!
//! 设计目标：
//! - 不依赖 / 修改 adk-rust 的 sessions 表，保持外部依赖隔离
//! - 一张表收纳一个会话的所有配置：标题 / agent_type / 模型绑定 /
//!   思考级别 / 沙箱+审批 / 助手绑定，一行一个会话
//! - `title` / `agent_type` 物化落列，供会话列表直接 SQL 排序/筛选/分页/连表，
//!   不再「拉全量会话内存处理」
//! - 取代旧的 4 张小表（session_models / session_assistants /
//!   session_thinking_levels / session_permission_policies），历史数据已清空
//!
//! 表结构（建表 DDL 见 `migrations/schema.sql`）：
//! ```sql
//! CREATE TABLE IF NOT EXISTS session_settings (
//!     session_id      VARCHAR(64) PRIMARY KEY,
//!     title           TEXT        NOT NULL DEFAULT '',
//!     agent_type      VARCHAR(32) NOT NULL DEFAULT 'custom',
//!     model_id        VARCHAR(64),          -- NULL=未绑定具体模型
//!     thinking_level  TEXT        NOT NULL DEFAULT 'high',
//!     sandbox_mode    TEXT        NOT NULL DEFAULT 'workspace-write',
//!     approval_policy TEXT        NOT NULL DEFAULT 'unless-trusted',
//!     assistant_id    VARCHAR(64),          -- NULL=未绑定助手
//!     updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
//! );
//! ```
//!
//! 一致性：session 删除时由 `delete_session` handler 调 `delete` 顺带清理本行。

use std::collections::HashMap;
use std::sync::Arc;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::domain::permissions::{ApprovalPolicy, SandboxMode};
use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::Store;

pub struct SessionSettingsStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for SessionSettingsStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl SessionSettingsStore {
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        let store = Arc::new(Self { pool });
        Ok(store)
    }

    // ========== 写入（局部 upsert：只更新目标列，不覆盖其它列）==========

    /// 创建会话时一次性落初始行（user_id / title / agent_type / model_id / assistant_id）。
    /// 未提供的列取表默认值；model_id / assistant_id 为 None 时保持 NULL。
    pub async fn init_session(
        &self,
        session_id: &str,
        user_id: &str,
        title: &str,
        agent_type: &str,
        model_id: Option<&str>,
        assistant_id: Option<&str>,
    ) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"
            INSERT INTO session_settings (session_id, user_id, title, agent_type, model_id, assistant_id, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW())
            ON CONFLICT (session_id) DO UPDATE
                SET user_id      = EXCLUDED.user_id,
                    title        = EXCLUDED.title,
                    agent_type   = EXCLUDED.agent_type,
                    model_id     = EXCLUDED.model_id,
                    assistant_id = EXCLUDED.assistant_id,
                    updated_at   = NOW()
            "#,
        )
        .bind::<sql_types::Text, _>(session_id)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::Text, _>(title)
        .bind::<sql_types::Text, _>(agent_type)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(model_id)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(assistant_id)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    /// 重命名 / 默认标题回填：只更新 title
    pub async fn set_title(&self, session_id: &str, title: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"
            INSERT INTO session_settings (session_id, title, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (session_id) DO UPDATE
                SET title = EXCLUDED.title, updated_at = NOW()
            "#,
        )
        .bind::<sql_types::Text, _>(session_id)
        .bind::<sql_types::Text, _>(title)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    /// 绑定/解绑模型：Some(mid) 绑定；None（default/auto/空）解绑置 NULL
    pub async fn set_model(&self, session_id: &str, model_id: Option<&str>) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"
            INSERT INTO session_settings (session_id, model_id, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (session_id) DO UPDATE
                SET model_id = EXCLUDED.model_id, updated_at = NOW()
            "#,
        )
        .bind::<sql_types::Text, _>(session_id)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(model_id)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    /// 绑定/解绑助手：Some(aid) 绑定；None 解绑置 NULL
    pub async fn set_assistant(
        &self,
        session_id: &str,
        assistant_id: Option<&str>,
    ) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"
            INSERT INTO session_settings (session_id, assistant_id, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (session_id) DO UPDATE
                SET assistant_id = EXCLUDED.assistant_id, updated_at = NOW()
            "#,
        )
        .bind::<sql_types::Text, _>(session_id)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(assistant_id)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    /// 写入/更新会话级思考级别（low/medium/high/xhigh/max）
    pub async fn set_thinking_level(&self, session_id: &str, level: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"
            INSERT INTO session_settings (session_id, thinking_level, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (session_id) DO UPDATE
                SET thinking_level = EXCLUDED.thinking_level, updated_at = NOW()
            "#,
        )
        .bind::<sql_types::Text, _>(session_id)
        .bind::<sql_types::Text, _>(level)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    /// 写入/更新会话级审批方式（沙箱模式 + 审批策略）
    pub async fn set_permission_policy(
        &self,
        session_id: &str,
        sandbox_mode: SandboxMode,
        approval_policy: ApprovalPolicy,
    ) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"
            INSERT INTO session_settings (session_id, sandbox_mode, approval_policy, updated_at)
            VALUES ($1, $2, $3, NOW())
            ON CONFLICT (session_id) DO UPDATE
                SET sandbox_mode    = EXCLUDED.sandbox_mode,
                    approval_policy = EXCLUDED.approval_policy,
                    updated_at      = NOW()
            "#,
        )
        .bind::<sql_types::Text, _>(session_id)
        .bind::<sql_types::Text, _>(sandbox_mode.codex_id())
        .bind::<sql_types::Text, _>(approval_policy.codex_id())
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    // ========== 读取 ==========

    /// 读取单个会话绑定的模型 id（None=未绑定，运行时解析全局默认）
    pub async fn get_model(&self, session_id: &str) -> Result<Option<String>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT model_id AS val FROM session_settings WHERE session_id = $1",
        )
        .bind::<sql_types::Text, _>(session_id)
        .get_results::<NullableTextRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next().and_then(|r| r.val))
    }

    /// 读取单个会话绑定的助手 id（None=未绑定）
    pub async fn get_assistant(&self, session_id: &str) -> Result<Option<String>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT assistant_id AS val FROM session_settings WHERE session_id = $1",
        )
        .bind::<sql_types::Text, _>(session_id)
        .get_results::<NullableTextRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next().and_then(|r| r.val))
    }

    /// 读取会话级思考级别（不存在返回 None，由调用方按默认 high 处理）
    pub async fn get_thinking_level(&self, session_id: &str) -> Result<Option<String>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT thinking_level AS val FROM session_settings WHERE session_id = $1",
        )
        .bind::<sql_types::Text, _>(session_id)
        .get_results::<NullableTextRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next().and_then(|r| r.val))
    }

    /// 读取会话级审批方式（不存在/脏值 → None，由调用方回退全局默认）
    pub async fn get_permission_policy(
        &self,
        session_id: &str,
    ) -> Result<Option<(SandboxMode, ApprovalPolicy)>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT sandbox_mode AS sm, approval_policy AS ap FROM session_settings WHERE session_id = $1",
        )
        .bind::<sql_types::Text, _>(session_id)
        .get_results::<PermissionPolicyRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next().and_then(|r| {
            Some((
                SandboxMode::from_codex_id(&r.sm)?,
                ApprovalPolicy::from_codex_id(&r.ap)?,
            ))
        }))
    }

    /// 批量读取多个会话的「模型 + 助手」绑定（列表一次性注入，避免 N+1）
    pub async fn get_settings_batch(
        &self,
        session_ids: &[String],
    ) -> Result<HashMap<String, (Option<String>, Option<String>)>, AppError> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut conn = self.get_conn().await?;
        let ids: Vec<&str> = session_ids.iter().map(|s| s.as_str()).collect();
        let rows = diesel::sql_query(
            "SELECT session_id AS sid, model_id AS mid, assistant_id AS aid \
             FROM session_settings WHERE session_id = ANY($1)",
        )
        .bind::<sql_types::Array<sql_types::Text>, _>(&ids)
        .get_results::<SettingsBatchRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().map(|r| (r.sid, (r.mid, r.aid))).collect())
    }

    /// 删除某会话的整行配置（会话被删除时清理）
    pub async fn delete(&self, session_id: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("DELETE FROM session_settings WHERE session_id = $1")
            .bind::<sql_types::Text, _>(session_id)
            .execute(&mut conn)
            .await?;
        Ok(())
    }

    // ========== 会话列表：SQL 连表排序/筛选/分页 ==========

    /// 会话列表分页查询（LEFT JOIN assistants 取 name/kind）。
    ///
    /// 排序固定按 session_id 字符串倒序（UUID v7 = 创建时间倒序，最新在前）。
    /// 可选筛选：keyword(title 模糊) / agent_type / assistant_id / kind。
    /// 返回 (当前页行, 总数)。
    /// 按 session_id 反查会话归属用户（`session_settings.user_id`）。
    ///
    /// 管理员跨用户访问的钥匙：管理员操作时需用**归属者**的 user_id 去读写 ADK session
    /// （ADK 表按 user_id 隔离），从而不改归属、不串记忆。返回 None=会话不存在或无归属记录。
    pub async fn get_owner(&self, session_id: &str) -> Result<Option<String>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT user_id AS val FROM session_settings WHERE session_id = $1",
        )
        .bind::<sql_types::Text, _>(session_id)
        .get_results::<NullableTextRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next().and_then(|r| r.val).filter(|s| !s.is_empty()))
    }

    /// 分页列出会话。
    ///
    /// `admin_view=false`：按 `user_id` 硬过滤（普通用户只看自己）。
    /// `admin_view=true`：放开归属过滤（管理员看全部），`user_id` 参数被忽略。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_page(
        &self,
        user_id: &str,
        admin_view: bool,
        page: usize,
        page_size: usize,
        keyword: Option<&str>,
        agent_type: Option<&str>,
        assistant_id: Option<&str>,
        kind: Option<i16>,
    ) -> Result<(Vec<SessionListRow>, i64), AppError> {
        // 动态拼 WHERE：管理员放开归属过滤，普通用户按归属硬隔离；其余按需叠加
        let mut where_sql = if admin_view {
            String::from("TRUE")
        } else {
            String::from("s.user_id = $1")
        };
        let mut bind_idx = if admin_view { 1 } else { 2 };
        let mut kw_pos = 0usize;
        let mut at_pos = 0usize;
        let mut aid_pos = 0usize;
        let mut kind_pos = 0usize;

        if keyword.is_some() {
            kw_pos = bind_idx;
            where_sql.push_str(&format!(" AND s.title ILIKE ${bind_idx}"));
            bind_idx += 1;
        }
        if agent_type.is_some() {
            at_pos = bind_idx;
            where_sql.push_str(&format!(" AND s.agent_type = ${bind_idx}"));
            bind_idx += 1;
        }
        if assistant_id.is_some() {
            aid_pos = bind_idx;
            where_sql.push_str(&format!(" AND s.assistant_id = ${bind_idx}"));
            bind_idx += 1;
        }
        if kind.is_some() {
            kind_pos = bind_idx;
            // kind 缺省（未绑助手/助手已删 → JOIN 不到）按「自定义=1」归类（沿用历史语义）
            where_sql.push_str(&format!(" AND COALESCE(a.kind, 1) = ${bind_idx}"));
            bind_idx += 1;
        }
        let limit_pos = bind_idx;
        let offset_pos = bind_idx + 1;

        let from_sql = "FROM session_settings s \
                        LEFT JOIN assistants a ON s.assistant_id = a.id \
                        LEFT JOIN users u ON s.user_id = u.id";
        let select_sql = format!(
            "SELECT s.session_id AS sid, s.title AS title, s.agent_type AS agent_type, \
                    s.model_id AS mid, s.assistant_id AS aid, s.updated_at AS updated_at, \
                    a.name AS assistant_name, a.kind AS assistant_kind, \
                    COALESCE(NULLIF(u.name, ''), NULLIF(u.username, ''), LEFT(s.user_id, 8)) AS owner \
             {from_sql} WHERE {where_sql} \
             ORDER BY s.session_id DESC LIMIT ${limit_pos} OFFSET ${offset_pos}"
        );
        let count_sql = format!("SELECT COUNT(*) AS cnt {from_sql} WHERE {where_sql}");

        let mut conn = self.get_conn().await?;

        // 分页主查询。管理员视图不 bind user_id（where 以 $1 起始于后续参数）。
        let mut q = diesel::sql_query(&select_sql).into_boxed();
        if !admin_view {
            q = q.bind::<sql_types::Text, _>(user_id);
        }
        if let Some(kw) = keyword {
            let _ = kw_pos;
            q = q.bind::<sql_types::Text, _>(format!("%{kw}%"));
        }
        if let Some(at) = agent_type {
            let _ = at_pos;
            q = q.bind::<sql_types::Text, _>(at);
        }
        if let Some(aid) = assistant_id {
            let _ = aid_pos;
            q = q.bind::<sql_types::Text, _>(aid);
        }
        if let Some(k) = kind {
            let _ = kind_pos;
            q = q.bind::<sql_types::SmallInt, _>(k);
        }
        let limit = page_size as i64;
        let offset = ((page.max(1) - 1) * page_size) as i64;
        q = q.bind::<sql_types::BigInt, _>(limit);
        q = q.bind::<sql_types::BigInt, _>(offset);
        let rows = q.get_results::<SessionListRow>(&mut conn).await?;

        // 总数查询（同 WHERE，不含 LIMIT/OFFSET）
        let mut cq = diesel::sql_query(&count_sql).into_boxed();
        if !admin_view {
            cq = cq.bind::<sql_types::Text, _>(user_id);
        }
        if let Some(kw) = keyword {
            cq = cq.bind::<sql_types::Text, _>(format!("%{kw}%"));
        }
        if let Some(at) = agent_type {
            cq = cq.bind::<sql_types::Text, _>(at);
        }
        if let Some(aid) = assistant_id {
            cq = cq.bind::<sql_types::Text, _>(aid);
        }
        if let Some(k) = kind {
            cq = cq.bind::<sql_types::SmallInt, _>(k);
        }
        let total = cq
            .get_result::<CountRow>(&mut conn)
            .await?
            .cnt;

        Ok((rows, total))
    }
}

/// 会话列表行（连 assistants 后的展示字段）
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
pub struct SessionListRow {
    #[diesel(sql_type = sql_types::Varchar)]
    pub sid: String,
    #[diesel(sql_type = sql_types::Text)]
    pub title: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub agent_type: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub mid: Option<String>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub aid: Option<String>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub assistant_name: Option<String>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::SmallInt>)]
    pub assistant_kind: Option<i16>,
    /// 归属用户（管理员视图展示「谁的会话」；普通用户视图恒等于自己）
    #[diesel(sql_type = sql_types::Varchar)]
    pub owner: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct CountRow {
    #[diesel(sql_type = sql_types::BigInt)]
    cnt: i64,
}

// ========== 查询行结构 ==========

/// 单可空文本列通用行（model_id / assistant_id / thinking_level 复用）
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct NullableTextRow {
    #[diesel(sql_type = sql_types::Nullable<sql_types::Text>)]
    val: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct PermissionPolicyRow {
    #[diesel(sql_type = sql_types::Text)]
    sm: String,
    #[diesel(sql_type = sql_types::Text)]
    ap: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct SettingsBatchRow {
    #[diesel(sql_type = sql_types::Varchar)]
    sid: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    mid: Option<String>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    aid: Option<String>,
}
