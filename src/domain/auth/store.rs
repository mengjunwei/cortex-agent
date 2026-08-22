//! 用户与第三方身份存储层
//!
//! 两张表（遵循 architecture.md §8）：
//! - `users`：用户主表，VARCHAR(36) UUID v7 主键
//! - `user_identities`：第三方身份绑定，`(provider, external_id)` 联合唯一
//!
//! 核心查询：`find_user_by_identity` 按 provider + external_id 定位用户（登录入口）。

use std::sync::Arc;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::{Store, new_id};

use super::enums::UserStatus;
use super::models::ExternalIdentity;

// ===== DB 行结构 =====

#[derive(Debug, Clone, QueryableByName)]
pub struct UserRow {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub name: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub avatar: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub email: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub status: i16,
    /// 本地账号登录名（仅本地注册用户有值，SSO 用户为 NULL）
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub username: Option<String>,
    /// argon2 密码哈希（PHC 串，仅本地用户有值）
    #[diesel(sql_type = sql_types::Nullable<sql_types::Text>)]
    pub password_hash: Option<String>,
    /// 是否管理员（0=否，1=是）
    #[diesel(sql_type = sql_types::Int2)]
    pub is_admin: i16,
}

impl UserRow {
    pub fn status_enum(&self) -> UserStatus {
        UserStatus::from_i16(self.status)
    }

    pub fn is_admin(&self) -> bool {
        self.is_admin == 1
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct IdentityRow {
    #[diesel(sql_type = sql_types::Varchar)]
    id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    provider: String,
    #[diesel(sql_type = sql_types::Varchar)]
    external_id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    user_id: String,
}

// ===== 存在性检查辅助行 =====

#[allow(dead_code)]
#[derive(Debug, Clone, QueryableByName)]
struct ExistsRow {
    #[diesel(sql_type = sql_types::Integer)]
    flag: i32,
}

// ===== Store =====

pub struct UserStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for UserStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl UserStore {
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        let store = Arc::new(Self { pool });
        Ok(store)
    }

    /// 按 provider + external_id 查找已绑定的用户（登录入口）
    ///
    /// 同时过滤已禁用用户（status = 0）。
    pub async fn find_user_by_identity(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<UserRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT u.id, u.name, u.avatar, u.email, u.status,
                   u.username, u.password_hash, u.is_admin
            FROM user_identities i
            INNER JOIN users u ON u.id = i.user_id
            WHERE i.provider = $1 AND i.external_id = $2 AND u.status = 1
            LIMIT 1
            "#,
        )
        .bind::<sql_types::Text, _>(provider)
        .bind::<sql_types::Text, _>(external_id)
        .get_results::<UserRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    /// 创建新用户并绑定第三方身份（登录注册一体化）
    ///
    /// 用户名/头像/邮箱取自 ExternalIdentity；若同一 provider:external_id 已存在，
    /// 则因联合唯一约束触发冲突，调用方应先调用 `find_user_by_identity`。
    pub async fn create_user_with_identity(
        &self,
        ext: &ExternalIdentity,
    ) -> Result<UserRow, AppError> {
        let user_id = new_id();
        let identity_id = new_id();
        let mut conn = self.get_conn().await?;

        diesel::sql_query("BEGIN").execute(&mut conn).await?;

        let tx: Result<(), AppError> = async {
            diesel::sql_query(
                r#"
                INSERT INTO users (id, name, avatar, email, status, is_admin)
                VALUES ($1, $2, $3, $4, 1, 0)
                "#,
            )
            .bind::<sql_types::Text, _>(&user_id)
            .bind::<sql_types::Text, _>(&ext.name)
            .bind::<sql_types::Text, _>(&ext.avatar)
            .bind::<sql_types::Text, _>(&ext.email)
            .execute(&mut conn)
            .await?;

            diesel::sql_query(
                r#"
                INSERT INTO user_identities (id, provider, external_id, user_id, name, avatar, email, raw_payload)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                "#,
            )
            .bind::<sql_types::Text, _>(&identity_id)
            .bind::<sql_types::Text, _>(&ext.provider)
            .bind::<sql_types::Text, _>(&ext.external_id)
            .bind::<sql_types::Text, _>(&user_id)
            .bind::<sql_types::Text, _>(&ext.name)
            .bind::<sql_types::Text, _>(&ext.avatar)
            .bind::<sql_types::Text, _>(&ext.email)
            .bind::<sql_types::Text, _>(&ext.raw_payload)
            .execute(&mut conn)
            .await?;
            Ok(())
        }
        .await;

        match tx {
            Ok(()) => {
                diesel::sql_query("COMMIT").execute(&mut conn).await?;
            }
            Err(e) => {
                let _ = diesel::sql_query("ROLLBACK").execute(&mut conn).await;
                return Err(e);
            }
        }

        Ok(UserRow {
            id: user_id,
            name: ext.name.clone(),
            avatar: ext.avatar.clone(),
            email: ext.email.clone(),
            status: UserStatus::Active.as_i16(),
            username: None,
            password_hash: None,
            is_admin: 0,
        })
    }

    /// 按 ID 获取用户
    pub async fn get_user(&self, user_id: &str) -> Result<Option<UserRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, name, avatar, email, status,
                   username, password_hash, is_admin
            FROM users
            WHERE id = $1
            LIMIT 1
            "#,
        )
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<UserRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    /// 按用户名查找本地账号用户（本地登录入口）。
    ///
    /// 仅匹配 username 非空的行。**不过滤 status**：禁用用户仍会被找到，
    /// 由 `login_local` 在密码校验通过后统一返回"账号已被禁用"，
    /// 这样既给合法用户清晰反馈，又不泄露信息（不知道密码就看不到禁用提示）。
    pub async fn find_user_by_username(&self, username: &str) -> Result<Option<UserRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, name, avatar, email, status,
                   username, password_hash, is_admin
            FROM users
            WHERE username = $1
            LIMIT 1
            "#,
        )
        .bind::<sql_types::Text, _>(username)
        .get_results::<UserRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    /// 创建本地账号用户（用户名密码登录）。
    ///
    /// - `password_hash`：argon2 PHC 串（调用方负责哈希）
    /// - `name`：显示名，为空时回退为 username
    /// - `is_admin`：是否管理员（首用户 bootstrap 时设为 true）
    ///
    /// 用户名冲突由 `uq_users_username` 唯一索引保证，触发时返回 `ConflictError`（→ code::CONFLICT）。
    pub async fn create_local_user(
        &self,
        username: &str,
        password_hash: &str,
        name: &str,
        is_admin: bool,
    ) -> Result<UserRow, AppError> {
        let user_id = new_id();
        let display_name = if name.trim().is_empty() {
            username
        } else {
            name
        };
        let mut conn = self.get_conn().await?;

        let result = diesel::sql_query(
            r#"
            INSERT INTO users (id, name, avatar, email, status, is_admin, username, password_hash)
            VALUES ($1, $2, '', '', 1, $3, $4, $5)
            "#,
        )
        .bind::<sql_types::Text, _>(&user_id)
        .bind::<sql_types::Text, _>(display_name)
        .bind::<sql_types::Int2, _>(if is_admin { 1 } else { 0 })
        .bind::<sql_types::Text, _>(username)
        .bind::<sql_types::Text, _>(password_hash)
        .execute(&mut conn)
        .await;

        match result {
            Ok(_) => Ok(UserRow {
                id: user_id,
                name: display_name.to_string(),
                avatar: String::new(),
                email: String::new(),
                status: UserStatus::Active.as_i16(),
                username: Some(username.to_string()),
                password_hash: Some(password_hash.to_string()),
                is_admin: if is_admin { 1 } else { 0 },
            }),
            Err(e) => {
                // 唯一约束冲突 → 业务层友好提示（ConflictError → code::CONFLICT → HTTP 409）
                let msg = format!("{e}");
                if msg.to_lowercase().contains("uq_users_username")
                    || msg.to_lowercase().contains("duplicate key")
                    || msg.to_lowercase().contains("unique")
                {
                    Err(AppError::ConflictError("用户名已被占用".into()))
                } else {
                    Err(AppError::from(e))
                }
            }
        }
    }

    /// 统计用户总数（判断是否需要首用户自动管理员 bootstrap）
    pub async fn count_users(&self) -> Result<i64, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query("SELECT COUNT(*) AS cnt FROM users")
            .get_result::<CountRow>(&mut conn)
            .await?;
        Ok(rows.cnt)
    }

    /// 读取用户 `updated_at` 的 Unix 时间戳（秒，浮点）。
    ///
    /// 复用为「会话凭证版本时间戳」：改密时 [`set_password`] 把 `updated_at` 推进到 `NOW()`，
    /// [`AuthService::verify_token`](super::AuthService::verify_token) 据此判断 token 是否在
    /// 改密前签发（`claims.iat < updated_at` → 旧会话失效），实现「改密后该账号全部会话失效」，
    /// 无需新增列或 Claims 字段（token 自带 `iat`）。用户不存在返回 `None`。
    pub async fn pwd_changed_at_epoch(&self, user_id: &str) -> Result<Option<f64>, AppError> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = sql_types::Double)]
            epoch: f64,
        }
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT EXTRACT(EPOCH FROM updated_at)::DOUBLE PRECISION AS epoch FROM users WHERE id = $1",
        )
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<Row>(&mut conn)
        .await?;
        Ok(rows.into_iter().next().map(|r| r.epoch))
    }

    /// 设置/更新用户密码（argon2 PHC），同时推进 `updated_at = NOW()`。
    ///
    /// `updated_at` 推进使该用户全部已有 JWT 会话立即失效（见 [`pwd_changed_at_epoch`]）。
    pub async fn set_password(&self, user_id: &str, password_hash: &str) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("UPDATE users SET password_hash = $2, updated_at = NOW() WHERE id = $1")
            .bind::<sql_types::Text, _>(user_id)
            .bind::<sql_types::Text, _>(password_hash)
            .execute(&mut conn)
            .await?;
        Ok(())
    }
}

/// COUNT 查询结果行
#[derive(Debug, Clone, QueryableByName)]
struct CountRow {
    #[diesel(sql_type = sql_types::BigInt)]
    cnt: i64,
}
