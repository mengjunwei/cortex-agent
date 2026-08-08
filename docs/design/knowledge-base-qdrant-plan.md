# 知识库迁移到 Qdrant（替换 Dify）实施计划

> ⚠️ **本计划已废弃**：「一刀切删 Dify 全上 Qdrant」的方向已改为**多 provider 并存**（Dify 外挂 + 内置 Qdrant）。实际落地按多 provider 设计走，最新设计见 [`docs/superpowers/specs/2026-08-02-kb-multi-provider-design.md`](../superpowers/specs/2026-08-02-kb-multi-provider-design.md)。本文保留作历史实施计划储备，下方 Task 不再反映当前代码。

> ⚠️ **历史实施计划 — 多数 Task 已变形落地**。以下 Task **并非全部未执行**：依赖配置（Task 1）、`embedding.rs`（Task 3）、`uuid_chunker.rs`（Task 4）、`KbConfig`（Task 5，字段已变）、`qdrant_store.rs`（Task 6）、`document_store.rs`+`chunk_store.rs`（Task 7）、`KnowledgeManager` 重写（Task 8/9，改为多 provider 路由）、装配（Task 10）**均已以多 provider 形态实现**；唯 Task 11「删除 `dify_client.rs`」方向相反未执行（`dify_client.rs` 保留为 `DifyProvider` 客户端）。实际落地按多 provider 设计走，见 [`2026-08-02-kb-multi-provider-design.md`](../superpowers/specs/2026-08-02-kb-multi-provider-design.md)。本计划保留作历史实施记录，**不建议据此再启动任何工作**。

> **For agentic workers:** 实施本计划时按任务顺序逐个执行，每步用 checkbox (`- [ ]`) 跟踪。TDD：先写失败测试 → 跑 → 实现 → 跑通 → 提交。

**Goal:** 用 Qdrant + adk-rag 在本进程内完整替换 Dify 知识库，包含检索、上传、删除、分段预览与 FAQ 自学习全链路。

**Architecture:** `KnowledgeManager` 重写为持有 `OpenAiCompatibleEmbeddingProvider`（域名从「模型供应商」取）+ `KnowledgeVectorStore`（封装 qdrant-client，打平 payload + 下推 filter）+ adk-rag `RagPipeline`（ingest 用）+ PG `DocumentStore`/`ChunkStore`（元数据/分段预览）。`search_kb` 工具、GraphQL `kb*`、前端 `KnowledgePage.vue` 签名全部不变。

**Tech Stack:** adk-rust 1.0（`rag` feature）、adk-rag 1.0（`qdrant`+`openai` feature）、qdrant-client 1.13、diesel-async、PostgreSQL、wiremock（测试）、uuid v7（已在依赖）。

**已确认决策**：① Embedding 用 OpenAI 格式 + 域名从「模型供应商」解析（仅拼 `/embeddings`）；② 不保留 Dify（一刀切删除）；③ FAQ 随 MVP 一起迁移；④ 无存量数据，不写迁移脚本；⑤ embedding 模型用 DB `purpose=embedding` 标记。

---

## 文件结构（先定后拆）

**新建：**
- `src/domain/knowledge/embedding.rs` — `OpenAiCompatibleEmbeddingProvider`（抄 adk-rag `OpenAIEmbeddingProvider`，改 URL 拼法）
- `src/domain/knowledge/qdrant_store.rs` — `KnowledgeVectorStore`（impl `VectorStore` + filter search/delete_by_document/scroll）
- `src/domain/knowledge/uuid_chunker.rs` — `UuidChunker`（包一层 MarkdownChunker，chunk.id 改写为 UUID v7）
- `src/domain/knowledge/document_store.rs` — `kb_documents` 表 CRUD（替代 `doc_meta_store`）
- `src/domain/knowledge/chunk_store.rs` — `kb_chunks` 表 CRUD（分段预览）
- `src/domain/knowledge/kb_config.rs` — `[kb]` 配置 + 启动建集合 + payload 索引

**重写：**
- `src/domain/knowledge/mod.rs` — `KnowledgeManager` 全部方法改走 Qdrant；纯函数（`normalize_topic_key`/`merge_similar_candidates`/`normalize_faq_content`/`prepare_conversation`/`extract_faqs`/`parse_faq_json`/`build_candidate` 等）**原样保留**

**修改：**
- `Cargo.toml` — 加 `adk-rag`/`qdrant-client`；`adk-rust` 开 `rag`
- `src/config/mod.rs` — 加 `KbConfig`；移除 `DifyConfig`
- `src/model_provider/store.rs` — `llm_models` 加 `purpose`/`embedding_dimensions`/`embedding_default`；`resolve_embedding_model()`
- `src/model_provider/dto.rs` — 模型响应加 embedding 字段
- `src/main.rs` — 装配新 `KnowledgeManager`
- `src/server/kb.rs` — GraphQL 接口适配（返回结构对齐）

**删除：**
- `src/domain/knowledge/dify_client.rs`
- `src/domain/knowledge/doc_meta_store.rs`

---

## Task 1: 依赖与 feature 配置

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: 编辑 `Cargo.toml` 的 `[dependencies]`**

在 adk-rust 依赖上补 `rag` feature，并新增 adk-rag + qdrant-client：

```toml
adk-rust = { version = "1", features = [
    "server",
    "google_search",
    "rag",
] }
adk-rag = { version = "1", features = ["qdrant", "openai"] }
qdrant-client = "1.13"
```

> 注意：必须**同时**直接依赖 `adk-rag` 并开 `qdrant`/`openai`，因为 adk-rust 的 `rag` feature 没有透传子 feature（详见设计文档 §2.2）。

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 成功（adk-rag/qdrant-client 拉取并编译通过；可能首次较慢）

- [ ] **Step 3: 提交**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: 引入 adk-rag(qdrant+openai) 与 qdrant-client 依赖"
```

---

## Task 2: 模型供应商支持 embedding 用途

**Files:**
- Modify: `src/model_provider/store.rs`
- Test: `src/model_provider/store.rs` 内 `#[cfg(test)]`

目标：`llm_models` 加三列（`purpose`/`embedding_dimensions`/`embedding_default`），缓存与解析支持 embedding 模型。

- [ ] **Step 1: 在 `ensure_schema` 末尾追加列迁移与索引**

在 `src/model_provider/store.rs` 的 `ensure_schema`（建 `llm_models` 表之后、`log::info!("[ModelProvider] 表 ... 初始化成功")` 之前）追加：

```rust
        // embedding 用途支持（知识库迁移到 Qdrant 后复用模型供应商体系）
        diesel::sql_query(
            "ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS purpose SMALLINT NOT NULL DEFAULT 0",
        )
        .execute(&mut conn)
        .await?;
        diesel::sql_query(
            "ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS embedding_dimensions INT",
        )
        .execute(&mut conn)
        .await?;
        diesel::sql_query(
            "ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS embedding_default BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(&mut conn)
        .await?;
        // 全局至多一个默认 embedding 模型
        diesel::sql_query(
            r#"CREATE UNIQUE INDEX IF NOT EXISTS uq_llm_models_embedding_default
               ON llm_models (embedding_default) WHERE embedding_default = TRUE"#,
        )
        .execute(&mut conn)
        .await?;
```

- [ ] **Step 2: 扩展 `ModelRow` 与 `CachedModel`**

`ModelRow` 增加字段：

```rust
#[derive(Debug, Clone, QueryableByName)]
struct ModelRow {
    #[diesel(sql_type = sql_types::Varchar)]
    id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    provider_id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    name: String,
    #[diesel(sql_type = sql_types::Varchar)]
    model: String,
    #[diesel(sql_type = sql_types::Bool)]
    is_default: bool,
    #[diesel(sql_type = sql_types::Int2)]
    status: i16,
    #[diesel(sql_type = sql_types::Int2)]
    purpose: i16,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    embedding_dimensions: Option<i32>,
    #[diesel(sql_type = sql_types::Bool)]
    embedding_default: bool,
    #[diesel(sql_type = sql_types::Timestamptz)]
    created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    updated_at: chrono::DateTime<chrono::Utc>,
}
```

`list_models` 的 SELECT 改为：

```rust
        let rows = diesel::sql_query(
            r#"
            SELECT id, provider_id, name, model, is_default, status,
                   purpose, embedding_dimensions, embedding_default,
                   created_at, updated_at
            FROM llm_models
            ORDER BY created_at ASC
            "#,
        )
        .get_results::<ModelRow>(&mut conn)
        .await?;
```

`CachedModel` 增加：

```rust
#[derive(Debug, Clone)]
struct CachedModel {
    id: String,
    name: String,
    model: String,
    vendor_name: String,
    base_url: String,
    api_key: String,
    purpose: i16,            // 0=chat, 1=embedding
    embedding_dimensions: Option<i32>,
    embedding_default: bool,
}
```

`Cache` 增加 `embedding_default_id: Option<String>`：

```rust
#[derive(Default)]
struct Cache {
    models: HashMap<String, CachedModel>,
    default_id: Option<String>,
    embedding_default_id: Option<String>,
}
```

- [ ] **Step 3: `refresh_cache` 填充新字段**

在 `refresh_cache` 构造 `CachedModel` 处补字段：

```rust
                    cache.models.insert(
                        m.id.clone(),
                        CachedModel {
                            id: m.id.clone(),
                            name: m.name.clone(),
                            model: m.model.clone(),
                            vendor_name: p_vendor.clone(),
                            base_url: base_url.clone(),
                            api_key: api_key.clone(),
                            purpose: m.purpose,
                            embedding_dimensions: m.embedding_dimensions,
                            embedding_default: m.embedding_default,
                        },
                    );
                    if m.purpose == 1 && m.embedding_default {
                        cache.embedding_default_id = Some(m.id.clone());
                    }
```

并把 `is_default` 处的 `cache.default_id = Some(...)` 守卫加上 `m.purpose == 0`（chat 默认）。

- [ ] **Step 4: 新增 embedding 解析方法与结果类型**

在文件顶部 `ResolvedLlmConfig` 旁新增：

```rust
/// 解析后的 embedding 模型描述（知识库向量化用）。
#[derive(Debug, Clone)]
pub struct ResolvedEmbeddingConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub dimensions: usize,
}
```

在 `impl ModelProviderStore` 内 `resolve_model` 之后新增：

```rust
    /// 解析 embedding 模型（命中内存缓存）
    ///
    /// - `model_id` 指定具体 embedding 模型 id；为空/None 时使用默认 embedding 模型
    /// - 仅返回 purpose=embedding 且已启用的模型
    pub fn resolve_embedding_model(
        &self,
        model_id: Option<&str>,
    ) -> anyhow::Result<ResolvedEmbeddingConfig> {
        let guard = self.cache.read().unwrap();
        let pick = |m: &CachedModel| -> anyhow::Result<ResolvedEmbeddingConfig> {
            let dims = m.embedding_dimensions.ok_or_else(|| {
                anyhow::anyhow!(
                    "embedding 模型「{}」未配置维度(embedding_dimensions)，请在模型管理中填写",
                    m.name
                )
            })? as usize;
            Ok(ResolvedEmbeddingConfig {
                base_url: m.base_url.clone(),
                api_key: m.api_key.clone(),
                model: m.model.clone(),
                dimensions: dims,
            })
        };

        match model_id.map(str::trim) {
            Some(v) if !v.is_empty() && v != "default" && v != "auto" => {
                let m = guard.models.get(v).ok_or_else(|| {
                    anyhow::anyhow!("指定的 embedding 模型 {} 不可用（未启用或不存在）", v)
                })?;
                if m.purpose != 1 {
                    anyhow::bail!("模型 {} 不是 embedding 用途", m.name);
                }
                pick(m)
            }
            _ => {
                let id = guard.embedding_default_id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "未配置默认 embedding 模型，请在「模型供应商管理」中添加一个 purpose=embedding 的模型并设为默认"
                    )
                })?;
                let m = guard.models.get(id).ok_or_else(|| {
                    anyhow::anyhow!("默认 embedding 模型不可用（未启用）")
                })?;
                pick(m)
            }
        }
    }
```

- [ ] **Step 5: 验证编译**

Run: `cargo build`
Expected: 成功

- [ ] **Step 6: 提交**

```bash
git add src/model_provider/store.rs
git commit -m "feat(model_provider): 支持 embedding 模型用途(purpose/dimensions/embedding_default)"
```

> 注：本任务未覆盖「设为默认 embedding」的 GraphQL/CRUD 接口（属阶段二前端增强）。MVP 阶段可通过直接 SQL 或后续补 create_model 时传 purpose 来标记。后续 Task 会确保 `create_model` 支持 purpose 入参。

---

## Task 3: OpenAiCompatibleEmbeddingProvider（薄封装，抄 adk-rag）

**Files:**
- Create: `src/domain/knowledge/embedding.rs`
- Test: `src/domain/knowledge/embedding.rs` 内 `#[cfg(test)]`

目标：实现 `adk_rag::EmbeddingProvider`，URL 拼 `{base_url}/embeddings`（域名来自模型供应商），逻辑抄 adk-rag `OpenAIEmbeddingProvider`。

- [ ] **Step 1: 写失败测试（用 wiremock 模拟 embedding 端点）**

```rust
//! OpenAI 兼容 Embedding Provider —— 抄自 adk-rag `OpenAIEmbeddingProvider`，
//! 唯一区别：URL 从 base_url 拼接（format!("{base_url}/embeddings")），而非硬编码 api.openai.com。
//! 域名从「模型供应商」解析，支持 OpenAI / Ollama / SiliconFlow / GLM 等 OpenAI 兼容端点。

use std::sync::Arc;

use adk_rag::embedding::EmbeddingProvider;
use adk_rag::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn resp_body(dims: usize, n: usize) -> serde_json::Value {
        serde_json::json!({
            "data": (0..n).map(|i| serde_json::json!({
                "object": "embedding",
                "index": i,
                "embedding": vec![0.1_f32; dims]
            })).collect::<Vec<_>>(),
            "model": "text-embedding-3-small",
            "usage": {"prompt_tokens": 1, "total_tokens": 1}
        })
    }

    #[tokio::test]
    async fn embed_posts_to_base_url_embeddings_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(resp_body(8, 1)))
            .mount(&server)
            .await;

        let provider = OpenAiCompatibleEmbeddingProvider::new(
            server.uri(), // base_url，已含 http://host:port
            "sk-test".into(),
            "text-embedding-3-small".into(),
            8,
        );

        let vec = provider.embed("hello").await.expect("embedding ok");
        assert_eq!(vec.len(), 8);
        assert_eq!(provider.dimensions(), 8);
    }

    #[tokio::test]
    async fn embed_batch_returns_one_vector_per_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(resp_body(4, 3)))
            .mount(&server)
            .await;

        let provider = OpenAiCompatibleEmbeddingProvider::new(
            server.uri(),
            "sk-test".into(),
            "m".into(),
            4,
        );

        let inputs = ["a", "b", "c"];
        let out = provider.embed_batch(&inputs).await.expect("batch ok");
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|v| v.len() == 4));
    }

    #[tokio::test]
    async fn embed_surfaces_error_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(serde_json::json!({
                    "error": {"message": "invalid api key"}
                })),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiCompatibleEmbeddingProvider::new(server.uri(), "bad".into(), "m".into(), 4);
        let err = provider.embed("x").await.unwrap_err();
        assert!(format!("{err}").contains("invalid api key"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib domain::knowledge::embedding`
Expected: 编译失败（`OpenAiCompatibleEmbeddingProvider` 未定义）

- [ ] **Step 3: 实现主体（把上面测试上方的 `//!` 注释后补上结构体与 impl）**

在测试模块之前补上：

```rust
#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
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
///
/// 与 adk-rag `OpenAIEmbeddingProvider` 的唯一区别：URL 从 `base_url` 拼接，
/// 支持任意 OpenAI 兼容端点（域名来自「模型供应商」）。
pub struct OpenAiCompatibleEmbeddingProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimensions: usize,
    /// 可选：Matryoshka 截断维度（如 text-embedding-3-small 可指定 512/1536）
    request_dimensions: Option<usize>,
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
            request_dimensions: None,
        }
    }

    /// 设置请求时下发的 dimensions（Matryoshka 截断）；不调用则用模型原生维度。
    pub fn with_request_dimensions(mut self, dims: Option<usize>) -> Self {
        self.request_dimensions = dims;
        self
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
            dimensions: self.request_dimensions,
        };
        let url = format!("{}/embeddings", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| adk_rag::RagError::EmbeddingError(format!("请求失败: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<ErrorResponse>(&body)
                .map(|e| e.error.message)
                .unwrap_or(body);
            return Err(adk_rag::RagError::EmbeddingError(format!(
                "embedding 端点返回 {status}: {msg}"
            )));
        }

        let parsed = resp
            .json::<EmbeddingResponse>()
            .await
            .map_err(|e| adk_rag::RagError::EmbeddingError(format!("解析响应失败: {e}")))?;
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
```

> 说明：错误类型用 `adk_rag::RagError::EmbeddingError(String)`（见 adk-rag `error` 模块）。如该变体名不同，编译报错时按实际变体名调整。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib domain::knowledge::embedding`
Expected: 3 个测试全绿

- [ ] **Step 5: 提交**

```bash
git add src/domain/knowledge/embedding.rs
git commit -m "feat(kb): OpenAiCompatibleEmbeddingProvider(域名从供应商取,仅拼/embeddings)"
```

---

## Task 4: UuidChunker（包一层 MarkdownChunker）

**Files:**
- Create: `src/domain/knowledge/uuid_chunker.rs`
- Test: 内联

目标：adk-rag chunker 把 chunk.id 生成为 `{docid}_{i}`，Qdrant 只收 UUID/数字。包一层把每个 chunk.id 改写为 UUID v7。

- [ ] **Step 1: 写失败测试**

```rust
//! UUID Chunker —— 包一层 adk-rag Chunker，把每个 chunk.id 改写为 UUID v7。
//!
//! 原因：adk-rag 三个 chunker 都把 id 设为 `{document.id}_{i}`，而 Qdrant 的 point id
//! 必须是 UUID 或无符号整数，`uuid_0` 这类拼接 id 会被 Qdrant 拒绝。删除走 payload
//! `document_id` 过滤，不依赖 chunk id，故这里直接重写为合法 UUID。

use std::sync::Arc;

use adk_rag::chunking::{Chunker, MarkdownChunker};
use adk_rag::document::Document;
use async_trait::async_trait;
use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn doc(text: &str) -> Document {
        Document {
            id: "01958000-0000-7000-8000-000000000001".to_string(),
            text: text.to_string(),
            metadata: HashMap::new(),
            source_uri: None,
        }
    }

    #[test]
    fn rewrites_chunk_ids_to_valid_uuids() {
        let inner = MarkdownChunker::new(1024, 100);
        let chunker = UuidChunker::new(Arc::new(inner));
        let d = doc("# A\n内容一\n\n# B\n内容二\n");
        let chunks = chunker.chunk(&d);
        assert!(!chunks.is_empty(), "markdown 应至少切出 2 段");
        for c in &chunks {
            assert!(Uuid::parse_str(&c.id).is_ok(), "chunk.id 必须是合法 UUID: {}", c.id);
            assert_eq!(c.document_id, d.id, "document_id 必须保留为原文档 id");
        }
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib domain::knowledge::uuid_chunker`
Expected: 编译失败（`UuidChunker` 未定义）

- [ ] **Step 3: 实现主体（测试上方）**

```rust
pub struct UuidChunker {
    inner: Arc<dyn Chunker>,
}

impl UuidChunker {
    pub fn new(inner: Arc<dyn Chunker>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Chunker for UuidChunker {
    fn chunk(&self, document: &Document) -> Vec<adk_rag::document::Chunk> {
        let mut chunks = self.inner.chunk(document);
        for c in chunks.iter_mut() {
            c.id = Uuid::now_v7().to_string();
        }
        chunks
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib domain::knowledge::uuid_chunker`
Expected: 通过

- [ ] **Step 5: 提交**

```bash
git add src/domain/knowledge/uuid_chunker.rs
git commit -m "feat(kb): UuidChunker 把 chunk.id 改写为 UUID v7 以满足 Qdrant point id 约束"
```

---

## Task 5: KbConfig（[kb] 配置段）

**Files:**
- Modify: `src/config/mod.rs`
- Test: 内联

目标：新增 `[kb]` 配置段；移除 `[dify]`。

- [ ] **Step 1: 写失败测试（在 `src/config/mod.rs` 末尾追加）**

```rust
#[cfg(test)]
mod kb_config_tests {
    use super::*;

    #[test]
    fn parse_kb_section_with_defaults() {
        let toml = r#"
            [server]
            port = "8090"
            [db]
            db_type = "postgres"
            host = "localhost"
            port = 5432
            password = "x"
            user = "u"
            db = "d"
            [redis]
            host = "localhost"
            port = 6379
            password = ""
            [log]
            debug = true
            path = "./logs"
            level = "INFO"
            [kb]
            qdrant_url = "http://localhost:6334"
        "#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.kb.qdrant_url, "http://localhost:6334");
        assert_eq!(cfg.kb.collection, "kb_docs");
        assert_eq!(cfg.kb.chunk_size, 1024);
        assert_eq!(cfg.kb.top_k, 6);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib config::kb_config_tests`
Expected: 编译失败（无 `KbConfig`/`cfg.kb`）

- [ ] **Step 3: 实现**

在 `src/config/mod.rs` 中：删除 `DifyConfig` 及其两个 default 函数 `default_dify_url`/`default_dify_topk`；新增：

```rust
/// 知识库配置（`[kb]` 段）— 基于 Qdrant 的知识库
#[derive(Debug, Clone, Deserialize)]
pub struct KbConfig {
    /// Qdrant gRPC 地址
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    /// 可选 Qdrant API Key
    #[serde(default)]
    pub qdrant_api_key: String,
    /// 集合名
    #[serde(default = "default_kb_collection")]
    pub collection: String,
    /// 切片大小
    #[serde(default = "default_chunk_size")]
    pub chunk_size: usize,
    /// 切片重叠
    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap: usize,
    /// 检索 top_k
    #[serde(default = "default_kb_top_k")]
    pub top_k: usize,
    /// 相似度阈值（低于此分数过滤）
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f32,
    /// embedding 模型 id（为空走默认 embedding 模型）
    #[serde(default)]
    pub embedding_model_id: String,
}

impl Default for KbConfig {
    fn default() -> Self {
        Self {
            qdrant_url: default_qdrant_url(),
            qdrant_api_key: String::new(),
            collection: default_kb_collection(),
            chunk_size: default_chunk_size(),
            chunk_overlap: default_chunk_overlap(),
            top_k: default_kb_top_k(),
            similarity_threshold: default_similarity_threshold(),
            embedding_model_id: String::new(),
        }
    }
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".to_string()
}
fn default_kb_collection() -> String {
    "kb_docs".to_string()
}
fn default_chunk_size() -> usize {
    1024
}
fn default_chunk_overlap() -> usize {
    100
}
fn default_kb_top_k() -> usize {
    6
}
fn default_similarity_threshold() -> f32 {
    0.35
}
```

在 `AppConfig` 中把 `pub dify: DifyConfig,` 替换为 `#[serde(default)] pub kb: KbConfig,`。

支持 `QDRANT_URL` 环境变量覆盖：在 `AppConfig::load` 末尾 `Ok(cfg)` 前加：

```rust
        let mut cfg: AppConfig = toml::from_str(&content).with_context(|| "解析配置文件失败")?;
        if let Ok(url) = std::env::var("QDRANT_URL") {
            cfg.kb.qdrant_url = url;
        }
        if let Ok(key) = std::env::var("QDRANT_API_KEY") {
            cfg.kb.qdrant_api_key = key;
        }
        Ok(cfg)
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib config::kb_config_tests`
Expected: 通过

- [ ] **Step 5: 同步更新 `config/config.toml`**

把 `[dify]` 段替换为：

```toml
[kb]
qdrant_url = "http://localhost:6334"
collection = "kb_docs"
chunk_size = 1024
chunk_overlap = 100
top_k = 6
similarity_threshold = 0.35
embedding_model_id = ""
```

- [ ] **Step 6: 提交**

```bash
git add src/config/mod.rs config/config.toml
git commit -m "feat(config): 新增 [kb] 配置段(Qdrant), 移除 [dify]"
```

---

## Task 6: KnowledgeVectorStore（封装 qdrant-client，打平 payload + filter）

**Files:**
- Create: `src/domain/knowledge/qdrant_store.rs`
- Test: 内联（集成测试，标 `#[ignore]`，需本地 Qdrant）

目标：实现 `adk_rag::VectorStore`，upsert 时把 brand/dev_type/doc_type/title/document_id **打平到 payload 顶层**；并提供 `search_filtered`（下推 Qdrant Filter）、`delete_by_document`、`scroll_by_document`。

> qdrant-client API 参考 adk-rag `qdrant.rs`（同一版本 1.13）。下列实现沿用其 `QdrantClientConfig::from_url`/`CreateCollectionBuilder`/`PointStruct`/`SearchPointsBuilder`/`DeletePointsBuilder`/`ScrollPointsBuilder`/`Filter`/`Condition` 用法。

- [ ] **Step 1: 写集成测试（标 `#[ignore]`，需本地 Qdrant 6334）**

```rust
//! 知识库向量存储 —— 封装 qdrant-client，实现 adk_rag::VectorStore。
//!
//! 与 adk-rag `QdrantVectorStore` 的区别：
//! 1. upsert 时把 brand/dev_type/doc_type/title/document_id/chunk_index 打平到 payload 顶层
//!    （adk-rag 嵌套为 metadata:{...}），便于下推 Qdrant keyword filter；
//! 2. 新增 search_filtered（带 brand/dev_type/doc_type 过滤的向量检索）；
//! 3. 新增 delete_by_document / scroll_by_document（按 document_id payload 过滤）。

use std::collections::HashMap;
use std::sync::Arc;

use adk_rag::document::{Chunk, SearchResult};
use adk_rag::error::Result;
use adk_rag::vectorstore::VectorStore;
use async_trait::async_trait;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, Distance, Filter, PointStruct, SearchPointsBuilder,
    VectorParamsBuilder, VectorsConfig,
};
use qdrant_client::{Payload, Qdrant, QdrantClientConfig};
use serde_json::json;

/// 知识库过滤条件（brand/dev_type/doc_type 任一为 None 表示不过滤该项）
#[derive(Debug, Clone, Default)]
pub struct KbFilter {
    pub brand: Option<String>,
    pub dev_type: Option<String>,
    pub doc_type: Option<String>,
}

impl KbFilter {
    fn to_qdrant(&self) -> Option<Filter> {
        let mut conds: Vec<Condition> = Vec::new();
        if let Some(b) = &self.brand {
            conds.push(Condition::match_kw("brand", b.clone()));
        }
        if let Some(d) = &self.dev_type {
            conds.push(Condition::match_kw("dev_type", d.clone()));
        }
        if let Some(t) = &self.doc_type {
            conds.push(Condition::match_kw("doc_type", t.clone()));
        }
        if conds.is_empty() {
            None
        } else {
            Some(Filter::must(conds))
        }
    }
}

pub struct KnowledgeVectorStore {
    client: Qdrant,
}

impl KnowledgeVectorStore {
    pub fn new(qdrant_url: &str, api_key: Option<&str>) -> Result<Self> {
        let mut cfg = QdrantClientConfig::from_url(qdrant_url);
        if let Some(key) = api_key {
            if !key.is_empty() {
                cfg.api_key = Some(key.to_string());
            }
        }
        let client = Qdrant::new(cfg).map_err(|e| {
            adk_rag::RagError::VectorStoreError(format!("连接 Qdrant 失败: {e}"))
        })?;
        Ok(Self { client })
    }

    /// 创建 payload 索引（加速 brand/dev_type/doc_type/document_id 过滤）
    pub async fn ensure_payload_indexes(&self, collection: &str) -> Result<()> {
        use qdrant_client::qdrant::FieldType;
        for field in ["brand", "dev_type", "doc_type", "document_id"] {
            self.client
                .create_field_index(collection, field, FieldType::Keyword, None, None)
                .await
                .map_err(|e| {
                    adk_rag::RagError::VectorStoreError(format!("创建索引 {field} 失败: {e}"))
                })?;
        }
        Ok(())
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
        let resp = self
            .client
            .search_points(builder)
            .await
            .map_err(|e| adk_rag::RagError::VectorStoreError(format!("检索失败: {e}")))?;
        Ok(resp
            .result
            .into_iter()
            .map(|point| {
                let payload = point.payload.clone();
                let chunk = payload_to_chunk(point.id.unwrap().point_id_options.unwrap(), payload);
                SearchResult {
                    chunk,
                    score: point.score,
                }
            })
            .collect())
    }

    /// 按 document_id 删除该文档全部切片
    pub async fn delete_by_document(&self, collection: &str, document_id: &str) -> Result<()> {
        use qdrant_client::qdrant::PointsSelector;
        let filter = Filter::must(vec![Condition::match_kw("document_id", document_id.to_string())]);
        self.client
            .delete_points(
                collection,
                PointsSelector::Filter(filter),
                None,
                true,
                None,
            )
            .await
            .map_err(|e| adk_rag::RagError::VectorStoreError(format!("按文档删除失败: {e}")))?;
        Ok(())
    }

    /// 按 document_id 拉取全部分段（分段预览/重建索引用）
    pub async fn scroll_by_document(
        &self,
        collection: &str,
        document_id: &str,
    ) -> Result<Vec<Chunk>> {
        use qdrant_client::qdrant::ScrollPointsBuilder;
        let filter = Filter::must(vec![Condition::match_kw("document_id", document_id.to_string())]);
        let resp = self
            .client
            .scroll(ScrollPointsBuilder::new(collection).filter(filter).limit(256))
            .await
            .map_err(|e| adk_rag::RagError::VectorStoreError(format!("scroll 失败: {e}")))?;
        Ok(resp
            .result
            .into_iter()
            .map(|point| payload_to_chunk(point.id.unwrap().point_id_options.unwrap(), point.payload))
            .collect())
    }
}

#[async_trait]
impl VectorStore for KnowledgeVectorStore {
    async fn create_collection(&self, name: &str, dimensions: usize) -> Result<()> {
        if self.client.collection_exists(name).await.unwrap_or(false) {
            return Ok(());
        }
        self.client
            .create_collection(
                CreateCollectionBuilder::new(name)
                    .vectors_config(VectorsConfig::Params(VectorParamsBuilder::new(
                        dimensions as u64,
                        Distance::Cosine,
                    ))),
            )
            .await
            .map_err(|e| adk_rag::RagError::VectorStoreError(format!("建集合失败: {e}")))?;
        self.ensure_payload_indexes(name).await?;
        Ok(())
    }

    async fn delete_collection(&self, name: &str) -> Result<()> {
        self.client
            .delete_collection(name)
            .await
            .map_err(|e| adk_rag::RagError::VectorStoreError(format!("删集合失败: {e}")))?;
        Ok(())
    }

    async fn upsert(&self, collection: &str, chunks: &[Chunk]) -> Result<()> {
        let points: Vec<PointStruct> = chunks
            .iter()
            .map(|c| {
                let mut payload: HashMap<String, Payload> = HashMap::new();
                payload.insert("text".into(), Payload::String(c.text.clone()));
                payload.insert("document_id".into(), Payload::String(c.document_id.clone()));
                // 打平元数据：brand/dev_type/doc_type/title/chunk_index/header_path 等全部提到顶层
                for (k, v) in &c.metadata {
                    payload.insert(k.clone(), Payload::String(v.clone()));
                }
                PointStruct::new(uuid::Uuid::parse_str(&c.id).unwrap_or_else(|_| uuid::Uuid::now_v7()), c.embedding.clone(), payload)
            })
            .collect();
        self.client
            .upsert_points(collection, points, true)
            .await
            .map_err(|e| adk_rag::RagError::VectorStoreError(format!("upsert 失败: {e}")))?;
        Ok(())
    }

    async fn delete(&self, collection: &str, ids: &[&str]) -> Result<()> {
        use qdrant_client::qdrant::PointsSelector;
        let uuids: Vec<uuid::Uuid> = ids.iter().filter_map(|s| uuid::Uuid::parse_str(s).ok()).collect();
        if uuids.is_empty() {
            return Ok(());
        }
        self.client
            .delete_points(collection, PointsSelector::Points(uuids), None, true, None)
            .await
            .map_err(|e| adk_rag::RagError::VectorStoreError(format!("删除失败: {e}")))?;
        Ok(())
    }

    async fn search(
        &self,
        collection: &str,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        self.search_filtered(collection, embedding, top_k, None).await
    }
}

/// 从 Qdrant point 还原 Chunk（payload 顶层 → metadata）
fn payload_to_chunk(
    point_id: qdrant_client::qdrant::point_id::PointIdOptions,
    payload: HashMap<String, Payload>,
) -> Chunk {
    let id = match point_id {
        qdrant_client::qdrant::point_id::PointIdOptions::Uuid(u) => u,
        qdrant_client::qdrant::point_id::PointIdOptions::Num(n) => n.to_string(),
    };
    let text = payload
        .get("text")
        .and_then(|p| match p {
            Payload::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let document_id = payload
        .get("document_id")
        .and_then(|p| match p {
            Payload::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let mut metadata = HashMap::new();
    for (k, v) in payload {
        if k == "text" || k == "document_id" {
            continue;
        }
        if let Payload::String(s) = v {
            metadata.insert(k, s);
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

#[cfg(test)]
mod tests {
    use super::*;

    // 集成测试：需本地 Qdrant（docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant）
    // 运行：cargo test --lib domain::knowledge::qdrant_store -- --ignored
    #[tokio::test]
    #[ignore]
    async fn upsert_and_filtered_search_roundtrip() {
        let store = KnowledgeVectorStore::new("http://localhost:6334", None).unwrap();
        let col = "kb_test_roundtrip";
        let _ = store.delete_collection(col).await;
        store.create_collection(col, 4).await.unwrap();

        let mk_chunk = |id, doc_id, brand, dev_type, emb: Vec<f32>| Chunk {
            id: id.to_string(),
            text: format!("content-{id}"),
            embedding: emb,
            metadata: HashMap::from([
                ("brand".into(), brand.into()),
                ("dev_type".into(), dev_type.into()),
                ("doc_type".into(), "manual".into()),
            ]),
            document_id: doc_id.into(),
        };
        store
            .upsert(
                col,
                &[
                    mk_chunk(uuid::Uuid::now_v7(), "docA", "H3C", "router", vec![1.0, 0.0, 0.0, 0.0]),
                    mk_chunk(uuid::Uuid::now_v7(), "docB", "Huawei", "switch", vec![0.0, 1.0, 0.0, 0.0]),
                ],
            )
            .await
            .unwrap();

        let res = store
            .search_filtered(
                col,
                &[1.0, 0.0, 0.0, 0.0],
                5,
                Some(KbFilter {
                    brand: Some("H3C".into()),
                    dev_type: None,
                    doc_type: None,
                }),
            )
            .await
            .unwrap();
        assert!(res.iter().all(|r| r.chunk.metadata.get("brand") == Some(&"H3C".to_string())));

        store.delete_by_document(col, "docA").await.unwrap();
        store.delete_collection(col).await.unwrap();
    }
}
```

- [ ] **Step 2: 跑测试（确认编译通过；集成测试默认跳过）**

Run: `cargo test --lib domain::knowledge::qdrant_store`
Expected: 编译通过，`#[ignore]` 测试默认不跑

- [ ] **Step 3:（可选）跑集成测试验证真实 Qdrant**

Run: `cargo test --lib domain::knowledge::qdrant_store -- --ignored`
Expected: 需要 Qdrant 运行；通过则证明 upsert/过滤检索/按文档删除闭环正确

- [ ] **Step 4: 提交**

```bash
git add src/domain/knowledge/qdrant_store.rs
git commit -m "feat(kb): KnowledgeVectorStore(封装qdrant-client,打平payload+下推filter)"
```

> 注：qdrant-client 1.13 的 `Qdrant::new(cfg)` / `delete_points` / `create_field_index` / `search_points` / `scroll` / `PointIdOptions` 的确切签名需对照实际 crate 版本微调（编译器会精确报错）。`payload_to_chunk` 的枚举分支按 `Payload`/`point_id::PointIdOptions` 实际定义调整。这是本任务最可能需要微调的部分。

---

## Task 7: DocumentStore + ChunkStore（PG 元数据 + 分段预览）

**Files:**
- Create: `src/domain/knowledge/document_store.rs`
- Create: `src/domain/knowledge/chunk_store.rs`

目标：替代 `doc_meta_store.rs`。建 `kb_documents` + `kb_chunks` 两表，提供 CRUD。模式完全对齐现有 `DocMetaStore`（diesel `sql_query` + `QueryableByName`）。

- [ ] **Step 1: 实现 `document_store.rs`**

```rust
//! kb_documents 表 CRUD —— 文档元数据（替代 kb_doc_meta）。
use crate::error::AppError;
use crate::infra::db::{DbPool, DbPooledConnection};
use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

#[derive(Debug, Clone, QueryableByName, serde::Serialize)]
pub struct KbDocument {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub doc_type: i16, // 1=手册, 2=FAQ
    #[diesel(sql_type = sql_types::Varchar)]
    pub brand: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub dev_type: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub firmware_ver: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub title: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub source: String, // manual/faq
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

impl DocumentStore {
    pub async fn new(pool: DbPool) -> Result<Self, AppError> {
        let mut conn = pool.get().await?;
        diesel::sql_query(
            r#"
            CREATE TABLE IF NOT EXISTS kb_documents (
                id           VARCHAR(36) PRIMARY KEY,
                doc_type     SMALLINT NOT NULL DEFAULT 1,
                brand        VARCHAR(64)  NOT NULL DEFAULT '',
                dev_type     VARCHAR(64)  NOT NULL DEFAULT '',
                firmware_ver VARCHAR(64)  NOT NULL DEFAULT '',
                title        VARCHAR(256) NOT NULL DEFAULT '',
                source       VARCHAR(32)  NOT NULL DEFAULT 'manual',
                word_count   INTEGER      NOT NULL DEFAULT 0,
                chunk_count  INTEGER      NOT NULL DEFAULT 0,
                status       SMALLINT     NOT NULL DEFAULT 1,
                uploaded_by  VARCHAR(64)  NOT NULL DEFAULT '',
                created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
                updated_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(&mut conn).await?;
        diesel::sql_query("CREATE INDEX IF NOT EXISTS idx_kb_documents_brand_dev ON kb_documents(brand, dev_type)")
            .execute(&mut conn).await?;
        diesel::sql_query("CREATE INDEX IF NOT EXISTS idx_kb_documents_title ON kb_documents(title)")
            .execute(&mut conn).await?;
        Ok(Self { pool })
    }

    async fn get_conn(&self) -> Result<DbPooledConnection, AppError> {
        self.pool.get().await.map_err(AppError::from)
    }

    pub async fn insert(
        &self,
        id: &str,
        doc_type: i16,
        brand: &str,
        dev_type: &str,
        firmware_ver: &str,
        title: &str,
        source: &str,
        word_count: i32,
        chunk_count: i32,
        uploaded_by: &str,
    ) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO kb_documents (id, doc_type, brand, dev_type, firmware_ver, title, source, word_count, chunk_count, uploaded_by)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)"#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Int2, _>(doc_type)
        .bind::<sql_types::Text, _>(brand)
        .bind::<sql_types::Text, _>(dev_type)
        .bind::<sql_types::Text, _>(firmware_ver)
        .bind::<sql_types::Text, _>(title)
        .bind::<sql_types::Text, _>(source)
        .bind::<sql_types::Int4, _>(word_count)
        .bind::<sql_types::Int4, _>(chunk_count)
        .bind::<sql_types::Text, _>(uploaded_by)
        .execute(&mut conn).await?;
        Ok(())
    }

    pub async fn update_chunk_count(&self, id: &str, chunk_count: i32) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("UPDATE kb_documents SET chunk_count=$2, updated_at=NOW() WHERE id=$1")
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Int4, _>(chunk_count)
            .execute(&mut conn).await?;
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("DELETE FROM kb_documents WHERE id=$1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut conn).await?;
        Ok(())
    }

    /// 分页 + brand/dev_type/keyword 过滤
    pub async fn list(
        &self,
        page: u32,
        limit: u32,
        brand: Option<&str>,
        dev_type: Option<&str>,
        keyword: Option<&str>,
    ) -> Result<(Vec<KbDocument>, i64), AppError> {
        let mut conn = self.get_conn().await?;
        let offset = ((page.saturating_sub(1)) * limit) as i64;
        let rows = diesel::sql_query(
            r#"SELECT id, doc_type, brand, dev_type, firmware_ver, title, source, word_count, chunk_count, created_at
               FROM kb_documents
               WHERE ($1::text IS NULL OR brand = $1)
                 AND ($2::text IS NULL OR dev_type = $2)
                 AND ($3::text IS NULL OR title ILIKE '%' || $3 || '%')
               ORDER BY created_at DESC
               LIMIT $4 OFFSET $5"#,
        )
        .bind::<sql_types::Nullable<sql_types::Text>, _>(brand.filter(|s| !s.is_empty()))
        .bind::<sql_types::Nullable<sql_types::Text>, _>(dev_type.filter(|s| !s.is_empty()))
        .bind::<sql_types::Nullable<sql_types::Text>, _>(keyword.filter(|s| !s.is_empty()))
        .bind::<sql_types::Int8, _>(limit as i64)
        .bind::<sql_types::Int8, _>(offset)
        .get_results::<KbDocument>(&mut conn).await?;

        let total: i64 = diesel::sql_query(
            r#"SELECT COUNT(*) AS cnt FROM kb_documents
               WHERE ($1::text IS NULL OR brand = $1)
                 AND ($2::text IS NULL OR dev_type = $2)
                 AND ($3::text IS NULL OR title ILIKE '%' || $3 || '%')"#,
        )
        .bind::<sql_types::Nullable<sql_types::Text>, _>(brand.filter(|s| !s.is_empty()))
        .bind::<sql_types::Nullable<sql_types::Text>, _>(dev_type.filter(|s| !s.is_empty()))
        .bind::<sql_types::Nullable<sql_types::Text>, _>(keyword.filter(|s| !s.is_empty()))
        .get_result::<CountRow>(&mut conn)
        .await?
        .cnt;
        Ok((rows, total))
    }

    /// 查同 brand/dev_type 下标题（FAQ 查重用）
    pub async fn list_titles_by_brand_dev(
        &self,
        brand: &str,
        dev_type: &str,
    ) -> Result<Vec<String>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT title FROM kb_documents WHERE brand=$1 AND dev_type=$2"#,
        )
        .bind::<sql_types::Text, _>(brand)
        .bind::<sql_types::Text, _>(dev_type)
        .get_results::<TitleRow>(&mut conn).await?;
        Ok(rows.into_iter().map(|r| r.title).collect())
    }

    /// 按 (brand, dev_type, 归一化 title) 查找已存在的 FAQ 文档 id
    pub async fn find_by_brand_dev_titles(
        &self,
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
               WHERE brand=$1 AND dev_type=$2 AND title = ANY($3)"#,
        )
        .bind::<sql_types::Text, _>(brand)
        .bind::<sql_types::Text, _>(dev_type)
        .bind::<sql_types::Array<sql_types::Text>, _>(titles)
        .get_results::<IdTitleRow>(&mut conn).await?;
        Ok(rows.into_iter().map(|r| (r.id, r.title)).collect())
    }
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = sql_types::Int8)]
    cnt: i64,
}
#[derive(QueryableByName)]
struct TitleRow {
    #[diesel(sql_type = sql_types::Varchar)]
    title: String,
}
#[derive(QueryableByName)]
struct IdTitleRow {
    #[diesel(sql_type = sql_types::Varchar)]
    id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    title: String,
}
```

- [ ] **Step 2: 实现 `chunk_store.rs`**

```rust
//! kb_chunks 表 CRUD —— 分段预览。
use crate::error::AppError;
use crate::infra::db::{DbPool, DbPooledConnection};
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

impl ChunkStore {
    pub async fn new(pool: DbPool) -> Result<Self, AppError> {
        let mut conn = pool.get().await?;
        diesel::sql_query(
            r#"
            CREATE TABLE IF NOT EXISTS kb_chunks (
                id           VARCHAR(36) PRIMARY KEY,
                document_id  VARCHAR(36) NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
                chunk_index  INTEGER     NOT NULL DEFAULT 0,
                content      TEXT        NOT NULL DEFAULT '',
                word_count   INTEGER     NOT NULL DEFAULT 0,
                header_path  VARCHAR(512) NOT NULL DEFAULT '',
                created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(&mut conn).await?;
        diesel::sql_query("CREATE INDEX IF NOT EXISTS idx_kb_chunks_document ON kb_chunks(document_id)")
            .execute(&mut conn).await?;
        Ok(Self { pool })
    }

    async fn get_conn(&self) -> Result<DbPooledConnection, AppError> {
        self.pool.get().await.map_err(AppError::from)
    }

    pub async fn insert_batch(
        &self,
        rows: &[(String, String, i32, String, i32, String)], // (id, document_id, chunk_index, content, word_count, header_path)
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
            .execute(&mut conn).await?;
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
        .get_results::<KbChunk>(&mut conn).await?;
        Ok(rows)
    }
}
```

- [ ] **Step 3: 验证编译**

Run: `cargo build`
Expected: 成功

- [ ] **Step 4: 提交**

```bash
git add src/domain/knowledge/document_store.rs src/domain/knowledge/chunk_store.rs
git commit -m "feat(kb): DocumentStore+ChunkStore(kb_documents/kb_chunks 表 CRUD)"
```

---

## Task 8: 重写 KnowledgeManager —— 装配 + 检索/上传/删除/列表/分段

**Files:**
- Modify: `src/domain/knowledge/mod.rs`

目标：`KnowledgeManager` 改持有 embedding provider + qdrant store + pipeline + document/chunk store；search/upload/delete/list/segments 走 Qdrant。**保留全部纯函数与 FAQ LLM 提取逻辑不动**。

- [ ] **Step 1: 改模块声明与 imports**

把文件头的 `pub mod dify_client; pub mod doc_meta_store;` 替换为：

```rust
pub mod chunk_store;
pub mod document_store;
pub mod embedding;
pub mod kb_config;
pub mod qdrant_store;
pub mod uuid_chunker;
```

imports 部分替换为（去掉 DifyClient/DocMetaStore/adk_rust LLM 相关暂留）：

```rust
use crate::error::AppError;
use crate::config::AppConfig;
use crate::domain::enum_def::RiskLevel;
use crate::domain::knowledge::chunk_store::ChunkStore;
use crate::domain::knowledge::document_store::DocumentStore;
use crate::domain::knowledge::embedding::OpenAiCompatibleEmbeddingProvider;
use crate::domain::knowledge::qdrant_store::{KbFilter, KnowledgeVectorStore};
use crate::domain::meta::DeviceMeta;
use crate::model_provider::store::ModelProviderStore;
use adk_rag::chunking::{Chunker, MarkdownChunker};
use adk_rag::document::Document;
use adk_rag::embedding::EmbeddingProvider;
use adk_rag::pipeline::RagPipeline;
use adk_rag::vectorstore::VectorStore;
use adk_rust::{Content, Llm, LlmRequest};
use chrono::Utc;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;
```

- [ ] **Step 2: 重写 `KnowledgeManager` 结构体与 `new`**

```rust
pub struct KnowledgeManager {
    embedding: Arc<OpenAiCompatibleEmbeddingProvider>,
    store: Arc<KnowledgeVectorStore>,
    pipeline: Arc<RagPipeline>,
    collection: String,
    top_k: usize,
    similarity_threshold: f32,
    documents: Arc<DocumentStore>,
    chunks: Arc<ChunkStore>,
    feedback_cache: RwLock<HashMap<String, i64>>,
}

impl KnowledgeManager {
    pub fn new(
        config: Arc<AppConfig>,
        model_store: Arc<ModelProviderStore>,
        documents: Arc<DocumentStore>,
        chunks: Arc<ChunkStore>,
    ) -> Result<Self, AppError> {
        let emb_cfg = model_store
            .resolve_embedding_model(if config.kb.embedding_model_id.is_empty() {
                None
            } else {
                Some(&config.kb.embedding_model_id)
            })
            .map_err(|e| AppError::BusinessError(format!("embedding 模型解析失败: {e}")))?;
        let embedding = Arc::new(OpenAiCompatibleEmbeddingProvider::new(
            emb_cfg.base_url,
            emb_cfg.api_key,
            emb_cfg.model,
            emb_cfg.dimensions,
        ));

        let store = Arc::new(KnowledgeVectorStore::new(
            &config.kb.qdrant_url,
            Some(&config.kb.qdrant_api_key),
        ).map_err(|e| AppError::BusinessError(format!("Qdrant 初始化失败: {e}")))?);

        let chunker = Arc::new(crate::domain::knowledge::uuid_chunker::UuidChunker::new(
            Arc::new(MarkdownChunker::new(config.kb.chunk_size, config.kb.chunk_overlap)),
        ));

        let pipeline = Arc::new(
            RagPipeline::builder()
                .config(adk_rag::config::RagConfig::default())
                .embedding_provider(embedding.clone())
                .vector_store(store.clone())
                .chunker(chunker)
                .build()
                .map_err(|e| AppError::BusinessError(format!("RagPipeline 构建失败: {e}")))?,
        );

        Ok(KnowledgeManager {
            embedding,
            store,
            pipeline,
            collection: config.kb.collection.clone(),
            top_k: config.kb.top_k,
            similarity_threshold: config.kb.similarity_threshold,
            documents,
            chunks,
            feedback_cache: RwLock::new(HashMap::new()),
        })
    }

    /// 启动时调用：建集合（按 embedding 维度）
    pub async fn ensure_collection(&self) -> Result<(), AppError> {
        self.pipeline.create_collection(&self.collection).await
            .map_err(|e| AppError::BusinessError(format!("建集合失败: {e}")))?;
        Ok(())
    }
}
```

- [ ] **Step 3: 重写 `search`**

```rust
    pub async fn search(
        &self,
        _biz_id: &str,
        query: &str,
        brand: Option<&str>,
        dev_type: Option<&str>,
        _risk_level: Option<&RiskLevel>,
        topk: Option<usize>,
    ) -> Result<Vec<DeviceMeta>, AppError> {
        let k = topk.unwrap_or(self.top_k);
        let emb = self.embedding.embed(query).await
            .map_err(|e| AppError::BusinessError(format!("query embedding 失败: {e}")))?;

        let filter = KbFilter {
            brand: brand.filter(|s| !s.is_empty()).map(str::to_string),
            dev_type: dev_type.filter(|s| !s.is_empty()).map(str::to_string),
            doc_type: None,
        };
        let results = self.store.search_filtered(&self.collection, &emb, k, Some(filter)).await
            .map_err(|e| AppError::BusinessError(format!("检索失败: {e}")))?;

        let now = Utc::now().timestamp();
        let out: Vec<DeviceMeta> = results
            .into_iter()
            .filter(|r| r.score >= self.similarity_threshold)
            .map(|r| {
                let m = &r.chunk.metadata;
                DeviceMeta {
                    brand: m.get("brand").cloned().unwrap_or_default(),
                    dev_type: m.get("dev_type").cloned().unwrap_or_default(),
                    firmware_ver: m.get("firmware_ver").cloned().unwrap_or_default(),
                    doc_id: r.chunk.document_id.clone(),
                    title: m.get("title").cloned().unwrap_or_default(),
                    content: r.chunk.text.clone(),
                    op_type: crate::domain::enum_def::OpType::Query,
                    risk_level: RiskLevel::Low,
                    cmd_tags: Vec::new(),
                    create_at: now,
                    last_access_at: now,
                    access_count: 0,
                    expire_at: None,
                    quality_score: ((r.score * 100.0) as u8).max(50),
                    is_deleted: false,
                    like_count: 0,
                    dislike_count: 0,
                    weight: r.score,
                    feedback_status: "normal".to_string(),
                    feedback_note: String::new(),
                    doc_source: "qdrant".to_string(),
                }
            })
            .collect();
        Ok(out)
    }
```

- [ ] **Step 4: 重写 `upload_document`（pipeline.ingest + PG 落库）**

```rust
    pub async fn upload_document(
        &self,
        brand: &str,
        dev_type: &str,
        firmware_ver: &str,
        title: &str,
        content: &str,
        user_role: &str,
    ) -> Result<Vec<String>, AppError> {
        let doc_id = Uuid::now_v7().to_string();
        let mut metadata = HashMap::new();
        metadata.insert("brand".to_string(), brand.to_string());
        metadata.insert("dev_type".to_string(), dev_type.to_string());
        metadata.insert("firmware_ver".to_string(), firmware_ver.to_string());
        metadata.insert("title".to_string(), title.to_string());
        metadata.insert("doc_type".to_string(), "manual".to_string());
        metadata.insert("source".to_string(), "manual".to_string());
        let document = Document {
            id: doc_id.clone(),
            text: content.to_string(),
            metadata,
            source_uri: None,
        };

        let stored = self.pipeline.ingest(&self.collection, &document).await
            .map_err(|e| AppError::BusinessError(format!("文档入库失败: {e}")))?;
        let chunk_count = stored.len() as i32;
        let word_count = content.chars().count() as i32;

        // PG 文档元数据
        self.documents.insert(
            &doc_id, 1, brand, dev_type, firmware_ver, title, "manual",
            word_count, chunk_count, user_role,
        ).await.ok();

        // PG 分段预览
        let rows: Vec<_> = stored.iter().enumerate().map(|(i, c)| {
            (c.id.clone(), doc_id.clone(), i as i32, c.text.clone(),
             c.text.chars().count() as i32, c.metadata.get("header_path").cloned().unwrap_or_default())
        }).collect();
        self.chunks.insert_batch(&rows).await.ok();

        log::info!("[upload] 文档入库成功: id={}, chunks={}", doc_id, chunk_count);
        Ok(vec![doc_id])
    }
```

- [ ] **Step 5: 重写 `delete_document` + `list_documents` + `get_document_segments`**

```rust
    pub async fn delete_document(&self, doc_id: &str) -> Result<(), AppError> {
        // Qdrant：按 document_id payload 删除全部切片
        self.store.delete_by_document(&self.collection, doc_id).await
            .map_err(|e| AppError::BusinessError(format!("Qdrant 删除失败: {e}")))?;
        // PG：文档 + 分段（kb_chunks ON DELETE CASCADE 联动删除）
        if let Err(e) = self.documents.delete(doc_id).await {
            log::warn!("[delete] PG 删除失败({}), id={}", e, doc_id);
        }
        log::info!("[delete] 文档已删除: id={}", doc_id);
        Ok(())
    }

    /// 文档列表（从 PG，返回结构对齐前端所需）
    pub async fn list_documents(
        &self,
        page: u32,
        limit: u32,
        brand: Option<&str>,
        dev_type: Option<&str>,
        keyword: Option<&str>,
    ) -> Result<DocumentListResponse, AppError> {
        let (docs, total) = self.documents.list(page, limit, brand, dev_type, keyword).await?;
        Ok(DocumentListResponse { data: docs, total, page, limit })
    }

    /// 分段预览（从 PG kb_chunks，按 chunk_index 排序）
    pub async fn get_document_segments(
        &self,
        document_id: &str,
    ) -> Result<Vec<crate::domain::knowledge::chunk_store::KbChunk>, AppError> {
        self.chunks.list_by_document(document_id).await
    }
```

并在 `mod.rs` 文件级新增响应结构体（供 server 层序列化）：

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct DocumentListResponse {
    pub data: Vec<crate::domain::knowledge::document_store::KbDocument>,
    pub total: i64,
    pub page: u32,
    pub limit: u32,
}
```

- [ ] **Step 6: `add_feedback` 保留不变**（仅依赖 feedback_cache，与 Dify 无关）。删去 `doc_meta_store()` 访问器。

- [ ] **Step 7: 验证编译**

Run: `cargo build`
Expected: KnowledgeManager 的检索/上传/删除/列表/分段方法编译通过（FAQ 方法可能因引用旧 Dify 调用而报错，下个任务修复）

- [ ] **Step 8: 提交**

```bash
git add src/domain/knowledge/mod.rs
git commit -m "feat(kb): KnowledgeManager 检索/上传/删除/列表/分段改为走 Qdrant"
```

---

## Task 9: 重写 KnowledgeManager —— FAQ 自学习全量迁移

**Files:**
- Modify: `src/domain/knowledge/mod.rs`

目标：把 `commit_faqs` / `learn_single_faq` / `mark_duplicates` / `find_dify_doc_for_topic`（改名为 `find_doc_for_topic`）/ `generate_candidates` / `regenerate_candidates` 中的 Dify 调用全部换成 Qdrant ingest + PG 查询。**保留 LLM 提取逻辑**（`compress_if_too_long` / `extract_faqs` / `parse_faq_json` / `build_candidate` / `merge_similar_candidates` / `normalize_*`）原样不动。

- [ ] **Step 1: `find_dify_doc_for_topic` → `find_doc_for_topic`（改查 PG）**

把原方法体（Dify `list_documents` 过滤）替换为：

```rust
    /// 按 brand/dev_type/topic 查找已存在的 FAQ 文档 id（改查 PG，不再调 Dify）
    pub async fn find_doc_for_topic(
        &self,
        brand: &str,
        dev_type: &str,
        topic: &str,
    ) -> Result<Option<String>, AppError> {
        let topic_key = normalize_topic_key(topic);
        let exists = self.documents.find_by_brand_dev_titles(
            brand, dev_type, &[topic_key.clone()],
        ).await?;
        Ok(exists.into_iter().next().map(|(id, _)| id))
    }
```

- [ ] **Step 2: `commit_faqs`（写入端改为 ingest）**

核心改动：不再调 Dify `create_document_by_text`，改为复用 ingest 路径。把原方法体中「Dify 创建文档」段替换为：

```rust
    pub async fn commit_faqs(
        &self,
        brand: &str,
        dev_type: &str,
        firmware_ver: &str,
        candidates: &[FaqCandidate],
        user_role: &str,
    ) -> Result<i64, AppError> {
        let mut merged = merge_similar_candidates(candidates);
        let now_ts = chrono::Utc::now().timestamp();
        for c in merged.iter_mut() {
            normalize_faq_content(c);
        }

        let mut count = 0i64;
        for c in &merged {
            let topic_key = normalize_topic_key(&c.topic);
            // 1. 查/建 FAQ 文档（PG + Qdrant 文档元数据切片）
            let doc_id = match self.find_doc_for_topic(brand, dev_type, &topic_key).await? {
                Some(id) => id,
                None => {
                    let id = Uuid::now_v7().to_string();
                    self.documents.insert(
                        &id, 2, brand, dev_type, firmware_ver, &topic_key, "faq",
                        c.content.chars().count() as i32, 1, user_role,
                    ).await?;
                    id
                }
            };
            // 2. FAQ 正文作为文档切片 ingest 到 Qdrant（带 doc_type=faq 过滤标识）
            let mut metadata = HashMap::new();
            metadata.insert("brand".to_string(), brand.to_string());
            metadata.insert("dev_type".to_string(), dev_type.to_string());
            metadata.insert("firmware_ver".to_string(), firmware_ver.to_string());
            metadata.insert("title".to_string(), topic_key.clone());
            metadata.insert("doc_type".to_string(), "faq".to_string());
            metadata.insert("source".to_string(), "faq".to_string());
            metadata.insert("created_at".to_string(), now_ts.to_string());
            let doc = Document {
                id: Uuid::now_v7().to_string(),
                text: c.content.clone(),
                metadata,
                source_uri: None,
            };
            let stored = self.pipeline.ingest(&self.collection, &doc).await
                .map_err(|e| AppError::BusinessError(format!("FAQ 入库失败: {e}")))?;
            // 分段预览落 PG
            let rows: Vec<_> = stored.iter().enumerate().map(|(i, ch)| {
                (ch.id.clone(), doc_id.clone(), i as i32, ch.text.clone(),
                 ch.text.chars().count() as i32, ch.metadata.get("header_path").cloned().unwrap_or_default())
            }).collect();
            self.chunks.insert_batch(&rows).await.ok();
            count += stored.len() as i64;
        }
        log::info!("[commit_faqs] brand={} dev={} 提交 {} 条 FAQ", brand, dev_type, count);
        Ok(count)
    }
```

- [ ] **Step 3: `learn_single_faq`（单条 FAQ 学习，同样改 ingest）**

把原方法体中 Dify 创建段替换为复用 `commit_faqs` 的单条路径：

```rust
    pub async fn learn_single_faq(
        &self,
        brand: &str,
        dev_type: &str,
        firmware_ver: &str,
        candidate: FaqCandidate,
        user_role: &str,
    ) -> Result<i64, AppError> {
        self.commit_faqs(brand, dev_type, firmware_ver, &[candidate], user_role).await
    }
```

- [ ] **Step 4: `mark_duplicates`（改查 PG）**

把原方法体（Dify list + 比对）替换为：

```rust
    pub async fn mark_duplicates(
        &self,
        brand: &str,
        dev_type: &str,
        candidates: &[FaqCandidate],
    ) -> Result<Vec<String>, AppError> {
        let titles: Vec<String> = candidates.iter().map(|c| normalize_topic_key(&c.topic)).collect();
        let existing = self.documents.find_by_brand_dev_titles(brand, dev_type, &titles).await?;
        let existing_titles: std::collections::HashSet<String> =
            existing.into_iter().map(|(_, t)| t).collect();
        let dup: Vec<String> = candidates
            .iter()
            .filter(|c| existing_titles.contains(&normalize_topic_key(&c.topic)))
            .map(|c| c.id.clone())
            .collect();
        Ok(dup)
    }
```

- [ ] **Step 5: `generate_candidates` / `regenerate_candidates` —— 保留 LLM 提取逻辑**

这两个方法的核心（调 LLM 抽取 FAQ → `parse_faq_json` → `build_candidate` → `merge_similar_candidates`）**完全保留不动**。唯一需改：若它们内部调用了 `search`/`find_dify_doc_for_topic` 作为上下文，把调用名改为 `find_doc_for_topic`。其余逻辑零改动。

> 检查清单：grep `dify` / `client.retrieve` / `client.create` / `client.list` 在 mod.rs 中应为 0 命中。

- [ ] **Step 6: 删除 `dify_client()` 访问器与所有 DifyClient 字段引用**

- [ ] **Step 7: 更新现有单测**

原 `#[cfg(test)]` 中针对 `normalize_topic_key` / `merge_similar_candidates` / `normalize_faq_content` / `parse_faq_json` 的纯函数测试**原样保留**；若有直接测 KnowledgeManager 的集成测试（涉及 Dify），改为 `#[ignore]` 或删除。

- [ ] **Step 8: 验证编译 + 跑纯函数测试**

Run: `cargo build && cargo test --lib domain::knowledge`
Expected: 编译通过；纯函数测试全绿（集成测试因需外部服务跳过）

- [ ] **Step 9: 提交**

```bash
git add src/domain/knowledge/mod.rs
git commit -m "feat(kb): FAQ 自学习全量迁移到 Qdrant(commit_faqs/learn_single_faq/mark_duplicates)"
```

---

## Task 10: main.rs 装配 + kb.rs GraphQL 适配

**Files:**
- Modify: `src/main.rs`
- Modify: `src/server/kb.rs`

- [ ] **Step 1: main.rs 装配新组件**

找到原 `KnowledgeManager::new(...)` 装配处，替换为：

```rust
    let document_store = Arc::new(
        document_store::DocumentStore::new(db_pool.clone()).await?,
    );
    let chunk_store = Arc::new(
        chunk_store::ChunkStore::new(db_pool.clone()).await?,
    );
    let knowledge_manager = Arc::new(
        KnowledgeManager::new(config.clone(), model_store.clone(), document_store, chunk_store)?,
    );
    knowledge_manager.ensure_collection().await?;
```

（在 `src/main.rs` 顶部加 `use crate::domain::knowledge::{chunk_store, document_store};`）

- [ ] **Step 2: kb.rs GraphQL 适配**

`kb.rs` 中各 resolver 调用方法名调整（保持 GraphQL 字段名/返回 JSON 形状不变）：
- `list_dify_documents(...)` → `list_documents(...)`
- `get_document_segments(...)` 返回 `KbChunk` 列表 → 在 resolver 内映射为前端期望的 `SegmentItem` 字段（id/index/content/word_count/sign_content 等）。保持 GraphQL schema 字段名不变。
- `kbLearn` / `kbLearnCommit` / `kbLearnRegenerate` → 调用新 `commit_faqs` / `generate_candidates`（签名不变）。

> 检查：`kb.rs` 内 grep `dify` 应为 0 命中（除非有日志文案）。

- [ ] **Step 3: 验证编译**

Run: `cargo build`
Expected: 成功

- [ ] **Step 4: 提交**

```bash
git add src/main.rs src/server/kb.rs
git commit -m "feat(kb): 装配新 KnowledgeManager + GraphQL 适配"
```

---

## Task 11: 删除 Dify 残留 + 全量验证

**Files:**
- Delete: `src/domain/knowledge/dify_client.rs`
- Delete: `src/domain/knowledge/doc_meta_store.rs`
- Modify: `src/config/config.toml`（移除 `[dify]`，Task 5 已做，此处确认）
- Modify: `Cargo.toml`（移除 Dify 相关依赖，若有）

- [ ] **Step 1: 删除文件**

```bash
git rm src/domain/knowledge/dify_client.rs
git rm src/domain/knowledge/doc_meta_store.rs
```

- [ ] **Step 2: 全局搜索确认无残留**

```bash
# 期望全部为 0 命中（排除 docs/ 与本次新增的注释）
rg -i "dify" src/ --type rust
rg "DifyClient|DocMetaStore|dify_client|doc_meta_store" src/ --type rust
rg "DifyConfig|dify_url|dify_topk" src/ --type rust
```

如有命中：删除对应 import / 字段 / 配置项。

- [ ] **Step 3: 全量编译 + 类型检查**

Run: `cargo build`
Expected: 成功，无 warning（或仅 qdrant-client 未使用导入的提示）

- [ ] **Step 4: 跑全部单测**

Run: `cargo test --lib`
Expected: 纯函数测试全绿（normalize_*/merge_*/parse_faq_json/build_candidate/embedding/uuid_chunker）；`#[ignore]` 集成测试跳过

- [ ] **Step 5: lint（如有配置）**

Run: `cargo clippy -- -D warnings` 或项目约定的 lint 命令
Expected: 通过

- [ ] **Step 6: 提交**

```bash
git add -A
git commit -m "chore(kb): 移除 Dify 残留(dify_client/doc_meta_store/DifyConfig)"
```

---

## 自检（Self-Review）

**1. 设计覆盖**：检索/search_kb（Task 8）、上传（8）、删除（8）、列表（8）、分段预览（8）、FAQ 全链路（9）、Embedding 复用供应商（2+3）、Qdrant filter 下推（6）、UUID id 约束（4+6）、配置（5）、装配（10）、清理（11）。✅ 全覆盖。阶段二（Reranker/重建索引/前端 embedding 管理）明确不在 MVP。

**2. 类型一致性**：`KbFilter{brand,dev_type,doc_type:Option<String>}`（Task 6 定义）在 Task 8 search 中使用一致；`Document{id,text,metadata,source_uri}`（adk-rag）在 Task 8/9 ingest 一致；`ResolvedEmbeddingConfig{base_url,api_key,model,dimensions}`（Task 2）在 Task 8 `new` 中消费一致；`KbChunk`（Task 7）在 Task 8 `get_document_segments` 返回一致。✅

**3. 已知风险点（计划中已标注）**：qdrant-client 1.13 的 `Qdrant::new` / `delete_points` / `search_points` / `scroll` / `Payload` / `PointIdOptions` 的确切签名需对照实际 crate 编译微调（Task 6 Step 4 注释已提示）。这是唯一依赖外部 crate 版本的脆弱点，编译器会精确报错。

**4. 测试策略**：可单测的（embedding via wiremock、uuid_chunker、纯函数）走真实 `cargo test`；依赖 PG/Qdrant 的集成测试标 `#[ignore]`，实施时用本地容器 `--ignored` 验证。