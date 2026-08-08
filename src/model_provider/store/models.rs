//! 模型 CRUD 与会话可选模型视图。

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::error::AppError;
use crate::infra::store_base::{Store, is_unique_violation, new_id};
use crate::model_provider::dto::ModelOptionResponse;
use crate::model_provider::enums::{ProviderProtocol, Status};

use super::{
    BoolRow, ExistsRow, ModelProviderStore, ModelRow, UpdateOutcome, reassign_default_if_missing,
    reassign_default_to_any_enabled, validate_field,
};

/// 供应商 + 模型关联查询行（用于 /api/models 列表）
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct ModelJoinRow {
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
    /// 能力标签（JSON 数组字符串）
    #[diesel(sql_type = sql_types::Text)]
    tags: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    embedding_dimensions: Option<i32>,
    #[diesel(sql_type = sql_types::Bool)]
    embedding_default: bool,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    context_window: Option<i32>,
    #[diesel(sql_type = sql_types::Varchar)]
    p_name: String,
    #[diesel(sql_type = sql_types::Varchar)]
    p_vendor: String,
    /// 供应商协议（关联自 llm_providers.protocol）
    #[diesel(sql_type = sql_types::Varchar)]
    p_protocol: String,
}

/// 带 p_status 的关联查询行（用于「禁用但可见」下拉）
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct ModelJoinWithProviderStatusRow {
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
    /// 能力标签（JSON 数组字符串）
    #[diesel(sql_type = sql_types::Text)]
    tags: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    embedding_dimensions: Option<i32>,
    #[diesel(sql_type = sql_types::Bool)]
    embedding_default: bool,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    context_window: Option<i32>,
    #[diesel(sql_type = sql_types::Varchar)]
    p_name: String,
    #[diesel(sql_type = sql_types::Varchar)]
    p_vendor: String,
    #[diesel(sql_type = sql_types::Varchar)]
    p_protocol: String,
    #[diesel(sql_type = sql_types::Int2)]
    p_status: i16,
}

impl ModelJoinWithProviderStatusRow {
    fn into_join(self) -> ModelJoinRow {
        ModelJoinRow {
            id: self.id,
            provider_id: self.provider_id,
            name: self.name,
            model: self.model,
            is_default: self.is_default,
            status: self.status,
            tags: self.tags,
            embedding_dimensions: self.embedding_dimensions,
            embedding_default: self.embedding_default,
            context_window: self.context_window,
            p_name: self.p_name,
            p_vendor: self.p_vendor,
            p_protocol: self.p_protocol,
        }
    }
}

impl ModelProviderStore {
    pub async fn create_model(
        &self,
        provider_id: &str,
        name: &str,
        model: &str,
        status: Status,
        tags: Vec<String>,
        embedding_dimensions: Option<i32>,
        context_window: Option<i32>,
    ) -> Result<String, AppError> {
        if name.trim().is_empty() || model.trim().is_empty() {
            return Err(AppError::BusinessError("模型名称/模型ID不能为空".into()));
        }
        if tags.is_empty() {
            return Err(AppError::BusinessError(
                "模型至少需要一个能力标签（如 chat）".into(),
            ));
        }
        validate_field(name, 128, "模型显示名称")?;
        validate_field(model, 128, "模型ID")?;
        let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[\"chat\"]".to_string());

        let id = new_id();
        let mut conn = self.get_conn().await?;

        // 先确认供应商存在
        let exists = diesel::sql_query("SELECT 1 AS flag FROM llm_providers WHERE id = $1")
            .bind::<sql_types::Text, _>(provider_id)
            .get_results::<ExistsRow>(&mut conn)
            .await?;
        if exists.is_empty() {
            return Err(AppError::BusinessError("供应商不存在".into()));
        }

        // 始终以 is_default = FALSE 插入，避免与部分唯一索引冲突（并发安全）。
        // 默认指派由后续 reassign_default_if_missing 统一处理。
        diesel::sql_query(
            r#"
            INSERT INTO llm_models (id, provider_id, name, model, is_default, status, tags, embedding_dimensions, context_window)
            VALUES ($1, $2, $3, $4, FALSE, $5, $6, $7, $8)
            "#,
        )
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(provider_id)
        .bind::<sql_types::Text, _>(name.trim())
        .bind::<sql_types::Text, _>(model.trim())
        .bind::<sql_types::Int2, _>(status.as_i16())
        .bind::<sql_types::Text, _>(&tags_json)
        .bind::<sql_types::Nullable<sql_types::Int4>, _>(embedding_dimensions)
        .bind::<sql_types::Nullable<sql_types::Int4>, _>(context_window)
        .execute(&mut conn)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::BusinessError("该供应商下已存在相同的模型ID，不可重复创建".into())
            } else {
                AppError::from(e)
            }
        })?;

        // 确保全局存在默认模型（首个模型将自动成为默认）
        reassign_default_if_missing(&mut conn).await?;

        self.refresh_cache().await?;
        Ok(id)
    }

    pub async fn update_model(
        &self,
        id: &str,
        name: &str,
        model: &str,
        status: Status,
        tags: Vec<String>,
        embedding_dimensions: Option<i32>,
        context_window: Option<i32>,
    ) -> Result<UpdateOutcome, AppError> {
        if name.trim().is_empty() || model.trim().is_empty() {
            return Err(AppError::BusinessError("模型名称/模型ID不能为空".into()));
        }
        if tags.is_empty() {
            return Err(AppError::BusinessError(
                "模型至少需要一个能力标签（如 chat）".into(),
            ));
        }
        validate_field(name, 128, "模型显示名称")?;
        validate_field(model, 128, "模型ID")?;
        let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[\"chat\"]".to_string());
        let has_embedding = tags.iter().any(|t| t == "embedding");

        let mut conn = self.get_conn().await?;
        let mut notice: Option<String> = None;

        // 若本次操作是要把模型从启用改为禁用，需要特殊保护默认模型：
        //  - 若该模型是全局默认，且存在其他已启用候选 → 自动转移默认
        //  - 若该模型是全局默认，且没有其他已启用候选 → 拒绝禁用（保证系统始终有默认可用）
        if !status.is_enabled() {
            let is_default_and_target = diesel::sql_query(
                r#"
                SELECT 1 AS flag FROM llm_models
                WHERE id = $1 AND is_default = TRUE
                "#,
            )
            .bind::<sql_types::Text, _>(id)
            .get_results::<ExistsRow>(&mut conn)
            .await?;

            if !is_default_and_target.is_empty() {
                match reassign_default_to_any_enabled(&mut conn, id, None).await? {
                    true => {
                        tracing::info!("[ModelProvider] 默认模型 {} 被禁用，已自动转移默认", id);
                        notice = Some(
                            "该模型原为默认模型，系统已自动将默认转移给另一个已启用的模型".into(),
                        );
                    }
                    false => {
                        return Err(AppError::BusinessError(
                            "无法禁用：该模型是当前唯一的默认模型，且系统中没有其他可用的已启用模型。请先启用另一个模型再禁用本模型".into(),
                        ));
                    }
                }
            }
        }

        let affected = diesel::sql_query(
            r#"
            UPDATE llm_models
            SET name = $2, model = $3, status = $4, tags = $5, embedding_dimensions = $6,
                -- tags 不含 embedding 时清除默认向量标记（避免非 embedding 模型当默认向量）
                embedding_default = CASE WHEN $7 THEN embedding_default ELSE FALSE END,
                context_window = $8,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(name.trim())
        .bind::<sql_types::Text, _>(model.trim())
        .bind::<sql_types::Int2, _>(status.as_i16())
        .bind::<sql_types::Text, _>(&tags_json)
        .bind::<sql_types::Nullable<sql_types::Int4>, _>(embedding_dimensions)
        .bind::<sql_types::Bool, _>(has_embedding)
        .bind::<sql_types::Nullable<sql_types::Int4>, _>(context_window)
        .execute(&mut conn)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::BusinessError("该供应商下已存在相同的模型ID，不可重复创建".into())
            } else {
                AppError::from(e)
            }
        })?;

        if affected > 0 {
            self.refresh_cache().await?;
        }
        Ok(UpdateOutcome {
            updated: affected > 0,
            notice,
        })
    }

    pub async fn delete_model(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;

        // 删除前记录被删模型是否为默认
        let info = diesel::sql_query("SELECT is_default AS value FROM llm_models WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .get_results::<BoolRow>(&mut conn)
            .await?;
        let was_default = match info.into_iter().next() {
            Some(row) => row.value,
            None => return Ok(false),
        };

        let affected = diesel::sql_query("DELETE FROM llm_models WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut conn)
            .await?;

        // 若删掉的是默认模型，自动把最早创建的剩余模型设为默认（并发安全）
        if affected > 0 && was_default {
            reassign_default_if_missing(&mut conn).await?;
        }

        if affected > 0 {
            self.refresh_cache().await?;
        }
        Ok(affected > 0)
    }

    /// 预检：统计删除该模型会牵连的引用（只读，不删除）。
    pub async fn impact_of_model_delete(
        &self,
        id: &str,
    ) -> Result<super::ModelDeletionImpact, AppError> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = sql_types::BigInt)]
            assistants: i64,
            #[diesel(sql_type = sql_types::BigInt)]
            sessions: i64,
            #[diesel(sql_type = sql_types::BigInt)]
            kb_instances: i64,
        }
        let mut conn = self.get_conn().await?;
        let row = diesel::sql_query(
            r#"SELECT
                 (SELECT COUNT(*) FROM assistants WHERE model_id = $1) AS assistants,
                 (SELECT COUNT(*) FROM session_settings WHERE model_id = $1) AS sessions,
                 (SELECT COUNT(*) FROM kb_instances WHERE provider_kind = 2 AND config::jsonb->>'embedding_model_id' = $1) AS kb_instances"#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_result::<Row>(&mut conn)
        .await?;
        Ok(super::ModelDeletionImpact {
            assistants: row.assistants,
            sessions: row.sessions,
            kb_instances: row.kb_instances,
        })
    }

    /// 删除模型并级联清理引用（单事务内，任一步失败整体回滚）。
    ///
    /// 引用清理：`assistants.model_id` 置空、`session_settings.model_id` 置 NULL（均保留引用方主体）；
    /// 若删的是默认模型，事务内重派默认；COMMIT 后刷新缓存。
    pub async fn delete_model_with_cleanup(
        &self,
        id: &str,
    ) -> Result<super::ModelDeletionCleanup, AppError> {
        let mut conn = self.get_conn().await?;
        // 删前记录是否为默认模型
        let info = diesel::sql_query("SELECT is_default AS value FROM llm_models WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .get_results::<BoolRow>(&mut conn)
            .await?;
        let was_default = match info.into_iter().next() {
            Some(row) => row.value,
            None => {
                return Ok(super::ModelDeletionCleanup {
                    deleted: false,
                    assistants_unbound: 0,
                    sessions_unbound: 0,
                    kb_instances_unbound: 0,
                })
            }
        };

        diesel::sql_query("BEGIN").execute(&mut conn).await?;
        let tx: Result<super::ModelDeletionCleanup, AppError> = async {
            let (assistants_unbound, sessions_unbound, kb_instances_unbound) =
                super::unbind_model_references(&mut conn, &[id.to_string()]).await?;

            let aff = diesel::sql_query("DELETE FROM llm_models WHERE id = $1")
                .bind::<sql_types::Text, _>(id)
                .execute(&mut conn)
                .await?;

            if aff > 0 && was_default {
                reassign_default_if_missing(&mut conn).await?;
            }
            Ok(super::ModelDeletionCleanup {
                deleted: aff > 0,
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

    /// 设为默认（事务内两步切换，保证全局唯一且并发安全）
    ///
    /// 仅允许将「模型已启用 且 所属供应商已启用」的模型设为默认。
    pub async fn set_default(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let id = id.to_string();

        // 校验目标模型存在、且模型与供应商均为启用状态
        let eligible = diesel::sql_query(
            r#"
            SELECT 1 AS flag FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            WHERE m.id = $1 AND m.status = 1 AND p.status = 1
            "#,
        )
        .bind::<sql_types::Text, _>(&id)
        .get_results::<ExistsRow>(&mut conn)
        .await?;
        if eligible.is_empty() {
            return Err(AppError::BusinessError(
                "模型不存在或未启用，请先启用该模型及其供应商后再设为默认".into(),
            ));
        }

        // 事务保证两步切换的原子性与并发安全：
        // - 不能用单条 UPDATE + CASE：PostgreSQL 对部分唯一索引 uq_llm_models_default
        //   逐行即时校验，若先命中目标行置 TRUE 而旧默认尚未置 FALSE，会触发唯一约束冲突。
        // - 不能用两条自动提交语句：并发下两次调用交错执行（各自先清空再置位）会瞬时
        //   出现两行 is_default = TRUE，导致后到调用方踩中约束冲突。
        // - 用显式事务串行化：第一条 UPDATE 取得旧默认行的行锁，并发调用方会阻塞至本事务提交，
        //   从而保证「任意时刻至多一行 is_default = TRUE」。
        //
        // 说明：未使用 `conn.transaction(|c| Box::pin(async move {...}))`。diesel-async 0.9 的
        // transaction 闭包约束为 `for<'a> AsyncFnOnce<&'a mut Self>`，而 `Box::pin` 会把 future
        // 固化为某个具体生命周期，无法满足高阶生命周期约束（HRTB）。故采用显式 BEGIN/COMMIT/ROLLBACK。
        diesel::sql_query("BEGIN").execute(&mut conn).await?;

        let tx_outcome: Result<(), AppError> = async {
            // 1) 清除现有默认
            diesel::sql_query("UPDATE llm_models SET is_default = FALSE WHERE is_default = TRUE")
                .execute(&mut conn)
                .await?;
            // 2) 设置新默认（合格性已在上方 SELECT 校验通过）
            diesel::sql_query(
                r#"
                UPDATE llm_models
                SET is_default = TRUE, updated_at = NOW()
                WHERE id = $1
                "#,
            )
            .bind::<sql_types::Text, _>(&id)
            .execute(&mut conn)
            .await?;
            Ok(())
        }
        .await;

        match tx_outcome {
            Ok(()) => {
                diesel::sql_query("COMMIT").execute(&mut conn).await?;
            }
            Err(e) => {
                // 尽力回滚；忽略回滚本身的错误（原错误优先上报，避免掩盖根因）
                let _ = diesel::sql_query("ROLLBACK").execute(&mut conn).await;
                return Err(e);
            }
        }

        self.refresh_cache().await?;
        Ok(true)
    }

    /// 列出全部模型（供缓存刷新与供应商视图组装跨模块调用）
    pub(super) async fn list_models(&self) -> Result<Vec<ModelRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, provider_id, name, model, is_default, status,
                   tags, embedding_dimensions, embedding_default, context_window,
                   created_at, updated_at
            FROM llm_models
            ORDER BY created_at ASC
            "#,
        )
        .get_results::<ModelRow>(&mut conn)
        .await?;
        Ok(rows)
    }

    /// 列出会话可选的模型（已启用，默认排首位）
    /// 列出所有模型（含禁用），并携带供应商启用状态。
    /// 返回 (模型行, 供应商状态映射)，用于前端下拉「禁用但可见」。
    async fn list_all_models_with_provider_status(
        &self,
    ) -> Result<Vec<(ModelJoinRow, i16)>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT m.id, m.provider_id, m.name, m.model, m.is_default, m.status,
                   m.tags, m.embedding_dimensions, m.embedding_default, m.context_window,
                   p.name AS p_name, p.vendor_name AS p_vendor,
                   p.protocol AS p_protocol,
                   p.status AS p_status
            FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            ORDER BY m.is_default DESC, m.created_at ASC
            "#,
        )
        .get_results::<ModelJoinWithProviderStatusRow>(&mut conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let p_status = r.p_status;
                (r.into_join(), p_status)
            })
            .collect())
    }

    /// 会话模型下拉选项（默认 id + 已启用模型）— 供 /api/models
    ///
    /// 仅当默认模型本身处于启用状态（模型启用 + 供应商启用）时才返回其 id，
    /// 否则返回 None，避免前端下拉框选中一个不存在的选项。
    pub async fn model_options(
        &self,
    ) -> Result<(Option<String>, Vec<ModelOptionResponse>), AppError> {
        let raw_default_id = self.default_model_id();
        let rows = self.list_all_models_with_provider_status().await?;
        let options: Vec<ModelOptionResponse> = rows
            .into_iter()
            .map(|(m, p_status)| ModelOptionResponse {
                id: m.id.clone(),
                name: m.name,
                model: m.model,
                provider_name: m.p_name,
                vendor_name: m.p_vendor,
                protocol: ProviderProtocol::parse(&m.p_protocol),
                is_default: m.is_default,
                // 模型或供应商任一禁用，整体视为不可用（0）
                status: if m.status == 1 && p_status == 1 { 1 } else { 0 },
                tags: super::parse_tags(&m.tags),
                embedding_default: m.embedding_default,
                context_window: m.context_window,
            })
            .collect();

        // 默认模型只返回「启用」的，避免前端选中一个禁用项
        let valid_default =
            raw_default_id.filter(|id| options.iter().any(|m| &m.id == id && m.status == 1));

        Ok((valid_default, options))
    }

    /// 设为默认 embedding 模型（事务内两步切换，全局唯一，并发安全）。
    ///
    /// 仅允许将「purpose=embedding 且 模型启用 且 供应商启用」的模型设为默认 embedding。
    pub async fn set_embedding_default(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let id = id.to_string();

        // 校验：必须是 purpose=embedding 且启用（模型 + 供应商）
        let eligible = diesel::sql_query(
            r#"
            SELECT 1 AS flag FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            WHERE m.id = $1 AND m.tags::jsonb @> '["embedding"]' AND m.status = 1 AND p.status = 1
            "#,
        )
        .bind::<sql_types::Text, _>(&id)
        .get_results::<ExistsRow>(&mut conn)
        .await?;
        if eligible.is_empty() {
            return Err(AppError::BusinessError(
                "模型不存在、未标记为 embedding 用途或未启用，请先在模型管理中将其设为 embedding 并启用".into(),
            ));
        }

        // 显式事务串行化两步切换，理由同 set_default（避免部分唯一索引即时校验冲突）
        diesel::sql_query("BEGIN").execute(&mut conn).await?;
        let tx_outcome: Result<(), AppError> = async {
            diesel::sql_query(
                "UPDATE llm_models SET embedding_default = FALSE WHERE embedding_default = TRUE",
            )
            .execute(&mut conn)
            .await?;
            diesel::sql_query(
                r#"
                UPDATE llm_models
                SET embedding_default = TRUE, updated_at = NOW()
                WHERE id = $1
                "#,
            )
            .bind::<sql_types::Text, _>(&id)
            .execute(&mut conn)
            .await?;
            Ok(())
        }
        .await;

        match tx_outcome {
            Ok(()) => {
                diesel::sql_query("COMMIT").execute(&mut conn).await?;
            }
            Err(e) => {
                let _ = diesel::sql_query("ROLLBACK").execute(&mut conn).await;
                return Err(e);
            }
        }

        self.refresh_cache().await?;
        Ok(true)
    }
}
