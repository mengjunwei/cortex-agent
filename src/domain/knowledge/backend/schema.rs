//! Provider 配置字段声明（驱动前端动态表单 — 差异化整合的关键）。
//!
//! 每个 provider 声明自己需要的配置字段（[`ConfigFieldSpec`]），后端 `kbProviderSchema`
//! 接口返回给前端，前端通用动态表单按 schema 渲染。新增 provider 类型时只需在此加一份
//! schema 声明 + 实现 trait，前端零改动。
//!
//! secret 字段（如 Dify api_key）入库前由 [`encrypt_secret_fields`] 加密、读取时由
//! [`decrypt_secret_fields`] 解密。

use crate::error::AppError;
use crate::security::crypto::AesCodec;

use super::ProviderKind;

/// 字段类型（前端据此渲染 input/password/number/url/select）
#[derive(Debug, Clone, Copy)]
pub enum FieldType {
    Text,
    Secret,
    Number,
    Url,
    Select,
}

/// 配置字段规格
#[derive(Debug, Clone)]
pub struct ConfigFieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub field_type: FieldType,
    pub required: bool,
    pub default: Option<&'static str>,
    pub placeholder: Option<&'static str>,
    pub help: Option<&'static str>,
}

impl ConfigFieldSpec {
    pub fn is_secret(&self) -> bool {
        matches!(self.field_type, FieldType::Secret)
    }
}

/// Dify provider 配置字段
pub static DIFY_SCHEMA: &[ConfigFieldSpec] = &[
    ConfigFieldSpec {
        key: "base_url",
        label: "Dify Base URL",
        field_type: FieldType::Url,
        required: true,
        default: Some("https://api.dify.ai/v1"),
        placeholder: Some("https://your-dify-host/v1"),
        help: Some("Dify 服务地址，需含 /v1 版本前缀"),
    },
    ConfigFieldSpec {
        key: "api_key",
        label: "API Key",
        field_type: FieldType::Secret,
        required: true,
        default: None,
        placeholder: Some("Dify 知识库 API Key（dataset-scoped）"),
        help: Some("在 Dify 知识库「API」页获取，加密存储"),
    },
    ConfigFieldSpec {
        key: "secret_key",
        label: "SECRET_KEY",
        field_type: FieldType::Secret,
        required: false,
        default: None,
        placeholder: Some("Dify 服务端 SECRET_KEY（.env）"),
        help: Some(
            "用于知识库文档图片预览签名（HMAC-SHA256）；取 Dify 部署 .env 中的 SECRET_KEY，加密存储。\
             未填则文档图片无法显示",
        ),
    },
    ConfigFieldSpec {
        key: "dataset_id",
        label: "Dataset ID",
        field_type: FieldType::Text,
        required: true,
        default: None,
        placeholder: Some("Dify 知识库 ID"),
        help: Some("Dify 知识库的 dataset id"),
    },
    ConfigFieldSpec {
        key: "top_k",
        label: "检索 Top K",
        field_type: FieldType::Number,
        required: false,
        default: Some("5"),
        placeholder: None,
        help: Some("检索返回条数，默认 5"),
    },
];

/// 内置 provider 配置字段
pub static BUILTIN_SCHEMA: &[ConfigFieldSpec] = &[
    ConfigFieldSpec {
        key: "embedding_model_id",
        label: "Embedding 模型",
        field_type: FieldType::Select,
        required: true,
        default: None,
        placeholder: Some("选择 purpose=embedding 的模型"),
        help: Some("来自「模型供应商管理」中含 embedding 标签的模型；选项由前端动态拉取"),
    },
    ConfigFieldSpec {
        key: "chunk_size",
        label: "切片大小",
        field_type: FieldType::Number,
        required: false,
        default: Some("1024"),
        placeholder: None,
        help: Some("单切片最大字符数，默认 1024"),
    },
    ConfigFieldSpec {
        key: "chunk_overlap",
        label: "切片重叠",
        field_type: FieldType::Number,
        required: false,
        default: Some("100"),
        placeholder: None,
        help: Some("相邻切片重叠字符数，默认 100"),
    },
    ConfigFieldSpec {
        key: "top_k",
        label: "检索 Top K",
        field_type: FieldType::Number,
        required: false,
        default: Some("6"),
        placeholder: None,
        help: Some("检索返回条数，默认 6"),
    },
    ConfigFieldSpec {
        key: "similarity_threshold",
        label: "相似度阈值",
        field_type: FieldType::Number,
        required: false,
        default: Some("0.35"),
        placeholder: None,
        help: Some("低于此分数的结果过滤，默认 0.35"),
    },
];

/// 按 provider kind 取配置 schema
pub fn schema_for(kind: ProviderKind) -> &'static [ConfigFieldSpec] {
    match kind {
        ProviderKind::Dify => DIFY_SCHEMA,
        ProviderKind::Builtin => BUILTIN_SCHEMA,
    }
}

/// 从 config JSON 取字符串字段（trim + 去空）
pub fn get_str(cfg: &serde_json::Value, key: &str) -> Option<String> {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 从 config JSON 取数字字段
pub fn get_u64(cfg: &serde_json::Value, key: &str) -> Option<u64> {
    cfg.get(key).and_then(|v| v.as_u64())
}

/// 校验 config（必填字段非空）
pub fn validate_config(kind: ProviderKind, cfg: &serde_json::Value) -> Result<(), AppError> {
    for spec in schema_for(kind) {
        if spec.required {
            let val = cfg.get(spec.key).and_then(|v| v.as_str()).unwrap_or("");
            if val.trim().is_empty() {
                return Err(AppError::BusinessError(format!(
                    "知识库配置缺少必填字段：{}",
                    spec.label
                )));
            }
        }
    }
    Ok(())
}

/// 加密 config 中的 secret 字段（入库前调用）。返回新的 JSON 字符串。
pub fn encrypt_secret_fields(
    kind: ProviderKind,
    cfg: &serde_json::Value,
    codec: &AesCodec,
) -> Result<String, AppError> {
    let mut obj = match cfg {
        serde_json::Value::Object(m) => m.clone(),
        _ => {
            return Err(AppError::BusinessError(
                "知识库 config 必须是 JSON 对象".into(),
            ));
        }
    };
    for spec in schema_for(kind) {
        if spec.is_secret() {
            if let Some(plain) = obj.get(spec.key).and_then(|v| v.as_str()) {
                if !plain.is_empty() {
                    let enc = codec.encrypt(plain).map_err(|e| {
                        AppError::BusinessError(format!("加密字段 {} 失败: {e}", spec.key))
                    })?;
                    obj.insert(spec.key.into(), serde_json::Value::String(enc));
                }
            }
        }
    }
    serde_json::to_string(&obj).map_err(|e| AppError::SerializationError(e.to_string()))
}

/// 解密 config 中的 secret 字段（读取后调用）。
///
/// `mask` 为 true 时用末 4 位掩码替代明文（供前端编辑回显，不泄露明文）。
pub fn decrypt_secret_fields(
    kind: ProviderKind,
    cfg: &serde_json::Value,
    codec: &AesCodec,
    mask: bool,
) -> serde_json::Value {
    let mut obj = match cfg {
        serde_json::Value::Object(m) => m.clone(),
        other => return other.clone(),
    };
    for spec in schema_for(kind) {
        if spec.is_secret() {
            if let Some(enc) = obj
                .get(spec.key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
            {
                if !enc.is_empty() {
                    let shown = if mask {
                        mask_secret(&enc)
                    } else {
                        codec.decrypt(&enc).unwrap_or_default()
                    };
                    obj.insert(spec.key.into(), serde_json::Value::String(shown));
                }
            }
        }
    }
    serde_json::Value::Object(obj)
}

fn mask_secret(s: &str) -> String {
    let len = s.chars().count();
    if len <= 4 {
        "****".to_string()
    } else {
        s.chars().skip(len - 4).collect()
    }
}
