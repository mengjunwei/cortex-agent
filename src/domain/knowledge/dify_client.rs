//! Dify 知识库 API 客户端 — 封装文档检索、上传、删除、列表等操作
//!
//! 核心功能：
//! - **语义检索**（`retrieve`）：调用 Dify `/datasets/{id}/retrieve` 接口，使用知识库配置的检索模型（含 reranking/weights）
//! - **文档创建**（`create_document_by_text`）：通过文本创建文档，Dify 自动完成切片 + Embedding
//! - **Metadata 管理**（`set_document_metadata`）：设置文档元数据（厂商、设备类型等），支持自动创建字段
//! - **文档管理**（`list_documents` / `delete_document` / `list_segments`）
//!
//! ## retrieval_model 缓存
//!
//! 检索模型配置（`retrieval_model_dict`）从 Dify 知识库配置中获取，缓存 10 秒后自动刷新，
//! 确保 search_method、reranking、score_threshold 等参数与 Dify 后台一致。

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::error::AppError;

/// Dify 检索结果中的一条记录
#[derive(Debug, Clone, Deserialize)]
pub struct DifyRecord {
    pub segment: DifySegment,
    pub score: Option<f32>,
}

/// Dify 文档分段
#[derive(Debug, Clone, Deserialize)]
pub struct DifySegment {
    pub document_id: String,
    pub content: String,
    pub keywords: Option<Vec<String>>,
    pub document: Option<DifyDocumentInfo>,
}

/// Dify 文档信息（嵌套在 segment 中）
#[derive(Debug, Clone, Deserialize)]
pub struct DifyDocumentInfo {
    pub name: String,
}

/// Dify 创建文档响应
#[derive(Debug, Clone, Deserialize)]
pub struct DifyCreateDocResponse {
    pub document: DifyDocInfo,
    pub batch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DifyDocInfo {
    pub id: String,
}

/// Dify 文档列表项
#[derive(Debug, Clone, Deserialize)]
pub struct DifyDocListItem {
    pub id: String,
    pub name: String,
    pub indexing_status: String,
    pub enabled: bool,
    pub word_count: Option<u32>,
    pub hit_count: Option<u32>,
}

/// Dify 文档列表响应
#[derive(Debug, Clone, Deserialize)]
pub struct DifyDocListResponse {
    pub data: Vec<DifyDocListItem>,
    pub total: u32,
}

/// Dify 文档分段（用于预览）
#[derive(Debug, Clone, Deserialize)]
pub struct DifySegmentItem {
    pub id: String,
    pub content: String,
    pub word_count: Option<u32>,
    pub enabled: Option<bool>,
    pub keywords: Option<Vec<String>>,
}

/// Dify 分段列表响应
#[derive(Debug, Clone, Deserialize)]
pub struct DifySegmentListResponse {
    pub data: Vec<DifySegmentItem>,
}

/// Dify 连接配置（DifyClient 构造用）。
///
/// 实例配置存 DB `kb_instances`（config JSON），不入 `config.toml`；DifyProvider 构造时
/// 从实例 config 解析字段，组装成此结构喂给 [`DifyClient::new`]。
pub struct DifyConfig {
    pub base_url: String,
    pub api_key: String,
    pub dataset_id: String,
    pub top_k: usize,
}

/// Dify 知识库 API 客户端 — 封装所有与 Dify 知识库的 HTTP 交互
///
/// 持有 HTTP 连接池、API 凭证和知识库 ID，所有操作都基于 `dataset_id` 进行。
/// `retrieval_model` 字段缓存知识库检索配置（含 reranking/weights），定期刷新。
pub struct DifyClient {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
    dataset_id: String,
    top_k: usize,
    /// 从 Dify 知识库配置中获取的完整 retrieval_model（含 reranking/weights），定时刷新
    retrieval_model: tokio::sync::RwLock<Option<(serde_json::Value, Instant)>>,
}

/// retrieval_model 缓存有效期（秒）
const RETRIEVAL_MODEL_TTL: u64 = 10;

impl DifyClient {
    /// 创建 Dify 客户端
    ///
    /// # 错误
    /// 返回 `ConfigError` 如果 `api_key` 为空或 HTTP 客户端创建失败
    pub fn new(cfg: &DifyConfig) -> Result<Self, AppError> {
        if cfg.api_key.is_empty() {
            return Err(AppError::ConfigError(
                "Dify API Key 未配置，请在 config.toml [dify] 或环境变量 DIFY_API_KEY 中设置"
                    .to_string(),
            ));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();

        Ok(Self {
            agent,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            dataset_id: cfg.dataset_id.clone(),
            top_k: cfg.top_k,
            retrieval_model: tokio::sync::RwLock::new(None),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get_json<T>(&self, url: String) -> Result<T, AppError>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let agent = self.agent.clone();
        let api_key = self.api_key.clone();
        tokio::task::spawn_blocking(move || {
            let mut resp = agent
                .get(&url)
                .header("Authorization", &format!("Bearer {}", api_key))
                .call()
                .map_err(|e| AppError::NetworkError(format!("Dify GET 请求失败: {}", e)))?;
            resp.body_mut()
                .read_json()
                .map_err(|e| AppError::SerializationError(format!("解析 Dify 响应失败: {}", e)))
        })
        .await
        .map_err(|e| AppError::NetworkError(format!("Dify GET 任务失败: {}", e)))?
    }

    async fn post_json<T>(&self, url: String, body: Value) -> Result<T, AppError>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let agent = self.agent.clone();
        let api_key = self.api_key.clone();
        tokio::task::spawn_blocking(move || {
            let mut resp = agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", api_key))
                .send_json(&body)
                .map_err(|e| AppError::NetworkError(format!("Dify POST 请求失败: {}", e)))?;
            resp.body_mut()
                .read_json()
                .map_err(|e| AppError::SerializationError(format!("解析 Dify 响应失败: {}", e)))
        })
        .await
        .map_err(|e| AppError::NetworkError(format!("Dify POST 任务失败: {}", e)))?
    }

    async fn post_json_ignore(&self, url: String, body: Value) -> Result<(), AppError> {
        let agent = self.agent.clone();
        let api_key = self.api_key.clone();
        tokio::task::spawn_blocking(move || {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", api_key))
                .send_json(&body)
                .map_err(|e| AppError::NetworkError(format!("Dify POST 请求失败: {}", e)))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::NetworkError(format!("Dify POST 任务失败: {}", e)))?
    }

    async fn delete_ignore(&self, url: String) -> Result<(), AppError> {
        let agent = self.agent.clone();
        let api_key = self.api_key.clone();
        tokio::task::spawn_blocking(move || {
            agent
                .delete(&url)
                .header("Authorization", &format!("Bearer {}", api_key))
                .call()
                .map_err(|e| AppError::NetworkError(format!("Dify DELETE 请求失败: {}", e)))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::NetworkError(format!("Dify DELETE 任务失败: {}", e)))?
    }

    /// 从 Dify 知识库获取 retrieval_model_dict（含 reranking/weights 配置）
    /// 缓存 5 分钟，过期自动刷新
    async fn get_retrieval_model(&self) -> Result<serde_json::Value, AppError> {
        // 先读缓存
        {
            let cache = self.retrieval_model.read().await;
            if let Some((model, ts)) = cache.as_ref()
                && ts.elapsed().as_secs() < RETRIEVAL_MODEL_TTL
            {
                return Ok(model.clone());
            }
        }

        // 缓存过期或不存在，重新获取
        #[derive(Deserialize)]
        struct DatasetResp {
            retrieval_model_dict: serde_json::Value,
        }

        let ds: DatasetResp = self
            .get_json(self.url(&format!("/datasets/{}", self.dataset_id)))
            .await?;

        tracing::info!(
            "[Dify] 刷新知识库 retrieval_model: search_method={}, reranking={}, score_threshold={}",
            ds.retrieval_model_dict
                .get("search_method")
                .and_then(|v| v.as_str())
                .unwrap_or("?"),
            ds.retrieval_model_dict
                .get("reranking_enable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            ds.retrieval_model_dict
                .get("score_threshold")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
        );

        // 写缓存
        let mut cache = self.retrieval_model.write().await;
        *cache = Some((ds.retrieval_model_dict.clone(), Instant::now()));

        Ok(ds.retrieval_model_dict)
    }

    /// 检索知识库 — 调用 Dify retrieve API 进行语义检索
    ///
    /// `retrieval_model` 直接从 Dify 知识库配置获取（含 reranking/weights），
    /// 确保与 Dify 后台配置一致。`top_k` 会覆盖知识库配置中的值。
    ///
    /// # 参数
    /// - `query`：检索关键词
    /// - `top_k`：返回条数（可选，默认使用配置值）
    pub async fn retrieve(
        &self,
        query: &str,
        top_k: Option<usize>,
    ) -> Result<Vec<DifyRecord>, AppError> {
        let dataset_id = if self.dataset_id.is_empty() {
            return Err(AppError::ConfigError("Dify dataset_id 未配置".to_string()));
        } else {
            &self.dataset_id
        };

        let k = top_k.unwrap_or(self.top_k);

        // 获取知识库配置的 retrieval_model（含 reranking/weights），覆盖 top_k
        let mut retrieval_model = self.get_retrieval_model().await?;
        if let Some(obj) = retrieval_model.as_object_mut() {
            obj.insert("top_k".to_string(), json!(k));
        }

        let req = json!({
            "query": query,
            "retrieval_model": retrieval_model,
        });

        #[derive(Deserialize)]
        struct RetrieveResp {
            records: Vec<DifyRecord>,
        }

        let resp_body: RetrieveResp = self
            .post_json(self.url(&format!("/datasets/{}/retrieve", dataset_id)), req)
            .await?;

        // 打印检索结果 score
        if !resp_body.records.is_empty() {
            let scores: Vec<String> = resp_body
                .records
                .iter()
                .map(|r| format!("{:.4}", r.score.unwrap_or(0.0)))
                .collect();
            tracing::info!(
                "[Dify] retrieve 结果: query=\"{}\", count={}, scores=[{}]",
                &query,
                resp_body.records.len(),
                scores.join(", ")
            );
        }

        Ok(resp_body.records)
    }

    /// 通过文本创建文档 — Dify 自动完成切片 + Embedding
    ///
    /// 兼容新旧两种 Dify API 端点：
    /// - 新版：`/datasets/{id}/document/create-by-text`
    /// - 旧版：`/datasets/{id}/document/create_by_text`
    ///
    /// 自动尝试两种端点，首个成功的返回结果。使用 `high_quality` 索引模式。
    pub async fn create_document_by_text(
        &self,
        name: &str,
        text: &str,
    ) -> Result<DifyCreateDocResponse, AppError> {
        #[derive(Serialize)]
        struct CreateReq<'a> {
            name: &'a str,
            text: &'a str,
            indexing_technique: &'a str,
            doc_form: &'a str,
            doc_language: &'a str,
            process_rule: ProcessRule,
        }

        #[derive(Serialize)]
        struct ProcessRule {
            mode: &'static str,
            rules: ProcessRules,
        }

        #[derive(Serialize)]
        struct ProcessRules {
            pre_processing_rules: Vec<PreProcessingRule>,
            segmentation: Segmentation,
        }

        #[derive(Serialize)]
        struct PreProcessingRule {
            id: &'static str,
            enabled: bool,
        }

        #[derive(Serialize)]
        struct Segmentation {
            separator: &'static str,
            max_tokens: u32,
            chunk_overlap: u32,
        }

        let req = CreateReq {
            name,
            text,
            indexing_technique: "high_quality",
            doc_form: "text_model",
            doc_language: "Chinese",
            process_rule: ProcessRule {
                mode: "custom",
                rules: ProcessRules {
                    pre_processing_rules: vec![
                        PreProcessingRule {
                            id: "remove_extra_spaces",
                            enabled: true,
                        },
                        PreProcessingRule {
                            id: "remove_urls_emails",
                            enabled: false,
                        },
                    ],
                    segmentation: Segmentation {
                        separator: "\n\n\n\n\n",
                        max_tokens: 2048,
                        chunk_overlap: 80,
                    },
                },
            },
        };

        // 尝试两种端点格式
        let endpoints = [format!(
            "/datasets/{}/document/create-by-text",
            self.dataset_id
        )];

        let req_value = serde_json::to_value(&req).map_err(|e| {
            AppError::SerializationError(format!("序列化 Dify 创建请求失败: {}", e))
        })?;
        let mut last_error = String::new();
        for endpoint in &endpoints {
            match self
                .post_json::<DifyCreateDocResponse>(self.url(endpoint), req_value.clone())
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    last_error = e.to_string();
                    tracing::warn!(
                        "[Dify] create-document endpoint={} failed: {}",
                        endpoint,
                        last_error
                    );
                }
            }
        }

        Err(AppError::NetworkError(format!(
            "Dify create-document 失败（所有端点均返回错误）: {}\n请检查 Dify 后台是否已配置 Embedding 模型",
            last_error
        )))
    }

    /// 设置文档的 metadata（Dify v1.1.0+）
    ///
    /// 流程：
    /// 1. 调用 [`ensure_metadata_fields`](Self::ensure_metadata_fields) 确保字段存在（自动创建缺失字段）
    /// 2. 批量更新文档 metadata
    ///
    /// 空值字段会被自动过滤跳过。
    pub async fn set_document_metadata(
        &self,
        document_id: &str,
        metadata: &[(&str, &str)],
    ) -> Result<(), AppError> {
        // 过滤掉空值
        let filtered: Vec<(&str, &str)> = metadata
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .copied()
            .collect();

        if filtered.is_empty() {
            tracing::debug!("[Dify] metadata 为空，跳过设置: doc={}", document_id);
            return Ok(());
        }

        // 获取已有的 metadata 字段列表（含 ID）
        let field_map = self.ensure_metadata_fields(&filtered).await?;

        // 构建 operation_data
        let metadata_list: Vec<serde_json::Value> = filtered
            .iter()
            .filter_map(|(name, value)| {
                field_map.get(*name).map(|id| {
                    json!({
                        "id": id,
                        "name": name,
                        "value": value
                    })
                })
            })
            .collect();

        let body = json!({
            "operation_data": [{
                "document_id": document_id,
                "metadata_list": metadata_list,
                "partial_update": false
            }]
        });

        self.post_json_ignore(
            self.url(&format!("/datasets/{}/documents/metadata", self.dataset_id)),
            body,
        )
        .await?;

        let keys: Vec<&str> = filtered.iter().map(|(k, _)| *k).collect();
        tracing::info!(
            "[Dify] metadata 设置成功: doc={}, fields=[{}]",
            document_id,
            keys.join(", ")
        );

        Ok(())
    }

    /// 确保 metadata 字段存在，返回 name -> id 映射
    async fn ensure_metadata_fields(
        &self,
        fields: &[(&str, &str)],
    ) -> Result<HashMap<String, String>, AppError> {
        // 先查询已有字段
        #[derive(Deserialize)]
        struct MetadataListResp {
            #[serde(default)]
            doc_metadata: Vec<MetadataFieldInfo>,
        }
        #[derive(Deserialize)]
        struct MetadataFieldInfo {
            id: String,
            name: String,
        }

        let existing: MetadataListResp = self
            .get_json(self.url(&format!("/datasets/{}/metadata", self.dataset_id)))
            .await?;

        let mut map: std::collections::HashMap<String, String> = existing
            .doc_metadata
            .into_iter()
            .map(|f| (f.name, f.id))
            .collect();

        // 创建缺失的字段
        for (name, _) in fields {
            if !map.contains_key(*name) {
                tracing::info!("[Dify] 创建 metadata 字段: {}", name);
                let body = json!({"type": "string", "name": name});
                #[derive(Deserialize)]
                struct CreatedField {
                    id: String,
                    name: String,
                }
                match self
                    .post_json::<CreatedField>(
                        self.url(&format!("/datasets/{}/metadata", self.dataset_id)),
                        body,
                    )
                    .await
                {
                    Ok(created) => {
                        map.insert(created.name, created.id);
                    }
                    Err(e) => {
                        tracing::warn!("[Dify] create-metadata 字段 {} 失败: {}", name, e);
                    }
                }
            }
        }

        Ok(map)
    }

    /// 删除文档
    pub async fn delete_document(&self, document_id: &str) -> Result<(), AppError> {
        self.delete_ignore(self.url(&format!(
            "/datasets/{}/documents/{}",
            self.dataset_id, document_id
        )))
        .await
    }

    /// 列出知识库文档
    pub async fn list_documents(
        &self,
        page: u32,
        limit: u32,
        keyword: Option<&str>,
    ) -> Result<DifyDocListResponse, AppError> {
        let mut path = format!(
            "/datasets/{}/documents?page={}&limit={}",
            self.dataset_id, page, limit
        );
        if let Some(kw) = keyword {
            let kw = kw.trim();
            if !kw.is_empty() {
                path.push_str(&format!("&keyword={}", urlencoding::encode(kw)));
            }
        }
        self.get_json(self.url(&path)).await
    }

    /// 获取文档的分段列表（用于预览文档内容）
    pub async fn list_segments(
        &self,
        document_id: &str,
        limit: u32,
    ) -> Result<DifySegmentListResponse, AppError> {
        self.get_json(self.url(&format!(
            "/datasets/{}/documents/{}/segments?limit={}",
            self.dataset_id, document_id, limit
        )))
        .await
    }
}
