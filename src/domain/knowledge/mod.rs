//! 知识库管理模块 — 封装文档检索、上传、删除、反馈与 FAQ 自动学习
//!
//! 核心职责：
//! - 通过 Dify 知识库 API 进行语义检索（含本地厂商/设备类型过滤）
//! - 文档上传到 Dify（自动切片 + Embedding）并同步元数据到 PostgreSQL
//! - 从完整对话中用 LLM 提取 FAQ，去重后写入知识库（持续学习闭环）
//! - 记录用户反馈（点赞/点踩），用于知识质量评估
//!
//! ## 检索流程
//!
//! ```text
//! 用户查询 → Dify retrieve API（语义检索 + reranking）
//!                 ↓
//!           本地过滤（brand / dev_type 匹配）
//!                 ↓
//!           转换为 DeviceMeta 列表返回
//! ```
//!
//! ## FAQ 学习流程（两阶段，前端审查）
//!
//! ```text
//! 完整对话 → 预处理(清洗"请输入继续"等提示) → 超长则 LLM 压缩
//!              ↓
//!        LLM 提取多组 FAQ 候选（命令规则对齐、每条 ≤1000 字）
//!              ↓
//!        批量查 Dify 重名 → 返回前端审查（标记 duplicate）
//!              ↓
//!        用户勾选 / 不满意则定向重生成 → 提交写入
//!              ↓
//!        写入 Dify + PG 映射表（重名则删旧重建）
//! ```
//!
//! 实现按职责拆到子模块：[`search`](self::search)、[`document`](self::document)、
//! [`faq`](self::faq)、[`compress`](self::compress)；FAQ 提取的纯函数辅助在
//! [`faq_helpers`]。`KnowledgeManager` 的方法分散在各子模块的 `impl` 块中。

pub(crate) mod backend;
pub(crate) mod chunk_store;
mod compress;
pub mod dify_client;
pub(crate) mod document_store;
pub(crate) mod embedding;
mod faq;
pub(crate) mod faq_helpers;
pub(crate) mod kb_instance_store;
pub(crate) mod qdrant_store;
pub(crate) mod uuid_chunker;

use adk_rag::vectorstore::VectorStore;
use crate::config::AppConfig;
use crate::domain::knowledge::chunk_store::ChunkStore;
use crate::domain::knowledge::document_store::DocumentStore;
use crate::domain::knowledge::kb_instance_store::KbInstanceStore;
use crate::error::AppError;
use crate::model_provider::crypto::AesCodec;
use crate::model_provider::store::ModelProviderStore;
use std::sync::Arc;

/// 知识库管理器 — 多 provider 路由（按 kb_instance_id 分发到 Dify/内置 provider）。
///
/// 持有：知识库实例存储、模型供应商（解析 embedding）、内置文档/分段存储、Qdrant 连接、
/// provider 缓存（instance_id -> Arc<dyn KnowledgeProvider>）。FAQ 学习与文档操作统一走 provider。
pub struct KnowledgeManager {
    kb_instance_store: Arc<KbInstanceStore>,
    model_store: Arc<ModelProviderStore>,
    document_store: Arc<DocumentStore>,
    chunk_store: Arc<ChunkStore>,
    codec: AesCodec,
    qdrant_url: String,
    qdrant_api_key: String,
    providers: dashmap::DashMap<String, Arc<dyn backend::KnowledgeProvider>>,
}

/// FAQ 候选条目 — 由 LLM 从会话中提取，返回前端供用户审查后勾选写入
///
/// - `title`：功能意图标题（10字以内，不含厂商/设备类型前缀）
/// - `content`：标准化命令文档正文（命令说明/命令格式/参数说明/配置示例/回退命令/注意事项），控制在 1000 字以内
/// - `duplicate`：是否与知识库中已有文档重名（`brand_dev_type_title`），用于前端提示
/// - `char_count`：content 字符数，前端展示压缩效果
#[derive(Debug, Clone, serde::Serialize)]
pub struct FaqCandidate {
    pub title: String,
    pub content: String,
    pub duplicate: bool,
    pub char_count: usize,
}

/// 会话预处理后允许送入 LLM 的最大字符数（超出则先压缩摘要，防止上下文过长导致模型报错）
const MAX_CONVERSATION_CHARS: usize = 8000;
/// 单条 FAQ content 的目标字数上限（prompt 强约束 + 后端校验）
const MAX_FAQ_CONTENT_CHARS: usize = 1000;

impl KnowledgeManager {
    pub fn new(
        config: Arc<AppConfig>,
        kb_instance_store: Arc<KbInstanceStore>,
        model_store: Arc<ModelProviderStore>,
        document_store: Arc<DocumentStore>,
        chunk_store: Arc<ChunkStore>,
        codec: AesCodec,
    ) -> Self {
        KnowledgeManager {
            kb_instance_store,
            model_store,
            document_store,
            chunk_store,
            codec,
            qdrant_url: config.kb.qdrant_url.clone(),
            qdrant_api_key: config.kb.qdrant_api_key.clone(),
            providers: dashmap::DashMap::new(),
        }
    }

    /// 暴露 kb_instance_store 供 server CRUD 知识库实例
    pub fn kb_instance_store(&self) -> &KbInstanceStore {
        &self.kb_instance_store
    }

    /// 暴露 codec 供 server 加解密实例 config（create/update 时加密 secret）
    pub fn codec(&self) -> &AesCodec {
        &self.codec
    }

    // ===== 多 provider 路由（按 kb_instance_id 分发到对应 provider） =====

    /// 取/构造 instance 的 provider（缓存命中或现场构造）。
    async fn provider_for(
        &self,
        instance_id: &str,
    ) -> Result<Arc<dyn backend::KnowledgeProvider>, AppError> {
        if let Some(p) = self.providers.get(instance_id) {
            return Ok(p.clone());
        }
        let inst = self
            .kb_instance_store
            .get_enabled(instance_id)
            .await?
            .ok_or_else(|| {
                AppError::BusinessError(format!("知识库实例 {} 不存在或未启用", instance_id))
            })?;
        let p = backend::build_provider(
            &inst,
            &self.model_store,
            &self.qdrant_url,
            &self.qdrant_api_key,
            self.document_store.clone(),
            self.chunk_store.clone(),
            &self.codec,
        )
        .await?;
        self.providers.insert(instance_id.to_string(), p.clone());
        Ok(p)
    }

    /// 实例配置变更后清缓存（下次访问时重建 provider）
    pub fn invalidate_provider(&self, instance_id: &str) {
        self.providers.remove(instance_id);
    }

    /// 清理内置实例的 Qdrant 向量集合（仅 provider_kind=Builtin 有意义）。
    ///
    /// 在 kb_instance 删除后调用：PG 的 kb_documents/kb_chunks 已被 CASCADE 删除，
    /// 但 Qdrant collection 不会自动消失，需显式 drop。Dify 类型无本地向量，无需调用。
    /// 失败仅告警不阻断——向量残留可由运维手动清理，不影响已解绑的助手/会话。
    pub async fn purge_qdrant_collection(&self, instance_id: &str) {
        let collection = format!("kb_{}", instance_id.replace('-', "_"));
        let api_key = if self.qdrant_api_key.is_empty() {
            None
        } else {
            Some(self.qdrant_api_key.as_str())
        };
        match qdrant_store::KnowledgeVectorStore::new(&self.qdrant_url, api_key) {
            Ok(vs) => match vs.delete_collection(&collection).await {
                Ok(()) => tracing::info!(
                    "[KnowledgeManager] 已删除 Qdrant collection {}",
                    collection
                ),
                Err(e) => tracing::warn!(
                    "[KnowledgeManager] 删除 Qdrant collection {} 失败(向量残留，可手动清理): {}",
                    collection,
                    e
                ),
            },
            Err(e) => tracing::warn!(
                "[KnowledgeManager] 构造 Qdrant 客户端失败，跳过向量清理: {}",
                e
            ),
        }
    }

    /// 第一个启用的知识库实例 id（FAQ 学习未指定实例时的默认写入目标）
    pub async fn first_enabled_instance_id(&self) -> Result<String, AppError> {
        let insts = self.kb_instance_store.list_all().await?;
        insts
            .into_iter()
            .find(|i| i.status == 1)
            .map(|i| i.id)
            .ok_or_else(|| {
                AppError::BusinessError("没有启用的知识库实例，请先在「知识库管理」创建".into())
            })
    }

    pub async fn search_instance(
        &self,
        instance_id: &str,
        q: backend::KbQuery,
    ) -> Result<Vec<backend::KbDoc>, AppError> {
        let p = self.provider_for(instance_id).await?;
        p.search(&q).await
    }

    pub async fn upload_to_instance(
        &self,
        instance_id: &str,
        input: backend::KbDocInput,
    ) -> Result<String, AppError> {
        let p = self.provider_for(instance_id).await?;
        p.upload(&input).await
    }

    /// 整篇上传（FAQ 不切片）：dify 走整篇 create；内置整篇 embed。
    pub async fn upload_whole_to_instance(
        &self,
        instance_id: &str,
        input: backend::KbDocInput,
    ) -> Result<String, AppError> {
        let p = self.provider_for(instance_id).await?;
        p.upload_whole(&input).await
    }

    pub async fn list_instance(
        &self,
        instance_id: &str,
        f: backend::KbListFilter,
    ) -> Result<backend::KbDocPage, AppError> {
        let p = self.provider_for(instance_id).await?;
        p.list(&f).await
    }

    pub async fn delete_instance(&self, instance_id: &str, doc_id: &str) -> Result<(), AppError> {
        let p = self.provider_for(instance_id).await?;
        p.delete(doc_id).await
    }

    pub async fn segments_instance(
        &self,
        instance_id: &str,
        doc_id: &str,
    ) -> Result<Vec<backend::KbSegment>, AppError> {
        let p = self.provider_for(instance_id).await?;
        p.segments(doc_id).await
    }

    pub async fn health_instance(&self, instance_id: &str) -> Result<(), AppError> {
        let p = self.provider_for(instance_id).await?;
        p.health().await
    }
}
