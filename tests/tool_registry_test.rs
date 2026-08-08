//! 工具注册表集成测试
//!
//! 验证：
//! - `custom_options()` 只暴露 `custom_enabled = true` 的工具
//! - `sanitize_custom_tools()` 过滤未知 / 仅内置 / 重复 key

use cortex_agent::tools::registry;

#[test]
fn custom_options_excludes_builtin_only_tools() {
    let keys: Vec<&str> = registry::custom_options().iter().map(|t| t.key).collect();
    assert!(keys.contains(&"search_kb"));
    assert!(keys.contains(&"query_device_catalog"));
    assert!(!keys.contains(&"browser"));
    assert!(!keys.contains(&"validate_monitor_plugin"));
}

#[test]
fn sanitize_filters_invalid_and_builtin_only() {
    let out =
        registry::sanitize_custom_tools(&["search_kb".into(), "browser".into(), "bogus".into()]);
    assert_eq!(out, vec!["search_kb".to_string()]);
}

#[test]
fn sanitize_dedupes_kept_keys() {
    let out = registry::sanitize_custom_tools(&[
        "search_kb".into(),
        "search_kb".into(),
        "shell_command".into(),
    ]);
    assert_eq!(
        out,
        vec!["search_kb".to_string(), "shell_command".to_string()]
    );
}

#[test]
fn registry_keys_are_unique() {
    let keys: Vec<&str> = registry::registry().iter().map(|t| t.key).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    let dup_count = sorted.windows(2).filter(|w| w[0] == w[1]).count();
    assert_eq!(dup_count, 0, "duplicate tool keys: {:?}", keys);
}
