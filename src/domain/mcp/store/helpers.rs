//! McpServerStore 的纯函数助手 — 从 store.rs 拆出。
//!
//! 三组职责:
//! - 凭据加解密:`prepare_secret` / `merge_secret_map` / `encrypt_map` / `decrypt_map` / `strict_decrypt_map`
//! - 入参校验:`validate_name` / `validate_args` / `validate_endpoint`
//! - DB 维护:`purge_mcp_from_assistants`;`random_suffix` 供 slug 生成

use std::collections::HashMap;

use diesel::sql_types;
use diesel_async::RunQueryDsl;
use uuid::Uuid;

use crate::domain::mcp::enums::TransportKind;
use crate::domain::mcp::models::mask_map;
use crate::error::AppError;
use crate::infra::db::DbPooledConnection;
use crate::security::crypto::AesCodec;

// ========== 工具函数 ==========

/// 加密 map 并生成脱敏 JSON 字符串，返回 (enc, mask)
pub(super) fn prepare_secret(
    codec: &AesCodec,
    map: &HashMap<String, String>,
) -> Result<(String, String), AppError> {
    let enc = encrypt_map(codec, map)?;
    let mask = serde_json::to_string(&mask_map(map))?;
    Ok((enc, mask))
}

/// 按键合并 update 输入与已存明文：以 input 的键集为最终键集（WYSIWYG）。
/// - `None`：保留该键已存值（前端回显掩码、值留空即传 null）
/// - `Some(非空)`：覆盖/新增
/// - `Some(空串)`：删除该键（env 无空值语义）
/// - 键缺席：删除该键
pub(super) fn merge_secret_map(
    existing: &HashMap<String, String>,
    input: &HashMap<String, Option<String>>,
) -> HashMap<String, String> {
    input
        .iter()
        .filter_map(|(k, v)| match v {
            None => existing.get(k).map(|old| (k.clone(), old.clone())),
            Some(v) if v.trim().is_empty() => None,
            Some(v) => Some((k.clone(), v.clone())),
        })
        .collect()
}

pub(super) fn validate_name(name: &str) -> Result<(), AppError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(AppError::BusinessError("MCP Server 名称不能为空".into()));
    }
    if n.chars().count() > 128 {
        return Err(AppError::BusinessError(
            "MCP Server 名称不能超过 128 字符".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_args(args: &[String]) -> Result<(), AppError> {
    if args.iter().any(|a| a.chars().count() > 4096) {
        return Err(AppError::BusinessError("单个启动参数过长".into()));
    }
    Ok(())
}

/// 校验 endpoint：stdio 必须非空命令名；http 必须是 http(s) URL
pub(crate) fn validate_endpoint(transport: &TransportKind, endpoint: &str) -> Result<(), AppError> {
    let e = endpoint.trim();
    if e.is_empty() {
        return Err(AppError::BusinessError("endpoint 不能为空".into()));
    }
    if e.chars().count() > 1024 {
        return Err(AppError::BusinessError("endpoint 过长（>1024）".into()));
    }
    match transport {
        TransportKind::Stdio => {
            // 命令名：禁止 shell 元字符，避免注入（实际 args 用 Command::arg 逐个传）
            if e.contains(['|', ';', '&', '>', '<', '`', '$']) {
                return Err(AppError::BusinessError(
                    "stdio 命令包含非法 shell 元字符".into(),
                ));
            }
        }
        TransportKind::StreamableHttp => {
            if !(e.starts_with("https://") || e.starts_with("http://")) {
                return Err(AppError::BusinessError(
                    "http 传输的 endpoint 必须以 http:// 或 https:// 开头".into(),
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn encrypt_map(codec: &AesCodec, map: &HashMap<String, String>) -> Result<String, AppError> {
    if map.is_empty() {
        return Ok(String::new());
    }
    let json = serde_json::to_string(map)?;
    let enc = codec
        .encrypt(&json)
        .map_err(|e| AppError::BusinessError(format!("MCP 凭据加密失败: {e}")))?;
    Ok(enc)
}

pub(super) fn decrypt_map(codec: &AesCodec, enc: &str) -> Result<HashMap<String, String>, AppError> {
    if enc.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let plain = codec
        .decrypt(enc)
        .map_err(|e| AppError::BusinessError(format!("MCP 凭据解密失败: {e}")))?;
    let map: HashMap<String, String> = serde_json::from_str(&plain)?;
    Ok(map)
}

/// update 合并路径专用：解密失败显式报错（密钥变更等场景），绝不静默返回空 map——
/// 空 map 会成为合并基数，等价于把既有密钥全部清空。
pub(super) fn strict_decrypt_map(
    codec: &AesCodec,
    enc: &str,
    field: &str,
) -> Result<HashMap<String, String>, AppError> {
    decrypt_map(codec, enc).map_err(|_| {
        AppError::BusinessError(format!(
            "MCP {field} 凭据解密失败（加密密钥可能已变更）：为防止覆盖丢密钥已拒绝保存，请重新配置"
        ))
    })
}

/// 从所有 assistant.enabled_mcps（JSON 数组）中移除指定 mcp_id 引用。
/// 使用 PostgreSQL jsonb 路径表达式避免拉取全表到内存。
pub(crate) async fn purge_mcp_from_assistants(
    conn: &mut DbPooledConnection,
    mcp_id: &str,
) -> Result<(), AppError> {
    // enabled_mcps 存 TEXT(json 数组)；用 jsonb 转换过滤后写回。
    // 仅更新包含该 id 的行。
    diesel::sql_query(
        r#"UPDATE assistants SET enabled_mcps = (
                SELECT COALESCE(jsonb_agg(elem)::text, '[]')
                FROM jsonb_array_elements(enabled_mcps::jsonb) AS elem
                WHERE elem::text <> to_jsonb($1::text)::text
           )
           WHERE enabled_mcps::jsonb @> to_jsonb(ARRAY[$1::text])"#,
    )
    .bind::<sql_types::Text, _>(mcp_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// 生成给定长度的随机后缀（小写字母+数字）
pub(super) fn random_suffix(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut state: u64 = {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        let uuid_rand = (Uuid::now_v7().as_u128() as u64).wrapping_mul(0xFF51AFD7ED558CCD);
        t ^ uuid_rand
    };
    if state == 0 {
        state = 0x9E3779B97F4A7C15;
    }
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        s.push(ALPHABET[(state % ALPHABET.len() as u64) as usize] as char);
    }
    s
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::super::McpServerStore;
    use crate::domain::mcp::enums::Status;
    use crate::domain::mcp::models::{McpServer, ServerHealth};

    #[test]
    fn merge_keeps_existing_on_none() {
        let mut existing = HashMap::new();
        existing.insert("TOKEN".into(), "sk-old".into());
        let mut input = HashMap::new();
        input.insert("TOKEN".into(), None);
        let merged = merge_secret_map(&existing, &input);
        assert_eq!(merged.get("TOKEN").unwrap(), "sk-old");
    }

    #[test]
    fn merge_overrides_and_adds_on_value() {
        let mut existing = HashMap::new();
        existing.insert("A".into(), "old-a".into());
        existing.insert("B".into(), "old-b".into());
        let mut input = HashMap::new();
        input.insert("A".into(), Some("new-a".into()));
        input.insert("C".into(), Some("c".into()));
        let merged = merge_secret_map(&existing, &input);
        assert_eq!(merged.get("A").unwrap(), "new-a");
        assert_eq!(merged.get("C").unwrap(), "c");
        // 键缺席 = 删除
        assert!(!merged.contains_key("B"));
    }

    #[test]
    fn merge_deletes_on_absent_or_empty() {
        let mut existing = HashMap::new();
        existing.insert("A".into(), "old-a".into());
        existing.insert("B".into(), "old-b".into());
        let mut input = HashMap::new();
        input.insert("B".into(), Some("  ".into()));
        let merged = merge_secret_map(&existing, &input);
        assert!(!merged.contains_key("A"));
        assert!(!merged.contains_key("B"));
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_none_keep_for_unknown_key_is_noop() {
        // 前端新增行忘填值：键不在 existing 中，None 保留无值可取 → 不产生条目
        let existing = HashMap::new();
        let mut input = HashMap::new();
        input.insert("NEW".into(), None);
        let merged = merge_secret_map(&existing, &input);
        assert!(!merged.contains_key("NEW"));
    }

    #[test]
    fn strict_decrypt_empty_enc_yields_empty_map() {
        let codec = AesCodec::from_passphrase("test-key-fixed-value-32b!!!");
        let m = strict_decrypt_map(&codec, "", "env").unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn strict_decrypt_failure_is_explicit_error() {
        // 用 A 密钥加密、B 密钥解密 → 显式报错（绝不静默空 map 导致合并清空）
        let codec_a = AesCodec::from_passphrase("test-key-fixed-value-32b!!!");
        let codec_b = AesCodec::from_passphrase("another-key-fixed-value-32b!!!!");
        let mut m = HashMap::new();
        m.insert("TOKEN".into(), "sk-old".into());
        let enc = encrypt_map(&codec_a, &m).unwrap();
        let err = strict_decrypt_map(&codec_b, &enc, "env").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("env"), "报错应指明字段: {msg}");
        assert!(msg.contains("拒绝保存"), "报错应说明已拒绝: {msg}");
    }

    #[test]
    fn validate_endpoint_stdio_rejects_shell_metachars() {
        assert!(validate_endpoint(&TransportKind::Stdio, "npx -y foo").is_ok());
        assert!(validate_endpoint(&TransportKind::Stdio, "npx; rm -rf").is_err());
        assert!(validate_endpoint(&TransportKind::Stdio, "sh && cmd").is_err());
        assert!(validate_endpoint(&TransportKind::Stdio, "").is_err());
    }

    #[test]
    fn validate_endpoint_http_requires_scheme() {
        assert!(validate_endpoint(&TransportKind::StreamableHttp, "https://x.com/mcp").is_ok());
        assert!(
            validate_endpoint(&TransportKind::StreamableHttp, "http://localhost:8080/mcp").is_ok()
        );
        assert!(validate_endpoint(&TransportKind::StreamableHttp, "localhost:8080").is_err());
    }

    #[test]
    fn validate_name_checks_empty_and_length() {
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("GitHub").is_ok());
    }

    #[test]
    fn encrypt_decrypt_map_roundtrip() {
        let codec = AesCodec::from_passphrase("test-key-fixed-value-32b!!!");
        let mut m = HashMap::new();
        m.insert("TOKEN".into(), "sk-1234567890abcd".into());
        m.insert("ENV".into(), "prod".into());
        let enc = encrypt_map(&codec, &m).unwrap();
        assert!(!enc.is_empty());
        let dec = decrypt_map(&codec, &enc).unwrap();
        assert_eq!(dec.get("TOKEN").unwrap(), "sk-1234567890abcd");
        assert_eq!(dec.get("ENV").unwrap(), "prod");
    }

    #[test]
    fn encrypt_empty_map_yields_empty_string() {
        let codec = AesCodec::from_passphrase("test-key-fixed-value-32b!!!");
        let m = HashMap::new();
        let enc = encrypt_map(&codec, &m).unwrap();
        assert!(enc.is_empty());
        let dec = decrypt_map(&codec, "").unwrap();
        assert!(dec.is_empty());
    }

    #[test]
    fn random_suffix_is_lowercase_alnum() {
        let s = random_suffix(6);
        assert_eq!(s.len(), 6);
        for c in s.chars() {
            assert!(c.is_ascii_lowercase() || c.is_ascii_digit());
        }
    }

    #[test]
    fn to_response_masks_sensitive_values() {
        let mut env = HashMap::new();
        env.insert("KEY".into(), "sk-1234567890abcd".into());
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), "Bearer xyz12345".into());
        let server = McpServer {
            id: "id1".into(),
            name: "GitHub".into(),
            slug: "github_abcd".into(),
            transport: TransportKind::Stdio,
            endpoint: "npx".into(),
            args: vec![],
            env: env.clone(),
            headers: headers.clone(),
            status: Status::Enabled,
            tool_timeout_secs: 60,
            user_id: "user1".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let resp = McpServerStore::to_response(&server, ServerHealth::Unknown);
        assert_eq!(resp.env.get("KEY").unwrap(), "****abcd");
        assert_eq!(resp.headers.get("Authorization").unwrap(), "****2345");
        assert_eq!(resp.health, ServerHealth::Unknown);
        // slug 透传
        assert_eq!(resp.slug, "github_abcd");
        // 证明明文未进入响应
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("sk-1234567890abcd"));
        assert!(!json.contains("Bearer xyz12345"));
    }
}
