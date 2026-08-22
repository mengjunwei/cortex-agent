//! Dify 知识库 Provider — 包装现有 [`DifyClient`]，实现 [`KnowledgeProvider`]。
//!
//! 文档真相留在 Dify 服务：list/segments/upload/delete 全部实时调 Dify API，不入本地 PG
//! （这是与内置 provider 的关键区别）。检索结果映射为统一 [`KbDoc`]。
//!
//! 上传时文档名编码为 `brand_dev_type_title`（与历史 list 解析对齐），并把 brand/dev_type/
//! title 写入 Dify metadata，便于 Dify 侧检索过滤。

use std::sync::Arc;

use crate::domain::knowledge::backend::schema::{get_str, get_u64};
use crate::domain::knowledge::backend::{
    KbDoc, KbDocInput, KbDocPage, KbListFilter, KbQuery, KbSegment, KnowledgeProvider,
};
use crate::domain::knowledge::dify_client::DifyClient;
use crate::domain::knowledge::dify_client::DifyConfig;
use crate::error::AppError;

pub struct DifyProvider {
    client: Arc<DifyClient>,
    top_k: usize,
}

impl DifyProvider {
    /// 从 kb_instance 的 config JSON（api_key 已解密为明文）构造。
    pub fn new(config: &serde_json::Value) -> Result<Self, AppError> {
        let base_url = get_str(config, "base_url")
            .ok_or_else(|| AppError::BusinessError("Dify 知识库缺少 base_url 配置".into()))?;
        let api_key = get_str(config, "api_key")
            .ok_or_else(|| AppError::BusinessError("Dify 知识库缺少 api_key 配置".into()))?;
        let dataset_id = get_str(config, "dataset_id")
            .ok_or_else(|| AppError::BusinessError("Dify 知识库缺少 dataset_id 配置".into()))?;
        let top_k = get_u64(config, "top_k").unwrap_or(5) as usize;

        let dify_cfg = DifyConfig {
            base_url,
            api_key,
            dataset_id,
            top_k,
        };
        let client = DifyClient::new(&dify_cfg)?;
        Ok(Self {
            client: Arc::new(client),
            top_k,
        })
    }
}

#[async_trait::async_trait]
impl KnowledgeProvider for DifyProvider {
    async fn health(&self) -> Result<(), AppError> {
        // 试调 list_documents(1,1) 验证连通与凭证
        self.client.list_documents(1, 1, None).await.map(|_| ())
    }

    async fn search(&self, q: &KbQuery) -> Result<Vec<KbDoc>, AppError> {
        let records = self
            .client
            .retrieve(&q.query, Some(q.topk.unwrap_or(self.top_k)))
            .await?;
        let docs = records
            .into_iter()
            .map(|r| KbDoc {
                id: r.segment.document_id.clone(),
                title: r
                    .segment
                    .document
                    .as_ref()
                    .map(|d| d.name.clone())
                    .unwrap_or_default(),
                brand: String::new(),
                dev_type: String::new(),
                model: String::new(),
                content: r.segment.content.clone(),
                source: "dify".to_string(),
                word_count: r.segment.content.chars().count() as i64,
                doc_type: 1,
                hit_count: None,
            })
            .collect();
        Ok(docs)
    }

    async fn upload(&self, input: &KbDocInput) -> Result<String, AppError> {
        // 文档名直接用 title（不再编码厂商/设备类型）
        let name = if input.title.is_empty() {
            "未命名文档".to_string()
        } else {
            input.title.clone()
        };
        // 有原始文件 → 交给 Dify 自带解析（create_by_file）；否则按文本创建
        let resp = if let Some(f) = input.file.as_ref() {
            self.client
                .create_document_by_file(&name, &f.name, &f.mime, f.bytes.clone())
                .await?
        } else {
            self.client
                .create_document_by_text(&name, &input.content)
                .await?
        };
        let doc_id = resp.document.id;
        // title 写入 Dify metadata（失败不阻断，仅告警）
        if let Err(e) = self
            .client
            .set_document_metadata(&doc_id, &[("title", input.title.as_str())])
            .await
        {
            tracing::warn!("[DifyProvider] 设置 metadata 失败(不阻断): {e}");
        }
        Ok(doc_id)
    }

    async fn delete(&self, doc_id: &str) -> Result<(), AppError> {
        self.client.delete_document(doc_id).await
    }

    async fn list(&self, f: &KbListFilter) -> Result<KbDocPage, AppError> {
        let resp = self
            .client
            .list_documents(f.page, f.limit, f.keyword.as_deref())
            .await?;
        let data = resp
            .data
            .into_iter()
            .map(|d| KbDoc {
                id: d.id,
                title: d.name,
                brand: String::new(),
                dev_type: String::new(),
                model: String::new(),
                content: String::new(),
                source: "dify".to_string(),
                word_count: d.word_count.unwrap_or(0) as i64,
                doc_type: 1,
                hit_count: d.hit_count.map(|h| h as i64),
            })
            .collect();
        Ok(KbDocPage {
            data,
            total: resp.total as i64,
            page: f.page,
            limit: f.limit,
        })
    }

    async fn segments(&self, doc_id: &str) -> Result<Vec<KbSegment>, AppError> {
        let resp = self.client.list_segments(doc_id, 256).await?;
        let segs = resp
            .data
            .into_iter()
            .enumerate()
            .map(|(i, s)| KbSegment {
                index: i as i32,
                content: s.content,
                word_count: s.word_count.unwrap_or(0) as i64,
            })
            .collect();
        Ok(segs)
    }
}
