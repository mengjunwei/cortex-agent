//! Agent 工具模块 — 定义各 Agent 可调用的 FunctionTool 及工具输出治理
//!
//! ## 工具定义（FunctionTool）
//!
//! | 模块 | 工具 | 说明 |
//! |------|------|------|
//! | [`device_command`] | `search_kb`、`query_device_catalog`、`lookup_device_id`、`snmp_test_collect` | 知识库检索 + 设备目录查询 + 设备 ID 反查 + SNMP 采集 |
//! | [`monitor_plugin`] | `validate_monitor_plugin`、`register_monitor_plugin` | Rhai 监控插件生成 system prompt + 三层校验 / 注册 |
//!
//! ## 工具输出治理
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`truncating`] | `TruncatingToolset` — 包装工具集，限制单工具输出字节数（`context.tool_max_output_bytes`） |
//! | [`filter`] | 语义过滤器 — 按工具家族（表格 / Markdown / grep）在硬截断前做结构化压缩 |
//! | [`redact`] | 敏感信息脱敏（密码 / Token 等） |
//! | [`registry`] | 工具注册表 — 自定义助手可勾选的通用工具白名单（`GET` 工具清单的数据源） |
//!
//! 自定义助手经 [`registry`] 白名单勾选工具；专业工具（监控校验、浏览器 MCP、头脑风暴）
//! 仅内置助手可用。[`wrap_toolset_with_truncation`] 为所有工具集统一套上输出上限与语义过滤。

use std::sync::Arc;

use crate::infra::object_store::ObjectStore;

pub mod code;
pub mod device_command;
pub mod filter;
pub mod monitor_plugin;
pub mod propose_memory;
pub mod redact;
pub mod registry;
pub mod screenshot;
pub mod shell_command;
pub mod skill_read;
pub mod truncating;

/// 用 `TruncatingToolset` 包装工具集，限制单工具输出大小
pub fn wrap_toolset_with_truncation(
    toolset: Option<Arc<dyn adk_rust::Toolset>>,
    tool_max_output_bytes: usize,
    object_store: Option<Arc<ObjectStore>>,
) -> Option<Arc<dyn adk_rust::Toolset>> {
    toolset.map(|ts| {
        let mut wrapped =
            truncating::TruncatingToolset::new(ts).with_max_output_bytes(tool_max_output_bytes);
        if let Some(os) = object_store {
            wrapped = wrapped.with_object_store(os);
        }
        Arc::new(wrapped) as Arc<dyn adk_rust::Toolset>
    })
}
