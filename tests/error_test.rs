//! 应用错误类型单元测试

use cortex_agent::error::AppError;

#[test]
fn test_display_config_error() {
    let err = AppError::ConfigError("missing api_key".to_string());
    assert_eq!(err.to_string(), "配置解析错误: missing api_key");
}

#[test]
fn test_display_network_error() {
    let err = AppError::NetworkError("connection refused".to_string());
    assert_eq!(err.to_string(), "网络请求错误: connection refused");
}

#[test]
fn test_display_database_error() {
    let err = AppError::DatabaseError("query timeout".to_string());
    assert_eq!(err.to_string(), "数据库操作错误: query timeout");
}

#[test]
fn test_display_business_error() {
    let err = AppError::BusinessError("duplicate feedback".to_string());
    assert_eq!(err.to_string(), "业务逻辑错误: duplicate feedback");
}

#[test]
fn test_display_serialization_error() {
    let err = AppError::SerializationError("invalid json".to_string());
    assert_eq!(err.to_string(), "序列化/反序列化错误: invalid json");
}

#[test]
fn test_display_file_error() {
    let err = AppError::FileError("permission denied".to_string());
    assert_eq!(err.to_string(), "文件操作错误: permission denied");
}

#[test]
fn test_display_unknown_error() {
    let err = AppError::Unknown("something went wrong".to_string());
    assert_eq!(err.to_string(), "未知错误: something went wrong");
}

#[test]
fn test_unsupported_db_type() {
    let err = AppError::UnsupportedDbType("sqlite".to_string());
    assert_eq!(
        err.to_string(),
        "不支持的数据库类型: 'sqlite' (仅支持postgres/mysql)"
    );
}

#[test]
fn test_ip_error() {
    let err = AppError::IpError("invalid format".to_string());
    assert_eq!(err.to_string(), "IP地址错误: invalid format");
}

#[test]
fn test_query_execution_failed() {
    let err = AppError::QueryExecutionFailed("syntax error".to_string());
    assert_eq!(err.to_string(), "SQL查询执行失败: syntax error");
}

#[test]
fn test_debug_format() {
    let err = AppError::ConfigError("test".to_string());
    let debug_str = format!("{:?}", err);
    assert!(debug_str.contains("ConfigError"));
    assert!(debug_str.contains("test"));
}

#[test]
fn test_error_source_chain() {
    // Test that errors can be converted from std::io::Error
    use std::io;
    let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
    let app_err: AppError = io_err.into();
    assert_eq!(app_err.to_string(), "文件操作错误: file not found");
}

#[test]
fn test_error_source_chain_json() {
    // Test that errors can be converted from serde_json::Error
    let json_err = serde_json::from_str::<String>("invalid json").unwrap_err();
    let app_err: AppError = json_err.into();
    assert!(app_err.to_string().contains("序列化"));
}

#[test]
fn test_error_source_chain_toml() {
    // Test that errors can be converted from toml::de::Error
    let toml_err = toml::from_str::<String>("invalid: toml").unwrap_err();
    let app_err: AppError = toml_err.into();
    assert!(app_err.to_string().contains("配置"));
}
