//! 应用错误类型定义模块
//!
//! 使用 `thiserror` 定义统一的错误枚举 `AppError`，覆盖配置、数据库、网络、
//! 文件、序列化、业务逻辑等错误场景，并实现常见错误类型的 `From` 转换。

use diesel::result::Error as DieselError;
use diesel_async::pooled_connection::deadpool::PoolError as DieselPoolError;
use std::error::Error;
use thiserror::Error;

/// 应用统一错误类型
///
/// 所有模块返回 `Result<T, AppError>`，通过 `?` 自动转换底层错误。
#[derive(Error, Debug)]
pub enum AppError {
    /// 不支持的数据库类型
    #[error("不支持的数据库类型: '{0}' (仅支持postgres/mysql)")]
    UnsupportedDbType(String),

    /// 数据库连接池初始化失败
    #[error("数据库连接池初始化失败: {0}")]
    PoolInitFailed(#[from] Box<dyn Error + Send + Sync>),

    /// 从连接池获取连接失败
    #[error("从连接池获取连接失败: {0}")]
    GetConnectionFailed(#[from] DieselPoolError),

    /// Diesel ORM查询错误
    #[error("Diesel查询错误: {0}")]
    DieselQueryError(#[from] DieselError),
    /// IP地址格式错误
    #[error("IP地址错误: {0}")]
    IpError(String),

    /// SQL查询执行失败
    #[error("SQL查询执行失败: {0}")]
    QueryExecutionFailed(String),

    /// 配置解析错误（如 TOML 解析失败、必填项缺失）
    #[error("配置解析错误: {0}")]
    ConfigError(String),

    /// 数据库操作错误（PostgreSQL 连接/查询失败）
    #[error("数据库操作错误: {0}")]
    DatabaseError(String),

    /// 网络请求错误（HTTP 调用 Dify / LLM API 失败）
    #[error("网络请求错误: {0}")]
    NetworkError(String),

    /// 文件操作错误（读写本地文件失败）
    #[error("文件操作错误: {0}")]
    FileError(String),

    /// 序列化/反序列化错误（JSON / TOML 解析失败）
    #[error("序列化/反序列化错误: {0}")]
    SerializationError(String),

    /// 业务逻辑错误（如重复反馈、参数校验失败）
    #[error("业务逻辑错误: {0}")]
    BusinessError(String),

    /// 资源冲突（如唯一约束冲突：用户名已被占用）
    #[error("资源冲突: {0}")]
    ConflictError(String),

    /// 目标资源不存在
    #[error("资源不存在: {0}")]
    NotFoundError(String),

    /// 对象存储操作错误（S3/RustFS 读写、签名失败）
    #[error("对象存储错误: {0}")]
    ObjectStoreError(String),

    /// 未知错误（兜底）
    #[error("未知错误: {0}")]
    Unknown(String),
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::FileError(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::SerializationError(e.to_string())
    }
}

impl From<toml::de::Error> for AppError {
    fn from(e: toml::de::Error) -> Self {
        AppError::ConfigError(e.to_string())
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Unknown(e.to_string())
    }
}

impl From<tokio_postgres::Error> for AppError {
    fn from(e: tokio_postgres::Error) -> Self {
        AppError::DatabaseError(e.to_string())
    }
}
