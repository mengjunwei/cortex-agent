//! 统一响应封装 — 所有 HTTP/GraphQL 业务返回值的信封结构
//!
//! ## 信封结构
//!
//! 所有业务接口统一返回：
//!
//! ```json
//! { "code": 0, "message": "", "data": { ... } }
//! ```
//!
//! - `code == 0` 表示成功；非 0 表示错误，每个码对应一类错误（见 [`code`]）。
//! - `message` 成功时为空字符串，失败时为可展示给人的错误描述。
//! - `data` 承载业务 payload；失败时为 `null`。
//!
//! ## 与 GraphQL 的关系
//!
//! GraphQL resolver 返回 `Json` 标量，其内部值即为本模块生成的信封 `serde_json::Value`。
//! 前端 `gql()` 解包 GraphQL `{ data, errors }` 后，拿到本信封，再拆出
//! `{ data, code, message }` 供调用方使用。

use serde_json::{Value, json};

use crate::error::AppError;

/// 业务错误码常量（每类错误一个码，按千位分段）
///
/// - `0`      成功
/// - `1xxx`   参数 / 请求类（格式错误、缺失、非法）
/// - `2xxx`   业务逻辑类（规则冲突、资源不存在、状态非法）
/// - `3xxx`   数据层（数据库连接 / 查询 / 持久化）
/// - `4xxx`   外部依赖（HTTP 上游 / LLM / 超时）
/// - `5xxx`   系统级（配置、内部异常、未知）
///
/// 注：部分码为前后端约定的预留分类，当前 handler 暂未触发，
/// 保留全集以稳定契约，故允许 dead_code。
#[allow(dead_code)]
pub mod code {
    /// 成功
    pub const OK: i32 = 0;

    // ---- 1xxx 参数 / 请求 ----
    /// 参数校验失败（缺失、非法值）
    pub const INVALID_PARAMS: i32 = 1001;
    /// 未认证(未登录)—— 与 HTTP 401 对应
    pub const UNAUTHORIZED: i32 = 1002;
    /// 入参反序列化 / 解析失败
    pub const PARSE_ERROR: i32 = 1002;

    // ---- 2xxx 业务逻辑 ----
    /// 通用业务规则错误
    pub const BUSINESS: i32 = 2001;
    /// 目标资源不存在
    pub const NOT_FOUND: i32 = 2002;
    /// 冲突（如唯一约束、重复操作）
    pub const CONFLICT: i32 = 2003;

    // ---- 3xxx 数据层 ----
    /// 数据库（连接 / 查询 / 持久化）错误
    pub const DATABASE: i32 = 3001;

    // ---- 4xxx 外部依赖 ----
    /// 网络 / 上游 HTTP（如 Dify）错误
    pub const NETWORK: i32 = 4001;
    /// LLM 相关错误（模型解析、调用失败）
    pub const LLM: i32 = 4002;
    /// 超时
    pub const TIMEOUT: i32 = 4003;

    // ---- 5xxx 系统级 ----
    /// 内部错误（配置、文件、初始化）
    pub const INTERNAL: i32 = 5001;
    /// 未知兜底
    pub const UNKNOWN: i32 = 5999;
}

/// 构造成功响应：`{ "code": 0, "message": "", "data": <data> }`
pub fn ok(data: Value) -> Value {
    json!({ "code": code::OK, "message": "", "data": data })
}

/// 构造失败响应：`{ "code": <code>, "message": <message>, "data": null }`
pub fn err(code: i32, message: impl Into<String>) -> Value {
    json!({ "code": code, "message": message.into(), "data": Value::Null })
}

/// 从 [`AppError`] 构造失败响应。
///
/// 错误码由 [`AppError::code`] 映射；消息剔除 `业务逻辑错误:` / `资源冲突:` /
/// `资源不存在:` 前缀（半角冒号，须与 [`AppError`] 各变体 `#[error]` 字面量对齐），
/// 以便直接展示给用户。
pub fn from_app_error(e: &AppError) -> Value {
    let msg = e.to_string();
    let text = msg
        .trim_start_matches("业务逻辑错误:")
        .trim_start_matches("资源冲突:")
        .trim_start_matches("资源不存在:")
        .trim_start();
    err(e.code(), text)
}

/// 把 [`AppError`] 映射到对应的业务错误码。
///
/// 实现为 `AppError` 的 inherent method（Rust 允许 inherent impl 分布在同一 crate 的不同模块），
/// 使所有业务 handler 可用 `e.code()` 获取分类码。
impl AppError {
    pub fn code(&self) -> i32 {
        match self {
            AppError::SerializationError(_) | AppError::IpError(_) => code::INVALID_PARAMS,
            AppError::BusinessError(_) => code::BUSINESS,
            AppError::ConflictError(_) => code::CONFLICT,
            AppError::NotFoundError(_) => code::NOT_FOUND,
            AppError::DatabaseError(_)
            | AppError::DieselQueryError(_)
            | AppError::QueryExecutionFailed(_)
            | AppError::GetConnectionFailed(_) => code::DATABASE,
            AppError::NetworkError(_) => code::NETWORK,
            AppError::UnsupportedDbType(_)
            | AppError::PoolInitFailed(_)
            | AppError::ConfigError(_)
            | AppError::FileError(_)
            | AppError::ObjectStoreError(_) => code::INTERNAL,
            AppError::Unknown(_) => code::UNKNOWN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;

    /// 取出信封中的 message 字段，断言其为给定字符串。
    fn message_of(v: &Value) -> &str {
        v["message"].as_str().unwrap_or_else(|| {
            panic!("信封缺少字符串 message 字段: {v}");
        })
    }

    /// `BusinessError` 经 `from_app_error` 应剔除「业务逻辑错误:」前缀，
    /// 返回干净 message 与对应业务码（BUSINESS = 2001）。
    #[test]
    fn strips_business_error_prefix() {
        let v = from_app_error(&AppError::BusinessError("模型不存在".to_string()));
        let msg = message_of(&v);
        assert_eq!(msg, "模型不存在");
        assert!(
            !msg.contains("业务逻辑错误"),
            "不应残留「业务逻辑错误」前缀，实际: {msg}"
        );
        assert_eq!(v["code"].as_i64(), Some(code::BUSINESS as i64));
        assert!(v["data"].is_null(), "失败响应 data 应为 null");
    }

    /// `ConflictError` 经 `from_app_error` 应剔除「资源冲突:」前缀，
    /// 返回干净 message 与对应业务码（CONFLICT = 2003）。
    #[test]
    fn strips_conflict_error_prefix() {
        let v = from_app_error(&AppError::ConflictError("名称重复".to_string()));
        let msg = message_of(&v);
        assert_eq!(msg, "名称重复");
        assert!(
            !msg.contains("资源冲突"),
            "不应残留「资源冲突」前缀，实际: {msg}"
        );
        assert_eq!(v["code"].as_i64(), Some(code::CONFLICT as i64));
    }

    /// `NotFoundError` 经 `from_app_error` 应剔除「资源不存在:」前缀，
    /// 返回干净 message 与对应业务码（NOT_FOUND = 2002）。
    #[test]
    fn strips_not_found_error_prefix() {
        let v = from_app_error(&AppError::NotFoundError("无此资源".to_string()));
        let msg = message_of(&v);
        assert_eq!(msg, "无此资源");
        assert!(
            !msg.contains("资源不存在"),
            "不应残留「资源不存在」前缀，实际: {msg}"
        );
        assert_eq!(v["code"].as_i64(), Some(code::NOT_FOUND as i64));
    }

    /// 其他不含前缀的变体（如 `Unknown`）应原样保留 message，不被误删。
    #[test]
    fn keeps_message_without_known_prefix() {
        let v = from_app_error(&AppError::Unknown("boom".to_string()));
        assert_eq!(message_of(&v), "未知错误: boom");
        assert_eq!(v["code"].as_i64(), Some(code::UNKNOWN as i64));
    }
}
