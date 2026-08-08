//! 内置知识库 Provider — adk-rag 编排 + Qdrant 向量库。
//!
//! 文档真相在本地：切片→embedding→存 Qdrant + PG 元数据（kb_documents/kb_chunks）。
//! 每个内置实例对应一个 Qdrant collection（名 `kb_<instance_id>`），互不干扰。
//! 检索走 Qdrant 向量相似度 + payload filter（brand/dev_type 下推）。

use std::collections::HashMap;
use std::sync::Arc;

use adk_rag::chunking::{Chunker, MarkdownChunker};
use adk_rag::document::Document;
use adk_rag::embedding::EmbeddingProvider;
use adk_rag::vectorstore::VectorStore;

use crate::domain::knowledge::backend::schema::{get_str, get_u64};
use crate::domain::knowledge::backend::{
    KbDoc, KbDocInput, KbDocPage, KbListFilter, KbQuery, KbSegment, KnowledgeProvider,
};
use crate::domain::knowledge::chunk_store::ChunkStore;
use crate::domain::knowledge::document_store::DocumentStore;
use crate::domain::knowledge::embedding::OpenAiCompatibleEmbeddingProvider;
use crate::domain::knowledge::qdrant_store::{KbFilter, KnowledgeVectorStore};
use crate::domain::knowledge::uuid_chunker::UuidChunker;
use crate::error::AppError;
use crate::infra::store_base::new_id;
use crate::model_provider::store::ModelProviderStore;

pub struct BuiltinProvider {
    instance_id: String,
    embedding: Arc<OpenAiCompatibleEmbeddingProvider>,
    store: Arc<KnowledgeVectorStore>,
    chunker: Arc<dyn Chunker>,
    collection: String,
    top_k: usize,
    similarity_threshold: f32,
    documents: Arc<DocumentStore>,
    chunks: Arc<ChunkStore>,
}

impl BuiltinProvider {
    /// 从 kb_instance 配置构造（建 Qdrant collection）。
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        instance_id: &str,
        config: &serde_json::Value,
        model_store: &ModelProviderStore,
        qdrant_url: &str,
        qdrant_api_key: &str,
        documents: Arc<DocumentStore>,
        chunks: Arc<ChunkStore>,
    ) -> Result<Self, AppError> {
        let emb_model_id = get_str(config, "embedding_model_id");
        let emb = model_store
            .resolve_embedding_model(emb_model_id.as_deref())
            .map_err(|e| AppError::BusinessError(format!("embedding 模型解析失败: {e}")))?;
        let embedding = Arc::new(OpenAiCompatibleEmbeddingProvider::new(
            emb.base_url,
            emb.api_key,
            emb.model,
            emb.dimensions,
        ));

        let store = Arc::new(
            KnowledgeVectorStore::new(
                qdrant_url,
                if qdrant_api_key.is_empty() {
                    None
                } else {
                    Some(qdrant_api_key)
                },
            )
            .map_err(|e| AppError::BusinessError(format!("Qdrant 初始化失败: {e}")))?,
        );

        let chunk_size = get_u64(config, "chunk_size").unwrap_or(1024) as usize;
        let chunk_overlap = get_u64(config, "chunk_overlap").unwrap_or(100) as usize;
        let chunker: Arc<dyn Chunker> = Arc::new(UuidChunker::new(Arc::new(MarkdownChunker::new(
            chunk_size,
            chunk_overlap,
        ))));

        let top_k = get_u64(config, "top_k").unwrap_or(6) as usize;
        let similarity_threshold = config
            .get("similarity_threshold")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32)
            .unwrap_or(0.35);

        // collection 名：kb_<instance_id 去连字符>
        let collection = format!("kb_{}", instance_id.replace('-', "_"));

        let provider = Self {
            instance_id: instance_id.to_string(),
            embedding,
            store,
            chunker,
            collection,
            top_k,
            similarity_threshold,
            documents,
            chunks,
        };

        // 建 collection（按 embedding 维度，幂等）
        provider
            .store
            .create_collection(&provider.collection, provider.embedding.dimensions())
            .await
            .map_err(|e| AppError::BusinessError(format!("Qdrant 建集合失败: {e}")))?;

        Ok(provider)
    }
}

#[async_trait::async_trait]
impl KnowledgeProvider for BuiltinProvider {
    async fn health(&self) -> Result<(), AppError> {
        // create_collection 幂等，借此验证 Qdrant 连通
        self.store
            .create_collection(&self.collection, self.embedding.dimensions())
            .await
            .map_err(|e| AppError::BusinessError(format!("Qdrant 连通失败: {e}")))
    }

    async fn search(&self, q: &KbQuery) -> Result<Vec<KbDoc>, AppError> {
        let emb = self
            .embedding
            .embed(&q.query)
            .await
            .map_err(|e| AppError::BusinessError(format!("query embedding 失败: {e}")))?;
        // brand/dev_type 过滤：「填了要遵守，不填都符合」（空字符串视同不过滤，详见 KbFilter::to_qdrant）
        let filter = KbFilter {
            brand: q.brand.clone().filter(|s| !s.is_empty()),
            dev_type: q.dev_type.clone().filter(|s| !s.is_empty()),
            model: q.model.clone().filter(|s| !s.is_empty()),
            doc_type: None,
        };
        let results = self
            .store
            .search_filtered(
                &self.collection,
                &emb,
                q.topk.unwrap_or(self.top_k),
                Some(filter),
            )
            .await
            .map_err(|e| AppError::BusinessError(format!("检索失败: {e}")))?;
        let docs = results
            .into_iter()
            .filter(|r| r.score >= self.similarity_threshold)
            .map(|r| {
                let m = &r.chunk.metadata;
                KbDoc {
                    id: r.chunk.document_id.clone(),
                    title: m.get("title").cloned().unwrap_or_default(),
                    brand: m.get("brand").cloned().unwrap_or_default(),
                    dev_type: m.get("dev_type").cloned().unwrap_or_default(),
                    model: m.get("model").cloned().unwrap_or_default(),
                    content: r.chunk.text.clone(),
                    source: m
                        .get("source")
                        .cloned()
                        .unwrap_or_else(|| "builtin".to_string()),
                    word_count: r.chunk.text.chars().count() as i64,
                    doc_type: 1,
                    hit_count: None,
                }
            })
            .collect();
        Ok(docs)
    }

    async fn upload(&self, input: &KbDocInput) -> Result<String, AppError> {
        let doc_id = new_id();
        let mut metadata = HashMap::new();
        metadata.insert("firmware_ver".to_string(), input.firmware_ver.clone());
        metadata.insert("title".to_string(), input.title.clone());
        metadata.insert("doc_type".to_string(), "manual".to_string());
        metadata.insert("source".to_string(), "manual".to_string());
        // brand/dev_type 仅在非空时写入 payload（空则不存，让检索时 is_empty 命中）
        if !input.brand.is_empty() {
            metadata.insert("brand".to_string(), input.brand.clone());
        }
        if !input.dev_type.is_empty() {
            metadata.insert("dev_type".to_string(), input.dev_type.clone());
        }
        if !input.model.is_empty() {
            metadata.insert("model".to_string(), input.model.clone());
        }
        let document = Document {
            id: doc_id.clone(),
            text: input.content.clone(),
            metadata,
            source_uri: None,
        };

        // 切片（同步）→ 批量 embedding → 填回 chunk
        let mut chunks = self.chunker.chunk(&document);
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let embeddings = self
            .embedding
            .embed_batch(&texts)
            .await
            .map_err(|e| AppError::BusinessError(format!("文档 embedding 失败: {e}")))?;
        for (c, e) in chunks.iter_mut().zip(embeddings) {
            c.embedding = e;
        }

        // upsert Qdrant（payload 打平 brand/dev_type/title/document_id）
        self.store
            .upsert(&self.collection, &chunks)
            .await
            .map_err(|e| AppError::BusinessError(format!("Qdrant upsert 失败: {e}")))?;

        let chunk_count = chunks.len() as i32;
        let word_count = input.content.chars().count() as i32;

        // PG 文档元数据（id 与 Qdrant document_id 一致）
        self.documents
            .insert(
                &doc_id,
                &self.instance_id,
                1,
                &input.brand,
                &input.dev_type,
                &input.model,
                &input.firmware_ver,
                &input.title,
                "manual",
                word_count,
                chunk_count,
                &input.user_role,
            )
            .await?;

        // PG 分段预览
        let rows: Vec<(String, String, i32, String, i32, String)> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    c.id.clone(),
                    doc_id.clone(),
                    i as i32,
                    c.text.clone(),
                    c.text.chars().count() as i32,
                    c.metadata.get("header_path").cloned().unwrap_or_default(),
                )
            })
            .collect();
        self.chunks.insert_batch(&rows).await?;

        tracing::info!(
            "[BuiltinProvider] 文档入库: instance={} doc={} chunks={}",
            self.instance_id,
            doc_id,
            chunk_count
        );
        Ok(doc_id)
    }

    /// 整篇上传（FAQ 不切片）：整篇 embed 成一个向量，绕过 chunker。
    ///
    /// FAQ 是 ≤1000 字的结构化短文档，整篇一个向量召回更准（整体语义），不按 ## 标题切碎。
    async fn upload_whole(&self, input: &KbDocInput) -> Result<String, AppError> {
        let doc_id = new_id();
        let emb = self
            .embedding
            .embed(&input.content)
            .await
            .map_err(|e| AppError::BusinessError(format!("FAQ embedding 失败: {e}")))?;
        let mut metadata = HashMap::new();
        metadata.insert("title".to_string(), input.title.clone());
        metadata.insert("doc_type".to_string(), "faq".to_string());
        metadata.insert("source".to_string(), "faq".to_string());
        // brand/dev_type 仅在非空时写入 payload（空则不存，让检索时 is_empty 命中）
        if !input.brand.is_empty() {
            metadata.insert("brand".to_string(), input.brand.clone());
        }
        if !input.dev_type.is_empty() {
            metadata.insert("dev_type".to_string(), input.dev_type.clone());
        }
        if !input.model.is_empty() {
            metadata.insert("model".to_string(), input.model.clone());
        }
        let chunk = adk_rag::document::Chunk {
            id: new_id(),
            text: input.content.clone(),
            embedding: emb,
            metadata,
            document_id: doc_id.clone(),
        };
        self.store
            .upsert(&self.collection, std::slice::from_ref(&chunk))
            .await
            .map_err(|e| AppError::BusinessError(format!("Qdrant upsert 失败: {e}")))?;
        let word_count = input.content.chars().count() as i32;
        self.documents
            .insert(
                &doc_id,
                &self.instance_id,
                2, // doc_type=2 FAQ
                &input.brand,
                &input.dev_type,
                &input.model,
                &input.firmware_ver,
                &input.title,
                "faq",
                word_count,
                1,
                &input.user_role,
            )
            .await?;
        // PG 分段预览（整篇一段）
        self.chunks
            .insert_batch(&[(
                chunk.id.clone(),
                doc_id.clone(),
                0,
                chunk.text.clone(),
                word_count,
                String::new(),
            )])
            .await?;
        tracing::info!(
            "[BuiltinProvider] FAQ 整篇入库(不切片): instance={} doc={}",
            self.instance_id,
            doc_id
        );
        Ok(doc_id)
    }

    async fn delete(&self, doc_id: &str) -> Result<(), AppError> {
        // Qdrant 按 document_id 删全部切片
        self.store
            .delete_by_document(&self.collection, doc_id)
            .await
            .map_err(|e| AppError::BusinessError(format!("Qdrant 删除失败: {e}")))?;
        // PG 删文档（kb_chunks ON DELETE CASCADE 联动删分段）
        if let Err(e) = self.documents.delete(doc_id).await {
            tracing::warn!("[BuiltinProvider] PG 删除文档失败(不阻断): {e}");
        }
        Ok(())
    }

    async fn list(&self, f: &KbListFilter) -> Result<KbDocPage, AppError> {
        let (docs, total) = self
            .documents
            .list(
                &self.instance_id,
                f.page,
                f.limit,
                f.brand.as_deref(),
                f.dev_type.as_deref(),
                f.keyword.as_deref(),
            )
            .await?;
        let data = docs
            .into_iter()
            .map(|d| KbDoc {
                id: d.id,
                title: d.title,
                brand: d.brand,
                dev_type: d.dev_type,
                model: d.model,
                content: String::new(),
                source: d.source,
                word_count: d.word_count as i64,
                doc_type: d.doc_type,
                hit_count: None,
            })
            .collect();
        Ok(KbDocPage {
            data,
            total,
            page: f.page,
            limit: f.limit,
        })
    }

    async fn segments(&self, doc_id: &str) -> Result<Vec<KbSegment>, AppError> {
        let rows = self.chunks.list_by_document(doc_id).await?;
        Ok(rows
            .into_iter()
            .map(|c| KbSegment {
                index: c.chunk_index,
                content: c.content,
                word_count: c.word_count as i64,
            })
            .collect())
    }
}
