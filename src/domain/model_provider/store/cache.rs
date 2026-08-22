//! 缓存刷新与运行时模型解析（命中内存缓存）。
//!
//! **按用户隔离**：`models` 跨用户扁平存储（model_id 全局唯一），每条 `CachedModel`
//! 携带 `user_id`；默认模型 id 按 `user_id` 分桶。所有解析方法接收 `user_id`，
//! 仅返回归属于该用户的模型（系统桶 `user_id=""` 仅 boot/无用户上下文场景使用），
//! 杜绝跨用户 API Key 串用。

use std::collections::HashMap;

use diesel_async::RunQueryDsl;

use crate::error::AppError;
use crate::infra::store_base::Store;
use crate::domain::model_provider::ResolvedForProbe;
use crate::domain::model_provider::enums::{ProviderProtocol, Status};

use super::{
    Cache, CachedModel, ModelProviderStore, ResolvedEmbeddingConfig, ResolvedLlmConfig, parse_tags,
};

impl ModelProviderStore {
    /// 从 DB 重新加载缓存（解密 API Key）
    pub(super) async fn refresh_cache(&self) -> Result<(), AppError> {
        let providers = self.list_providers().await?;
        let models = self.list_models().await?;

        // 解密每个供应商的 API Key（仅缓存，不外泄）
        let mut key_map: HashMap<String, (String, String, String, String, ProviderProtocol)> =
            HashMap::new();
        for p in &providers {
            if p.encrypted_key.is_empty() {
                continue;
            }
            match self.codec.decrypt(&p.encrypted_key) {
                Ok(plain) => {
                    key_map.insert(
                        p.id.clone(),
                        (
                            p.base_url.clone(),
                            plain,
                            p.name.clone(),
                            p.vendor_name.clone(),
                            ProviderProtocol::parse(&p.protocol),
                        ),
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "[ModelProvider] 供应商 {} 的 API Key 解密失败，已跳过: {}",
                        p.name,
                        e
                    );
                }
            }
        }

        let mut cache = Cache::default();
        for m in &models {
            let m_status = Status::from_i16(m.status);
            if let Some((base_url, api_key, _p_name, p_vendor, p_protocol)) =
                key_map.get(&m.provider_id)
            {
                let p_enabled = providers
                    .iter()
                    .find(|p| p.id == m.provider_id)
                    .map(|p| Status::from_i16(p.status).is_enabled())
                    .unwrap_or(false);
                let tags = parse_tags(&m.tags);
                let is_embedding = tags.iter().any(|t| t == "embedding");
                // 仅缓存「供应商启用 且 模型启用」的条目
                if p_enabled && m_status.is_enabled() {
                    // 默认模型按 user_id 分桶（每用户至多一个 chat 默认 / embedding 默认）
                    if m.is_default && !is_embedding {
                        cache.default_id.insert(m.user_id.clone(), m.id.clone());
                    }
                    if is_embedding && m.embedding_default {
                        cache
                            .embedding_default_id
                            .insert(m.user_id.clone(), m.id.clone());
                    }
                    cache.models.insert(
                        m.id.clone(),
                        CachedModel {
                            id: m.id.clone(),
                            name: m.name.clone(),
                            model: m.model.clone(),
                            vendor_name: p_vendor.clone(),
                            base_url: base_url.clone(),
                            api_key: api_key.clone(),
                            protocol: *p_protocol,
                            tags,
                            embedding_dimensions: m.embedding_dimensions,
                            context_window: m.context_window,
                            user_id: m.user_id.clone(),
                        },
                    );
                }
            }
        }

        let mut guard = self.cache.write().unwrap();
        *guard = cache;
        tracing::debug!(
            "[ModelProvider] 缓存已刷新，可解析模型数: {}",
            guard.models.len()
        );
        Ok(())
    }

    /// 指定用户是否存在可用的（已启用）模型
    pub fn has_models(&self, user_id: &str) -> bool {
        self.cache
            .read()
            .unwrap()
            .models
            .values()
            .any(|m| m.user_id == user_id && !m.model.is_empty())
    }

    /// 解析模型为运行时配置（命中内存缓存，按 `user_id` 隔离）
    ///
    /// - `model_id` 为具体模型 id 时直接解析；为空/`default`/`auto`/None 时使用该用户的默认模型
    /// - **归属校验**：仅解析 `user_id` 与调用者一致的模型；他人/系统桶模型视为不可见，回退默认
    /// - 若指定模型已被禁用/删除/不属于本用户，自动回退到该用户的默认模型
    /// - 若默认模型也不可用，回退到该用户名下任意一个已启用模型，避免历史会话报错
    pub fn resolve_model(
        &self,
        model_id: Option<&str>,
        user_id: &str,
    ) -> anyhow::Result<ResolvedLlmConfig> {
        let guard = self.cache.read().unwrap();

        // 闭包：该用户默认模型不可用时，回退到该用户名下任意已启用模型（不跨用户）
        let fallback_any = || -> Option<&CachedModel> {
            let m = guard.models.values().find(|m| m.user_id == user_id)?;
            tracing::warn!(
                "[ModelProvider] 用户 {} 的默认模型不可用，临时回退到模型: {} ({})，base_url={}",
                user_id,
                m.name,
                m.id,
                m.base_url
            );
            Some(m)
        };

        let target = match model_id.map(str::trim) {
            Some(v) if !v.is_empty() && v != "default" && v != "auto" => {
                match guard.models.get(v) {
                    // 命中且归属一致 → 直接用
                    Some(m) if m.user_id == user_id => {
                        tracing::debug!(
                            "[ModelProvider] resolve_model: 命中会话指定模型 {} ({}), base_url={}",
                            m.name,
                            m.id,
                            m.base_url
                        );
                        m
                    }
                    // 命中但不属于本用户（他人/系统桶），或不存在 → 回退该用户默认
                    _ => {
                        tracing::warn!(
                            "[ModelProvider] resolve_model: 会话指定的模型 {} 在缓存中不可见（已删除/停用/不属于本用户），回退到该用户默认模型",
                            v
                        );
                        guard
                            .default_id
                            .get(user_id)
                            .and_then(|id| guard.models.get(id))
                            .or_else(fallback_any)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "当前没有可用的模型，请先在「模型供应商管理」中配置并启用模型"
                                )
                            })?
                    }
                }
            }
            _ => {
                if model_id.map(str::trim).filter(|s| !s.is_empty()).is_some() {
                    tracing::debug!(
                        "[ModelProvider] resolve_model: model_id={:?} 被识别为 default/auto，走该用户默认模型",
                        model_id
                    );
                } else {
                    tracing::debug!(
                        "[ModelProvider] resolve_model: 请求未携带 model_id，走用户 {} 的默认模型",
                        user_id
                    );
                }
                guard
                    .default_id
                    .get(user_id)
                    .and_then(|id| guard.models.get(id))
                    .or_else(fallback_any)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "当前没有可用的模型，请先在「模型供应商管理」中配置并启用模型"
                        )
                    })?
            }
        };

        Ok(ResolvedLlmConfig {
            id: target.id.clone(),
            name: target.name.clone(),
            api_key: target.api_key.clone(),
            base_url: target.base_url.clone(),
            model: target.model.clone(),
            provider_name: target.vendor_name.clone(),
            protocol: target.protocol,
            context_window: target.context_window,
        })
    }

    /// boot 期模型解析：供无用户上下文的系统级服务（如 `query_understanding` 单例）。
    ///
    /// 与 [`resolve_model`] 的区别：系统桶（user_id=""）无可用模型时，**回退到缓存中任意
    /// 已启用模型**，避免管理员删除/禁用种子模型后服务器启动失败（修复按 user_id 隔离引入的
    /// boot 回归）。
    ///
    /// 安全性：本方法**仅供 boot**。运行时模型解析必须走 [`resolve_model`]（严格按 user_id
    /// 隔离、绝不跨桶）。本方法"任意回退"产出的单例仅作降级 fallback（per-request 主路径仍
    /// 按请求归属人解析），恢复的是归属改造前「boot 单例=全局默认」的旧行为，不引入新的跨用户
    /// API Key 泄漏。
    pub fn resolve_model_for_boot(&self) -> anyhow::Result<ResolvedLlmConfig> {
        // 1. 优先系统桶（user_id=""）默认 → 系统桶任意（严格隔离路径，绝不跨桶）
        if let Ok(m) = self.resolve_model(None, "") {
            return Ok(m);
        }
        // 2. 系统桶空（如管理员删了种子 Ollama）→ 回退缓存中任意已启用模型（boot 专属兜底）
        let guard = self.cache.read().unwrap();
        let m = guard.models.values().next().ok_or_else(|| {
            anyhow::anyhow!("当前没有任何可用模型，请先在「模型供应商管理」中配置并启用模型")
        })?;
        tracing::warn!(
            "[ModelProvider] boot 兜底：系统桶无可用模型，回退到 {} ({}, user_id={}), base_url={}",
            m.name,
            m.id,
            m.user_id,
            m.base_url
        );
        Ok(ResolvedLlmConfig {
            id: m.id.clone(),
            name: m.name.clone(),
            api_key: m.api_key.clone(),
            base_url: m.base_url.clone(),
            model: m.model.clone(),
            provider_name: m.vendor_name.clone(),
            protocol: m.protocol,
            context_window: m.context_window,
        })
    }

    /// 指定用户的默认模型 id（供 /api/models 返回）
    pub fn default_model_id(&self, user_id: &str) -> Option<String> {
        self.cache.read().unwrap().default_id.get(user_id).cloned()
    }

    /// 指定用户的默认 embedding 模型 id（供 /api/models 返回）
    pub fn embedding_default_model_id(&self, user_id: &str) -> Option<String> {
        self.cache
            .read()
            .unwrap()
            .embedding_default_id
            .get(user_id)
            .cloned()
    }

    /// 解析 embedding 模型为运行时配置（命中内存缓存，按 `user_id` 隔离）
    ///
    /// - `model_id` 指定具体模型 id；为空/None/`default`/`auto` 时使用该用户的默认 embedding 模型
    /// - 仅返回 tags 含 `embedding`、已启用、且归属于 `user_id` 的模型
    pub fn resolve_embedding_model(
        &self,
        model_id: Option<&str>,
        user_id: &str,
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
                if m.user_id != user_id {
                    anyhow::bail!("指定的 embedding 模型 {} 不可用（不属于本用户）", v);
                }
                if !m.tags.iter().any(|t| t == "embedding") {
                    anyhow::bail!(
                        "模型 {} 未标记 embedding 能力（tags 不含 embedding）",
                        m.name
                    );
                }
                pick(m)
            }
            _ => {
                let id = guard.embedding_default_id.get(user_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "未配置默认 embedding 模型，请在「模型供应商管理」中标记一个含 embedding 标签的模型并设为默认向量模型"
                    )
                })?;
                let m = guard
                    .models
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("默认 embedding 模型不可用（未启用）"))?;
                pick(m)
            }
        }
    }

    /// 探测专用解析：不走 cache、不过滤启用状态、不回退。
    ///
    /// 按 model_id 直接从 DB 取该模型 + 其供应商（解密 api_key）。
    /// **归属校验**：仅归属人/管理员可探测（`is_admin` 放开）；他人模型返回「不存在」。
    /// - 模型不存在 / 不属于本用户 → Err
    /// - 模型/供应商被禁用 → 仍正常返回（探测的核心场景就是测这些）
    pub async fn resolve_for_probe(
        &self,
        model_id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<ResolvedForProbe, AppError> {
        let mut conn = self.get_conn().await?;

        // 一条 JOIN 查询：取模型行 + 其供应商行（不论启用状态），并校验归属
        #[derive(diesel::QueryableByName)]
        struct ProbeRow {
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            model_id: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            model_name: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            model: String,
            #[diesel(sql_type = diesel::sql_types::Text)]
            tags: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            provider_name: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            vendor_name: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            base_url: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            protocol: String,
            #[diesel(sql_type = diesel::sql_types::Text)]
            encrypted_key: String,
        }

        let rows = diesel::sql_query(
            r#"
            SELECT m.id AS model_id, m.name AS model_name, m.model AS model, m.tags AS tags,
                   p.name AS provider_name, p.vendor_name AS vendor_name,
                   p.base_url AS base_url, p.protocol AS protocol, p.encrypted_key AS encrypted_key
            FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            WHERE m.id = $1 AND ($2 OR m.user_id = $3)
            LIMIT 1
            "#,
        )
        .bind::<diesel::sql_types::Text, _>(model_id)
        .bind::<diesel::sql_types::Bool, _>(is_admin)
        .bind::<diesel::sql_types::Text, _>(user_id)
        .get_results::<ProbeRow>(&mut conn)
        .await?;

        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| AppError::BusinessError("模型不存在（可能已被删除）".into()))?;

        let api_key = if row.encrypted_key.is_empty() {
            String::new()
        } else {
            self.codec.decrypt(&row.encrypted_key).map_err(|e| {
                tracing::error!(
                    "[ModelProvider] 探测解析：模型 {} 的 API Key 解密失败: {}",
                    row.model_id,
                    e
                );
                AppError::BusinessError("API Key 解密失败，请检查服务端安全配置".into())
            })?
        };

        Ok(ResolvedForProbe {
            id: row.model_id,
            name: row.model_name,
            model: row.model,
            provider_name: row.provider_name,
            vendor_name: row.vendor_name,
            base_url: row.base_url,
            api_key,
            protocol: ProviderProtocol::parse(&row.protocol),
            tags: parse_tags(&row.tags),
        })
    }
}
