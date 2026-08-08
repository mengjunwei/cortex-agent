//! 监控插件数据结构 —— 与 nm `nm-plugin-api` 完全对齐
//!
//! 复用 nm 项目的数据契约，保证 cortex-agent 生成的 Rhai 插件
//! 既能在本进程内执行，也能在迁移到 nm 后端时无缝对接。
//!
//! 参考：nm `crates/nm-plugin-api/src/monitor.rs`

use serde::{Deserialize, Serialize};

/// OID 获取方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OidMethod {
    /// snmpget — 精确获取单个值
    #[serde(rename = "get")]
    Get,
    /// snmpwalk — 遍历获取表结构数据
    #[serde(rename = "walk")]
    Walk,
}

impl OidMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            OidMethod::Get => "get",
            OidMethod::Walk => "walk",
        }
    }
}

/// 单个 OID 描述（`prepare_oids` 返回值的元素）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidItem {
    /// OID 标识符，如 "1.3.6.1.2.1.1.1.0"
    pub oid: String,
    /// 该 OID 的获取方式
    pub method: OidMethod,
    /// 可选别名/说明
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// 单个 OID 获取后的值（作为 map 的 value，key 为 oid）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidValue {
    /// oid 值类型：1=字符串, 2=数字
    pub oid_value_type: i8,
    /// 字符串值
    #[serde(rename = "value_str")]
    pub value_str: String,
    /// 数字值
    #[serde(rename = "value_num")]
    pub value_num: f64,
}

/// 监控项解析结果值
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MonitorValue {
    #[serde(rename = "number")]
    Number(f64),
    #[serde(rename = "string")]
    String(String),
}

/// 监控项解析结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorResult {
    pub success: bool,
    #[serde(rename = "value", skip_serializing_if = "Option::is_none")]
    pub value: Option<MonitorValue>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl MonitorResult {
    pub fn ok(value: MonitorValue) -> Self {
        Self {
            success: true,
            value: Some(value),
            label: String::new(),
            errors: vec![],
        }
    }

    pub fn ok_with_label(value: MonitorValue, label: impl Into<String>) -> Self {
        Self {
            success: true,
            value: Some(value),
            label: label.into(),
            errors: vec![],
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            value: None,
            label: String::new(),
            errors: [msg.into()].to_vec(),
        }
    }
}

/// 监控项解析插件 trait（仅用于本地静态类型约束）
///
/// Rhai 脚本无需实现此 trait —— 它对应 Rhai 脚本里的两个顶层函数
/// `prepare_oids()` 和 `parse(json)`。
pub trait MonitorPlugin: Send + Sync {
    fn prepare_oids(&self) -> String;
    fn parse(&self, oid_values_json: &str) -> String;
}
