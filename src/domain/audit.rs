//! 审计日志 — 记录增删改类写操作（谁、何时、做了什么、结果）。
//!
//! 落 `audit_logs` 表。GraphQL mutation 在 `graphql_handler` 统一拦截；
//! REST 写操作（auth 登录/注册/注销、shell-approve、upload）在各 handler 内显式记录。
//! 写入异步、失败仅丢日志（审计可降级，绝不阻塞业务主流程）。

use std::sync::Arc;

use async_graphql::parser::types::OperationType;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::infra::db::{DbPool, DbPooledConnection};

/// 一条审计记录（owned，可跨 `tokio::spawn`）
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub user_id: String,
    /// 显示名 / username（失败登录时 user_id 为空，靠 actor 记是谁）
    pub actor: String,
    /// 来源：`web`（账号登录）/ `api_token`（程序化 Bearer）
    pub source: String,
    /// 操作名：GraphQL mutation 名（deleteSession…）或 REST 动作（login/upload_image…）
    pub operation: String,
    /// 被操作对象 id（从参数提取）
    pub target_id: String,
    pub success: bool,
    /// 脱敏后的参数 JSON
    pub detail: String,
    pub ip: String,
}

/// 审计日志存储（仅 INSERT，不缓存、不自动建表——表由 schema.sql 部署时建）
pub struct AuditStore {
    pool: DbPool,
}

impl AuditStore {
    pub async fn new(pool: DbPool) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self { pool }))
    }

    async fn get_conn(&self) -> anyhow::Result<DbPooledConnection> {
        self.pool
            .get()
            .await
            .map_err(|e| anyhow::anyhow!("DB 连接获取失败: {e}"))
    }

    /// 写入一条审计记录。失败返回 Err（调用方在 spawn 内忽略即可，审计不阻塞业务）。
    pub async fn record(&self, e: AuditEntry) -> anyhow::Result<()> {
        let id = uuid::Uuid::now_v7().to_string();
        let now = chrono::Utc::now();
        let mut c = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO audit_logs
               (id, user_id, actor, source, operation, target_id, success, detail, ip, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
        )
        .bind::<sql_types::VarChar, _>(&id)
        .bind::<sql_types::VarChar, _>(&e.user_id)
        .bind::<sql_types::VarChar, _>(&e.actor)
        .bind::<sql_types::VarChar, _>(&e.source)
        .bind::<sql_types::VarChar, _>(&e.operation)
        .bind::<sql_types::VarChar, _>(&e.target_id)
        .bind::<sql_types::SmallInt, _>(if e.success { 1i16 } else { 0i16 })
        .bind::<sql_types::Text, _>(&e.detail)
        .bind::<sql_types::VarChar, _>(&e.ip)
        .bind::<sql_types::Timestamptz, _>(now)
        .execute(&mut c)
        .await?;
        Ok(())
    }
}

/// 异步记录审计（spawn，不阻塞业务；store 为 None 时跳过）。供 REST handler 简便调用。
pub fn spawn_record(store: Option<&Arc<AuditStore>>, entry: AuditEntry) {
    if let Some(store) = store {
        let store = store.clone();
        tokio::spawn(async move {
            let _ = store.record(entry).await;
        });
    }
}

// ========== 辅助函数（供 graphql_handler / REST handler 复用） ==========

/// 需脱敏的 key（小写子串匹配）
fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_lowercase();
    ["password", "api_key", "apikey", "token", "secret", "authorization"]
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
    if let Some(v) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    {
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
                Some(want) => name.map(|n| n.as_str()) == Some(want) && ty == OperationType::Mutation,
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
