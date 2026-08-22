//! 用户状态枚举（SMALLINT 数字存储，遵循 architecture.md §8.3）
//!
//! 与 `model_provider::enums::Status` 风格一致：
//! 0=禁用, 1=启用；Serde 始终以 i16 数字序列化。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 用户启用状态
///
/// | 值 | 含义 |
/// |----|------|
/// | 0  | 禁用（Disabled） |
/// | 1  | 启用（Active）   |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserStatus {
    Disabled = 0,
    #[default]
    Active = 1,
}

impl UserStatus {
    pub fn as_i16(self) -> i16 {
        self as i16
    }

    pub fn from_i16(v: i16) -> Self {
        if v == UserStatus::Active.as_i16() {
            UserStatus::Active
        } else {
            UserStatus::Disabled
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, UserStatus::Active)
    }
}

impl Serialize for UserStatus {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i16(self.as_i16())
    }
}

impl<'de> Deserialize<'de> for UserStatus {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(UserStatus::from_i16(i16::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_i16() {
        assert_eq!(UserStatus::Active.as_i16(), 1);
        assert_eq!(UserStatus::Disabled.as_i16(), 0);
        assert_eq!(UserStatus::from_i16(1), UserStatus::Active);
        assert_eq!(UserStatus::from_i16(0), UserStatus::Disabled);
    }

    #[test]
    fn from_i16_unknown_falls_back_to_disabled() {
        assert_eq!(UserStatus::from_i16(99), UserStatus::Disabled);
    }

    #[test]
    fn serde_as_number() {
        let json = serde_json::to_string(&UserStatus::Active).unwrap();
        assert_eq!(json, "1");

        let parsed: UserStatus = serde_json::from_str("0").unwrap();
        assert_eq!(parsed, UserStatus::Disabled);
    }

    #[test]
    fn default_is_active() {
        assert_eq!(UserStatus::default(), UserStatus::Active);
    }
}
