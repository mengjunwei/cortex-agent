//! 助手枚举：`AssistantKind` / `AgentType` / `Visibility`。
//!
//! 落库遵循 `docs/architecture.md` §8.3：SMALLINT 存储，禁止原生 ENUM；
//! API 全程以 i16 传输（自定义 Serde），追加类型只往末尾扩展。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 助手来源类型：内置 / 自定义（数据驱动后两者均可由归属人编辑，仅来源标记不同）。
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AssistantKind {
    Builtin = 0,
    #[default]
    Custom = 1,
}

impl AssistantKind {
    pub fn as_i16(self) -> i16 {
        self as i16
    }
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::Custom,
            _ => Self::Builtin,
        }
    }
    pub fn try_from_i16(v: i16) -> Option<Self> {
        match v {
            0 => Some(Self::Builtin),
            1 => Some(Self::Custom),
            _ => None,
        }
    }
    pub fn is_custom(self) -> bool {
        matches!(self, Self::Custom)
    }
}

impl Serialize for AssistantKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i16(self.as_i16())
    }
}
impl<'de> Deserialize<'de> for AssistantKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_i16(i16::deserialize(d)?))
    }
}

/// 内置 Agent 调度类型，对应各 Agent 模块的分发字符串。
///
/// `Auto`(0) 和 `Chat`(1) 已废弃，旧数据统一按自定义助手处理。
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentType {
    DeviceCommand = 2,
    MonitorPlugin = 4,
    /// 自定义助手专用（走 `build_custom_agent`）。
    #[default]
    Custom = 9,
}

impl AgentType {
    pub fn as_i16(self) -> i16 {
        self as i16
    }
    pub fn from_i16(v: i16) -> Self {
        match v {
            2 => Self::DeviceCommand,
            4 => Self::MonitorPlugin,
            9 => Self::Custom,
            _ => Self::Custom,
        }
    }
    pub fn try_from_i16(v: i16) -> Option<Self> {
        match v {
            2 => Some(Self::DeviceCommand),
            4 => Some(Self::MonitorPlugin),
            9 => Some(Self::Custom),
            _ => None,
        }
    }
    pub fn dispatch_key(self) -> &'static str {
        match self {
            Self::DeviceCommand => "device_command",
            Self::MonitorPlugin => "monitor_plugin",
            Self::Custom => "custom",
        }
    }
}

impl Serialize for AgentType {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i16(self.as_i16())
    }
}
impl<'de> Deserialize<'de> for AgentType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_i16(i16::deserialize(d)?))
    }
}

/// 助手可见性：私有 / 广场公开 / 内置。
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Private = 0,
    Shared = 1,
    Builtin = 2,
}

impl Visibility {
    pub fn as_i16(self) -> i16 {
        self as i16
    }
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::Shared,
            2 => Self::Builtin,
            _ => Self::Private,
        }
    }
    pub fn try_from_i16(v: i16) -> Option<Self> {
        match v {
            0 => Some(Self::Private),
            1 => Some(Self::Shared),
            2 => Some(Self::Builtin),
            _ => None,
        }
    }
    pub fn is_public(self) -> bool {
        matches!(self, Self::Shared | Self::Builtin)
    }
}

impl Serialize for Visibility {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i16(self.as_i16())
    }
}
impl<'de> Deserialize<'de> for Visibility {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_i16(i16::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip_i16() {
        for v in [0, 1] {
            let k = AssistantKind::from_i16(v);
            assert_eq!(k.as_i16(), v);
        }
    }

    #[test]
    fn kind_serde_is_i16() {
        let json = serde_json::to_string(&AssistantKind::Custom).unwrap();
        assert_eq!(json, "1");
        let parsed: AssistantKind = serde_json::from_str("0").unwrap();
        assert_eq!(parsed, AssistantKind::Builtin);
    }

    #[test]
    fn agent_type_dispatch_key_matches_router() {
        assert_eq!(AgentType::DeviceCommand.dispatch_key(), "device_command");
        assert_eq!(AgentType::MonitorPlugin.dispatch_key(), "monitor_plugin");
    }

    #[test]
    fn agent_type_from_i16_unknown_falls_back_to_custom() {
        assert_eq!(AgentType::from_i16(0), AgentType::Custom);
        assert_eq!(AgentType::from_i16(1), AgentType::Custom);
        assert_eq!(AgentType::from_i16(99), AgentType::Custom);
    }

    #[test]
    fn agent_type_serde_is_i16() {
        assert_eq!(serde_json::to_string(&AgentType::Custom).unwrap(), "9");
        let parsed: AgentType = serde_json::from_str("2").unwrap();
        assert_eq!(parsed, AgentType::DeviceCommand);
    }

    #[test]
    fn visibility_serde_is_i16() {
        let json = serde_json::to_string(&Visibility::Shared).unwrap();
        assert_eq!(json, "1");
        let parsed: Visibility = serde_json::from_str("2").unwrap();
        assert_eq!(parsed, Visibility::Builtin);
    }

    #[test]
    fn visibility_is_public() {
        assert!(Visibility::Shared.is_public());
        assert!(Visibility::Builtin.is_public());
        assert!(!Visibility::Private.is_public());
    }
}
