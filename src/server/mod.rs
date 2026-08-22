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
//! | `/api/uploads` | POST | 上传图片/文档附件（≤20MB，存对象存储返回 presigned URL） |
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
pub(crate) mod audit;
pub(crate) mod auth;
pub(crate) mod dify_proxy;
pub(crate) mod device;
pub(crate) mod files;
pub(crate) mod graphql;
pub(crate) mod knowledge;
pub(crate) mod knowledge_instances;
pub(crate) mod mcp;
pub(crate) mod memory;
pub(crate) mod model_provider;
pub(crate) mod monitor;
pub(crate) mod owner;
pub(crate) mod response;
pub(crate) mod scheduled_task;
pub(crate) mod session;
pub(crate) mod shell_approval;
pub(crate) mod shell_approve;
pub(crate) mod skill_install;
pub(crate) mod sse;
pub(crate) mod upload;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
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

    // ── 定时任务调度引擎（tokio-cron-scheduler + postgres_storage）──
    // 需在 AppState Arc 化后启动（引擎持有 Arc<AppState>）。DB 可用且有任务存储时启动；
    // 失败仅告警降级（不影响其它功能），handler 经 state.scheduler() 拿到 None 时返回降级错误。
    if state.scheduled_task_store.is_some() {
        let db_url = state.config.db.url();
        match scheduled_task::SchedulerEngine::start(state.clone(), &db_url).await {
            Ok(engine) => {
                if state.scheduler.set(engine).is_ok() {
                    tracing::info!("[infra] 定时任务调度引擎已启动（postgres_storage 持久化）");
                }
            }
            Err(e) => {
                tracing::warn!("[infra] 定时任务调度引擎启动失败（定时任务不可用）: {e}");
            }
        }
    } else {
        tracing::warn!("[infra] 数据库未启用，定时任务功能不可用");
    }

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
        // 定时任务：/api/scheduled-tasks/*（CRUD / parse-schedule / runs / run-now）
        .merge(scheduled_task::routes())
        // 助手功能已迁移到 GraphQL，不再注册 REST 路由
        // 保留的 REST 接口（流式 / 健康检查 / 静态资源）
        .route("/", get(serve_index))
        .route("/api/health", get(health_check))
        .route("/api/v1/monitor/health", get(monitor_fast_health))
        .route("/api/run_sse", post(sse::handle_run_sse))
        .route("/api/shell-approve", post(shell_approve::handle_shell_approve))
        .route("/assets/{*path}", get(serve_assets))
        .route("/api/screenshots/{*path}", get(files::serve_screenshot))
        .route(
            "/api/sessions/{session_id}/files/{*path}",
            get(files::serve_session_file),
        )
        .route(
            "/api/uploads",
            post(upload::handle_upload).layer(DefaultBodyLimit::max(20 * 1024 * 1024)),
        )
        .route(
            "/api/kb-instances/{instance_id}/upload-file",
            post(upload::handle_kb_doc_upload).layer(DefaultBodyLimit::max(20 * 1024 * 1024)),
        )
        // 知识库图片代理：Dify 文档里的图片直连会 400（缺 dataset api_key），
        // 由服务端用「会话绑定实例」的 api_key 带 Bearer 拉取后回传。
        .route("/api/kb/proxy-image", get(dify_proxy::handle_kb_proxy_image))
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
    let is_mutation = audit::is_mutation_request(&mut request);
    let op_name = audit::operation_from_query(&request.query);
    let vars_json = serde_json::to_value(&request.variables).unwrap_or_default();

    let response = schema
        .execute(request.data(user_id.clone()).data(graphql::GqlAuthCtx {
            is_admin,
            via_api_token,
        }))
        .await;

    // 写操作落审计：异步 spawn，不阻塞响应；DB 不可用时跳过；失败仅丢日志。
    if is_mutation && !op_name.is_empty() {
        let entry = crate::domain::audit::AuditEntry {
            user_id: user_id.clone(),
            actor,
            source: if via_api_token { "api_token" } else { "web" }.to_string(),
            operation: op_name,
            target_id: audit::extract_target_id(&vars_json),
            success: response.is_ok(),
            detail: audit::redact_variables(vars_json).to_string(),
            ip: audit::client_ip(&headers),
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


/// 设备目录 API — 返回所有厂商和设备类型（从 system_builtin 缓存）
pub async fn catalog(state: &AppState) -> serde_json::Value {
    response::ok(state.catalog.to_json().await)
}

pub async fn models(state: &AppState, user_id: &str, is_admin: bool) -> serde_json::Value {
    // 模型列表的唯一数据源是模型供应商存储（DB）；按归属隔离（普通用户只看自己的）
    match state.model_provider_store.as_ref() {
        Some(store) => match store.model_options(user_id, is_admin).await {
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

