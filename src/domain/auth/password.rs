//! 本地账号密码哈希（argon2id）与输入校验。
//!
//! 选型说明：argon2id（PHC 串格式存储，含随机盐 + 参数）是 OWASP 推荐的密码哈希算法。
//! 验证使用常量时间比较，避免时序侧信道泄露。

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};

use crate::error::AppError;

/// 用户名长度区间
pub const USERNAME_MIN: usize = 3;
pub const USERNAME_MAX: usize = 32;
/// 密码最小长度
pub const PASSWORD_MIN: usize = 8;
/// 密码最大长度（防止 DoS：argon2 对超长输入开销大）
pub const PASSWORD_MAX: usize = 128;
/// 显示名最大长度（与 DB users.name VARCHAR(128) 对齐）
pub const DISPLAY_NAME_MAX: usize = 128;

/// 对明文密码进行 argon2id 哈希，返回 PHC 串（可直接存入数据库）
pub fn hash_password(plain: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| AppError::Unknown(format!("密码哈希失败: {e}")))?
        .to_string();
    Ok(hash)
}

/// 校验明文密码是否匹配 PHC 串（常量时间）。
///
/// 任何解析/校验失败均返回 `false`，调用方统一按"密码错误"处理，
/// 避免区分"用户不存在"与"密码错误"导致用户名枚举。
pub fn verify_password(plain: &str, phc: &str) -> bool {
    let parsed = match PasswordHash::new(phc) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok()
}

/// 校验用户名格式：3-32 字符，仅允许字母/数字/下划线/连字符。
/// 返回错误信息（None 表示合法）。
pub fn validate_username(username: &str) -> Option<&'static str> {
    let len = username.chars().count();
    if len < USERNAME_MIN {
        return Some("用户名至少 3 个字符");
    }
    if len > USERNAME_MAX {
        return Some("用户名最多 32 个字符");
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some("用户名仅允许字母、数字、下划线和连字符");
    }
    None
}

/// 校验密码强度：最少 8 位，防止过短；最大 128 位防 DoS。
pub fn validate_password(password: &str) -> Option<&'static str> {
    if password.len() < PASSWORD_MIN {
        return Some("密码至少 8 个字符");
    }
    if password.len() > PASSWORD_MAX {
        return Some("密码过长");
    }
    None
}

/// 校验显示名长度（空字符串合法，表示回退为用户名）。
/// 超过 [`DISPLAY_NAME_MAX`] 字符返回错误提示。
pub fn validate_display_name(name: &str) -> Option<&'static str> {
    if name.chars().count() > DISPLAY_NAME_MAX {
        return Some("显示名过长");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        assert!(!verify_password("anything", "not-a-valid-hash"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn username_validation_rules() {
        assert!(validate_username("ab").is_some(), "过短");
        assert!(validate_username("alice_01").is_none());
        assert!(validate_username("bob-2").is_none());
        assert!(validate_username("张三").is_some(), "非 ASCII 字母数字");
        assert!(validate_username("a.b").is_some(), "含非法字符");
        assert!(validate_username(&"a".repeat(33)).is_some(), "过长");
    }

    #[test]
    fn password_validation_rules() {
        assert!(validate_password("1234567").is_some(), "过短");
        assert!(validate_password("12345678").is_none());
        assert!(validate_password(&"a".repeat(129)).is_some(), "过长");
    }
}
