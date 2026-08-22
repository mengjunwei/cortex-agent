//! 请求/响应 DTO
//!
//! 重要安全约定：**API Key 永不返回给前端**。
//! - 创建/重置时通过请求体接收明文 `api_key`
//! - 响应中仅返回 `api_key_suffix`（末 4 位掩码），用于前端识别

use serde::{Deserialize, Serialize};

use crate::domain::model_provider::enums::{ProviderProtocol, Status};

// ========== 供应商 ==========

/// 新建供应商请求
#[derive(Debug, Deserialize)]
pub struct CreateProviderRequest {
    pub vendor_name: String,
    pub name: String,
    pub base_url: String,
    /// 接入协议（缺省 openai_compat）
    #[serde(default)]
    pub protocol: ProviderProtocol,
    /// 明文 API Key（仅在此处传入，加密存储）
    pub api_key: String,
    #[serde(default)]
    pub status: Status,
}

/// 更新供应商请求（**不含 api_key**，密钥通过 reset-key 接口重置）
#[derive(Debug, Deserialize)]
pub struct UpdateProviderRequest {
    pub vendor_name: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub protocol: ProviderProtocol,
    #[serde(default)]
    pub status: Status,
}

/// 重置 API Key 请求
#[derive(Debug, Deserialize)]
pub struct ResetKeyRequest {
    pub api_key: String,
}

/// 供应商响应（对外展示，无明文密钥）
#[derive(Debug, Serialize)]
pub struct ProviderResponse {
    pub id: String,
    pub vendor_name: String,
    pub name: String,
    pub base_url: String,
    pub protocol: ProviderProtocol,
    /// 末 4 位掩码，如 "ab12"；为空表示尚未配置密钥
    pub api_key_suffix: String,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
    /// 该供应商下的模型列表
    pub models: Vec<ModelResponse>,
    /// 归属人 user_id（管理员视图「归属」列用；普通用户恒为自己）
    pub user_id: String,
}

// ========== 模型 ==========

/// tags 默认值（新建/更新模型未传 tags 时）
fn default_tags() -> Vec<String> {
    vec!["chat".to_string()]
}

/// 新建模型请求
#[derive(Debug, Deserialize)]
pub struct CreateModelRequest {
    pub name: String,
    /// API 模型 ID，如 "deepseek-chat"
    pub model: String,
    #[serde(default)]
    pub status: Status,
    /// 能力标签（多选）：chat / embedding / rerank / reasoning / vision ...
    /// 一个模型可同时具备多能力（如 `["chat","reasoning"]` 推理模型）。
    #[serde(default = "default_tags")]
    pub tags: Vec<String>,
    /// embedding 维度（仅 tags 含 embedding 时有意义，如 bge-m3=1024、nomic-embed-text=768）
    #[serde(default)]
    pub embedding_dimensions: Option<i32>,
    /// 上下文窗口（token），用于动态压缩阈值；空=回退默认
    #[serde(default)]
    pub context_window: Option<i32>,
}

/// 更新模型请求
#[derive(Debug, Deserialize)]
pub struct UpdateModelRequest {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default = "default_tags")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub embedding_dimensions: Option<i32>,
    /// 上下文窗口（token），用于动态压缩阈值；空=回退默认
    #[serde(default)]
    pub context_window: Option<i32>,
}

/// 模型响应
#[derive(Debug, Serialize, Clone)]
pub struct ModelResponse {
    pub id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub vendor_name: String,
    pub protocol: ProviderProtocol,
    pub name: String,
    pub model: String,
    pub is_default: bool,
    pub status: Status,
    /// 能力标签
    pub tags: Vec<String>,
    pub embedding_dimensions: Option<i32>,
    /// 上下文窗口（token），用于动态压缩阈值；空=回退默认
    pub context_window: Option<i32>,
    pub embedding_default: bool,
    pub created_at: String,
    pub updated_at: String,
}

// ========== 会话模型下拉用（/api/models 兼容） ==========

/// 会话模型选择项
#[derive(Debug, Serialize)]
pub struct ModelOptionResponse {
    pub id: String,
    pub name: String,
    pub model: String,
    pub provider_name: String,
    pub vendor_name: String,
    pub protocol: ProviderProtocol,
    pub is_default: bool,
    /// 1=启用，0=禁用（含供应商禁用）。前端据此禁选但可见。
    pub status: i16,
    /// 能力标签（知识库实例创建时前端过滤 tags 含 embedding）
    pub tags: Vec<String>,
    pub embedding_default: bool,
    /// 上下文窗口（token），用于动态压缩阈值；空=回退默认
    pub context_window: Option<i32>,
}

// ========== 模型探测 ==========

/// 批量探测请求（ids 为模型 id 列表）
#[derive(Debug, Deserialize)]
pub struct ProbeModelsInput {
    pub ids: Vec<String>,
}
