//! 归属人显示名批量解析 —— 供各资源列表给管理员视图注入「归属」列。
//!
//! 与 `session_settings` 列表归属列同款：`COALESCE(name, username, LEFT(id,8))`。
//! 采用**批量后处理注入**而非逐个列表查询改 SQL，避免改动既有查询引入回归。

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;
use serde_json::Value;

use crate::infra::db::DbPool;

#[derive(Debug, QueryableByName)]
struct UserLabelRow {
    #[diesel(sql_type = sql_types::Text)]
    id: String,
    #[diesel(sql_type = sql_types::Text)]
    label: String,
}

/// 批量把 user_id 解析为显示名。空输入 / DB 不可用 / 查询失败 → 空 map
/// （调用方回退到 user_id 前 8 位，与会话列表 LEFT(id,8) 一致）。
pub async fn resolve_labels(
    pool: &DbPool,
    user_ids: &[&str],
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let dedup: Vec<&str> = {
        let mut seen = std::collections::HashSet::new();
        user_ids
            .iter()
            .filter(|s| !s.is_empty() && seen.insert(**s))
            .copied()
            .collect()
    };
    if dedup.is_empty() {
        return map;
    }
    let mut conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "owner", "归属名解析取连接失败（降级为 id 前缀）: {e}");
            return map;
        }
    };
    let rows: Vec<UserLabelRow> = diesel::sql_query(
        "SELECT id, COALESCE(NULLIF(name, ''), NULLIF(username, ''), LEFT(id, 8)) AS label \
         FROM users WHERE id = ANY($1)",
    )
    .bind::<sql_types::Array<sql_types::Text>, _>(&dedup)
    .get_results(&mut conn)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(target: "owner", "归属名解析查询失败（降级为 id 前缀）: {e}");
        Vec::new()
    });
    for r in rows {
        map.insert(r.id, r.label);
    }
    map
}

/// 给一组已序列化为 JSON 数组的列表项注入 `owner` 显示名字段。
///
/// - `is_admin`：仅管理员响应注入（归属列只给管理员看；非管理员不多发他人信息）。
/// - `pool_opt`：DB 连接池；None（DB 未启用）时直接返回（管理员视图本就不适用）。
/// - `id_field`：每项里归属 user_id 的字段名（实体不同：`user_id` / `creator`）。
/// - id 为空的项注入空串 owner（前端 `v-if="row.owner"` 判 falsy 显示「-」）。
pub async fn inject_owners(
    pool_opt: Option<&DbPool>,
    is_admin: bool,
    items: &mut [Value],
    id_field: &str,
) {
    if !is_admin {
        return;
    }
    let Some(pool) = pool_opt else {
        return;
    };
    let ids: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get(id_field).and_then(|v| v.as_str()))
        .collect();
    let map = resolve_labels(pool, &ids).await;
    for item in items.iter_mut() {
        let uid = item.get(id_field).and_then(|v| v.as_str()).unwrap_or("");
        let label = if uid.is_empty() {
            String::new()
        } else {
            map.get(uid)
                .cloned()
                .unwrap_or_else(|| uid.get(..8).unwrap_or(uid).to_string())
        };
        if let Some(obj) = item.as_object_mut() {
            obj.insert("owner".into(), serde_json::json!(label));
        }
    }
}
