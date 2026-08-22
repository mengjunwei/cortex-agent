//! 模型 CRUD 与会话可选模型视图。
//!
//! **按 user_id 隔离**：写操作以 `AND ($admin OR user_id=$caller)` 限定；默认模型的
//! 清旧/立新按归属人 user_id 作用域（每用户至多一个默认，由部分唯一索引约束）。

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::error::AppError;
use crate::infra::store_base::{Store, is_unique_violation, new_id};
use crate::domain::model_provider::dto::ModelOptionResponse;
use crate::domain::model_provider::enums::{ProviderProtocol, Status};

use super::{
    ExistsRow, ModelProviderStore, ModelRow, OwnerRow, UpdateOutcome, reassign_default_if_missing,
    reassign_default_to_any_enabled, reassign_embedding_default_if_missing,
    reassign_embedding_default_to_any_enabled, validate_field,
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
    #[allow(clippy::too_many_arguments)] // 参数即 models 表字段
    pub async fn create_model(
        &self,
        provider_id: &str,
        name: &str,
        model: &str,
        status: Status,
        tags: Vec<String>,
        embedding_dimensions: Option<i32>,
        context_window: Option<i32>,
        user_id: &str,
        is_admin: bool,
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

        // 确认供应商存在且归属 caller（模型 user_id 继承自 provider.user_id）
        let prov = diesel::sql_query(
            r#"SELECT user_id AS uid FROM llm_providers WHERE id = $1 AND ($2 OR user_id = $3)"#,
        )
        .bind::<sql_types::Text, _>(provider_id)
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<OwnerRow>(&mut conn)
        .await?;
        let Some(owner) = prov.into_iter().next() else {
            return Err(AppError::BusinessError("供应商不存在".into()));
        };

        // 始终以 is_default = FALSE 插入，避免与部分唯一索引冲突（并发安全）。
        // 默认指派由后续 reassign_default_if_missing 统一处理（按归属人 user_id 作用域）。
        diesel::sql_query(
            r#"
            INSERT INTO llm_models (id, provider_id, name, model, is_default, status, tags, embedding_dimensions, context_window, user_id)
            VALUES ($1, $2, $3, $4, FALSE, $5, $6, $7, $8, $9)
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
        .bind::<sql_types::Text, _>(&owner.uid)
        .execute(&mut conn)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::BusinessError("该供应商下已存在相同的模型ID，不可重复创建".into())
            } else {
                AppError::from(e)
            }
        })?;

        // 确保该用户存在默认模型（首个模型将自动成为默认）
        reassign_default_if_missing(&mut conn, &owner.uid).await?;

        self.refresh_cache().await?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)] // 参数即 models 表字段
    pub async fn update_model(
        &self,
        id: &str,
        name: &str,
        model: &str,
        status: Status,
        tags: Vec<String>,
        embedding_dimensions: Option<i32>,
        context_window: Option<i32>,
        user_id: &str,
        is_admin: bool,
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

        // 归属校验 + 取归属人 user_id
        let owner = diesel::sql_query(
            r#"SELECT user_id AS uid FROM llm_models WHERE id = $1 AND ($2 OR user_id = $3)"#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<OwnerRow>(&mut conn)
        .await?;
        let Some(owner) = owner.into_iter().next() else {
            return Ok(UpdateOutcome {
                updated: false,
                notice: None,
            });
        };

        // 若本次操作是要把模型从启用改为禁用，需要特殊保护该用户的默认模型 / 默认向量模型：
        //  - 若该模型是该用户默认，且存在其他已启用候选 → 自动转移默认
        //  - 若该模型是该用户默认，且没有其他已启用候选 → 拒绝禁用（保证该用户始终有默认可用）
        //  embedding_default 对称处理（否则禁用/删除默认向量模型后向量检索静默失败）
        if !status.is_enabled() {
            let is_default_and_target = diesel::sql_query(
                r#"
                SELECT 1 AS flag FROM llm_models
                WHERE id = $1 AND is_default = TRUE AND user_id = $2
                "#,
            )
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(&owner.uid)
            .get_results::<ExistsRow>(&mut conn)
            .await?;

            if !is_default_and_target.is_empty() {
                match reassign_default_to_any_enabled(&mut conn, id, None, &owner.uid).await? {
                    true => {
                        tracing::info!("[ModelProvider] 默认模型 {} 被禁用，已自动转移默认", id);
                        notice = Some(
                            "该模型原为默认模型，系统已自动将默认转移给另一个已启用的模型".into(),
                        );
                    }
                    false => {
                        return Err(AppError::BusinessError(
                            "无法禁用：该模型是您当前的默认模型，且您名下没有其他可用的已启用模型。请先启用另一个模型再禁用本模型".into(),
                        ));
                    }
                }
            }

            // 对称保护默认向量模型（embedding_default）
            let is_emb_default_and_target = diesel::sql_query(
                r#"SELECT 1 AS flag FROM llm_models
                   WHERE id = $1 AND embedding_default = TRUE AND user_id = $2"#,
            )
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(&owner.uid)
            .get_results::<ExistsRow>(&mut conn)
            .await?;
            if !is_emb_default_and_target.is_empty() {
                match reassign_embedding_default_to_any_enabled(&mut conn, id, None, &owner.uid)
                    .await?
                {
                    true => {
                        tracing::info!(
                            "[ModelProvider] 默认向量模型 {} 被禁用，已自动转移默认向量",
                            id
                        );
                        let msg = "该模型原为默认向量模型，系统已自动将默认向量转移给另一个已启用的向量模型";
                        notice = Some(match notice {
                            Some(n) => format!("{n}；{msg}"),
                            None => msg.to_string(),
                        });
                    }
                    false => {
                        return Err(AppError::BusinessError(
                            "无法禁用：该模型是您当前的默认向量模型，且您名下没有其他可用的已启用向量模型。请先启用另一个向量模型或更换默认向量模型再禁用本模型".into(),
                        ));
                    }
                }
            }
        }

        let affected = diesel::sql_query(
            r#"
            UPDATE llm_models
            SET name = $2, model = $3, status = $4, tags = $5, embedding_dimensions = $6,
                -- 禁用（status<>1）时强制清除默认标记：禁用模型不可为默认，
                -- 也关闭「并发 set_default 把正在禁用的模型重新设为默认」的竞争窗口。
                -- tags 不含 embedding 时亦清除默认向量标记（避免非 embedding 模型当默认向量）。
                is_default = CASE WHEN $4 = 1 THEN is_default ELSE FALSE END,
                embedding_default = CASE WHEN ($7 AND $4 = 1) THEN embedding_default ELSE FALSE END,
                context_window = $8,
                updated_at = NOW()
            WHERE id = $1 AND ($9 OR user_id = $10)
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
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
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

    pub async fn delete_model(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;

        // 删除前记录被删模型是否为默认/默认向量 + 归属人（按归属隔离）
        let info = diesel::sql_query(
            r#"SELECT is_default AS value, embedding_default AS emb, user_id AS uid FROM llm_models
               WHERE id = $1 AND ($2 OR user_id = $3)"#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<BoolOwnerRow>(&mut conn)
        .await?;
        let Some(info) = info.into_iter().next() else {
            return Ok(false);
        };

        let affected =
            diesel::sql_query("DELETE FROM llm_models WHERE id = $1 AND ($2 OR user_id = $3)")
                .bind::<sql_types::Text, _>(id)
                .bind::<sql_types::Bool, _>(is_admin)
                .bind::<sql_types::Text, _>(user_id)
                .execute(&mut conn)
                .await?;

        // 若删掉的是该用户的默认模型/默认向量模型，在其名下重派（并发安全）
        if affected > 0 && info.value {
            reassign_default_if_missing(&mut conn, &info.uid).await?;
        }
        if affected > 0 && info.emb {
            reassign_embedding_default_if_missing(&mut conn, &info.uid).await?;
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
    /// 若删的是该用户默认模型，事务内按用户重派；COMMIT 后刷新缓存。
    pub async fn delete_model_with_cleanup(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<super::ModelDeletionCleanup, AppError> {
        let mut conn = self.get_conn().await?;
        // 删前记录是否为默认模型/默认向量模型 + 归属人（按归属隔离）
        let info = diesel::sql_query(
            r#"SELECT is_default AS value, embedding_default AS emb, user_id AS uid FROM llm_models
               WHERE id = $1 AND ($2 OR user_id = $3)"#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<BoolOwnerRow>(&mut conn)
        .await?;
        let Some(info) = info.into_iter().next() else {
            return Ok(super::ModelDeletionCleanup {
                deleted: false,
                assistants_unbound: 0,
                sessions_unbound: 0,
                kb_instances_unbound: 0,
            });
        };

        diesel::sql_query("BEGIN").execute(&mut conn).await?;
        let tx: Result<super::ModelDeletionCleanup, AppError> = async {
            let (assistants_unbound, sessions_unbound, kb_instances_unbound) =
                super::unbind_model_references(&mut conn, &[id.to_string()]).await?;

            let aff =
                diesel::sql_query("DELETE FROM llm_models WHERE id = $1 AND ($2 OR user_id = $3)")
                    .bind::<sql_types::Text, _>(id)
                    .bind::<sql_types::Bool, _>(is_admin)
                    .bind::<sql_types::Text, _>(user_id)
                    .execute(&mut conn)
                    .await?;

            if aff > 0 && info.value {
                reassign_default_if_missing(&mut conn, &info.uid).await?;
            }
            if aff > 0 && info.emb {
                reassign_embedding_default_if_missing(&mut conn, &info.uid).await?;
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

    /// 设为默认（事务内两步切换，按归属人 user_id 作用域保证每用户唯一且并发安全）
    ///
    /// 仅允许将「模型已启用 且 所属供应商已启用」的模型设为默认。
    pub async fn set_default(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let id = id.to_string();

        // 校验目标模型存在、归属 caller、且模型与供应商均为启用状态；同时取归属人 uid
        let eligible = diesel::sql_query(
            r#"
            SELECT m.user_id AS uid FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            WHERE m.id = $1 AND m.status = 1 AND p.status = 1 AND ($2 OR m.user_id = $3)
            "#,
        )
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<OwnerRow>(&mut conn)
        .await?;
        let Some(target) = eligible.into_iter().next() else {
            return Err(AppError::BusinessError(
                "模型不存在或未启用，请先启用该模型及其供应商后再设为默认".into(),
            ));
        };

        // 事务保证两步切换的原子性与并发安全：
        // - 清除默认按归属人 user_id 作用域（每用户至多一个默认，互不干扰）。
        // - 不能用单条 UPDATE + CASE：PostgreSQL 对部分唯一索引 uq_llm_models_default
        //   逐行即时校验，若先命中目标行置 TRUE 而旧默认尚未置 FALSE，会触发唯一约束冲突。
        // - 用显式事务串行化：第一条 UPDATE 取得旧默认行的行锁，并发调用方会阻塞至本事务提交，
        //   从而保证「该用户任意时刻至多一行 is_default = TRUE」。
        diesel::sql_query("BEGIN").execute(&mut conn).await?;

        let tx_outcome: Result<(), AppError> = async {
            // 1) 清除该用户现有默认
            diesel::sql_query(
                "UPDATE llm_models SET is_default = FALSE WHERE is_default = TRUE AND user_id = $1",
            )
            .bind::<sql_types::Text, _>(&target.uid)
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

    /// 列出全部模型（供缓存刷新与供应商视图组装跨模块调用；含 user_id，不过滤归属）
    pub(super) async fn list_models(&self) -> Result<Vec<ModelRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, provider_id, name, model, user_id, is_default, status,
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

    /// 列出所有模型（含禁用），并携带供应商启用状态。
    /// 返回 (模型行, 供应商状态映射)，用于前端下拉「禁用但可见」。按归属隔离。
    async fn list_all_models_with_provider_status(
        &self,
        user_id: &str,
        admin_view: bool,
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
            WHERE $1 OR m.user_id = $2
            ORDER BY m.is_default DESC, m.created_at ASC
            "#,
        )
        .bind::<sql_types::Bool, _>(admin_view)
        .bind::<sql_types::Text, _>(user_id)
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

    /// 会话模型下拉选项（该用户默认 id + 该用户已启用模型）— 供 /api/models
    ///
    /// 仅当默认模型本身处于启用状态（模型启用 + 供应商启用）时才返回其 id，
    /// 否则返回 None，避免前端下拉框选中一个不存在的选项。
    pub async fn model_options(
        &self,
        user_id: &str,
        admin_view: bool,
    ) -> Result<(Option<String>, Vec<ModelOptionResponse>), AppError> {
        let raw_default_id = self.default_model_id(user_id);
        let rows = self
            .list_all_models_with_provider_status(user_id, admin_view)
            .await?;
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

    /// 设为默认 embedding 模型（事务内两步切换，按归属人 user_id 作用域唯一，并发安全）。
    ///
    /// 仅允许将「purpose=embedding 且 模型启用 且 供应商启用」的模型设为默认 embedding。
    pub async fn set_embedding_default(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let id = id.to_string();

        // 校验：必须是 purpose=embedding 且启用（模型 + 供应商），并归属 caller；取归属人 uid
        let eligible = diesel::sql_query(
            r#"
            SELECT m.user_id AS uid FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            WHERE m.id = $1 AND m.tags::jsonb @> '["embedding"]' AND m.status = 1 AND p.status = 1
              AND ($2 OR m.user_id = $3)
            "#,
        )
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<OwnerRow>(&mut conn)
        .await?;
        let Some(target) = eligible.into_iter().next() else {
            return Err(AppError::BusinessError(
                "模型不存在、未标记为 embedding 用途或未启用，请先在模型管理中将其设为 embedding 并启用".into(),
            ));
        };

        // 显式事务串行化两步切换，理由同 set_default（避免部分唯一索引即时校验冲突）
        diesel::sql_query("BEGIN").execute(&mut conn).await?;
        let tx_outcome: Result<(), AppError> = async {
            diesel::sql_query(
                "UPDATE llm_models SET embedding_default = FALSE WHERE embedding_default = TRUE AND user_id = $1",
            )
            .bind::<sql_types::Text, _>(&target.uid)
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

    /// 反查模型归属人 user_id（经 llm_models.user_id；供 handler/跨实体校验）
    pub async fn get_model_owner(&self, id: &str) -> Result<Option<String>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query("SELECT user_id AS uid FROM llm_models WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .get_results::<OwnerRow>(&mut conn)
            .await?;
        Ok(rows.into_iter().next().map(|r| r.uid))
    }
}

/// 双布尔（is_default + embedding_default）+ 归属人查询行
#[derive(Debug, Clone, QueryableByName)]
struct BoolOwnerRow {
    #[diesel(sql_type = sql_types::Bool)]
    value: bool,
    #[diesel(sql_type = sql_types::Bool)]
    emb: bool,
    #[diesel(sql_type = sql_types::Varchar)]
    uid: String,
}
