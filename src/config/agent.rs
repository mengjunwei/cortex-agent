//! Agent 行为配置段 — `[kb]` / `[context]` / `[agents]` / `[skill]`

use serde::Deserialize;
use std::collections::BTreeMap;

/// 知识库配置（`[kb]` 段）— 多 provider 知识库的全局设置。
///
/// Qdrant 连接（内置 provider 的向量后端）+ 内置实例默认切片/检索参数
/// （前端新建内置实例时作表单默认值）。
#[derive(Debug, Clone, Deserialize)]
pub struct KbConfig {
    /// Qdrant gRPC 地址
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    /// Qdrant API Key（无鉴权留空）
    #[serde(default)]
    pub qdrant_api_key: String,
    #[serde(default = "default_kb_chunk_size")]
    pub default_chunk_size: usize,
    #[serde(default = "default_kb_chunk_overlap")]
    pub default_chunk_overlap: usize,
    #[serde(default = "default_kb_top_k")]
    pub default_top_k: usize,
    #[serde(default = "default_kb_similarity")]
    pub default_similarity_threshold: f64,
}

impl Default for KbConfig {
    fn default() -> Self {
        Self {
            qdrant_url: default_qdrant_url(),
            qdrant_api_key: String::new(),
            default_chunk_size: default_kb_chunk_size(),
            default_chunk_overlap: default_kb_chunk_overlap(),
            default_top_k: default_kb_top_k(),
            default_similarity_threshold: default_kb_similarity(),
        }
    }
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".to_string()
}
fn default_kb_chunk_size() -> usize {
    1024
}
fn default_kb_chunk_overlap() -> usize {
    100
}
fn default_kb_top_k() -> usize {
    6
}
fn default_kb_similarity() -> f64 {
    0.35
}

/// Context 治理配置（`[context]` 段）
///
/// 上下文压缩对齐 codex：**仅按模型 context_window 触发**（软闸 ×0.95 / 硬闸 ×0.95 / 提醒 ×0.15，
/// 比例固化为常量见 `cortex_agent`），不再按轮数 / 单轮 token 阈值压缩。
/// 本段只保留与窗口压缩无关或为其兜底的通用项。
#[derive(Debug, Clone, Deserialize)]
pub struct ContextConfig {
    /// token 估算：字符/token 比率，默认 4（中英文混合合理值）
    #[serde(default = "default_chars_per_token")]
    pub chars_per_token: u32,
    /// 工具输出截断阈值（字节），默认 48KB
    #[serde(default = "default_tool_max_output_bytes")]
    pub tool_max_output_bytes: usize,
    /// 模型未配 context_window 时的回退窗口（token），默认 128000
    #[serde(default = "default_fallback_context_window")]
    pub fallback_context_window: usize,
    /// 压缩专用便宜模型 id（None=用主模型），默认 None
    #[serde(default)]
    pub compact_model_id: Option<String>,
    /// spawn_agent 最大嵌套深度（顶层=0，每 spawn 子 agent +1），默认 3。
    /// 注：这是 codex V1 的概念（V2 靠容量限制不查深度）；cortex 借它做进程内
    /// 防失控递归护栏（无 residency/rollout 容量兜底），语义自洽。
    #[serde(default = "default_max_spawn_depth")]
    pub max_spawn_depth: u32,
    /// 同时运行的最大子 agent 数（对齐 codex effective_agent_max_threads 语义——
    /// max_concurrent_threads_per_session - 1，root 不占槽），0=不限，默认 3
    #[serde(default = "default_max_concurrent_children")]
    pub max_concurrent_children: usize,
    /// 多智能体委派模式（对齐 codex MultiAgentMode）：explicit=仅用户明确要求才 spawn，
    /// proactive=主动委派，auto=按思考级别推导（max→proactive，否则 explicit）。默认 explicit。
    #[serde(default = "default_multi_agent_mode")]
    pub multi_agent_mode: MultiAgentModeConfig,
}

fn default_chars_per_token() -> u32 {
    4
}
fn default_tool_max_output_bytes() -> usize {
    48 * 1024
}
fn default_fallback_context_window() -> usize {
    128_000
}
fn default_max_spawn_depth() -> u32 {
    3
}
fn default_max_concurrent_children() -> usize {
    // 3：兼容弱模型（如 mimo-v2.5-pro）的稳定并行上限——实测 6 个长任务会让主 agent 复读退化。
    // 强模型（如 glm-5.2）靠并发排队分批仍可完成更多子任务。
    3
}

/// 多智能体委派模式配置（`multi_agent_mode`，字符串枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MultiAgentModeConfig {
    /// 仅用户明确要求才 spawn（对齐 codex ExplicitRequestOnly，默认）
    #[default]
    Explicit,
    /// 主动委派：并行能显著提速/提质时主动 spawn（对齐 codex Proactive）
    Proactive,
    /// 按思考级别推导：max → Proactive，否则 Explicit（对齐 codex 按 reasoning effort 推导）
    Auto,
}

fn default_multi_agent_mode() -> MultiAgentModeConfig {
    MultiAgentModeConfig::default()
}

/// 用户自定义角色配置（`[agents.roles.<name>]`，对齐 codex `[agents.<role>]` 表）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentRoleToml {
    pub description: String,
    #[serde(default)]
    pub instruction: Option<String>,
    #[serde(default)]
    pub nickname_candidates: Option<Vec<String>>,
}

/// `[agents]` 段（子 agent 角色与默认覆盖，对齐 codex）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentsConfig {
    /// 用户自定义角色表：`[agents.roles.researcher]` → description/instruction/nickname_candidates
    #[serde(default)]
    pub roles: BTreeMap<String, AgentRoleToml>,
    /// spawn 未指定 model 时的默认子 agent 模型 id（None=继承父模型）
    #[serde(default)]
    pub default_subagent_model: Option<String>,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            chars_per_token: default_chars_per_token(),
            tool_max_output_bytes: default_tool_max_output_bytes(),
            fallback_context_window: default_fallback_context_window(),
            compact_model_id: None,
            max_spawn_depth: default_max_spawn_depth(),
            max_concurrent_children: default_max_concurrent_children(),
            multi_agent_mode: default_multi_agent_mode(),
        }
    }
}

/// Skill（Agent 技能）配置（`[skill]` 段）
///
/// Skill 能力随数据库启用而始终启用（无独立开关）。详见 docs/design/skill-management.md §8。
///
/// 物化根目录由 `AppConfig.data_dir` 统一管理（`{data_dir}/skills`），此处不再保留 root_dir。
#[derive(Debug, Clone, Deserialize)]
pub struct SkillConfig {
    /// 单个 skill body 注入到对话的最大字符数
    #[serde(default = "default_skill_max_inject_chars")]
    pub max_inject_chars: usize,
    /// skill 目录占上下文窗口的百分比(默认 2)
    #[serde(default = "default_catalog_token_budget_pct")]
    pub catalog_token_budget_pct: u8,
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            max_inject_chars: default_skill_max_inject_chars(),
            catalog_token_budget_pct: default_catalog_token_budget_pct(),
        }
    }
}

fn default_skill_max_inject_chars() -> usize {
    // 对齐 codex MAX_SKILL_PROMPT_BYTES = 8_000(ext/skills/src/render.rs)
    8_000
}
fn default_catalog_token_budget_pct() -> u8 {
    2
}
