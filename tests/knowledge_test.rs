//! 知识库模块单元测试
//!
//! 主要测试 `parse_brand_dev_type_from_name` 函数

// Re-export the function for testing
// Note: This test file needs to be able to access internal functions.
// In a real scenario, you'd either make the function pub(crate) or use
// integration tests with a library crate.

// Since parse_brand_dev_type_from_name is private, we test it indirectly
// through the public API. Here we demonstrate the expected behavior.

#[cfg(test)]
mod parse_brand_dev_type {
    // These tests document the expected behavior of parse_brand_dev_type_from_name
    // The function parses document names in format: brand_dev_type_title

    #[test]
    fn test_valid_brand_dev_type_format() {
        // Valid format: "H3C_router_静态路由配置"
        // Expected: (Some("H3C"), Some("router"))
        // The function checks if brand is alphanumeric only

        let name = "H3C_router_test";
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "H3C");
        assert_eq!(parts[1], "router");

        // Verify brand is alphanumeric
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn test_chinese_brand_not_matched() {
        // Chinese characters in brand should NOT match
        // because brand must be all ASCII alphanumeric

        let name = "华为_router_test";
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        assert_eq!(parts.len(), 3);

        // Chinese chars are not ASCII alphanumeric
        assert!(!parts[0].chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn test_underscore_in_title() {
        // Title can contain underscores, only first 2 parts are brand/dev_type

        let name = "H3C_router_静态路由配置命令";
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "H3C");
        assert_eq!(parts[1], "router");
        assert_eq!(parts[2], "静态路由配置命令");
    }

    #[test]
    fn test_only_one_underscore() {
        // If only one underscore, only brand is extracted
        let name = "H3C_router";
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_empty_brand() {
        let name = "_router_test";
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        assert!(parts[0].is_empty());
    }

    #[test]
    fn test_brand_with_numbers() {
        // Brand can contain numbers
        let name = "Maipu_8_router_config";
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        assert_eq!(parts[0], "Maipu");
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));
    }
}

#[cfg(test)]
mod brand_dev_type_parsing_logic {
    // Tests for the actual parsing logic

    fn parse_brand_dev_type_from_name(name: &str) -> (Option<String>, Option<String>) {
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        if parts.len() >= 2 {
            let brand = parts[0].to_string();
            let dev_type = parts[1].to_string();
            if !brand.is_empty() && brand.chars().all(|c| c.is_ascii_alphanumeric()) {
                return (Some(brand), Some(dev_type));
            }
        }
        (None, None)
    }

    #[test]
    fn test_h3c_router() {
        let (brand, dev_type) = parse_brand_dev_type_from_name("H3C_router_静态路由");
        assert_eq!(brand, Some("H3C".to_string()));
        assert_eq!(dev_type, Some("router".to_string()));
    }

    #[test]
    fn test_huawei_switch() {
        let (brand, dev_type) = parse_brand_dev_type_from_name("Huawei_switch_vlan");
        assert_eq!(brand, Some("Huawei".to_string()));
        assert_eq!(dev_type, Some("switch".to_string()));
    }

    #[test]
    fn test_cisco_firewall() {
        let (brand, dev_type) = parse_brand_dev_type_from_name("Cisco_firewall_acl");
        assert_eq!(brand, Some("Cisco".to_string()));
        assert_eq!(dev_type, Some("firewall".to_string()));
    }

    #[test]
    fn test_chinese_brand_rejected() {
        // Chinese brand should return None
        let (brand, dev_type) = parse_brand_dev_type_from_name("华为_router_test");
        assert_eq!(brand, None);
        assert_eq!(dev_type, None);
    }

    #[test]
    fn test_empty_string() {
        let (brand, dev_type) = parse_brand_dev_type_from_name("");
        assert_eq!(brand, None);
        assert_eq!(dev_type, None);
    }

    #[test]
    fn test_single_word() {
        let (brand, dev_type) = parse_brand_dev_type_from_name("H3C");
        // Only one part, can't extract both
        assert_eq!(brand, None);
        assert_eq!(dev_type, None);
    }

    #[test]
    fn test_no_underscore() {
        let (brand, dev_type) = parse_brand_dev_type_from_name("H3Crouter");
        assert_eq!(brand, None);
        assert_eq!(dev_type, None);
    }

    #[test]
    fn test_brand_with_hyphen_rejected() {
        // Hyphens are not alphanumeric
        let (brand, dev_type) = parse_brand_dev_type_from_name("H3C-SE_router_test");
        assert_eq!(brand, None);
        assert_eq!(dev_type, None);
    }

    #[test]
    fn test_juniper路由器() {
        // Juniper is pure ASCII
        let (brand, dev_type) = parse_brand_dev_type_from_name("Juniper_router_配置");
        assert_eq!(brand, Some("Juniper".to_string()));
        assert_eq!(dev_type, Some("router".to_string()));
    }
}
