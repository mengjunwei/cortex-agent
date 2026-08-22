//! Skill 正文注入块渲染器。
//!
//! 输出格式(对齐 codex `SkillInstructions` fragment,ext/skills/src/fragments.rs):
//! ```text
//! <skill>
//! <name>skill-creator</name>
//! <path>/abs/skill-dir</path>
//!
//! (正文,去掉 frontmatter,截断到 max_chars)
//! </skill>
//! ```

use crate::domain::skill::loader::strip_frontmatter;

/// 渲染 skill 正文为 XML 包裹块。
///
/// - `raw_text`:SKILL.md 全文(含 frontmatter);函数内部调用 `strip_frontmatter`
/// - `max_chars`:正文最大字符数;超出则截断并追加截断标记
pub fn render_skill_body_block(name: &str, raw_text: &str, max_chars: usize) -> String {
    render_skill_body_block_with_path(name, None, None, raw_text, max_chars)
}

/// 渲染 skill 正文为 XML 包裹块(带 skill 目录路径)。
///
/// - `skill_dir`:单个 skill 的目录(用于把正文 `scripts/`/`references/`/`assets/` 相对路径
///   替换为绝对路径,并在 `<skill>` 内插入 `<path>` 标签)。
/// - `skill_root`:**全局 skill 根目录**(用于把正文 `{data_dir}` 占位符替换为真实路径)。
///   skill-creator 等 skill 用 `{data_dir}/skills` 指引模型创建新 skill 的位置 —— 不替换则
///   模型拿到字面 `{data_dir}`、`init_skill.py --path {data_dir}/skills` 会在错误位置建目录。
///   两者语义不同,勿混。
pub fn render_skill_body_block_with_path(
    name: &str,
    skill_dir: Option<&std::path::Path>,
    skill_root: Option<&std::path::Path>,
    raw_text: &str,
    max_chars: usize,
) -> String {
    let mut body = strip_frontmatter(raw_text);

    // {data_dir} 占位符 → 全局 skill 根绝对路径(归一化为正斜杠,Windows 兼容)
    if let Some(root) = skill_root {
        let root_normalized = root.to_string_lossy().replace('\\', "/");
        if !root_normalized.is_empty() {
            body = body.replace("{data_dir}", &root_normalized);
        }
    }

    // 如果有 skill 目录，把正文中的相对路径替换为绝对路径
    // 这样 LLM 不需要理解 <path> 标签，直接在命令里用绝对路径
    if let Some(dir) = skill_dir {
        let dir_str = dir.to_string_lossy();
        let dir_normalized = dir_str.replace('\\', "/");
        // 只替换不在绝对路径中的相对引用（避免二次替换）
        if !body.contains(&dir_normalized) {
            body = body
                .replace("scripts/", &format!("{dir_normalized}/scripts/"))
                .replace("references/", &format!("{dir_normalized}/references/"))
                .replace("assets/", &format!("{dir_normalized}/assets/"));
        }
    }

    let mut out = String::with_capacity(128 + body.len().min(max_chars + 64));
    // 对齐 codex SkillInstructions:<skill> 无属性开标签 + <name> + <path> + 正文
    out.push_str("<skill>\n<name>");
    out.push_str(name);
    out.push_str("</name>\n");
    if let Some(dir) = skill_dir {
        out.push_str("<path>");
        out.push_str(&dir.to_string_lossy());
        out.push_str("</path>\n");
    }
    out.push('\n');

    let body_trimmed = body.trim();
    if body_trimmed.chars().count() <= max_chars {
        out.push_str(body_trimmed);
    } else {
        let truncated: String = body_trimmed.chars().take(max_chars).collect();
        let original_len = body_trimmed.chars().count();
        out.push_str(&truncated);
        out.push_str(&format!(
            "\n\n...[truncated: original {original_len} chars]"
        ));
    }
    out.push_str("\n</skill>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: foo\ndescription: A foo skill\n---\n\n# Foo\n\nDo the thing.";

    #[test]
    fn renders_xml_block() {
        let block = render_skill_body_block("foo", SAMPLE, 1000);
        // 对齐 codex 格式:<skill> 无属性 + <name> 标签,无 <description>
        assert!(block.starts_with("<skill>\n<name>foo</name>\n"));
        assert!(!block.contains("<description>"));
        assert!(block.contains("# Foo"));
        assert!(block.ends_with("</skill>"));
    }

    #[test]
    fn truncates_when_over_max() {
        let long_body = "x".repeat(100);
        let content = format!("---\nname: foo\ndescription: d\n---\n\n{long_body}");
        let block = render_skill_body_block("foo", &content, 10);
        assert!(block.contains("truncated"));
    }

    #[test]
    fn no_truncation_when_under_max() {
        let block = render_skill_body_block("foo", SAMPLE, 1000);
        assert!(!block.contains("truncated"));
    }

    #[test]
    fn handles_missing_description() {
        let content = "no frontmatter here";
        let block = render_skill_body_block("foo", content, 1000);
        assert!(!block.contains("<description>"));
        assert!(block.contains("no frontmatter here"));
    }

    #[test]
    fn replaces_data_dir_placeholder() {
        let content = "---\nname: foo\ndescription: d\n---\n\n创建 skill 到 {data_dir}/skills 下";
        let block = render_skill_body_block_with_path(
            "foo",
            None,
            Some(std::path::Path::new("/var/data/skills")),
            content,
            1000,
        );
        // {data_dir} → skill 根,故 {data_dir}/skills → /var/data/skills/skills
        assert!(
            block.contains("/var/data/skills/skills"),
            "占位符应被替换为 skill 根: {block}"
        );
        assert!(!block.contains("{data_dir}"), "不应残留字面占位符: {block}");
    }
}
