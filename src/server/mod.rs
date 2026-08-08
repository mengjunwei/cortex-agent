//! HTTP 服务器模块 — Axum 路由、中间件与服务启动
//!
//! ## API 路由总览
//!
//! 除流式，健康检查，静态资源与认证外，**所有业务接口统一走 GraphQL 单入口**
//! （见 [`graphql`]）。GraphQL resolver 内部复用本模块各 `pub async fn` 业务函数。
//!
//! ### REST（保留 — 流式 / 健康检查 / 静态资源 / 上传 / 截图 / Shell 审批）
//!
//! | 路由 | 方法 | 说明 |
//! |------|------|------|
//! | `/` | GET | Web UI 首页（SPA fallback 同样回退到 index.html） |
//! | `/api/health` | GET | 健康检查 |
//! | `/api/v1/monitor/health` | GET | 高性能监控 API 健康检查 |
//! | `/api/run_sse` | POST | SSE 流式对话（按会话绑定的助手构建 Agent） |
//! | `/api/shell-approve` | POST | 响应 `SHELL_APPROVAL_REQUEST`（Shell 命令审批决策） |
//! | `/api/uploads` | POST | 上传图片附件（≤10MB，存对象存储返回 presigned URL） |
//! | `/api/skills/install` | POST | 从工作区绝对路径安装 Skill（JSON body） |
//! | `/api/skills/upload` | POST | 上传 tar.gz 安装 Skill（multipart） |
//! | `/assets/{path}` | GET | 前端静态资源 |
//! | `/api/screenshots/{session_id}/{filename}` | GET | 截图（按会话隔离存对象存储，后端代理读 + 鉴权） |
//!
//! ### 认证路由组（[`auth::routes`]，挂载到 `/api/auth/*`）
//!
//! | 路由 | 方法 | 说明 |
//! |------|------|------|
//! | `/api/auth/providers` | GET | 已配置身份提供商 + 本地登录可用性 |
//! | `/api/auth/login/{key}` | GET | SSO 授权跳转（写 CSRF state Cookie） |
//! | `/api/auth/callback/{key}` | GET | SSO 回调（换身份、签发会话 Cookie） |
//! | `/api/auth/register` | POST | 本地账号注册（首用户自动管理员） |
//! | `/api/auth/login/local` | POST | 本地账号登录（用户名密码） |
//! | `/api/auth/me` | GET | 当前登录用户（未登录返回 authenticated:false） |
//! | `/api/auth/logout` | POST | 注销（Redis 黑名单 + 清除 Cookie） |
//!
//! ### GraphQL（其余所有业务接口）
//!
//! - 入口：`POST /api/graphql`
//! - 覆盖：助手（CRUD/复制/分享/fork/导入导出）、会话、知识库、设备检索、监控
//!   （含 OID 准备与采集值解析）、模型供应商、MCP Server、Skill、跨会话记忆、
//!   任务取消、目录/模型/工具查询等
//! - 设计：所有字段使用 `JSON` 标量透传，响应信封 `{ code, message, data }`
//!   （`code == 0` 表示成功，见 [`response`]），与原 REST 一致，前端解构方式不变。
//!
//! ## 组合根分离
//!
//! 历史版本的依赖装配代码（session/artifact/memory/model_store/...）内联在本模块的
//! `run` 函数中，现已抽到 [`crate::bootstrap`]（架构 §3 Q6、§11）。
//! 本模块的 `run` 只做：路由注册 → Schema 注入 → TCP 监听。
//!
//! ## 降级策略
//!
//! - Session 服务：PostgreSQL 不可用时降级为 InMemory
//! - Artifact 服务：文件系统不可用时降级为 InMemory
//! - Memory 服务：Redis 不可用时降级为 InMemory

pub(crate) mod api_token;
pub(crate) mod assistant;
pub(crate) mod auth;
pub(crate) mod device;
pub(crate) mod graphql;
pub(crate) mod knowledge;
pub(crate) mod knowledge_instances;
pub(crate) mod mcp;
pub(crate) mod memory;
pub(crate) mod model_provider;
pub(crate) mod monitor;
pub(crate) mod response;
pub(crate) mod session;
pub(crate) mod shell_approval;
pub(crate) mod skill_install;
pub(crate) mod sse;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use rust_embed::RustEmbed;
use serde_json::json;
use std::sync::Arc;

/// 应用状态别名 — 真正的定义在组合根 [`crate::bootstrap::AppDeps`]。
///
/// 保留 `AppState` 名称仅为减少历史 handler 签名的改动（`State<Arc<AppState>>`、
/// GraphQL `ctx.data_unchecked::<Arc<AppState>>()`）。
pub use crate::bootstrap::AppDeps as AppState;

// ========================================================================
//  服务启动
// ========================================================================

/// 启动 HTTP 服务器
///
/// 职责（仅路由 + 启动；依赖装配见 [`crate::bootstrap::build_app_deps`]）：
/// 1. 构建 GraphQL Schema，注入 AppState
/// 2. 注册全部路由（GraphQL 单入口 + 保留 REST）
/// 3. 绑定 `0.0.0.0:{port}` 启动 Axum 服务
///
/// 注：孤儿截图回收交给对象存储（RustFS）生命周期规则，不再启动后台扫描任务。
pub async fn run(deps: AppState) -> anyhow::Result<()> {
    let port = deps.config.server.port.clone();

    let state = Arc::new(deps);

    // 注:孤儿截图回收交给对象存储(RustFS)生命周期规则,不再启动后台扫描任务。

    // 构建 GraphQL Schema，注入 AppState
    let gql_schema = graphql::build_schema(state.clone());

    let app = Router::new()
        // GraphQL 单一入口
        .route("/api/graphql", post(graphql_handler))
        // 认证（SSO）路由组：/api/auth/providers, /login, /callback, /me, /logout
        .merge(auth::routes())
        // 账户 API Token 管理：/api/auth/tokens（Bearer 调用走通用提取器，此处仅管理面）
        .merge(api_token::routes())
        // Skill 安装：/api/skills/install（JSON 路径安装）、/api/skills/upload（tar.gz 上传）
        .merge(skill_install::routes())
        // 助手功能已迁移到 GraphQL，不再注册 REST 路由
        // 保留的 REST 接口（流式 / 健康检查 / 静态资源）
        .route("/", get(serve_index))
        .route("/api/health", get(health_check))
        .route("/api/v1/monitor/health", get(monitor_fast_health))
        .route("/api/run_sse", post(sse::handle_run_sse))
        .route("/api/shell-approve", post(handle_shell_approve))
        .route("/assets/{*path}", get(serve_assets))
        .route("/api/screenshots/{*path}", get(serve_screenshot))
        .route(
            "/api/sessions/{session_id}/files/{*path}",
            get(serve_session_file),
        )
        .route(
            "/api/uploads",
            post(handle_upload_image).layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .fallback(spa_fallback)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP 服务器启动: http://127.0.0.1:{}", port);
    tracing::info!("Web UI: http://127.0.0.1:{}", port);
    tracing::info!("局域网访问: http://<本机IP>:{}", port);
    tracing::info!("GraphQL 入口: POST http://127.0.0.1:{}/api/graphql", port);
    tracing::info!("保留 REST: /api/run_sse (SSE)、/api/health、/api/v1/monitor/health");

    // GraphQL Schema 需在 axum::serve 之前 move 进 handler 闭包；
    // 通过 Arc 包装避免每次请求克隆整个 Schema（Schema 内部已 Arc 化，此处仅包一层）。
    let shared_schema = std::sync::Arc::new(gql_schema);
    // axum::serve 需要 app 拥有所有 state；graph handler 通过 Extension 注入 schema
    let app = app.layer(axum::Extension(shared_schema));

    if let Err(e) = axum::serve(listener, app).await {
        // axum::serve 返回 Err（底层 accept/IO 失败）时，原先经 ? 静默传播会让进程
        // 以 exit code 1 退出且不留任何日志（表现为“运行一段时间后莫名退出”）。
        // 这里显式记录错误内容，便于定位真正的触发原因（系统休眠唤醒、listener 异常等）。
        tracing::error!("HTTP 服务器 accept 循环异常退出: {e}");
        return Err(anyhow::anyhow!("axum serve 失败: {e}"));
    }
    Ok(())
}

/// GraphQL HTTP handler — 解析 JSON 请求体，执行 schema，返回 GraphQL JSON 响应
async fn graphql_handler(
    State(state): State<Arc<AppState>>,
    Extension(schema): Extension<std::sync::Arc<graphql::GqlSchema>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    Json(request): Json<async_graphql::Request>,
) -> axum::Json<async_graphql::Response> {
    // 当前登录用户（记忆等按用户隔离的接口从 GraphQL Context 取用）；
    // auth 未启用 / 未登录时回退 "user"，与系统其他会话路径一致。
    let user_id = opt_user
        .as_ref()
        .map(|u| u.user_id.clone())
        .unwrap_or_else(|| "user".to_string());
    let actor = opt_user
        .as_ref()
        .map(|u| u.name.clone())
        .unwrap_or_default();
    // 是否管理员：注入 GraphQL Context，供「完全访问」等特权能力的后端强制校验。
    // 未登录/旧 token 缺省 false——特权能力 fail-closed（宁可误判非管理员也不放行）。
    let is_admin = opt_user.as_ref().is_some_and(|u| u.is_admin);
    // 认证方式：通过 Authorization: Bearer 头成功认证 = API Token（程序化访问）。
    // API Token 认证的请求受限——仅允许删除会话（见 graphql 删除 resolver 守卫）；
    // 账号登录（Cookie JWT）与未登录不受此限。
    let via_api_token = opt_user.is_some()
        && headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split_once(' '))
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"));

    // 审计预提取：判断写操作 + 取操作名/变量（须在 execute 消费 request 之前）。
    let mut request = request;
    let is_mutation = crate::domain::audit::is_mutation_request(&mut request);
    let op_name = crate::domain::audit::operation_from_query(&request.query);
    let vars_json = serde_json::to_value(&request.variables).unwrap_or_default();

    let response = schema
        .execute(
            request
                .data(user_id.clone())
                .data(via_api_token)
                .data(is_admin),
        )
        .await;

    // 写操作落审计：异步 spawn，不阻塞响应；DB 不可用时跳过；失败仅丢日志。
    if is_mutation && !op_name.is_empty() {
        let entry = crate::domain::audit::AuditEntry {
            user_id: user_id.clone(),
            actor,
            source: if via_api_token { "api_token" } else { "web" }.to_string(),
            operation: op_name,
            target_id: crate::domain::audit::extract_target_id(&vars_json),
            success: response.is_ok(),
            detail: crate::domain::audit::redact_variables(vars_json).to_string(),
            ip: crate::domain::audit::client_ip(&headers),
        };
        if let Some(store) = state.audit_store.as_ref() {
            let store = store.clone();
            tokio::spawn(async move {
                let _ = store.record(entry).await;
            });
        }
    }

    axum::Json(response)
}

/// 高性能监控 API 健康检查（保留 REST）
async fn monitor_fast_health() -> impl IntoResponse {
    Json(response::ok(json!({
        "status": "ok",
        "service": "monitor-fast-api",
        "cache_capacity": 10000
    })))
}

// ========================================================================
//  小 handler（路由辅助）
// ========================================================================

/// 编译期嵌入的前端构建产物（vite 输出到 `static/`）。
/// release 编译时打包进二进制 → 单文件自包含部署；debug 时运行时读磁盘
/// （开发改前端无需重编 Rust）。前端必须先 `npm run build` 再编译后端（release）。
#[derive(RustEmbed)]
#[folder = "static/"]
struct FrontendAsset;

/// 按相对路径（相对 `static/` 根，如 `index.html`、`assets/index-xxx.js`）取嵌入文件，
/// 返回带正确 `Content-Type` / 缓存头 / `ETag` 的响应；文件不存在返回 `None`。
///
/// - `assets/*` 是内容 hash 文件名 → 长缓存不可变；`index.html` 等入口/根文件不缓存。
/// - `ETag` 取内容 sha256 前 8 字节（release 下编译期预算好的常量，零运行时开销）。
fn serve_embedded(rel_path: &str) -> Option<axum::response::Response> {
    let file = FrontendAsset::get(rel_path)?;
    let ct = mime_guess::from_path(rel_path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let cache = if rel_path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let hash = file.metadata.sha256_hash();
    let short = u64::from_be_bytes(hash[..8].try_into().unwrap());
    let etag = format!("\"{short:016x}\"");

    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, ct.as_str())],
        file.data.into_owned(),
    )
        .into_response();
    resp.headers_mut()
        .insert(header::ETAG, etag.parse().unwrap());
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, cache.parse().unwrap());
    Some(resp)
}

async fn serve_index() -> axum::response::Response {
    serve_embedded("index.html").unwrap_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "index.html 未嵌入二进制（检查 release 构建前是否已 npm run build）",
        )
            .into_response()
    })
}

async fn serve_assets(Path(path): Path<String>) -> axum::response::Response {
    let safe_path = path.replace("..", "").replace('\\', "/");
    let rel = format!("assets/{safe_path}");
    serve_embedded(&rel).unwrap_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            b"Not Found".to_vec(),
        )
            .into_response()
    })
}

/// 截图读取：`/api/screenshots/{*path}`
///
/// path 两段 `{session_id}/{filename}` → 按会话隔离存储的新格式；单段 `{filename}` → 历史
/// 扁平兼容。鉴权：auth 启用时强制登录 + 校验当前用户拥有该会话（adk session 按 user 查），
/// 无权 403、未登录 401；auth 未启用（单机本地模式）放行。路径段做防穿越校验。
async fn serve_screenshot(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    Path(rest): Path<String>,
) -> axum::response::Response {
    // 解析：rsplit_once 取最后 / → (session_id, filename)；单段 → 历史 filename
    let (session_id, filename) = match rest.rsplit_once('/') {
        Some((sid, fname)) => (Some(sid), fname),
        None => (None, rest.as_str()),
    };
    // 防穿越：各路径段必须安全（无 / \ ..）
    if !is_safe_screenshot_segment(filename) {
        return screenshot_not_found();
    }
    if let Some(sid) = session_id {
        if !is_safe_screenshot_segment(sid) {
            return screenshot_not_found();
        }
    }

    let auth_enabled = state.auth.is_some();
    let key = if let Some(sid) = session_id {
        // 新格式：auth 启用时校验当前用户拥有该会话
        if auth_enabled {
            let user = match opt_user {
                Some(u) => u,
                None => return screenshot_unauthorized(),
            };
            if !session_belongs_to_user(&state, &user.user_id, sid).await {
                return screenshot_forbidden();
            }
        }
        format!("screenshots/{sid}/{filename}")
    } else {
        // 历史扁平格式（不考虑历史数据迁移，直接 404）
        if auth_enabled && opt_user.is_none() {
            return screenshot_unauthorized();
        }
        return screenshot_not_found();
    };

    // 从对象存储代理读取（保留登录 + 会话归属鉴权，不暴露对象存储内部）
    let object_store = match &state.object_store {
        Some(os) => os,
        None => return screenshot_unavailable(),
    };
    match object_store.get(&key).await {
        Ok(content) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, screenshot_mime(filename))],
            content.to_vec(),
        )
            .into_response(),
        Err(_) => screenshot_not_found(),
    }
}

/// 按文件名后缀推断截图 MIME(jpg/webp/gif/png),避免一律 image/png 与实际格式不符
fn screenshot_mime(filename: &str) -> &'static str {
    let f = filename.to_ascii_lowercase();
    if f.ends_with(".jpg") || f.ends_with(".jpeg") {
        "image/jpeg"
    } else if f.ends_with(".webp") {
        "image/webp"
    } else if f.ends_with(".gif") {
        "image/gif"
    } else {
        "image/png"
    }
}

/// 截图路径段安全校验：非空、长度合法、无 / \ ..（防路径穿越）
fn is_safe_screenshot_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 256
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains("..")
}

fn screenshot_not_found() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        b"Not Found".to_vec(),
    )
        .into_response()
}

fn screenshot_unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        b"Unauthorized".to_vec(),
    )
        .into_response()
}

fn screenshot_forbidden() -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        b"Forbidden".to_vec(),
    )
        .into_response()
}

/// 会话工作区文件下载/在线看：`/api/sessions/{session_id}/files/{*path}`
///
/// serve 该会话工作区内的产物文件(报表/导出等)给浏览器。鉴权同 screenshots:
/// auth 启用时强制登录 + 校验会话归属;路径双重防穿越(分段校验 + canonicalize 必须
/// 在该会话工作区内)。HTML 走 inline(浏览器直接看),其余走 attachment(下载)。
async fn serve_session_file(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    Path((session_id, rel)): Path<(String, String)>,
) -> axum::response::Response {
    if !is_safe_screenshot_segment(&session_id) {
        return session_file_not_found();
    }
    // rel 防穿越:去前导 /,逐段安全(禁 .. / \ 空)
    let rel_clean = rel.trim_start_matches('/');
    let segs: Vec<&str> = rel_clean.split('/').collect();
    if rel_clean.is_empty() || !segs.iter().all(|s| is_safe_screenshot_segment(s)) {
        return session_file_not_found();
    }
    // 鉴权 + 会话归属(同 screenshots)
    if state.auth.is_some() {
        let user = match opt_user {
            Some(u) => u,
            None => return screenshot_unauthorized(),
        };
        if !session_belongs_to_user(&state, &user.user_id, &session_id).await {
            return screenshot_forbidden();
        }
    }
    // 解析到工作区文件 + canonicalize 防穿越(必须在该会话工作区内 + 是文件)
    let base = state.config.workspace_session_dir(&session_id);
    let canon_base = match std::fs::canonicalize(&base) {
        Ok(b) => b,
        Err(_) => return session_file_not_found(),
    };
    let target = canon_base.join(rel_clean);
    let canon_target = match std::fs::canonicalize(&target) {
        Ok(t) => t,
        Err(_) => return session_file_not_found(),
    };
    if !canon_target.starts_with(&canon_base) || !canon_target.is_file() {
        return session_file_not_found();
    }
    let bytes = match std::fs::read(&canon_target) {
        Ok(b) => b,
        Err(_) => return session_file_not_found(),
    };
    let raw_fname = canon_target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    // Content-Disposition 文件名只用 ASCII(HTTP header 值禁止非 ASCII)
    let safe_fname: String = raw_fname
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .collect();
    let safe_fname = if safe_fname.is_empty() {
        "file".to_string()
    } else {
        safe_fname
    };
    let mime = workspace_file_mime(raw_fname);
    let disp = if mime.starts_with("text/html") {
        format!("inline; filename=\"{}\"", safe_fname)
    } else {
        format!("attachment; filename=\"{}\"", safe_fname)
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, disp.as_str()),
        ],
        bytes,
    )
        .into_response()
}

/// 按文件名后缀推断工作区产物的 MIME(报表类为主)
fn workspace_file_mime(filename: &str) -> &'static str {
    let f = filename.to_ascii_lowercase();
    if f.ends_with(".html") || f.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if f.ends_with(".csv") {
        "text/csv; charset=utf-8"
    } else if f.ends_with(".json") {
        "application/json"
    } else if f.ends_with(".pdf") {
        "application/pdf"
    } else if f.ends_with(".xlsx") {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    } else if f.ends_with(".png") {
        "image/png"
    } else if f.ends_with(".jpg") || f.ends_with(".jpeg") {
        "image/jpeg"
    } else if f.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

fn session_file_not_found() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        b"Not Found".to_vec(),
    )
        .into_response()
}

fn screenshot_unavailable() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        b"Object Storage Unavailable".to_vec(),
    )
        .into_response()
}

/// 校验会话归属：adk session 按 (app, user, session) 查询，get 成功（sessions 表 fetch_one 命中）
/// 即表示该 user 拥有此会话。归属判断只依赖 sessions 表行，与 events 无关。
async fn session_belongs_to_user(state: &AppState, user_id: &str, session_id: &str) -> bool {
    let get_req = adk_rust::session::GetRequest {
        app_name: "cortex-agent".to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        num_recent_events: Some(1),
        after: None,
    };
    state.adk_session_service.get(get_req).await.is_ok()
}

/// 上传图片附件（multipart/form-data，字段名 file），返回 base64 data URL 供会话多模态输入直接引用。
///
/// 限制：单文件 ≤ 10MB；MIME 白名单 image/png|jpeg|webp|gif。
/// 返回 `{ code:0, data:{ url, filename, mime_type, size } }`，其中 `url` 为 `data:<mime>;base64,...`。
async fn handle_upload_image(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // auth 启用时强制登录(与 serve_screenshot 鉴权基线一致),未登录拒绝上传(防匿名滥用共享存储)
    if state.auth.is_some() && opt_user.is_none() {
        return Json(response::err(
            response::code::UNAUTHORIZED,
            "请先登录后再上传图片",
        ));
    }
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() != Some("file") {
            continue;
        }
        let mime = field
            .content_type()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "image/png".to_string());
        let allowed = ["image/png", "image/jpeg", "image/webp", "image/gif"];
        if !allowed.contains(&mime.as_str()) {
            return Json(response::err(
                response::code::INVALID_PARAMS,
                format!("不支持的图片类型: {mime}（仅支持 png/jpeg/webp/gif）"),
            ));
        }
        let filename = field.file_name().unwrap_or("upload.png").to_string();
        match field.bytes().await {
            Ok(bytes) => {
                if bytes.len() > 10 * 1024 * 1024 {
                    return Json(response::err(
                        response::code::INVALID_PARAMS,
                        "图片大小超过 10MB 限制",
                    ));
                }
                // 上传到对象存储，返回 presigned URL（模型与前端凭此直链拉取，无需 base64 入库）
                let object_store = match &state.object_store {
                    Some(os) => os.clone(),
                    None => {
                        return Json(response::err(
                            response::code::INTERNAL,
                            "对象存储未启用，无法上传图片",
                        ))
                    }
                };
                let user_id = opt_user
                    .as_ref()
                    .map(|u| u.user_id.clone())
                    .unwrap_or_else(|| "anonymous".to_string());
                let ext = match mime.as_str() {
                    "image/png" => "png",
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    "image/gif" => "gif",
                    _ => "png",
                };
                let key = format!("uploads/{user_id}/{}.{}", uuid::Uuid::now_v7().simple(), ext);
                if let Err(e) = object_store.put(&key, bytes.clone()).await {
                    return Json(response::err(
                        response::code::INTERNAL,
                        format!("上传对象存储失败: {e}"),
                    ));
                }
                let url = match object_store
                    .presign_get(&key, object_store.default_presign_ttl())
                    .await
                {
                    Ok(u) => u,
                    Err(e) => {
                        return Json(response::err(
                            response::code::INTERNAL,
                            format!("生成下载链接失败: {e}"),
                        ))
                    }
                };
                crate::domain::audit::spawn_record(
                    state.audit_store.as_ref(),
                    crate::domain::audit::AuditEntry {
                        user_id: user_id.clone(),
                        actor: opt_user
                            .as_ref()
                            .map(|u| u.name.clone())
                            .unwrap_or_default(),
                        source: "web".to_string(),
                        operation: "upload_image".to_string(),
                        target_id: String::new(),
                        success: true,
                        detail: json!({
                            "filename": filename,
                            "mime_type": mime,
                            "size": bytes.len(),
                            "key": key,
                        })
                        .to_string(),
                        ip: crate::domain::audit::client_ip(&headers),
                    },
                );
                return Json(response::ok(json!({
                    "url": url,
                    "filename": filename,
                    "mime_type": mime,
                    "size": bytes.len(),
                })));
            }
            Err(e) => {
                return Json(response::err(
                    response::code::INVALID_PARAMS,
                    format!("读取上传数据失败: {e}"),
                ));
            }
        }
    }
    Json(response::err(
        response::code::INVALID_PARAMS,
        "未找到上传字段 file",
    ))
}

/// SPA fallback — 未匹配的 GET 请求：先按路径查嵌入静态资源（favicon.svg / icons.svg 等
/// 根级文件），命中直接返回；否则回退 `index.html`（支持 Vue Router history 模式）。
async fn spa_fallback(req: axum::extract::Request) -> axum::response::Response {
    if req.method() == axum::http::Method::GET {
        let rel = req.uri().path().trim_start_matches('/');
        if let Some(resp) = serve_embedded(rel) {
            return resp;
        }
        serve_embedded("index.html").unwrap_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "index.html 未嵌入二进制",
            )
                .into_response()
        })
    } else {
        (StatusCode::NOT_FOUND, "Not Found").into_response()
    }
}

async fn health_check() -> impl IntoResponse {
    Json(response::ok(json!({ "status": "ok" })))
}

#[derive(serde::Deserialize)]
struct ShellApproveRequest {
    approval_id: String,
    decision: String,
}

async fn handle_shell_approve(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    Json(req): Json<ShellApproveRequest>,
) -> impl IntoResponse {
    let decision = match req.decision.to_lowercase().as_str() {
        "approved" | "approve" | "yes" | "true" => shell_approval::ApprovalDecision::Approved,
        _ => shell_approval::ApprovalDecision::Rejected,
    };
    let resolved = state
        .shell_approval_registry
        .resolve(&req.approval_id, decision)
        .await;
    crate::domain::audit::spawn_record(
        state.audit_store.as_ref(),
        crate::domain::audit::AuditEntry {
            user_id: opt_user
                .as_ref()
                .map(|u| u.user_id.clone())
                .unwrap_or_default(),
            actor: opt_user.as_ref().map(|u| u.name.clone()).unwrap_or_default(),
            source: "web".to_string(),
            operation: "shell_approve".to_string(),
            target_id: req.approval_id.clone(),
            success: resolved,
            detail: json!({ "decision": req.decision }).to_string(),
            ip: crate::domain::audit::client_ip(&headers),
        },
    );
    match resolved {
        true => Json(response::ok(json!({ "resolved": true }))),
        false => Json(response::err(
            response::code::NOT_FOUND,
            "审批请求不存在或已过期",
        )),
    }
}

/// 设备目录 API — 返回所有厂商和设备类型（从 system_builtin 缓存）
pub async fn catalog(state: &AppState) -> serde_json::Value {
    response::ok(state.catalog.to_json().await)
}

pub async fn models(state: &AppState) -> serde_json::Value {
    // 模型列表的唯一数据源是模型供应商存储（DB）；未初始化时返回错误
    match state.model_provider_store.as_ref() {
        Some(store) => match store.model_options().await {
            Ok((default_id, models)) => response::ok(json!({
                "default_model_id": default_id,
                "models": models
            })),
            Err(e) => {
                tracing::warn!("[server] 读取模型列表失败: {}", e);
                response::err(response::code::LLM, format!("读取模型列表失败: {}", e))
            }
        },
        None => response::err(
            response::code::LLM,
            "模型供应商存储未初始化，请检查数据库是否启用并完成模型配置",
        ),
    }
}

// ========================================================================
//  高性能监控 API（已迁移至 GraphQL monitorOids / monitorCalculate）
// ========================================================================

/// 拉取插件的 OID 列表（命中缓存时直接返回；否则计算并缓存）
pub async fn monitor_get_oids(state: &AppState, plugin_id: &str) -> serde_json::Value {
    if plugin_id.is_empty() {
        return response::err(response::code::INVALID_PARAMS, "plugin_id is required");
    }

    if let Some(cached) = state.oid_cache.get(plugin_id) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(cached.as_ref()) {
            return v;
        }
    }

    let oids = state.plugin_manager.prepare_oids(plugin_id);
    let response = response::ok(json!({
        "plugin_id": plugin_id,
        "oids": serde_json::from_str::<serde_json::Value>(&oids).unwrap_or(json!([]))
    }));

    if let Ok(bytes) = serde_json::to_vec(&response) {
        if state.oid_cache.len() >= 10000 {
            state.oid_cache.clear();
        }
        state
            .oid_cache
            .insert(plugin_id.to_string(), bytes::Bytes::from(bytes));
    }

    response
}

/// 调用插件 parse 函数计算监控结果
pub async fn monitor_calculate(
    state: &AppState,
    plugin_id: &str,
    oid_values: &serde_json::Value,
) -> serde_json::Value {
    if plugin_id.is_empty() {
        return response::err(response::code::INVALID_PARAMS, "plugin_id is required");
    }

    let oid_values_json = serde_json::to_string(oid_values).unwrap_or_default();
    let results = state.plugin_manager.parse(plugin_id, &oid_values_json);
    response::ok(json!({
        "plugin_id": plugin_id,
        "results": serde_json::from_str::<serde_json::Value>(&results).unwrap_or(json!([]))
    }))
}
