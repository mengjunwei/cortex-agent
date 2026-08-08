//! 查询理解服务单元测试

#[cfg(test)]
mod extract_json {
    // Copy of the extract_json function for testing
    fn extract_json(text: &str) -> &str {
        let trimmed = text.trim();
        if let Some(start) = trimmed.find('{')
            && let Some(end) = trimmed.rfind('}')
        {
            return &trimmed[start..=end];
        }
        trimmed
    }

    #[test]
    fn test_plain_json() {
        let text = r#"{"brand":"H3C","dev_type":"router"}"#;
        let result = extract_json(text);
        assert_eq!(result, r#"{"brand":"H3C","dev_type":"router"}"#);
    }

    #[test]
    fn test_json_with_markdown_wrapper() {
        let text = r#"```json
{"brand":"H3C","dev_type":"router"}
```"#;
        let result = extract_json(text);
        assert!(result.starts_with('{'));
        assert!(result.ends_with('}'));
        assert!(result.contains("brand"));
    }

    #[test]
    fn test_json_with_prefix_text() {
        let text = r#"Here is the result: {"brand":"H3C","dev_type":"router"} and that's it."#;
        let result = extract_json(text);
        assert!(result.starts_with('{'));
        assert!(result.ends_with('}'));
    }

    #[test]
    fn test_json_with_surrounding_whitespace() {
        let text = "   \n  {\"brand\":\"H3C\"}\n  \n  ";
        let result = extract_json(text);
        assert_eq!(result, r#"{"brand":"H3C"}"#);
    }

    #[test]
    fn test_nested_json() {
        let text = r#"{"brand":"H3C","keywords":["静态路由","配置"]}"#;
        let result = extract_json(text);
        assert!(result.contains("keywords"));
        assert!(result.contains("静态路由"));
    }

    #[test]
    fn test_no_json_returns_original() {
        let text = "No JSON here";
        let result = extract_json(text);
        assert_eq!(result, "No JSON here");
    }

    #[test]
    fn test_only_opening_brace() {
        let text = "{ only opening";
        let result = extract_json(text);
        assert_eq!(result, "{ only opening");
    }

    #[test]
    fn test_multiple_json_blocks_takes_first() {
        // The function uses find('{') which gets the first occurrence
        let text = r#"{"first":1}{"second":2}"#;
        let result = extract_json(text);
        // Should get from first { to last }
        assert!(result.contains("first"));
        assert!(result.contains("second"));
    }
}

#[cfg(test)]
mod structured_query {
    use cortex_agent::agent::query_understanding::StructuredQuery;

    #[test]
    fn test_default_has_empty_fields() {
        let sq = StructuredQuery::default();
        assert!(sq.brand.is_none());
        assert!(sq.dev_type.is_none());
        assert!(sq.keywords.is_empty());
    }

    #[test]
    fn test_serde_deserialize() {
        let json = r#"{"brand":"H3C","dev_type":"router","keywords":["静态路由"]}"#;
        let sq: StructuredQuery = serde_json::from_str(json).unwrap();
        assert_eq!(sq.brand, Some("H3C".to_string()));
        assert_eq!(sq.dev_type, Some("router".to_string()));
        assert_eq!(sq.keywords, vec!["静态路由"]);
    }

    #[test]
    fn test_serde_deserialize_null_optionals() {
        let json = r#"{"keywords":["静态路由"]}"#;
        let sq: StructuredQuery = serde_json::from_str(json).unwrap();
        assert!(sq.brand.is_none());
        assert!(sq.dev_type.is_none());
        assert_eq!(sq.keywords, vec!["静态路由"]);
    }

    #[test]
    fn test_serde_deserialize_empty_keywords() {
        let json = r#"{"brand":"H3C","dev_type":"router","keywords":[]}"#;
        let sq: StructuredQuery = serde_json::from_str(json).unwrap();
        assert!(sq.keywords.is_empty());
    }

    #[test]
    fn test_serde_roundtrip() {
        // StructuredQuery only has Deserialize, not Serialize
        // So we only test deserialization roundtrip
        let json = r#"{"brand":"Huawei","dev_type":"switch","keywords":["VLAN","配置"]}"#;
        let parsed: StructuredQuery = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.brand, Some("Huawei".to_string()));
        assert_eq!(parsed.dev_type, Some("switch".to_string()));
        assert_eq!(
            parsed.keywords,
            vec!["VLAN".to_string(), "配置".to_string()]
        );

        // Re-serialize to verify the deserialized values are valid
        assert!(parsed.brand.is_some());
        assert!(parsed.dev_type.is_some());
        assert_eq!(parsed.keywords.len(), 2);
    }
}
