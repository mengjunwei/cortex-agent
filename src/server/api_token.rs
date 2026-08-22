//! 账户 API Token 管理 REST 接口（挂载到 `/api/auth/tokens`）。
//!
//! 用 [`AuthUser`] 提取器强制登录（浏览器会话 Cookie），用户**只能管理自己的令牌**。
//! 创建接口返回一次性明文；其余接口只返回脱敏信息（前缀 / 启用 / 时间）。
//!
//! Bearer 认证（外部系统凭令牌调接口）由 `server::auth` 的提取器统一处理，
//! 不在本模块——任何挂载 `OptionalAuthUser` 的接口（GraphQL / SSE）都自动支持。

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::auth::ApiTokenRow;
use crate::error::AppError;
use crate::server::AppState;
use crate::server::auth::AuthUser;
use crate::server::response::{self, code};

// ===== 请求体 =====

#[derive(Debug, Deserialize)]
struct CreateTokenRequest {
    name: String,
    #[serde(default)]
    remark: String,
    /// 生效起始（ISO 8601 / RFC3339，前端转 UTC）；None=立即生效
    valid_from: Option<String>,
    /// 过期时间（ISO 8601 / RFC3339）；None=永不过期
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateTokenRequest {
    name: String,
    #[serde(default)]
    remark: String,
    valid_from: Option<String>,
    expires_at: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

const fn default_true() -> bool {
    true
}

// ===== Handler =====

/// `GET /api/auth/tokens` — 列出当前用户的全部令牌（脱敏，无明文 / 哈希）。
async fn list_tokens(State(state): State<Arc<AppState>>, AuthUser(user): AuthUser) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };
    match svc.list_tokens(&user.user_id).await {
        Ok(rows) => {
            let tokens: Vec<Value> = rows.iter().map(token_to_json).collect();
            Json(response::ok(json!({ "tokens": tokens }))).into_response()
        }
        Err(e) => error_response(e),
    }
}

/// `POST /api/auth/tokens` — 创建令牌，**返回一次性明文**。
async fn create_token(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateTokenRequest>,
) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };
    let valid_from = match body.valid_from.as_deref().map(parse_dt) {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => {
            return bad_request("生效起始时间格式错误（需 ISO 8601，如 2026-08-02T10:00:00Z）");
        }
        None => None,
    };
    let expires_at = match body.expires_at.as_deref().map(parse_dt) {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => {
            return bad_request("过期时间格式错误（需 ISO 8601，如 2026-08-02T10:00:00Z）");
        }
        None => None,
    };
    match svc
        .create_token(
            &user.user_id,
            &body.name,
            &body.remark,
            valid_from,
            expires_at,
        )
        .await
    {
        Ok((raw, row)) => {
            let mut data = token_to_json(&row);
            data["token"] = json!(raw);
            Json(response::ok(data)).into_response()
        }
        Err(e) => error_response(e),
    }
}

/// `PATCH /api/auth/tokens/{id}` — 更新令牌可编辑字段（名称 / 备注 / 生效时间段 / 启用状态）。
async fn update_token(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdateTokenRequest>,
) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };
    let valid_from = match body.valid_from.as_deref().map(parse_dt) {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => return bad_request("生效起始时间格式错误（需 ISO 8601）"),
        None => None,
    };
    let expires_at = match body.expires_at.as_deref().map(parse_dt) {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => return bad_request("过期时间格式错误（需 ISO 8601）"),
        None => None,
    };
    match svc
        .update_token(
            &user.user_id,
            &id,
            &body.name,
            &body.remark,
            valid_from,
            expires_at,
            body.enabled,
        )
        .await
    {
        Ok(true) => Json(response::ok(json!({}))).into_response(),
        Ok(false) => not_found(),
        Err(e) => error_response(e),
    }
}

/// `DELETE /api/auth/tokens/{id}` — 删除令牌。
async fn delete_token(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };
    match svc.delete_token(&user.user_id, &id).await {
        Ok(true) => Json(response::ok(json!({}))).into_response(),
        Ok(false) => not_found(),
        Err(e) => error_response(e),
    }
}

// ===== 辅助 =====

/// 令牌行 → 脱敏 JSON（不含 token_hash / 明文）。
fn token_to_json(row: &ApiTokenRow) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "remark": row.remark,
        "prefix": row.prefix,
        "enabled": row.is_enabled(),
        "valid_from": row.valid_from.map(|d| d.to_rfc3339()),
        "expires_at": row.expires_at.map(|d| d.to_rfc3339()),
        "last_used_at": row.last_used_at.map(|d| d.to_rfc3339()),
        "created_at": row.created_at.to_rfc3339(),
        "updated_at": row.updated_at.to_rfc3339(),
    })
}

/// 解析 ISO 8601 / RFC3339 时间字符串为 UTC DateTime。
fn parse_dt(s: &str) -> Result<chrono::DateTime<chrono::Utc>, ()> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|_| ())
}

/// 业务错误 → HTTP 响应（按错误码映射状态码）。
fn error_response(e: AppError) -> Response {
    let env = response::from_app_error(&e);
    let http_code = env
        .get("code")
        .and_then(|v| v.as_i64())
        .map(|c| match c {
            c if c == code::CONFLICT as i64 => 409,
            c if c == code::BUSINESS as i64 => 400,
            c if c == code::NOT_FOUND as i64 => 404,
            _ => 500,
        })
        .unwrap_or(500) as u16;
    let status = StatusCode::from_u16(http_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(env)).into_response()
}

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(response::err(code::INVALID_PARAMS, msg)),
    )
        .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(response::err(code::NOT_FOUND, "令牌不存在")),
    )
        .into_response()
}

fn auth_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(response::err(code::INTERNAL, "认证服务未启用")),
    )
        .into_response()
}

/// Token 管理路由组（挂载到 `/api/auth/tokens`，由 `server::run` `.merge`）。
pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, patch};
    axum::Router::new()
        .route("/api/auth/tokens", get(list_tokens).post(create_token))
        .route(
            "/api/auth/tokens/{id}",
            patch(update_token).delete(delete_token),
        )
}
