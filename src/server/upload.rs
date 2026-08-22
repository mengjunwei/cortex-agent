use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, Path, State};
use axum::response::IntoResponse;
use serde_json::json;

use super::AppState;
use super::{auth, knowledge_instances, response, sse};
use crate::domain::audit;

/// 上传图片 / 文档附件（multipart/form-data，字段名 file），存对象存储并返回 presigned URL。
///
/// 支持：图片 png/jpeg/webp/gif；文档 pdf/word/excel/ppt/csv/txt/md/rtf（由 markitdown 解析）。
/// 限制：单文件 ≤ 20MB（路由层 DefaultBodyLimit 同步放开）。
/// 返回 `{ code:0, data:{ url, filename, mime_type, size } }`。
pub(super) async fn handle_upload(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // auth 启用时强制登录(与 serve_screenshot 鉴权基线一致),未登录拒绝上传(防匿名滥用共享存储)
    if state.auth.is_some() && opt_user.is_none() {
        return Json(response::err(
            response::code::UNAUTHORIZED,
            "请先登录后再上传",
        ));
    }
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() != Some("file") {
            continue;
        }
        let filename = field.file_name().unwrap_or("upload").to_string();
        let content_type = field
            .content_type()
            .map(|s| s.to_string())
            .unwrap_or_default();
        // 按文件名后缀推断 (mime, ext)；浏览器对 Office 文档的 MIME 常不可靠，故以后缀为准
        let Some((mime, ext)) = infer_upload_meta(&filename, &content_type) else {
            return Json(response::err(
                response::code::INVALID_PARAMS,
                format!(
                    "不支持的文件类型: {filename}（支持 png/jpeg/webp/gif/pdf/word/excel/ppt/csv/txt/md/rtf）"
                ),
            ));
        };
        match field.bytes().await {
            Ok(bytes) => {
                if bytes.len() > 20 * 1024 * 1024 {
                    return Json(response::err(
                        response::code::INVALID_PARAMS,
                        "文件大小超过 20MB 限制",
                    ));
                }
                // 上传到对象存储，返回 presigned URL（前端回填附件，模型/前端凭此直链拉取）
                let object_store = match &state.object_store {
                    Some(os) => os.clone(),
                    None => {
                        return Json(response::err(
                            response::code::INTERNAL,
                            "对象存储未启用，无法上传",
                        ));
                    }
                };
                let user_id = opt_user
                    .as_ref()
                    .map(|u| u.user_id.clone())
                    .unwrap_or_else(|| "anonymous".to_string());
                let key = format!(
                    "uploads/{user_id}/{}.{}",
                    uuid::Uuid::now_v7().simple(),
                    ext
                );
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
                        ));
                    }
                };
                audit::spawn_record(
                    state.audit_store.as_ref(),
                    audit::AuditEntry {
                        user_id: user_id.clone(),
                        actor: opt_user
                            .as_ref()
                            .map(|u| u.name.clone())
                            .unwrap_or_default(),
                        source: "web".to_string(),
                        operation: "upload".to_string(),
                        target_id: String::new(),
                        success: true,
                        detail: json!({
                            "filename": filename,
                            "mime_type": mime,
                            "size": bytes.len(),
                            "key": key,
                        })
                        .to_string(),
                        ip: crate::server::audit::client_ip(&headers),
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

/// 按文件名后缀推断 (mime, ext)；未知后缀回退用浏览器上报的 content_type 再判一次。
/// 返回 None 表示格式不在白名单。结果 ext 用于对象存储 key，mime 用于响应与附件注入。
pub(super) fn infer_upload_meta(filename: &str, content_type: &str) -> Option<(&'static str, &'static str)> {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase());
    let by_ext = |e: &str| -> Option<(&'static str, &'static str)> {
        Some(match e {
            "png" => ("image/png", "png"),
            "jpg" | "jpeg" => ("image/jpeg", "jpg"),
            "webp" => ("image/webp", "webp"),
            "gif" => ("image/gif", "gif"),
            "pdf" => ("application/pdf", "pdf"),
            "doc" => ("application/msword", "doc"),
            "docx" => (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "docx",
            ),
            "xls" => ("application/vnd.ms-excel", "xls"),
            "xlsx" => (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "xlsx",
            ),
            "ppt" => ("application/vnd.ms-powerpoint", "ppt"),
            "pptx" => (
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "pptx",
            ),
            "csv" => ("text/csv", "csv"),
            "txt" => ("text/plain", "txt"),
            "md" | "markdown" => ("text/markdown", "md"),
            "rtf" => ("application/rtf", "rtf"),
            _ => return None,
        })
    };
    // 优先后缀；无后缀或未知后缀时，按浏览器 content_type 兜底（仅图片）
    ext.as_deref().and_then(by_ext).or(match content_type {
        "image/png" => Some(("image/png", "png")),
        "image/jpeg" => Some(("image/jpeg", "jpg")),
        "image/webp" => Some(("image/webp", "webp")),
        "image/gif" => Some(("image/gif", "gif")),
        _ => None,
    })
}

/// 上传文档文件到指定知识库实例（multipart/form-data）。
///
/// 按 provider 分流（与各自能力对齐，避免无谓的本地预处理）：
/// - **Dify**：直接把原始文件交给 Dify `create_by_file`（Dify 自带文档解析）。
/// - **内置（Qdrant）**：先解析为文本——txt/md/csv 直接 UTF-8 解码，其余走 markitdown——再写入。
///
/// 表单字段：`file`（必填）、`title`、`brand`、`dev_type`、`model`（均可选）。
/// 标题缺省时取文件名（去扩展名）。单文件 ≤ 20MB（路由层放开 body 限制）。
pub(super) async fn handle_kb_doc_upload(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    Path(instance_id): Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // 写操作需要归属校验：未登录按匿名 "user" 处理（与 GraphQL 写操作基线一致）
    let user_id = opt_user
        .as_ref()
        .map(|u| u.user_id.clone())
        .unwrap_or_else(|| "user".to_string());
    let is_admin = opt_user.as_ref().map(|u| u.is_admin).unwrap_or(false);

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename = String::new();
    let mut file_ct = String::new();
    let mut title: Option<String> = None;
    let mut brand: Option<String> = None;
    let mut dev_type: Option<String> = None;
    let mut model: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                filename = field.file_name().unwrap_or("upload").to_string();
                file_ct = field.content_type().unwrap_or("").to_string();
                match field.bytes().await {
                    Ok(b) => {
                        if b.len() > 20 * 1024 * 1024 {
                            return Json(response::err(
                                response::code::INVALID_PARAMS,
                                "文件大小超过 20MB 限制",
                            ));
                        }
                        file_bytes = Some(b.to_vec());
                    }
                    Err(e) => {
                        return Json(response::err(
                            response::code::INVALID_PARAMS,
                            format!("读取上传数据失败: {e}"),
                        ));
                    }
                }
            }
            "title" | "brand" | "dev_type" | "model" => {
                let v = field.text().await.unwrap_or_default();
                let v = v.trim().to_string();
                if v.is_empty() {
                    continue;
                }
                match name.as_str() {
                    "title" => title = Some(v),
                    "brand" => brand = Some(v),
                    "dev_type" => dev_type = Some(v),
                    "model" => model = Some(v),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let Some(bytes) = file_bytes else {
        return Json(response::err(
            response::code::INVALID_PARAMS,
            "未找到上传字段 file",
        ));
    };

    // 写权限校验 + 取实例（provider_kind 决定解析策略）
    let inst = match knowledge_instances::require_writable(&state, &instance_id, &user_id, is_admin)
        .await
    {
        Ok(i) => i,
        Err(v) => return Json(v),
    };
    use crate::domain::knowledge::backend::ProviderKind;

    // 推断 (mime, ext)：复用 /api/uploads 的白名单
    let Some((mime, _ext)) = infer_upload_meta(&filename, &file_ct) else {
        return Json(response::err(
            response::code::INVALID_PARAMS,
            format!("不支持的文件类型: {filename}（支持 pdf/word/excel/ppt/csv/txt/md/rtf）"),
        ));
    };
    // 知识库文档不支持图片
    if mime.starts_with("image/") {
        return Json(response::err(
            response::code::INVALID_PARAMS,
            "知识库不支持图片文件，请上传文档类文件",
        ));
    }

    // 按 provider 构造入参：
    //  - Dify：自带解析，直接把原始文件交给它（create_by_file），不在本地 markitdown
    //  - 内置：先解析为文本（txt/md/csv 直接 UTF-8，其余走 markitdown）再写入
    let (content, file) = match ProviderKind::from_i16(inst.provider_kind) {
        Some(ProviderKind::Dify) => (
            String::new(),
            Some(crate::domain::knowledge::backend::KbFile {
                name: filename.clone(),
                mime: mime.to_string(),
                bytes,
            }),
        ),
        _ => {
            let ext_lower = filename
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            let text = match ext_lower.as_str() {
                "txt" | "md" | "markdown" | "csv" => String::from_utf8_lossy(&bytes).into_owned(),
                _ => match sse::attachment::convert_document_to_markdown(&state, bytes, mime).await
                {
                    Some(t) if !t.trim().is_empty() => t,
                    _ => {
                        return Json(response::err(
                            response::code::BUSINESS,
                            "文档解析失败或内容为空（markitdown 未配置或格式不受支持），请改用粘贴文本上传",
                        ));
                    }
                },
            };
            let text = text.trim().to_string();
            if text.is_empty() {
                return Json(response::err(response::code::BUSINESS, "文档内容为空"));
            }
            (text, None)
        }
    };

    // 标题缺省 → 文件名去扩展名
    let title = title.unwrap_or_else(|| {
        filename
            .rsplit_once('.')
            .map(|(stem, _)| stem.to_string())
            .unwrap_or_else(|| filename.clone())
    });

    let input = crate::domain::knowledge::backend::KbDocInput {
        brand: brand.unwrap_or_default(),
        dev_type: dev_type.unwrap_or_default(),
        model: model.unwrap_or_default(),
        firmware_ver: String::new(),
        title,
        content,
        user_role: "admin".to_string(),
        file,
    };
    match state
        .knowledge_manager
        .upload_to_instance(&instance_id, input)
        .await
    {
        Ok(id) => Json(response::ok(json!({ "doc_id": id, "message": "上传成功" }))),
        Err(e) => Json(response::err(response::code::NETWORK, e.to_string())),
    }
}
