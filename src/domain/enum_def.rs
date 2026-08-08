//! 枚举定义模块 — 操作类型与风险等级
//!
//! ## 操作类型（OpType）
//!
//! | 变体 | 说明 | 示例 |
//! |------|------|------|
//! | `Query` | 只读查询 | show, display, 查询 |
//! | `Modify` | 配置变更 | create, set, 配置 |
//! | `Dangerous` | 高危操作 | delete, reset, reboot |
//!
//! ## 风险等级（RiskLevel）
//!
//! 从低到高：`Low` < `Medium` < `High` < `Extreme`

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// 操作类型 — 标识命令的危险程度分类
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpType {
    /// 只读查询操作（如 show、display）
    Query,
    /// 配置变更操作（如 create、set）
    Modify,
    /// 高危操作（如 delete、reset、reboot）
    Dangerous,
}

impl FromStr for OpType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "query" => Ok(OpType::Query),
            "modify" => Ok(OpType::Modify),
            "dangerous" => Ok(OpType::Dangerous),
            _ => Err(format!("Invalid OpType: {}", s)),
        }
    }
}

impl fmt::Display for OpType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpType::Query => write!(f, "query"),
            OpType::Modify => write!(f, "modify"),
            OpType::Dangerous => write!(f, "dangerous"),
        }
    }
}

/// 风险等级 — 用于权限控制和操作确认
///
/// 实现了 `Ord`，可以直接比较：`RiskLevel::Low < RiskLevel::Extreme`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// 低风险：只读查询，所有角色可用
    Low,
    /// 中风险：配置变更，需 Admin 及以上角色
    Medium,
    /// 高风险：批量/全局变更，需 Admin 及以上角色
    High,
    /// 极高风险：高危操作（删除、重启等），需 SuperAdmin 角色
    Extreme,
}

impl FromStr for RiskLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "low" => Ok(RiskLevel::Low),
            "medium" => Ok(RiskLevel::Medium),
            "high" => Ok(RiskLevel::High),
            "extreme" => Ok(RiskLevel::Extreme),
            _ => Err(format!("Invalid RiskLevel: {}", s)),
        }
    }
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "low"),
            RiskLevel::Medium => write!(f, "medium"),
            RiskLevel::High => write!(f, "high"),
            RiskLevel::Extreme => write!(f, "extreme"),
        }
    }
}
