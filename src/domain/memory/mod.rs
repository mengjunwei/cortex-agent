//! 跨会话记忆存储 — memories（已确认）+ memory_proposals（待确认建议）。
//!
//! 范式参照 [`crate::domain::session::assistant_binding`]：diesel 原生 SQL + QueryableByName。
//!
//! ## 作用域
//! - `scope=0` 用户级：跨所有助手共享（默认）。
//! - `scope=1` 助手级：仅 `assistant_id` 命中时注入。
//!
//! 会话**不参与隔离**（会话内信息走 conversation_history），`source_session_id` 仅作溯源。
//!
//! ## 写入流程
//! agent 调 `propose_memory` 工具 → [`MemoryProposalStore::create`]（status=pending）→
//! 前端「建议记忆」卡片确认 → GraphQL `acceptMemoryProposal`（claim + 转正入 memories）/
//! `rejectMemoryProposal`。
//!
//! ## user_id
//! 记忆按真实用户隔离。user_id 来源：对话侧由 SSE 提取的登录用户（贯通自 `OptionalAuthUser`），
//! 管理侧由 GraphQL 解析的当前用户；auth 未启用时回退 `"user"`（与系统其他会话路径一致）。

use std::sync::Arc;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;
use serde::Serialize;

use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::{Store, new_id};

// ========== 枚举常量（DB 存 SMALLINT，避免魔法数字）==========

/// 记忆作用域
pub mod scope {
    /// 用户级：跨所有助手共享（默认）
    pub const USER: i16 = 0;
    /// 助手级：仅该助手
    pub const ASSISTANT: i16 = 1;

    pub fn is_valid(v: i16) -> bool {
        matches!(v, USER | ASSISTANT)
    }
}

/// 记忆类型
pub mod mem_type {
    /// 习惯 / 偏好
    pub const PREFERENCE: i16 = 0;
    /// 坑 / 避坑
    pub const PITFALL: i16 = 1;

    pub fn is_valid(v: i16) -> bool {
        matches!(v, PREFERENCE | PITFALL)
    }
}

/// 建议状态
pub mod proposal_status {
    pub const PENDING: i16 = 0;
    pub const ACCEPTED: i16 = 1;
    pub const REJECTED: i16 = 2;
}

// 把作用域文本标签（前端展示用）
pub fn scope_label(v: i16) -> &'static str {
    match v {
        scope::ASSISTANT => "助手级",
        _ => "用户级",
    }
}
pub fn type_label(v: i16) -> &'static str {
    match v {
        mem_type::PITFALL => "坑",
        _ => "习惯",
    }
}

// =========================================================================
//  已确认记忆：memories
// =========================================================================

/// 一条已确认记忆
#[derive(Debug, Clone, Serialize, QueryableByName)]
pub struct Memory {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub user_id: String,
    #[diesel(sql_type = sql_types::SmallInt)]
    pub scope: i16,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub assistant_id: Option<String>,
    #[diesel(sql_type = sql_types::SmallInt)]
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub mem_type: i16,
    #[diesel(sql_type = sql_types::Text)]
    pub content: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub source_session_id: Option<String>,
    #[diesel(sql_type = sql_types::Text)]
    pub created_at: String,
    #[diesel(sql_type = sql_types::Text)]
    pub updated_at: String,
}

/// memories 查询的统一列（type 是 Rust 关键字 → 用 mem_type 接收，serde 对外仍输出 type）
const MEM_COLS: &str = "id, user_id, scope, assistant_id, type AS mem_type, content, \
     source_session_id, created_at::text AS created_at, updated_at::text AS updated_at";

pub struct MemoryStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for MemoryStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl MemoryStore {
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        Ok(Arc::new(Self { pool }))
    }

    /// 拉取注入用记忆：scope=0 全部 + scope=1 且 assistant_id 命中。
    /// 按 scope 升序（用户级在前）、updated_at 降序；上限 200 条防止 prompt 膨胀。
    pub async fn list_for_inject(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Vec<Memory>, AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(format!(
            "SELECT {MEM_COLS} FROM memories \
             WHERE user_id = $1 AND (scope = 0 OR (scope = 1 AND assistant_id = $2)) \
             ORDER BY scope ASC, updated_at DESC \
             LIMIT 200"
        ))
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::Text, _>(assistant_id)
        .get_results::<Memory>(&mut conn)
        .await
        .map_err(AppError::from)
    }

    /// 管理页：列出用户全部记忆
    pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<Memory>, AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(format!(
            "SELECT {MEM_COLS} FROM memories WHERE user_id = $1 \
             ORDER BY updated_at DESC LIMIT 500"
        ))
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<Memory>(&mut conn)
        .await
        .map_err(AppError::from)
    }

    /// 新增一条记忆（RETURNING 拿回完整行，含服务端生成的时间戳）。
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        user_id: &str,
        scope: i16,
        assistant_id: Option<&str>,
        mem_type: i16,
        content: &str,
        source_session_id: Option<&str>,
    ) -> Result<Memory, AppError> {
        let id = new_id();
        let assistant_id = assistant_id.map(|s| s.to_string());
        let source_session_id = source_session_id.map(|s| s.to_string());
        let mut conn = self.get_conn().await?;
        diesel::sql_query(format!(
            "INSERT INTO memories (id, user_id, scope, assistant_id, type, content, source_session_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING {MEM_COLS}"
        ))
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::SmallInt, _>(scope)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(assistant_id)
        .bind::<sql_types::SmallInt, _>(mem_type)
        .bind::<sql_types::Text, _>(content)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(source_session_id)
        .get_result::<Memory>(&mut conn)
        .await
        .map_err(AppError::from)
    }

    /// 编辑记忆正文/类型（带 user_id 防越权）
    pub async fn update(
        &self,
        id: &str,
        user_id: &str,
        mem_type: i16,
        content: &str,
    ) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            "UPDATE memories SET type = $3, content = $4, updated_at = NOW() \
             WHERE id = $1 AND user_id = $2",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::SmallInt, _>(mem_type)
        .bind::<sql_types::Text, _>(content)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    /// 删除（带 user_id 防越权：只能删自己的记忆）
    pub async fn delete(&self, id: &str, user_id: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("DELETE FROM memories WHERE id = $1 AND user_id = $2")
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(user_id)
            .execute(&mut conn)
            .await?;
        Ok(())
    }
}

// =========================================================================
//  待确认建议：memory_proposals
// =========================================================================

/// 一条待确认的记忆建议（agent 通过 propose_memory 工具产出）
#[derive(Debug, Clone, Serialize, QueryableByName)]
pub struct Proposal {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub user_id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub session_id: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub assistant_id: Option<String>,
    #[diesel(sql_type = sql_types::SmallInt)]
    pub scope: i16,
    #[diesel(sql_type = sql_types::SmallInt)]
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub mem_type: i16,
    #[diesel(sql_type = sql_types::Text)]
    pub content: String,
    #[diesel(sql_type = sql_types::Text)]
    pub reason: String,
    #[diesel(sql_type = sql_types::SmallInt)]
    pub status: i16,
    #[diesel(sql_type = sql_types::Text)]
    pub created_at: String,
}

const PROP_COLS: &str = "id, user_id, session_id, assistant_id, scope, type AS mem_type, \
     content, reason, status, created_at::text AS created_at";

pub struct MemoryProposalStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for MemoryProposalStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl MemoryProposalStore {
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        Ok(Arc::new(Self { pool }))
    }

    /// agent 产出一条建议（status=pending）
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        user_id: &str,
        session_id: &str,
        assistant_id: Option<&str>,
        scope: i16,
        mem_type: i16,
        content: &str,
        reason: &str,
    ) -> Result<Proposal, AppError> {
        let id = new_id();
        let assistant_id = assistant_id.map(|s| s.to_string());
        let mut conn = self.get_conn().await?;
        diesel::sql_query(format!(
            "INSERT INTO memory_proposals \
             (id, user_id, session_id, assistant_id, scope, type, content, reason, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0) \
             RETURNING {PROP_COLS}"
        ))
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::Text, _>(session_id)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(assistant_id)
        .bind::<sql_types::SmallInt, _>(scope)
        .bind::<sql_types::SmallInt, _>(mem_type)
        .bind::<sql_types::Text, _>(content)
        .bind::<sql_types::Text, _>(reason)
        .get_result::<Proposal>(&mut conn)
        .await
        .map_err(AppError::from)
    }

    /// 卡片列表：用户的待确认建议
    pub async fn list_pending(&self, user_id: &str) -> Result<Vec<Proposal>, AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(format!(
            "SELECT {PROP_COLS} FROM memory_proposals \
             WHERE user_id = $1 AND status = 0 \
             ORDER BY created_at DESC LIMIT 200"
        ))
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<Proposal>(&mut conn)
        .await
        .map_err(AppError::from)
    }

    /// 按会话拉建议（前端在对话流里渲染该会话产生的卡片）
    pub async fn list_for_session(&self, session_id: &str) -> Result<Vec<Proposal>, AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(format!(
            "SELECT {PROP_COLS} FROM memory_proposals \
             WHERE session_id = $1 \
             ORDER BY created_at ASC LIMIT 200"
        ))
        .bind::<sql_types::Text, _>(session_id)
        .get_results::<Proposal>(&mut conn)
        .await
        .map_err(AppError::from)
    }

    /// 乐观锁领取：只把「本用户 + pending」的建议置为 accepted，返回领取到的建议。
    /// 并发安全（同一建议只会被领取一次）；非本人 / 非 pending / 不存在返回 None。
    pub async fn claim(&self, id: &str, user_id: &str) -> Result<Option<Proposal>, AppError> {
        let mut conn = self.get_conn().await?;
        let n = diesel::sql_query(
            "UPDATE memory_proposals SET status = 1 \
             WHERE id = $1 AND status = 0 AND user_id = $2",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(user_id)
        .execute(&mut conn)
        .await?;
        if n == 0 {
            return Ok(None);
        }
        let p = diesel::sql_query(format!(
            "SELECT {PROP_COLS} FROM memory_proposals WHERE id = $1"
        ))
        .bind::<sql_types::Text, _>(id)
        .get_result::<Proposal>(&mut conn)
        .await?;
        Ok(Some(p))
    }

    /// 乐观锁拒绝：把「本用户 + pending」的建议置为 rejected。返回是否实际生效。
    pub async fn reject(&self, id: &str, user_id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let n = diesel::sql_query(
            "UPDATE memory_proposals SET status = 2 \
             WHERE id = $1 AND status = 0 AND user_id = $2",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(user_id)
        .execute(&mut conn)
        .await?;
        Ok(n > 0)
    }

    /// 按用户拉全部建议（管理页/调试用，含历史状态）
    pub async fn list_all_for_user(&self, user_id: &str) -> Result<Vec<Proposal>, AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(format!(
            "SELECT {PROP_COLS} FROM memory_proposals \
             WHERE user_id = $1 ORDER BY created_at DESC LIMIT 500"
        ))
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<Proposal>(&mut conn)
        .await
        .map_err(AppError::from)
    }
}

// =========================================================================
//  注入块渲染
// =========================================================================

/// 把记忆列表渲染成注入 stable prefix 的文本块（习惯 / 坑分组）。
///
/// 输出形如：
/// ```text
/// ## 关于这位用户的长期记忆（跨会话积累）
/// 请严格遵循下面的「习惯」，并主动避开下面的「坑」。
///
/// ### 应遵循的习惯 / 偏好
/// - 用简体中文回复
///
/// ### 必须避开的坑
/// - 这个项目用 PostgreSQL，不是 MySQL
/// ```
pub fn render_inject_block(memories: &[Memory]) -> String {
    let mut prefs: Vec<&str> = Vec::new();
    let mut pits: Vec<&str> = Vec::new();
    for m in memories {
        if m.mem_type == mem_type::PITFALL {
            pits.push(m.content.as_str());
        } else {
            prefs.push(m.content.as_str());
        }
    }
    if prefs.is_empty() && pits.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## 关于这位用户的长期记忆（跨会话积累）\n\
         请严格遵循下面的「习惯」，并主动避开下面的「坑」。",
    );
    if !prefs.is_empty() {
        out.push_str("\n\n### 应遵循的习惯 / 偏好");
        for p in prefs {
            out.push_str("\n- ");
            out.push_str(p);
        }
    }
    if !pits.is_empty() {
        out.push_str("\n\n### 必须避开的坑");
        for p in pits {
            out.push_str("\n- ");
            out.push_str(p);
        }
    }
    out
}
