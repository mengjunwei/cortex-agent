//! 截图保存辅助（SSE 层兜底）— 从工具返回结果中提取 base64 / saved_path 并落盘。
//!
//! 主路径已在工具层（[`crate::tools::screenshot`]）「截断前存盘」完成；本模块仅作
//! SSE 事件流（`FunctionResponse`）的兜底，覆盖未走 `TruncatingToolset` 的截图来源。
//! base64 提取复用 [`crate::tools::screenshot::extract_base64_from_value`] 权威实现，
//! 避免两处逻辑漂移；多模态附件注入复用 [`decode_data_url`]。

use serde_json::Value;

// 复用工具层的 base64 提取（权威实现），本模块只保留 SSE 层特有的兜底落盘逻辑
use crate::tools::screenshot::extract_base64_from_value;

/// 解码 `data:<mime>;base64,<payload>` 形式的 data URL，返回原始字节。
/// 非 data URL 返回 None。
pub(super) fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let payload = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split(",").nth(1))?;
    if payload.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()
}

/// 从工具返回的 JSON Value 中提取 saved_path（Agent 指定了保存路径）
pub(super) fn extract_saved_path(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(path)) = map.get("saved_path") {
                return Some(path.clone());
            }
            for val in map.values() {
                if let Some(path) = extract_saved_path(val) {
                    return Some(path);
                }
            }
            None
        }
        _ => None,
    }
}

/// 生成截图文件名：`{run_id}_{call_id}.png`
pub(super) fn make_screenshot_filename(run_id: &str, call_id: &str) -> String {
    format!(
        "{}_{}.png",
        run_id.replace("-", ""),
        call_id.replace("-", "")
    )
}

/// 若检测到截图结果（base64 或已上传的 object key），上传/复用到该会话截图前缀，返回文件名
///
/// `object_store` 为对象存储客户端;对象 key 为 `screenshots/{session_id}/{filename}`。
/// 返回的 filename 不含 session_id（调用方拼 image_url 时带上 session_id）。
pub(super) async fn save_screenshot_if_needed(
    object_store: &crate::infra::object_store::ObjectStore,
    session_id: &str,
    resp: &Value,
    run_id: &str,
    call_id: &str,
) -> Option<String> {
    use base64::Engine as _;
    use bytes::Bytes;

    // 诊断：打印 resp 结构（不打印 base64 内容，避免日志爆炸）
    let resp_keys: Vec<&str> = resp
        .as_object()
        .map(|m| m.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();
    tracing::info!(
        "[screenshot] save_screenshot_if_needed 进入: session_id={session_id} resp_keys={resp_keys:?} resp_type={}",
        if resp.is_object() { "object" } else if resp.is_array() { "array" } else { "other" }
    );

    if !crate::config::is_safe_path_segment(session_id) {
        tracing::warn!("[screenshot] 拒绝不安全 session_id: {session_id}");
        return None;
    }

    let filename = make_screenshot_filename(run_id, call_id);

    // 情况1：结果含 saved_path 且已是对象 key（工具层 TruncatingTool 已上传 screenshots/{sid}/{file}）
    // → 直接复用其 filename，不再重复上传。
    if let Some(prev_key) = extract_saved_path(resp) {
        if prev_key.starts_with("screenshots/") {
            if let Some(fname) = prev_key.rsplit('/').next() {
                tracing::info!("[screenshot] saved_path 已是对象 key，复用: {prev_key}");
                return Some(fname.to_string());
            }
        }
    }

    // 情况2：结果含 base64 → 上传对象存储
    let b64_opt = extract_base64_from_value(resp);
    tracing::info!("[screenshot] extract_base64 结果: {}", if b64_opt.is_some() { "Some" } else { "None" });
    if let Some(b64) = b64_opt {
        let key = format!("screenshots/{session_id}/{filename}");
        match base64::engine::general_purpose::STANDARD.decode(&b64) {
            Ok(decoded) => match object_store.put(&key, Bytes::from(decoded)).await {
                Ok(_) => {
                    tracing::info!("[screenshot] 已上传 base64 截图: {key}");
                    return Some(filename);
                }
                Err(e) => {
                    tracing::warn!("[screenshot] 上传截图失败 key={key}: {e}");
                }
            },
            Err(e) => {
                tracing::warn!("[screenshot] base64 解码失败: {e}");
            }
        }
    }

    None
}
