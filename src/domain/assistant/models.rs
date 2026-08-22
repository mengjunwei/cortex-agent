//! 助手领域模型与 DB 行映射。
//!
//! - [`Assistant`]：纯领域模型（强类型枚举），不 derive serde，序列化由传输层 DTO 负责
//!   （`docs/architecture.md` §2.1 / 计划 A2）。
//! - [`AssistantRow`]：diesel 反序列化行，`enabled_tools` 以 TEXT 存 JSON
//!   （架构 §8.2 禁 JSONB，见计划 A3）。
//! - [`AssistantPublicCard`]：广场/分享脱敏视图（排除 `system_prompt`）。

use std::collections::BTreeMap;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use serde::Serialize;

use super::enums::{AgentType, AssistantKind, Visibility};

/// 助手领域模型（业务层使用强类型枚举）。
#[derive(Debug, Clone)]
pub struct Assistant {
    pub id: String,
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub kind: AssistantKind,
    pub agent_type: AgentType,
    pub system_prompt: String,
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i32>,
    /// 思考级别：low/medium/high，None 表示不发（走模型默认）
    pub thinking_level: Option<String>,
    pub enabled_tools: Vec<String>,
    pub knowledge_enabled: bool,
    /// 绑定的知识库实例 id（多 provider；None=不绑定）
    pub kb_instance_id: Option<String>,
    /// 已启用的 MCP Server id 列表（JSON 数组存 TEXT，架构 §8.2）
    pub enabled_mcps: Vec<String>,
    /// 可用 Skill 白名单（JSON 数组存 TEXT，存 skill name）。空 = 不限制 = 全部可见；
    /// 非空 = 仅列出的 skill 对该助手可见（硬隔离：catalog/read_skill/$mention 三路过滤）。
    pub enabled_skills: Vec<String>,
    /// 助手级环境变量（JSON 对象 `{"KEY":"VALUE"}` 存 TEXT）。会话执行 shell/脚本时注入
    /// 子进程环境，供 skill 脚本等读取。可能含密钥——不进公开 DTO/导出/fork。
    pub env_vars: BTreeMap<String, String>,
    pub greeting: String,
    pub share_token: String,
    pub fork_count: i32,
    pub creator: String,
    pub visibility: Visibility,
    pub sort_order: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 广场/分享脱敏卡片：不暴露 `system_prompt`、`share_token`、`creator` 等内部字段。
#[derive(Debug, Clone, Serialize)]
pub struct AssistantPublicCard {
    pub id: String,
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub agent_type: AgentType,
    pub greeting: String,
    pub enabled_tools: Vec<String>,
    pub fork_count: i32,
}

impl From<&Assistant> for AssistantPublicCard {
    fn from(a: &Assistant) -> Self {
        Self {
            id: a.id.clone(),
            name: a.name.clone(),
            description: a.description.clone(),
            avatar: a.avatar.clone(),
            agent_type: a.agent_type,
            greeting: a.greeting.clone(),
            enabled_tools: a.enabled_tools.clone(),
            fork_count: a.fork_count,
        }
    }
}

/// 创建/更新自定义助手的领域层输入参数。
///
/// 不依赖传输层 DTO（架构 §2.1 / 计划 A2）：handler 把 Axum JSON DTO 转成本结构，
/// 再调 [`crate::domain::assistant::store::AssistantStore::create_custom`]。
///
/// `enabled_tools` 应由调用方先经
/// [`crate::tools::registry::sanitize_custom_tools`] 过滤。
#[derive(Debug, Clone)]
pub struct CustomAssistantInput {
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub system_prompt: String,
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i32>,
    /// 思考级别：low/medium/high，None 表示不发（走模型默认）
    pub thinking_level: Option<String>,
    pub enabled_tools: Vec<String>,
    pub knowledge_enabled: bool,
    /// 绑定的知识库实例 id（多 provider；None=不绑定）
    pub kb_instance_id: Option<String>,
    /// 已启用的 MCP Server id 列表（助手构建时注入对应 MCP 工具集）
    #[allow(dead_code)]
    pub enabled_mcps: Vec<String>,
    /// 可用 Skill 白名单（存 skill name）。空 = 全部可见；非空 = 硬隔离仅列出的可见。
    #[allow(dead_code)]
    pub enabled_skills: Vec<String>,
    /// 助手级环境变量（会话执行时注入子进程环境）。`None` = 更新时保持原值（脱敏编辑场景：
    /// 前端未解锁就不传，后端跳过该列）；`Some(map)` = 整体替换（已加密落库）。
    pub env_vars: Option<BTreeMap<String, String>>,
    pub greeting: String,
    pub visibility: Visibility,
}

/// DB 行（枚举以 i16 落库；`enabled_tools` 以 TEXT 存 JSON 字符串）。
#[derive(Debug, Clone, QueryableByName)]
pub struct AssistantRow {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub name: String,
    #[diesel(sql_type = sql_types::Text)]
    pub description: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub avatar: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub kind: i16,
    #[diesel(sql_type = sql_types::Int2)]
    pub agent_type: i16,
    #[diesel(sql_type = sql_types::Text)]
    pub system_prompt: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub model_id: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Float8>)]
    pub temperature: Option<f64>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Float8>)]
    pub top_p: Option<f64>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    pub max_tokens: Option<i32>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Text>)]
    pub thinking_level: Option<String>,
    #[diesel(sql_type = sql_types::Text)]
    pub enabled_tools: String,
    #[diesel(sql_type = sql_types::Bool)]
    pub knowledge_enabled: bool,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub kb_instance_id: Option<String>,
    #[diesel(sql_type = sql_types::Text)]
    pub enabled_mcps: String,
    #[diesel(sql_type = sql_types::Text)]
    pub enabled_skills: String,
    #[diesel(sql_type = sql_types::Text)]
    pub env_vars: String,
    #[diesel(sql_type = sql_types::Text)]
    pub greeting: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub share_token: String,
    #[diesel(sql_type = sql_types::Int4)]
    pub fork_count: i32,
    #[diesel(sql_type = sql_types::Varchar)]
    pub creator: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub visibility: i16,
    #[diesel(sql_type = sql_types::Int4)]
    pub sort_order: i32,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<AssistantRow> for Assistant {
    fn from(r: AssistantRow) -> Self {
        let enabled_tools: Vec<String> = serde_json::from_str(&r.enabled_tools).unwrap_or_default();
        let enabled_mcps: Vec<String> = serde_json::from_str(&r.enabled_mcps).unwrap_or_default();
        let enabled_skills: Vec<String> =
            serde_json::from_str(&r.enabled_skills).unwrap_or_default();
        // env_vars 列存的是 AES 密文（非 JSON），这里**故意不解析**——解析必然失败产生误导。
        // 生产读路径走 `AssistantStore::row_to_assistant`（用 codec 解密为明文）。此 From 仅作
        // 结构映射（测试夹具等），env_vars 留空，由 store 按需填充。fork 路径也依赖此「不解密」
        // 保证密钥不会进入 fork 副本（见 store::fork 显式置空）。
        Assistant {
            id: r.id,
            name: r.name,
            description: r.description,
            avatar: r.avatar,
            kind: AssistantKind::from_i16(r.kind),
            agent_type: AgentType::from_i16(r.agent_type),
            system_prompt: r.system_prompt,
            model_id: r.model_id,
            temperature: r.temperature,
            top_p: r.top_p,
            max_tokens: r.max_tokens,
            thinking_level: r.thinking_level,
            enabled_tools,
            knowledge_enabled: r.knowledge_enabled,
            kb_instance_id: r.kb_instance_id,
            enabled_mcps,
            enabled_skills,
            env_vars: BTreeMap::new(),
            greeting: r.greeting,
            share_token: r.share_token,
            fork_count: r.fork_count,
            creator: r.creator,
            visibility: Visibility::from_i16(r.visibility),
            sort_order: r.sort_order,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> AssistantRow {
        AssistantRow {
            id: "01937000-0000-7000-8000-000000000001".into(),
            name: "Auto".into(),
            description: "d".into(),
            avatar: "🤖".into(),
            kind: 0,
            agent_type: 0,
            system_prompt: "secret".into(),
            model_id: "m".into(),
            temperature: Some(0.5),
            top_p: None,
            max_tokens: None,
            thinking_level: None,
            enabled_tools: r#"["search_kb","shell_command"]"#.into(),
            knowledge_enabled: true,
            kb_instance_id: None,
            enabled_mcps: r#"["01Habc","01Hdef"]"#.into(),
            enabled_skills: r#"["skill-a"]"#.into(),
            env_vars: r#"{"FOO":"bar"}"#.into(),
            greeting: "hi".into(),
            share_token: "TOK12345".into(),
            fork_count: 3,
            creator: "019feab3-20d2-7993-8886-d05f225e4e54".into(),
            visibility: 1,
            sort_order: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn row_to_assistant_parses_enabled_tools_text() {
        let a: Assistant = sample_row().into();
        assert_eq!(a.enabled_tools, vec!["search_kb", "shell_command"]);
        assert_eq!(a.kind, AssistantKind::Builtin);
        assert_eq!(a.visibility, Visibility::Shared);
        assert_eq!(a.fork_count, 3);
    }

    #[test]
    fn row_to_assistant_does_not_parse_env_vars() {
        // env_vars 列存密文，From 故意不解析 → 恒为空（明文由 AssistantStore::row_to_assistant 解密填充）
        let a: Assistant = sample_row().into();
        assert!(a.env_vars.is_empty(), "From 不应解析 env_vars（列存密文）");
        // 无论列里是什么（明文 JSON / 密文 / 乱码），From 都不碰 → 恒空
        let mut r = sample_row();
        r.env_vars = r#"{"FOO":"bar"}"#.into();
        assert!(Into::<Assistant>::into(r).env_vars.is_empty());
    }

    #[test]
    fn row_to_assistant_handles_invalid_tools_json() {
        let mut r = sample_row();
        r.enabled_tools = "not json".into();
        let a: Assistant = r.into();
        assert!(a.enabled_tools.is_empty());
    }

    #[test]
    fn public_card_excludes_system_prompt_and_token() {
        let a: Assistant = sample_row().into();
        let card = AssistantPublicCard::from(&a);
        let json = serde_json::to_string(&card).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("TOK12345"));
        assert!(json.contains("Auto"));
        assert!(json.contains("\"fork_count\":3"));
    }
}
