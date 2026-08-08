//! kb_chunks 表 CRUD —— 内置知识库 provider 的分段预览。
//!
//! 表带 `ON DELETE CASCADE`（document_id REFERENCES kb_documents），删文档时自动联动删分段。

use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::Store;
use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

#[derive(Debug, Clone, QueryableByName, serde::Serialize)]
pub struct KbChunk {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub document_id: String,
    #[diesel(sql_type = sql_types::Int4)]
    pub chunk_index: i32,
    #[diesel(sql_type = sql_types::Text)]
    pub content: String,
    #[diesel(sql_type = sql_types::Int4)]
    pub word_count: i32,
    #[diesel(sql_type = sql_types::Varchar)]
    pub header_path: String,
}

pub struct ChunkStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for ChunkStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl ChunkStore {
    pub async fn new(pool: DbPool) -> Result<Self, AppError> {
        Ok(Self { pool })
    }

    /// 批量插入分段：(id, document_id, chunk_index, content, word_count, header_path)
    pub async fn insert_batch(
        &self,
        rows: &[(String, String, i32, String, i32, String)],
    ) -> Result<(), AppError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.get_conn().await?;
        for (id, document_id, chunk_index, content, word_count, header_path) in rows {
            diesel::sql_query(
                r#"INSERT INTO kb_chunks (id, document_id, chunk_index, content, word_count, header_path)
                   VALUES ($1,$2,$3,$4,$5,$6)"#,
            )
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(document_id)
            .bind::<sql_types::Int4, _>(chunk_index)
            .bind::<sql_types::Text, _>(content)
            .bind::<sql_types::Int4, _>(word_count)
            .bind::<sql_types::Text, _>(header_path)
            .execute(&mut conn)
            .await?;
        }
        Ok(())
    }

    pub async fn list_by_document(&self, document_id: &str) -> Result<Vec<KbChunk>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT id, document_id, chunk_index, content, word_count, header_path
               FROM kb_chunks WHERE document_id=$1 ORDER BY chunk_index ASC"#,
        )
        .bind::<sql_types::Text, _>(document_id)
        .get_results::<KbChunk>(&mut conn)
        .await?;
        Ok(rows)
    }
}
