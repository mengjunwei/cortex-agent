//! Rhai Engine 的 host function 注册
//!
//! 这些函数是 Rhai 脚本与 Rust 运行时之间的桥梁：
//! 脚本通过名称调用它们（如 `parse_json(s)`），实际执行 Rust 闭包。
//!
//! 一次性注册到 Engine 上，所有 AST 共享。

use rhai::{Dynamic, Engine, ImmutableString};
use serde_json::Value as JsonValue;

/// Rhai 端使用的"可空 f64"包装类型。
///
/// 背景：Rhai 会自动把 Rust 的 `Option<T>` 解包成值或 `()`，导致脚本端
/// 无法调用 `is_none()`/`unwrap()`。为了让 LLM 生成的脚本可以保持 Rust 风格
/// （与 nm 的 Rust 插件保持一致的 API），我们用一个包装类型把 Option 带回到脚本里。
#[derive(Debug, Clone, Copy)]
pub struct OptFloat(pub Option<f64>);

impl OptFloat {
    pub fn some(v: f64) -> Self {
        Self(Some(v))
    }
    pub fn none() -> Self {
        Self(None)
    }
}

/// Rhai 端使用的"可空字符串"包装类型。同 [`OptFloat`]。
#[derive(Debug, Clone)]
pub struct OptStr(pub Option<ImmutableString>);

impl OptStr {
    pub fn some(v: ImmutableString) -> Self {
        Self(Some(v))
    }
    pub fn none() -> Self {
        Self(None)
    }
}

/// 把所有监控插件可用的 host function 注册到 Engine
///
/// 已注册的函数：
/// - `parse_json(s) -> Dynamic`：解析 JSON 字符串为 Rhai 对象
/// - `to_json(d) -> String`：把任意 Rhai 值序列化为 JSON 字符串
/// - `get_num(map, oid) -> OptFloat`：从 OID map 便捷取数字
/// - `get_num_str(map, oid) -> OptStr`：从 OID map 便捷取字符串
/// - `log_info(msg)` / `log_warn(msg)` / `log_error(msg)`：日志输出
///
/// `OptFloat`/`OptStr` 提供 `is_none()`/`is_some()`/`unwrap()`/`unwrap_or()` 方法。
pub fn register_host_functions(engine: &mut Engine) {
    // ─── 注册包装类型 ─────────────────────────────
    engine.register_type::<OptFloat>();
    engine.register_fn("is_none", |x: &mut OptFloat| -> bool { x.0.is_none() });
    engine.register_fn("is_some", |x: &mut OptFloat| -> bool { x.0.is_some() });
    engine.register_fn("unwrap", |x: &mut OptFloat| -> f64 {
        x.0.unwrap_or_else(|| panic!("called `OptFloat::unwrap()` on a None"))
    });
    engine.register_fn("unwrap_or", |x: &mut OptFloat, def: f64| -> f64 {
        x.0.unwrap_or(def)
    });

    engine.register_type::<OptStr>();
    engine.register_fn("is_none", |x: &mut OptStr| -> bool { x.0.is_none() });
    engine.register_fn("is_some", |x: &mut OptStr| -> bool { x.0.is_some() });
    engine.register_fn("unwrap", |x: &mut OptStr| -> ImmutableString {
        x.0.clone()
            .unwrap_or_else(|| panic!("called `OptStr::unwrap()` on a None"))
    });
    engine.register_fn(
        "unwrap_or",
        |x: &mut OptStr, def: ImmutableString| -> ImmutableString { x.0.clone().unwrap_or(def) },
    );

    // ─── parse_json / to_json ─────────────────────
    engine.register_fn("parse_json", |s: ImmutableString| -> Dynamic {
        match serde_json::from_str::<JsonValue>(s.as_str()) {
            Ok(v) => json_to_dynamic(v),
            Err(_) => Dynamic::UNIT,
        }
    });

    engine.register_fn("to_json", |d: Dynamic| -> ImmutableString {
        let jv = dynamic_to_json(d);
        ImmutableString::from(serde_json::to_string(&jv).unwrap_or_else(|_| "null".to_string()))
    });

    // ─── get_num / get_num_str ────────────────────
    engine.register_fn(
        "get_num",
        |map: Dynamic, oid: ImmutableString| -> OptFloat {
            let entry = match get_oid_entry(&map, &oid) {
                Some(v) => v,
                None => return OptFloat::none(),
            };
            match get_field(&entry, "value_num") {
                Some(v) => dynamic_to_float(&v),
                None => OptFloat::none(),
            }
        },
    );

    engine.register_fn(
        "get_num_str",
        |map: Dynamic, oid: ImmutableString| -> OptStr {
            let entry = match get_oid_entry(&map, &oid) {
                Some(v) => v,
                None => return OptStr::none(),
            };
            match get_field(&entry, "value_str") {
                Some(v) => match v.clone().into_string() {
                    Ok(s) => OptStr::some(ImmutableString::from(s)),
                    Err(_) => OptStr::none(),
                },
                None => OptStr::none(),
            }
        },
    );

    // ─── 日志 ─────────────────────────────────────
    engine.register_fn("log_info", |msg: ImmutableString| {
        tracing::info!("[rhai-plugin] {}", msg);
    });
    engine.register_fn("log_warn", |msg: ImmutableString| {
        tracing::warn!("[rhai-plugin] {}", msg);
    });
    engine.register_fn("log_error", |msg: ImmutableString| {
        tracing::error!("[rhai-plugin] {}", msg);
    });
}

/// 定位 `map[oid]` 对应的内层 entry（一个 map）。
fn get_oid_entry(map: &Dynamic, oid: &ImmutableString) -> Option<Dynamic> {
    if !map.is_map() {
        return None;
    }
    let m = map.read_lock::<rhai::Map>()?;
    let oid_str: &str = oid.as_str();
    let inner_dyn = m
        .iter()
        .find(|(k, _)| k.as_str() == oid_str)
        .map(|(_, v)| v.clone())?;
    if !inner_dyn.is_map() {
        return None;
    }
    Some(inner_dyn)
}

/// 从 entry map 中按字段名精确取值，不依赖迭代顺序。
fn get_field(entry: &Dynamic, field: &str) -> Option<Dynamic> {
    if !entry.is_map() {
        return None;
    }
    let m = entry.read_lock::<rhai::Map>()?;
    m.iter()
        .find(|(k, _)| k.as_str() == field)
        .map(|(_, v)| v.clone())
}

/// 把 Rhai Dynamic 尝试转为 f64，兼容 float / int / 可解析字符串。
fn dynamic_to_float(v: &Dynamic) -> OptFloat {
    match v.as_float() {
        Ok(n) => OptFloat::some(n),
        Err(_) => match v.as_int() {
            Ok(i) => OptFloat::some(i as f64),
            Err(_) => match v.clone().into_string() {
                Ok(s) => match s.parse::<f64>() {
                    Ok(n) => OptFloat::some(n),
                    Err(_) => OptFloat::none(),
                },
                Err(_) => OptFloat::none(),
            },
        },
    }
}

// ─── serde_json::Value <-> rhai::Dynamic 转换 ───────────────────

fn json_to_dynamic(v: JsonValue) -> Dynamic {
    match v {
        JsonValue::Null => Dynamic::UNIT,
        JsonValue::Bool(b) => b.into(),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else {
                n.as_f64().unwrap_or(0.0).into()
            }
        }
        JsonValue::String(s) => ImmutableString::from(s).into(),
        JsonValue::Array(arr) => arr
            .into_iter()
            .map(json_to_dynamic)
            .collect::<Vec<Dynamic>>()
            .into(),
        JsonValue::Object(obj) => {
            let mut m = rhai::Map::new();
            for (k, v) in obj {
                m.insert(k.into(), json_to_dynamic(v));
            }
            m.into()
        }
    }
}

fn dynamic_to_json(d: Dynamic) -> JsonValue {
    if d.is_unit() {
        return JsonValue::Null;
    }
    if let Ok(b) = d.as_bool() {
        return JsonValue::Bool(b);
    }
    if let Ok(i) = d.as_int() {
        return JsonValue::Number(i.into());
    }
    if let Ok(f) = d.as_float() {
        return serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null);
    }
    if d.is_string() {
        return JsonValue::String(d.into_string().unwrap_or_default());
    }
    if d.is_array() {
        let arr = d.into_array().unwrap_or_default();
        return JsonValue::Array(arr.into_iter().map(dynamic_to_json).collect());
    }
    if d.is_map() {
        let map = d.cast::<rhai::Map>();
        let mut obj = serde_json::Map::new();
        for (k, v) in map {
            obj.insert(k.to_string(), dynamic_to_json(v));
        }
        return JsonValue::Object(obj);
    }
    JsonValue::String(format!("{d}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_object() {
        let s = r#"{"oid_value_type":2,"value_str":"","value_num":1234.56}"#;
        let v: JsonValue = serde_json::from_str(s).unwrap();
        let d = json_to_dynamic(v.clone());
        let back = dynamic_to_json(d);
        assert_eq!(back["value_num"], JsonValue::from(1234.56));
    }

    #[test]
    fn json_roundtrip_array() {
        let s = r#"[1,2,3,"hello"]"#;
        let v: JsonValue = serde_json::from_str(s).unwrap();
        let d = json_to_dynamic(v);
        let arr = d.into_array().unwrap();
        assert_eq!(arr.len(), 4);
    }
}
