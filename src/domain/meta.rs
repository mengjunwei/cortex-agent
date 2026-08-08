//! 设备元数据定义模块
//!
//! 定义知识库文档的完整元数据结构，用于 API 响应和内部传递。

use crate::domain::enum_def::{OpType, RiskLevel};
use serde::{Deserialize, Serialize};

/// 设备文档完整元数据 — 包含内容，用于 API 响应
///
/// 一个 `DeviceMeta` 对应知识库中的一条文档记录，包含：
/// - 设备信息：厂商、设备类型、固件版本
/// - 文档内容：doc_id、标题、正文内容
/// - 风险控制：操作类型、风险等级、命令标签
/// - 质量指标：质量评分、点赞/点踩数、权重、访问统计
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceMeta {
    pub brand: String,
    pub dev_type: String,
    pub firmware_ver: String,

    pub doc_id: String,
    pub title: String,
    pub content: String,

    pub op_type: OpType,
    pub risk_level: RiskLevel,
    pub cmd_tags: Vec<String>,

    pub create_at: i64,
    pub last_access_at: i64,
    pub access_count: u32,
    pub expire_at: Option<i64>,
    pub quality_score: u8,
    pub is_deleted: bool,

    pub like_count: u32,
    pub dislike_count: u32,
    pub weight: f32,
    pub feedback_status: String,
    pub feedback_note: String,
    pub doc_source: String,
}
