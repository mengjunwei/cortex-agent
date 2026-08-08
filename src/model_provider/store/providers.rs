//! 供应商 CRUD 与管理后台视图组装。

use std::collections::HashMap;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::error::AppError;
use crate::infra::store_base::{Store, new_id};
use crate::model_provider::dto::{ModelResponse, ProviderResponse};
use crate::model_provider::enums::{ProviderProtocol, Status};

use super::{
    ExistsRow, IdRow, ModelProviderStore, ModelRow, ProviderRow, UpdateOutcome,
    reassign_default_if_missing, reassign_default_to_any_enabled, validate_field,
};

impl ModelProviderStore {
    pub async fn create_provider(
        &self,
        vendor_name: &str,
        name: &str,
        base_url: &str,
        api_key: &str,
        protocol: ProviderProtocol,
        status: Status,
    ) -> Result<String, AppError> {
        if vendor_name.trim().is_empty() || name.trim().is_empty() || base_url.trim().is_empty() {
            return Err(AppError::BusinessError(
                "供应商品牌/名称/地址不能为空".into(),
            ));
        }
        if api_key.trim().is_empty() {
            return Err(AppError::BusinessError("API Key 不能为空".into()));
        }
        validate_field(vendor_name, 128, "供应商品牌")?;
        validate_field(name, 128, "供应商名称")?;
        validate_field(base_url, 512, "Base URL")?;
        validate_field(api_key, 1024, "API Key")?;

        let id = new_id();
        let encrypted = self.codec.encrypt(api_key).map_err(|e| {
            tracing::error!("[ModelProvider] API Key 加密失败: {}", e);
            AppError::BusinessError("API Key 加密失败，请检查服务端安全配置".into())
        })?;
        let suffix = Self::key_suffix(api_key);

        let mut conn = self.get_conn().await?;
        let affected = diesel::sql_query(
            r#"
            INSERT INTO llm_providers (id, vendor_name, name, base_url, protocol, encrypted_key, key_suffix, status)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(vendor_name.trim())
        .bind::<sql_types::Text, _>(name.trim())
        .bind::<sql_types::Text, _>(base_url.trim())
        .bind::<sql_types::Text, _>(protocol.as_str())
        .bind::<sql_types::Text, _>(&encrypted)
        .bind::<sql_types::Text, _>(&suffix)
        .bind::<sql_types::Int2, _>(status.as_i16())
        .execute(&mut conn)
        .await
        .map_err(AppError::from)?;

        if affected == 0 {
            return Err(AppError::BusinessError("创建供应商失败".into()));
        }

        self.refresh_cache().await?;
        Ok(id)
    }

    pub async fn update_provider(
        &self,
        id: &str,
        vendor_name: &str,
        name: &str,
        base_url: &str,
        protocol: ProviderProtocol,
        status: Status,
    ) -> Result<UpdateOutcome, AppError> {
        if vendor_name.trim().is_empty() || name.trim().is_empty() || base_url.trim().is_empty() {
            return Err(AppError::BusinessError(
                "供应商品牌/名称/地址不能为空".into(),
            ));
        }
        validate_field(vendor_name, 128, "供应商品牌")?;
        validate_field(name, 128, "供应商名称")?;
        validate_field(base_url, 512, "Base URL")?;

        let mut conn = self.get_conn().await?;
        let mut notice: Option<String> = None;

        // 若本次操作要禁用供应商，而其下持有全局默认模型时，需要保护默认：
        //  - 存在其他已启用候选 → 自动转移默认
        //  - 无其他已启用候选 → 拒绝禁用
        if !status.is_enabled() {
            let default_under_provider = diesel::sql_query(
                r#"
                SELECT m.id AS mid FROM llm_models m
                WHERE m.provider_id = $1 AND m.is_default = TRUE
                LIMIT 1
                "#,
            )
            .bind::<sql_types::Text, _>(id)
            .get_results::<IdRow>(&mut conn)
            .await?;

            if let Some(row) = default_under_provider.into_iter().next() {
                match reassign_default_to_any_enabled(&mut conn, &row.mid, Some(id)).await? {
                    true => {
                        tracing::info!(
                            "[ModelProvider] 供应商 {} 被禁用，其默认模型已自动转移",
                            id
                        );
                        notice = Some(
                            "该供应商下原持有默认模型，系统已自动将默认转移给另一个已启用的模型"
                                .into(),
                        );
                    }
                    false => {
                        return Err(AppError::BusinessError(
                            "无法禁用：该供应商下持有全局默认模型，且系统中没有其他可用的已启用模型。请先启用其他供应商的模型再禁用本供应商".into(),
                        ));
                    }
                }
            }
        }

        let affected = diesel::sql_query(
            r#"
            UPDATE llm_providers
            SET vendor_name = $2, name = $3, base_url = $4, protocol = $5, status = $6, updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(vendor_name.trim())
        .bind::<sql_types::Text, _>(name.trim())
        .bind::<sql_types::Text, _>(base_url.trim())
        .bind::<sql_types::Text, _>(protocol.as_str())
        .bind::<sql_types::Int2, _>(status.as_i16())
        .execute(&mut conn)
        .await
        .map_err(AppError::from)?;

        if affected > 0 {
            self.refresh_cache().await?;
        }
        Ok(UpdateOutcome {
            updated: affected > 0,
            notice,
        })
    }

    /// 重置 API Key（只写不读）
    pub async fn reset_api_key(&self, id: &str, api_key: &str) -> Result<bool, AppError> {
        if api_key.trim().is_empty() {
            return Err(AppError::BusinessError("API Key 不能为空".into()));
        }
        validate_field(api_key, 1024, "API Key")?;
        let encrypted = self.codec.encrypt(api_key).map_err(|e| {
            tracing::error!("[ModelProvider] API Key 加密失败: {}", e);
            AppError::BusinessError("API Key 加密失败，请检查服务端安全配置".into())
        })?;
        let suffix = Self::key_suffix(api_key);

        let mut conn = self.get_conn().await?;
        let affected = diesel::sql_query(
            "UPDATE llm_providers SET encrypted_key = $2, key_suffix = $3, updated_at = NOW() WHERE id = $1",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(&encrypted)
        .bind::<sql_types::Text, _>(&suffix)
        .execute(&mut conn)
        .await?;

        if affected > 0 {
            self.refresh_cache().await?;
        }
        Ok(affected > 0)
    }

    pub async fn delete_provider(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;

        // 先确认该供应商下是否持有全局默认模型（级联删除会一并删掉它）
        let had_default = diesel::sql_query(
            r#"
            SELECT 1 AS flag FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            WHERE p.id = $1 AND m.is_default = TRUE
            LIMIT 1
            "#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_results::<ExistsRow>(&mut conn)
        .await?;
        let need_reassign = !had_default.is_empty();

        // 级联删除供应商及其下所有模型
        let affected = diesel::sql_query("DELETE FROM llm_providers WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut conn)
            .await?;

        // 若删掉的供应商持有默认模型，自动将剩余最早的模型设为默认
        if affected > 0 && need_reassign {
            reassign_default_if_missing(&mut conn).await?;
        }
        if affected > 0 {
            self.refresh_cache().await?;
        }
        Ok(affected > 0)
    }

    /// 预检：统计删除该供应商会牵连的引用（只读，不删除）。
    ///
    /// `models` = 其下模型数（将被 CASCADE 删除）；
    /// `assistants`/`sessions` = 引用其下模型的助手/会话数（删除时解绑、回退默认）。
    pub async fn impact_of_provider_delete(
        &self,
        id: &str,
    ) -> Result<super::ProviderDeletionImpact, AppError> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = sql_types::BigInt)]
            models: i64,
            #[diesel(sql_type = sql_types::BigInt)]
            assistants: i64,
            #[diesel(sql_type = sql_types::BigInt)]
            sessions: i64,
            #[diesel(sql_type = sql_types::BigInt)]
            kb_instances: i64,
        }
        let mut conn = self.get_conn().await?;
        let row = diesel::sql_query(
            r#"WITH mids AS (SELECT id FROM llm_models WHERE provider_id = $1)
               SELECT
                 (SELECT COUNT(*) FROM mids) AS models,
                 (SELECT COUNT(*) FROM assistants WHERE model_id IN (SELECT id FROM mids)) AS assistants,
                 (SELECT COUNT(*) FROM session_settings WHERE model_id IN (SELECT id FROM mids)) AS sessions,
                 (SELECT COUNT(*) FROM kb_instances WHERE provider_kind = 2 AND config::jsonb->>'embedding_model_id' IN (SELECT id FROM mids)) AS kb_instances"#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_result::<Row>(&mut conn)
        .await?;
        Ok(super::ProviderDeletionImpact {
            models: row.models,
            assistants: row.assistants,
            sessions: row.sessions,
            kb_instances: row.kb_instances,
        })
    }

    /// 删除供应商并级联清理引用（单事务内，任一步失败整体回滚）。
    ///
    /// 引用清理：先把其下所有模型的引用（assistants/session_settings.model_id）解绑，再删供应商
    /// （DB CASCADE 删其下模型）；若曾持有默认模型，事务内重派；COMMIT 后刷新缓存。
    pub async fn delete_provider_with_cleanup(
        &self,
        id: &str,
    ) -> Result<super::ProviderDeletionCleanup, AppError> {
        let mut conn = self.get_conn().await?;
        // 删前：收集其下所有模型 id + 是否持有默认模型
        let model_ids: Vec<String> = diesel::sql_query(
            "SELECT id AS mid FROM llm_models WHERE provider_id = $1",
        )
        .bind::<sql_types::Text, _>(id)
        .get_results::<IdRow>(&mut conn)
        .await?
        .into_iter()
        .map(|r| r.mid)
        .collect();

        let had_default = diesel::sql_query(
            r#"SELECT 1 AS flag FROM llm_models WHERE provider_id = $1 AND is_default = TRUE LIMIT 1"#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_results::<ExistsRow>(&mut conn)
        .await?;
        let need_reassign = !had_default.is_empty();

        let models_removed = model_ids.len();
        diesel::sql_query("BEGIN").execute(&mut conn).await?;
        let tx: Result<super::ProviderDeletionCleanup, AppError> = async {
            let (assistants_unbound, sessions_unbound, kb_instances_unbound) =
                super::unbind_model_references(&mut conn, &model_ids).await?;

            let aff = diesel::sql_query("DELETE FROM llm_providers WHERE id = $1")
                .bind::<sql_types::Text, _>(id)
                .execute(&mut conn)
                .await?;

            if aff > 0 && need_reassign {
                reassign_default_if_missing(&mut conn).await?;
            }
            Ok(super::ProviderDeletionCleanup {
                deleted: aff > 0,
                models_removed,
                assistants_unbound,
                sessions_unbound,
                kb_instances_unbound,
            })
        }
        .await;

        match tx {
            Ok(res) => {
                diesel::sql_query("COMMIT").execute(&mut conn).await?;
                if res.deleted {
                    self.refresh_cache().await?;
                }
                Ok(res)
            }
            Err(e) => {
                let _ = diesel::sql_query("ROLLBACK").execute(&mut conn).await;
                Err(e)
            }
        }
    }

    /// 列出全部供应商（含嵌套模型，无明文密钥）
    pub(super) async fn list_providers(&self) -> Result<Vec<ProviderRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, vendor_name, name, base_url, protocol, encrypted_key, key_suffix, status, created_at, updated_at
            FROM llm_providers
            ORDER BY created_at ASC
            "#,
        )
        .get_results::<ProviderRow>(&mut conn)
        .await?;
        Ok(rows)
    }

    /// 供应商列表（含嵌套模型，无明文密钥）— 供管理后台
    pub async fn list_providers_with_models(&self) -> Result<Vec<ProviderResponse>, AppError> {
        let providers = self.list_providers().await?;
        let models = self.list_models().await?;

        let mut by_provider: HashMap<String, Vec<ModelRow>> = HashMap::new();
        for m in models {
            by_provider
                .entry(m.provider_id.clone())
                .or_default()
                .push(m);
        }

        let mut out = Vec::with_capacity(providers.len());
        for p in providers {
            let provider_name = p.name.clone();
            let vendor_name = p.vendor_name.clone();
            let models_resp: Vec<ModelResponse> = by_provider
                .remove(&p.id)
                .unwrap_or_default()
                .into_iter()
                .map(|m| ModelResponse {
                    provider_name: provider_name.clone(),
                    vendor_name: vendor_name.clone(),
                    protocol: ProviderProtocol::parse(&p.protocol),
                    id: m.id,
                    provider_id: m.provider_id,
                    name: m.name,
                    model: m.model,
                    is_default: m.is_default,
                    status: Status::from_i16(m.status),
                    tags: super::parse_tags(&m.tags),
                    embedding_dimensions: m.embedding_dimensions,
                    context_window: m.context_window,
                    embedding_default: m.embedding_default,
                    created_at: m.created_at.to_rfc3339(),
                    updated_at: m.updated_at.to_rfc3339(),
                })
                .collect();

            out.push(ProviderResponse {
                id: p.id,
                vendor_name: p.vendor_name,
                name: p.name,
                base_url: p.base_url,
                protocol: ProviderProtocol::parse(&p.protocol),
                api_key_suffix: p.key_suffix,
                status: Status::from_i16(p.status),
                created_at: p.created_at.to_rfc3339(),
                updated_at: p.updated_at.to_rfc3339(),
                models: models_resp,
            });
        }
        Ok(out)
    }
}
