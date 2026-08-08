# 基于 Qdrant 的知识库管理（替换 Dify）— 调研与设计

> ⚠️ **本方案已废弃**：「一刀切删 Dify 全上 Qdrant」的方向已改为**多 provider 并存**（Dify 外挂 + 内置 Qdrant），知识库实例化为 `kb_instances` 表、助手绑 `kb_instance_id`。最新设计见 [`docs/superpowers/specs/2026-08-02-kb-multi-provider-design.md`](../superpowers/specs/2026-08-02-kb-multi-provider-design.md)。本文保留作历史设计储备，下方内容不再反映当前代码。

> **状态**：⚠️ **历史设计储备 — 核心组件已落地，但走向与本文档设想不同**。本方案的 Embedding 薄封装（`OpenAiCompatibleEmbeddingProvider`）、`KnowledgeVectorStore`（Qdrant filter 下推）、`UuidChunker`、PG 双表（`document_store`/`chunk_store`）等核心组件，**已以多 provider 架构在 `domain/knowledge/backend/builtin.rs` 内置 provider 中实现**（见 `src/domain/knowledge/{embedding,qdrant_store,uuid_chunker,document_store,chunk_store}.rs`）。**注意**：实际并未"一刀切替换 Dify"，而是 **Dify 外挂 provider + 内置 Qdrant provider 并存**（`dify_client.rs` 保留为 `DifyProvider` 的客户端），与本文档决策②相反。下方"未实施 / 仍 100% 走 Dify"表述为方案撰写时的前瞻判断，**已不符合现状**，仅保留作设计思路参考。
> **目标读者**：cortex-agent 维护者
> **结论先行**：方案**可行**。adk-rust 通过 `adk-rag` crate（`rag` feature）已内置完整 RAG 能力，并提供 `qdrant` feature 的 `QdrantVectorStore`。基于它替换 Dify 不需要自研向量检索，只需补齐「知识库管理」这一层（文档 CRUD、元数据、分段预览、FAQ 学习）。
>
> **已确认的关键决策**（⚠️ 撰写时为前瞻判断，标"尚未执行"的表述已过时——多数已以多 provider 形态落地，详见上方状态注）：
> 1. **Embedding**：使用 **OpenAI 格式兼容** embedding（`{model,input,dimensions}` → `{data:[{embedding}]}`）。**域名从「模型供应商」解析**（`base_url` 已含 `/v1`，如 OpenAI / Ollama / SiliconFlow / GLM），仅拼 `/embeddings` 路径——与现有 chat 端点 `format!("{base_url}/chat/completions")` 约定一致。因 adk-rag 原生 `OpenAIEmbeddingProvider` 把域名硬编码为 `api.openai.com`，需 ~40 行薄封装 `OpenAiCompatibleEmbeddingProvider`（复用供应商解密凭证 + URL 拼接约定）。
> 2. **不保留 Dify**：一刀切移除 `dify_client.rs` 与 `[dify]` 配置，无灰度并行期。
> 3. **FAQ 学习随 MVP 一起迁移**：`commit_faqs` / `learn_single_faq` / `mark_duplicates` / `find_*_for_topic` 全部改为走 Qdrant，不延后。

---

## 1. 背景与目标

### 1.1 现状
当前知识库完全依赖 Dify：
- [src/domain/knowledge/dify_client.rs](../../src/domain/knowledge/dify_client.rs)：通过 Dify HTTP API 做检索/上传/删除/列表。
- [src/domain/knowledge/mod.rs](../../src/domain/knowledge/mod.rs)：`KnowledgeManager` 包装 Dify 客户端 + PG 元数据映射表。
- [src/server/kb.rs](../../src/server/kb.rs)：GraphQL `kb*` 系列接口。
- 检索入口：`search_kb` 工具（[src/tools/device_command.rs](../../src/tools/device_command.rs)）→ `KnowledgeManager::search` → Dify retrieve API。

### 1.2 痛点（为什么要替换 Dify）
1. **多一套外部依赖**：Dify 自身要部署、要配 Embedding/Rerank 模型，运维成本高。
2. **检索能力受限于 Dify API**：Dify retrieve API **不支持 metadata 过滤**，当前只能「多取 3 倍结果 + 本地过滤」brand/dev_type（见 [mod.rs:129](../../src/domain/knowledge/mod.rs)），召回质量与性能都打折。
3. **元数据管理繁琐**：要调 `ensure_metadata_fields` + `set_document_metadata` 两个接口，还要在 PG 维护映射。
4. **黑盒**：切片、Embedding、Reranking 全在 Dify 内部，难以针对性调优。
5. **数据归属**：知识沉淀在 Dify，cortex-agent 无法直接掌控向量数据。

### 1.3 目标
- 用 **Qdrant + adk-rag** 在本进程内实现等价于 Dify 的知识库能力。
- **保持对外接口不变**：`search_kb` 工具签名、GraphQL `kb*` 接口语义、前端 `KnowledgePage.vue` 交互全部不动，平滑替换底层。
- **支持原生 metadata 过滤**（brand/dev_type/doc_type），替换「本地过滤」hack。
- 复用现有「模型供应商」体系做 Embedding，不引入新的密钥管理。

---

## 2. 可行性调研

### 2.1 adk-rust 的 RAG 能力（adk-rag crate）
adk-rust `1.0.0` 提供 `rag` feature（依赖 `adk-rag 1.0.0`）。`adk-rag` 是一个 trait-based、可插拔的 RAG 系统：

| 概念 | Trait/类型 | 说明 |
|------|-----------|------|
| 文档 | `Document{id,text,metadata,source_uri}` | metadata 是 `HashMap<String,String>`，可存 brand/dev_type |
| 切片 | `Chunk{id,text,embedding,metadata,document_id}` | 切片结果 |
| 检索结果 | `SearchResult{chunk, score}` | 带相似度分数 |
| 切片器 | `Chunker`（`FixedSizeChunker`/`RecursiveChunker`/`MarkdownChunker`） | MarkdownChunker 按 `#` 标题切，保留 header_path，**非常适合 FAQ 的 6 段式模板** |
| Embedding | `EmbeddingProvider`（`embed`/`embed_batch`/`dimensions`） | trait，可自定义 |
| 向量库 | `VectorStore`（`create_collection`/`upsert`/`delete`/`search`） | trait，实现含 `QdrantVectorStore`/`InMemoryVectorStore`/`PgVectorStore`/`LanceDBVectorStore` |
| 重排 | `Reranker`（`NoOpReranker`） | 可选，MVP 用 NoOp |
| 编排 | `RagPipeline`（`ingest`=chunk→embed→store；`query`=embed→search→rerank→filter） | 组合上述组件 |
| Agent 工具 | `RagTool::new(pipeline, default_collection)` | 实现 `adk_core::Tool`，可直接挂到 Agent |

**结论**：adk-rag 提供了 RAG 所需的全部抽象，且自带 Qdrant 后端。我们不需要重写检索引擎。

### 2.2 feature 透传的关键坑（重要）
adk-rust 的 `rag` feature 定义是 `rag = ["dep:adk-rag"]`，**没有把 `qdrant`/`openai` 子 feature 透传下去**。而 `adk-rag` 的 `default = []`（不含任何后端）。

因此**只开 adk-rust 的 `rag` 是用不到 Qdrant 的**。必须在 `Cargo.toml` 里**同时直接依赖 `adk-rag`** 并显式开启后端 feature，让 cargo 做 feature 合并：

```toml
adk-rust = { version = "1", features = [ /* 现有..., */ "rag" ] }
adk-rag  = { version = "1", features = ["qdrant", "openai"] }
```

### 2.3 必须自定义的组件（两项）

调研 `adk-rag` 源码后发现两处不能「开箱即用」，需要薄封装：

**(A) Embedding Provider —— adk-rag 的 `OpenAIEmbeddingProvider` 把 URL 硬编码为 `https://api.openai.com/v1/embeddings`**（见 `openai.rs:13` 常量 + `:160` 直接引用，结构体无 `base_url` 字段、构造器无 `with_base_url`）。
本项目要求**域名从「模型供应商」解析**（OpenAI / Ollama / SiliconFlow / GLM 等都可能是 base_url），不能写死。
→ 需要实现 `OpenAiCompatibleEmbeddingProvider`，接受 `{base_url, api_key, model, dimensions}`，URL 拼法 `format!("{base_url}/embeddings")`，与现有 chat 端点 `format!("{base_url}/chat/completions")` 约定一致。请求/响应沿用 OpenAI 格式。

**(B) Qdrant 过滤检索 —— `VectorStore::search(collection, embedding, top_k)` 签名不带 filter**。adk-rag 的 `QdrantVectorStore::search` 只做纯向量检索，无法下推 brand/dev_type 过滤；且其 `upsert` 把 metadata 嵌套为 `metadata:{...}` 而非打平，过滤不直观。
→ 需要实现 `KnowledgeVectorStore`（实现 `VectorStore` trait，内部持有 `Qdrant` 客户端），在 `search`/`upsert`/`delete` 时利用 Qdrant payload filter，把元数据过滤下推到 Qdrant。

> 这两点都是「薄封装 + 复用 trait」，不是重写。`RagPipeline` 仍可正常编排 ingest；检索路径则由 `KnowledgeManager` 直接驱动「embed → 过滤 search」，绕开 `RagPipeline::query` 的无 filter 限制。

### 2.4 Qdrant point ID 约束
Qdrant 的 point id 必须是 **UUID 或无符号整数**。adk-rag 默认 chunker 生成 `{document_id}_{index}`，若 document_id 是 UUID，拼接后既不是合法 UUID 也不是数字，**upsert 时会被 Qdrant 拒绝**。
→ 设计决策：**文档 id 与 chunk id 都用 UUID**（自定义 chunker 在切片时为每个 chunk 生成新 UUID，document_id 作为 payload 字段）。删除文档时按 payload `document_id` 过滤删除，不依赖 chunk id 列表。

---

## 3. 方案可行性评估

| 维度 | 评估 | 说明 |
|------|------|------|
| 检索引擎 | ✅ 完全可行 | adk-rag `QdrantVectorStore` + 自定义 filter 封装 |
| Embedding | ✅ 可行 | ~40 行薄封装 `OpenAiCompatibleEmbeddingProvider`，OpenAI 格式 + 域名从「模型供应商」取（仅拼 `/embeddings`） |
| 切片 | ✅ 可行 | `MarkdownChunker` 天然契合 FAQ markdown；手册类用 `RecursiveChunker` |
| 元数据过滤 | ✅ 提升 | 从「本地过滤」升级为 Qdrant 原生 payload filter，召回更准 |
| 文档/分段管理 | ✅ 可行 | PG 存文档元数据 + chunk 元数据；分段预览从 Qdrant 按 doc_id 拉取 |
| FAQ 自学习 | ✅ 可行 | 现有 LLM 提取逻辑不变，写入端从 Dify 改为 Qdrant ingest |
| 接口兼容 | ✅ 可行 | GraphQL `kb*` / `search_kb` 工具签名保持不变 |
| 运维 | ✅ 下降 | 去掉 Dify，只多一个 Qdrant（单容器，比 Dify 轻得多） |

**总评：方案可行，且在召回质量、可控性、运维成本上全面优于现状。**

---

## 4. 架构设计

### 4.1 总体架构

```text
┌──────────────────────────────────────────────────────────────┐
│                        cortex-agent                          │
│                                                              │
│  Agent (device_command / custom)                             │
│    └─ Tool: search_kb  ──────────────┐                       │
│                                      ▼                       │
│  ┌────────────────────────────────────────────────────┐      │
│  │            KnowledgeManager (重写)                 │      │
│  │  search / upload / delete / list / segments /      │      │
│  │  generate_candidates / commit_faqs (FAQ 学习不变)   │      │
│  └──────┬───────────────────────────────┬─────────────┘      │
│         │                               │                    │
│         ▼                               ▼                    │
│  ┌──────────────┐           ┌────────────────────────┐       │
│  │ Embedding    │           │ KnowledgeVectorStore   │       │
│  │ OpenAiCompat │           │ (impl VectorStore,     │       │
│  │ ible (薄封装) │           │  封装 qdrant-client +   │       │
│  │ {base}/embed │           │  payload filter)       │       │
│  └──────┬───────┘           └──────────┬─────────────┘       │
│         │                   └──────────┬─────────────┘       │
│         │                              │                     │
│         │ 复用 ModelProviderStore       │                    │
│         │ (base_url/api_key/model)     │                     │
│         ▼                              ▼                     │
│  ┌──────────────┐           ┌────────────────────────┐       │
│  │ LLM 供应商    │           │       Qdrant           │       │
│  │ (DB 管理)     │           │  collection: kb_docs   │       │
│  └──────────────┘           └────────────────────────┘       │
│         │                              │                     │
│         │           ┌──────────────────┘                     │
│         ▼           ▼                                        │
│  ┌──────────────────────────┐                                │
│  │  PostgreSQL              │                                │
│  │  - kb_documents (文档元数据)                                │
│  │  - kb_chunks   (分段元数据/预览)                            │
│  └──────────────────────────┘                                │
└──────────────────────────────────────────────────────────────┘
```

### 4.2 检索流程（对比 Dify 时代）

```text
[Dify]  query → Dify /retrieve(无 filter) → 本地过滤 brand/dev_type → TopK
[新]    query → embed → Qdrant search(filter: brand/dev_type 下推) → rerank → TopK
```

### 4.3 写入流程（对比 Dify 时代）

```text
[Dify]  text → Dify create-by-text(自动切片+embed) → 记 doc_id
[新]    text → MarkdownChunker 切片 → embed_batch → Qdrant upsert(带 payload)
              → PG 记录 document + chunks 元数据
```

---

## 5. 数据模型

### 5.1 Qdrant Collection：`kb_docs`
- 距离：`Cosine`（与 adk-rag 默认一致）
- 维度：由 Embedding 模型决定（如 `bge-m3`=1024、`text-embedding-3-small`=1536），建集合时从 `provider.dimensions()` 取
- 每个 point 的 **payload** 结构：

```json
{
  "document_id": "uuid-v7",
  "text": "切片正文",
  "title": "静态路由配置",
  "brand": "H3C",
  "dev_type": "router",
  "doc_type": "manual | faq",
  "chunk_index": 0,
  "header_path": "命令格式 > ...",
  "created_at": 1783000000
}
```

> 把 brand/dev_type/doc_type **打平到 payload 顶层**（而非嵌套在 metadata 对象里），这样 Qdrant filter 直接 `match` 即可，性能最好。这与 adk-rag 默认把 metadata 嵌套为 `{metadata:{...}}` 不同，是我们自定义 store 的关键差异点。

**Qdrant Payload 索引**（建集合后创建，加速过滤）：
- `brand`（keyword）
- `dev_type`（keyword）
- `doc_type`（keyword）
- `document_id`（keyword，用于按文档删除/拉分段）

### 5.2 PostgreSQL 表

> 替换现有 `kb_doc_meta`，扩为两表。`doc_meta_store.rs` 重写为 `document_store.rs` + `chunk_store.rs`。

```sql
-- 文档元数据（替代 kb_doc_meta）
CREATE TABLE IF NOT EXISTS kb_documents (
    id           VARCHAR(36) PRIMARY KEY,        -- UUID v7，与 Qdrant document_id 一致
    doc_type     SMALLINT NOT NULL DEFAULT 1,    -- 1=上传手册, 2=FAQ
    brand        VARCHAR(64)  NOT NULL DEFAULT '',
    dev_type     VARCHAR(64)  NOT NULL DEFAULT '',
    firmware_ver VARCHAR(64)  NOT NULL DEFAULT '',
    title        VARCHAR(256) NOT NULL DEFAULT '',
    source       VARCHAR(32)  NOT NULL DEFAULT 'manual', -- manual/faq
    word_count   INTEGER      NOT NULL DEFAULT 0,
    chunk_count  INTEGER      NOT NULL DEFAULT 0,
    status       SMALLINT     NOT NULL DEFAULT 1, -- 1=正常 0=处理中 -1=失败
    uploaded_by  VARCHAR(64)  NOT NULL DEFAULT '',
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_kb_documents_brand_dev    ON kb_documents(brand, dev_type);
CREATE INDEX IF NOT EXISTS idx_kb_documents_doc_type     ON kb_documents(doc_type);
CREATE INDEX IF NOT EXISTS idx_kb_documents_title        ON kb_documents(title);

-- 分段元数据（支撑「分段预览」与列表 word_count，避免每次回查 Qdrant）
CREATE TABLE IF NOT EXISTS kb_chunks (
    id           VARCHAR(36) PRIMARY KEY,        -- UUID v7，与 Qdrant point id 一致
    document_id  VARCHAR(36) NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    chunk_index  INTEGER     NOT NULL DEFAULT 0,
    content      TEXT        NOT NULL DEFAULT '',
    word_count   INTEGER     NOT NULL DEFAULT 0,
    header_path  VARCHAR(512) NOT NULL DEFAULT '',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_kb_chunks_document ON kb_chunks(document_id);
```

---

## 6. 关键技术决策

| # | 决策点 | 选择 | 理由 |
|---|--------|------|------|
| 1 | Embedding 模型 | OpenAI 格式 + 域名从「模型供应商」取 | 复用现有供应商体系；adk-rag 原生域名硬编码，故 ~40 行薄封装 `OpenAiCompatibleEmbeddingProvider`，URL 拼 `{base_url}/embeddings`（对齐 chat 的 `{base_url}/chat/completions`） |
| 2 | 向量库 | Qdrant（gRPC `:6334`） | 用户指定；adk-rag 原生支持；payload filter 解决本地过滤痛点 |
| 3 | 切片策略 | 默认 `MarkdownChunker(1024, 100)` | FAQ 是 6 段 markdown，按 header 切保语义完整；手册可配置 `RecursiveChunker` |
| 4 | chunk id | UUID v7（每片新 UUID） | 满足 Qdrant point id 约束；删除走 payload filter 不依赖 id |
| 5 | 元数据过滤 | Qdrant payload filter（下推） | 替换 Dify 时代的「过取+本地过滤」，召回与性能双升 |
| 6 | Reranker | MVP 用 `NoOpReranker` | 先跑通；二期可接 Jina/Cohere/本地 cross-encoder |
| 7 | 检索驱动 | `KnowledgeManager` 直接 embed→filter search | 绕开 `RagPipeline::query`（其无 filter）；ingest 仍用 pipeline |
| 8 | 接口兼容 | GraphQL `kb*` / `search_kb` 签名不变 | 前端零改动，平滑替换 |
| 9 | 文档存储 | 原文不落盘（仅切片进 Qdrant + PG） | 与 Dify 一致；二期可加原文归档 |

### 6.1 风险与对策
- **Embedding 维度变更**：换模型→维度变→Qdrant collection 维度不匹配。
  - 对策：维度属于「知识库配置」，存 DB；换模型需「重建集合」操作（提供一键 reindex：遍历 PG 文档→重新切片 embed）。MVP 提示用户重建。
- **qdrant-client 版本**：adk-rag 依赖 `qdrant-client 1.13`，需与项目 reqwest/tokio 版本兼容（编译期暴露，无运行时风险）。
- **批量 upsert 性能**：大手册切片多，`embed_batch` 默认串行。对策：自定义 provider override `embed_batch` 走真正批量 `/v1/embeddings`（OpenAI 兼容 API 原生支持批量）。
- **写入半成品**：embed 失败可能只写了部分。对策：upsert 用 `wait=true`；失败时回滚 PG 记录 + 按 document_id 清理 Qdrant。

---

## 7. 模块划分（文件结构）

```text
src/domain/knowledge/
├── mod.rs                  # KnowledgeManager 重写（保持对外方法签名）
├── embedding.rs            # 【新】OpenAiCompatibleEmbeddingProvider (impl EmbeddingProvider)
├── qdrant_store.rs         # 【新】KnowledgeVectorStore (impl VectorStore, payload filter)
├── rag_pipeline.rs         # 【新】构建/持有 RagPipeline（ingest 用）
├── document_store.rs       # 【新】kb_documents 表 CRUD（替代 doc_meta_store）
├── chunk_store.rs          # 【新】kb_chunks 表 CRUD（分段预览）
└── settings.rs             # 【新】知识库运行时设置（collection/模型/切片参数），DB 持久化

删除：
└── dify_client.rs          # 移除（可选：保留作灰度回退，用 feature flag）
└── doc_meta_store.rs       # 由 document_store + chunk_store 替代
```

**对外不变点**（保证 server/tools 层不动）：
- `KnowledgeManager::search(...)` → 仍返回 `Vec<DeviceMeta>`
- `KnowledgeManager::upload_document(...) / delete_document(...) / list_dify_documents(...)（更名为 list_documents）/ get_document_segments(...)`
- `KnowledgeManager::generate_candidates(...) / regenerate_candidates(...) / commit_faqs(...)`（FAQ 学习的 LLM 提取逻辑完全保留，只换写入端）

### 7.1 Embedding Provider 关键实现

```rust
pub struct OpenAiCompatibleEmbeddingProvider {
    client: reqwest::Client,
    base_url: String,   // 来自「模型供应商」，如 https://api.openai.com/v1、http://localhost:11434/v1
    api_key: String,    // 供应商解密后的明文
    model: String,      // 如 text-embedding-3-small / bge-m3
    dimensions: usize,  // 由模型决定，持久化在 llm_models.embedding_dimensions
}

#[async_trait]
impl EmbeddingProvider for OpenAiCompatibleEmbeddingProvider {
    // 关键：URL 拼 {base_url}/embeddings，对齐 chat 的 {base_url}/chat/completions
    async fn embed(&self, text: &str) -> Result<Vec<f32>> { /* POST {base_url}/embeddings */ }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // 原生批量，input: texts[] —— 比 adk-rag 默认串行快 N 倍
    }
    fn dimensions(&self) -> usize { self.dimensions }
}
```

### 7.2 KnowledgeVectorStore 关键实现

```rust
pub struct KnowledgeVectorStore { client: Qdrant }

impl KnowledgeVectorStore {
    pub fn new(qdrant_url: &str) -> Result<Self> { ... }
    /// 按 brand/dev_type 过滤检索（下推 Qdrant Filter）
    pub async fn search_filtered(
        &self, collection: &str, embedding: &[f32], top_k: usize,
        filter: Option<KbFilter>,        // { brand, dev_type, doc_type }
    ) -> Result<Vec<SearchResult>> { /* SearchPointsBuilder + Filter */ }
    /// 按 document_id 删除该文档全部切片
    pub async fn delete_by_document(&self, collection: &str, document_id: &str) -> Result<()> {
        /* DeletePointsBuilder + Filter::match_(document_id) */
    }
    /// 按 document_id 拉取全部分段（预览/重建用）
    pub async fn scroll_by_document(&self, collection: &str, document_id: &str) -> Result<Vec<Chunk>> { ... }
}

// 同时 impl VectorStore（无 filter 版，供 RagPipeline::ingest 内部 upsert 用）
#[async_trait]
impl VectorStore for KnowledgeVectorStore { /* create_collection/delete_collection/upsert/delete/search */ }
```

> 注：`upsert` 时把 chunk.metadata 的 brand/dev_type/doc_type/title **打平**进 Qdrant payload 顶层（见 §5.1）。

---

## 8. 配置设计

### 8.1 `config/config.toml` 新增 `[kb]` 段（替代 `[dify]`）

```toml
[kb]
enabled = true
backend = "qdrant"              # 预留：qdrant | inmemory（开发联调）
qdrant_url = "http://localhost:6334"
collection = "kb_docs"
# 切片参数
chunker = "markdown"            # markdown | recursive | fixed
chunk_size = 1024
chunk_overlap = 100
# 检索参数
top_k = 6
similarity_threshold = 0.35
# Embedding：引用 DB「模型供应商」中 purpose=embedding 的模型 id
# 留空则取「默认 embedding 模型」
embedding_model_id = ""
```

### 8.2 模型供应商扩展（`llm_models` 加列）

```sql
ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS purpose SMALLINT NOT NULL DEFAULT 0;
-- 0=chat(默认), 1=embedding
-- 并增加「默认 embedding 模型」概念（可复用 is_default 语义或新增 embedding_default_id）
```

「模型供应商管理」前端增加模型用途选择 + Embedding 维度输入。`ModelProviderStore::resolve_model` 增加 `resolve_embedding_model(Option<&str>)` 方法。

### 8.3 环境变量（沿用现有约定）

| 环境变量 | 作用 |
|---------|------|
| `QDRANT_URL` | 覆盖 `kb.qdrant_url` |
| `MODEL_AES_KEY` | 现有，解密供应商 API Key |

---

## 9. 接口与兼容性

### 9.1 GraphQL `kb*` 接口（签名不变，语义不变）
保持：`kbUpload` / `kbFeedback` / `kbLearn` / `kbLearnRegenerate` / `kbLearnCommit` / `kbDocuments` / `kbDocumentSegments` / `deleteDocument`。仅底层 `KnowledgeManager` 实现替换。`kbDocuments` 的分页/brand/dev_type/keyword 入参语义不变。

### 9.2 `search_kb` 工具（签名不变）
`device_command::create_search_tool` 调用 `KnowledgeManager::search(query, brand, dev_type, ...)` 不变；brand/dev_type 由原先「本地过滤」变为「Qdrant filter 下推」，对 Agent 透明。

### 9.3 前端 `KnowledgePage.vue`
**零改动**。文档列表/分段预览/上传/FAQ 学习/反馈全部沿用现有字段。二期可增加「重建索引」「切换 embedding 模型」等增强按钮。

---

## 10. 切换策略（一刀切，不保留 Dify）

由于确认不保留 Dify，采用直接替换：

1. **依赖切换**：`Cargo.toml` 移除 `[dify]` 相关，引入 `adk-rag`（`qdrant`/`openai` feature）+ `qdrant-client`。
2. **数据搬迁（一次性脚本）**：提供 `POST /api/kb/migrate-from-dify`（或独立 bin `kb-migrate`）：
   - 遍历 Dify `list_documents` → 对每个文档 `list_segments` 拼回原文 → 经新管线 ingest 到 Qdrant + PG。
   - 搬迁完成、校验文档数与抽样检索一致后，下线 Dify 服务。
3. **代码清理**：删除 `dify_client.rs` / `doc_meta_store.rs` / `DifyConfig` / `[dify]` 配置段。
4. **存量 PG 数据迁移**：`kb_doc_meta` → `kb_documents`（doc_id 维持原 Dify id 作为 document_id，或重新生成 UUID 并建立映射）。

> 注：搬迁脚本只在「现有 Dify 有存量数据」时需要。若可接受重传，直接清空重建最简单。

---

## 11. 实施阶段

### 阶段一（MVP — 完整替换，含 FAQ）
- [ ] `Cargo.toml`：adk-rust 开 `rag`，新增 `adk-rag = { features=["qdrant","openai"] }`，加 `qdrant-client`；移除 `[dify]` 相关
- [ ] `model_provider`：`llm_models` 加 `purpose` + `embedding_dimensions` 列；`resolve_embedding_model` 方法
- [ ] `embedding.rs`：`OpenAiCompatibleEmbeddingProvider`（含原生批量，base_url 可配）
- [ ] `qdrant_store.rs`：`KnowledgeVectorStore`（filter search / delete_by_document / scroll / upsert 打平 payload）
- [ ] `document_store.rs` + `chunk_store.rs`：建表 + CRUD（替代 `doc_meta_store`）
- [ ] `settings.rs`：`[kb]` 配置加载 + 启动时 `create_collection` + payload 索引
- [ ] `mod.rs` 重写 `KnowledgeManager`：
  - search / upload / delete / list / segments（走 Qdrant + PG）
  - **FAQ 全量迁移**：`commit_faqs` / `learn_single_faq`（改 ingest）/ `mark_duplicates`（改查 PG）/ `find_*_for_topic`（改查 PG）→ 去除全部 Dify 调用
- [ ] `main.rs` / `kb.rs`：装配新 `KnowledgeManager`；GraphQL `kb*` 保持 JSON 形状不变
- [ ] **删除**：`dify_client.rs` / `doc_meta_store.rs` / `DifyConfig` / `[dify]` 配置段 / `dify_client` 模块声明
- [ ] 测试：单测（store/embedding/chunker/normalize_*）+ 集成（本地 Qdrant 容器）

### 阶段二（增强）
- [ ] Dify→Qdrant 存量数据搬迁脚本（若有存量）
- [ ] Reranker 接入（Jina/Cohere API 或 cross-encoder）
- [ ] 「重建索引」「切换 embedding 模型」管理操作
- [ ] 前端模型供应商增加 embedding 用途管理 + 维度输入

### 阶段三（收尾）
- [ ] 文档更新（DEPLOY.md / architecture.md / api.md：说明 Qdrant 部署要求）

---

## 12. 参考资料
- adk-rag crate：https://docs.rs/adk-rag
- Qdrant 文档：https://qdrant.tech/documentation/
- 现状代码：[src/domain/knowledge/](../../src/domain/knowledge/)、[src/server/kb.rs](../../src/server/kb.rs)
