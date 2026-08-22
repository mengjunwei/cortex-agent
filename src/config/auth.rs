//! 认证配置段 — `[auth]` / `[[auth.providers]]`

use serde::Deserialize;

/// 单个身份认证提供商配置（`[[auth.providers]]` 数组项）
///
/// 支持同类型多实例（如两个 OIDC），通过 `kind:name` 复合键区分。
/// `client_secret_enc` 约定以 `enc:` 前缀标识已用 AesCodec 加密；
/// 不带前缀时视为明文（仅限开发调试，生产环境务必加密）。
#[derive(Debug, Clone, Deserialize)]
pub struct AuthProviderConfig {
    /// 提供商类型：`feishu` / `wechat` / `oidc`
    pub kind: String,
    /// 实例展示名称（前端展示 + 实例键的一部分）
    pub name: String,
    /// OAuth client_id / app_id
    pub client_id: String,
    /// OAuth client_secret（密文，`enc:` 前缀表示 AesCodec 加密）
    #[serde(default)]
    pub client_secret_enc: String,
    /// 回调 URI（需与第三方后台配置一致）
    pub redirect_uri: String,

    // ---- OIDC 专用（feishu/wechat 忽略）----
    /// OIDC issuer URL（用于 `/.well-known/openid-configuration` 自动发现）
    #[serde(default)]
    pub issuer: String,
    /// 手动覆盖 authorize endpoint（留空时走 OIDC discovery）
    #[serde(default)]
    pub authorize_url: String,
    /// 手动覆盖 token endpoint
    #[serde(default)]
    pub token_url: String,
    /// 手动覆盖 userinfo endpoint
    #[serde(default)]
    pub userinfo_url: String,
    /// OIDC scope（默认 `openid profile email`）
    #[serde(default = "default_oidc_scope")]
    pub scope: String,
}

/// 认证配置（`[auth]` 段）
///
/// 支持配置多个身份提供商并存，全部注册到 ProviderRegistry。
/// 认证**始终启用**（数据库可用时即生效，不可关闭），避免 `enabled` 开关被误触关闭。
/// `providers` 为空时仅本地用户名密码登录（首个注册用户自动成为管理员）。
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// JWT 有效期（秒），默认 86400
    #[serde(default = "default_jwt_ttl")]
    pub token_ttl_secs: i64,
    /// 会话 Cookie 名称
    #[serde(default = "default_cookie_name")]
    pub cookie_name: String,
    /// 身份提供商列表（支持多实例并存）
    #[serde(default)]
    pub providers: Vec<AuthProviderConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            token_ttl_secs: default_jwt_ttl(),
            cookie_name: default_cookie_name(),
            providers: Vec::new(),
        }
    }
}

/// 默认 OIDC scope
fn default_oidc_scope() -> String {
    "openid profile email".to_string()
}
/// 默认 JWT 有效期：24 小时
fn default_jwt_ttl() -> i64 {
    86_400
}
/// 默认 Cookie 名称
fn default_cookie_name() -> String {
    "cortex_session".to_string()
}
