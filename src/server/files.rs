use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

use super::AppState;
use super::auth;

/// 截图读取：`/api/screenshots/{*path}`
///
/// path 两段 `{session_id}/{filename}` → 按会话隔离存储的新格式；单段 `{filename}` → 历史
/// 扁平兼容。鉴权：auth 启用时强制登录 + 校验当前用户拥有该会话（adk session 按 user 查），
/// 无权 403、未登录 401；auth 未启用（单机本地模式）放行。路径段做防穿越校验。
pub(super) async fn serve_screenshot(
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

/// 会话工作区文件下载/在线看：`/api/sessions/{session_id}/files/{*path}`
///
/// serve 该会话工作区内的产物文件(报表/导出等)给浏览器。鉴权同 screenshots:
/// auth 启用时强制登录 + 校验会话归属;路径双重防穿越(分段校验 + canonicalize 必须
/// 在该会话工作区内)。HTML 走 inline(浏览器直接看),其余走 attachment(下载)。
pub(super) async fn serve_session_file(
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
    let mime = workspace_file_mime(raw_fname);
    let pct = rfc5987_encode(raw_fname);
    let fallback = ascii_fallback_name(raw_fname);
    let disp = if mime.starts_with("text/html") {
        format!(
            "inline; filename=\"{}\"; filename*=UTF-8''{}",
            fallback, pct
        )
    } else {
        format!(
            "attachment; filename=\"{}\"; filename*=UTF-8''{}",
            fallback, pct
        )
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

// ======================== 内部辅助函数 ========================

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
    !s.is_empty() && s.len() < 256 && !s.contains('/') && !s.contains('\\') && !s.contains("..")
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
    } else if f.ends_with(".docx") {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    } else if f.ends_with(".pptx") {
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
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

/// RFC 5987 百分号编码，用于 Content-Disposition 的 `filename*=UTF-8''<...>`。
fn rfc5987_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let attr_char = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'&'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'{'
                    | b'|'
                    | b'}'
                    | b'~'
            );
        if attr_char {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// 纯 ASCII 的 `filename=` 兜底名。
fn ascii_fallback_name(raw: &str) -> String {
    let safe: String = raw
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .collect();
    if safe.is_empty() {
        return "file".to_string();
    }
    if safe.chars().next().unwrap_or(' ').is_ascii_alphanumeric() {
        return safe;
    }
    let ext = safe.split_once('.').map(|(_, e)| e).unwrap_or("");
    if ext.is_empty() {
        "file".to_string()
    } else {
        format!("file.{}", ext)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：中文名「古诗词.pptx」必须保留原名 + 扩展名（界面下载丢后缀的 bug）。
    #[test]
    fn rfc5987_keeps_unicode_and_extension() {
        assert_eq!(
            rfc5987_encode("古诗词.pptx"),
            "%E5%8F%A4%E8%AF%97%E8%AF%8D.pptx"
        );
        assert_eq!(rfc5987_encode("a b.txt"), "a%20b.txt");
        assert_eq!(rfc5987_encode("report.pdf"), "report.pdf");
    }

    #[test]
    fn ascii_fallback_preserves_extension_for_chinese_name() {
        assert_eq!(ascii_fallback_name("古诗词.pptx"), "file.pptx");
        assert_eq!(ascii_fallback_name("report.pdf"), "report.pdf");
        assert_eq!(ascii_fallback_name("报告"), "file");
    }
}
