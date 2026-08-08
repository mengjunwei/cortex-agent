//! 监控项插件助手 Agent 构建模块
//!
//! 根据用户需求生成 Rust 监控插件代码，用于网络设备运行状态监控。
//! 支持的监控类型包括 CPU/内存/流量利用率、SNMP OID、告警阈值等。

use std::sync::Arc;

use crate::agent::runtime::cortex_agent::CortexAgentBuilder;
use adk_rust::agent::Agent;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::infra::db::DbPool;
use crate::infra::redis::SharedRedisPool;
use crate::llm::{make_gen_config_from, make_model_by_id};
use crate::model_provider::store::ModelProviderStore;
use crate::monitor::PluginManager;

/// 构建监控项插件助手 Agent
pub fn build_monitor_plugin_agent(
    cfg: &AppConfig,
    model_store: &ModelProviderStore,
) -> anyhow::Result<Arc<dyn Agent>> {
    build_monitor_plugin_agent_with_model(cfg, model_store, None, None, None, None, None, CancellationToken::new())
}

pub fn build_monitor_plugin_agent_with_model(
    cfg: &AppConfig,
    model_store: &ModelProviderStore,
    model_id: Option<&str>,
    thinking_level: Option<&str>,
    db_pool: Option<DbPool>,
    redis_pool: Option<SharedRedisPool>,
    plugin_manager: Option<Arc<PluginManager>>,
    cancel_token: CancellationToken,
) -> anyhow::Result<Arc<dyn Agent>> {
    let model = make_model_by_id(model_store, model_id)?;
    let tools = crate::tools::monitor_plugin::create_monitor_plugin_tools(
        cfg,
        db_pool,
        redis_pool,
        plugin_manager,
    );
    let mut builder = CortexAgentBuilder::new("MonitorPluginAgent")
        .description("监控项插件助手")
        .instruction(crate::tools::monitor_plugin::get_system_prompt())
        .model(model)
        .generate_content_config(make_gen_config_from(None, None, None, thinking_level))
        .cancel_token(cancel_token);
    for tool in tools {
        builder = builder.tool(Arc::new(tool));
    }
    let agent = builder
        .build()
        .map_err(|e| anyhow::anyhow!("创建 MonitorPluginAgent 失败: {}", e))?;
    Ok(Arc::new(agent))
}
