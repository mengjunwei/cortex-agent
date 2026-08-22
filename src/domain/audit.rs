//! 审计日志 — 记录增删改类写操作（谁、何时、做了什么、结果）。
//!
//! 落 `audit_logs` 表。GraphQL mutation 在 `graphql_handler` 统一拦截；
//! REST 写操作（auth 登录/注册/注销、shell-approve、upload）在各 handler 内显式记录。
//! 写入异步、失败仅丢日志（审计可降级，绝不阻塞业务主流程）。

use std::sync::Arc;

use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::infra::db::{DbPool, DbPooledConnection};

/// 一条审计记录（owned，可跨 `tokio::spawn`）
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub user_id: String,
    /// 显示名 / username（失败登录时 user_id 为空，靠 actor 记是谁）
    pub actor: String,
    /// 来源：`web`（账号登录）/ `api_token`（程序化 Bearer）
    pub source: String,
    /// 操作名：GraphQL mutation 名（deleteSession…）或 REST 动作（login/upload_image…）
    pub operation: String,
    /// 被操作对象 id（从参数提取）
    pub target_id: String,
    pub success: bool,
    /// 脱敏后的参数 JSON
    pub detail: String,
    pub ip: String,
}

/// 审计日志存储（仅 INSERT，不缓存、不自动建表——表由 schema.sql 部署时建）
pub struct AuditStore {
    pool: DbPool,
}

impl AuditStore {
    pub async fn new(pool: DbPool) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self { pool }))
    }

    async fn get_conn(&self) -> anyhow::Result<DbPooledConnection> {
        self.pool
            .get()
            .await
            .map_err(|e| anyhow::anyhow!("DB 连接获取失败: {e}"))
    }

    /// 写入一条审计记录。失败返回 Err（调用方在 spawn 内忽略即可，审计不阻塞业务）。
    pub async fn record(&self, e: AuditEntry) -> anyhow::Result<()> {
        let id = uuid::Uuid::now_v7().to_string();
        let now = chrono::Utc::now();
        let mut c = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO audit_logs
               (id, user_id, actor, source, operation, target_id, success, detail, ip, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
        )
        .bind::<sql_types::VarChar, _>(&id)
        .bind::<sql_types::VarChar, _>(&e.user_id)
        .bind::<sql_types::VarChar, _>(&e.actor)
        .bind::<sql_types::VarChar, _>(&e.source)
        .bind::<sql_types::VarChar, _>(&e.operation)
        .bind::<sql_types::VarChar, _>(&e.target_id)
        .bind::<sql_types::SmallInt, _>(if e.success { 1i16 } else { 0i16 })
        .bind::<sql_types::Text, _>(&e.detail)
        .bind::<sql_types::VarChar, _>(&e.ip)
        .bind::<sql_types::Timestamptz, _>(now)
        .execute(&mut c)
        .await?;
        Ok(())
    }
}

/// 异步记录审计（spawn，不阻塞业务；store 为 None 时跳过）。供 REST handler 简便调用。
pub fn spawn_record(store: Option<&Arc<AuditStore>>, entry: AuditEntry) {
    if let Some(store) = store {
        let store = store.clone();
        tokio::spawn(async move {
            let _ = store.record(entry).await;
        });
    }
}

