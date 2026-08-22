//! Rhai 监控插件实例 —— 对标 nm `LoadedMonitorPlugin`
//!
//! 与 nm 的差异：
//! - nm：从 `.so` 文件 libloading 加载，调用 FFI 符号
//! - cortex：从源码字符串编译为 AST，调用顶层函数
//!
//! 与上层 HTTP API 的契约保持一致：`prepare_oids()` / `parse(json)` 都返回 JSON 字符串。

use std::sync::Arc;

use anyhow::{Context, Result};
use rhai::{AST, Engine, Scope};

use super::host_fns::register_host_functions;
use super::types::MonitorResult;

/// 单个 Rhai 监控插件实例
pub struct RhaiMonitorPlugin {
    plugin_id: String,
    platform: String,
    version: u32,
    source_code: String,
    engine: Arc<Engine>,
    ast: AST,
}

impl RhaiMonitorPlugin {
    /// 脚本跨平台，统一返回 "any"，跳过上层平台校验
    pub fn platform_placeholder() -> &'static str {
        "any"
    }

    /// 编译 Rhai 脚本为 AST
    ///
    /// 编译失败返回 anyhow::Error，调用方决定降级策略。
    pub fn compile(plugin_id: impl Into<String>, source: &str, version: u32) -> Result<Self> {
        let id: String = plugin_id.into();
        let mut engine = Engine::new();
        apply_safety_limits(&mut engine);
        register_host_functions(&mut engine);

        let ast = engine
            .compile(source)
            .with_context(|| format!("rhai 编译失败 (plugin_id={id})"))?;

        Ok(Self {
            plugin_id: id,
            platform: Self::platform_placeholder().to_string(),
            version,
            source_code: source.to_string(),
            engine: Arc::new(engine),
            ast,
        })
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn source_code(&self) -> &str {
        &self.source_code
    }

    /// 准备阶段：返回 OID 列表 JSON 字符串
    pub fn prepare_oids(&self) -> String {
        let mut scope = Scope::new();
        match self
            .engine
            .call_fn::<String>(&mut scope, &self.ast, "prepare_oids", ())
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "[rhai-plugin] prepare_oids failed (plugin_id={}): {e}",
                    self.plugin_id
                );
                "[]".to_string()
            }
        }
    }

    /// 解析阶段：传入 OID 值 JSON 字符串，返回解析结果 JSON 字符串
    ///
    /// 失败时返回单个 `MonitorResult::err` 的 JSON 数组，保证上层反序列化不崩。
    pub fn parse(&self, oid_values_json: &str) -> String {
        let mut scope = Scope::new();
        let result: Result<String, _> = self.engine.call_fn::<String>(
            &mut scope,
            &self.ast,
            "parse",
            (oid_values_json.to_string(),),
        );
        match result {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "[rhai-plugin] parse failed (plugin_id={}): {e}",
                    self.plugin_id
                );
                let r = MonitorResult::err(format!("rhai parse failed: {e}"));
                serde_json::to_string(&vec![r]).unwrap_or_else(|_| "[]".to_string())
            }
        }
    }

    /// 仅做语法检查（不实际执行）—— 供 validate tool 的 Layer1 使用
    pub fn check_syntax(source: &str) -> std::result::Result<(), String> {
        let mut engine = Engine::new();
        apply_safety_limits(&mut engine);
        register_host_functions(&mut engine);
        engine
            .compile(source)
            .map(|_| ())
            .map_err(|e| format!("{e}"))
    }
}

/// 给 Engine 设置安全限制，防止 LLM 生成的恶意/错误脚本拖垮进程。
/// 同时被 L1（进程内）和 L2（进程内 spawn_blocking）复用，避免两处规则漂移。
pub fn apply_safety_limits(engine: &mut Engine) {
    engine.set_max_expr_depths(64, 64);
    engine.set_max_call_levels(50);
    engine.set_max_operations(1_000);
    engine.set_max_string_size(1_000_000);
    engine.set_max_array_size(10_000);
    engine.set_max_map_size(10_000);
}

// ─── 编译期编译类型擦除的辅助 trait，避免泛型 T: Send+Sync+Variant 约束 ─────
//
// 由于 Rhai 的 `Engine::call_fn` 返回 `Result<T, Box<EvalAltResult>>`，
// 而 `Err` 分支在我们关心的路径上不发生（被 unwrap_or 兜底），
// 用一个适配层把结果统一为 Option<String>。

impl RhaiMonitorPlugin {
    /// 内部辅助：把 call_fn 的 Result 转成 Option<T>（失败/None 都返回 None）
    fn _call_top_fn_opt(&self, name: &str, args: String) -> Option<String> {
        let mut scope = Scope::new();
        match name {
            "prepare_oids" => self
                .engine
                .call_fn::<String>(&mut scope, &self.ast, "prepare_oids", ())
                .ok(),
            "parse" => self
                .engine
                .call_fn::<String>(&mut scope, &self.ast, "parse", (args,))
                .ok(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYS_UPTIME_SCRIPT: &str = r#"
        fn prepare_oids() {
            `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get","label":"sysUpTime"}]`
        }

        fn parse(oid_values_json) {
            let map = parse_json(oid_values_json);
            let raw = get_num(map, ".1.3.6.1.2.1.1.3.0");
            if raw.is_none() {
                return `[{"success":false,"errors":["sysUpTime missing"]}]`;
            }
            let seconds = raw.unwrap() / 100.0;
            `[{"success":true,"value":{"number":${seconds}},"label":"sysUpTime"}]`
        }
    "#;

    #[test]
    fn compile_success() {
        let p = RhaiMonitorPlugin::compile("test-sysuptime", SYS_UPTIME_SCRIPT, 1);
        assert!(p.is_ok(), "compile failed: {:?}", p.err());
    }

    #[test]
    fn compile_syntax_error() {
        let bad = "fn prepare_oids( { [] }";
        let p = RhaiMonitorPlugin::compile("bad", bad, 1);
        assert!(p.is_err());
    }

    #[test]
    fn check_syntax_ok() {
        assert!(RhaiMonitorPlugin::check_syntax(SYS_UPTIME_SCRIPT).is_ok());
    }

    #[test]
    fn check_syntax_fail() {
        assert!(RhaiMonitorPlugin::check_syntax("fn broken(").is_err());
    }

    #[test]
    fn prepare_oids_returns_json() {
        let p = RhaiMonitorPlugin::compile("t", SYS_UPTIME_SCRIPT, 1).unwrap();
        let json = p.prepare_oids();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v[0]["oid"], ".1.3.6.1.2.1.1.3.0");
        assert_eq!(v[0]["method"], "get");
    }

    #[test]
    fn parse_returns_correct_value() {
        let p = RhaiMonitorPlugin::compile("t", SYS_UPTIME_SCRIPT, 1).unwrap();
        let input =
            r#"{".1.3.6.1.2.1.1.3.0":{"oid_value_type":2,"value_str":"","value_num":123456}}"#;
        let out = p.parse(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["success"], true);
        assert_eq!(v[0]["value"]["number"], 1234.56);
        assert_eq!(v[0]["label"], "sysUpTime");
    }

    #[test]
    fn parse_handles_missing_oid() {
        let p = RhaiMonitorPlugin::compile("t", SYS_UPTIME_SCRIPT, 1).unwrap();
        let out = p.parse("{}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["success"], false);
    }
}
