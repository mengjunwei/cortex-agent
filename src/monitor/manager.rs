use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Result;
use serde::Serialize;

use crate::monitor::plugin_store::PluginStore;
use crate::monitor::rhai_plugin::RhaiMonitorPlugin;

/// 判断字符串是否为合法 UUID v7 格式
fn is_uuid_v7(s: &str) -> bool {
    match uuid::Uuid::parse_str(s) {
        Ok(u) => u.get_version_num() == 7,
        Err(_) => false,
    }
}

/// 规范化 plugin_id：传入的是 UUID v7 则直接使用，否则自动生成 UUID v7
fn normalize_plugin_id(input: &str) -> String {
    if is_uuid_v7(input) {
        input.to_string()
    } else {
        uuid::Uuid::now_v7().to_string()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginVersionInfo {
    pub version: u32,
    pub source_code: String,
    pub change_description: String,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginListItem {
    pub plugin_id: String,
    pub version: u32,
    pub description: String,
    pub enabled: bool,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub plugin_id: String,
    pub version: u32,
    pub platform: String,
    pub source_code: String,
    pub description: String,
    pub enabled: bool,
}

/// 监控插件管理器
///
/// 内存缓存 + 数据库持久化双写模式。
/// - 运行时操作走内存缓存（`RwLock<HashMap>`）
/// - 注册/注销/回滚时同步写入数据库
/// - 启动时从数据库加载所有激活插件
pub struct PluginManager {
    plugins: RwLock<HashMap<String, RhaiMonitorPlugin>>,
    store: Option<Arc<PluginStore>>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self {
            plugins: RwLock::new(HashMap::new()),
            store: None,
        }
    }

    pub fn with_store(store: Arc<PluginStore>) -> Self {
        Self {
            plugins: RwLock::new(HashMap::new()),
            store: Some(store),
        }
    }

    fn read_plugins(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, RhaiMonitorPlugin>> {
        self.plugins.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_plugins(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, RhaiMonitorPlugin>> {
        self.plugins.write().unwrap_or_else(|e| e.into_inner())
    }

    /// 从数据库加载所有激活的插件到内存缓存
    pub async fn load_from_db(&self) -> Result<()> {
        let store = match &self.store {
            Some(s) => s,
            None => return Ok(()),
        };

        let rows = store.load_all_active_plugins().await?;
        let mut map = self.write_plugins();
        map.clear();

        for row in rows {
            match RhaiMonitorPlugin::compile(&row.plugin_id, &row.source_code, row.version as u32) {
                Ok(plugin) => {
                    tracing::info!(
                        "[PluginManager] 从数据库加载插件: {} v{}",
                        row.plugin_id,
                        row.version
                    );
                    map.insert(row.plugin_id, plugin);
                }
                Err(e) => {
                    tracing::warn!(
                        "[PluginManager] 加载插件 {} v{} 编译失败: {}",
                        row.plugin_id,
                        row.version,
                        e
                    );
                }
            }
        }

        tracing::info!("[PluginManager] 从数据库加载完成，共 {} 个插件", map.len());
        Ok(())
    }

    /// 注册/覆盖一个插件
    ///
    /// 1. 校验 plugin_id：若非 UUID v7 格式则自动生成
    /// 2. 编译脚本
    /// 3. 写入数据库（版本记录 + 描述 + 变更说明 + 更新激活版本）
    /// 4. 更新内存缓存
    ///
    /// - `description`：插件整体描述（首次发布必填，后续可选）
    /// - `change_description`：本次发版的变更说明
    ///
    /// 返回 (plugin_id, version)
    pub async fn register(
        &self,
        plugin_id: impl Into<String>,
        description: &str,
        source: &str,
        change_description: &str,
    ) -> Result<(String, u32)> {
        let input_id: String = plugin_id.into();
        let id = normalize_plugin_id(&input_id);

        let current_version = {
            let map = self.read_plugins();
            map.get(&id).map(|p| p.version()).unwrap_or(0)
        };

        let new_version = current_version + 1;
        let plugin = RhaiMonitorPlugin::compile(&id, source, new_version)?;

        if let Some(ref store) = self.store {
            store
                .register_plugin(
                    &id,
                    description,
                    source,
                    new_version as i32,
                    change_description,
                )
                .await?;
        }

        let mut map = self.write_plugins();
        map.insert(id.clone(), plugin);

        Ok((id, new_version))
    }

    /// 注销插件
    pub async fn unregister(&self, plugin_id: &str) -> bool {
        if let Some(ref store) = self.store {
            match store.delete_plugin(plugin_id).await {
                Ok(removed) => {
                    self.write_plugins().remove(plugin_id);
                    return removed;
                }
                Err(e) => {
                    tracing::warn!("[PluginManager] 数据库注销失败: {e}");
                    return false;
                }
            }
        }
        self.write_plugins().remove(plugin_id).is_some()
    }

    /// 列出所有已注册插件
    pub async fn list(&self) -> Vec<PluginListItem> {
        if let Some(ref store) = self.store {
            match store.list_plugins().await {
                Ok(rows) => {
                    return rows
                        .iter()
                        .map(|r| PluginListItem {
                            plugin_id: r.plugin_id.clone(),
                            version: r.active_version.unwrap_or(0) as u32,
                            description: r.description.clone(),
                            enabled: r.enabled,
                            registered_at: r.created_at.to_rfc3339(),
                        })
                        .collect();
                }
                Err(e) => {
                    tracing::warn!("[PluginManager] 数据库查询失败: {e}");
                }
            }
        }
        self.read_plugins()
            .iter()
            .map(|(id, p)| PluginListItem {
                plugin_id: id.clone(),
                version: p.version(),
                description: String::new(),
                enabled: true,
                registered_at: String::new(),
            })
            .collect()
    }

    pub fn contains(&self, plugin_id: &str) -> bool {
        self.read_plugins().contains_key(plugin_id)
    }

    /// 获取插件详细信息
    pub async fn get_plugin_info(&self, plugin_id: &str) -> Option<PluginInfo> {
        if let Some(ref store) = self.store {
            if let Ok(Some(row)) = store.get_plugin(plugin_id).await {
                let source = if let Some(ver) = row.active_version {
                    store
                        .get_version(plugin_id, ver)
                        .await
                        .ok()
                        .flatten()
                        .map(|v| v.source_code)
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                return Some(PluginInfo {
                    plugin_id: row.plugin_id,
                    version: row.active_version.unwrap_or(0) as u32,
                    platform: "any".to_string(),
                    source_code: source,
                    description: row.description,
                    enabled: row.enabled,
                });
            }
        }

        self.read_plugins().get(plugin_id).map(|p| PluginInfo {
            plugin_id: p.plugin_id().to_string(),
            version: p.version(),
            platform: p.platform().to_string(),
            source_code: p.source_code().to_string(),
            description: String::new(),
            enabled: true,
        })
    }

    /// 列出指定插件的所有历史版本
    pub async fn list_versions(&self, plugin_id: &str) -> Vec<PluginVersionInfo> {
        if let Some(ref store) = self.store {
            match store.list_versions(plugin_id).await {
                Ok(rows) => {
                    return rows
                        .iter()
                        .map(|r| PluginVersionInfo {
                            version: r.version as u32,
                            source_code: r.source_code.clone(),
                            change_description: r.change_description.clone(),
                            registered_at: r.created_at.to_rfc3339(),
                        })
                        .collect();
                }
                Err(e) => {
                    tracing::warn!("[PluginManager] 查询版本历史失败: {e}");
                }
            }
        }
        Vec::new()
    }

    /// 回滚到指定版本（仅切换 active_version 指针，不创建新版本）
    pub async fn rollback(&self, plugin_id: &str, target_version: u32) -> Result<u32> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no store configured"))?;

        let ver = store
            .get_version(plugin_id, target_version as i32)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "version {} not found for plugin {}",
                    target_version,
                    plugin_id
                )
            })?;

        store
            .set_active_version(plugin_id, target_version as i32)
            .await?;

        let plugin = RhaiMonitorPlugin::compile(plugin_id, &ver.source_code, target_version)?;
        self.write_plugins().insert(plugin_id.to_string(), plugin);

        tracing::info!(
            "[PluginManager] rollback plugin {} → active_version={}",
            plugin_id,
            target_version
        );

        Ok(target_version)
    }

    pub fn get_active_version(&self, plugin_id: &str) -> Option<u32> {
        self.read_plugins().get(plugin_id).map(|p| p.version())
    }

    /// 调用插件的 `prepare_oids()`
    pub fn prepare_oids(&self, plugin_id: &str) -> String {
        let map = self.read_plugins();
        match map.get(plugin_id) {
            Some(p) => p.prepare_oids(),
            None => {
                tracing::warn!("[plugin-manager] plugin not found: {plugin_id}");
                "[]".to_string()
            }
        }
    }

    /// 调用插件的 `parse(json)`
    pub fn parse(&self, plugin_id: &str, oid_values_json: &str) -> String {
        let map = self.read_plugins();
        match map.get(plugin_id) {
            Some(p) => p.parse(oid_values_json),
            None => {
                tracing::warn!("[plugin-manager] plugin not found: {plugin_id}");
                let r = crate::monitor::types::MonitorResult::err(format!(
                    "plugin {plugin_id} not loaded"
                ));
                serde_json::to_string(&vec![r]).unwrap_or_else(|_| "[]".to_string())
            }
        }
    }
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        fn prepare_oids() { `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]` }
        fn parse(j) {
            let m = parse_json(j);
            let n = get_num(m, ".1.3.6.1.2.1.1.3.0");
            if n.is_none() { return `[{"success":false,"errors":["x"]}]`; }
            `[{"success":true,"value":{"number":${n.unwrap()}}}]`
        }
    "#;

    #[test]
    fn register_and_list_no_store() {
        let mgr = PluginManager::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (id, v) = rt
            .block_on(mgr.register("cpu-monitor", "CPU 利用率监控", SAMPLE, "首次发布"))
            .unwrap();
        assert_eq!(v, 1);
        assert!(mgr.contains(&id));
        assert_eq!(mgr.get_active_version(&id), Some(1));
    }

    #[test]
    fn normalize_plugin_id_generates_uuid_v7_for_non_uuid() {
        let id = normalize_plugin_id("cpu-usage-monitor");
        assert!(is_uuid_v7(&id));
    }

    #[test]
    fn normalize_plugin_id_keeps_valid_uuid_v7() {
        let uuid_v7 = uuid::Uuid::now_v7().to_string();
        let id = normalize_plugin_id(&uuid_v7);
        assert_eq!(id, uuid_v7);
    }

    #[test]
    fn register_bad_source_fails() {
        let mgr = PluginManager::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(
            rt.block_on(mgr.register("bad", "", "fn broken(", ""))
                .is_err()
        );
    }

    #[test]
    fn prepare_oids_unknown_returns_empty() {
        let mgr = PluginManager::new();
        assert_eq!(mgr.prepare_oids("nope"), "[]");
    }

    #[test]
    fn parse_routes_to_plugin() {
        let mgr = PluginManager::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (id, _) = rt.block_on(mgr.register("p1", "", SAMPLE, "")).unwrap();
        let out = mgr.parse(
            &id,
            r#"{".1.3.6.1.2.1.1.3.0":{"oid_value_type":2,"value_str":"","value_num":42.0}}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["success"], true);
        assert_eq!(v[0]["value"]["number"], 42.0);
    }
}
