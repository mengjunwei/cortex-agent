//! boot 期密钥轮换 re-encrypt：把历史密钥加密的密文 re-wrap 到活动密钥。
//!
//! 仅在 [`crate::security::APP_SECRETS`] 含多个密钥时实质生效（单密钥时所有密文已是活动密钥
//! 加密，`decrypt_active` 全部命中 → 整轮 no-op，提前返回省一轮 DB 扫描）。
//!
//! **幂等 + 不丢数据**核心策略——对每条密文：
//! 1. 活动密钥可解（`decrypt_active` 成功）→ 已是最新，跳过，不写库；
//! 2. 历史密钥可解（`decrypt` 遍历成功）→ 用活动密钥重加密并 UPDATE；
//! 3. 所有密钥都无法解 → 告警并**保留原值**（绝不写空覆盖，防丢数据）。
//!
//! ⚠️ **直接用 SQL 读原始密文列**，不走各 store 的 `list_all()`：后者 fail-safe 会把解密失败
//! 的密文吞成空值/默认值（如 assistant 的 `decrypt_env_vars` 失败返回空 map、kb 的
//! `decrypt_secret_fields` 用 `unwrap_or_default()`），重加密会丢数据。直接读密文列才能区分
//! 「已最新」「历史密钥可解」「损坏」三种状态。

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::infra::db::{DbPool, DbPooledConnection};
use crate::security::crypto::AesCodec;

/// 把所有历史密文 re-wrap 到活动密钥（幂等）。boot 期各 store 初始化后调用一次。
///
/// 单密钥配置下整轮 no-op（提前返回）；多密钥时扫描 4 类加密字段。
pub async fn reencrypt_all(pool: &DbPool) -> anyhow::Result<()> {
    let codec = AesCodec::from_secrets();
    // 单密钥：所有密文必为活动密钥加密，整轮必然 no-op，直接跳过省 DB 扫描
    if codec.is_single_key() {
        return Ok(());
    }

    tracing::info!(
        "[security] 检测到多密钥配置（{} 个），启动 re-encrypt 扫描",
        crate::security::APP_SECRETS.len()
    );

    let mut conn = pool
        .get()
        .await
        .map_err(|e| anyhow::anyhow!("re-encrypt 获取数据库连接失败: {e}"))?;

    let mut total = 0usize;
    total += reencrypt_llm_providers(&codec, &mut conn).await?;
    total += reencrypt_mcp_servers(&codec, &mut conn).await?;
    total += reencrypt_kb_instances(&codec, &mut conn).await?;
    total += reencrypt_assistants(&codec, &mut conn).await?;

    if total > 0 {
        tracing::info!("[security] re-encrypt 完成，共更新 {total} 条历史密文到活动密钥");
    } else {
        tracing::info!("[security] re-encrypt 扫描完成，无历史密文需更新");
    }
    Ok(())
}

/// 重加密单条密文（纯逻辑，无 DB 副作用）。
///
/// 返回 `Ok(Some((new_ct, plaintext)))` 表示需 UPDATE（`plaintext` 供调用方重算派生字段，
/// 如 llm_providers 的 `key_suffix`）；`Ok(None)` 表示已是活动密钥加密或空值，可跳过；
/// `Err(_)` 表示所有密钥均无法解密（调用方应告警并保留原值）。
fn rewrap(codec: &AesCodec, ct: &str) -> anyhow::Result<Option<(String, String)>> {
    if ct.trim().is_empty() {
        return Ok(None);
    }
    // 已是活动密钥加密 → 跳过（幂等关键：已迁移过的不再动）
    if codec.decrypt_active(ct).is_ok() {
        return Ok(None);
    }
    // 历史密钥可解 → 用活动密钥重加密
    let plain = codec.decrypt(ct)?;
    let new_ct = codec.encrypt(&plain)?;
    Ok(Some((new_ct, plain)))
}

/// llm_providers.encrypted_key（模型供应商 API Key）。重加密后同步重算 key_suffix。
async fn reencrypt_llm_providers(
    codec: &AesCodec,
    conn: &mut DbPooledConnection,
) -> anyhow::Result<usize> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = sql_types::Text)]
        id: String,
        #[diesel(sql_type = sql_types::Text)]
        encrypted_key: String,
    }

    let rows: Vec<Row> =
        diesel::sql_query("SELECT id, encrypted_key FROM llm_providers WHERE encrypted_key <> ''")
            .get_results::<Row>(conn)
            .await
            .map_err(|e| anyhow::anyhow!("re-encrypt 读取 llm_providers 失败: {e}"))?;

    let mut updated = 0usize;
    for r in rows {
        match rewrap(codec, &r.encrypted_key) {
            Ok(None) => {}
            Ok(Some((new_ct, plain))) => {
                let suffix = crate::domain::model_provider::store::ModelProviderStore::key_suffix(&plain);
                diesel::sql_query(
                    r#"UPDATE llm_providers
                       SET encrypted_key = $2, key_suffix = $3, updated_at = NOW()
                       WHERE id = $1"#,
                )
                .bind::<sql_types::Text, _>(&r.id)
                .bind::<sql_types::Text, _>(&new_ct)
                .bind::<sql_types::Text, _>(&suffix)
                .execute(conn)
                .await
                .map_err(|e| anyhow::anyhow!("re-encrypt 更新 llm_providers {} 失败: {e}", r.id))?;
                updated += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "[security] llm_providers {} 的 encrypted_key 无法被任何密钥解密，保留原值（请人工核查）: {e}",
                    r.id
                );
            }
        }
    }
    Ok(updated)
}

/// mcp_servers.env_enc + headers_enc（MCP 环境变量 / 请求头凭据）。
/// env_mask / headers_mask 是脱敏明文，不参与重加密。
async fn reencrypt_mcp_servers(
    codec: &AesCodec,
    conn: &mut DbPooledConnection,
) -> anyhow::Result<usize> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = sql_types::Text)]
        id: String,
        #[diesel(sql_type = sql_types::Text)]
        env_enc: String,
        #[diesel(sql_type = sql_types::Text)]
        headers_enc: String,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT id, env_enc, headers_enc FROM mcp_servers \
         WHERE env_enc <> '' OR headers_enc <> ''",
    )
    .get_results::<Row>(conn)
    .await
    .map_err(|e| anyhow::anyhow!("re-encrypt 读取 mcp_servers 失败: {e}"))?;

    let mut updated = 0usize;
    for r in rows {
        let mut touched = false;

        match rewrap(codec, &r.env_enc) {
            Ok(Some((new_ct, _))) => {
                diesel::sql_query(
                    "UPDATE mcp_servers SET env_enc = $2, updated_at = NOW() WHERE id = $1",
                )
                .bind::<sql_types::Text, _>(&r.id)
                .bind::<sql_types::Text, _>(&new_ct)
                .execute(conn)
                .await
                .map_err(|e| anyhow::anyhow!("re-encrypt 更新 mcp {} env 失败: {e}", r.id))?;
                touched = true;
            }
            Err(e) => {
                tracing::warn!(
                    "[security] mcp {} 的 env_enc 无法被任何密钥解密，保留原值（请人工核查）: {e}",
                    r.id
                );
            }
            _ => {}
        }

        match rewrap(codec, &r.headers_enc) {
            Ok(Some((new_ct, _))) => {
                diesel::sql_query(
                    "UPDATE mcp_servers SET headers_enc = $2, updated_at = NOW() WHERE id = $1",
                )
                .bind::<sql_types::Text, _>(&r.id)
                .bind::<sql_types::Text, _>(&new_ct)
                .execute(conn)
                .await
                .map_err(|e| anyhow::anyhow!("re-encrypt 更新 mcp {} headers 失败: {e}", r.id))?;
                touched = true;
            }
            Err(e) => {
                tracing::warn!(
                    "[security] mcp {} 的 headers_enc 无法被任何密钥解密，保留原值（请人工核查）: {e}",
                    r.id
                );
            }
            _ => {}
        }

        if touched {
            updated += 1;
        }
    }
    Ok(updated)
}

/// kb_instances.config（仅 Dify provider_kind=1 的 config 含加密 secret `api_key`）。
/// Builtin（provider_kind=2）config 无 secret，跳过。
async fn reencrypt_kb_instances(
    codec: &AesCodec,
    conn: &mut DbPooledConnection,
) -> anyhow::Result<usize> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = sql_types::Text)]
        id: String,
        #[diesel(sql_type = sql_types::Text)]
        config: String,
    }

    // provider_kind = 1 → Dify
    let rows: Vec<Row> = diesel::sql_query(
        "SELECT id, config FROM kb_instances WHERE provider_kind = 1 AND config <> ''",
    )
    .get_results::<Row>(conn)
    .await
    .map_err(|e| anyhow::anyhow!("re-encrypt 读取 kb_instances 失败: {e}"))?;

    let mut updated = 0usize;
    for r in rows {
        let mut cfg: serde_json::Value = match serde_json::from_str(&r.config) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "[security] kb {} 的 config 不是合法 JSON，跳过（请人工核查）: {e}",
                    r.id
                );
                continue;
            }
        };

        // 取出 api_key 密文（克隆，避免与后续 cfg 可变借用冲突）
        let api_key_ct = cfg
            .get("api_key")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_default();
        if api_key_ct.is_empty() {
            continue;
        }

        match rewrap(codec, &api_key_ct) {
            Ok(Some((new_ct, _))) => {
                if let Some(obj) = cfg.as_object_mut() {
                    obj.insert("api_key".into(), serde_json::Value::String(new_ct));
                }
                let new_config = serde_json::to_string(&cfg)
                    .map_err(|e| anyhow::anyhow!("kb {} 重序列化 config 失败: {e}", r.id))?;
                diesel::sql_query(
                    "UPDATE kb_instances SET config = $2, updated_at = NOW() WHERE id = $1",
                )
                .bind::<sql_types::Text, _>(&r.id)
                .bind::<sql_types::Text, _>(&new_config)
                .execute(conn)
                .await
                .map_err(|e| anyhow::anyhow!("re-encrypt 更新 kb {} 失败: {e}", r.id))?;
                updated += 1;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    "[security] kb {} 的 api_key 无法被任何密钥解密，保留原值（请人工核查）: {e}",
                    r.id
                );
            }
        }
    }
    Ok(updated)
}

/// assistants.env_vars（助手环境变量，整体 JSON 加密）。
async fn reencrypt_assistants(
    codec: &AesCodec,
    conn: &mut DbPooledConnection,
) -> anyhow::Result<usize> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = sql_types::Text)]
        id: String,
        #[diesel(sql_type = sql_types::Text)]
        env_vars: String,
    }

    let rows: Vec<Row> =
        diesel::sql_query("SELECT id, env_vars FROM assistants WHERE env_vars <> ''")
            .get_results::<Row>(conn)
            .await
            .map_err(|e| anyhow::anyhow!("re-encrypt 读取 assistants 失败: {e}"))?;

    let mut updated = 0usize;
    for r in rows {
        match rewrap(codec, &r.env_vars) {
            Ok(Some((new_ct, _))) => {
                diesel::sql_query(
                    "UPDATE assistants SET env_vars = $2, updated_at = NOW() WHERE id = $1",
                )
                .bind::<sql_types::Text, _>(&r.id)
                .bind::<sql_types::Text, _>(&new_ct)
                .execute(conn)
                .await
                .map_err(|e| anyhow::anyhow!("re-encrypt 更新 assistant {} 失败: {e}", r.id))?;
                updated += 1;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    "[security] assistant {} 的 env_vars 无法被任何密钥解密，保留原值（请人工核查）: {e}",
                    r.id
                );
            }
        }
    }
    Ok(updated)
}

// ===========================================================================
//  单元测试（rewrap 纯逻辑：跳过空 / 跳过活动密钥 / 重加密历史密钥 / 损坏报错）
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = "legacy-32-byte-secret-key-for-test!!!";
    const ACTIVE: &str = "active-32-byte-secret-key-for-test!!!";

    #[test]
    fn rewrap_empty_returns_none() {
        let codec = AesCodec::for_rotation_test(LEGACY, ACTIVE);
        assert!(rewrap(&codec, "").unwrap().is_none());
        assert!(rewrap(&codec, "   ").unwrap().is_none());
    }

    #[test]
    fn rewrap_active_ciphertext_skipped() {
        // 活动密钥（末项 ACTIVE）加密的密文 → 已是最新，跳过
        let codec = AesCodec::for_rotation_test(LEGACY, ACTIVE);
        let enc = codec.encrypt("fresh").unwrap();
        assert!(rewrap(&codec, &enc).unwrap().is_none());
    }

    #[test]
    fn rewrap_legacy_ciphertext_reencrypted_to_active() {
        // 旧密钥（LEGACY）加密的密文
        let legacy_enc = AesCodec::from_passphrase(LEGACY)
            .encrypt("legacy-data")
            .unwrap();
        let codec = AesCodec::for_rotation_test(LEGACY, ACTIVE);

        let (new_ct, plain) = rewrap(&codec, &legacy_enc).unwrap().expect("应重加密");
        assert_eq!(plain, "legacy-data");
        // 重加密后的密文用活动密钥能解（decrypt_active 成功）
        assert!(codec.decrypt_active(&new_ct).is_ok());
        // 且旧密钥单独解不开了（证明确实换成了活动密钥）
        assert!(AesCodec::from_passphrase(LEGACY).decrypt(&new_ct).is_err());
    }

    #[test]
    fn rewrap_is_idempotent() {
        let legacy_enc = AesCodec::from_passphrase(LEGACY).encrypt("x").unwrap();
        let codec = AesCodec::for_rotation_test(LEGACY, ACTIVE);
        // 第一次：重加密
        let (new_ct, _) = rewrap(&codec, &legacy_enc).unwrap().unwrap();
        // 第二次：已是活动密钥 → None（幂等）
        assert!(rewrap(&codec, &new_ct).unwrap().is_none());
    }

    #[test]
    fn rewrap_undecryptable_returns_err() {
        // codec 只认 [LEGACY, ACTIVE]，用第三把密钥加密的密文 → 都解不开 → Err
        let other_enc = AesCodec::from_passphrase("third-32-byte-secret-key-unknown!!")
            .encrypt("x")
            .unwrap();
        let codec = AesCodec::for_rotation_test(LEGACY, ACTIVE);
        assert!(rewrap(&codec, &other_enc).is_err());
    }

    #[test]
    fn rewrap_garbage_returns_err() {
        let codec = AesCodec::for_rotation_test(LEGACY, ACTIVE);
        assert!(rewrap(&codec, "not-valid-base64-!!!").is_err());
        assert!(rewrap(&codec, "dG9v").is_err()); // base64 解码后不足 12 字节
    }
}
