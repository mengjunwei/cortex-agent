//! 多模态输入附件处理：上传图片按长边降采样，并把用户文本 + 附件拼成提交给 Runner 的 Content。
//!
//! - `data:<mime>;base64,...`（本地图片）→ InlineData（先按长边上限 resize 降采样再注入，省 token）
//! - 对象存储 http(s) URL：
//!   - 图片：后端取字节 → resize → InlineData（云端 LLM 拉不到内网 URL，故由后端统一内联）
//!   - 文档（Office/PDF/...）：后端取字节 → **原始字节落盘到会话工作区 `uploads/`（供 agent 作附件
//!     发送/转发）** → data URI → 调 markitdown MCP 转 markdown → 以文本注入（并附带本地路径）；
//!     解析服务不可用则降级为「原文已落盘但无法解析」提示（仍给出路径）。
//!
//! 关键：presigned URL 的 host 是后端本机（如 localhost:9000），云端 LLM 与跨机 markitdown-mcp
//! 都拉不到，所以图片/文档的字节统一由后端（本机可达）取回后再内联/转换。

use std::time::Duration;

use base64::Engine as _;

use super::screenshot::decode_data_url;
use super::types::{InputAttachment, InputMessage};
use crate::server::AppState;

/// 上传图片长边上限：Anthropic 1568px / OpenAI 2048px，取保守的 1568（两端都安全）。
/// 超过此尺寸的原图会被降采样，避免白白烧 token + 被 provider 静默降采样。
const UPLOAD_IMAGE_MAX_SIDE: u32 = 1568;

/// markitdown MCP server 的 slug（在 config.toml 的 [[mcp.seeds]] 里配置，
/// 后端按此 slug 编程式调用其 convert_to_markdown 工具做文档解析）。
const MARKITDOWN_SLUG: &str = "markitdown";

/// 后端拉取附件字节（presigned URL / 对象存储）的超时（含连接 + 读体）。
const FETCH_BYTES_TIMEOUT: Duration = Duration::from_secs(30);
/// adk-rust 的 `with_inline_data` 硬上限是 10MB（超出直接 panic）。内联图片必须低于此值，
/// 否则降级为文本提示——绝不把超大字节送进去（曾因上传上限提到 20MB 而可达此 panic）。
const MAX_INLINE_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// 后端拉取附件字节的上限（匹配上传 20MB 上限），防恶意/超大响应流式撑爆内存。
const MAX_FETCH_BYTES: usize = 20 * 1024 * 1024;
/// markitdown 输出注入文本的字符上限，防巨型文档撑爆上下文 / 费用（超出截断）。
const MAX_MARKDOWN_CHARS: usize = 200_000;

/// 文档类附件需要 markitdown 解析的扩展名（小写，不含点）。
/// 覆盖 markitdown 支持的常见格式：Office、PDF、以及纯文本/结构化文本。
const DOC_EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "rtf", "odt", "ods", "odp", "csv", "txt",
    "md", "markdown", "html", "htm", "xml", "json",
];

/// 对上传图片按长边上限降采样。非 image/* 或解码/重编码失败 → 原样返回（不阻塞流程）。
fn resize_image_if_needed(data: Vec<u8>, mime: &str) -> (Vec<u8>, String) {
    if !mime.starts_with("image/") {
        return (data, mime.to_string());
    }
    let img = match image::load_from_memory(&data) {
        Ok(im) => im,
        Err(_) => return (data, mime.to_string()),
    };
    let longest = img.width().max(img.height());
    if longest <= UPLOAD_IMAGE_MAX_SIDE {
        return (data, mime.to_string());
    }
    let resized = img.resize(
        UPLOAD_IMAGE_MAX_SIDE,
        UPLOAD_IMAGE_MAX_SIDE,
        image::imageops::FilterType::Lanczos3,
    );
    let mut buf = Vec::new();
    // PNG 保持透明重编码；其余转 JPEG（更小，照片/截图质量足够）
    let (out_format, out_mime) = if mime == "image/png" {
        (image::ImageFormat::Png, "image/png")
    } else {
        (image::ImageFormat::Jpeg, "image/jpeg")
    };
    match resized.write_to(&mut std::io::Cursor::new(&mut buf), out_format) {
        Ok(_) => (buf, out_mime.to_string()),
        Err(_) => (data, mime.to_string()),
    }
}

/// 判断附件是否为「需要 markitdown 解析的文档」类型（非图片）。
fn is_document_attachment(att: &InputAttachment) -> bool {
    if att.mime_type.starts_with("image/") {
        return false;
    }
    // 优先用文件名后缀判断（浏览器对 Office 文档的 MIME 常不可靠）
    if let Some(fname) = &att.filename {
        if let Some(ext) = fname.rsplit('.').next() {
            if DOC_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
                return true;
            }
        }
    }
    // 无文件名或后缀不在列表 → 按已知文档 MIME 兜底
    matches!(
        att.mime_type.as_str(),
        "application/pdf"
            | "text/plain"
            | "text/csv"
            | "text/markdown"
            | "text/html"
            | "application/json"
            | "application/xml"
            | "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-powerpoint"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/rtf"
    )
}

/// 净化上传文件名为「可作单段路径 + 可安全出现在 `<document filename="...">` 属性」的形式。
///
/// 剥除：路径分隔符 `/` `\` 与连续点 `..`（防目录穿越）、`"` `<` `>` `&`（防破坏 XML 包装
/// /提示注入）、控制符；限长 120 字符。原始名为空或净化后为空 → 回退「文档」。
///
/// 同一净化结果同时用于磁盘文件名与注入文本，保证二者一致——agent 在文本里看到的名字即
/// 磁盘上的真实文件名（重名去重时落盘路径会带 uuid 后缀，由 [`persist_uploaded_document`] 返回）。
fn sanitize_filename(raw: &str) -> String {
    // 1) 剥除路径分隔符 / XML 危险字符 / 常见控制符
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, '"' | '<' | '>' | '&' | '/' | '\\' | '\n' | '\r' | '\t'))
        .collect();
    // 2) 折叠连续点（防 `..` 路径穿越段）：name..pptx → name.pptx；单点扩展名保留
    let mut dedotted = String::with_capacity(cleaned.len());
    let mut prev_dot = false;
    for c in cleaned.chars() {
        if c == '.' {
            if prev_dot {
                continue;
            }
            prev_dot = true;
        } else {
            prev_dot = false;
        }
        dedotted.push(c);
    }
    // 3) 去掉其余 C0 控制符并限长
    let truncated: String = dedotted
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if truncated.is_empty() {
        "文档".to_string()
    } else {
        truncated
    }
}

/// 从 URL 中提取 host[:port]（仅 http/https；解析失败返回 None）。
fn url_host(s: &str) -> Option<String> {
    let rest = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    Some(authority.to_string())
}

/// 仅允许后端取回**本系统对象存储**的附件字节：att.url 的主机必须与 `[object_storage].endpoint`
/// 一致。att.url 来自请求体、客户端可控——无此校验即构成 SSRF（可被指向云元数据/内网管理端口，
/// 且字节会经 markitdown 回流进模型上下文）。
fn is_allowed_attachment_url(state: &AppState, url: &str) -> bool {
    let Some(want) = url_host(&state.config.object_storage.endpoint) else {
        return false;
    };
    url_host(url).map(|h| h == want).unwrap_or(false)
}

/// 从 presigned URL 取回附件原始字节。
///
/// 安全 / 健壮性：
/// - host 白名单（`is_allowed_attachment_url`）防 SSRF；
/// - 校验 HTTP 状态（非 2xx 视为失败，避免把 404/5xx 错误页当正文喂给 markitdown/模型）；
/// - 流式读取并在 `MAX_FETCH_BYTES` 处截断，防超大响应耗尽内存；
/// - 连接 + 读体整体受 `FETCH_BYTES_TIMEOUT` 约束，防慢速 drip 式 DoS。
///
/// 失败返回 None（调用方降级）。
async fn fetch_attachment_bytes(state: &AppState, att: &InputAttachment) -> Option<Vec<u8>> {
    if !is_allowed_attachment_url(state, &att.url) {
        tracing::warn!(
            "[attachment] 拒绝非对象存储主机的附件 URL（防 SSRF）: {}",
            att.url
        );
        return None;
    }
    use futures::StreamExt;
    let fetch = async {
        let resp = reqwest::get(&att.url).await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if buf.len() + chunk.len() > MAX_FETCH_BYTES {
                return Err(format!("超出最大字节数 {MAX_FETCH_BYTES}"));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok::<_, String>(buf)
    };
    match tokio::time::timeout(FETCH_BYTES_TIMEOUT, fetch).await {
        Ok(Ok(bytes)) => Some(bytes),
        Ok(Err(e)) => {
            tracing::warn!("[attachment] 读取附件失败 url={}: {e}", att.url);
            None
        }
        Err(_) => {
            tracing::warn!("[attachment] 请求附件超时 url={}", att.url);
            None
        }
    }
}

/// 调 markitdown MCP 把文档字节转成 markdown 文本。
/// 字节先编码为 data URI（避免 markitdown 跨机去拉内网 URL），再调 convert_to_markdown。
/// MCP 服务未配置 / 未启用 / 调用失败 / 返回空 → 返回 None（调用方注入降级提示）。
pub(crate) async fn convert_document_to_markdown(
    state: &AppState,
    bytes: Vec<u8>,
    mime: &str,
) -> Option<String> {
    let mgr = state.mcp_manager.as_ref()?;
    // data URI：markitdown 原生支持 data: 入参，无需它回源拉取
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let data_uri = format!("data:{mime};base64,{b64}");
    match mgr
        .call_tool_by_slug(
            MARKITDOWN_SLUG,
            "convert_to_markdown",
            serde_json::json!({ "uri": data_uri }),
            None,
        )
        .await
    {
        Ok(text) if !text.trim().is_empty() => {
            tracing::info!(
                "[attachment] markitdown 解析成功: {} 字符",
                text.chars().count()
            );
            Some(text)
        }
        Ok(_) => {
            tracing::warn!("[attachment] markitdown 返回空内容");
            None
        }
        Err(e) => {
            tracing::warn!("[attachment] markitdown 解析失败（降级为仅提示）: {e}");
            None
        }
    }
}

/// 把上传文档的原始字节落盘到会话工作区的 `uploads/` 子目录，返回落地文件的绝对路径。
///
/// 缘由：markitdown 只把文档转成文本注入对话，原始二进制对 agent 不可达——用户「把这个文件
/// 发邮件」时 agent 找不到原文件。落盘到会话工作区后，agent 可凭绝对路径直接作附件发送/转发；
/// 同时复用 `/api/sessions/{id}/files/` 鉴权下载端点、纳入沙箱快照备份、随会话删除清理。
///
/// 设计取舍：对象存储仍是上传的持久源（立即持久、不污染每轮快照），此处的本地副本是给 agent
/// 本地工具读取/发送用的工作副本（working copy）。落盘失败返回 None（调用方降级为不附带路径的
/// 提示，不阻塞解析/注入流程）。
///
/// 文件名经 [`sanitize_filename`] 净化；同一会话重名上传时在扩展名前追加短 uuid 防静默覆盖。
async fn persist_uploaded_document(
    state: &AppState,
    session_id: &str,
    filename: Option<&str>,
    bytes: &[u8],
) -> Option<String> {
    let dir = state
        .config
        .workspace_session_dir(session_id)
        .join("uploads");
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        tracing::warn!("[attachment] 创建会话 uploads 目录失败: {}", dir.display());
        return None;
    }
    let safe = sanitize_filename(filename.unwrap_or("文档"));
    let mut target = dir.join(&safe);
    if target.exists() {
        // 重名 → 扩展名前追加短 uuid（如 name.pptx → name_a1b2c3d4.pptx），避免静默覆盖
        let (stem, ext) = match safe.rsplit_once('.') {
            Some((s, e)) => (s, format!(".{e}")),
            None => (safe.as_str(), String::new()),
        };
        let uid = uuid::Uuid::now_v7().simple().to_string();
        let short = &uid[..8.min(uid.len())];
        target = dir.join(format!("{stem}_{short}{ext}"));
    }
    match tokio::fs::write(&target, bytes).await {
        Ok(_) => {
            // 规范化为绝对路径：data_dir 可能是相对路径（./data），agent/本地工具跨 cwd 取用更稳妥
            match tokio::fs::canonicalize(&target).await {
                Ok(abs) => {
                    let abs = abs.to_string_lossy().to_string();
                    tracing::info!("[attachment] 上传文档原文已落盘: {}", abs);
                    Some(abs)
                }
                Err(e) => {
                    // 文件已写入但无法规范化（极少见）——退回 target 展示路径，仍可用
                    tracing::warn!("[attachment] 规范化路径失败，使用原始路径: {e}");
                    Some(target.to_string_lossy().to_string())
                }
            }
        }
        Err(e) => {
            tracing::warn!("[attachment] 上传文档落盘失败: {}", e);
            None
        }
    }
}

/// 构造注入对话的「文档已上传 + 原文本地路径」提示文本。
///
/// - `parsed=true`：解析成功，引导语后由调用方追加 `<document>` 文本块；
/// - `parsed=false`：解析失败但原文已落盘（若有），告知 agent 原文路径可直接作附件发送。
///
/// 路径用自然语言给出（而非仅 XML 属性），确保模型读到并知道可取用。
fn path_hint(name: &str, path: Option<&str>, parsed: bool) -> String {
    match (path, parsed) {
        (Some(p), true) => format!(
            "\n\n[已上传文档 {name}，原始文件已保存到本地 {p}（可直接作为附件发送/转发）；以下为解析出的文本内容：]"
        ),
        (None, true) => format!("\n\n[已上传文档 {name}；以下为解析出的文本内容：]"),
        (Some(p), false) => format!(
            "\n\n[已上传文档 {name}，原始文件已保存到本地 {p}（可直接作为附件发送/转发），但文档解析服务暂不可用，无法提取文本内容。]"
        ),
        (None, false) => format!("\n\n[已上传文档 {name}，但文档解析服务暂不可用，无法提取内容。]"),
    }
}

/// 把图片字节内联进 content。超过 adk-rust 内联硬上限（10MB，超出 panic）则降级为文本提示，
/// 绝不让超大字节走到 `with_inline_data`。
fn inline_image(mut content: adk_rust::Content, mime: &str, data: Vec<u8>) -> adk_rust::Content {
    if data.len() <= MAX_INLINE_IMAGE_BYTES {
        content = content.with_inline_data(mime, data);
    } else {
        tracing::warn!(
            "[attachment] 图片 {} 字节超内联上限 {}，降级为文本提示",
            data.len(),
            MAX_INLINE_IMAGE_BYTES
        );
        content = content.with_text("\n\n[图片过大，未内联]");
    }
    content
}

/// 把 markitdown 输出截断到 `MAX_MARKDOWN_CHARS` 字符（超出加截断标记），防巨型文档撑爆上下文。
fn truncate_markdown(md: String) -> String {
    if md.chars().count() <= MAX_MARKDOWN_CHARS {
        return md;
    }
    tracing::warn!(
        "[attachment] markitdown 输出超 {} 字符，已截断",
        MAX_MARKDOWN_CHARS
    );
    let head: String = md.chars().take(MAX_MARKDOWN_CHARS).collect();
    format!("{head}\n\n[…文档内容过长，已截断…]")
}

/// 构造提交给 Runner 的 user content：user_text + 多模态附件注入。
///
/// - 图片附件：取字节 → resize → 内联；
/// - 文档附件：取字节 → markitdown 解析为文本注入，**并把原始字节落盘到会话工作区**
///   （`session_id` 决定落盘目录），使 agent 能拿到原文件作附件发送/转发。
///
/// 异步：文档附件需调用 markitdown MCP 做解析 + 落盘 IO。调用点（stream.rs）
/// 已在 `tokio::spawn` 异步任务内，直接 `.await`。
pub(crate) async fn build_user_content(
    user_text: &str,
    messages: &[InputMessage],
    state: &AppState,
    session_id: &str,
) -> adk_rust::Content {
    let mut content = adk_rust::Content::new("user").with_text(user_text);
    let Some(last_user_msg) = messages.iter().rev().find(|m| m.role == "user") else {
        return content;
    };
    for att in &last_user_msg.attachments {
        let is_image = att.mime_type.starts_with("image/");
        // 1) 本地图片 data URL：解码 → resize → 内联（仅图片；文档 data URL 不在此内联）
        if is_image && let Some(data) = decode_data_url(&att.url) {
            let (data, mime) = resize_image_if_needed(data, &att.mime_type);
            content = inline_image(content, &mime, data);
            continue;
        }
        // 2) 对象存储 http(s) URL：后端取字节再处理（云端 LLM 拉不到内网 URL）
        if att.url.starts_with("http://") || att.url.starts_with("https://") {
            if is_document_attachment(att) {
                let name = sanitize_filename(att.filename.as_deref().unwrap_or("文档"));
                match fetch_attachment_bytes(state, att).await {
                    Some(bytes) => {
                        // 先落盘原始文件（不依赖解析是否成功）：agent 可凭此路径直接作附件发送/转发。
                        // 用 build_user_content 入参 session_id 定位会话工作区。
                        let path = persist_uploaded_document(
                            state,
                            session_id,
                            att.filename.as_deref(),
                            &bytes,
                        )
                        .await;
                        match convert_document_to_markdown(state, bytes, &att.mime_type).await {
                            Some(md) => {
                                let md = truncate_markdown(md);
                                let header = path_hint(&name, path.as_deref(), true);
                                content = content.with_text(format!(
                                    "{header}\n<document filename=\"{name}\">\n{md}\n</document>"
                                ));
                            }
                            None => {
                                // 字节已取回且已落盘（若有），但解析失败：提示模型原文路径仍可取用
                                content =
                                    content.with_text(path_hint(&name, path.as_deref(), false));
                            }
                        }
                    }
                    None => {
                        tracing::warn!("[attachment] 文档字节获取失败，跳过解析: {}", att.url);
                        content = content
                            .with_text(format!("\n\n[已上传文档 {name}，但无法读取文件内容。]"));
                    }
                }
            } else if is_image {
                // 图片（http URL）：取字节 → resize → 内联（云端 LLM 拉不到内网 URL，故内联）
                match fetch_attachment_bytes(state, att).await {
                    Some(bytes) => {
                        let (data, mime) = resize_image_if_needed(bytes, &att.mime_type);
                        content = inline_image(content, &mime, data);
                    }
                    None => {
                        // 取不到字节：退回 file_uri（本地 / 可达 LLM 仍可用，不阻塞流程）
                        content = content.with_file_uri(&att.mime_type, &att.url);
                    }
                }
            } else {
                // 非图片非文档（音视频 / 未知二进制）：不内联（模型用不上，且可能超内联上限 panic）
                tracing::warn!(
                    "[attachment] 不支持的附件类型 mime={}，跳过: {}",
                    att.mime_type,
                    att.url
                );
            }
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::sanitize_filename;

    #[test]
    fn sanitize_keeps_normal_name_and_extension() {
        assert_eq!(sanitize_filename("任务完成通知.pptx"), "任务完成通知.pptx");
        assert_eq!(sanitize_filename("report_2024.xlsx"), "report_2024.xlsx");
    }

    #[test]
    fn sanitize_strips_path_separators() {
        // 防目录穿越：/ 与 \ 必须去掉，否则落盘 join 后越界
        // "../evil.pptx" → 去掉 / 得 "..evil.pptx" → 折叠 .. 得 ".evil.pptx"
        assert_eq!(sanitize_filename("../evil.pptx"), ".evil.pptx");
        assert_eq!(sanitize_filename("a/b/c.pptx"), "abc.pptx");
        assert_eq!(sanitize_filename("a\\b.pptx"), "ab.pptx");
    }

    #[test]
    fn sanitize_collapses_dotdot() {
        // 连续点折叠为单点：保留扩展名单点，消除 `..` 穿越/保留段
        assert_eq!(sanitize_filename("name..pptx"), "name.pptx");
        assert_eq!(sanitize_filename("foo....bar"), "foo.bar");
    }

    #[test]
    fn sanitize_strips_xml_dangerous_chars() {
        // 这些字符会破坏 <document filename="..."> 包装或被用于提示注入
        let s = sanitize_filename(r#"a"<>&b.docx"#);
        assert_eq!(s, "ab.docx");
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let s = sanitize_filename("a\tb\nc\r.pptx");
        assert_eq!(s, "abc.pptx");
    }

    #[test]
    fn sanitize_falls_back_when_empty() {
        assert_eq!(sanitize_filename(""), "文档");
        assert_eq!(sanitize_filename("   "), "文档");
        // 只剩被剥字符 → 也回退
        assert_eq!(sanitize_filename("///"), "文档");
    }

    #[test]
    fn sanitize_truncates_overlong() {
        // 截断按字符从左到右（与原 display_name 语义一致），上限 120
        let long = "a".repeat(200);
        let s = sanitize_filename(&long);
        assert!(s.chars().count() <= 120);
        assert!(s.chars().all(|c| c == 'a'));
    }
}
