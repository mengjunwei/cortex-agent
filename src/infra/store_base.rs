//! Store 公共基础设施：消除各 domain store 重复的样板代码。
//!
//! 各 `<Entity>Store` 曾各自重复实现连接获取（`get_conn`）、ID 生成（`new_id`）、
//! 唯一键判定（`is_unique_violation`）。本模块集中提供这些公共能力：
//! - 自由函数 [`new_id`] / [`is_unique_violation`]：无状态工具，直接调用。
//! - [`Store`] trait：实现 `pool()` 即获得默认 `get_conn()`，消除连接获取样板。

use crate::error::AppError;
use crate::infra::db::{DbPool, DbPooledConnection};
use uuid::Uuid;

/// 生成 UUID v7 字符串（应用层 ID 生成，架构 §8.1）
pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

/// 判断 diesel 错误是否为唯一键冲突（用于 insert / upsert 的错误分流）
pub fn is_unique_violation(e: &diesel::result::Error) -> bool {
    matches!(
        e,
        diesel::result::Error::DatabaseError(diesel::result::DatabaseErrorKind::UniqueViolation, _)
    )
}

/// 所有 domain store 的公共能力：持有连接池 + 获取连接。
///
/// store 只需实现 `pool()`，即获得默认的 `get_conn()`：
///
/// ```ignore
/// #[async_trait::async_trait]
/// impl Store for MyStore {
///     fn pool(&self) -> &DbPool { &self.pool }
/// }
/// ```
#[async_trait::async_trait]
pub trait Store {
    /// 共享的数据库连接池引用
    fn pool(&self) -> &DbPool;

    /// 从连接池获取一个连接（默认实现：`pool.get()` + `AppError` 转换）
    async fn get_conn(&self) -> Result<DbPooledConnection, AppError> {
        self.pool().get().await.map_err(AppError::from)
    }
}
