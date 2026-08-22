//! Codex-style skill system — file-system discovery + progressive disclosure injection.
//!
//! See `docs/superpowers/specs/2026-07-28-codex-style-skills-design.md` for full design.
//!
//! Submodules (added incrementally in later tasks):
//! - [`loader`]: filesystem BFS discovery + frontmatter parse
//! - [`render`]: `SkillService` — catalog rendering + skill text lookup
//! - [`mention`]: `$skill-name` parser
//! - [`inject`]: XML body-block renderer

pub mod inject;
pub mod loader;
pub mod mention;
pub mod render;

pub use loader::ParsedFrontmatter;
pub use render::SkillService;

use std::collections::HashMap;
use std::path::PathBuf;

/// Skill 来源层级。User 覆盖同名的 Builtin。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// 编译期嵌入,启动时解压到 `{skill_dir}/.builtin/`
    Builtin,
    /// 用户在 `{skill_dir}/skills/` 下手动放置
    User,
}

/// Skill 运行时元数据(从 SKILL.md frontmatter 解析)。
#[derive(Debug, Clone)]
pub struct SkillMetadata {
    /// 目录名 / frontmatter name;格式 `^[a-z0-9-]+$`,1-64 字符
    pub name: String,
    /// frontmatter description(必填);用于目录渲染 + 模型相关性判断
    pub description: String,
    /// frontmatter metadata.short-description(可选)
    pub short_description: Option<String>,
    /// SKILL.md 绝对路径
    pub path: PathBuf,
    /// 来源层级
    pub scope: SkillScope,
}

/// 全量 skill 索引(启动时构建,只读)。
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    /// 去重后的有效 skill 列表(同名时 User 覆盖 Builtin),按 scope + name 排序
    pub skills: Vec<SkillMetadata>,
    /// name → skills 索引(快速查找)
    pub by_name: HashMap<String, usize>,
}

impl SkillCatalog {
    /// 按 name 查找元数据。
    pub fn find_by_name(&self, name: &str) -> Option<&SkillMetadata> {
        self.by_name.get(name).map(|&i| &self.skills[i])
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// 校验 skill name:仅 `[a-z0-9-]`,1-64 字符,非空,禁首尾/连续连字符。
///
/// 对齐 codex `quick_validate.py`:连字符规则与 mention 正则
/// `(?:[a-z0-9]+-)*[a-z0-9]+` 保持一致 —— 否则 name 能加载进目录,
/// 但 mention 正则匹配不到,skill 正文永远触发不了(看得见点不到)。
pub fn is_valid_skill_name(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 {
        return false;
    }
    // 字符类:[a-z0-9-]
    let chars_ok = n
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    // 禁首/尾连字符、禁连续连字符(对齐 codex,与 mention 正则匹配规则一致)
    chars_ok && !n.starts_with('-') && !n.ends_with('-') && !n.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert!(is_valid_skill_name("skill-creator"));
        assert!(is_valid_skill_name("a1"));
        assert!(is_valid_skill_name("abc-def-123"));
        assert!(is_valid_skill_name(&"a".repeat(64)));
    }

    #[test]
    fn invalid_names() {
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("   "));
        assert!(!is_valid_skill_name("Skill_Creator")); // uppercase + underscore
        assert!(!is_valid_skill_name("with space"));
        assert!(!is_valid_skill_name("../etc"));
        assert!(!is_valid_skill_name(&"a".repeat(65)));
        // 对齐 codex:禁首尾/连续连字符(否则 mention 正则匹配不到)
        assert!(!is_valid_skill_name("-foo")); // 首连字符
        assert!(!is_valid_skill_name("foo-")); // 尾连字符
        assert!(!is_valid_skill_name("foo--bar")); // 连续连字符
    }

    #[test]
    fn catalog_find_by_name_hits() {
        let mut cat = SkillCatalog::default();
        cat.skills.push(SkillMetadata {
            name: "foo".into(),
            description: "d".into(),
            short_description: None,
            path: PathBuf::from("/x/SKILL.md"),
            scope: SkillScope::Builtin,
        });
        cat.by_name.insert("foo".into(), 0);
        assert!(cat.find_by_name("foo").is_some());
        assert!(cat.find_by_name("missing").is_none());
    }

    #[test]
    fn catalog_empty_default() {
        assert!(SkillCatalog::default().is_empty());
    }
}
