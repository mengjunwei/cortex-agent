//! 知识库实例管理接口（多 provider）— CRUD + schema 声明 + 连通性测试。
//!
//! 与 [`knowledge`](super::knowledge)（文档操作）拆分以控制文件体积。实例配置的 secret 字段
//! （如 Dify api_key）入库前经 AesCodec 加密；列表返回时掩码（不泄露明文）。

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::knowledge::backend::{ProviderKind, schema};
use crate::domain::knowledge::kb_instance_store::{self, KbInstance};
use crate::error::AppError;

use super::AppState;
use super::response;
use super::response::code;

// ===================== 归属 / 可见性校验（供 knowledge.rs 共用）=====================

/// 取实例并校验**写**权限：归属人 / 管理员放行；否则业务错误。实例不存在 → NOT_FOUND。
pub(super) async fn require_writable(
    state: &AppState,
    id: &str,
    user_id: &str,
    is_admin: bool,
) -> Result<KbInstance, Value> {
    let store = state.knowledge_manager.kb_instance_store();
    match store.get(id).await {
        Ok(Some(inst)) => {
            if is_admin || inst.creator == user_id {
                Ok(inst)
            } else {
                Err(response::err(code::BUSINESS, "无权操作他人创建的知识库"))
            }
        }
        Ok(None) => Err(response::err(code::NOT_FOUND, "知识库实例不存在")),
        Err(e) => Err(response::err(code::DATABASE, e.to_string())),
    }
}

/// 取实例并校验**读**权限：归属人 / 管理员 / 公开（visibility>0）放行；
/// 私有且非归属人 → NOT_FOUND（不暴露存在性）。
pub(super) async fn require_readable(
    state: &AppState,
    id: &str,
    user_id: &str,
    is_admin: bool,
) -> Result<KbInstance, Value> {
    let store = state.knowledge_manager.kb_instance_store();
    match store.get(id).await {
        Ok(Some(inst)) => {
            if is_admin
                || inst.creator == user_id
                || inst.visibility > kb_instance_store::visibility::PRIVATE
            {
                Ok(inst)
            } else {
                Err(response::err(code::NOT_FOUND, "知识库实例不存在"))
            }
        }
        Ok(None) => Err(response::err(code::NOT_FOUND, "知识库实例不存在")),
        Err(e) => Err(response::err(code::DATABASE, e.to_string())),
    }
}

// ===================== 实例 CRUD =====================

/// 列出知识库实例（config 中 secret 字段掩码返回）。
/// 普通用户=自己创建的 + 公开（visibility>0）；管理员=全部。
pub async fn kb_instance_list(state: &AppState, user_id: &str, is_admin: bool) -> Value {
    let store = state.knowledge_manager.kb_instance_store();
    let codec = state.knowledge_manager.codec();
    match store.list_for_owner(user_id, is_admin).await {
        Ok(instances) => {
            let mut data: Vec<Value> = instances
                .iter()
                .map(|inst| {
                    let kind = ProviderKind::from_i16(inst.provider_kind);
                    let cfg = inst.config_value();
                    let config_masked = match kind {
                        Some(k) => schema::decrypt_secret_fields(k, &cfg, codec, true),
                        None => cfg,
                    };
                    json!({
                        "id": inst.id,
                        "name": inst.name,
                        "provider_kind": inst.provider_kind,
                        "config": config_masked,
                        "status": inst.status,
                        "creator": inst.creator,
                        "visibility": inst.visibility,
                        "created_at": inst.created_at,
                        "updated_at": inst.updated_at,
                    })
                })
                .collect();
            super::owner::inject_owners(state.db_pool.as_ref(), is_admin, &mut data, "creator")
                .await;
            response::ok(json!({ "instances": data }))
        }
        Err(e) => response::err(code::DATABASE, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct KbInstanceCreateRequest {
    pub name: String,
    /// 1=Dify 2=Builtin
    pub provider_kind: i16,
    pub config: Value,
    /// 0=私有（默认）1=公开；None 视为私有
    #[serde(default)]
    pub visibility: Option<i16>,
}

pub async fn kb_instance_create(
    state: &AppState,
    user_id: &str,
    input: KbInstanceCreateRequest,
) -> Value {
    let kind = match ProviderKind::from_i16(input.provider_kind) {
        Some(k) => k,
        None => {
            return response::err(
                code::BUSINESS,
                format!("未知 provider_kind: {}", input.provider_kind),
            );
        }
    };
    if let Err(e) = schema::validate_config(kind, &input.config) {
        return response::err(code::BUSINESS, e.to_string());
    }
    let codec = state.knowledge_manager.codec();
    let enc_config = match schema::encrypt_secret_fields(kind, &input.config, codec) {
        Ok(s) => s,
        Err(e) => return response::err(code::BUSINESS, e.to_string()),
    };
    let visibility = match input.visibility {
        Some(v) if v > kb_instance_store::visibility::PRIVATE => {
            kb_instance_store::visibility::PUBLIC
        }
        _ => kb_instance_store::visibility::PRIVATE,
    };
    let store = state.knowledge_manager.kb_instance_store();
    match store
        .create(
            &input.name,
            input.provider_kind,
            &enc_config,
            user_id,
            visibility,
        )
        .await
    {
        Ok(id) => response::ok(json!({ "id": id, "message": "创建成功" })),
        Err(e) => response::err(code::DATABASE, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct KbInstanceUpdateRequest {
    pub id: String,
    pub name: String,
    pub provider_kind: i16,
    pub config: Value,
    /// 1=启用 0=禁用；None=不变
    pub status: Option<i16>,
    /// 0=私有 1=公开；None=不变
    #[serde(default)]
    pub visibility: Option<i16>,
}

pub async fn kb_instance_update(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    input: KbInstanceUpdateRequest,
) -> Value {
    // 归属校验：仅归属人 / 管理员可改
    let existing = match require_writable(state, &input.id, user_id, is_admin).await {
        Ok(inst) => inst,
        Err(v) => return v,
    };
    let kind = match ProviderKind::from_i16(input.provider_kind) {
        Some(k) => k,
        None => {
            return response::err(
                code::BUSINESS,
                format!("未知 provider_kind: {}", input.provider_kind),
            );
        }
    };
    // 先合并 secret（空/掩码保留 DB 原密文，新值加密），再校验（此时 secret 已非空）
    let merged = match merge_secrets(state, &input.id, kind, &input.config).await {
        Ok(v) => v,
        Err(e) => return response::err(code::BUSINESS, e.to_string()),
    };
    if let Err(e) = schema::validate_config(kind, &merged) {
        return response::err(code::BUSINESS, e.to_string());
    }
    let enc_config = match serde_json::to_string(&merged) {
        Ok(s) => s,
        Err(e) => return response::err(code::BUSINESS, format!("序列化失败: {e}")),
    };
    let status = input.status.unwrap_or(existing.status);
    let visibility = match input.visibility {
        Some(v) if v > kb_instance_store::visibility::PRIVATE => {
            kb_instance_store::visibility::PUBLIC
        }
        Some(_) => kb_instance_store::visibility::PRIVATE,
        // None=不变，沿用 DB 现值
        None => existing.visibility,
    };
    let store = state.knowledge_manager.kb_instance_store();
    match store
        .update(
            &input.id,
            &input.name,
            input.provider_kind,
            &enc_config,
            status,
            visibility,
        )
        .await
    {
        Ok(true) => {
            state.knowledge_manager.invalidate_provider(&input.id);
            response::ok(json!({ "updated": true }))
        }
        Ok(false) => response::err(code::BUSINESS, "实例不存在"),
        Err(e) => response::err(code::DATABASE, e.to_string()),
    }
}

pub async fn kb_instance_delete(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: String,
    force: bool,
) -> Value {
    let store = state.knowledge_manager.kb_instance_store();
    if !force {
        // 预检也需归属：仅归属人/管理员可见影响清单
        if let Err(v) = require_writable(state, &id, user_id, is_admin).await {
            return v;
        }
        match store.impact_of_delete(&id).await {
            Ok(impact) => response::ok(json!({
                "deleted": false,
                "impact": { "assistants": impact.assistants },
                "summary": if impact.assistants > 0 {
                    format!("{} 个助手绑定该知识库（将解除绑定）", impact.assistants)
                } else {
                    "无关联数据，可直接删除".to_string()
                },
            })),
            Err(e) => response::err(code::DATABASE, e.to_string()),
        }
    } else {
        // 归属校验：仅归属人/管理员可删
        let inst = match require_writable(state, &id, user_id, is_admin).await {
            Ok(i) => i,
            Err(v) => return v,
        };
        let is_builtin = ProviderKind::from_i16(inst.provider_kind)
            .is_some_and(|k| matches!(k, ProviderKind::Builtin));
        match store.delete_with_cleanup(&id).await {
            Ok(res) if res.deleted => {
                state.knowledge_manager.invalidate_provider(&id);
                // 内置实例：清理 Qdrant 向量集合（Dify 类型无本地向量，跳过）
                if is_builtin {
                    state.knowledge_manager.purge_qdrant_collection(&id).await;
                }
                response::ok(json!({
                    "deleted": true,
                    "cleanup": { "assistants_unbound": res.assistants_unbound },
                }))
            }
            Ok(_) => response::err(code::BUSINESS, "实例不存在"),
            Err(e) => response::err(code::DATABASE, e.to_string()),
        }
    }
}

/// 连通性测试（health）：归属人/管理员可测
pub async fn kb_instance_test(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: String,
) -> Value {
    if let Err(v) = require_writable(state, &id, user_id, is_admin).await {
        return v;
    }
    match state.knowledge_manager.health_instance(&id).await {
        Ok(()) => response::ok(json!({ "ok": true, "message": "连通正常" })),
        Err(e) => response::ok(json!({ "ok": false, "message": e.to_string() })),
    }
}

// ===================== schema 声明（驱动前端动态表单） =====================

pub async fn kb_provider_schema(_state: &AppState) -> Value {
    let providers = json!([
        { "kind": 1, "name": "Dify（外挂）", "fields": schema_fields(ProviderKind::Dify) },
        { "kind": 2, "name": "内置（Qdrant 向量库）", "fields": schema_fields(ProviderKind::Builtin) },
    ]);
    response::ok(json!({ "providers": providers }))
}

fn schema_fields(kind: ProviderKind) -> Vec<Value> {
    schema::schema_for(kind)
        .iter()
        .map(|s| {
            let ft = match s.field_type {
                schema::FieldType::Text => "text",
                schema::FieldType::Secret => "secret",
                schema::FieldType::Number => "number",
                schema::FieldType::Url => "url",
                schema::FieldType::Select => "select",
            };
            json!({
                "key": s.key,
                "label": s.label,
                "field_type": ft,
                "required": s.required,
                "default": s.default,
                "placeholder": s.placeholder,
                "help": s.help,
            })
        })
        .collect()
}

// ===================== 内部辅助 =====================

/// 合并 secret：前端传空/掩码的 secret 字段保留 DB 原密文；新明文则加密。
async fn merge_secrets(
    state: &AppState,
    id: &str,
    kind: ProviderKind,
    new_config: &Value,
) -> Result<Value, AppError> {
    let store = state.knowledge_manager.kb_instance_store();
    let codec = state.knowledge_manager.codec();
    let inst = store
        .get(id)
        .await?
        .ok_or_else(|| AppError::BusinessError("实例不存在".into()))?;
    let db_config: Value = serde_json::from_str(&inst.config).unwrap_or_default();
    let mut merged = new_config.clone();
    for spec in schema::schema_for(kind) {
        if spec.is_secret() {
            let new_val = merged.get(spec.key).and_then(|v| v.as_str()).unwrap_or("");
            if new_val.is_empty() || new_val.contains('*') {
                // 空/掩码 → 用 DB 原密文
                if let Some(db_val) = db_config.get(spec.key).cloned() {
                    merged[spec.key] = db_val;
                }
            } else {
                let enc = codec
                    .encrypt(new_val)
                    .map_err(|e| AppError::BusinessError(format!("加密失败: {e}")))?;
                merged[spec.key] = Value::String(enc);
            }
        }
    }
    serde_json::to_string(&merged).map_err(|e| AppError::SerializationError(e.to_string()))?;
    Ok(merged)
}
