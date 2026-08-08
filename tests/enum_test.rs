//! 枚举类型单元测试

use cortex_agent::domain::enum_def::{OpType, RiskLevel};

mod op_type {
    use super::*;

    #[test]
    fn test_parse_query() {
        assert_eq!("query".parse::<OpType>().unwrap(), OpType::Query);
        assert_eq!("Query".parse::<OpType>().unwrap(), OpType::Query);
        assert_eq!("QUERY".parse::<OpType>().unwrap(), OpType::Query);
    }

    #[test]
    fn test_parse_modify() {
        assert_eq!("modify".parse::<OpType>().unwrap(), OpType::Modify);
        assert_eq!("Modify".parse::<OpType>().unwrap(), OpType::Modify);
    }

    #[test]
    fn test_parse_dangerous() {
        assert_eq!("dangerous".parse::<OpType>().unwrap(), OpType::Dangerous);
        assert_eq!("Dangerous".parse::<OpType>().unwrap(), OpType::Dangerous);
    }

    #[test]
    fn test_parse_invalid() {
        let result: Result<OpType, String> = "invalid".parse();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid OpType"));
    }

    #[test]
    fn test_display() {
        assert_eq!(OpType::Query.to_string(), "query");
        assert_eq!(OpType::Modify.to_string(), "modify");
        assert_eq!(OpType::Dangerous.to_string(), "dangerous");
    }

    #[test]
    fn test_serde_serialize() {
        // serde uses PascalCase variant names by default
        let json = serde_json::to_string(&OpType::Query).unwrap();
        assert_eq!(json, "\"Query\"");

        let json = serde_json::to_string(&OpType::Modify).unwrap();
        assert_eq!(json, "\"Modify\"");

        let json = serde_json::to_string(&OpType::Dangerous).unwrap();
        assert_eq!(json, "\"Dangerous\"");
    }

    #[test]
    fn test_serde_deserialize() {
        // serde deserializes PascalCase variant names
        let op: OpType = serde_json::from_str("\"Query\"").unwrap();
        assert_eq!(op, OpType::Query);

        let op: OpType = serde_json::from_str("\"Modify\"").unwrap();
        assert_eq!(op, OpType::Modify);

        let op: OpType = serde_json::from_str("\"Dangerous\"").unwrap();
        assert_eq!(op, OpType::Dangerous);
    }
}

mod risk_level {
    use super::*;

    #[test]
    fn test_parse_low() {
        assert_eq!("low".parse::<RiskLevel>().unwrap(), RiskLevel::Low);
        assert_eq!("Low".parse::<RiskLevel>().unwrap(), RiskLevel::Low);
    }

    #[test]
    fn test_parse_medium() {
        assert_eq!("medium".parse::<RiskLevel>().unwrap(), RiskLevel::Medium);
    }

    #[test]
    fn test_parse_high() {
        assert_eq!("high".parse::<RiskLevel>().unwrap(), RiskLevel::High);
    }

    #[test]
    fn test_parse_extreme() {
        assert_eq!("extreme".parse::<RiskLevel>().unwrap(), RiskLevel::Extreme);
    }

    #[test]
    fn test_parse_invalid() {
        let result: Result<RiskLevel, String> = "critical".parse();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid RiskLevel"));
    }

    #[test]
    fn test_display() {
        assert_eq!(RiskLevel::Low.to_string(), "low");
        assert_eq!(RiskLevel::Medium.to_string(), "medium");
        assert_eq!(RiskLevel::High.to_string(), "high");
        assert_eq!(RiskLevel::Extreme.to_string(), "extreme");
    }

    #[test]
    fn test_ord() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Extreme);
        assert!(RiskLevel::Low < RiskLevel::Extreme);
    }

    #[test]
    fn test_serde_roundtrip() {
        for level in [
            RiskLevel::Low,
            RiskLevel::Medium,
            RiskLevel::High,
            RiskLevel::Extreme,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let parsed: RiskLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, parsed);
        }
    }
}
