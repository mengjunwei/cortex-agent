//! 模型供应商/模型的枚举定义
//!
//! 数据库中以 `SMALLINT` 数字存储（0/1），API 也传输数字，
//! 前端做 `0↔禁用` / `1↔启用` 的映射转换。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 启用状态（数字枚举）
///
/// | 值 | 含义 |
/// |----|------|
/// | 0  | 禁用（Disabled） |
/// | 1  | 启用（Enabled）  |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Disabled = 0,
    #[default]
    Enabled = 1,
}

impl Status {
    /// 转为 DB/API 数字表示
    pub fn as_i16(self) -> i16 {
        self as i16
    }

    /// 从 DB/API 数字还原
    pub fn from_i16(v: i16) -> Self {
        if v == Status::Enabled.as_i16() {
            Status::Enabled
        } else {
            Status::Disabled
        }
    }

    pub fn is_enabled(self) -> bool {
        matches!(self, Status::Enabled)
    }

    /// 前端展示用的中文标签
    pub fn label(self) -> &'static str {
        match self {
            Status::Disabled => "禁用",
            Status::Enabled => "启用",
        }
    }
}

// === Serde：始终以 i16 数字序列化/反序列化（对接前后端数字协议） ===

impl Serialize for Status {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i16(self.as_i16())
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Status::from_i16(i16::deserialize(d)?))
    }
}

// === 供应商接入协议（字符串枚举，决定 make_model 走哪条客户端链路） ===

/// 模型供应商的接入协议
///
/// | 值 | 含义 |
/// |----|------|
/// | `openai_compat` | OpenAI Compatible 协议（`/chat/completions`），走自研 OpenAICustomCompatible |
/// | `anthropic`     | Anthropic Messages 协议（`/v1/messages`），走本地 anthropic_custom::AnthropicClient |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderProtocol {
    #[default]
    OpenAiCompat,
    Anthropic,
}

impl ProviderProtocol {
    /// DB 存储字符串
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompat => "openai_compat",
            Self::Anthropic => "anthropic",
        }
    }

    /// 从 DB/JSON 字符串解析；未知值一律按 OpenAI 兼容（历史数据兜底）
    pub fn parse(s: &str) -> Self {
        if s.trim() == "anthropic" {
            Self::Anthropic
        } else {
            Self::OpenAiCompat
        }
    }

    /// 前端展示用的中文标签
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompat => "OpenAI 兼容",
            Self::Anthropic => "Anthropic",
        }
    }
}

impl Serialize for ProviderProtocol {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderProtocol {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(d)?))
    }
}
