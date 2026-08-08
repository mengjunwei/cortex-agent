//! kb_documents 表 CRUD —— 内置知识库 provider 的文档元数据。
//!
//! 与 dify 时代的 `kb_doc_meta` 区别：
//! - 主键 VARCHAR(36) UUID v7（架构 §8.1；旧表用 SERIAL，已废弃不再读写）
//! - 带 `kb_instance_id`（文档归属某个内置知识库实例）
//! - 仅内置 provider 使用；dify provider 文档不入此表（实时调 dify API）

use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::Store;
use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

/// `kb_documents` 表查询结果行
#[derive(Debug, Clone, QueryableByName, serde::Serialize)]
pub struct KbDocument {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub kb_instance_id: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub doc_type: i16, // 1=手册 2=FAQ
    #[diesel(sql_type = sql_types::Varchar)]
    pub brand: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub dev_type: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub model: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub firmware_ver: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub title: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub source: String,
    #[diesel(sql_type = sql_types::Int4)]
    pub word_count: i32,
    #[diesel(sql_type = sql_types::Int4)]
    pub chunk_count: i32,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub struct DocumentStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for DocumentStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl DocumentStore {
    /// 接收共享连接池（建表由 migrations/schema.sql 负责）
    pub async fn new(pool: DbPool) -> Result<Self, AppError> {
        Ok(Self { pool })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        &self,
        id: &str,
        kb_instance_id: &str,
        doc_type: i16,
        brand: &str,
        dev_type: &str,
        model: &str,
        firmware_ver: &str,
        title: &str,
        source: &str,
        word_count: i32,
        chunk_count: i32,
        uploaded_by: &str,
    ) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO kb_documents
               (id, kb_instance_id, doc_type, brand, dev_type, model, firmware_ver, title, source, word_count, chunk_count, uploaded_by)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)"#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(kb_instance_id)
        .bind::<sql_types::Int2, _>(doc_type)
        .bind::<sql_types::Text, _>(brand)
        .bind::<sql_types::Text, _>(dev_type)
        .bind::<sql_types::Text, _>(model)
        .bind::<sql_types::Text, _>(firmware_ver)
        .bind::<sql_types::Text, _>(title)
        .bind::<sql_types::Text, _>(source)
        .bind::<sql_types::Int4, _>(word_count)
        .bind::<sql_types::Int4, _>(chunk_count)
        .bind::<sql_types::Text, _>(uploaded_by)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    pub async fn update_chunk_count(&self, id: &str, chunk_count: i32) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("UPDATE kb_documents SET chunk_count=$2, updated_at=NOW() WHERE id=$1")
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Int4, _>(chunk_count)
            .execute(&mut conn)
            .await?;
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("DELETE FROM kb_documents WHERE id=$1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut conn)
            .await?;
        Ok(())
    }

    /// 分页 + brand/dev_type/keyword 过滤（限定在某知识库实例内）
    pub async fn list(
        &self,
        kb_instance_id: &str,
        page: u32,
        limit: u32,
        brand: Option<&str>,
        dev_type: Option<&str>,
        keyword: Option<&str>,
    ) -> Result<(Vec<KbDocument>, i64), AppError> {
        let mut conn = self.get_conn().await?;
        let offset = (page.saturating_sub(1) * limit) as i64;
        let brand = brand.filter(|s| !s.is_empty());
        let dev_type = dev_type.filter(|s| !s.is_empty());
        let keyword = keyword.filter(|s| !s.is_empty());

        let rows = diesel::sql_query(
            r#"SELECT id, kb_instance_id, doc_type, brand, dev_type, model, firmware_ver, title, source, word_count, chunk_count, created_at
               FROM kb_documents
               WHERE kb_instance_id = $1
                 AND ($2::text IS NULL OR brand = $2)
                 AND ($3::text IS NULL OR dev_type = $3)
                 AND ($4::text IS NULL OR title ILIKE '%' || $4 || '%')
               ORDER BY created_at DESC
               LIMIT $5 OFFSET $6"#,
        )
        .bind::<sql_types::Text, _>(kb_instance_id)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(brand)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(dev_type)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(keyword)
        .bind::<sql_types::Int8, _>(limit as i64)
        .bind::<sql_types::Int8, _>(offset)
        .get_results::<KbDocument>(&mut conn)
        .await?;

        let total: i64 = diesel::sql_query(
            r#"SELECT COUNT(*) AS cnt FROM kb_documents
               WHERE kb_instance_id = $1
                 AND ($2::text IS NULL OR brand = $2)
                 AND ($3::text IS NULL OR dev_type = $3)
                 AND ($4::text IS NULL OR title ILIKE '%' || $4 || '%')"#,
        )
        .bind::<sql_types::Text, _>(kb_instance_id)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(brand)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(dev_type)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(keyword)
        .get_result::<CountRow>(&mut conn)
        .await?
        .cnt;
        Ok((rows, total))
    }

    /// FAQ 查重：按 (kb_instance, brand, dev_type, title) 查已存在文档 id
    pub async fn find_by_instance_brand_dev_titles(
        &self,
        kb_instance_id: &str,
        brand: &str,
        dev_type: &str,
        titles: &[String],
    ) -> Result<Vec<(String, String)>, AppError> {
        if titles.is_empty() {
            return Ok(vec![]);
        }
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT id, title FROM kb_documents
               WHERE kb_instance_id=$1 AND brand=$2 AND dev_type=$3 AND title = ANY($4)"#,
        )
        .bind::<sql_types::Text, _>(kb_instance_id)
        .bind::<sql_types::Text, _>(brand)
        .bind::<sql_types::Text, _>(dev_type)
        .bind::<sql_types::Array<sql_types::Text>, _>(titles)
        .get_results::<IdTitleRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().map(|r| (r.id, r.title)).collect())
    }
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = sql_types::Int8)]
    cnt: i64,
}

#[derive(QueryableByName)]
struct IdTitleRow {
    #[diesel(sql_type = sql_types::Varchar)]
    id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    title: String,
}
