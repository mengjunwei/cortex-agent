//! 失败原因清洗：去 AppError Display 前缀、折叠空白、截断超长文本。

/// 去掉 AppError Display 前缀（如"网络请求错误:"、"业务逻辑错误:"），避免重复拼接。
fn strip_app_error_prefix(msg: &str) -> &str {
    const PREFIXES: &[&str] = &[
        "网络请求错误: ",
        "业务逻辑错误: ",
        "数据库操作错误: ",
        "文件操作错误: ",
        "序列化/反序列化错误: ",
        "资源冲突: ",
        "资源不存在: ",
        "对象存储错误: ",
        "配置解析错误: ",
        "未知错误: ",
    ];
    for p in PREFIXES {
        if let Some(rest) = msg.strip_prefix(p) {
            return rest;
        }
    }
    msg
}

pub(super) fn sanitize_reason(raw: &str) -> String {
    let stripped = strip_app_error_prefix(raw);
    let cleaned = stripped.replace(['\n', '\r'], " ");
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() > 500 {
        let truncated: String = cleaned.chars().take(500).collect();
        format!("{truncated}...")
    } else {
        cleaned
    }
}

// ============================== Tests ==============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_reason_truncates_long() {
        let long = "x".repeat(600);
        let s = sanitize_reason(&long);
        assert!(s.chars().count() <= 503);
        assert!(s.ends_with("..."));
    }

    #[test]
    fn sanitize_reason_collapses_whitespace() {
        let s = sanitize_reason("line1\nline2\r\nline3");
        assert_eq!(s, "line1 line2 line3");
    }

    #[test]
    fn sanitize_reason_short_passthrough() {
        let s = sanitize_reason("connection refused");
        assert_eq!(s, "connection refused");
    }

    #[test]
    fn sanitize_reason_strips_app_error_prefix() {
        let s = sanitize_reason("网络请求错误: stdio MCP 握手失败: connection closed");
        assert_eq!(s, "stdio MCP 握手失败: connection closed");
    }

    #[test]
    fn strip_app_error_prefix_passthrough() {
        assert_eq!(strip_app_error_prefix("some raw error"), "some raw error");
    }
}
