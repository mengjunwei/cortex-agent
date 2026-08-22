//! 截图结果处理 — 从工具返回中提取 base64 并落盘为 `image_url`。
//!
//! ## 为何在工具层（而非 SSE 层）做
//!
//! 截图 base64 通常远超 [`super::truncating`] 的输出阈值（默认 48KB）。若等
//! SSE 层（`FunctionResponse`）再存盘，base64 已被 `TruncatingTool` 硬截断并
//! 追加「`[... 内容过长已截断 ...]`」标记：JSON 结构从中间断裂、base64 残缺，
//! 既无法解码成图片、前端也无法识别。
//!
//! 因此本模块在 `TruncatingTool::execute` **截断之前**就把巨大的 base64 落盘、
//! 替换成很小的 `image_url`，一举两得：既不爆 LLM 上下文，前端又能直接显示。
//!
//! SSE 层（`crate::server::sse::screenshot`）保留同名兜底，覆盖未走
//! `TruncatingToolset` 的截图来源；其 `extract_base64_from_value` 直接复用
//! 本模块的权威实现。

use bytes::Bytes;
use serde_json::Value;

use crate::infra::object_store::ObjectStore;

/// 已知的截图 base64 字段名（小写匹配，兼容驼峰 / 下划线 / 各种 MCP 工具命名）
const BASE64_KEYS: [&str; 9] = [
    "data",
    "image",
    "base64",
    "base64data",
    "base64_data",
    "screenshot",
    "png",
    "result",
    "imagedata",
];

/// 判定字符串是否疑似 JSON（被 JSON 化后塞进字符串字段的结构）。
///
/// 浏览器 / MCP 截图工具常把完整结果对象序列化成字符串放进 `output` 字段，
/// 例如 `{"output":"{\"base64Data\":\"/9j/...\"}"}`，需要先 parse 再递归挖掘，
/// 否则该串会被 [`is_likely_base64`] 当作非 base64 丢弃。
fn looks_like_json(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with('{') || t.starts_with('[')
}

/// 判定字符串是否疑似 base64 图片数据（仅含 base64 字符集 + 足够长）。
pub fn is_likely_base64(s: &str) -> bool {
    s.len() > 100
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '+' || c == '/' || c == '=')
}

/// 从工具返回的 JSON Value 中递归提取 base64 图片数据。
///
/// 兼容三种真实形态：
/// 1. 对象的已知字段（`base64Data` / `data` / `image` …）直接持有 base64 串；
/// 2. 纯 base64 字符串（`/9j/…`、`iVBOR…`）；
/// 3. **被 JSON 化的字符串字段**（如 `output: "{\"base64Data\": …}"`）——
///    先 `from_str` 还原为对象再递归，否则其中的 `{ " :` 等字符会令
///    [`is_likely_base64`] 判定为非 base64。
pub fn extract_base64_from_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            // 先尝试把 JSON 字符串还原为结构化值再递归挖掘（见上方形态 3）
            if looks_like_json(s) {
                if let Ok(inner) = serde_json::from_str::<Value>(s) {
                    if let Some(found) = extract_base64_from_value(&inner) {
                        return Some(found);
                    }
                }
            }
            if is_likely_base64(s) {
                return Some(s.clone());
            }
            None
        }
        Value::Array(arr) => {
            for val in arr {
                if let Some(s) = extract_base64_from_value(val) {
                    return Some(s);
                }
            }
            None
        }
        Value::Object(map) => {
            // 优先按已知字段名（大小写不敏感）直接命中
            for (key, val) in map {
                if BASE64_KEYS.contains(&key.to_lowercase().as_str()) {
                    if let Value::String(s) = val {
                        if is_likely_base64(s) {
                            return Some(s.clone());
                        }
                    }
                    if let Some(found) = extract_base64_from_value(val) {
                        return Some(found);
                    }
                }
            }
            // 兜底：递归所有 value（覆盖未知字段名，最终命中底层纯 base64 串）
            for val in map.values() {
                if let Some(s) = extract_base64_from_value(val) {
                    return Some(s);
                }
            }
            None
        }
        _ => None,
    }
}

/// 按 base64 数据头（magic bytes）推断图片扩展名，避免 JPEG 存成 `.png`。
fn detect_ext(b64: &str) -> &'static str {
    if b64.starts_with("/9j/") {
        "jpg"
    } else if b64.starts_with("iVBOR") {
        "png"
    } else if b64.starts_with("R0lGOD") {
        "gif"
    } else if b64.starts_with("UklGR") {
        "webp"
    } else {
        "png"
    }
}

/// 把 base64 截图上传到对象存储 `screenshots/{session_id}/{uuid}.{ext}`，返回文件名。
///
/// 解码或上传失败时返回 `None`。
async fn save_screenshot_to_store(
    object_store: &ObjectStore,
    session_id: &str,
    b64: &str,
) -> Option<String> {
    if !crate::config::is_safe_path_segment(session_id) {
        tracing::warn!("[screenshot] 拒绝不安全 session_id: {session_id}");
        return None;
    }
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let filename = format!("{}.{}", uuid::Uuid::now_v7().simple(), detect_ext(b64));
    let key = format!("screenshots/{session_id}/{filename}");
    match object_store.put(&key, Bytes::from(decoded)).await {
        Ok(_) => {
            tracing::info!("[screenshot] 工具层已上传截图: {key}");
            Some(filename)
        }
        Err(e) => {
            tracing::warn!("[screenshot] 上传截图失败 key={key}: {e}");
            None
        }
    }
}

/// 处理截图工具返回：提取 base64 → 上传到对象存储 → 用 `image_url` 替换巨大的 base64。
///
/// 成功时返回精简后的 `{ image_url, saved_path, note }`，体积很小，不会触发
/// 后续 [`super::truncating`] 截断；提取 / 上传失败时原样返回 `resp`，交由
/// 截断层正常处理（防爆上下文优先于图片可显示性）。
///
/// 对象 key 为 `screenshots/{session_id}/{uuid}.ext`；`image_url` 为
/// `/api/screenshots/{session_id}/{file}`（按会话隔离，serve_screenshot 时校验会话归属，
/// 后端从对象存储代理读取）。`saved_path` 存 object key（可移植），不再存本地绝对路径。
pub async fn process_screenshot_response(
    object_store: &ObjectStore,
    session_id: &str,
    resp: Value,
) -> Value {
    let b64 = match extract_base64_from_value(&resp) {
        Some(s) => s,
        None => return resp,
    };
    match save_screenshot_to_store(object_store, session_id, &b64).await {
        Some(filename) => {
            let key = format!("screenshots/{session_id}/{filename}");
            serde_json::json!({
                "image_url": format!("/api/screenshots/{session_id}/{filename}"),
                "saved_path": key,
                "note": "截图已保存，可通过 image_url 查看"
            })
        }
        None => resp,
    }
}
