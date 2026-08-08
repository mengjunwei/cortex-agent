//! OpenAI 兼容 Embedding Provider —— 实现 `adk_rag::EmbeddingProvider`。
//!
//! 与 adk-rag 自带 `OpenAIEmbeddingProvider` 的唯一区别：URL 从 `base_url` 拼接
//! (`format!("{base_url}/embeddings")`)，支持任意 OpenAI 兼容端点（OpenAI / Ollama /
//! SiliconFlow / GLM 等）。adk-rag 原生把 URL 硬编码为 api.openai.com，故薄封装。
//! base_url / api_key / model / dimensions 均来自「模型供应商」解析结果。
//!
//! 原生批量：`input` 传数组，一次请求批量 embedding（比 adk-rag 默认逐条快 N 倍）。

use async_trait::async_trait;
use serde::Deserialize;

use adk_rag::embedding::EmbeddingProvider;
use adk_rag::error::{RagError, Result};

#[derive(serde::Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

/// OpenAI 兼容 Embedding Provider。
pub struct OpenAiCompatibleEmbeddingProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimensions: usize,
}

impl OpenAiCompatibleEmbeddingProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: String,
        model: String,
        dimensions: usize,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model,
            dimensions,
        }
    }
}

fn emb_err(msg: impl Into<String>) -> RagError {
    RagError::EmbeddingError {
        provider: "openai_compatible".to_string(),
        message: msg.into(),
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiCompatibleEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(&[text]).await?;
        Ok(out.pop().unwrap_or_default())
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let req = EmbeddingRequest {
            model: &self.model,
            input: texts.to_vec(),
        };
        let url = format!("{}/embeddings", self.base_url);
        let mut rb = self.client.post(&url).json(&req);
        // api_key 为空（个别本地端点）时不发 Authorization 头
        if !self.api_key.is_empty() {
            rb = rb.bearer_auth(&self.api_key);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| emb_err(format!("请求失败: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<ErrorResponse>(&body)
                .map(|e| e.error.message)
                .unwrap_or(body);
            return Err(emb_err(format!("embedding 端点返回 {status}: {msg}")));
        }

        let parsed = resp
            .json::<EmbeddingResponse>()
            .await
            .map_err(|e| emb_err(format!("解析响应失败: {e}")))?;
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
