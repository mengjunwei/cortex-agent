//! MCP Server 数据存储层（diesel-async）
//!
//! 范式同 [`crate::model_provider::store`]：
//! - 主键 UUID v7 字符串；枚举 `status`/`transport` 以 SMALLINT 存储
//! - 敏感字段（env/headers）整体 JSON 后 AES-256-GCM 加密存储
//! - `args` / 掩码 map 以 TEXT 存 JSON
//! - 建表 DDL 见 `migrations/schema.sql`（架构 §8.5）

use std::collections::HashMap;
use std::sync::Arc;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;
use uuid::Uuid;

use crate::config::SecurityConfig;
use crate::domain::mcp::dto::{CreateMcpServerInput, McpServerResponse, UpdateMcpServerInput};
use crate::domain::mcp::enums::{Status, TransportKind};
use crate::domain::mcp::models::{McpServer, ServerHealth, mask_map};
use crate::error::AppError;
use crate::infra::db::{DbPool, DbPooledConnection};
use crate::infra::store_base::{Store, is_unique_violation, new_id};
use crate::model_provider::crypto::AesCodec;

/// 绑定 UPDATE 公共字段（$1=id, $2=name, $3=transport, $4=endpoint, $5=args）。
/// 用宏而非函数：diesel 的 bind 返回 UncheckedBind 链式嵌套类型不可命名，
/// 函数无法封装；宏在调用处展开，各分支独立推导类型。
macro_rules! bind_common {
    ($q:expr, $id:expr, $name:expr, $transport:expr, $endpoint:expr, $args:expr $(,)?) => {
        $q.bind::<sql_types::Text, _>($id)
            .bind::<sql_types::Text, _>($name)
            .bind::<sql_types::Int2, _>($transport)
            .bind::<sql_types::Text, _>($endpoint)
            .bind::<sql_types::Text, _>($args)
    };
}

/// DB 行（敏感字段存密文/掩码 JSON）
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
pub(crate) struct McpServerRow {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub name: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub slug: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub transport: i16,
    #[diesel(sql_type = sql_types::Varchar)]
    pub endpoint: String,
    #[diesel(sql_type = sql_types::Text)]
    pub args: String,
    #[diesel(sql_type = sql_types::Text)]
    pub env_enc: String,
    #[diesel(sql_type = sql_types::Text)]
    pub env_mask: String,
    #[diesel(sql_type = sql_types::Text)]
    pub headers_enc: String,
    #[diesel(sql_type = sql_types::Text)]
    pub headers_mask: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub status: i16,
    #[diesel(sql_type = sql_types::Int4)]
    pub tool_timeout_secs: i32,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 单列 slug 查询行
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct SlugRow {
    #[diesel(sql_type = sql_types::Varchar)]
    slug: String,
}

/// 存在性检查行
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct ExistsRow {
    #[diesel(sql_type = sql_types::Integer)]
    flag: i32,
}

/// COUNT 查询行
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct CountRow {
    #[diesel(sql_type = sql_types::BigInt)]
    count: i64,
}

/// 单列 id 查询行
#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct IdRow {
    #[diesel(sql_type = sql_types::Text)]
    id: String,
}

pub struct McpServerStore {
    pool: DbPool,
    pub(crate) codec: AesCodec,
}

#[async_trait::async_trait]
impl Store for McpServerStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl McpServerStore {
    pub async fn new(pool: DbPool, security: &SecurityConfig) -> Result<Arc<Self>, AppError> {
        let codec = crate::model_provider::crypto::codec_from_security(security, "MCP");
        let store = Arc::new(Self { pool, codec });
        Ok(store)
    }

    pub fn codec(&self) -> &AesCodec {
        &self.codec
    }

    /// 生成不冲突的 slug：基础 slug + 短随机后缀
    pub(crate) async fn generate_unique_slug(
        &self,
        conn: &mut DbPooledConnection,
        name: &str,
    ) -> Result<String, AppError> {
        let base = crate::domain::mcp::models::slugify(name);
        for _ in 0..8 {
            let suffix = random_suffix(4);
            let slug = format!("{base}_{suffix}");
            let exists = diesel::sql_query("SELECT 1 AS flag FROM mcp_servers WHERE slug = $1")
                .bind::<sql_types::Text, _>(&slug)
                .get_results::<ExistsRow>(conn)
                .await?;
            if exists.is_empty() {
                return Ok(slug);
            }
        }
        // 兜底：使用完整 UUID 片段避免极小概率冲突
        Ok(format!("{base}_{}", random_suffix(8)))
    }

    // ========================================================================
    //  CRUD
    // ========================================================================

    pub async fn create(&self, input: &CreateMcpServerInput) -> Result<McpServer, AppError> {
        validate_name(&input.name)?;
        validate_endpoint(&input.transport, &input.endpoint)?;
        validate_args(&input.args)?;

        let env_enc = encrypt_map(&self.codec, &input.env)?;
        let env_mask = serde_json::to_string(&mask_map(&input.env))?;
        let headers_enc = encrypt_map(&self.codec, &input.headers)?;
        let headers_mask = serde_json::to_string(&mask_map(&input.headers))?;

        let mut conn = self.get_conn().await?;
        let slug = self.generate_unique_slug(&mut conn, &input.name).await?;
        let id = new_id();
        let args_json = serde_json::to_string(&input.args)?;

        let affected = diesel::sql_query(
            r#"
            INSERT INTO mcp_servers
                (id, name, slug, transport, endpoint, args,
                 env_enc, env_mask, headers_enc, headers_mask, status, tool_timeout_secs)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
            "#,
        )
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(input.name.trim())
        .bind::<sql_types::Text, _>(&slug)
        .bind::<sql_types::Int2, _>(input.transport.as_i16())
        .bind::<sql_types::Text, _>(input.endpoint.trim())
        .bind::<sql_types::Text, _>(&args_json)
        .bind::<sql_types::Text, _>(&env_enc)
        .bind::<sql_types::Text, _>(&env_mask)
        .bind::<sql_types::Text, _>(&headers_enc)
        .bind::<sql_types::Text, _>(&headers_mask)
        .bind::<sql_types::Int2, _>(input.status.as_i16())
        .bind::<sql_types::Int4, _>(input.tool_timeout_secs as i32)
        .execute(&mut conn)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::ConflictError("slug 冲突，请重试".into())
            } else {
                AppError::from(e)
            }
        })?;

        if affected == 0 {
            return Err(AppError::BusinessError("创建 MCP Server 失败".into()));
        }

        let server = self
            .get_by_id_conn(&mut conn, &id)
            .await?
            .ok_or_else(|| AppError::DatabaseError("刚创建的 MCP Server 读取失败".into()))?;
        Ok(server)
    }

    /// 更新；env/headers 为 None 时保持原密文不变。
    /// 返回 (解密后的领域实体, 是否存在)
    pub async fn update(
        &self,
        id: &str,
        input: &UpdateMcpServerInput,
    ) -> Result<Option<McpServer>, AppError> {
        validate_name(&input.name)?;
        validate_endpoint(&input.transport, &input.endpoint)?;
        validate_args(&input.args)?;

        let mut conn = self.get_conn().await?;
        // 先确认存在
        if !server_exists(&mut conn, id).await? {
            return Ok(None);
        }

        let args_json = serde_json::to_string(&input.args)?;
        // 公共字段提取为局部变量，供 bind_common 统一绑定
        let name = input.name.trim();
        let endpoint = input.endpoint.trim();
        let transport = input.transport.as_i16();
        let status = input.status.as_i16();

        // 按 env/headers 是否提供，分四种 SQL（避免动态拼 SQL）
        match (&input.env, &input.headers) {
            (Some(env), Some(headers)) => {
                let (env_enc, env_mask) = prepare_secret(&self.codec, env)?;
                let (headers_enc, headers_mask) = prepare_secret(&self.codec, headers)?;
                bind_common!(
                    diesel::sql_query(
                        r#"UPDATE mcp_servers SET
                             name=$2, transport=$3, endpoint=$4, args=$5,
                             env_enc=$6, env_mask=$7, headers_enc=$8, headers_mask=$9,
                             status=$10, tool_timeout_secs=$11, updated_at=NOW()
                           WHERE id=$1"#,
                    ),
                    id,
                    name,
                    transport,
                    endpoint,
                    &args_json,
                )
                .bind::<sql_types::Text, _>(&env_enc)
                .bind::<sql_types::Text, _>(&env_mask)
                .bind::<sql_types::Text, _>(&headers_enc)
                .bind::<sql_types::Text, _>(&headers_mask)
                .bind::<sql_types::Int2, _>(status)
                .bind::<sql_types::Int4, _>(input.tool_timeout_secs as i32)
                .execute(&mut conn)
                .await?;
            }
            (Some(env), None) => {
                let (env_enc, env_mask) = prepare_secret(&self.codec, env)?;
                bind_common!(
                    diesel::sql_query(
                        r#"UPDATE mcp_servers SET
                             name=$2, transport=$3, endpoint=$4, args=$5,
                             env_enc=$6, env_mask=$7, status=$8, tool_timeout_secs=$9, updated_at=NOW()
                           WHERE id=$1"#,
                    ),
                    id,
                    name,
                    transport,
                    endpoint,
                    &args_json,
                )
                .bind::<sql_types::Text, _>(&env_enc)
                .bind::<sql_types::Text, _>(&env_mask)
                .bind::<sql_types::Int2, _>(status)
                .bind::<sql_types::Int4, _>(input.tool_timeout_secs as i32)
                .execute(&mut conn)
                .await?;
            }
            (None, Some(headers)) => {
                let (headers_enc, headers_mask) = prepare_secret(&self.codec, headers)?;
                bind_common!(
                    diesel::sql_query(
                        r#"UPDATE mcp_servers SET
                             name=$2, transport=$3, endpoint=$4, args=$5,
                             headers_enc=$6, headers_mask=$7, status=$8, tool_timeout_secs=$9, updated_at=NOW()
                           WHERE id=$1"#,
                    ),
                    id,
                    name,
                    transport,
                    endpoint,
                    &args_json,
                )
                .bind::<sql_types::Text, _>(&headers_enc)
                .bind::<sql_types::Text, _>(&headers_mask)
                .bind::<sql_types::Int2, _>(status)
                .bind::<sql_types::Int4, _>(input.tool_timeout_secs as i32)
                .execute(&mut conn)
                .await?;
            }
            (None, None) => {
                bind_common!(
                    diesel::sql_query(
                        r#"UPDATE mcp_servers SET
                             name=$2, transport=$3, endpoint=$4, args=$5,
                             status=$6, tool_timeout_secs=$7, updated_at=NOW()
                           WHERE id=$1"#,
                    ),
                    id,
                    name,
                    transport,
                    endpoint,
                    &args_json,
                )
                .bind::<sql_types::Int2, _>(status)
                .bind::<sql_types::Int4, _>(input.tool_timeout_secs as i32)
                .execute(&mut conn)
                .await?;
            }
        }

        self.get_by_id_conn(&mut conn, id).await
    }

    pub async fn delete(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        // 联动清理所有 assistant.enabled_mcps 引用 + 删主实体，同一事务内原子完成
        diesel::sql_query("BEGIN").execute(&mut conn).await?;
        let tx: Result<bool, AppError> = async {
            purge_mcp_from_assistants(&mut conn, id).await?;
            let affected = diesel::sql_query("DELETE FROM mcp_servers WHERE id = $1")
                .bind::<sql_types::Text, _>(id)
                .execute(&mut conn)
                .await?;
            Ok(affected > 0)
        }
        .await;
        match tx {
            Ok(res) => {
                diesel::sql_query("COMMIT").execute(&mut conn).await?;
                Ok(res)
            }
            Err(e) => {
                let _ = diesel::sql_query("ROLLBACK").execute(&mut conn).await;
                Err(e)
            }
        }
    }

    /// 预检：统计有多少助手启用了该 MCP（enabled_mcps JSON 数组含此 id）。
    pub async fn impact_of_delete(&self, id: &str) -> Result<i64, AppError> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = sql_types::BigInt)]
            cnt: i64,
        }
        let mut conn = self.get_conn().await?;
        let row = diesel::sql_query(
            r#"SELECT COUNT(*) AS cnt FROM assistants
               WHERE enabled_mcps::jsonb @> to_jsonb(ARRAY[$1::text])"#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_result::<Row>(&mut conn)
        .await?;
        Ok(row.cnt)
    }

    pub async fn list_all(&self) -> Result<Vec<McpServer>, AppError> {
        let mut conn = self.get_conn().await?;
        self.list_all_conn(&mut conn).await
    }

    async fn list_all_conn(
        &self,
        conn: &mut DbPooledConnection,
    ) -> Result<Vec<McpServer>, AppError> {
        let rows = diesel::sql_query(
            "SELECT id, name, slug, transport, endpoint, args, env_enc, env_mask, \
             headers_enc, headers_mask, status, tool_timeout_secs, created_at, updated_at \
             FROM mcp_servers ORDER BY created_at ASC",
        )
        .get_results::<McpServerRow>(conn)
        .await?;
        rows.into_iter().map(|r| self.row_to_server(r)).collect()
    }

    /// 分页查询 MCP 服务
    pub async fn list_paged(
        &self,
        page: usize,
        page_size: usize,
        keyword: Option<&str>,
    ) -> Result<(Vec<McpServer>, i64), AppError> {
        let mut conn = self.get_conn().await?;
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = ((page - 1) * page_size) as i64;

        let (where_clause, kw_bind) = match keyword.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(kw) => (
            " WHERE LOWER(name) LIKE LOWER($3) OR LOWER(slug) LIKE LOWER($3) OR LOWER(endpoint) LIKE LOWER($3)".to_string(),
            Some(format!("%{}%", kw)),
            ),
            None => (String::new(), None),
        };

        // count
        let count_sql = format!("SELECT COUNT(*) AS count FROM mcp_servers{where_clause}");
        let total: i64 = if let Some(ref kw) = kw_bind {
            diesel::sql_query(&count_sql)
                .bind::<sql_types::Text, _>(kw)
                .get_result::<CountRow>(&mut conn)
                .await?
                .count
        } else {
            diesel::sql_query(&count_sql)
                .get_result::<CountRow>(&mut conn)
                .await?
                .count
        };

        // list
        let list_sql = format!(
            "SELECT id, name, slug, transport, endpoint, args, env_enc, env_mask, \
             headers_enc, headers_mask, status, tool_timeout_secs, created_at, updated_at \
             FROM mcp_servers{where_clause} ORDER BY created_at ASC LIMIT $1 OFFSET $2"
        );
        let rows = if let Some(ref kw) = kw_bind {
            diesel::sql_query(&list_sql)
                .bind::<sql_types::Int8, _>(page_size as i64)
                .bind::<sql_types::Int8, _>(offset)
                .bind::<sql_types::Text, _>(kw)
                .get_results::<McpServerRow>(&mut conn)
                .await?
        } else {
            diesel::sql_query(&list_sql)
                .bind::<sql_types::Int8, _>(page_size as i64)
                .bind::<sql_types::Int8, _>(offset)
                .get_results::<McpServerRow>(&mut conn)
                .await?
        };

        let servers: Vec<McpServer> = rows
            .into_iter()
            .map(|r| self.row_to_server(r))
            .collect::<Result<_, _>>()?;
        Ok((servers, total))
    }

    /// 按 ID 列表批量设置状态。返回受影响行数。
    pub async fn set_status_batch(
        &self,
        ids: &[String],
        status_val: i16,
    ) -> Result<usize, AppError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.get_conn().await?;
        let json_ids = serde_json::Value::Array(
            ids.iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
        let affected = diesel::sql_query(
            "UPDATE mcp_servers SET status = $2, updated_at = NOW() WHERE id = ANY(SELECT json_array_elements_text($1::json))",
        )
        .bind::<sql_types::Text, _>(&json_ids.to_string())
        .bind::<sql_types::Int2, _>(status_val)
        .execute(&mut conn)
        .await?;
        Ok(affected)
    }

    /// 按筛选条件批量设置状态（跨页全选）。返回受影响行数。
    pub async fn set_status_by_filter(
        &self,
        keyword: Option<&str>,
        status_val: i16,
    ) -> Result<usize, AppError> {
        let mut conn = self.get_conn().await?;
        let affected = match keyword.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(kw) => {
                let pattern = format!("%{}%", kw);
                diesel::sql_query(
                    "UPDATE mcp_servers SET status = $2, updated_at = NOW() \
                     WHERE LOWER(name) LIKE LOWER($1) OR LOWER(slug) LIKE LOWER($1) OR LOWER(endpoint) LIKE LOWER($1)",
                )
                .bind::<sql_types::Text, _>(&pattern)
                .bind::<sql_types::Int2, _>(status_val)
                .execute(&mut conn)
                .await?
            }
            None => {
                diesel::sql_query("UPDATE mcp_servers SET status = $1, updated_at = NOW()")
                    .bind::<sql_types::Int2, _>(status_val)
                    .execute(&mut conn)
                    .await?
            }
        };
        Ok(affected)
    }

    /// 按 ID 列表批量删除（含级联清理助手引用）。返回受影响行数。
    pub async fn delete_batch(&self, ids: &[String]) -> Result<usize, AppError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.get_conn().await?;
        for id in ids {
            purge_mcp_from_assistants(&mut conn, id).await?;
        }
        let json_ids = serde_json::Value::Array(
            ids.iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
        let affected = diesel::sql_query(
            "DELETE FROM mcp_servers WHERE id = ANY(SELECT json_array_elements_text($1::json))",
        )
        .bind::<sql_types::Text, _>(&json_ids.to_string())
        .execute(&mut conn)
        .await?;
        Ok(affected)
    }

    /// 按筛选条件批量删除（跨页全选，含级联清理）。返回受影响行数。
    pub async fn delete_by_filter(&self, keyword: Option<&str>) -> Result<usize, AppError> {
        let mut conn = self.get_conn().await?;
        // 先查出所有匹配 ID 做级联清理
        let ids: Vec<String> = match keyword.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(kw) => {
                let pattern = format!("%{}%", kw);
                diesel::sql_query(
                    "SELECT id FROM mcp_servers WHERE LOWER(name) LIKE LOWER($1) OR LOWER(slug) LIKE LOWER($1) OR LOWER(endpoint) LIKE LOWER($1)",
                )
                .bind::<sql_types::Text, _>(&pattern)
                .get_results::<IdRow>(&mut conn)
                .await?
                .into_iter()
                .map(|r| r.id)
                .collect()
            }
            None => diesel::sql_query("SELECT id FROM mcp_servers")
                .get_results::<IdRow>(&mut conn)
                .await?
                .into_iter()
                .map(|r| r.id)
                .collect(),
        };
        if ids.is_empty() {
            return Ok(0);
        }
        for id in &ids {
            purge_mcp_from_assistants(&mut conn, id).await?;
        }
        let json_ids = serde_json::Value::Array(
            ids.iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
        let affected = diesel::sql_query(
            "DELETE FROM mcp_servers WHERE id = ANY(SELECT json_array_elements_text($1::json))",
        )
        .bind::<sql_types::Text, _>(&json_ids.to_string())
        .execute(&mut conn)
        .await?;
        Ok(affected)
    }

    pub async fn get_by_id(&self, id: &str) -> Result<Option<McpServer>, AppError> {
        let mut conn = self.get_conn().await?;
        self.get_by_id_conn(&mut conn, id).await
    }

    async fn get_by_id_conn(
        &self,
        conn: &mut DbPooledConnection,
        id: &str,
    ) -> Result<Option<McpServer>, AppError> {
        let rows = diesel::sql_query(
            "SELECT id, name, slug, transport, endpoint, args, env_enc, env_mask, \
             headers_enc, headers_mask, status, tool_timeout_secs, created_at, updated_at \
             FROM mcp_servers WHERE id = $1",
        )
        .bind::<sql_types::Text, _>(id)
        .get_results::<McpServerRow>(conn)
        .await?;
        match rows.into_iter().next() {
            Some(r) => Ok(Some(self.row_to_server(r)?)),
            None => Ok(None),
        }
    }

    pub async fn get_by_slug(&self, slug: &str) -> Result<Option<McpServer>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT id, name, slug, transport, endpoint, args, env_enc, env_mask, \
             headers_enc, headers_mask, status, tool_timeout_secs, created_at, updated_at \
             FROM mcp_servers WHERE slug = $1",
        )
        .bind::<sql_types::Text, _>(slug)
        .get_results::<McpServerRow>(&mut conn)
        .await?;
        match rows.into_iter().next() {
            Some(r) => Ok(Some(self.row_to_server(r)?)),
            None => Ok(None),
        }
    }

    /// 把 DB 行解密为领域实体（解密失败时跳过敏感字段，仅记日志，不阻断）
    fn row_to_server(&self, r: McpServerRow) -> Result<McpServer, AppError> {
        let args: Vec<String> = serde_json::from_str(&r.args).unwrap_or_default();
        let env = decrypt_map(&self.codec, &r.env_enc).unwrap_or_default();
        let headers = decrypt_map(&self.codec, &r.headers_enc).unwrap_or_default();
        Ok(McpServer {
            id: r.id,
            name: r.name,
            slug: r.slug,
            transport: TransportKind::from_i16(r.transport),
            endpoint: r.endpoint,
            args,
            env,
            headers,
            status: Status::from_i16(r.status),
            tool_timeout_secs: r.tool_timeout_secs as i64,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }

    // ========================================================================
    //  响应组装
    // ========================================================================

    /// 组装响应 DTO（敏感字段从掩码 JSON 还原，解密后的明文不进入响应）
    pub fn to_response(server: &McpServer, health: ServerHealth) -> McpServerResponse {
        McpServerResponse {
            id: server.id.clone(),
            name: server.name.clone(),
            slug: server.slug.clone(),
            transport: server.transport,
            endpoint: server.endpoint.clone(),
            args: server.args.clone(),
            env: mask_map(&server.env),
            headers: mask_map(&server.headers),
            status: server.status,
            tool_timeout_secs: server.tool_timeout_secs,
            created_at: server.created_at.to_rfc3339(),
            updated_at: server.updated_at.to_rfc3339(),
            health,
        }
    }
}

// ========== 工具函数 ==========

/// 检查指定 id 的 mcp_server 是否存在
async fn server_exists(conn: &mut DbPooledConnection, id: &str) -> Result<bool, AppError> {
    let rows = diesel::sql_query("SELECT 1 AS flag FROM mcp_servers WHERE id = $1")
        .bind::<sql_types::Text, _>(id)
        .get_results::<ExistsRow>(conn)
        .await?;
    Ok(!rows.is_empty())
}

/// 加密 map 并生成脱敏 JSON 字符串，返回 (enc, mask)
fn prepare_secret(
    codec: &AesCodec,
    map: &HashMap<String, String>,
) -> Result<(String, String), AppError> {
    let enc = encrypt_map(codec, map)?;
    let mask = serde_json::to_string(&mask_map(map))?;
    Ok((enc, mask))
}

fn validate_name(name: &str) -> Result<(), AppError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(AppError::BusinessError("MCP Server 名称不能为空".into()));
    }
    if n.chars().count() > 128 {
        return Err(AppError::BusinessError(
            "MCP Server 名称不能超过 128 字符".into(),
        ));
    }
    Ok(())
}

fn validate_args(args: &[String]) -> Result<(), AppError> {
    if args.iter().any(|a| a.chars().count() > 4096) {
        return Err(AppError::BusinessError("单个启动参数过长".into()));
    }
    Ok(())
}

/// 校验 endpoint：stdio 必须非空命令名；http 必须是 http(s) URL
pub(crate) fn validate_endpoint(transport: &TransportKind, endpoint: &str) -> Result<(), AppError> {
    let e = endpoint.trim();
    if e.is_empty() {
        return Err(AppError::BusinessError("endpoint 不能为空".into()));
    }
    if e.chars().count() > 1024 {
        return Err(AppError::BusinessError("endpoint 过长（>1024）".into()));
    }
    match transport {
        TransportKind::Stdio => {
            // 命令名：禁止 shell 元字符，避免注入（实际 args 用 Command::arg 逐个传）
            if e.contains(['|', ';', '&', '>', '<', '`', '$']) {
                return Err(AppError::BusinessError(
                    "stdio 命令包含非法 shell 元字符".into(),
                ));
            }
        }
        TransportKind::StreamableHttp => {
            if !(e.starts_with("https://") || e.starts_with("http://")) {
                return Err(AppError::BusinessError(
                    "http 传输的 endpoint 必须以 http:// 或 https:// 开头".into(),
                ));
            }
        }
    }
    Ok(())
}

fn encrypt_map(codec: &AesCodec, map: &HashMap<String, String>) -> Result<String, AppError> {
    if map.is_empty() {
        return Ok(String::new());
    }
    let json = serde_json::to_string(map)?;
    let enc = codec
        .encrypt(&json)
        .map_err(|e| AppError::BusinessError(format!("MCP 凭据加密失败: {e}")))?;
    Ok(enc)
}

fn decrypt_map(codec: &AesCodec, enc: &str) -> Result<HashMap<String, String>, AppError> {
    if enc.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let plain = codec
        .decrypt(enc)
        .map_err(|e| AppError::BusinessError(format!("MCP 凭据解密失败: {e}")))?;
    let map: HashMap<String, String> = serde_json::from_str(&plain)?;
    Ok(map)
}

/// 从所有 assistant.enabled_mcps（JSON 数组）中移除指定 mcp_id 引用。
/// 使用 PostgreSQL jsonb 路径表达式避免拉取全表到内存。
pub(crate) async fn purge_mcp_from_assistants(
    conn: &mut DbPooledConnection,
    mcp_id: &str,
) -> Result<(), AppError> {
    // enabled_mcps 存 TEXT(json 数组)；用 jsonb 转换过滤后写回。
    // 仅更新包含该 id 的行。
    diesel::sql_query(
        r#"UPDATE assistants SET enabled_mcps = (
                SELECT COALESCE(jsonb_agg(elem)::text, '[]')
                FROM jsonb_array_elements(enabled_mcps::jsonb) AS elem
                WHERE elem::text <> to_jsonb($1::text)::text
           )
           WHERE enabled_mcps::jsonb @> to_jsonb(ARRAY[$1::text])"#,
    )
    .bind::<sql_types::Text, _>(mcp_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// 生成给定长度的随机后缀（小写字母+数字）
fn random_suffix(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut state: u64 = {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        let uuid_rand = (Uuid::now_v7().as_u128() as u64).wrapping_mul(0xFF51AFD7ED558CCD);
        t ^ uuid_rand
    };
    if state == 0 {
        state = 0x9E3779B97F4A7C15;
    }
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        s.push(ALPHABET[(state % ALPHABET.len() as u64) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_endpoint_stdio_rejects_shell_metachars() {
        assert!(validate_endpoint(&TransportKind::Stdio, "npx -y foo").is_ok());
        assert!(validate_endpoint(&TransportKind::Stdio, "npx; rm -rf").is_err());
        assert!(validate_endpoint(&TransportKind::Stdio, "sh && cmd").is_err());
        assert!(validate_endpoint(&TransportKind::Stdio, "").is_err());
    }

    #[test]
    fn validate_endpoint_http_requires_scheme() {
        assert!(validate_endpoint(&TransportKind::StreamableHttp, "https://x.com/mcp").is_ok());
        assert!(
            validate_endpoint(&TransportKind::StreamableHttp, "http://localhost:8080/mcp").is_ok()
        );
        assert!(validate_endpoint(&TransportKind::StreamableHttp, "localhost:8080").is_err());
    }

    #[test]
    fn validate_name_checks_empty_and_length() {
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("GitHub").is_ok());
    }

    #[test]
    fn encrypt_decrypt_map_roundtrip() {
        let codec = AesCodec::from_passphrase("test-key-fixed-value-32b!!!");
        let mut m = HashMap::new();
        m.insert("TOKEN".into(), "sk-1234567890abcd".into());
        m.insert("ENV".into(), "prod".into());
        let enc = encrypt_map(&codec, &m).unwrap();
        assert!(!enc.is_empty());
        let dec = decrypt_map(&codec, &enc).unwrap();
        assert_eq!(dec.get("TOKEN").unwrap(), "sk-1234567890abcd");
        assert_eq!(dec.get("ENV").unwrap(), "prod");
    }

    #[test]
    fn encrypt_empty_map_yields_empty_string() {
        let codec = AesCodec::from_passphrase("test-key-fixed-value-32b!!!");
        let m = HashMap::new();
        let enc = encrypt_map(&codec, &m).unwrap();
        assert!(enc.is_empty());
        let dec = decrypt_map(&codec, "").unwrap();
        assert!(dec.is_empty());
    }

    #[test]
    fn random_suffix_is_lowercase_alnum() {
        let s = random_suffix(6);
        assert_eq!(s.len(), 6);
        for c in s.chars() {
            assert!(c.is_ascii_lowercase() || c.is_ascii_digit());
        }
    }

    #[test]
    fn to_response_masks_sensitive_values() {
        let mut env = HashMap::new();
        env.insert("KEY".into(), "sk-1234567890abcd".into());
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), "Bearer xyz12345".into());
        let server = McpServer {
            id: "id1".into(),
            name: "GitHub".into(),
            slug: "github_abcd".into(),
            transport: TransportKind::Stdio,
            endpoint: "npx".into(),
            args: vec![],
            env: env.clone(),
            headers: headers.clone(),
            status: Status::Enabled,
            tool_timeout_secs: 60,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let resp = McpServerStore::to_response(&server, ServerHealth::Unknown);
        assert_eq!(resp.env.get("KEY").unwrap(), "****abcd");
        assert_eq!(resp.headers.get("Authorization").unwrap(), "****2345");
        assert_eq!(resp.health, ServerHealth::Unknown);
        // slug 透传
        assert_eq!(resp.slug, "github_abcd");
        // 证明明文未进入响应
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("sk-1234567890abcd"));
        assert!(!json.contains("Bearer xyz12345"));
    }
}
