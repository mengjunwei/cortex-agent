//! 知识库 Provider 抽象层 — 统一 Dify（外挂）与内置（adk-rag + Qdrant）两类后端。
//!
//! 设计目标（差异化整合）：不同 provider 的配置字段、底层存储、API 调用完全不同，
//! 但通过 [`KnowledgeProvider`] trait 与统一领域模型（[`KbDoc`]/[`KbQuery`] 等）抹平差异。
//! 上层（KnowledgeManager 路由器、search_kb 工具、GraphQL 接口）只认 `kb_instance_id`，
//! 不感知底层是 Dify 还是内置。

pub mod schema;

mod builtin;
mod dify;

use std::sync::Arc;

use crate::domain::knowledge::chunk_store::ChunkStore;
use crate::domain::knowledge::document_store::DocumentStore;
use crate::domain::knowledge::kb_instance_store::KbInstance;
use crate::error::AppError;
use crate::model_provider::crypto::AesCodec;
use crate::model_provider::store::ModelProviderStore;

/// Provider 类型（与 `kb_instances.provider_kind` 对应，SMALLINT 存储）
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Dify = 1,
    Builtin = 2,
}

impl ProviderKind {
    pub fn from_i16(v: i16) -> Option<Self> {
        match v {
            1 => Some(Self::Dify),
            2 => Some(Self::Builtin),
            _ => None,
        }
    }
}

// ========== 统一领域模型（不绑定具体 provider） ==========

/// 检索入参
#[derive(Debug, Clone)]
pub struct KbQuery {
    pub query: String,
    pub brand: Option<String>,
    pub dev_type: Option<String>,
    /// 设备型号（如 S5300），可选；None=不按型号过滤
    pub model: Option<String>,
    pub topk: Option<usize>,
}

/// 文档（统一返回结构，dify/内置都映射成它）
#[derive(Debug, Clone, serde::Serialize)]
pub struct KbDoc {
    pub id: String,
    pub title: String,
    pub brand: String,
    pub dev_type: String,
    /// 设备型号（如 S5300），无则空串
    pub model: String,
    pub content: String,
    pub source: String,
    pub word_count: i64,
    /// 1=手册 2=FAQ
    pub doc_type: i16,
    pub hit_count: Option<i64>,
}

/// 分段
#[derive(Debug, Clone, serde::Serialize)]
pub struct KbSegment {
    pub index: i32,
    pub content: String,
    pub word_count: i64,
}

/// 上传文档入参
#[derive(Debug, Clone)]
pub struct KbDocInput {
    /// 可选属性：厂商（空=不设该 metadata，检索时 is_empty 命中）
    pub brand: String,
    /// 可选属性：设备类型（空=不设该 metadata，检索时 is_empty 命中）
    pub dev_type: String,
    /// 可选属性：设备型号，如 S5300（空=不设该 metadata，检索时 is_empty 命中）
    pub model: String,
    pub firmware_ver: String,
    pub title: String,
    pub content: String,
    pub user_role: String,
}

/// 文档列表过滤
#[derive(Debug, Clone, Default)]
pub struct KbListFilter {
    pub page: u32,
    pub limit: u32,
    pub brand: Option<String>,
    pub dev_type: Option<String>,
    pub keyword: Option<String>,
}

/// 文档分页结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct KbDocPage {
    pub data: Vec<KbDoc>,
    pub total: i64,
    pub page: u32,
    pub limit: u32,
}

// ========== Provider trait（能力契约） ==========

/// 知识库后端能力契约。`DifyProvider` / `BuiltinProvider` 各自实现。
#[async_trait::async_trait]
pub trait KnowledgeProvider: Send + Sync {
    /// 连通性探测（界面"测试"按钮用）
    async fn health(&self) -> Result<(), AppError>;
    async fn search(&self, q: &KbQuery) -> Result<Vec<KbDoc>, AppError>;
    /// 上传文档，返回 doc_id
    async fn upload(&self, input: &KbDocInput) -> Result<String, AppError>;
    async fn delete(&self, doc_id: &str) -> Result<(), AppError>;
    async fn list(&self, f: &KbListFilter) -> Result<KbDocPage, AppError>;
    async fn segments(&self, doc_id: &str) -> Result<Vec<KbSegment>, AppError>;

    /// 整篇上传（不切片，FAQ 等短结构化文档用）。
    ///
    /// 默认实现等同于 [`upload`](Self::upload)（切片）；内置 provider override 为整篇 embed。
    async fn upload_whole(&self, input: &KbDocInput) -> Result<String, AppError> {
        self.upload(input).await
    }
}

/// 按 kb_instance 构造对应的 provider（解密 config secret 后分发）。
///
/// `codec` 用于解密 config 中的 secret 字段（如 Dify api_key）为明文，供 provider 构造。
pub async fn build_provider(
    inst: &KbInstance,
    model_store: &ModelProviderStore,
    qdrant_url: &str,
    qdrant_api_key: &str,
    documents: Arc<DocumentStore>,
    chunks: Arc<ChunkStore>,
    codec: &AesCodec,
) -> Result<Arc<dyn KnowledgeProvider>, AppError> {
    let kind = ProviderKind::from_i16(inst.provider_kind).ok_or_else(|| {
        AppError::BusinessError(format!("未知 provider_kind: {}", inst.provider_kind))
    })?;
    // 解密 config 中的 secret 字段（明文供 provider 构造用）
    let cfg_plain = schema::decrypt_secret_fields(kind, &inst.config_value(), codec, false);
    match kind {
        ProviderKind::Dify => Ok(Arc::new(dify::DifyProvider::new(&cfg_plain)?)),
        ProviderKind::Builtin => Ok(Arc::new(
            builtin::BuiltinProvider::new(
                &inst.id,
                &cfg_plain,
                model_store,
                qdrant_url,
                qdrant_api_key,
                documents,
                chunks,
            )
            .await?,
        )),
    }
}
