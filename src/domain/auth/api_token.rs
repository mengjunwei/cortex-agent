//! 账户 API Token（访问令牌）存储与工具。
//!
//! 用途：让外部系统/脚本以 `Authorization: Bearer <token>` 调接口，等价登录身份。
//!
//! 安全模型（与 GitHub / OpenAI PAT 一致）：
//! - 明文令牌 `cxat_<32字节随机>` 仅在创建时返回一次，之后永不可见。
//! - 库内只存 `token_hash = SHA-256(明文)`（不可逆），验证时对输入做同样哈希后
//!   按唯一索引 `uq_api_tokens_hash` 查找（O(1)）。
//! - 列表只展示脱敏前缀 `prefix`（明文前 12 字符），无法还原令牌。
//!
//! 遵循 architecture.md §8：VARCHAR(36) 主键、SMALLINT 枚举、TIMESTAMPTZ、
//! Store 复用 `infra::store_base`、DDL 在 `migrations/schema.sql`。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::{Store, is_unique_violation, new_id};

// ===== 令牌生成 / 哈希工具 =====

/// 令牌明文前缀（可辨识，便于在日志/列表中识别为 cortex API Token）
const TOKEN_PREFIX_TAG: &str = "cxat_";

/// 生成新的明文令牌：`cxat_` + base64url(32 随机字节)，约 49 字符，256 bit 熵。
pub(crate) fn generate_raw_token() -> String {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    use base64::Engine;
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!(
        "{TOKEN_PREFIX_TAG}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// 计算明文令牌的 SHA-256 哈希（64 位 hex），用于入库与验证比对。
pub(crate) fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(s.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// 取明文令牌的脱敏前缀（前 12 字符，形如 `cxat_aB3dXy`），供列表辨识。
pub(crate) fn token_prefix(raw: &str) -> String {
    raw.chars().take(12).collect()
}

// ===== DB 行结构（不含 token_hash：所有 SELECT 均不取该列，杜绝泄露） =====

#[derive(Debug, Clone, QueryableByName)]
pub struct ApiTokenRow {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub user_id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub name: String,
    #[diesel(sql_type = sql_types::Text)]
    pub remark: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub prefix: String,
    /// 启用状态：0=禁用 1=启用（与全站 status 语义一致）
    #[diesel(sql_type = sql_types::Int2)]
    pub enabled: i16,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Timestamptz>)]
    pub valid_from: Option<DateTime<Utc>>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Timestamptz>)]
    pub expires_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Timestamptz>)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: DateTime<Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: DateTime<Utc>,
}

impl ApiTokenRow {
    pub fn is_enabled(&self) -> bool {
        self.enabled == 1
    }
}

/// 所有 SELECT 共用的列清单（刻意排除 `token_hash`，避免任何读取路径泄露哈希）。
const TOKEN_COLS: &str = "id, user_id, name, remark, prefix, enabled, \
    valid_from, expires_at, last_used_at, created_at, updated_at";

// ===== Store =====

pub struct ApiTokenStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for ApiTokenStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl ApiTokenStore {
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        Ok(Arc::new(Self { pool }))
    }

    /// 创建令牌（调用方负责生成明文 + 哈希 + 前缀）。返回入库后的行（不含哈希）。
    ///
    /// `token_hash` 唯一冲突理论上不可能（256 bit 随机），但防御性映射为 `ConflictError`。
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        user_id: &str,
        name: &str,
        remark: &str,
        token_hash: &str,
        prefix: &str,
        valid_from: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<ApiTokenRow, AppError> {
        let id = new_id();
        let mut conn = self.get_conn().await?;
        let result = diesel::sql_query(
            "INSERT INTO api_tokens \
             (id, user_id, name, remark, token_hash, prefix, enabled, valid_from, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 1, $7, $8)",
        )
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::Text, _>(name)
        .bind::<sql_types::Text, _>(remark)
        .bind::<sql_types::Text, _>(token_hash)
        .bind::<sql_types::Text, _>(prefix)
        .bind::<sql_types::Nullable<sql_types::Timestamptz>, _>(valid_from)
        .bind::<sql_types::Nullable<sql_types::Timestamptz>, _>(expires_at)
        .execute(&mut conn)
        .await;

        match result {
            Ok(_) => Ok(ApiTokenRow {
                id,
                user_id: user_id.to_string(),
                name: name.to_string(),
                remark: remark.to_string(),
                prefix: prefix.to_string(),
                enabled: 1,
                valid_from,
                expires_at,
                last_used_at: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }),
            Err(e) if is_unique_violation(&e) => Err(AppError::ConflictError(
                "令牌哈希冲突，请重试（极小概率事件）".into(),
            )),
            Err(e) => Err(AppError::from(e)),
        }
    }

    /// 按哈希查找令牌（Bearer 验证入口，走 `uq_api_tokens_hash` 唯一索引）。
    pub async fn find_by_hash(&self, token_hash: &str) -> Result<Option<ApiTokenRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let sql = format!("SELECT {TOKEN_COLS} FROM api_tokens WHERE token_hash = $1 LIMIT 1");
        let rows = diesel::sql_query(&sql)
            .bind::<sql_types::Text, _>(token_hash)
            .get_results::<ApiTokenRow>(&mut conn)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// 列出某用户的全部令牌（按创建时间倒序）。不含哈希。
    pub async fn list_by_user(&self, user_id: &str) -> Result<Vec<ApiTokenRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let sql = format!(
            "SELECT {TOKEN_COLS} FROM api_tokens WHERE user_id = $1 ORDER BY created_at DESC"
        );
        let rows = diesel::sql_query(&sql)
            .bind::<sql_types::Text, _>(user_id)
            .get_results::<ApiTokenRow>(&mut conn)
            .await?;
        Ok(rows)
    }

    /// 取某用户名下的指定令牌（更新/删除前校验归属）。不属于该用户返回 `None`（防越权）。
    pub async fn get_for_user(
        &self,
        id: &str,
        user_id: &str,
    ) -> Result<Option<ApiTokenRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let sql =
            format!("SELECT {TOKEN_COLS} FROM api_tokens WHERE id = $1 AND user_id = $2 LIMIT 1");
        let rows = diesel::sql_query(&sql)
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(user_id)
            .get_results::<ApiTokenRow>(&mut conn)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// 更新令牌可编辑字段（名称/备注/生效时间段/启用状态）。仅作用于本人令牌。
    /// 返回是否命中（`false` = 令牌不存在或不属于该用户）。
    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        id: &str,
        user_id: &str,
        name: &str,
        remark: &str,
        valid_from: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
        enabled: i16,
    ) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let affected = diesel::sql_query(
            "UPDATE api_tokens \
             SET name = $3, remark = $4, valid_from = $5, expires_at = $6, enabled = $7, \
                 updated_at = NOW() \
             WHERE id = $1 AND user_id = $2",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::Text, _>(name)
        .bind::<sql_types::Text, _>(remark)
        .bind::<sql_types::Nullable<sql_types::Timestamptz>, _>(valid_from)
        .bind::<sql_types::Nullable<sql_types::Timestamptz>, _>(expires_at)
        .bind::<sql_types::Int2, _>(enabled)
        .execute(&mut conn)
        .await?;
        Ok(affected > 0)
    }

    /// 更新最近使用时间（验证通过后调用；失败由调用方忽略，不影响请求）。
    pub async fn touch_last_used(&self, id: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("UPDATE api_tokens SET last_used_at = NOW() WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut conn)
            .await?;
        Ok(())
    }

    /// 删除令牌（仅作用于本人令牌）。返回是否命中。
    pub async fn delete(&self, id: &str, user_id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let affected = diesel::sql_query("DELETE FROM api_tokens WHERE id = $1 AND user_id = $2")
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(user_id)
            .execute(&mut conn)
            .await?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_token_has_prefix_and_length() {
        let t = generate_raw_token();
        assert!(t.starts_with("cxat_"), "应带 cxat_ 前缀: {t}");
        // 5 前缀字符 + base64url(32B)≈43 → 至少 48 字符
        assert!(t.len() >= 48, "长度应 ≥48: {t} (len={})", t.len());
    }

    #[test]
    fn sha256_hex_is_64_lowercase_hex_and_deterministic() {
        let h = sha256_hex("cxat_abc");
        assert_eq!(h.len(), 64);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "应为 64 位小写 hex: {h}"
        );
        assert_eq!(h, sha256_hex("cxat_abc"), "同输入应同输出");
        assert_ne!(h, sha256_hex("cxat_abd"), "不同输入应不同输出");
    }

    #[test]
    fn token_prefix_takes_first_12_chars() {
        assert_eq!(token_prefix("cxat_ABCDEFGH1234567890"), "cxat_ABCDEFG");
        assert_eq!(token_prefix("cxat_short").len(), 10, "短串原样返回");
    }

    #[test]
    fn generate_produces_unique_tokens() {
        let a = generate_raw_token();
        let b = generate_raw_token();
        assert_ne!(a, b, "两次生成不应相同");
    }
}
