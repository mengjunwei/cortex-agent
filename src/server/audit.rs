//! 审计的 HTTP/GraphQL 侧辅助 — 从 `domain::audit` 拆出(架构 P5)。
//!
//! 领域层只保留 `AuditEntry` / `AuditStore` / `spawn_record`;
//! 依赖 axum HeaderMap / async_graphql Request 的请求解析函数属于传输层,归 server。

use async_graphql::parser::types::OperationType;

// ========== 辅助函数（供 graphql_handler / REST handler 复用） ==========

/// 需脱敏的 key（小写子串匹配）
fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_lowercase();
    [
        "password",
        "api_key",
        "apikey",
        "token",
        "secret",
        "authorization",
    ]
    .iter()
    .any(|s| k.contains(s))
}

/// 递归脱敏：把含敏感关键字的字段值替换为 `"***"`。
pub fn redact_variables(vars: serde_json::Value) -> serde_json::Value {
    match vars {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if is_sensitive_key(&k) {
                    out.insert(k, serde_json::Value::String("***".into()));
                } else {
                    out.insert(k, redact_variables(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(redact_variables).collect())
        }
        other => other,
    }
}

/// 从 GraphQL 变量里提取被操作对象 id。
/// 批量操作取 `ids` 数组拼逗号串；单值取 `id` 或任意 `*_id` 键的字符串值。
pub fn extract_target_id(vars: &serde_json::Value) -> String {
    let Some(obj) = vars.as_object() else {
        return String::new();
    };
    if let Some(ids) = obj.get("ids").and_then(|v| v.as_array()) {
        let s: Vec<&str> = ids.iter().filter_map(|v| v.as_str()).collect();
        if !s.is_empty() {
            return s.join(",");
        }
    }
    if let Some(v) = obj.get("id").and_then(|v| v.as_str()) {
        return v.to_string();
    }
    for (k, v) in obj {
        if k.len() > 3 && k.ends_with("_id") {
            if let Some(s) = v.as_str() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// 从请求头提取客户端 IP（`x-forwarded-for` 首段，否则 `x-real-ip`）。
pub fn client_ip(headers: &axum::http::HeaderMap) -> String {
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            let s = first.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    if let Some(v) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        return v.trim().to_string();
    }
    String::new()
}

/// 判断 GraphQL 请求是否为 mutation。
/// 优先用解析后的 AST（精确，能正确处理 operationName + 多 operation 文档）；
/// 解析失败（语法非法，极少见）才退回启发式。
pub fn is_mutation_request(request: &mut async_graphql::Request) -> bool {
    // 先把 operation_name clone 出来，避免与 parsed_query() 的 &mut 借用冲突
    let target = request.operation_name.clone();
    if let Ok(doc) = request.parsed_query() {
        return doc.operations.iter().any(|(name, op)| {
            let ty = op.node.ty;
            match target.as_deref() {
                Some(want) => {
                    name.map(|n| n.as_str()) == Some(want) && ty == OperationType::Mutation
                }
                None => ty == OperationType::Mutation,
            }
        });
    }
    request.query.trim_start().starts_with("mutation")
}

/// 从 GraphQL query 串提取首个字段名作为操作名。
/// `mutation($id: String!) { deleteSession(id: $id) }` → `deleteSession`
pub fn operation_from_query(query: &str) -> String {
    let after = match query.split_once('{') {
        Some((_, rest)) => rest,
        None => query,
    };
    after
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_masks_password_and_token() {
        let v = serde_json::json!({
            "username": "alice",
            "password": "secret123",
            "api_key": "sk-xxx",
            "data": { "token": "t", "note": "ok" }
        });
        let r = redact_variables(v);
        assert_eq!(r["username"], "alice");
        assert_eq!(r["password"], "***");
        assert_eq!(r["api_key"], "***");
        assert_eq!(r["data"]["token"], "***");
        assert_eq!(r["data"]["note"], "ok");
    }

    #[test]
    fn extract_target_id_single_and_batch() {
        assert_eq!(extract_target_id(&serde_json::json!({"id": "abc"})), "abc");
        assert_eq!(
            extract_target_id(&serde_json::json!({"ids": ["a", "b"]})),
            "a,b"
        );
        assert_eq!(
            extract_target_id(&serde_json::json!({"assistant_id": "x"})),
            "x"
        );
        assert_eq!(extract_target_id(&serde_json::json!({"input": {}})), "");
    }

    #[test]
    fn operation_from_query_extracts_first_field() {
        assert_eq!(
            operation_from_query("mutation($id: String!) { deleteSession(id: $id) }"),
            "deleteSession"
        );
        assert_eq!(
            operation_from_query("mutation { createAssistant(input: $i) }"),
            "createAssistant"
        );
        assert_eq!(operation_from_query("query { sessions }"), "sessions");
    }
}
