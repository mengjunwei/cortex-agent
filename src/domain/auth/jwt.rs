//! JWT 签发与校验服务（HS256，无状态会话令牌）
//!
//! 设计要点：
//! - 使用 `jsonwebtoken` crate，算法 HS256（对称密钥，无需非对称密钥管理）
//! - 每个 token 携带唯一 `jti`（UUID v7），用于 Redis 黑名单实现"主动登出"
//! - `exp` 由 `jsonwebtoken` 默认 Validation 自动校验

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use uuid::Uuid;

use crate::error::AppError;

use super::models::Claims;

/// JWT 编解码服务
pub struct JwtService {
    encoding: EncodingKey,
    decoding: DecodingKey,
    ttl_secs: i64,
}

impl JwtService {
    /// 构造服务。密钥至少 32 字节（HS256 安全要求），否则返回配置错误。
    pub fn new(secret: &str, ttl_secs: i64) -> Result<Self, AppError> {
        if secret.len() < 32 {
            return Err(AppError::ConfigError(
                "JWT 密钥至少需要 32 字节，请检查 [auth].jwt_secret".into(),
            ));
        }
        Ok(Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
            ttl_secs,
        })
    }

    /// 签发 JWT
    pub fn issue(
        &self,
        user_id: &str,
        name: &str,
        avatar: &str,
        is_admin: bool,
    ) -> Result<String, AppError> {
        let now = chrono::Utc::now();
        let exp = now + chrono::Duration::seconds(self.ttl_secs);
        let claims = Claims {
            sub: user_id.to_string(),
            name: name.to_string(),
            avatar: avatar.to_string(),
            is_admin,
            jti: Uuid::now_v7().to_string(),
            exp: exp.timestamp() as usize,
            iat: now.timestamp() as usize,
        };
        encode(&Header::default(), &claims, &self.encoding)
            .map_err(|e| AppError::Unknown(format!("JWT 签发失败: {e}")))
    }

    /// 校验 JWT 并返回 Claims
    pub fn verify(&self, token: &str) -> Result<Claims, AppError> {
        decode::<Claims>(token, &self.decoding, &Validation::new(Algorithm::HS256))
            .map(|d| d.claims)
            .map_err(|e| AppError::BusinessError(format!("无效或已过期的会话令牌: {e}")))
    }

    /// 从 Claims 提取 jti（供黑名单检查）
    pub fn token_jti(claims: &Claims) -> &str {
        &claims.jti
    }
}

// =========================================================================
//  单元测试（TDD：先写测试定义行为，再实现使其通过）
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret-must-be-at-least-32-bytes-long!!";

    #[test]
    fn reject_secret_shorter_than_32_bytes() {
        let result = JwtService::new("short", 3600);
        assert!(matches!(result, Err(AppError::ConfigError(_))));
    }

    #[test]
    fn issue_and_verify_roundtrip() {
        let svc = JwtService::new(SECRET, 3600).unwrap();
        let token = svc.issue("uid-1", "Alice", "https://avatar", true).unwrap();
        assert!(!token.is_empty());

        let claims = svc.verify(&token).unwrap();
        assert_eq!(claims.sub, "uid-1");
        assert_eq!(claims.name, "Alice");
        assert_eq!(claims.avatar, "https://avatar");
        assert!(claims.is_admin, "is_admin 应原样回传");
        assert!(!claims.jti.is_empty());
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn reject_tampered_token() {
        let svc = JwtService::new(SECRET, 3600).unwrap();
        let token = svc.issue("uid-1", "Alice", "", false).unwrap();

        // 篡改 payload 部分最后一个字符
        let mut parts: Vec<&str> = token.split('.').collect();
        let payload = parts[1].to_string();
        let (head, tail) = payload.split_at(payload.len() - 4);
        let tampered = format!("{}{}{}", head, &tail[..tail.len() - 1], "X");
        parts[1] = &tampered;
        let forged = parts.join(".");

        let err = svc.verify(&forged).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn reject_token_signed_with_different_secret() {
        let svc_a = JwtService::new(SECRET, 3600).unwrap();
        let svc_b = JwtService::new("another-secret-also-32-bytes-or-more!!", 3600).unwrap();

        let token = svc_a.issue("uid", "Bob", "", false).unwrap();
        let err = svc_b.verify(&token).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn reject_expired_token() {
        // ttl 远小于 0 且超过 jsonwebtoken 默认 leeway → 签发的 token 必然过期
        let svc = JwtService::new(SECRET, -120).unwrap();
        let token = svc.issue("uid", "Carol", "", false).unwrap();
        let result = svc.verify(&token);
        assert!(matches!(result, Err(AppError::BusinessError(_))));
    }

    #[test]
    fn jti_is_unique_per_token() {
        let svc = JwtService::new(SECRET, 3600).unwrap();
        let t1 = svc.issue("uid", "Dave", "", false).unwrap();
        let t2 = svc.issue("uid", "Dave", "", false).unwrap();
        let c1 = svc.verify(&t1).unwrap();
        let c2 = svc.verify(&t2).unwrap();
        assert_ne!(c1.jti, c2.jti);
    }

    #[test]
    fn token_jti_extracts_correctly() {
        let svc = JwtService::new(SECRET, 3600).unwrap();
        let token = svc.issue("uid", "Eve", "", false).unwrap();
        let claims = svc.verify(&token).unwrap();
        assert_eq!(JwtService::token_jti(&claims), claims.jti);
    }
}
