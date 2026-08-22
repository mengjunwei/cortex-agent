//! 知识库向量存储 —— 封装 qdrant-client 1.18，实现 `adk_rag::VectorStore` + 过滤检索。
//!
//! 与 adk-rag 自带 `QdrantVectorStore` 的区别：
//! 1. upsert 时把 brand/dev_type/doc_type/title/document_id 等 **打平到 payload 顶层**
//!    （adk-rag 嵌套为 `metadata:{...}`），便于下推 Qdrant keyword filter；
//! 2. 新增 `search_filtered`（带 brand/dev_type/doc_type 过滤的向量检索）；
//! 3. 新增 `delete_by_document`（按 document_id payload 过滤）。

use std::collections::HashMap;

use async_trait::async_trait;
use qdrant_client::qdrant::r#match::MatchValue;
use qdrant_client::qdrant::point_id::PointIdOptions;
use qdrant_client::qdrant::value::Kind;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, DeletePointsBuilder, Distance, Filter, PointStruct,
    PointsIdsList, SearchPointsBuilder, UpsertPointsBuilder, VectorParamsBuilder,
};
use qdrant_client::{Payload, Qdrant};

use adk_rag::document::{Chunk, SearchResult};
use adk_rag::error::{RagError, Result};
use adk_rag::vectorstore::VectorStore;

/// 知识库过滤条件（任一为 None 表示不过滤该项）
#[derive(Debug, Clone, Default)]
pub struct KbFilter {
    pub brand: Option<String>,
    pub dev_type: Option<String>,
    pub model: Option<String>,
    pub doc_type: Option<String>,
}

impl KbFilter {
    /// 转 Qdrant Filter：「填了要遵守，不填都符合」。
    ///
    /// 每个属性用 `should(值匹配, 字段为空)`：
    /// - 文档有该属性且值匹配 → 通过（match 命中）
    /// - 文档没该属性 → 通过（is_empty 命中）
    /// - 文档有该属性但值不匹配 → 过滤（两个都不命中）
    ///
    /// 多个属性之间用 `must` 组合（AND）。
    fn to_qdrant(&self) -> Option<Filter> {
        let mut attr_filters: Vec<Condition> = Vec::new();
        if let Some(b) = &self.brand {
            // Filter → Condition（qdrant-client 1.18 提供 From<Filter> for Condition）
            attr_filters.push(
                Filter::should(vec![
                    Condition::matches("brand", MatchValue::Keyword(b.clone())),
                    Condition::is_empty("brand"),
                ])
                .into(),
            );
        }
        if let Some(d) = &self.dev_type {
            attr_filters.push(
                Filter::should(vec![
                    Condition::matches("dev_type", MatchValue::Keyword(d.clone())),
                    Condition::is_empty("dev_type"),
                ])
                .into(),
            );
        }
        if let Some(m) = &self.model {
            attr_filters.push(
                Filter::should(vec![
                    Condition::matches("model", MatchValue::Keyword(m.clone())),
                    Condition::is_empty("model"),
                ])
                .into(),
            );
        }
        if let Some(t) = &self.doc_type {
            attr_filters.push(
                Filter::should(vec![
                    Condition::matches("doc_type", MatchValue::Keyword(t.clone())),
                    Condition::is_empty("doc_type"),
                ])
                .into(),
            );
        }
        if attr_filters.is_empty() {
            None
        } else {
            Some(Filter::must(attr_filters))
        }
    }
}

pub struct KnowledgeVectorStore {
    client: Qdrant,
}

impl KnowledgeVectorStore {
    /// 连接 Qdrant（gRPC，默认 :6334）。api_key 为空则不鉴权。
    pub fn new(url: &str, api_key: Option<&str>) -> Result<Self> {
        let mut builder = Qdrant::from_url(url);
        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            builder = builder.api_key(key.to_string());
        }
        let client = builder.build().map_err(map_err)?;
        Ok(Self { client })
    }

    /// 带过滤的向量检索（下推 Qdrant Filter）
    pub async fn search_filtered(
        &self,
        collection: &str,
        embedding: &[f32],
        top_k: usize,
        filter: Option<KbFilter>,
    ) -> Result<Vec<SearchResult>> {
        let mut builder = SearchPointsBuilder::new(collection, embedding.to_vec(), top_k as u64)
            .with_payload(true);
        if let Some(f) = filter.and_then(|f| f.to_qdrant()) {
            builder = builder.filter(f);
        }
        let resp = self.client.search_points(builder).await.map_err(map_err)?;
        Ok(resp
            .result
            .into_iter()
            .map(|scored| {
                let id = scored
                    .id
                    .as_ref()
                    .and_then(|pid| match &pid.point_id_options {
                        Some(PointIdOptions::Uuid(s)) => Some(s.clone()),
                        Some(PointIdOptions::Num(n)) => Some(n.to_string()),
                        None => None,
                    })
                    .unwrap_or_default();
                SearchResult {
                    chunk: payload_to_chunk(id, scored.payload),
                    score: scored.score,
                }
            })
            .collect())
    }

    /// 按 document_id 删除该文档全部切片
    pub async fn delete_by_document(&self, collection: &str, document_id: &str) -> Result<()> {
        let filter = Filter::must(vec![Condition::matches(
            "document_id",
            MatchValue::Keyword(document_id.to_string()),
        )]);
        self.client
            .delete_points(
                DeletePointsBuilder::new(collection)
                    .points(filter)
                    .wait(true),
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

#[async_trait]
impl VectorStore for KnowledgeVectorStore {
    async fn create_collection(&self, name: &str, dimensions: usize) -> Result<()> {
        let collections = self.client.list_collections().await.map_err(map_err)?;
        if collections.collections.iter().any(|c| c.name == name) {
            return Ok(());
        }
        self.client
            .create_collection(CreateCollectionBuilder::new(name).vectors_config(
                VectorParamsBuilder::new(dimensions as u64, Distance::Cosine),
            ))
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn delete_collection(&self, name: &str) -> Result<()> {
        self.client.delete_collection(name).await.map_err(map_err)?;
        Ok(())
    }

    /// upsert 时把 metadata 打平到 payload 顶层（便于 keyword filter）
    async fn upsert(&self, collection: &str, chunks: &[Chunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let points: Vec<PointStruct> = chunks
            .iter()
            .map(|chunk| {
                let mut payload_map = serde_json::Map::new();
                payload_map.insert("text".into(), serde_json::Value::String(chunk.text.clone()));
                payload_map.insert(
                    "document_id".into(),
                    serde_json::Value::String(chunk.document_id.clone()),
                );
                for (k, v) in &chunk.metadata {
                    payload_map.insert(k.clone(), serde_json::Value::String(v.clone()));
                }
                let payload =
                    Payload::try_from(serde_json::Value::Object(payload_map)).unwrap_or_default();
                PointStruct::new(chunk.id.clone(), chunk.embedding.clone(), payload)
            })
            .collect();

        self.client
            .upsert_points(UpsertPointsBuilder::new(collection, points).wait(true))
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn delete(&self, collection: &str, ids: &[&str]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let point_ids: Vec<qdrant_client::qdrant::PointId> =
            ids.iter().map(|id| (*id).into()).collect();
        self.client
            .delete_points(
                DeletePointsBuilder::new(collection)
                    .points(PointsIdsList { ids: point_ids })
                    .wait(true),
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn search(
        &self,
        collection: &str,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        self.search_filtered(collection, embedding, top_k, None)
            .await
    }
}

fn map_err(e: qdrant_client::QdrantError) -> RagError {
    RagError::VectorStoreError {
        backend: "qdrant".to_string(),
        message: e.to_string(),
    }
}

/// 从 Qdrant payload 还原 Chunk（payload 顶层 → metadata，剔除 text/document_id）
fn payload_to_chunk(id: String, payload: HashMap<String, qdrant_client::qdrant::Value>) -> Chunk {
    let mut text = String::new();
    let mut document_id = String::new();
    let mut metadata = HashMap::new();
    for (k, v) in payload {
        let s = match &v.kind {
            Some(Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        };
        match (k.as_str(), s) {
            ("text", Some(s)) => text = s,
            ("document_id", Some(s)) => document_id = s,
            (other, Some(s)) => {
                metadata.insert(other.to_string(), s);
            }
            _ => {}
        }
    }
    Chunk {
        id,
        text,
        embedding: Vec::new(),
        metadata,
        document_id,
    }
}
