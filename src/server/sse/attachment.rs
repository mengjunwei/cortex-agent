//! 多模态输入附件处理：上传图片按长边降采样，并把用户文本 + 附件拼成提交给 Runner 的 Content。
//!
//! - `data:<mime>;base64,...` → InlineData（本地图片先按长边上限 resize 降采样再注入，省 token）
//! - `https://...`           → FileData（外链，交给上游 LLM 拉取）

use super::screenshot::decode_data_url;
use super::types::InputMessage;

/// 上传图片长边上限：Anthropic 1568px / OpenAI 2048px，取保守的 1568（两端都安全）。
/// 超过此尺寸的原图会被降采样，避免白白烧 token + 被 provider 静默降采样。
const UPLOAD_IMAGE_MAX_SIDE: u32 = 1568;

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

/// 构造提交给 Runner 的 user content：user_text + 多模态附件注入。
pub(super) fn build_user_content(user_text: &str, messages: &[InputMessage]) -> adk_rust::Content {
    let mut content = adk_rust::Content::new("user").with_text(user_text);
    if let Some(last_user_msg) = messages.iter().rev().find(|m| m.role == "user") {
        for att in &last_user_msg.attachments {
            if let Some(data) = decode_data_url(&att.url) {
                let (data, mime) = resize_image_if_needed(data, &att.mime_type);
                content = content.with_inline_data(&mime, data);
            } else if att.url.starts_with("http://") || att.url.starts_with("https://") {
                content = content.with_file_uri(&att.mime_type, &att.url);
            }
        }
    }
    content
}
