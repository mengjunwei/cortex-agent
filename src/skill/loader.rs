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
use crate::skill::{SkillMetadata, SkillScope, is_valid_skill_name};

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
pub fn parse_frontmatter(content: &str) -> Option<ParsedFrontmatter> {
    let normalized = normalize_line_endings(content);
    let content = normalized.strip_prefix('\u{feff}').unwrap_or(&normalized);
    let after_open = content.strip_prefix("---\n")?;
    let end = after_open.find("\n---\n")?;
    let yaml_str = &after_open[..end];
    let parsed: FrontmatterYaml = serde_yaml::from_str(yaml_str).ok()?;
    let description = parsed.description?.trim().to_string();
    if description.is_empty() {
        return None;
    }
    Some(ParsedFrontmatter {
        name: parsed.name.unwrap_or_default(),
        description,
        short_description: parsed.metadata.short_description,
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
    // description 长度校验(对齐 codex:≤1024 字符)
    if parsed.description.chars().count() > 1024 {
        return Err(format!(
            "description 超过 1024 字符(当前 {}),请精简",
            parsed.description.chars().count()
        ));
    }
    Ok(SkillMetadata {
        name,
        description: parsed.description,
        short_description: parsed.short_description,
        path: skill_file.to_path_buf(),
        scope,
    })
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
    fn discover_skips_overlong_description() {
        // description > 1024 字符对齐 codex 拒绝
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
        assert_eq!(skills.len(), 1, "仅 good 应保留");
        assert_eq!(skills[0].name, "good");
    }
}
