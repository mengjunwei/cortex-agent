//! 模型供应商管理 — 已迁移至 GraphQL
//!
//! ## GraphQL 字段
//!
//! | 字段 | 类型 | 说明 |
//! |------|------|------|
//! | `modelProviders` | Query | 供应商列表（含嵌套模型，无明文密钥） |
//! | `createModelProvider` | Mutation | 新建供应商 |
//! | `updateModelProvider` | Mutation | 编辑供应商（不含密钥） |
//! | `deleteModelProvider` | Mutation | 删除供应商（级联模型） |
//! | `resetModelProviderKey` | Mutation | 重置 API Key |
//! | `createModel` | Mutation | 新建模型 |
//! | `updateModel` | Mutation | 编辑模型 |
//! | `deleteModel` | Mutation | 删除模型 |
//! | `setDefaultModel` | Mutation | 设为默认 |
//!
//! 安全约定：API Key 仅在「新建/重置」时经 input 接收，永不返回前端。

use serde_json::{Value, json};

use crate::error::AppError;
use crate::domain::model_provider::dto::{
    CreateModelRequest, CreateProviderRequest, ProbeModelsInput, ResetKeyRequest,
    UpdateModelRequest, UpdateProviderRequest,
};
use crate::domain::model_provider::store::UpdateOutcome;
use crate::server::AppState;

// 统一响应封装（见 `server::response`）
use super::response;
use super::response::code;

/// 成功响应：业务 payload 整体进 `data`
fn ok(data: Value) -> Value {
    response::ok(data)
}

/// 把 `UpdateOutcome` 渲染为标准响应；`updated=false` 表示资源不存在
fn ok_update(outcome: UpdateOutcome) -> Value {
    if outcome.updated {
        let mut payload = json!({ "updated": true });
        if let Some(notice) = outcome.notice
            && let Some(m) = payload.as_object_mut()
        {
            m.insert("notice".into(), Value::String(notice));
        }
        ok(payload)
    } else {
        response::err(code::NOT_FOUND, "资源不存在")
    }
}

/// 失败响应：映射 `AppError` → 业务错误码，剔除「业务逻辑错误:」前缀
fn fail(e: &AppError) -> Value {
    response::from_app_error(e)
}

/// 数据库未启用
fn db_unavailable() -> Value {
    response::err(code::DATABASE, "数据库不可用")
}

/// 供应商列表（管理后台，按归属隔离）
pub async fn list_providers(state: &AppState, user_id: &str, is_admin: bool) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store.list_providers_with_models(user_id, is_admin).await {
        Ok(list) => {
            let mut items = serde_json::to_value(&list).unwrap_or_else(|_| json!([]));
            if let Some(arr) = items.as_array_mut() {
                super::owner::inject_owners(state.db_pool.as_ref(), is_admin, arr, "user_id").await;
            }
            ok(json!({ "providers": items }))
        }
        Err(e) => fail(&e),
    }
}

/// 新建供应商（归属=当前用户）
pub async fn create_provider(state: &AppState, req: CreateProviderRequest, user_id: &str) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store
        .create_provider(
            &req.vendor_name,
            &req.name,
            &req.base_url,
            &req.api_key,
            req.protocol,
            req.status,
            user_id,
        )
        .await
    {
        Ok(id) => ok(json!({ "id": id })),
        Err(e) => fail(&e),
    }
}

/// 编辑供应商（不含密钥，归属人/管理员）
pub async fn update_provider(
    state: &AppState,
    id: &str,
    req: UpdateProviderRequest,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store
        .update_provider(
            id,
            &req.vendor_name,
            &req.name,
            &req.base_url,
            req.protocol,
            req.status,
            user_id,
            is_admin,
        )
        .await
    {
        Ok(outcome) => ok_update(outcome),
        Err(e) => fail(&e),
    }
}

/// 删除供应商（force 省略/false=仅预检返回影响清单；force=true=执行级联清理+删除）
/// 仅归属人/管理员可操作；非归属人预检/删除均按 NOT_FOUND 返回。
pub async fn delete_provider(
    state: &AppState,
    id: &str,
    force: bool,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    // 预检前先做归属校验，非归属人直接 NOT_FOUND（不泄露存在性）
    if !is_admin {
        match store.get_provider_owner(id).await {
            Ok(Some(owner)) if owner == user_id => {}
            _ => return response::err(code::NOT_FOUND, "供应商不存在"),
        }
    }
    if !force {
        match store.impact_of_provider_delete(id).await {
            Ok(impact) => ok(json!({
                "deleted": false,
                "impact": {
                    "models": impact.models,
                    "assistants": impact.assistants,
                    "sessions": impact.sessions,
                },
                "summary": summarize_provider_impact(&impact),
            })),
            Err(e) => fail(&e),
        }
    } else {
        match store
            .delete_provider_with_cleanup(id, user_id, is_admin)
            .await
        {
            Ok(res) if res.deleted => ok(json!({
                "deleted": true,
                "cleanup": {
                    "models_removed": res.models_removed,
                    "assistants_unbound": res.assistants_unbound,
                    "sessions_unbound": res.sessions_unbound,
                },
            })),
            Ok(_) => response::err(code::NOT_FOUND, "供应商不存在"),
            Err(e) => fail(&e),
        }
    }
}

/// 重置 API Key（只写不读，归属人/管理员）
pub async fn reset_key(
    state: &AppState,
    id: &str,
    req: ResetKeyRequest,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store
        .reset_api_key(id, &req.api_key, user_id, is_admin)
        .await
    {
        Ok(true) => ok(json!({ "reset": true })),
        Ok(false) => response::err(code::NOT_FOUND, "供应商不存在"),
        Err(e) => fail(&e),
    }
}

/// 新建模型（隶属于某供应商，归属继承自供应商）
pub async fn create_model(
    state: &AppState,
    provider_id: &str,
    req: CreateModelRequest,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store
        .create_model(
            provider_id,
            &req.name,
            &req.model,
            req.status,
            req.tags.clone(),
            req.embedding_dimensions,
            req.context_window,
            user_id,
            is_admin,
        )
        .await
    {
        Ok(id) => ok(json!({ "id": id })),
        Err(e) => fail(&e),
    }
}

/// 编辑模型（归属人/管理员）
pub async fn update_model(
    state: &AppState,
    id: &str,
    req: UpdateModelRequest,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store
        .update_model(
            id,
            &req.name,
            &req.model,
            req.status,
            req.tags.clone(),
            req.embedding_dimensions,
            req.context_window,
            user_id,
            is_admin,
        )
        .await
    {
        Ok(outcome) => ok_update(outcome),
        Err(e) => fail(&e),
    }
}

/// 删除模型（force 省略/false=仅预检返回影响清单；force=true=执行级联清理+删除）
/// 仅归属人/管理员可操作；非归属人预检/删除均按 NOT_FOUND 返回。
pub async fn delete_model(
    state: &AppState,
    id: &str,
    force: bool,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    // 预检前先做归属校验，非归属人直接 NOT_FOUND（不泄露存在性）
    if !is_admin {
        match store.get_model_owner(id).await {
            Ok(Some(owner)) if owner == user_id => {}
            _ => return response::err(code::NOT_FOUND, "模型不存在"),
        }
    }
    if !force {
        match store.impact_of_model_delete(id).await {
            Ok(impact) => ok(json!({
                "deleted": false,
                "impact": {
                    "assistants": impact.assistants,
                    "sessions": impact.sessions,
                },
                "summary": summarize_model_impact(&impact),
            })),
            Err(e) => fail(&e),
        }
    } else {
        match store.delete_model_with_cleanup(id, user_id, is_admin).await {
            Ok(res) if res.deleted => ok(json!({
                "deleted": true,
                "cleanup": {
                    "assistants_unbound": res.assistants_unbound,
                    "sessions_unbound": res.sessions_unbound,
                },
            })),
            Ok(_) => response::err(code::NOT_FOUND, "模型不存在"),
            Err(e) => fail(&e),
        }
    }
}

/// 设为默认模型（归属人/管理员，按用户作用域）
pub async fn set_default(state: &AppState, id: &str, user_id: &str, is_admin: bool) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store.set_default(id, user_id, is_admin).await {
        Ok(true) => ok(json!({ "default": true })),
        Ok(false) => response::err(code::NOT_FOUND, "模型不存在"),
        Err(e) => fail(&e),
    }
}

/// 设为默认 embedding 模型（知识库内置 provider 用，按用户作用域唯一）
pub async fn set_embedding_default(
    state: &AppState,
    id: &str,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    match store.set_embedding_default(id, user_id, is_admin).await {
        Ok(true) => ok(json!({ "embedding_default": true })),
        Ok(false) => response::err(code::NOT_FOUND, "模型不存在"),
        Err(e) => fail(&e),
    }
}

/// 批量探测模型存活（全并发，单模型 30s 超时，按归属隔离）
pub async fn probe_models(
    state: &AppState,
    req: ProbeModelsInput,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    if req.ids.is_empty() {
        return response::err(code::INVALID_PARAMS, "ids 不能为空");
    }

    // 全并发：每个 id 独立 resolve + probe + 超时（非归属人的 id 解析失败 → Fail）
    let futs = req
        .ids
        .iter()
        .map(|id| probe_one_id(store, id, user_id, is_admin));
    let results = futures::future::join_all(futs).await;
    ok(json!({ "results": results }))
}

/// 单个 id 的探测（resolve 失败也产出 Fail 结果，不阻断整体）
async fn probe_one_id(
    store: &crate::domain::model_provider::store::ModelProviderStore,
    id: &str,
    user_id: &str,
    is_admin: bool,
) -> crate::domain::model_provider::ProbeResult {
    match store.resolve_for_probe(id, user_id, is_admin).await {
        Ok(resolved) => {
            crate::domain::model_provider::probe::probe_one(
                &resolved,
                crate::domain::model_provider::probe::PROBE_TIMEOUT,
            )
            .await
        }
        Err(e) => {
            // resolve_for_probe 仅返回 BusinessError（模型不存在 / Key 解密失败），
            // 直接取其内部 message，避免 Display 的「业务逻辑错误:」前缀泄露到探测面板；
            // 其他变体（当前不返回）保留完整 to_string() 文案。
            let error = match e {
                AppError::BusinessError(msg) => msg,
                other => other.to_string(),
            };
            crate::domain::model_provider::ProbeResult {
                model_id: id.to_string(),
                model: String::new(),
                provider_name: String::new(),
                status: crate::domain::model_provider::ProbeStatus::Fail,
                latency_ms: 0,
                probe_kind: crate::domain::model_provider::ProbeKind::Chat,
                error: Some(error),
                probed_at: chrono::Utc::now().to_rfc3339(),
            }
        }
    }
}

/// 把模型删除预检影响转成人类可读摘要，供前端确认框展示。
fn summarize_model_impact(impact: &crate::domain::model_provider::store::ModelDeletionImpact) -> String {
    let mut parts: Vec<String> = Vec::new();
    if impact.assistants > 0 {
        parts.push(format!(
            "{} 个助手绑定该模型（将改为默认模型）",
            impact.assistants
        ));
    }
    if impact.sessions > 0 {
        parts.push(format!(
            "{} 个会话使用该模型（将改为默认模型）",
            impact.sessions
        ));
    }
    if impact.kb_instances > 0 {
        parts.push(format!(
            "{} 个内置知识库用该模型做 embedding（将改为默认 embedding 模型，需重新向量化）",
            impact.kb_instances
        ));
    }
    if parts.is_empty() {
        "无关联数据，可直接删除".to_string()
    } else {
        parts.join("；")
    }
}

/// 把供应商删除预检影响转成人类可读摘要，供前端确认框展示。
fn summarize_provider_impact(
    impact: &crate::domain::model_provider::store::ProviderDeletionImpact,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if impact.models > 0 {
        parts.push(format!("其下 {} 个模型将被删除", impact.models));
    }
    if impact.assistants > 0 {
        parts.push(format!(
            "{} 个助手绑定其下模型（将改为默认模型）",
            impact.assistants
        ));
    }
    if impact.sessions > 0 {
        parts.push(format!(
            "{} 个会话使用其下模型（将改为默认模型）",
            impact.sessions
        ));
    }
    if impact.kb_instances > 0 {
        parts.push(format!(
            "{} 个内置知识库用其下模型做 embedding（将改为默认 embedding 模型，需重新向量化）",
            impact.kb_instances
        ));
    }
    if parts.is_empty() {
        "无关联数据，可直接删除".to_string()
    } else {
        parts.join("；")
    }
}
