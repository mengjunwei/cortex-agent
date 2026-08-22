//! MCP Server 数据存储层（diesel-async）
//!
//! 范式同 [`crate::domain::model_provider::store`]：
//! - 主键 UUID v7 字符串；枚举 `status`/`transport` 以 SMALLINT 存储
//! - 敏感字段（env/headers）整体 JSON 后 AES-256-GCM 加密存储
//! - `args` / 掩码 map 以 TEXT 存 JSON
//! - 建表 DDL 见 `migrations/schema.sql`（架构 §8.5）

use std::sync::Arc;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::domain::mcp::dto::{CreateMcpServerInput, McpServerResponse, UpdateMcpServerInput};
use crate::domain::mcp::enums::{Status, TransportKind};
use crate::domain::mcp::models::{McpServer, ServerHealth, mask_map};
use crate::error::AppError;
use crate::infra::db::{DbPool, DbPooledConnection};
use crate::infra::store_base::{Store, is_unique_violation, new_id};
use crate::security::crypto::AesCodec;

mod helpers;

use helpers::*;

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
    #[diesel(sql_type = sql_types::Varchar)]
    pub user_id: String,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// mcp_servers 查询统一列
const MCP_COLS: &str = "id, name, slug, transport, endpoint, args, env_enc, env_mask, \
     headers_enc, headers_mask, status, tool_timeout_secs, user_id, created_at, updated_at";

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

/// 单列 user_id 查询行（反查归属人）
#[derive(Debug, Clone, QueryableByName)]
struct OwnerIdRow {
    #[diesel(sql_type = sql_types::Varchar)]
    user_id: String,
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
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        let codec = crate::security::crypto::AesCodec::from_secrets();
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

    pub async fn create(
        &self,
        input: &CreateMcpServerInput,
        user_id: &str,
    ) -> Result<McpServer, AppError> {
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
                 env_enc, env_mask, headers_enc, headers_mask, status, tool_timeout_secs, user_id)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
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
        .bind::<sql_types::Text, _>(user_id)
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

    /// 更新；env/headers 为 None 时保持原密文不变，为 Some 时按键合并（见 dto.rs 文件头）。
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
        // 取既有行：同时拿到存在性与 env/headers 密文（合并前严格解密）
        let Some(row) = self.get_row_by_id_conn(&mut conn, id).await? else {
            return Ok(None);
        };
        // 按键合并解析为最终明文 map，后续 SQL 走整体覆盖。
        // 涉及覆盖的字段解密失败时显式报错、绝不静默当空 map 合并——
        // 否则前端回显空（脱敏响应为 {}）→ 提交空键集 → 未动过的密钥被整体清空
        // （对齐 assistant revealAssistantEnvVars 的防覆盖丢密钥约定，docs/api.md）
        let env = match &input.env {
            Some(m) => Some(merge_secret_map(
                &strict_decrypt_map(&self.codec, &row.env_enc, "env")?,
                m,
            )),
            None => None,
        };
        let headers = match &input.headers {
            Some(m) => Some(merge_secret_map(
                &strict_decrypt_map(&self.codec, &row.headers_enc, "headers")?,
                m,
            )),
            None => None,
        };

        let args_json = serde_json::to_string(&input.args)?;
        // 公共字段提取为局部变量，供 bind_common 统一绑定
        let name = input.name.trim();
        let endpoint = input.endpoint.trim();
        let transport = input.transport.as_i16();
        let status = input.status.as_i16();

        // 按 env/headers 是否提供，分四种 SQL（避免动态拼 SQL）
        match (&env, &headers) {
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

    /// 按归属隔离列表（完全隔离）：普通用户仅自己的；管理员（admin_view）看全部。
    pub async fn list_for_owner(
        &self,
        user_id: &str,
        admin_view: bool,
    ) -> Result<Vec<McpServer>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(format!(
            "SELECT {MCP_COLS} FROM mcp_servers WHERE ($1 OR user_id = $2) ORDER BY created_at ASC"
        ))
        .bind::<sql_types::Bool, _>(admin_view)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<McpServerRow>(&mut conn)
        .await?;
        rows.into_iter().map(|r| self.row_to_server(r)).collect()
    }

    /// 反查归属人（跨实体引用校验用）
    pub async fn get_owner(&self, id: &str) -> Result<Option<String>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query("SELECT user_id FROM mcp_servers WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .get_results::<OwnerIdRow>(&mut conn)
            .await?;
        Ok(rows.into_iter().next().map(|r| r.user_id))
    }

    async fn list_all_conn(
        &self,
        conn: &mut DbPooledConnection,
    ) -> Result<Vec<McpServer>, AppError> {
        let rows = diesel::sql_query(format!(
            "SELECT {MCP_COLS} FROM mcp_servers ORDER BY created_at ASC"
        ))
        .get_results::<McpServerRow>(conn)
        .await?;
        rows.into_iter().map(|r| self.row_to_server(r)).collect()
    }

    /// 分页查询 MCP 服务（按归属隔离）：普通用户仅自己的；管理员（admin_view）看全部。
    pub async fn list_paged(
        &self,
        page: usize,
        page_size: usize,
        keyword: Option<&str>,
        user_id: &str,
        admin_view: bool,
    ) -> Result<(Vec<McpServer>, i64), AppError> {
        let mut conn = self.get_conn().await?;
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = ((page - 1) * page_size) as i64;

        // 基础归属过滤：($1=admin_view OR user_id=$2)；keyword 可选叠加
        // 参数顺序：count/list 均为 $1=admin_view, $2=user_id, ($3=kw 若有), 然后 limit/offset
        let (kw_clause, kw_bind) = match keyword.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(kw) => (
                " AND (LOWER(name) LIKE LOWER($3) OR LOWER(slug) LIKE LOWER($3) OR LOWER(endpoint) LIKE LOWER($3))".to_string(),
                Some(format!("%{}%", kw)),
            ),
            None => (String::new(), None),
        };
        let where_clause = format!(" WHERE ($1 OR user_id = $2){kw_clause}");

        // count
        let count_sql = format!("SELECT COUNT(*) AS count FROM mcp_servers{where_clause}");
        let total: i64 = if let Some(ref kw) = kw_bind {
            diesel::sql_query(&count_sql)
                .bind::<sql_types::Bool, _>(admin_view)
                .bind::<sql_types::Text, _>(user_id)
                .bind::<sql_types::Text, _>(kw)
                .get_result::<CountRow>(&mut conn)
                .await?
                .count
        } else {
            diesel::sql_query(&count_sql)
                .bind::<sql_types::Bool, _>(admin_view)
                .bind::<sql_types::Text, _>(user_id)
                .get_result::<CountRow>(&mut conn)
                .await?
                .count
        };

        // list：limit/offset 绑定在 kw 之后（$3/$4 或 $4/$5）
        let list_sql = format!(
            "SELECT {MCP_COLS} FROM mcp_servers{where_clause} ORDER BY created_at ASC LIMIT $3 OFFSET $4"
        );
        let rows = if let Some(ref kw) = kw_bind {
            let list_sql = format!(
                "SELECT {MCP_COLS} FROM mcp_servers{where_clause} ORDER BY created_at ASC LIMIT $4 OFFSET $5"
            );
            diesel::sql_query(&list_sql)
                .bind::<sql_types::Bool, _>(admin_view)
                .bind::<sql_types::Text, _>(user_id)
                .bind::<sql_types::Text, _>(kw)
                .bind::<sql_types::Int8, _>(page_size as i64)
                .bind::<sql_types::Int8, _>(offset)
                .get_results::<McpServerRow>(&mut conn)
                .await?
        } else {
            diesel::sql_query(&list_sql)
                .bind::<sql_types::Bool, _>(admin_view)
                .bind::<sql_types::Text, _>(user_id)
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

    /// 按 ID 列表批量设置状态（按归属隔离）。返回受影响行数。
    pub async fn set_status_batch(
        &self,
        ids: &[String],
        status_val: i16,
        user_id: &str,
        is_admin: bool,
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
            "UPDATE mcp_servers SET status = $2, updated_at = NOW() \
             WHERE id = ANY(SELECT json_array_elements_text($1::json)) AND ($3 OR user_id = $4)",
        )
        .bind::<sql_types::Text, _>(&json_ids.to_string())
        .bind::<sql_types::Int2, _>(status_val)
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .execute(&mut conn)
        .await?;
        Ok(affected)
    }

    /// 按筛选条件批量设置状态（跨页全选，按归属隔离）。返回受影响行数。
    pub async fn set_status_by_filter(
        &self,
        keyword: Option<&str>,
        status_val: i16,
        user_id: &str,
        is_admin: bool,
    ) -> Result<usize, AppError> {
        let mut conn = self.get_conn().await?;
        let affected = match keyword.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(kw) => {
                let pattern = format!("%{}%", kw);
                diesel::sql_query(
                    "UPDATE mcp_servers SET status = $2, updated_at = NOW() \
                     WHERE (LOWER(name) LIKE LOWER($1) OR LOWER(slug) LIKE LOWER($1) OR LOWER(endpoint) LIKE LOWER($1)) \
                     AND ($3 OR user_id = $4)",
                )
                .bind::<sql_types::Text, _>(&pattern)
                .bind::<sql_types::Int2, _>(status_val)
                .bind::<sql_types::Bool, _>(is_admin)
                .bind::<sql_types::Text, _>(user_id)
                .execute(&mut conn)
                .await?
            }
            None => diesel::sql_query(
                "UPDATE mcp_servers SET status = $1, updated_at = NOW() WHERE ($2 OR user_id = $3)",
            )
            .bind::<sql_types::Int2, _>(status_val)
            .bind::<sql_types::Bool, _>(is_admin)
            .bind::<sql_types::Text, _>(user_id)
            .execute(&mut conn)
            .await?,
        };
        Ok(affected)
    }

    /// 按 ID 列表批量删除（含级联清理助手引用，按归属隔离）。返回受影响行数。
    pub async fn delete_batch(
        &self,
        ids: &[String],
        user_id: &str,
        is_admin: bool,
    ) -> Result<usize, AppError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.get_conn().await?;
        // 仅对归属匹配的 id 做级联清理：先筛出本人/管理员可见的 id
        let json_ids = serde_json::Value::Array(
            ids.iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
        let owned: Vec<String> = diesel::sql_query(
            "SELECT id FROM mcp_servers \
             WHERE id = ANY(SELECT json_array_elements_text($1::json)) AND ($2 OR user_id = $3)",
        )
        .bind::<sql_types::Text, _>(&json_ids.to_string())
        .bind::<sql_types::Bool, _>(is_admin)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<IdRow>(&mut conn)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();
        if owned.is_empty() {
            return Ok(0);
        }
        for id in &owned {
            purge_mcp_from_assistants(&mut conn, id).await?;
        }
        let json_owned = serde_json::Value::Array(
            owned
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
        let affected = diesel::sql_query(
            "DELETE FROM mcp_servers WHERE id = ANY(SELECT json_array_elements_text($1::json))",
        )
        .bind::<sql_types::Text, _>(&json_owned.to_string())
        .execute(&mut conn)
        .await?;
        Ok(affected)
    }

    /// 按筛选条件批量删除（跨页全选，含级联清理，按归属隔离）。返回受影响行数。
    pub async fn delete_by_filter(
        &self,
        keyword: Option<&str>,
        user_id: &str,
        is_admin: bool,
    ) -> Result<usize, AppError> {
        let mut conn = self.get_conn().await?;
        // 先查出归属匹配的 id 做级联清理（keyword 可选叠加）
        let ids: Vec<String> = match keyword.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(kw) => {
                let pattern = format!("%{}%", kw);
                diesel::sql_query(
                    "SELECT id FROM mcp_servers \
                     WHERE (LOWER(name) LIKE LOWER($1) OR LOWER(slug) LIKE LOWER($1) OR LOWER(endpoint) LIKE LOWER($1)) \
                     AND ($2 OR user_id = $3)",
                )
                .bind::<sql_types::Text, _>(&pattern)
                .bind::<sql_types::Bool, _>(is_admin)
                .bind::<sql_types::Text, _>(user_id)
                .get_results::<IdRow>(&mut conn)
                .await?
                .into_iter()
                .map(|r| r.id)
                .collect()
            }
            None => diesel::sql_query("SELECT id FROM mcp_servers WHERE ($1 OR user_id = $2)")
                .bind::<sql_types::Bool, _>(is_admin)
                .bind::<sql_types::Text, _>(user_id)
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
        match self.get_row_by_id_conn(conn, id).await? {
            Some(r) => Ok(Some(self.row_to_server(r)?)),
            None => Ok(None),
        }
    }

    async fn get_row_by_id_conn(
        &self,
        conn: &mut DbPooledConnection,
        id: &str,
    ) -> Result<Option<McpServerRow>, AppError> {
        let rows = diesel::sql_query(format!("SELECT {MCP_COLS} FROM mcp_servers WHERE id = $1"))
            .bind::<sql_types::Text, _>(id)
            .get_results::<McpServerRow>(conn)
            .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn get_by_slug(&self, slug: &str) -> Result<Option<McpServer>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(format!(
            "SELECT {MCP_COLS} FROM mcp_servers WHERE slug = $1"
        ))
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
            user_id: r.user_id,
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
            user_id: server.user_id.clone(),
            created_at: server.created_at.to_rfc3339(),
            updated_at: server.updated_at.to_rfc3339(),
            health,
        }
    }
}

