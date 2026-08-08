//! 缓存刷新与运行时模型解析（命中内存缓存）。

use std::collections::HashMap;

use diesel_async::RunQueryDsl;

use crate::error::AppError;
use crate::infra::store_base::Store;
use crate::model_provider::ResolvedForProbe;
use crate::model_provider::enums::{ProviderProtocol, Status};

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
                    // 对话默认（is_default）只认非 embedding 模型
                    if m.is_default && !is_embedding {
                        cache.default_id = Some(m.id.clone());
                    }
                    if is_embedding && m.embedding_default {
                        cache.embedding_default_id = Some(m.id.clone());
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

    /// 是否存在可用的（已启用）模型
    pub fn has_models(&self) -> bool {
        self.cache
            .read()
            .unwrap()
            .models
            .values()
            .any(|m| !m.model.is_empty())
    }

    /// 解析模型为运行时配置（命中内存缓存）
    ///
    /// - `model_id` 为具体模型 id 时直接解析；为空/`default`/`auto`/None 时使用默认模型
    /// - 若指定模型已被禁用或不存在，自动回退到默认模型
    /// - 若默认模型也不可用（被禁用），回退到任意一个已启用模型，避免历史会话报错
    /// - 仅返回已启用的模型
    pub fn resolve_model(&self, model_id: Option<&str>) -> anyhow::Result<ResolvedLlmConfig> {
        let guard = self.cache.read().unwrap();

        // 闭包：默认模型不可用时，回退到缓存中任意已启用模型
        let fallback_any = || -> Option<&CachedModel> {
            let m = guard.models.values().next()?;
            tracing::warn!(
                "[ModelProvider] 默认模型不可用，临时回退到模型: {} ({})，base_url={}",
                m.name,
                m.id,
                m.base_url
            );
            Some(m)
        };

        let target = match model_id.map(str::trim) {
            Some(v) if !v.is_empty() && v != "default" && v != "auto" => {
                match guard.models.get(v) {
                    Some(m) => {
                        tracing::info!(
                            "[ModelProvider] resolve_model: 命中会话指定模型 {} ({}), base_url={}",
                            m.name,
                            m.id,
                            m.base_url
                        );
                        m
                    }
                    None => {
                        tracing::warn!(
                            "[ModelProvider] resolve_model: 会话指定的模型 {} 在缓存中不存在（可能已被删除或停用），回退到默认模型",
                            v
                        );
                        guard
                            .default_id
                            .as_deref()
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
                    tracing::info!(
                        "[ModelProvider] resolve_model: model_id={:?} 被识别为 default/auto，走默认模型",
                        model_id
                    );
                } else {
                    tracing::info!(
                        "[ModelProvider] resolve_model: 请求未携带 model_id，走默认模型 default_id={:?}",
                        guard.default_id
                    );
                }
                guard
                    .default_id
                    .as_deref()
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

    /// 当前默认模型 id（供 /api/models 返回）
    pub fn default_model_id(&self) -> Option<String> {
        self.cache.read().unwrap().default_id.clone()
    }

    /// 当前默认 embedding 模型 id（供 /api/models 返回）
    pub fn embedding_default_model_id(&self) -> Option<String> {
        self.cache.read().unwrap().embedding_default_id.clone()
    }

    /// 解析 embedding 模型为运行时配置（命中内存缓存）
    ///
    /// - `model_id` 指定具体模型 id；为空/None/`default`/`auto` 时使用默认 embedding 模型
    /// - 仅返回 tags 含 `embedding` 且已启用的模型
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
                if !m.tags.iter().any(|t| t == "embedding") {
                    anyhow::bail!(
                        "模型 {} 未标记 embedding 能力（tags 不含 embedding）",
                        m.name
                    );
                }
                pick(m)
            }
            _ => {
                let id = guard.embedding_default_id.as_deref().ok_or_else(|| {
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
    /// - 模型不存在 → Err
    /// - 模型/供应商被禁用 → 仍正常返回（探测的核心场景就是测这些）
    pub async fn resolve_for_probe(&self, model_id: &str) -> Result<ResolvedForProbe, AppError> {
        let mut conn = self.get_conn().await?;

        // 一条 JOIN 查询：取模型行 + 其供应商行（不论启用状态）
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
            WHERE m.id = $1
            LIMIT 1
            "#,
        )
        .bind::<diesel::sql_types::Text, _>(model_id)
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
