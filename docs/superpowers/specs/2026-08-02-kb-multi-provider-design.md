# 多 Provider 知识库改造设计（Dify 外挂 + 内置 Qdrant）

> 状态：✅ 已落地 · 日期：2026-08-02（落地后随代码演进的差异已在对应小节标注）
> 范围：知识库从「单一 Dify」升级为「多 provider 多实例并存」，配置入库界面管理，助手绑定具体实例。
> 取代 [knowledge-base-qdrant.md](../../design/knowledge-base-qdrant.md) 的「一刀切删 Dify」方向——改为 Dify 与内置并存。

---

## 1. 背景与目标

### 1.1 现状
- 知识库 100% 依赖 Dify：`[dify]` 配置段 + `DifyClient`（ureq）+ `kb_doc_meta`（PG 元数据镜像）。
- `KnowledgeManager` 持单一 `DifyClient`，`search_kb` 工具 / GraphQL `kb*` 全走它。
- 助手只有 `knowledge_enabled` 布尔开关（全局单一知识库）。

### 1.2 目标
1. **两类 provider 并存**：Dify（外挂 HTTP）+ 内置（adk-rag 编排 + Qdrant 向量库）。
2. **多实例**：一个部署可配多个知识库实例（多个 dify + 多个内置）。
3. **助手绑定实例**：`assistant.kb_instance_id`，后端按实例路由。
4. **配置入库**：Dify 配置从 `config.toml` 搬进数据库（启动自动 seed），界面可改。
5. **差异化整合 + 抽象**：`KnowledgeProvider` trait + `ConfigFieldSpec` schema 声明驱动前端动态表单，加新 provider 类型前端零改动。
6. **历史数据不迁**：Dify 既有文档留在 Dify；`kb_doc_meta` 保留表不删，新代码不再读写。

### 1.3 非目标
- Dify→内置存量数据搬迁（用户明确不要）。
- Reranker（MVP 用 NoOp，二期接 Jina/Cohere）。
- 知识库权限/多租户。

---

## 2. 核心抽象

### 2.1 统一领域模型（不绑定 provider）
```rust
KbQuery    { query, brand?, dev_type?, topk? }
KbDoc      { id, title, brand, dev_type, content, source, word_count, ... }
KbSegment  { index, content, word_count, ... }
KbDocInput { brand, dev_type, firmware_ver?, title, content, user_role? }
KbDocPage  { data: Vec<KbDoc>, total, page, limit }
```

### 2.2 KnowledgeProvider trait（能力契约）
```rust
#[async_trait]
pub trait KnowledgeProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn health(&self) -> Result<(), AppError>;
    async fn search(&self, q: &KbQuery) -> Result<Vec<KbDoc>, AppError>;
    async fn upload(&self, input: &KbDocInput) -> Result<String, AppError>;       // 返回 doc_id
    async fn delete(&self, doc_id: &str) -> Result<(), AppError>;
    async fn list(&self, f: &KbListFilter) -> Result<KbDocPage, AppError>;
    async fn segments(&self, doc_id: &str) -> Result<Vec<KbSegment>, AppError>;
}
```
- `DifyProvider`：包现有 `DifyClient`，每个方法映射 dify API；文档真相留 dify。
- `BuiltinProvider`：`OpenAiCompatibleEmbeddingProvider` + `KnowledgeVectorStore`(Qdrant) + adk-rag chunker；文档真相在 Qdrant + PG。

### 2.3 ConfigFieldSpec —— 差异化整合的关键
每个 provider **声明**自己需要的配置字段，后端、前端共用：
```rust
pub enum FieldType { Text, Secret, Number, Url, Select }
pub struct ConfigFieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub field_type: FieldType,
    pub required: bool,
    pub default: Option<&'static str>,
    pub placeholder: Option<&'static str>,
    pub help: Option<&'static str>,
    pub options: Option<&'static [(&'static str, &'static str)]>, // Select 选项 (value,label)
}
pub trait ProviderKindSpec {
    fn kind(&self) -> ProviderKind;
    fn config_schema(&self) -> &'static [ConfigFieldSpec];
    fn validate_config(&self, cfg: &serde_json::Value) -> Result<(), AppError>;
}
```
- **Dify** schema：`base_url`(Url) · `api_key`(Secret) · `dataset_id`(Text) · `top_k`(Number)
- **Builtin** schema：`embedding_model_id`(Select, 模型供应商拉 tags 含 embedding) · `chunk_size`(Number) · `chunk_overlap`(Number) · `top_k`(Number) · `similarity_threshold`(Number)

`GET /api/kb/provider-schema` 返回所有 provider 的字段定义。前端通用动态表单按 schema 渲染。

### 2.4 Provider 工厂 + 路由
```rust
fn build_provider(inst: &KbInstance, deps: &AppDeps) -> Result<Arc<dyn KnowledgeProvider>>;
// KnowledgeManager 持 {instance_id -> Arc<dyn KnowledgeProvider>} 缓存（DashMap）
// 所有方法接收 kb_instance_id，路由到对应 provider；实例配置变更刷新缓存。
```

---

## 3. 数据模型（migrations/schema.sql）

遵循 architecture.md §8：VARCHAR(36) UUID v7 主键、SMALLINT 枚举、TEXT 存 JSON、TIMESTAMPTZ、AesCodec 加密 secret。

```sql
-- 知识库实例（每条 = 一个知识库：dify 或内置）
CREATE TABLE IF NOT EXISTS kb_instances (
    id            VARCHAR(36)  PRIMARY KEY,
    name          VARCHAR(128) NOT NULL,
    provider_kind SMALLINT     NOT NULL,            -- 1=Dify 2=Builtin
    config        TEXT         NOT NULL DEFAULT '{}', -- JSON；secret 字段 AesCodec 加密
    status        SMALLINT     NOT NULL DEFAULT 1,   -- 1=启用 0=禁用
    creator       VARCHAR(128) NOT NULL DEFAULT 'local',
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_kb_instances_kind CHECK (provider_kind IN (1,2))
);
CREATE INDEX IF NOT EXISTS idx_kb_instances_status ON kb_instances(status);

-- 内置 provider 文档元数据（dify 文档不入此表，实时调 API）
CREATE TABLE IF NOT EXISTS kb_documents (
    id            VARCHAR(36)  PRIMARY KEY,
    kb_instance_id VARCHAR(36) NOT NULL REFERENCES kb_instances(id) ON DELETE CASCADE,
    doc_type      SMALLINT     NOT NULL DEFAULT 1,  -- 1=手册 2=FAQ
    brand         VARCHAR(64)  NOT NULL DEFAULT '',
    dev_type      VARCHAR(64)  NOT NULL DEFAULT '',
    firmware_ver  VARCHAR(64)  NOT NULL DEFAULT '',
    title         VARCHAR(256) NOT NULL DEFAULT '',
    source        VARCHAR(32)  NOT NULL DEFAULT 'manual',
    word_count    INTEGER      NOT NULL DEFAULT 0,
    chunk_count   INTEGER      NOT NULL DEFAULT 0,
    status        SMALLINT     NOT NULL DEFAULT 1,
    uploaded_by   VARCHAR(64)  NOT NULL DEFAULT '',
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_kb_documents_instance ON kb_documents(kb_instance_id);
CREATE INDEX IF NOT EXISTS idx_kb_documents_brand_dev ON kb_documents(kb_instance_id, brand, dev_type);

-- 内置分段预览
CREATE TABLE IF NOT EXISTS kb_chunks (
    id            VARCHAR(36)  PRIMARY KEY,
    document_id   VARCHAR(36)  NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    chunk_index   INTEGER      NOT NULL DEFAULT 0,
    content       TEXT         NOT NULL DEFAULT '',
    word_count    INTEGER      NOT NULL DEFAULT 0,
    header_path   VARCHAR(512) NOT NULL DEFAULT '',
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_kb_chunks_document ON kb_chunks(document_id);

-- assistants：幂等升级加列（保留 knowledge_enabled 兼容旧数据；新逻辑看 kb_instance_id）
ALTER TABLE assistants ADD COLUMN IF NOT EXISTS kb_instance_id VARCHAR(36);
```

> `kb_doc_meta` 保留不动（历史数据，新代码不读写）。

---

## 4. 模块结构（src/domain/knowledge/）

```text
src/domain/knowledge/
├── mod.rs                  # KnowledgeManager 改造为路由器（持 provider 缓存，按 instance_id 路由）
├── provider/               # 【新】provider 抽象层
│   ├── mod.rs              #   KnowledgeProvider trait + 统一模型(KbDoc/KbQuery/...) + ProviderKind + 工厂
│   ├── schema.rs           #   ConfigFieldSpec / FieldType / 各 provider 的 schema 声明
│   ├── dify.rs             #   DifyProvider（包 DifyClient）
│   └── builtin.rs          #   BuiltinProvider（embedding + qdrant store + chunker）
├── embedding.rs            # 【新】OpenAiCompatibleEmbeddingProvider（域名从模型供应商取，拼 /embeddings）
├── qdrant_store.rs         # 【新】KnowledgeVectorStore（封装 qdrant-client，打平 payload + 下推 filter）
├── uuid_chunker.rs         # 【新】UuidChunker（chunk.id 改 UUID v7，满足 Qdrant point id 约束）
├── kb_instance_store.rs    # 【新】kb_instances 表 CRUD（含 secret 字段加解密）
├── document_store.rs       # 【新】kb_documents 表 CRUD（内置 provider 文档元数据）
├── chunk_store.rs          # 【新】kb_chunks 表 CRUD（内置分段预览）
├── dify_client.rs          # 保留（DifyProvider 内部用；config 层的 DifyConfig 已删，局部 config 结构下沉到此）
├── faq.rs / faq_helpers.rs / compress.rs  # 保留（FAQ 提取纯函数 + 会话压缩，被 provider 复用）
#
# 已删除（多 provider 改造清理）：
#   - doc_meta_store.rs        旧 dify 直连元数据镜像（kb_doc_meta），随 provider 化移除
#   - search.rs / document.rs  旧 dify 直连检索/文档逻辑，已并入 provider/{dify,builtin}.rs
```

### 4.1 model_provider 扩展（embedding 能力）
- `llm_models` 加列：`tags TEXT DEFAULT '["chat"]'`（JSON 数组，多选能力标签：`chat`/`embedding`/`rerank`/`reasoning`/`vision`，可扩展；**不再用 purpose 单选**）、`embedding_dimensions INT`、`embedding_default BOOLEAN`。
- `resolve_embedding_model(Option<&str>) -> ResolvedEmbeddingConfig { base_url, api_key, model, dimensions }`（按 `tags` 含 `embedding` 过滤）。
- 复用现有模型供应商体系，不引入新密钥管理。

### 4.2 配置（src/config/mod.rs）
- **`DifyConfig` 已从 config 删除**（`config.toml` 不再有 `[dify]` 段）；Dify 连接所需的局部配置结构下沉到 `dify_client.rs`，供 `DifyProvider` 从实例 config JSON 构造。
- 新增 `KbConfig`：`qdrant_url`(default localhost:6334)、`qdrant_api_key`、`default_chunk_size`、`default_chunk_overlap`、`default_top_k`、`default_similarity_threshold`。全局默认值，新建内置实例时作表单默认。

### 4.3 配置迁移（bootstrap）
- **自动 seed 已移除**：随 `[dify]` 配置段删除，bootstrap 不再自动 seed Dify 实例（`config.dify.api_key` 已不存在）。`bootstrap.rs` 只装配 `KnowledgeManager`（kb_instance_store + document_store + chunk_store）；所有知识库实例（Dify / 内置）均通过界面 `kbInstanceCreate` 创建，config JSON + api_key 加密落库 `kb_instances`。

---

## 5. GraphQL 接口（src/server/kb.rs）

新增（JSON 标量透传，符合 ADR-007）：
- `Query.kbInstances` → 列出所有实例
- `Query.kbProviderSchema` → 返回各 provider 的 ConfigFieldSpec（前端动态表单）
- `Mutation.kbInstanceCreate(input)` / `kbInstanceUpdate(id, input)` / `kbInstanceDelete(id)`
- `Mutation.kbInstanceTest(id)` → health 探测
- `Mutation.kbInstanceUpload(input)` / `Query.kbInstanceDocuments(input)` / `Query.kbInstanceSegments(instance_id, doc_id)` / `Mutation.kbInstanceDeleteDocument(instance_id, doc_id)` → 文档操作按 `kb_instance_id` 路由到对应 provider

FAQ 系列（`kbLearn` / `kbLearnRegenerate` / `kbLearnCommit`）：保留，三个接口均接收 `instance_id`（不传则取第一个启用实例），经 `KnowledgeManager::generate_candidates` / `commit_faqs` → `upload_to_instance` 写入对应实例的 provider。

> **旧 dify 直连接口已废弃删除**：`kbUpload` / `kbDocuments` / `kbDocumentSegments` / `deleteDocument` / `kbFeedback` 不再存在（前端统一走上述 instance 接口）。

---

## 6. 前端

- **KnowledgePage.vue 改造**：顶部加「知识库实例」选择/管理（下拉选当前实例 + 「管理」按钮开实例管理弹层/页）；文档列表/上传/分段都带当前 `kb_instance_id`。
- **新增实例管理**：列表 + 新建/编辑表单。表单按 `kbProviderSchema` 动态渲染（text/password/number/url/select），secret 字段编辑时掩码不回显。
- **AssistantEditPage.vue**：「知识库开关」→「选择知识库」下拉（列 `kbInstances`），存 `kb_instance_id`。
- **api.js**：加 `fetchKbInstances / createKbInstance / updateKbInstance / deleteKbInstance / fetchKbProviderSchema / testKbInstance`，文档接口加 `kb_instance_id` 参数。

---

## 7. 实施批次（每批 cargo check 通过后提交）

| 批 | 内容 |
|----|------|
| 1 | Cargo.toml：adk-rust 开 `rag`；新增 adk-rag(qdrant+openai) + qdrant-client |
| 2 | schema.sql：kb_instances + kb_documents + kb_chunks + assistants.kb_instance_id |
| 3 | model_provider：embedding 用途（purpose/dimensions/embedding_default + resolve_embedding_model + DTO） |
| 4 | 内置 RAG 组件：embedding.rs + qdrant_store.rs + uuid_chunker.rs + document_store.rs + chunk_store.rs |
| 5 | provider 抽象：provider/{mod,schema,dify,builtin}.rs + kb_instance_store.rs |
| 6 | KnowledgeManager 路由改造 + bootstrap 装配 + dify 配置迁移 seed |
| 7 | server kb.rs：kb instances CRUD + provider-schema + 文档操作带 instance_id |
| 8 | assistant：model/store/DTO 加 kb_instance_id |
| 9 | 前端：KnowledgePage 实例管理 + 动态表单 + api.js |
| 10 | 前端：AssistantEditPage 知识库下拉 |
| 11 | 验证：cargo build/test/clippy/fmt + 前端 build + 修 bug |

---

## 8. 验证标准
- [ ] 可在界面创建 dify 实例和内置实例，配置持久化、api_key 加密落库；
- [ ] 助手绑定某实例后，检索/上传走对应 provider（dify 调 API、内置走 Qdrant）；
- [ ] 切换助手绑定的实例，检索结果随之变化；
- [ ] `kbProviderSchema` 驱动前端动态表单，dify/内置字段不同；
- [ ] 既有 dify 配置首次启动自动 seed 为一条实例；
- [ ] `cargo build && cargo clippy -- -D warnings && cargo fmt --check` 全绿；前端 build 通过。

---

## 9. 关键技术风险
- **adk-rag / qdrant-client 版本与 API**：feature 透传坑（见旧设计 §2.2）；qdrant-client 1.13 的 `Qdrant::new`/`search_points`/`Payload`/`PointIdOptions` 签名需编译期对照微调。
- **embedding 维度变更**：换模型→维度变→collection 不匹配。对策：维度存模型表；换模型需重建 collection（二期提供 reindex）。
- **dify ureq vs 内置 reqwest**：DifyProvider 沿用 ureq（DifyClient 已封装好）；内置用 reqwest。两套 HTTP 客户端并存，可接受。
