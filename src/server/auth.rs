//! 认证（SSO）HTTP 路由与 AuthUser 提取器。
//!
//! 路由总览（挂载到 `/api/auth/...`）：
//! - `GET  /api/auth/providers`      — 列出全部已配置的身份提供商 + 本地登录可用性（前端登录页展示）
//! - `GET  /api/auth/login/{key}`    — 生成 CSRF state 并 302 跳转到第三方授权页（SSO）
//! - `GET  /api/auth/callback/{key}` — 校验 state、用 code 换取身份、签发会话 Cookie（SSO）
//! - `POST /api/auth/register`       — 本地账号注册（用户名密码，首用户自动管理员）
//! - `POST /api/auth/login/local`    — 本地账号登录（用户名密码）
//! - `GET  /api/auth/me`             — 获取当前登录用户（未登录返回 authenticated:false）
//! - `POST /api/auth/logout`         — 注销当前会话（加入 Redis 黑名单 + 清除 Cookie）
//!
//! Cookie 安全策略：
//! - 会话 Cookie：`HttpOnly; SameSite=Lax; Path=/; Max-Age=TTL`，有效阻止 XSS 读取
//! - OAuth state Cookie：同上，TTL 5 分钟，仅用于跨请求 CSRF 校验
//! - `SameSite=Lax` 允许 OAuth 回调跳转携带 Cookie，同时抵御跨站请求伪造

use std::sync::Arc;

use axum::{
    Json,
    extract::{FromRequestParts, Path, Query, State},
    http::{StatusCode, header, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::domain::auth::{AuthUser as AuthUserModel, Claims};
use crate::server::AppState;
use crate::server::response::{self, code};

/// OAuth state Cookie 后缀（拼接在会话 Cookie 名之后）
const OAUTH_STATE_COOKIE_SUFFIX: &str = "_oauth_state";
/// OAuth state Cookie 有效期（秒）：5 分钟，足以覆盖完整的 OAuth 跳转流程
const OAUTH_STATE_TTL_SECS: i64 = 300;

// ===========================================================================
// Cookie 头辅助（手动构造 Set-Cookie，避免引入 time 依赖）
// ===========================================================================

/// 拼接 OAuth state Cookie 名
fn state_cookie_name(session_cookie_name: &str) -> String {
    format!("{session_cookie_name}{OAUTH_STATE_COOKIE_SUFFIX}")
}

/// 构造 Set-Cookie 头值：`name=value; Path=/; HttpOnly; SameSite=Lax; Max-Age=...`
fn build_set_cookie(name: &str, value: &str, max_age_secs: i64) -> header::HeaderValue {
    let s = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}");
    header::HeaderValue::from_str(&s)
        .expect("Set-Cookie 值为受控 ASCII（JWT/UUID 字符集），不可能解析失败")
}

/// 构造清除 Cookie 的头值（Max-Age=0，空值）
fn build_clear_cookie(name: &str) -> header::HeaderValue {
    build_set_cookie(name, "", 0)
}

// ===========================================================================
// 提取器
// ===========================================================================

/// 必需认证的提取器：从 Cookie 中解析 JWT 并校验，成功返回当前登录用户。
///
/// 未登录、会话过期、token 无效或 auth 服务未启用时返回 401。
/// 需要登录保护的路由在 handler 签名中声明 `AuthUser` 即可。
///
/// 当前 SSO 路由组自身不需要强制认证（providers/login/callback/me/logout 均为公开或自带校验），
/// 该提取器供未来需要保护的业务路由（如 GraphQL 写操作）直接使用。
pub struct AuthUser(pub AuthUserModel);

impl FromRequestParts<Arc<AppState>> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        // 先试 Authorization: Bearer（程序化调用 / 外部系统凭 API Token 访问）
        if let Some(user) = try_extract_bearer(state, parts).await {
            return Ok(AuthUser(user));
        }
        // 回退 Cookie（浏览器会话 JWT）
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|inf: std::convert::Infallible| match inf {})?;

        match try_extract_user(state, &jar).await {
            Some(user) => Ok(AuthUser(user)),
            None => Err(unauthorized()),
        }
    }
}

/// 可选认证的提取器：未登录或 auth 服务不可用时返回 `None`，不报错。
///
/// 用于 `/api/auth/me` 等需要区分「已登录 / 未登录」但不能强制要求认证的接口。
pub struct OptionalAuthUser(pub Option<AuthUserModel>);

impl FromRequestParts<Arc<AppState>> for OptionalAuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        // 先试 Authorization: Bearer（程序化调用 / 外部系统凭 API Token 访问）
        if let Some(user) = try_extract_bearer(state, parts).await {
            return Ok(OptionalAuthUser(Some(user)));
        }
        // 回退 Cookie（浏览器会话 JWT）
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|inf: std::convert::Infallible| match inf {})?;

        Ok(OptionalAuthUser(try_extract_user(state, &jar).await))
    }
}

/// 从请求 Cookie 中解析并校验 JWT，返回当前用户。
///
/// 以下情况返回 `None`（不报错，由调用方决定如何处理）：
/// - auth 服务未启用（`state.auth` 为 None）
/// - 未携带会话 Cookie
/// - JWT 校验失败（签名错误 / 已过期 / 在黑名单中）
async fn try_extract_user(state: &Arc<AppState>, jar: &CookieJar) -> Option<AuthUserModel> {
    let svc = state.auth.as_ref()?;
    let name = svc.cookie_name();
    let token = jar.get(name)?.value().to_string();
    let claims: Claims = svc.verify_token(&token).await.ok()?;
    Some(AuthUserModel::from_claims(&claims))
}

/// 从 `Authorization: Bearer <token>` 头解析 API Token 并校验，返回其所属用户。
///
/// 适用场景：外部系统/脚本凭账户下创建的 API Token 调用接口（等价登录身份）。
/// 以下情况返回 `None`（不报错，由调用方回退 Cookie 或返回 401）：
/// - auth 服务未启用、无 Authorization 头、非 Bearer 方案、token 为空
/// - token 不存在 / 已禁用 / 未生效 / 已过期 / 用户被禁用（统一视作无效，不泄露原因）
async fn try_extract_bearer(state: &Arc<AppState>, parts: &Parts) -> Option<AuthUserModel> {
    let svc = state.auth.as_ref()?;
    let value = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() {
        return None;
    }
    svc.verify_api_token(token).await.ok()
}

/// 构造 401 未认证响应
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(response::err(code::BUSINESS, "未登录或会话已过期")),
    )
        .into_response()
}

/// 构造 503 认证服务不可用响应
fn auth_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(response::err(
            code::INTERNAL,
            "认证服务未启用（未配置任何 auth.providers）",
        )),
    )
        .into_response()
}

// ===========================================================================
// Handlers
// ===========================================================================

/// `GET /api/auth/providers` — 列出全部已配置的身份提供商 + 本地登录可用性（前端登录页展示用）。
///
/// 无需认证。auth 服务未启用时返回空数组与 local_enabled=false。
async fn list_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (providers, local_enabled) = state.auth.as_ref().map_or((Vec::new(), false), |svc| {
        let list = svc
            .list_providers()
            .into_iter()
            .map(|p| {
                json!({
                    "key": p.key,
                    "kind": p.kind,
                    "name": p.name,
                })
            })
            .collect();
        // 本地用户名密码登录始终可用（AuthService 一旦初始化即支持）
        (list, true)
    });
    Json(response::ok(
        json!({ "providers": providers, "local_enabled": local_enabled }),
    ))
}

/// `GET /api/auth/login/{key}` — 生成 CSRF state，写入短期 Cookie，302 跳转到第三方授权页。
///
/// `key` 格式为 `{kind}-{name}`，如 `feishu-corp`、`oidc-google`。
async fn login(Path(key): Path<String>, State(state): State<Arc<AppState>>) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };

    // 生成不可预测的 CSRF state（UUID v7 含时间戳 + 随机）
    let oauth_state = Uuid::now_v7().to_string();

    // 构造授权 URL（包含 state 参数，由 provider 实现拼接）
    let authorize_url = match svc.build_authorize_url(&key, &oauth_state).await {
        Ok(u) => u,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(response::from_app_error(&e))).into_response();
        }
    };

    // 302 跳转 + 写入 state Cookie（短期，回调时校验）
    let mut resp = Redirect::to(&authorize_url).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        build_set_cookie(
            &state_cookie_name(svc.cookie_name()),
            &oauth_state,
            OAUTH_STATE_TTL_SECS,
        ),
    );
    resp
}

/// OAuth 回调查询参数（code 与 state 均可选，缺失时由 handler 校验报错）
#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
}

/// `GET /api/auth/callback/{key}` — 校验 CSRF state、用 code 换取身份、签发会话 Cookie。
///
/// 流程：
/// 1. 从 Cookie 读取 OAuth state，与 query 参数 `state` 比对（CSRF 防护）
/// 2. 调用 `AuthService::complete_login` 完成 code → 身份 → upsert → JWT
/// 3. 成功：设置会话 Cookie + 清除 state Cookie + 302 跳转首页
/// 4. 失败：清除 state Cookie + 返回 JSON 错误（便于前端调试）
async fn callback(
    Path(key): Path<String>,
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Query(params): Query<CallbackParams>,
) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };

    let state_cn = state_cookie_name(svc.cookie_name());

    // 1. 校验 CSRF state：Cookie 中的 state 必须与 query 参数一致且非空
    let expected_state = jar.get(&state_cn).map(|c| c.value().to_string());
    let params_state = params.state.as_deref().unwrap_or("");
    if params_state.is_empty() || expected_state.as_deref() != Some(params_state) {
        let mut resp = (
            StatusCode::BAD_REQUEST,
            Json(response::err(
                code::BUSINESS,
                "OAuth state 校验失败（可能是会话过期或 CSRF 攻击）",
            )),
        )
            .into_response();
        resp.headers_mut()
            .append(header::SET_COOKIE, build_clear_cookie(&state_cn));
        return resp;
    }

    // 2. 校验 code 参数存在且非空
    let code = match params.code.as_deref().filter(|s| !s.is_empty()) {
        Some(c) => c.to_string(),
        None => {
            let mut resp = (
                StatusCode::BAD_REQUEST,
                Json(response::err(code::INVALID_PARAMS, "缺少 OAuth code 参数")),
            )
                .into_response();
            resp.headers_mut()
                .append(header::SET_COOKIE, build_clear_cookie(&state_cn));
            return resp;
        }
    };

    // 3. code → 外部身份 → upsert → JWT
    match svc.complete_login(&key, &code).await {
        Ok((token, _claims)) => {
            let mut resp = Redirect::to("/").into_response();
            // 设置会话 Cookie（Max-Age 与 JWT TTL 一致）
            resp.headers_mut().append(
                header::SET_COOKIE,
                build_set_cookie(svc.cookie_name(), &token, svc.token_ttl_secs()),
            );
            // 清除 OAuth state Cookie（一次性）
            resp.headers_mut()
                .append(header::SET_COOKIE, build_clear_cookie(&state_cn));
            resp
        }
        Err(e) => {
            let mut resp =
                (StatusCode::BAD_REQUEST, Json(response::from_app_error(&e))).into_response();
            resp.headers_mut()
                .append(header::SET_COOKIE, build_clear_cookie(&state_cn));
            resp
        }
    }
}

/// `GET /api/auth/me` — 获取当前登录用户（未登录返回 `authenticated:false`）。
///
/// 使用 `OptionalAuthUser`，不强制认证。
async fn me(
    State(state): State<Arc<AppState>>,
    OptionalAuthUser(user): OptionalAuthUser,
) -> impl IntoResponse {
    let data: Value = match user {
        Some(u) => {
            // 是否设有本地密码（决定前端「修改密码」入口是否展示；纯 SSO 账号为 false）
            let has_password = match state.auth.as_ref() {
                Some(svc) => svc.user_has_password(&u.user_id).await.unwrap_or(false),
                None => false,
            };
            json!({
                "authenticated": true,
                "user": {
                    "user_id": u.user_id,
                    "name": u.name,
                    "avatar": u.avatar,
                    "is_admin": u.is_admin,
                    "has_password": has_password,
                }
            })
        }
        None => json!({ "authenticated": false }),
    };
    Json(response::ok(data))
}

/// `POST /api/auth/logout` — 注销当前会话（加入 Redis 黑名单 + 清除 Cookie）。
///
/// 幂等：即使未登录也返回成功。Redis 不可用时仅清除 Cookie（fail-open）。
async fn logout(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
) -> Response {
    // 计算要清除的 Cookie 名（auth 未启用时回退到默认名）
    let cookie_name = state
        .auth
        .as_ref()
        .map(|s| s.cookie_name().to_string())
        .unwrap_or_else(|| "cortex_session".to_string());

    // 尝试将当前 token 的 jti 加入 Redis 黑名单（失败忽略，仍清除 Cookie）
    if let Some(svc) = state.auth.as_ref() {
        if let Some(cookie) = jar.get(&cookie_name) {
            let token = cookie.value();
            if let Ok(claims) = svc.verify_token(token).await {
                crate::domain::audit::spawn_record(
                    state.audit_store.as_ref(),
                    crate::domain::audit::AuditEntry {
                        user_id: claims.sub.clone(),
                        actor: claims.name.clone(),
                        source: "web".to_string(),
                        operation: "logout".to_string(),
                        target_id: String::new(),
                        success: true,
                        detail: "{}".to_string(),
                        ip: super::audit::client_ip(&headers),
                    },
                );
                if let Err(e) = svc.revoke_token(&claims.jti).await {
                    tracing::warn!("[Auth] 注销时写入黑名单失败（忽略，仍清除 Cookie）: {e}");
                }
            }
        }
    }

    // 返回成功 + 清除会话 Cookie
    let mut resp = Json(response::ok(json!({}))).into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, build_clear_cookie(&cookie_name));
    resp
}

/// `POST /api/auth/register` — 本地账号注册（用户名密码）。
///
/// 请求体 `{ username, password, name? }`。首个注册用户自动成为管理员。
/// 成功后与会话 Cookie（与 SSO 登录共用同一 Cookie）。
async fn register(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };

    let username = body.username.trim();
    let name = body.name.as_deref().map(str::trim).unwrap_or("");

    match svc.register_local(username, &body.password, name).await {
        Ok((token, claims)) => {
            crate::domain::audit::spawn_record(
                state.audit_store.as_ref(),
                crate::domain::audit::AuditEntry {
                    user_id: claims.sub.clone(),
                    actor: username.to_string(),
                    source: "web".to_string(),
                    operation: "register".to_string(),
                    target_id: String::new(),
                    success: true,
                    detail: "{}".to_string(),
                    ip: super::audit::client_ip(&headers),
                },
            );
            let cookie_name = svc.cookie_name().to_string();
            let ttl = svc.token_ttl_secs();
            let mut resp = Json(response::ok(json!({
                "user": json_user_from_claims(&claims),
            })))
            .into_response();
            resp.headers_mut().append(
                header::SET_COOKIE,
                build_set_cookie(&cookie_name, &token, ttl),
            );
            resp
        }
        Err(e) => {
            crate::domain::audit::spawn_record(
                state.audit_store.as_ref(),
                crate::domain::audit::AuditEntry {
                    user_id: String::new(),
                    actor: username.to_string(),
                    source: "web".to_string(),
                    operation: "register".to_string(),
                    target_id: String::new(),
                    success: false,
                    detail: "{}".to_string(),
                    ip: super::audit::client_ip(&headers),
                },
            );
            let env = response::from_app_error(&e);
            let http_code = env
                .get("code")
                .and_then(|v| v.as_i64())
                .map(|c| match c {
                    c if c == code::CONFLICT as i64 => 409, // 用户名占用
                    c if c == code::BUSINESS as i64 => 400, // 校验失败
                    _ => 500,
                })
                .unwrap_or(500) as u16;
            let status =
                StatusCode::from_u16(http_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(env)).into_response()
        }
    }
}

/// `POST /api/auth/login/local` — 本地账号登录（用户名密码）。
///
/// 请求体 `{ username, password }`。成功后设置会话 Cookie。
async fn login_local(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };

    let username = body.username.trim();

    match svc.login_local(username, &body.password).await {
        Ok((token, claims)) => {
            crate::domain::audit::spawn_record(
                state.audit_store.as_ref(),
                crate::domain::audit::AuditEntry {
                    user_id: claims.sub.clone(),
                    actor: username.to_string(),
                    source: "web".to_string(),
                    operation: "login".to_string(),
                    target_id: String::new(),
                    success: true,
                    detail: "{}".to_string(),
                    ip: super::audit::client_ip(&headers),
                },
            );
            let cookie_name = svc.cookie_name().to_string();
            let ttl = svc.token_ttl_secs();
            let mut resp = Json(response::ok(json!({
                "user": json_user_from_claims(&claims),
            })))
            .into_response();
            resp.headers_mut().append(
                header::SET_COOKIE,
                build_set_cookie(&cookie_name, &token, ttl),
            );
            resp
        }
        Err(e) => {
            crate::domain::audit::spawn_record(
                state.audit_store.as_ref(),
                crate::domain::audit::AuditEntry {
                    user_id: String::new(),
                    actor: username.to_string(),
                    source: "web".to_string(),
                    operation: "login".to_string(),
                    target_id: String::new(),
                    success: false,
                    detail: "{}".to_string(),
                    ip: super::audit::client_ip(&headers),
                },
            );
            // 登录失败统一 401（用户名枚举防护）
            let body = response::from_app_error(&e);
            (StatusCode::UNAUTHORIZED, Json(body)).into_response()
        }
    }
}

/// `POST /api/auth/change-password` — 登录态修改密码（需校验原密码）。
///
/// 请求体 `{ old_password, new_password }`。成功后该账号全部会话立即失效
/// （后端 `verify_token` 因 `iat < updated_at` 拒绝旧 token），前端应引导用户重新登录。
/// 本接口不签发新 token。
async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    AuthUser(user): AuthUser,
    Json(body): Json<ChangePasswordRequest>,
) -> Response {
    let svc = match state.auth.as_ref() {
        Some(s) => s,
        None => return auth_unavailable(),
    };

    match svc
        .change_password(&user.user_id, &body.old_password, &body.new_password)
        .await
    {
        Ok(()) => {
            crate::domain::audit::spawn_record(
                state.audit_store.as_ref(),
                crate::domain::audit::AuditEntry {
                    user_id: user.user_id.clone(),
                    actor: user.name.clone(),
                    source: "web".to_string(),
                    operation: "change_password".to_string(),
                    target_id: String::new(),
                    success: true,
                    detail: "{}".to_string(),
                    ip: super::audit::client_ip(&headers),
                },
            );
            Json(response::ok(json!({}))).into_response()
        }
        Err(e) => {
            let env = response::from_app_error(&e);
            let http_code = env
                .get("code")
                .and_then(|v| v.as_i64())
                .map(|c| match c {
                    c if c == code::BUSINESS as i64 => 400, // 原密码错误 / 新密码格式不符
                    _ => 500,
                })
                .unwrap_or(500) as u16;
            let status =
                StatusCode::from_u16(http_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(env)).into_response()
        }
    }
}

/// 将 Claims 序列化为前端用户对象
fn json_user_from_claims(claims: &Claims) -> Value {
    json!({
        "user_id": claims.sub,
        "name": claims.name,
        "avatar": claims.avatar,
        "is_admin": claims.is_admin,
    })
}

/// 注册请求体
#[derive(Debug, serde::Deserialize)]
struct RegisterRequest {
    username: String,
    password: String,
    name: Option<String>,
}

/// 本地登录请求体
#[derive(Debug, serde::Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

/// 修改密码请求体
#[derive(Debug, serde::Deserialize)]
struct ChangePasswordRequest {
    old_password: String,
    new_password: String,
}

// ===========================================================================
// 路由装配
// ===========================================================================

/// 认证路由组（挂载到根路径，所有路由以 `/api/auth/` 开头）。
///
/// 返回 `Router<Arc<AppState>>`，由 `server::mod` 通过 `.merge()` 挂载。
pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/auth/providers", get(list_providers))
        .route("/api/auth/login/{key}", get(login))
        .route("/api/auth/callback/{key}", get(callback))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login/local", post(login_local))
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/change-password", post(change_password))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_cookie_name_拼接正确() {
        assert_eq!(
            state_cookie_name("cortex_session"),
            "cortex_session_oauth_state"
        );
        assert_eq!(state_cookie_name("sid"), "sid_oauth_state");
    }

    #[test]
    fn build_set_cookie_包含全部安全属性() {
        let hv = build_set_cookie("cortex_session", "abc.def.ghi", 3600);
        let s = hv.to_str().unwrap();
        assert!(s.contains("cortex_session=abc.def.ghi"));
        assert!(s.contains("Path=/"));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
        assert!(s.contains("Max-Age=3600"));
    }

    #[test]
    fn build_clear_cookie_sets_max_age_zero() {
        let hv = build_clear_cookie("cortex_session");
        let s = hv.to_str().unwrap();
        assert!(s.contains("cortex_session="));
        assert!(s.contains("Max-Age=0"));
        assert!(s.contains("HttpOnly"));
    }

    #[test]
    fn callback_params_可选字段反序列化() {
        let params: CallbackParams =
            serde_json::from_str(r#"{"code":"xyz","state":"abc"}"#).unwrap();
        assert_eq!(params.code.as_deref(), Some("xyz"));
        assert_eq!(params.state.as_deref(), Some("abc"));

        let empty: CallbackParams = serde_json::from_str("{}").unwrap();
        assert!(empty.code.is_none());
        assert!(empty.state.is_none());
    }
}
