//! 应用配置模块 — 从 TOML 文件加载并解析全局配置
//!
//! 配置文件结构见 `config/config.toml`（`AppConfig::load` 只读 TOML，不再支持
//! 环境变量覆盖业务配置项；启动配置路径由 `CORTEX_AGENT_CONFIG` / `--config` 指定）。
//!
//! 注：LLM 模型、知识库均不从配置文件读取，统一由 DB 管理
//! （见 `model_provider`、`domain/knowledge` 模块）；历史 `[dify]` 配置段已移除，
//! Dify 现作为运行时 KB provider 通过 GraphQL 配置。

use anyhow::Context;
use serde::Deserialize;


mod agent;
mod auth;
mod infra;
mod mcp;
mod storage;
mod workspace;

pub use agent::*;
pub use auth::*;
pub use infra::*;
pub use mcp::*;
pub use storage::*;
pub use workspace::*;

/// 应用全局配置根结构 — 对应 `config.toml` 的所有配置段
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub db: DbConfig,
    pub redis: RedisConfig,
    pub log: LogConfig,
    #[serde(default)]
    pub kb: KbConfig,
    #[serde(default)]
    pub context: ContextConfig,
    /// `[agents]` 段：子 agent 角色（内置 default/explorer/worker + 用户自定义 roles 表）
    /// 与默认子 agent 模型覆盖（对齐 codex `[agents]`）。
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub skill: SkillConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub assistant: AssistantConfig,
    /// 对象存储(S3/RustFS)配置
    #[serde(default)]
    pub object_storage: ObjectStorageConfig,
    /// 统一数据根目录，所有本地持久化数据的子目录都派生自此
    ///
    /// 派生目录（由 `AppConfig` 的 helper 方法计算，使用时自动创建）：
    /// - `{data_dir}/skills` — Skill 物化文件
    /// - `{data_dir}/workspaces/sessions/{session_id}` — 代码助手会话沙箱
    /// - `{data_dir}/artifacts` — ADK Artifact 持久化
    /// - `{data_dir}/screenshots` — 浏览器截图
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

fn default_data_dir() -> String {
    "./data".to_string()
}

/// 路径段安全校验:拒空 / `/` / `\` / `..` / `:`。
///
/// 用于 [`AppConfig::workspace_session_dir`] / [`AppConfig::screenshot_session_dir`] 派生目录时
/// 净化 `session_id` 等外部(客户端可控)输入,防 `Path::join` 路径穿越导致任意目录删除/写。
pub fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 256
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains("..")
        && !s.contains(':')
}

impl AppConfig {
    // ── 目录派生 helper ──
    // 所有子目录统一从 data_dir 派生，避免散落的硬编码路径

    /// Skill 物化根目录：`{data_dir}/skills`
    pub fn skill_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("skills")
    }

    /// 代码助手会话沙箱目录：`{data_dir}/workspaces/sessions/{session_id}`
    ///
    /// `session_id` 经 [`is_safe_path_segment`] 净化,异常(含 `/` `\` `..`)时落到专用
    /// 安全目录,绝不 join 原值——防 `Path::join` 路径穿越(任意目录删除/写)。
    pub fn workspace_session_dir(&self, session_id: &str) -> std::path::PathBuf {
        let seg = if is_safe_path_segment(session_id) {
            session_id
        } else {
            "__invalid_session__"
        };
        std::path::Path::new(&self.data_dir)
            .join("workspaces")
            .join("sessions")
            .join(seg)
    }

    /// Artifact 持久化目录：`{data_dir}/artifacts`
    pub fn artifact_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("artifacts")
    }

    /// 用户上传的图片附件目录：`{data_dir}/uploads`（与会话多模态输入配套）
    pub fn uploads_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("uploads")
    }

    /// 从 TOML 文件加载配置
    ///
    /// 加载流程：
    /// 1. 读取并解析 TOML 文件
    /// 2. 应用环境变量覆盖（LLM / Dify 相关配置）
    pub fn load(path: &str) -> Result<Self, anyhow::Error> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("读取配置文件 {} 失败", path))?;

        let cfg: AppConfig = toml::from_str(&content).with_context(|| "解析配置文件失败")?;

        Ok(cfg)
    }
}
