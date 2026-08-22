//! MCP 传输方式与运行状态的枚举定义
//!
//! - [`TransportKind`]：传输协议（DB 以 SMALLINT 存储，1=stdio / 2=streamable_http）
//! - [`Status`]：启用状态（复用 [`crate::domain::model_provider::enums::Status`] 的 0/1 语义）

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// MCP 传输方式
///
/// | 值 | 含义 |
/// |----|------|
/// | 1  | stdio（子进程传输） |
/// | 2  | streamable_http（远程 HTTP 传输） |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio = 1,
    StreamableHttp = 2,
}

impl TransportKind {
    /// 转为 DB/API 数字表示
    pub fn as_i16(self) -> i16 {
        self as i16
    }

    /// 从 DB/API 数字还原；非法值回退到 Stdio（防御性默认）
    pub fn from_i16(v: i16) -> Self {
        match v {
            2 => TransportKind::StreamableHttp,
            _ => TransportKind::Stdio,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::StreamableHttp => "streamable_http",
        }
    }
}

// === Serde：始终以 i16 数字序列化/反序列化（与 Status 风格一致） ===

impl Serialize for TransportKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i16(self.as_i16())
    }
}

impl<'de> Deserialize<'de> for TransportKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(TransportKind::from_i16(i16::deserialize(d)?))
    }
}

pub use crate::domain::model_provider::enums::Status;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_roundtrip() {
        assert_eq!(TransportKind::Stdio.as_i16(), 1);
        assert_eq!(TransportKind::StreamableHttp.as_i16(), 2);
        assert_eq!(TransportKind::from_i16(1), TransportKind::Stdio);
        assert_eq!(TransportKind::from_i16(2), TransportKind::StreamableHttp);
    }

    #[test]
    fn transport_invalid_falls_back_to_stdio() {
        assert_eq!(TransportKind::from_i16(0), TransportKind::Stdio);
        assert_eq!(TransportKind::from_i16(99), TransportKind::Stdio);
        assert_eq!(TransportKind::from_i16(-1), TransportKind::Stdio);
    }

    #[test]
    fn transport_serde_uses_i16() {
        let json = serde_json::to_string(&TransportKind::StreamableHttp).unwrap();
        assert_eq!(json, "2");
        let t: TransportKind = serde_json::from_str("1").unwrap();
        assert_eq!(t, TransportKind::Stdio);
    }

    #[test]
    fn transport_label() {
        assert_eq!(TransportKind::Stdio.label(), "stdio");
        assert_eq!(TransportKind::StreamableHttp.label(), "streamable_http");
    }

    #[test]
    fn status_reexport_matches_model_provider() {
        assert_eq!(Status::Enabled.as_i16(), 1);
        assert_eq!(Status::Disabled.as_i16(), 0);
    }
}
