//! Filesystem BFS discovery + SKILL.md frontmatter parsing.
//!
//! 扫描规则(对齐 Codex `discover_skills_under_root`,简化):
//! - 从 skill_dir 起 BFS,最大深度 3 层
//! - 跳过点号开头目录(除了 `.builtin` — 显式白名单)
//! - 跳过非 `SKILL.md` 文件名(大小写敏感)
//! - 同名 skill:User scope 覆盖 Builtin scope

use std::borrow::Cow;
use std::path::Path;

use crate::error::AppError;
use crate::domain::skill::{SkillMetadata, SkillScope, is_valid_skill_name};

/// 从 SKILL.md 解析出的 frontmatter。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFrontmatter {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
}

const SKILLS_FILENAME: &str = "SKILL.md";
const BUILTIN_DIR_NAME: &str = ".builtin";
const MAX_SCAN_DEPTH: usize = 3;

/// YAML frontmatter serde 模型。
#[derive(Debug, serde::Deserialize)]
struct FrontmatterYaml {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    metadata: FrontmatterMetadataYaml,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FrontmatterMetadataYaml {
    #[serde(rename = "short-description")]
    short_description: Option<String>,
}

/// 从 SKILL.md 文本解析 frontmatter。
///
/// 格式:首行为 `---`,某个后续行为 `---`,中间为 YAML。
/// 返回 None 表示无 frontmatter 或格式错误(调用方应跳过并记 warn)。
///
/// 容忍 CRLF / CR 行尾:内部归一化为 LF 后再匹配。
/// 容忍未加引号的含冒号标量(移植 codex `repair_frontmatter_scalar_fields`):
/// 如 `description: Build for AWS: ECS` 这种散文会破坏 YAML 解析,这里按行修复后重试,
/// 避免整份第三方 skill 被静默丢弃。
pub fn parse_frontmatter(content: &str) -> Option<ParsedFrontmatter> {
    let normalized = normalize_line_endings(content);
    let content = normalized.strip_prefix('\u{feff}').unwrap_or(&normalized);
    let after_open = content.strip_prefix("---\n")?;
    let end = after_open.find("\n---\n")?;
    let yaml_str = &after_open[..end];
    let parsed: FrontmatterYaml = match serde_yaml::from_str(yaml_str) {
        Ok(p) => p,
        Err(original_err) => {
            // 仅当修复产出了不同文本才重试；修复后仍失败则放弃（保持"跳过无效 skill"语义）
            let repaired = repair_frontmatter_scalar_fields(yaml_str)?;
            match serde_yaml::from_str(&repaired) {
                Ok(p) => p,
                Err(_) => {
                    tracing::debug!("[skill] frontmatter 修复后仍解析失败: {original_err}");
                    return None;
                }
            }
        }
    };
    let description = parsed
        .description
        .as_deref()
        .map(sanitize_single_line)
        .filter(|d| !d.is_empty())?;
    let name = parsed
        .name
        .map(|n| sanitize_single_line(&n))
        .unwrap_or_default();
    Some(ParsedFrontmatter {
        name,
        description,
        short_description: parsed
            .metadata
            .short_description
            .map(|s| sanitize_single_line(&s)),
    })
}

/// 返回 frontmatter 之后的正文(不含 frontmatter)。无 frontmatter 时返回原文。
///
/// 容忍 CRLF / CR 行尾:正文中的 CRLF 会被归一化为 LF,便于下游统一处理。
/// 注意:返回类型为 `String`(不再是 `&str`),因为归一化可能需要分配新内存。
pub fn strip_frontmatter(content: &str) -> String {
    let normalized = normalize_line_endings(content);
    let content = normalized.strip_prefix('\u{feff}').unwrap_or(&normalized);
    let Some(after_open) = content.strip_prefix("---\n") else {
        return normalized.into_owned();
    };
    let Some(end) = after_open.find("\n---\n") else {
        return normalized.into_owned();
    };
    let body = &after_open[end + "\n---\n".len()..];
    // 消费 frontmatter 关闭后的约定空白行(`---\n\n<body>` → `<body>`)。
    body.strip_prefix('\n').unwrap_or(body).to_string()
}

/// 将 CRLF / CR 归一化为 LF。无 `\r` 时零拷贝返回借用。
fn normalize_line_endings(content: &str) -> Cow<'_, str> {
    if !content.contains('\r') {
        return Cow::Borrowed(content);
    }
    // 先替换 \r\n,再处理孤立的 \r(老 Mac 行尾),避免重复替换
    let s = content.replace("\r\n", "\n").replace('\r', "\n");
    Cow::Owned(s)
}

/// 折叠单行标量内部的多余空白（移植 codex `sanitize_single_line`）。
/// 把任意空白序列归一为单个空格并 trim，避免 description 里混入双空格/制表符。
fn sanitize_single_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 修复 frontmatter 中未加引号、含冒号或类 flow 标量的行（移植 codex
/// `repair_frontmatter_scalar_fields`）。
///
/// 典型场景:第三方 skill 写 `description: Build for AWS: ECS` 或
/// `argument-hint: <duration: e.g. 7d>`，裸标量里出现 `: `（或以 `[`/`{`/`@`/`` ` `` 开头）
/// 会让 serde_yaml 解析整块失败。这里按行处理:仅对 `key: <空格>value` 形式、且 value 是
/// 需要转义的裸标量，用单引号包裹（单引号内的 `'` 转义为 `''`）。块标量（`|`/`>`）、
/// 已引用标量、注释、无冒号行一律原样保留，故无关的非法 YAML 仍会原样暴露失败。
///
/// 返回 `Some(修复后文本)` 表示有改动；`None` 表示无需/无法修复。
fn repair_frontmatter_scalar_fields(frontmatter: &str) -> Option<String> {
    let mut changed = false;
    let mut block_scalar_indent: Option<usize> = None;
    let mut repaired_lines: Vec<String> = Vec::new();
    for line in frontmatter.lines() {
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        if let Some(block_indent) = block_scalar_indent {
            if line.trim().is_empty() || indent > block_indent {
                repaired_lines.push(line.to_string());
                continue;
            }
            block_scalar_indent = None;
        }

        let Some((key, value)) = line.split_once(':') else {
            repaired_lines.push(line.to_string());
            continue;
        };
        // 仅处理 `key: value`（value 以空白开头）的形式
        let next_is_whitespace = value.chars().next().is_some_and(char::is_whitespace);
        if key.trim().is_empty() || !next_is_whitespace {
            repaired_lines.push(line.to_string());
            continue;
        }

        let trimmed_start = value.trim_start();
        let leading_whitespace = &value[..value.len() - trimmed_start.len()];
        let mut scalar = trimmed_start;
        let mut comment = "";
        for (index, character) in trimmed_start.char_indices() {
            if character == '#'
                && (index == 0
                    || trimmed_start[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace))
            {
                let comment_start = trimmed_start[..index].trim_end().len();
                scalar = &trimmed_start[..comment_start];
                comment = &trimmed_start[comment_start..];
                break;
            }
        }

        let scalar = scalar.trim_end();
        let Some(first_char) = scalar.chars().next() else {
            repaired_lines.push(line.to_string());
            continue;
        };
        // 块标量保持原样，并记录缩进以跳过其后续行
        if matches!(first_char, '|' | '>') {
            block_scalar_indent = Some(indent);
            repaired_lines.push(line.to_string());
            continue;
        }
        // 已用引号包裹的保持原样
        if matches!(first_char, '\'' | '"') {
            repaired_lines.push(line.to_string());
            continue;
        }
        // 检测裸标量内是否含 `: `（冒号 + 空白）——这是破坏 YAML 的根因
        let mut has_colon_separator = false;
        let mut chars = scalar.chars().peekable();
        while let Some(character) = chars.next() {
            if character == ':'
                && matches!(chars.peek(), Some(next_character) if next_character.is_whitespace())
            {
                has_colon_separator = true;
                break;
            }
        }
        // 以 flow 起始符开头、且自身不是合法 flow 标量的也要转义
        let invalid_flow_like_scalar = matches!(first_char, '[' | '{' | '@' | '`')
            && serde_yaml::from_str::<serde_yaml::Value>(scalar).is_err();
        if !has_colon_separator && !invalid_flow_like_scalar {
            repaired_lines.push(line.to_string());
            continue;
        }

        let quoted_scalar = format!("'{}'", scalar.replace('\'', "''"));
        repaired_lines.push(format!(
            "{key}:{leading_whitespace}{quoted_scalar}{comment}"
        ));
        changed = true;
    }
    changed.then(|| repaired_lines.join("\n"))
}

/// 读取 SKILL.md 文本:UTF-8 优先,失败回退 GB18030(兼容 GBK/CP936)。
///
/// 返回 `(text, fallback_used)`——`fallback_used` 为 true 表示源文件非 UTF-8,
/// 已用 GB18030 解码(调用方可据此打日志标注来源)。GB18030 是 GBK 的超集,能正确
/// 处理国内 Windows 记事本默认保存的中文编码;CRLF/CR 行尾由 `parse_frontmatter`/
/// `strip_frontmatter` 的归一化兜底,无需在此处理。
pub fn read_skill_file_text(path: &Path) -> std::io::Result<(String, bool)> {
    let bytes = std::fs::read(path)?;
    match String::from_utf8(bytes) {
        Ok(s) => Ok((s, false)),
        Err(e) => {
            // 用 let 绑定延长 into_bytes() 的临时值生命周期：decode 返回的 Cow 借用该 Vec，
            // 若写成内联临时值会在本语句结束释放、下一行 cow.into_owned() 即悬空（E0716）。
            let bytes = e.into_bytes();
            let (cow, _enc, _had_errors) = encoding_rs::GB18030.decode(&bytes);
            Ok((cow.into_owned(), true))
        }
    }
}

/// 从 skill 根目录 BFS 发现所有 SKILL.md,解析为 `SkillMetadata` 列表。
///
/// - Builtin scope:`root/.builtin/<name>/SKILL.md`
/// - User scope:`root/<name>/SKILL.md`(顶层直接子目录)
///
/// 解析失败或 name 非法的 skill 记 warn 并跳过,不阻塞其他 skill。
/// 同名 skill:后处理的覆盖先处理的(catalog 层做去重,这里返回全部)。
pub fn discover_skills(root: &Path) -> Result<Vec<SkillMetadata>, AppError> {
    let mut results = Vec::new();
    if !root.exists() {
        return Ok(results);
    }
    let entries = std::fs::read_dir(root).map_err(|e| {
        AppError::FileError(format!("读取 skill 根目录失败 {}: {e}", root.display()))
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        let is_builtin = name_str == BUILTIN_DIR_NAME;
        let scope = if is_builtin {
            SkillScope::Builtin
        } else {
            // 跳过点号开头的非 builtin 目录
            if name_str.starts_with('.') {
                continue;
            }
            SkillScope::User
        };
        scan_skill_dir(&path, scope, MAX_SCAN_DEPTH, &mut results);
    }
    Ok(results)
}

fn scan_skill_dir(dir: &Path, scope: SkillScope, depth_left: usize, out: &mut Vec<SkillMetadata>) {
    if depth_left == 0 {
        return;
    }
    // 当前目录有 SKILL.md?
    let skill_file = dir.join(SKILLS_FILENAME);
    if skill_file.is_file() {
        match load_skill_file(dir, &skill_file, scope) {
            Ok(meta) => out.push(meta),
            Err(e) => tracing::warn!("[skill] 跳过无效 SKILL.md {}: {}", skill_file.display(), e),
        }
        // SKILL.md 所在目录即一个 skill,不再深入子目录(避免误扫 references/scripts)
        return;
    }
    // 无 SKILL.md,继续 BFS 子目录
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            scan_skill_dir(&p, scope, depth_left - 1, out);
        }
    }
}

fn load_skill_file(
    dir: &Path,
    skill_file: &Path,
    scope: SkillScope,
) -> Result<SkillMetadata, String> {
    let (content, fallback) =
        read_skill_file_text(skill_file).map_err(|e| format!("读取失败: {e}"))?;
    if fallback {
        tracing::info!(
            "[skill] {} 非 UTF-8,已回退 GB18030 解码",
            skill_file.display()
        );
    }
    let parsed = parse_frontmatter(&content).ok_or_else(|| {
        "frontmatter 缺失或格式无效(需 ---\\n...\\n---\\n + description)".to_string()
    })?;
    // name 缺失时 fallback 到目录名(Codex 行为)
    let name = if parsed.name.trim().is_empty() {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    } else {
        parsed.name.trim().to_string()
    };
    if !is_valid_skill_name(&name) {
        return Err(format!(
            "name '{name}' 非法(需 ^[a-z0-9-]+$,1-64 字符,禁首尾/连续连字符)"
        ));
    }
    // description 长度:对齐 codex——loader 不拒绝超长 desc(parser 只校验 name),
    // 渲染层截断到 1024(1021 字符 + "...",render.rs `truncate_catalog_skill_description`)。
    Ok(SkillMetadata {
        name,
        description: parsed.description,
        short_description: parsed.short_description,
        path: skill_file.to_path_buf(),
        scope,
    })
}

/// 渲染层 desc 截断(对齐 codex `truncate_catalog_skill_description`):
/// 超 1024 字符截为 1021 字符 + "..."。catalog 渲染前调用。
pub fn truncate_catalog_skill_description(description: &str) -> String {
    const MAX_CATALOG_SKILL_DESCRIPTION_CHARS: usize = 1_024;
    const TRUNCATED_SKILL_DESCRIPTION_SUFFIX: &str = "...";
    if description
        .char_indices()
        .nth(MAX_CATALOG_SKILL_DESCRIPTION_CHARS)
        .is_none()
    {
        return description.to_string();
    }
    let prefix_chars = MAX_CATALOG_SKILL_DESCRIPTION_CHARS
        .saturating_sub(TRUNCATED_SKILL_DESCRIPTION_SUFFIX.chars().count());
    let prefix_end = description
        .char_indices()
        .nth(prefix_chars)
        .map_or(description.len(), |(index, _)| index);
    let mut truncated = description[..prefix_end].to_string();
    truncated.push_str(TRUNCATED_SKILL_DESCRIPTION_SUFFIX);
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("cortex-skill-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_skill(root: &Path, scope_path: &str, name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(scope_path).join(name);
        fs::create_dir_all(&dir).unwrap();
        let content = format!("---\n{frontmatter}\n---\n\n{body}");
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nname: foo\ndescription: A skill\n---\n\nbody";
        let p = parse_frontmatter(content).unwrap();
        assert_eq!(p.name, "foo");
        assert_eq!(p.description, "A skill");
        assert_eq!(p.short_description, None);
    }

    #[test]
    fn parse_frontmatter_with_short_desc() {
        let content = "---\nname: foo\ndescription: A skill\nmetadata:\n  short-description: short\n---\n\nbody";
        let p = parse_frontmatter(content).unwrap();
        assert_eq!(p.short_description.as_deref(), Some("short"));
    }

    #[test]
    fn parse_frontmatter_missing_description_returns_none() {
        let content = "---\nname: foo\n---\n\nbody";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn parse_frontmatter_no_frontmatter_returns_none() {
        assert!(parse_frontmatter("just body").is_none());
    }

    #[test]
    fn strip_frontmatter_returns_body() {
        let content = "---\nname: foo\ndescription: d\n---\n\n# Title\n\ntext";
        assert_eq!(strip_frontmatter(content), "# Title\n\ntext");
    }

    #[test]
    fn strip_frontmatter_no_frontmatter_returns_original() {
        assert_eq!(strip_frontmatter("just text"), "just text");
    }

    #[test]
    fn parse_frontmatter_tolerates_crlf() {
        let content = "---\r\nname: foo\r\ndescription: A skill\r\n---\r\n\r\nbody";
        let p = parse_frontmatter(content).unwrap();
        assert_eq!(p.name, "foo");
        assert_eq!(p.description, "A skill");
    }

    #[test]
    fn strip_frontmatter_tolerates_crlf() {
        let content = "---\r\nname: foo\r\ndescription: d\r\n---\r\n\r\n# Title\r\n\r\ntext";
        assert_eq!(strip_frontmatter(content), "# Title\n\ntext");
    }

    #[test]
    fn discover_handles_crlf_skill_file() {
        let root = tmp_dir("discover_crlf");
        let dir = root.join("crlf-skill");
        fs::create_dir_all(&dir).unwrap();
        let content = "---\r\nname: crlf-skill\r\ndescription: CRLF skill\r\n---\r\n\r\nbody";
        fs::write(dir.join("SKILL.md"), content).unwrap();
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "crlf-skill");
    }

    #[test]
    fn discover_handles_gbk_skill_file() {
        // 国内 Windows 记事本默认存 GBK/CP936;UTF-8 解码会失败,需回退 GB18030。
        let root = tmp_dir("discover_gbk");
        let dir = root.join("gbk-skill");
        fs::create_dir_all(&dir).unwrap();
        let (gb_bytes, _, _) =
            encoding_rs::GB18030.encode("---\nname: gbk-skill\ndescription: 中文描述\n---\n\n正文");
        // 确认确实是 GBK(非 UTF-8)——含高位字节,from_utf8 应失败
        assert!(
            String::from_utf8(gb_bytes.to_vec()).is_err(),
            "测试前置:编码后应非 UTF-8"
        );
        fs::write(dir.join("SKILL.md"), &*gb_bytes).unwrap();
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1, "GBK skill 应被回退解码并加载");
        assert_eq!(skills[0].name, "gbk-skill");
        assert_eq!(skills[0].description, "中文描述");
    }

    #[test]
    fn discover_finds_user_skill() {
        let root = tmp_dir("discover_user");
        write_skill(
            &root,
            "",
            "my-skill",
            "name: my-skill\ndescription: desc",
            "body",
        );
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].scope, SkillScope::User);
    }

    #[test]
    fn discover_finds_builtin_skill() {
        let root = tmp_dir("discover_builtin");
        write_skill(
            &root,
            ".builtin",
            "creator",
            "name: creator\ndescription: d",
            "b",
        );
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].scope, SkillScope::Builtin);
    }

    #[test]
    fn discover_skips_dot_dirs_except_builtin() {
        let root = tmp_dir("discover_dots");
        write_skill(
            &root,
            ".cache",
            "hidden",
            "name: hidden\ndescription: d",
            "b",
        );
        write_skill(&root, "", "visible", "name: visible\ndescription: d", "b");
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "visible");
    }

    #[test]
    fn discover_name_fallback_to_dirname() {
        let root = tmp_dir("discover_fallback");
        write_skill(&root, "", "from-dir", "description: no name field", "b");
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "from-dir");
    }

    #[test]
    fn discover_skips_invalid_name() {
        let root = tmp_dir("discover_invalid");
        write_skill(&root, "", "Bad_Name", "description: d", "b");
        write_skill(&root, "", "good", "description: d", "b");
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn discover_nonexistent_root_returns_empty() {
        let skills = discover_skills(Path::new("/nonexistent/path/xyz")).unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn discover_skips_bad_hyphens() {
        // 首尾/连续连字符的 name 对齐 codex 拒绝(mention 正则也匹配不到)
        let root = tmp_dir("discover_hyphens");
        write_skill(&root, "", "-bad", "description: d", "b");
        write_skill(&root, "", "bad-", "description: d", "b");
        write_skill(&root, "", "a--b", "description: d", "b");
        write_skill(&root, "", "good", "description: d", "b");
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1, "仅 good 应保留");
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn discover_accepts_overlong_description() {
        // 对齐 codex:description 超长不拒载(parser 只校验 name),
        // 渲染层截断到 1024(1021 + "...")。loader 应两个都加载。
        let root = tmp_dir("discover_long_desc");
        let long_desc = "x".repeat(1025);
        write_skill(
            &root,
            "",
            "long-skill",
            &format!("name: long-skill\ndescription: {long_desc}"),
            "b",
        );
        write_skill(&root, "", "good", "name: good\ndescription: ok", "b");
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 2, "两个都应加载(渲染层才截断)");
        // 渲染层截断:1025 → 1021 字符 + "..."
        let truncated = truncate_catalog_skill_description(&long_desc);
        assert_eq!(truncated.chars().count(), 1_024);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn parse_frontmatter_repairs_unquoted_colon() {
        // description 含未加引号的 ": "（散文冒号），原会破坏 YAML 解析 → 整 skill 被丢；
        // 修复应按行单引号包裹后重试，保住 skill。
        let content = "---\nname: reports\ndescription: Reports: generate and email\n---\n\nbody";
        let p = parse_frontmatter(content).expect("修复后应解析成功");
        assert_eq!(p.name, "reports");
        assert_eq!(p.description, "Reports: generate and email");
    }

    #[test]
    fn parse_frontmatter_repairs_multiple_needing_lines() {
        // 多行裸标量含 ": "，每行都应被独立修复
        let content = "---\nname: multi\ndescription: Tip: do X then Y\nargument-hint: Build for AWS: ECS\n---\n\nbody";
        let p = parse_frontmatter(content).expect("多行修复后应解析成功");
        assert_eq!(p.name, "multi");
        assert_eq!(p.description, "Tip: do X then Y");
    }

    #[test]
    fn parse_frontmatter_preserves_block_scalar_during_repair() {
        // 一处块标量 description、另一处裸标量含 ": "：块标量行及其缩进续行应原样保留，
        // 不被误判为需修复的裸标量（否则会把正文引号包裹破坏内容）。
        let content = "---\nname: blk\ndescription: |\n  body line one\nargument-hint: Build for AWS: ECS\n---\n\nx";
        let p = parse_frontmatter(content).expect("应解析成功");
        assert_eq!(p.name, "blk");
        // 块标量正文（折叠空白后）应保留
        assert!(
            p.description.contains("body line one"),
            "块标量未被破坏: {}",
            p.description
        );
    }

    #[test]
    fn parse_frontmatter_already_quoted_colon_unmolested() {
        // 已用引号包裹的含冒号标量本就能解析，不应被修复逻辑改动
        let content = "---\nname: q\ndescription: \"has: colon\"\n---\n\nbody";
        let p = parse_frontmatter(content).expect("引号标量应正常解析");
        assert_eq!(p.description, "has: colon");
    }
}
