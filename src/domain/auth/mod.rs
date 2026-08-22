//! 认证（SSO）领域服务
//!
//! 职责：
//! - 编排 OAuth 回调 → 换取外部身份 → 落库/复用用户 → 签发 JWT
//! - JWT 校验 + Redis 黑名单（主动登出）
//! - 暴露统一 API 供传输层调用
//!
//! 遵循 architecture.md §2.3：领域服务 + Repository 共置于此。

pub mod api_token;
pub mod enums;
pub mod jwt;
pub mod models;
pub mod password;
pub mod provider;
pub mod store;

use std::sync::Arc;

use bb8_redis::redis::AsyncCommands;
use chrono::{DateTime, Utc};
use tokio::time::{Duration, timeout};

use crate::error::AppError;
use crate::infra::redis::SharedRedisPool;

// 公开领域类型
pub use api_token::{ApiTokenRow, ApiTokenStore};
pub use enums::UserStatus;
pub use jwt::JwtService;
pub use models::{AuthUser, Claims, ExternalIdentity};
pub use provider::{OAuthProvider, ProviderInfo, ProviderRegistry};
pub use store::{UserRow, UserStore};

/// 认证领域服务（编排全部认证流程）
pub struct AuthService {
    users: Arc<UserStore>,
    api_tokens: Arc<ApiTokenStore>,
    registry: Arc<ProviderRegistry>,
    jwt: Arc<JwtService>,
    redis: Option<SharedRedisPool>,
    token_ttl_secs: i64,
    cookie_name: String,
    /// 用户不存在时用于消耗相似时间的 dummy 密码哈希（启动时生成，确保是合法 PHC 串）
    dummy_hash: String,
}

impl AuthService {
    /// 构造认证服务。密钥内置代码（APP_SECRETS），由 JwtService 校验。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        users: Arc<UserStore>,
        api_tokens: Arc<ApiTokenStore>,
        registry: Arc<ProviderRegistry>,
        jwt: Arc<JwtService>,
        redis: Option<SharedRedisPool>,
        token_ttl_secs: i64,
        cookie_name: String,
    ) -> Self {
        // 启动时生成一个合法的 argon2id 哈希，供用户不存在时跑一次 dummy 校验，
        // 使登录失败的两条路径（用户不存在 / 密码错误）耗时趋于一致，防止时序侧信道枚举用户名。
        let dummy_hash = password::hash_password("dummy-password-for-timing-equalization")
            .unwrap_or_else(|_| {
                // 理论上不会失败（OsRng + 标准参数）；若失败则退化为一个明显无效的串，
                // verify_password 会直接返回 false（降级为无 dummy 保护，但不影响正确性）
                String::new()
            });
        Self {
            users,
            api_tokens,
            registry,
            jwt,
            redis,
            token_ttl_secs,
            cookie_name,
            dummy_hash,
        }
    }

    /// 列出全部可用身份提供商（前端登录页展示）
    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        self.registry.list()
    }

    /// 按 key 获取 provider
    pub fn get_provider(&self, key: &str) -> Option<Arc<dyn OAuthProvider>> {
        self.registry.get(key)
    }

    /// 构造授权跳转 URL（由传输层调用，传入 CSRF state）
    pub async fn build_authorize_url(
        &self,
        provider_key: &str,
        state: &str,
    ) -> Result<String, AppError> {
        let provider = self
            .registry
            .get(provider_key)
            .ok_or_else(|| AppError::BusinessError(format!("未知的身份提供商: {provider_key}")))?;
        provider.authorize_url(state).await
    }

    /// 处理 OAuth 回调：code → 外部身份 → 落库/复用 → 签发 JWT
    ///
    /// 返回 (JWT token, Claims)。
    pub async fn complete_login(
        &self,
        provider_key: &str,
        code: &str,
    ) -> Result<(String, Claims), AppError> {
        let provider = self
            .registry
            .get(provider_key)
            .ok_or_else(|| AppError::BusinessError(format!("未知的身份提供商: {provider_key}")))?;

        tracing::info!(
            "[Auth] OAuth 回调处理开始 provider_key={} code_len={}",
            provider_key,
            code.len()
        );

        let ext = provider.exchange(code).await?;

        let user = match self
            .users
            .find_user_by_identity(&ext.provider, &ext.external_id)
            .await?
        {
            Some(u) => {
                tracing::info!("[Auth] 已有用户登录 user_id={}", u.id);
                u
            }
            None => {
                let u = self.users.create_user_with_identity(&ext).await?;
                tracing::info!("[Auth] 新用户注册 user_id={}", u.id);
                u
            }
        };

        let token = self
            .jwt
            .issue(&user.id, &user.name, &user.avatar, user.is_admin())?;
        let claims = self.jwt.verify(&token)?;
        Ok((token, claims))
    }

    /// 校验 JWT 并检查黑名单（Redis 可用时）
    pub async fn verify_token(&self, token: &str) -> Result<Claims, AppError> {
        let claims = self.jwt.verify(token)?;

        if let Some(redis) = &self.redis {
            // 给黑名单查询加 2 秒超时：Redis 不可达时快速降级（fail-open），
            // 避免每次请求都卡在 TCP 超时（默认 ~30s）导致接口极慢。
            match timeout(
                Duration::from_secs(2),
                self.is_blacklisted(redis, &claims.jti),
            )
            .await
            {
                Ok(Ok(true)) => {
                    return Err(AppError::BusinessError("会话已注销，请重新登录".into()));
                }
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    // Redis 查询出错（连接断开等）：fail-open，不阻断请求
                    tracing::warn!("[Auth] 黑名单查询失败（降级放行）: {e}");
                }
                Err(_) => {
                    // 2 秒超时：Redis 不可达，fail-open
                    tracing::warn!("[Auth] 黑名单查询超时 2s（Redis 不可达，降级放行）");
                }
            }
        }

        // 会话凭证版本校验：复用 users.updated_at 作为「改密时间戳」。
        // 改密时 set_password 把 updated_at 推进到 NOW()；token 自带的 iat（签发秒）早于它
        // 即为改密前签发的旧会话 → 一律拒绝。以此实现「改密后该账号在所有设备上的登录全部失效」，
        // 无需新增数据库列或 Claims 字段。用户已删除（None）同样拒绝。
        let changed_at = self
            .users
            .pwd_changed_at_epoch(&claims.sub)
            .await?
            .ok_or_else(|| AppError::BusinessError("会话用户不存在".into()))?;
        if (claims.iat as f64) < changed_at {
            return Err(AppError::BusinessError("会话凭证已变更，请重新登录".into()));
        }

        Ok(claims)
    }

    /// 将 token 加入黑名单（登出）。Redis 不可用时返回错误。
    pub async fn revoke_token(&self, jti: &str) -> Result<(), AppError> {
        let redis = self.redis.as_ref().ok_or_else(|| {
            // 没有 Redis 时，登出仅在客户端清除 Cookie 即可
            tracing::info!("[Auth] 未配置 Redis，登出仅清除客户端 Cookie");
            AppError::BusinessError("未配置 Redis，服务端黑名单不可用".into())
        })?;

        let key = blacklist_key(jti);
        let mut conn = redis
            .get()
            .await
            .map_err(|e| AppError::NetworkError(format!("获取 Redis 连接失败: {e}")))?;

        let ttl = self.token_ttl_secs.max(1) as u64;
        conn.set_ex::<_, _, ()>(&key, "1", ttl)
            .await
            .map_err(|e| AppError::NetworkError(format!("写入黑名单失败: {e}")))?;

        tracing::info!("[Auth] token 已加入黑名单 jti={jti}");
        Ok(())
    }

    // ── API Token（账户访问令牌，Bearer 认证路径） ──────────────────────

    /// 校验 API Token（`Authorization: Bearer` 路径）。
    ///
    /// 流程：SHA-256(明文) → 查表 → 校验启用 / 生效时段 / 所属用户启用态
    /// → 异步更新 `last_used_at`（失败忽略）→ 还原为 [`AuthUser`]。
    ///
    /// 安全：所有失败（不存在 / 已禁用 / 未生效 / 已过期 / 用户被禁用）统一返回相同错误，
    /// 不泄露具体原因，防止令牌探测。
    pub async fn verify_api_token(&self, raw_token: &str) -> Result<AuthUser, AppError> {
        let fail = || AppError::BusinessError("无效或已失效的 API Token".into());

        let hash = api_token::sha256_hex(raw_token);
        let row = self
            .api_tokens
            .find_by_hash(&hash)
            .await?
            .ok_or_else(fail)?;

        if !row.is_enabled() {
            return Err(fail());
        }
        let now = Utc::now();
        if matches!(row.valid_from, Some(from) if now < from) {
            return Err(fail());
        }
        if matches!(row.expires_at, Some(exp) if now > exp) {
            return Err(fail());
        }

        // 所属用户必须仍为启用态（禁用用户的令牌同步失效）
        let user = self.users.get_user(&row.user_id).await?.ok_or_else(fail)?;
        if user.status_enum() != UserStatus::Active {
            return Err(fail());
        }

        // 更新最近使用时间（非关键路径，失败仅记日志）
        if let Err(e) = self.api_tokens.touch_last_used(&row.id).await {
            tracing::warn!("[Auth] 更新 token last_used_at 失败（忽略）: {e}");
        }
        tracing::debug!(
            "[Auth] API Token 验证成功 user_id={} prefix={}",
            user.id,
            row.prefix
        );
        let is_admin = user.is_admin();
        Ok(AuthUser {
            user_id: user.id,
            name: user.name,
            avatar: user.avatar,
            is_admin,
        })
    }

    /// 创建 API Token。返回 `(明文令牌, 令牌行)`；明文仅此一次返回，调用方负责提示用户保存。
    pub async fn create_token(
        &self,
        user_id: &str,
        name: &str,
        remark: &str,
        valid_from: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(String, ApiTokenRow), AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::BusinessError("令牌名称不能为空".into()));
        }
        validate_token_window(valid_from, expires_at)?;

        let raw = api_token::generate_raw_token();
        let hash = api_token::sha256_hex(&raw);
        let prefix = api_token::token_prefix(&raw);
        let row = self
            .api_tokens
            .create(
                user_id,
                name,
                remark.trim(),
                &hash,
                &prefix,
                valid_from,
                expires_at,
            )
            .await?;
        tracing::info!(
            "[Auth] API Token 创建 user_id={} token_id={} prefix={}",
            user_id,
            row.id,
            prefix
        );
        Ok((raw, row))
    }

    /// 列出某用户的全部令牌（脱敏，不含哈希与明文）。
    pub async fn list_tokens(&self, user_id: &str) -> Result<Vec<ApiTokenRow>, AppError> {
        self.api_tokens.list_by_user(user_id).await
    }

    /// 更新令牌可编辑字段（名称 / 备注 / 生效时间段 / 启用状态）。仅作用于本人令牌。
    /// 返回是否命中（`false` = 令牌不存在或不属于该用户）。
    #[allow(clippy::too_many_arguments)]
    pub async fn update_token(
        &self,
        user_id: &str,
        id: &str,
        name: &str,
        remark: &str,
        valid_from: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
        enabled: bool,
    ) -> Result<bool, AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::BusinessError("令牌名称不能为空".into()));
        }
        validate_token_window(valid_from, expires_at)?;
        self.api_tokens
            .update(
                id,
                user_id,
                name,
                remark.trim(),
                valid_from,
                expires_at,
                if enabled { 1 } else { 0 },
            )
            .await
    }

    /// 删除令牌（仅作用于本人令牌）。返回是否命中。
    pub async fn delete_token(&self, user_id: &str, id: &str) -> Result<bool, AppError> {
        let hit = self.api_tokens.delete(id, user_id).await?;
        if hit {
            tracing::info!("[Auth] API Token 删除 user_id={} token_id={}", user_id, id);
        }
        Ok(hit)
    }

    /// Cookie 名称
    pub fn cookie_name(&self) -> &str {
        &self.cookie_name
    }

    /// JWT 有效期（秒）— 供传输层设置 Cookie Max-Age
    pub fn token_ttl_secs(&self) -> i64 {
        self.token_ttl_secs
    }

    /// 是否至少有一个 provider 可用
    pub fn has_providers(&self) -> bool {
        !self.registry.is_empty()
    }

    /// 本地账号注册（用户名密码）。
    ///
    /// - 校验用户名/密码格式
    /// - argon2id 哈希密码
    /// - **首用户自动成为管理员**（bootstrap：空库时 count == 0 → is_admin = true）
    ///
    /// 并发说明：首个用户注册存在极小竞态窗口（两个并发请求都读到 count=0），
    /// 最坏情况为产生 2 个管理员，影响可控（管理员可事后调整）。对于 bootstrap 场景可接受。
    ///
    /// 成功返回 (JWT token, Claims)。
    pub async fn register_local(
        &self,
        username: &str,
        password: &str,
        name: &str,
    ) -> Result<(String, Claims), AppError> {
        if let Some(msg) = password::validate_username(username) {
            return Err(AppError::BusinessError(msg.into()));
        }
        if let Some(msg) = password::validate_password(password) {
            return Err(AppError::BusinessError(msg.into()));
        }
        if let Some(msg) = password::validate_display_name(name) {
            return Err(AppError::BusinessError(msg.into()));
        }

        let hash = password::hash_password(password)?;

        // 首个用户自动管理员（bootstrap）
        let existing = self.users.count_users().await?;
        let is_admin = existing == 0;
        if is_admin {
            tracing::info!("[Auth] 系统尚无用户，首个注册账号 {username} 将成为管理员");
        }

        let user = self
            .users
            .create_local_user(username, &hash, name, is_admin)
            .await?;

        tracing::info!(
            "[Auth] 本地账号注册成功 user_id={} username={} is_admin={}",
            user.id,
            username,
            user.is_admin()
        );

        let token = self
            .jwt
            .issue(&user.id, &user.name, &user.avatar, user.is_admin())?;
        let claims = self.jwt.verify(&token)?;
        Ok((token, claims))
    }

    /// 本地账号登录（用户名密码）。
    ///
    /// 安全说明：用户不存在与密码错误统一返回相同错误信息，避免用户名枚举。
    /// 即使查不到用户也会执行一次 argon2 校验（dummy hash），使响应时序趋于一致。
    /// dummy hash 在 `AuthService::new` 时生成（合法 PHC 串），保证 argon2 真正执行。
    ///
    /// 成功返回 (JWT token, Claims)。
    pub async fn login_local(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(String, Claims), AppError> {
        let user = self.users.find_user_by_username(username).await?;

        let ok = Self::password_matches(user.as_ref(), password, &self.dummy_hash);
        if !ok {
            return Err(AppError::BusinessError("用户名或密码错误".into()));
        }

        // ok == true ⇒ user 必为 Some
        let user = user.unwrap();
        if user.status_enum() != UserStatus::Active {
            return Err(AppError::BusinessError("账号已被禁用".into()));
        }

        tracing::info!(
            "[Auth] 本地账号登录成功 user_id={} username={}",
            user.id,
            username
        );

        let token = self
            .jwt
            .issue(&user.id, &user.name, &user.avatar, user.is_admin())?;
        let claims = self.jwt.verify(&token)?;
        Ok((token, claims))
    }

    /// 校验明文密码 vs 用户 PHC。用户不存在 / 无密码哈希时都跑一次 dummy 校验消耗相似时间
    /// （恒返回 false），防登录与二次确认两条路径的时序侧信道。login_local 与
    /// verify_user_password 共用，避免安全关键逻辑双份漂移。
    fn password_matches(user: Option<&UserRow>, password: &str, dummy_hash: &str) -> bool {
        match user {
            Some(u) => match &u.password_hash {
                Some(phc) => password::verify_password(password, phc),
                None => {
                    let _ = password::verify_password(password, dummy_hash);
                    false
                }
            },
            None => {
                let _ = password::verify_password(password, dummy_hash);
                false
            }
        }
    }

    /// 按 user_id 校验明文密码（用于敏感操作的二次确认，如查看助手环境变量明文）。
    ///
    /// 与 [`login_local`](Self::login_local) 同路径（共用 [`password_matches`]），并额外校验
    /// 账号状态（被禁用账号不允许通过二次确认）。只做校验、不签发 token。
    /// DB 错误用 `?` 传播（区别于「密码错误」），避免把后端故障误报成鉴权失败。
    pub async fn verify_user_password(
        &self,
        user_id: &str,
        password: &str,
    ) -> Result<bool, AppError> {
        let user = self.users.get_user(user_id).await?;
        let ok = Self::password_matches(user.as_ref(), password, &self.dummy_hash);
        if !ok {
            return Ok(false);
        }
        // ok == true ⇒ user 必为 Some；禁用账号拒绝（对齐 login_local）
        if user.as_ref().unwrap().status_enum() != UserStatus::Active {
            return Ok(false);
        }
        Ok(true)
    }

    /// 修改当前用户密码（登录态自助修改）。
    ///
    /// 流程：校验原密码（复用 [`verify_user_password`](Self::verify_user_password)，含 dummy 防
    /// timing 与账号状态校验）→ 校验新密码格式 → argon2 哈希 → 写库（[`set_password`](UserStore::set_password)
    /// 同时推进 `updated_at`）。
    ///
    /// **安全语义**：写库后该用户全部已有 JWT 会话立即失效（`verify_token` 因 `iat < updated_at`
    /// 拒绝）。本方法不签发新 token——调用方（传输层）应引导用户重新登录。
    pub async fn change_password(
        &self,
        user_id: &str,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), AppError> {
        let ok = self.verify_user_password(user_id, old_password).await?;
        if !ok {
            return Err(AppError::BusinessError("原密码错误".into()));
        }
        if let Some(msg) = password::validate_password(new_password) {
            return Err(AppError::BusinessError(msg.into()));
        }
        let new_hash = password::hash_password(new_password)?;
        self.users.set_password(user_id, &new_hash).await?;
        tracing::info!("[Auth] 用户修改密码成功 user_id={}", user_id);
        Ok(())
    }

    /// 当前用户是否设有本地密码（本地账号 → `true`；纯 SSO 账号 `password_hash` 为 `NULL` → `false`）。
    /// 供前端决定是否显示「修改密码」入口。
    pub async fn user_has_password(&self, user_id: &str) -> Result<bool, AppError> {
        Ok(self
            .users
            .get_user(user_id)
            .await?
            .map(|u| u.password_hash.is_some())
            .unwrap_or(false))
    }

    async fn is_blacklisted(&self, redis: &SharedRedisPool, jti: &str) -> Result<bool, AppError> {
        let key = blacklist_key(jti);
        let mut conn = redis
            .get()
            .await
            .map_err(|e| AppError::NetworkError(format!("获取 Redis 连接失败: {e}")))?;
        let val: Option<String> = conn
            .get(&key)
            .await
            .map_err(|e| AppError::NetworkError(format!("查询黑名单失败: {e}")))?;
        Ok(val.is_some())
    }
}

fn blacklist_key(jti: &str) -> String {
    format!("auth:blacklist:{jti}")
}

/// 校验令牌生效时间段合理性：起始不得晚于结束（两者均存在时）。
fn validate_token_window(
    valid_from: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<(), AppError> {
    if matches!((valid_from, expires_at), (Some(from), Some(exp)) if from > exp) {
        return Err(AppError::BusinessError(
            "生效起始时间不能晚于过期时间".into(),
        ));
    }
    Ok(())
}

impl AuthUser {
    /// 从 Claims 构造 AuthUser
    pub fn from_claims(claims: &Claims) -> Self {
        Self {
            user_id: claims.sub.clone(),
            name: claims.name.clone(),
            avatar: claims.avatar.clone(),
            is_admin: claims.is_admin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blacklist_key_format() {
        assert_eq!(blacklist_key("abc-123"), "auth:blacklist:abc-123");
    }

    #[test]
    fn auth_user_from_claims() {
        let claims = Claims {
            sub: "uid-1".into(),
            name: "Alice".into(),
            avatar: "https://avatar".into(),
            is_admin: true,
            jti: "jti-1".into(),
            exp: 9999999999,
            iat: 1000000000,
        };
        let user = AuthUser::from_claims(&claims);
        assert_eq!(user.user_id, "uid-1");
        assert_eq!(user.name, "Alice");
        assert_eq!(user.avatar, "https://avatar");
        assert!(user.is_admin, "is_admin 应原样回传");
    }
}
