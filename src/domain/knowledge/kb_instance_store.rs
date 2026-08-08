//! kb_instances 表 CRUD — 知识库实例（Dify 外挂 / 内置 Qdrant）。
//!
//! 每条记录 = 一个知识库实例。`config` 为 JSON 文本（TEXT 存储，应用层 serde），
//! 其中 secret 字段（如 Dify api_key）经 AesCodec 加密；加解密在 provider/schema 层做，
//! 本 store 只存原样 config 文本。

use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::{Store, new_id};
use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

/// 知识库实例（一行 = 一个知识库：Dify 或内置）
#[derive(Debug, Clone, QueryableByName, serde::Serialize)]
pub struct KbInstance {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub name: String,
    /// 1=Dify 2=Builtin
    #[diesel(sql_type = sql_types::Int2)]
    pub provider_kind: i16,
    /// JSON 文本（secret 字段已加密）
    #[diesel(sql_type = sql_types::Text)]
    pub config: String,
    /// 1=启用 0=禁用
    #[diesel(sql_type = sql_types::Int2)]
    pub status: i16,
    #[diesel(sql_type = sql_types::Varchar)]
    pub creator: String,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl KbInstance {
    /// config 解析为 JSON Value（secret 字段仍是密文，由 provider 层解密）
    pub fn config_value(&self) -> serde_json::Value {
        serde_json::from_str(&self.config)
            .unwrap_or_else(|_| serde_json::Value::Object(Default::default()))
    }
}

/// 删除知识库实例前的引用影响预检结果（只读计数）
#[derive(Debug, Clone)]
pub struct KbInstanceDeletionImpact {
    /// 绑定该知识库的助手数（assistants.kb_instance_id），删除时将解绑
    pub assistants: i64,
}

/// 删除知识库实例并解绑引用的执行结果
#[derive(Debug, Clone)]
pub struct KbInstanceDeletionCleanup {
    pub deleted: bool,
    pub assistants_unbound: usize,
}

pub struct KbInstanceStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for KbInstanceStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl KbInstanceStore {
    pub async fn new(pool: DbPool) -> Result<Self, AppError> {
        Ok(Self { pool })
    }

    pub async fn list_all(&self) -> Result<Vec<KbInstance>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT id, name, provider_kind, config, status, creator, created_at, updated_at
               FROM kb_instances ORDER BY created_at ASC"#,
        )
        .get_results::<KbInstance>(&mut conn)
        .await?;
        Ok(rows)
    }

    pub async fn get(&self, id: &str) -> Result<Option<KbInstance>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT id, name, provider_kind, config, status, creator, created_at, updated_at
               FROM kb_instances WHERE id = $1"#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_results::<KbInstance>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    /// 启用且存在的实例（路由用）
    pub async fn get_enabled(&self, id: &str) -> Result<Option<KbInstance>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT id, name, provider_kind, config, status, creator, created_at, updated_at
               FROM kb_instances WHERE id = $1 AND status = 1"#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_results::<KbInstance>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn create(
        &self,
        name: &str,
        provider_kind: i16,
        config: &str,
        creator: &str,
    ) -> Result<String, AppError> {
        let id = new_id();
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO kb_instances (id, name, provider_kind, config, status, creator)
               VALUES ($1, $2, $3, $4, 1, $5)"#,
        )
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(name)
        .bind::<sql_types::Int2, _>(provider_kind)
        .bind::<sql_types::Text, _>(config)
        .bind::<sql_types::Text, _>(creator)
        .execute(&mut conn)
        .await?;
        Ok(id)
    }

    pub async fn update(
        &self,
        id: &str,
        name: &str,
        provider_kind: i16,
        config: &str,
        status: i16,
    ) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let affected = diesel::sql_query(
            r#"UPDATE kb_instances
               SET name = $2, provider_kind = $3, config = $4, status = $5, updated_at = NOW()
               WHERE id = $1"#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(name)
        .bind::<sql_types::Int2, _>(provider_kind)
        .bind::<sql_types::Text, _>(config)
        .bind::<sql_types::Int2, _>(status)
        .execute(&mut conn)
        .await?;
        Ok(affected > 0)
    }

    pub async fn delete(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let affected = diesel::sql_query("DELETE FROM kb_instances WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut conn)
            .await?;
        Ok(affected > 0)
    }

    /// 预检：统计删除该知识库实例会牵连的引用（只读，不删除）。
    pub async fn impact_of_delete(&self, id: &str) -> Result<KbInstanceDeletionImpact, AppError> {
        let mut conn = self.get_conn().await?;
        let row = diesel::sql_query(
            "SELECT COUNT(*) AS cnt FROM assistants WHERE kb_instance_id = $1",
        )
        .bind::<sql_types::Text, _>(id)
        .get_result::<CountRow>(&mut conn)
        .await?;
        Ok(KbInstanceDeletionImpact {
            assistants: row.cnt,
        })
    }

    /// 删除知识库实例并解绑所有助手引用（单事务内，任一步失败整体回滚）。
    ///
    /// 引用清理：`assistants.kb_instance_id` 置 NULL（助手保留，仅不再绑定知识库）；
    /// 再删 kb_instances（DB CASCADE 删 kb_documents/kb_chunks）。
    /// 注：Qdrant 向量集合的清理由调用方（knowledge_manager）在提交后处理进程内缓存，
    ///     向量本体残留为已知遗留（不影响解绑后的助手/会话功能）。
    pub async fn delete_with_cleanup(
        &self,
        id: &str,
    ) -> Result<KbInstanceDeletionCleanup, AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("BEGIN").execute(&mut conn).await?;
        let tx: Result<KbInstanceDeletionCleanup, AppError> = async {
            let assistants_unbound = diesel::sql_query(
                "UPDATE assistants SET kb_instance_id = NULL WHERE kb_instance_id = $1",
            )
            .bind::<sql_types::Text, _>(id)
            .execute(&mut conn)
            .await?;

            let aff = diesel::sql_query("DELETE FROM kb_instances WHERE id = $1")
                .bind::<sql_types::Text, _>(id)
                .execute(&mut conn)
                .await?;

            Ok(KbInstanceDeletionCleanup {
                deleted: aff > 0,
                assistants_unbound,
            })
        }
        .await;

        match tx {
            Ok(res) => {
                diesel::sql_query("COMMIT").execute(&mut conn).await?;
                Ok(res)
            }
            Err(e) => {
                let _ = diesel::sql_query("ROLLBACK").execute(&mut conn).await;
                Err(e)
            }
        }
    }

    /// 实例总数（配置迁移 seed 判断用）
    pub async fn count(&self) -> Result<i64, AppError> {
        let mut conn = self.get_conn().await?;
        let row = diesel::sql_query("SELECT COUNT(*) AS cnt FROM kb_instances")
            .get_result::<CountRow>(&mut conn)
            .await?;
        Ok(row.cnt)
    }
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = sql_types::Int8)]
    cnt: i64,
}
