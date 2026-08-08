//! 助手枚举集成测试（参照 tests/enum_test.rs 范式）。
use cortex_agent::domain::assistant::enums::{AgentType, AssistantKind, Visibility};

#[test]
fn kind_serde_is_i16() {
    let json = serde_json::to_string(&AssistantKind::Custom).unwrap();
    assert_eq!(json, "1");
    let parsed: AssistantKind = serde_json::from_str("0").unwrap();
    assert_eq!(parsed, AssistantKind::Builtin);
}

#[test]
fn agent_type_dispatch_key_matches_router() {
    // 注：Auto(0) 和 Chat(1) 已废弃删除，旧数据统一按 Custom(9) 处理
    assert_eq!(AgentType::DeviceCommand.dispatch_key(), "device_command");
    assert_eq!(AgentType::MonitorPlugin.dispatch_key(), "monitor_plugin");
    assert_eq!(AgentType::Custom.dispatch_key(), "custom");
}

#[test]
fn agent_type_serde_is_i16() {
    assert_eq!(serde_json::to_string(&AgentType::Custom).unwrap(), "9");
    let parsed: AgentType = serde_json::from_str("2").unwrap();
    assert_eq!(parsed, AgentType::DeviceCommand);
}

#[test]
fn visibility_serde_is_i16() {
    let json = serde_json::to_string(&Visibility::Shared).unwrap();
    assert_eq!(json, "1");
    let parsed: Visibility = serde_json::from_str("2").unwrap();
    assert_eq!(parsed, Visibility::Builtin);
}

#[test]
fn visibility_is_public() {
    assert!(Visibility::Shared.is_public());
    assert!(Visibility::Builtin.is_public());
    assert!(!Visibility::Private.is_public());
}
