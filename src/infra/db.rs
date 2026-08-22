//! 数据库连接池模块 — 基于 diesel-async + deadpool 的统一连接管理
//!
//! 提供 PostgreSQL / MySQL 的异步连接池，所有数据库操作共享同一个连接池实例。
//!
//! ## 使用方式
//!
//! ```ignore
//! // 1. main.rs 中初始化连接池
//! let pool = infra::db::init_db(&cfg.db).await?;
//!
//! // 2. 传递给需要 DB 访问的模块
//! let store = DocMetaStore::new(pool.clone());
//!
//! // 3. 模块内部从池中获取连接执行查询
//! let mut conn = self.pool.get().await?;
//! diesel::sql_query("SELECT ...").get_results::<MyRow>(&mut conn).await?;
//! ```

use crate::config::DbConfig;
use crate::error::AppError;
use deadpool::managed::PoolConfig;
#[cfg(all(feature = "mysql", not(feature = "postgres")))]
use diesel_async::AsyncMysqlConnection;
#[cfg(feature = "postgres")]
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::{Object, Pool};

#[cfg(feature = "postgres")]
/// PostgreSQL 数据库连接类型
pub type DbConnection = AsyncPgConnection;

#[cfg(all(feature = "mysql", not(feature = "postgres")))]
/// MySQL 数据库连接类型
pub type DbConnection = AsyncMysqlConnection;

/// 数据库连接池类型
pub type DbPool = Pool<DbConnection>;

/// 从连接池取出的数据库连接类型
pub type DbPooledConnection = Object<DbConnection>;

/// 根据配置创建数据库连接池
///
/// 验证数据库类型，创建对应数据库的 deadpool 连接池。
///
/// # 参数
/// - `conn_info`：数据库连接配置（`[db]` 段）
///
/// # 错误
/// - `AppError::UnsupportedDbType`：不支持的数据库类型
/// - `AppError::PoolInitFailed`：连接池创建失败
pub async fn init_db(conn_info: &DbConfig) -> Result<DbPool, AppError> {
    // 验证数据库类型
    let db_type_lower = conn_info.db_type.to_lowercase();
    match db_type_lower.as_str() {
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => (),
        #[cfg(all(feature = "mysql", not(feature = "postgres")))]
        "mysql" => (),
        other => return Err(AppError::UnsupportedDbType(other.to_string())),
    }

    // 配置连接池参数
    let pool_config = PoolConfig {
        max_size: conn_info.pool_max_size as usize,
        ..Default::default()
    };

    // 初始化连接池
    let pool: DbPool = match db_type_lower.as_str() {
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => {
            let encoded_user = urlencoding::encode(&conn_info.user);
            let encoded_password = urlencoding::encode(&conn_info.password);
            let config_url = format!(
                "postgres://{}:{}@{}:{}/{}",
                encoded_user, encoded_password, conn_info.host, conn_info.port, conn_info.db
            );

            tracing::info!(
                "[db] PostgreSQL 连接配置: host={}, port={}, user={}, db={}",
                conn_info.host,
                conn_info.port,
                conn_info.user,
                conn_info.db
            );
            tracing::debug!("[db] 完整连接字符串: {}", config_url);

            let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&config_url);
            let pool = Pool::builder(manager)
                .config(pool_config)
                .build()
                .map_err(|e| {
                    tracing::error!("[db] 连接池创建失败: {}", e);
                    AppError::PoolInitFailed(e.into())
                })?;

            tracing::info!("[db] 尝试获取数据库连接...");
            match pool.get().await {
                Ok(_conn) => tracing::info!("[db] 数据库连接成功!"),
                Err(e) => {
                    tracing::error!("[db] 数据库连接失败: {}", e);
                    return Err(AppError::PoolInitFailed(e.into()));
                }
            }

            pool
        }

        #[cfg(all(feature = "mysql", not(feature = "postgres")))]
        "mysql" => {
            let manager =
                AsyncDieselConnectionManager::<AsyncMysqlConnection>::new(&conn_info.url());
            Pool::builder(manager)
                .config(pool_config)
                .build()
                .map_err(|e| AppError::PoolInitFailed(e.into()))?
        }

        _ => unreachable!("已在前面验证过数据库类型"),
    };

    tracing::info!(
        "[db] 数据库连接池初始化成功 (type={}, max_size={}, timeout={}s)",
        db_type_lower,
        conn_info.pool_max_size,
        conn_info.pool_timeout
    );
    Ok(pool)
}
