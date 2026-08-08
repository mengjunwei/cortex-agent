//! 监控插件运行时模块 — 内置 Rhai 脚本引擎
//!
//! 本模块参考 nm 项目的 `LoadedMonitorPlugin` / `MonitorPluginManager` 抽象，
//! 将原先依赖 `.so`/`.dll` 动态库的监控插件机制，**完全内置**到 cortex-agent
//! 二进制中：LLM 生成 Rhai 脚本 → 进程内 Engine 解析执行，**零外部依赖**。
//!
//! ## 子模块
//!
//! | 模块 | 说明 |
//! |------|------|
//! | [`types`] | 复用 nm 的数据结构（`OidItem` / `MonitorResult` 等），与 nm HTTP API 兼容 |
//! | [`host_fns`] | 注册到 Rhai Engine 的 host function（`parse_json` / `to_json` / `get_num` 等） |
//! | [`rhai_plugin`] | `RhaiMonitorPlugin` —— 单个 Rhai 插件实例（对标 nm `LoadedMonitorPlugin`） |
//! | [`manager`] | `PluginManager` —— 按 plugin_id 索引的插件管理器（对标 nm `MonitorPluginManager`） |
//!
//! ## 工作流程
//!
//! 1. LLM 生成 Rhai 脚本（含 `prepare_oids()` 和 `parse(json)` 两个顶层函数）
//! 2. [`PluginManager::register`] 编译并缓存 AST
//! 3. HTTP `/monitor/prepare_oids` 调 `prepare_oids()` 返回 OID 列表
//! 4. 上层采集到 SNMP 值后，HTTP `/monitor/parse` 调 `parse(json)` 得到结果
//!
//! ## 安全限制
//!
//! Engine 设置了操作数 / 调用栈 / 字符串 / 数组上限，确保 LLM 生成的脚本
//! 即使有死循环、无限递归、爆内存等缺陷，也不会拖垮主进程。

pub mod host_fns;
pub mod manager;
pub mod plugin_store;
pub mod rhai_plugin;
pub mod types;

pub use host_fns::{OptFloat, OptStr, register_host_functions};
pub use manager::{PluginInfo, PluginListItem, PluginManager, PluginVersionInfo};
pub use rhai_plugin::{RhaiMonitorPlugin, apply_safety_limits};
pub use types::{MonitorPlugin, MonitorResult, MonitorValue, OidItem, OidMethod, OidValue};
