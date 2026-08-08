//! 模型供应商/模型的数据存储层
//!
//! - 两张表：`llm_providers`（供应商/分组）、`llm_models`（具体模型）
//! - 主键为 UUID v7 字符串；枚举字段 `status` 以 SMALLINT 数字存储
//! - API Key 使用 AES-256-GCM 加密存储，内存缓存中保存解密后的明文供运行时使用
//! - `resolve_model` 命中内存缓存，写操作后自动刷新缓存
//!
//! 模块拆分：本文件为入口与共享定义；具体实现按职责分到子模块——
//! [`providers`]（供应商 CRUD）、[`models`]（模型 CRUD）、[`cache`]（缓存与解析）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::config::SecurityConfig;
use crate::error::AppError;
use crate::infra::db::{DbPool, DbPooledConnection};
use crate::infra::store_base::{Store, is_unique_violation, new_id};
use crate::model_provider::crypto::{AesCodec, codec_from_security};
use crate::model_provider::enums::ProviderProtocol;

mod cache;
mod models;
mod providers;

/// 解析后的 LLM 模型描述（DB 供应商存储解析产物）。
///
/// 模型选择的唯一数据源是 DB 供应商存储；该结构由 `ModelProviderStore::resolve_model` 产出。
#[derive(Debug, Clone)]
pub struct ResolvedLlmConfig {
    pub id: String,
    pub name: String,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub provider_name: String,
    /// 供应商接入协议（决定 make_model 客户端链路）
    pub protocol: ProviderProtocol,
    /// 模型上下文窗口（token），用于动态压缩阈值；None=回退配置默认
    pub context_window: Option<i32>,
}

/// 解析后的 embedding 模型描述（知识库内置 provider 向量化用）。
///
/// 复用模型供应商体系（base_url/api_key 来自供应商解密凭证），不引入新密钥管理。
#[derive(Debug, Clone)]
pub struct ResolvedEmbeddingConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub dimensions: usize,
}

/// 更新操作的返回结果
///
/// `notice` 非空时，前端应向用户展示该提示（如「默认模型已自动转移」）。
#[derive(Debug, Clone, Default)]
pub struct UpdateOutcome {
    pub updated: bool,
    pub notice: Option<String>,
}

/// 删除模型前的引用影响预检结果（只读计数）
#[derive(Debug, Clone)]
pub struct ModelDeletionImpact {
    /// 绑定该模型的助手数（assistants.model_id），删除时将置空、回退默认模型
    pub assistants: i64,
    /// 使用该模型的会话数（session_settings.model_id），删除时将置 NULL、回退默认模型
    pub sessions: i64,
    /// 引用该模型做 embedding 的内置知识库数（kb_instances.config.embedding_model_id），删除时将解绑、回退默认 embedding
    pub kb_instances: i64,
}

/// 删除模型并级联清理引用的执行结果
#[derive(Debug, Clone)]
pub struct ModelDeletionCleanup {
    pub deleted: bool,
    pub assistants_unbound: usize,
    pub sessions_unbound: usize,
    pub kb_instances_unbound: usize,
}

/// 删除供应商前的引用影响预检结果（只读计数）
#[derive(Debug, Clone)]
pub struct ProviderDeletionImpact {
    /// 其下模型数（将被 CASCADE 删除）
    pub models: i64,
    /// 绑定其下模型的助手数
    pub assistants: i64,
    /// 使用其下模型的会话数
    pub sessions: i64,
    /// 引用其下模型做 embedding 的内置知识库数
    pub kb_instances: i64,
}

/// 删除供应商并级联清理引用的执行结果
#[derive(Debug, Clone)]
pub struct ProviderDeletionCleanup {
    pub deleted: bool,
    /// 被级联删除的模型数
    pub models_removed: usize,
    pub assistants_unbound: usize,
    pub sessions_unbound: usize,
    pub kb_instances_unbound: usize,
}

// ========== DB 行结构（跨子模块共享） ==========

#[derive(Debug, Clone, QueryableByName)]
struct ProviderRow {
    #[diesel(sql_type = sql_types::Varchar)]
    id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    vendor_name: String,
    #[diesel(sql_type = sql_types::Varchar)]
    name: String,
    #[diesel(sql_type = sql_types::Varchar)]
    base_url: String,
    /// 接入协议（openai_compat / anthropic）
    #[diesel(sql_type = sql_types::Varchar)]
    protocol: String,
    #[diesel(sql_type = sql_types::Text)]
    encrypted_key: String,
    #[diesel(sql_type = sql_types::Varchar)]
    key_suffix: String,
    #[diesel(sql_type = sql_types::Int2)]
    status: i16,
    #[diesel(sql_type = sql_types::Timestamptz)]
    created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    updated_at: chrono::DateTime<chrono::Utc>,
}

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
    /// 能力标签（JSON 数组字符串，如 ["chat","reasoning"] / ["embedding"]）
    #[diesel(sql_type = sql_types::Text)]
    tags: String,
    /// embedding 维度（tags 含 embedding 时有意义）
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    embedding_dimensions: Option<i32>,
    /// 是否为默认 embedding 模型（全局至多一个，由部分唯一索引约束）
    #[diesel(sql_type = sql_types::Bool)]
    embedding_default: bool,
    /// 上下文窗口（token），用于动态压缩阈值；空=回退默认
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    context_window: Option<i32>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// 存在性检查行（`SELECT 1 AS flag FROM ...`）
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct ExistsRow {
    #[diesel(sql_type = sql_types::Integer)]
    flag: i32,
}

/// 单列 id 查询行（`SELECT id AS mid FROM ...`）
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct IdRow {
    #[diesel(sql_type = sql_types::Varchar)]
    mid: String,
}

/// 单列布尔查询行（`SELECT is_default FROM ...`）
#[derive(Debug, Clone, QueryableByName)]
struct BoolRow {
    #[diesel(sql_type = sql_types::Bool)]
    value: bool,
}

// ========== 内存缓存 ==========

#[derive(Debug, Clone)]
struct CachedModel {
    id: String,
    name: String,
    model: String,
    vendor_name: String,
    base_url: String,
    api_key: String, // 运行时解密后的明文
    protocol: ProviderProtocol,
    /// 能力标签（解析后的 Vec）
    tags: Vec<String>,
    embedding_dimensions: Option<i32>,
    /// 上下文窗口（token），用于动态压缩阈值；None=回退默认
    context_window: Option<i32>,
}

#[derive(Default)]
struct Cache {
    /// 已启用的模型（id -> 解析配置）。仅包含「供应商启用 且 模型启用」的条目。
    models: HashMap<String, CachedModel>,
    /// 全局默认 chat 模型 id（不论启用状态）
    default_id: Option<String>,
    /// 全局默认 embedding 模型 id（知识库内置 provider 用）
    embedding_default_id: Option<String>,
}

// ========== Store ==========

pub struct ModelProviderStore {
    pool: DbPool,
    codec: AesCodec,
    cache: RwLock<Cache>,
}

#[async_trait::async_trait]
impl Store for ModelProviderStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl ModelProviderStore {
    /// 初始化：加载缓存（建表由 migrations/schema.sql 负责）
    pub async fn new(pool: DbPool, security: &SecurityConfig) -> Result<Arc<Self>, AppError> {
        let codec = codec_from_security(security, "ModelProvider");

        let store = Arc::new(Self {
            pool,
            codec,
            cache: RwLock::new(Cache::default()),
        });

        // 首次初始化（表为空）时种入 Ollama 本地示例，避免进程因「无可用模型」启动失败。
        // 幂等：仅在 llm_providers 完全为空时执行，绝不覆盖用户已有配置。
        store.seed_default_if_empty().await?;
        store.refresh_cache().await?;

        tracing::info!("[ModelProvider] 初始化完成");
        Ok(store)
    }

    /// 首次启动种子：当 `llm_providers` 表为空时，种入一个 Ollama 本地示例供应商 + 模型。
    ///
    /// 设计动机：数据库初始化后表为空，`resolve_model` 找不到任何模型会导致进程启动失败
    /// （如 `query_understanding` 服务强依赖 `make_model()`）。Ollama 的 OpenAI 兼容端点
    /// 本地运行、无需 API Key，是零配置起步的最佳选择。
    ///
    /// 幂等性：用 `WHERE NOT EXISTS (SELECT 1 FROM llm_providers)` 守卫，仅在完全为空时执行。
    /// 用户一旦配置了任何供应商（或删除种子后），本函数不再介入。
    async fn seed_default_if_empty(&self) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;

        // 仅当供应商表完全为空时才种子（幂等守卫）
        let existing: Vec<ExistsRow> =
            diesel::sql_query("SELECT 1 AS flag FROM llm_providers LIMIT 1")
                .get_results::<ExistsRow>(&mut conn)
                .await?;
        if !existing.is_empty() {
            return Ok(());
        }

        tracing::info!("[ModelProvider] 检测到首次启动（无任何供应商），种入 Ollama 本地示例配置");

        // Ollama OpenAI 兼容端点：本地运行，API Key 接受任意值（这里用占位符 "ollama"）
        const OLLAMA_VENDOR: &str = "Ollama";
        const OLLAMA_NAME: &str = "Ollama 本地（示例）";
        const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
        const OLLAMA_API_KEY: &str = "ollama";
        const OLLAMA_MODEL_NAME: &str = "Qwen2.5 7B（示例）";
        const OLLAMA_MODEL_ID: &str = "qwen2.5:7b";

        let provider_id = new_id();
        let encrypted = self.codec.encrypt(OLLAMA_API_KEY).map_err(|e| {
            tracing::error!("[ModelProvider] 种子 API Key 加密失败: {}", e);
            AppError::BusinessError("种子 API Key 加密失败".into())
        })?;
        let suffix = Self::key_suffix(OLLAMA_API_KEY);

        diesel::sql_query(
            r#"
            INSERT INTO llm_providers (id, vendor_name, name, base_url, protocol, encrypted_key, key_suffix, status)
            VALUES ($1, $2, $3, $4, $5, $6, $7, 1)
            "#,
        )
        .bind::<sql_types::Text, _>(&provider_id)
        .bind::<sql_types::Text, _>(OLLAMA_VENDOR)
        .bind::<sql_types::Text, _>(OLLAMA_NAME)
        .bind::<sql_types::Text, _>(OLLAMA_BASE_URL)
        .bind::<sql_types::Text, _>(ProviderProtocol::OpenAiCompat.as_str())
        .bind::<sql_types::Text, _>(&encrypted)
        .bind::<sql_types::Text, _>(&suffix)
        .execute(&mut conn)
        .await?;

        // 种入一个示例模型并直接设为默认（首个模型即默认，符合 reassign_default_if_missing 语义）
        let model_id = new_id();
        diesel::sql_query(
            r#"
            INSERT INTO llm_models (id, provider_id, name, model, is_default, status)
            VALUES ($1, $2, $3, $4, TRUE, 1)
            "#,
        )
        .bind::<sql_types::Text, _>(&model_id)
        .bind::<sql_types::Text, _>(&provider_id)
        .bind::<sql_types::Text, _>(OLLAMA_MODEL_NAME)
        .bind::<sql_types::Text, _>(OLLAMA_MODEL_ID)
        .execute(&mut conn)
        .await?;

        tracing::info!(
            "[ModelProvider] 已种入示例：供应商「{}」+ 模型「{}」，base_url={}",
            OLLAMA_NAME,
            OLLAMA_MODEL_ID,
            OLLAMA_BASE_URL
        );
        tracing::info!(
            "[ModelProvider] 提示：请安装 Ollama 并执行 `ollama pull {}` 后即可使用；\
             或在「模型供应商管理」中替换为你的真实配置",
            OLLAMA_MODEL_ID
        );
        Ok(())
    }

    /// 计算 API Key 末 4 位掩码（仅用于前端识别，不含明文）
    fn key_suffix(plain: &str) -> String {
        let trimmed = plain.trim();
        let len = trimmed.chars().count();
        if len <= 4 {
            // 短 key 不泄露任何明文，避免通过 suffix 暴露完整密钥
            "****".to_string()
        } else {
            trimmed.chars().skip(len - 4).collect()
        }
    }
}

// ========== 工具函数（跨子模块共享） ==========

/// 解析 tags JSON 数组字符串 → Vec<String>（失败回退 `["chat"]`）
pub(super) fn parse_tags(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_else(|_| vec!["chat".to_string()])
}

/// 校验字段字符数是否在允许范围内（PostgreSQL VARCHAR 按字符计数）
fn validate_field(s: &str, max_chars: usize, field_name: &str) -> Result<(), AppError> {
    let trimmed = s.trim();
    if trimmed.chars().count() > max_chars {
        return Err(AppError::BusinessError(format!(
            "{}长度不能超过 {} 个字符",
            field_name, max_chars
        )));
    }
    Ok(())
}

/// 解绑指向给定模型 id 集合的所有引用（保留引用方主体，只清指针）。
///
/// - `assistants.model_id` 置空串 → 助手回退会话/全局默认模型
/// - `session_settings.model_id` 置 NULL → 会话回退默认模型
/// - `kb_instances.config.embedding_model_id` 移除 → 内置知识库回退默认 embedding 模型
///
/// 返回 `(受影响助手数, 解除会话模型绑定数, 解除知识库 embedding 绑定数)`。供模型/供应商删除复用。
async fn unbind_model_references(
    conn: &mut DbPooledConnection,
    model_ids: &[String],
) -> Result<(usize, usize, usize), AppError> {
    if model_ids.is_empty() {
        return Ok((0, 0, 0));
    }
    let assistants_unbound = diesel::sql_query(
        "UPDATE assistants SET model_id = '' WHERE model_id = ANY($1)",
    )
    .bind::<sql_types::Array<sql_types::Text>, _>(model_ids)
    .execute(conn)
    .await?;

    let sessions_unbound = diesel::sql_query(
        "UPDATE session_settings SET model_id = NULL, updated_at = NOW() WHERE model_id = ANY($1)",
    )
    .bind::<sql_types::Array<sql_types::Text>, _>(model_ids)
    .execute(conn)
    .await?;

    // 内置知识库（provider_kind=2）的 config 是 TEXT 存 JSON，强转 jsonb 后删 embedding_model_id 键。
    // 解绑后该知识库 resolve_embedding_model(None) 回退默认 embedding；已索引向量维度可能不再匹配，
    // 需重新向量化（由 impact 摘要提示用户）。
    let kb_unbound = diesel::sql_query(
        r#"UPDATE kb_instances
           SET config = (config::jsonb - 'embedding_model_id')::text, updated_at = NOW()
           WHERE provider_kind = 2 AND config::jsonb->>'embedding_model_id' = ANY($1)"#,
    )
    .bind::<sql_types::Array<sql_types::Text>, _>(model_ids)
    .execute(conn)
    .await?;

    Ok((assistants_unbound, sessions_unbound, kb_unbound))
}

/// 当全局无默认模型时，自动将最早创建的模型设为默认。
///
/// 并发安全：通过 `NOT EXISTS` 守卫 + 忽略部分唯一索引冲突，
/// 即使多个请求并发触发，也最多有一个成功设置默认，其余优雅跳过。
async fn reassign_default_if_missing(conn: &mut DbPooledConnection) -> Result<(), AppError> {
    match diesel::sql_query(
        r#"
        UPDATE llm_models SET is_default = TRUE
        WHERE id = (
                SELECT m.id FROM llm_models m
                INNER JOIN llm_providers p ON p.id = m.provider_id
                WHERE m.status = 1 AND p.status = 1
                ORDER BY m.created_at ASC LIMIT 1
              )
          AND NOT EXISTS (SELECT 1 FROM llm_models WHERE is_default = TRUE)
        "#,
    )
    .execute(conn)
    .await
    {
        Ok(_) => Ok(()),
        Err(e) if is_unique_violation(&e) => {
            // 并发场景：其他请求已抢先设置默认，本次跳过即可
            tracing::debug!("[ModelProvider] 默认模型已被并发请求设置，跳过");
            Ok(())
        }
        Err(e) => Err(AppError::from(e)),
    }
}

/// 把默认模型从 `exclude_id` 转移给另一个已启用模型（模型+供应商均启用）。
///
/// - 返回 `Ok(true)`：已找到候选并完成转移
/// - 返回 `Ok(false)`：没有其他可用候选（调用方应据此拒绝禁用操作）
///
/// `exclude_provider_id`：可选，排除指定供应商下的模型（用于禁用供应商时避免转移到同供应商模型）。
///
/// 并发安全：通过两步 UPDATE（先清除旧默认再设置新默认），避免单条 CASE 语句触发
/// 部分唯一索引 `uq_llm_models_default` 的逐行即时校验冲突。
async fn reassign_default_to_any_enabled(
    conn: &mut DbPooledConnection,
    exclude_id: &str,
    exclude_provider_id: Option<&str>,
) -> Result<bool, AppError> {
    // 先查找候选，便于在无候选时让调用方决定是否拒绝
    let candidates = diesel::sql_query(
        r#"
        SELECT m.id AS mid FROM llm_models m
        INNER JOIN llm_providers p ON p.id = m.provider_id
        WHERE m.id <> $1 AND m.status = 1 AND p.status = 1
          AND ($2::text IS NULL OR m.provider_id <> $2)
        ORDER BY m.created_at ASC
        LIMIT 1
        "#,
    )
    .bind::<sql_types::Text, _>(exclude_id)
    .bind::<sql_types::Nullable<sql_types::Text>, _>(exclude_provider_id)
    .get_results::<IdRow>(conn)
    .await?;

    if candidates.is_empty() {
        return Ok(false);
    }

    let candidate_id = &candidates[0].mid;

    // 第一步：清除旧默认（移除唯一索引条目，避免冲突）
    diesel::sql_query("UPDATE llm_models SET is_default = FALSE WHERE id = $1")
        .bind::<sql_types::Text, _>(exclude_id)
        .execute(conn)
        .await?;

    // 第二步：设置新默认（此时旧默认已清除，不会再触发唯一约束冲突）
    match diesel::sql_query(
        "UPDATE llm_models SET is_default = TRUE, updated_at = NOW() WHERE id = $1",
    )
    .bind::<sql_types::Text, _>(candidate_id)
    .execute(conn)
    .await
    {
        Ok(_) => Ok(true),
        Err(e) if is_unique_violation(&e) => {
            // 并发场景：其他请求已抢先设置默认，本次跳过即可
            Ok(true)
        }
        Err(e) => Err(AppError::from(e)),
    }
}
