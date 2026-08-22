//! 认证领域模型 — 跨 provider 统一的身份与会话数据结构

use serde::{Deserialize, Serialize};

/// 第三方身份提供商返回的统一身份信息
///
/// 所有 provider 适配器（Feishu / WeChat / OIDC）的 `exchange()` 都产出此结构，
/// 使上层 `AuthService` 无需关心具体 provider 的字段差异。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalIdentity {
    /// provider 类型标识：`feishu` / `wechat` / `oidc`
    pub provider: String,
    /// 第三方返回的唯一用户 ID（open_id / sub / openid）
    pub external_id: String,
    /// 显示名称
    #[serde(default)]
    pub name: String,
    /// 头像 URL
    #[serde(default)]
    pub avatar: String,
    /// 邮箱（可能为空）
    #[serde(default)]
    pub email: String,
    /// 第三方返回的原始用户信息（JSON 序列化字符串，落库 raw_payload）
    #[serde(default)]
    pub raw_payload: String,
}

/// 已认证用户（从 JWT Claims 还原，供 handler 使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub user_id: String,
    pub name: String,
    pub avatar: String,
    /// 是否管理员
    #[serde(default)]
    pub is_admin: bool,
}

/// JWT Claims
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// 用户 ID
    pub sub: String,
    /// 显示名称
    pub name: String,
    /// 头像 URL
    pub avatar: String,
    /// 是否管理员（兼容旧 token：缺省时按 false 处理）
    #[serde(default)]
    pub is_admin: bool,
    /// 唯一令牌 ID（用于黑名单 / 撤销）
    pub jti: String,
    /// 过期时间（Unix 时间戳）
    pub exp: usize,
    /// 签发时间（Unix 时间戳）
    pub iat: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_identity_serialize_deserialize() {
        let ext = ExternalIdentity {
            provider: "feishu".into(),
            external_id: "ou_123".into(),
            name: "Alice".into(),
            avatar: "https://avatar".into(),
            email: "a@b.com".into(),
            raw_payload: r#"{"k":"v"}"#.into(),
        };
        let json = serde_json::to_string(&ext).unwrap();
        let back: ExternalIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider, "feishu");
        assert_eq!(back.external_id, "ou_123");
    }

    #[test]
    fn external_identity_defaults() {
        let ext: ExternalIdentity =
            serde_json::from_str(r#"{"provider":"wechat","external_id":"wx_1"}"#).unwrap();
        assert_eq!(ext.name, "");
        assert_eq!(ext.raw_payload, "");
    }
}
