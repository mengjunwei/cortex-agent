//! 监控插件 API — 已迁移至 GraphQL
//!
//! GraphQL 字段（与 nm `/monitor/*` 行为对齐，但改用 cortex-agent 进程内 Rhai 引擎）：
//!
//! | 字段 | 类型 | 说明 |
//! |------|------|------|
//! | `registerMonitorPlugin` | Mutation | 注册 Rhai 监控插件（script body） |
//! | `unregisterMonitorPlugin` | Mutation | 注销插件 |
//! | `monitorPlugins` | Query | 列出所有插件（含版本信息） |
//! | `monitorPlugin(pluginId)` | Query | 获取插件详情（含源码） |
//! | `monitorPluginVersions(pluginId)` | Query | 列出版本历史 |
//! | `rollbackMonitorPlugin` | Mutation | 回滚到指定版本 |

use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::AppState;

use super::response;
use super::response::code;

/// 脚本源码最大字节数（64 KB），防止超大脚本造成内存压力
const MAX_SCRIPT_BYTES: usize = 65_536;

/// 校验 plugin_id 格式：1-64 个字符，仅允许 [a-zA-Z0-9_-]
fn validate_plugin_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("plugin_id 不能为空".to_string());
    }
    if id.len() > 64 {
        return Err(format!("plugin_id 过长（{} > 64 字符）", id.len()));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("plugin_id 仅允许字母、数字、下划线、连字符".to_string());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub plugin_id: String,
    #[serde(default)]
    pub description: String,
    pub script: String,
    #[serde(default)]
    pub change_description: String,
}

pub async fn register(state: &AppState, req: RegisterRequest) -> Value {
    if let Err(msg) = validate_plugin_id(&req.plugin_id) {
        return response::err(code::INVALID_PARAMS, msg);
    }
    if req.script.len() > MAX_SCRIPT_BYTES {
        return response::err(
            code::INVALID_PARAMS,
            format!(
                "script 过大（{} > {} 字节）",
                req.script.len(),
                MAX_SCRIPT_BYTES
            ),
        );
    }

    let mgr = &state.plugin_manager;
    match mgr
        .register(
            &req.plugin_id,
            &req.description,
            &req.script,
            &req.change_description,
        )
        .await
    {
        Ok((final_id, version)) => {
            tracing::info!(
                "[monitor-api] registered {} (requested: {}) v{}",
                final_id,
                req.plugin_id,
                version
            );
            response::ok(json!({ "plugin_id": final_id, "version": version }))
        }
        Err(e) => {
            tracing::warn!("[monitor-api] register {} failed: {e}", req.plugin_id);
            response::err(code::BUSINESS, e.to_string())
        }
    }
}

pub async fn unregister(state: &AppState, plugin_id: &str) -> Value {
    if let Err(msg) = validate_plugin_id(plugin_id) {
        return response::err(code::INVALID_PARAMS, msg);
    }
    let removed = state.plugin_manager.unregister(plugin_id).await;
    response::ok(json!({ "removed": removed }))
}

pub async fn list(state: &AppState) -> Value {
    let plugins = state.plugin_manager.list().await;
    response::ok(json!({ "plugins": plugins, "count": plugins.len() }))
}

pub async fn get_plugin(state: &AppState, plugin_id: &str) -> Value {
    if let Err(msg) = validate_plugin_id(plugin_id) {
        return response::err(code::INVALID_PARAMS, msg);
    }
    match state.plugin_manager.get_plugin_info(plugin_id).await {
        Some(info) => response::ok(json!({ "plugin": info })),
        None => response::err(code::NOT_FOUND, format!("plugin {} not found", plugin_id)),
    }
}

pub async fn list_versions(state: &AppState, plugin_id: &str) -> Value {
    if let Err(msg) = validate_plugin_id(plugin_id) {
        return response::err(code::INVALID_PARAMS, msg);
    }
    let versions = state.plugin_manager.list_versions(plugin_id).await;
    let active = state.plugin_manager.get_active_version(plugin_id);
    response::ok(json!({
        "plugin_id": plugin_id,
        "active_version": active,
        "versions": versions
    }))
}

pub async fn rollback(state: &AppState, plugin_id: &str, version: u32) -> Value {
    if let Err(msg) = validate_plugin_id(plugin_id) {
        return response::err(code::INVALID_PARAMS, msg);
    }
    match state.plugin_manager.rollback(plugin_id, version).await {
        Ok(active_version) => {
            tracing::info!(
                "[monitor-api] rollback plugin {} → active_version=v{}",
                plugin_id,
                active_version
            );
            response::ok(json!({ "version": active_version }))
        }
        Err(e) => response::err(code::BUSINESS, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::PluginManager;

    fn sample_script() -> &'static str {
        r#"
        fn prepare_oids() { `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]` }
        fn parse(j) {
            let m = parse_json(j);
            let n = get_num(m, ".1.3.6.1.2.1.1.3.0");
            if n.is_none() { return `[{"success":false,"errors":["x"]}]`; }
            `[{"success":true,"value":{"number":${n.unwrap()}}}]`
        }
        "#
    }

    #[test]
    fn manager_round_trip() {
        let mgr = PluginManager::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (id, v) = rt
            .block_on(mgr.register("p1", "", sample_script(), ""))
            .unwrap();
        assert_eq!(v, 1);
        let po = mgr.prepare_oids(&id);
        assert!(po.contains(".1.3.6.1.2.1.1.3.0"));
        let out = mgr.parse(
            &id,
            r#"{".1.3.6.1.2.1.1.3.0":{"oid_value_type":2,"value_str":"","value_num":42.0}}"#,
        );
        assert!(out.contains("42"));
    }

    #[test]
    fn validate_plugin_id_accepts_valid_and_rejects_invalid() {
        assert!(validate_plugin_id("my-plugin_1").is_ok());
        assert!(validate_plugin_id("").is_err());
        assert!(validate_plugin_id("bad id!").is_err());
        assert!(validate_plugin_id(&"x".repeat(65)).is_err());
    }
}
