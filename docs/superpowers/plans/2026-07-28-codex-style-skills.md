# Codex-Style Skill System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the DB-based skill system with a Codex-style file-system skill system featuring progressive disclosure (catalog in system prompt + `$name` mention body injection + `read_skill` tool).

**Architecture:** New `src/skill/` module with self-built data model (no `adk-skill` dependency). Skills discovered from `{data_dir}/skills/*/SKILL.md` + compile-time-embedded builtin skills. Three injection points: catalog block into system prompt (custom.rs), `$name`-parsed bodies appended to user message (sse.rs), `read_skill` ADK FunctionTool registered on every custom agent.

**Tech Stack:** Rust edition 2024, `include_dir = "0.7"` (new), `serde_yaml = "0.9"` (new), `regex = "1"` (existing), `adk_rust::tool::FunctionTool` (existing pattern), `axum`/`async-graphql` (existing).

## Global Constraints

- **Skill name format**: `^[a-z0-9-]+$`, 1-64 chars (validated in `loader.rs`, enforced by `is_valid_skill_name`).
- **Frontmatter fields parsed**: only `name`, `description` (required), `metadata.short-description` (optional). All other YAML keys ignored.
- **Token budget**: catalog block capped at `catalog_token_budget_pct` (default 2) of context window (default 128000).
- **Body injection cap**: `max_inject_chars` (default 1500, retained from existing config).
- **Scope precedence**: User skills override Builtin skills of the same name.
- **Builtin install dir**: `{skill_dir}/.builtin/` (dot-prefixed, whitelisted in BFS scan).
- **Mention sigil**: `$` followed by `[a-z0-9-]+`.
- **No DB migration for cleanup**: delete `migrations/9.sql` and `ALTER TABLE` lines; existing deployed columns/tables are harmless residue (code stops reading them).
- **`adk-skill` dependency removed** from `Cargo.toml` in the deletion task.
- **Commit style**: terse, matches repo history (e.g., "重构: ...", "新增: ...").
- **Verification**: after each task, run `cargo build` (must pass); after deletion task, run `cargo clippy --all-targets -- -D warnings` + `cargo test --lib`.

---

## File Structure

### New files

| Path | Responsibility |
|------|----------------|
| `src/skill/mod.rs` | Module entry; public types `SkillMetadata`, `SkillScope`, `SkillCatalog`, `SkillService`; re-exports |
| `src/skill/loader.rs` | Filesystem BFS discovery + frontmatter YAML parse + `is_valid_skill_name` + `install_builtin_skills` |
| `src/skill/catalog.rs` | `SkillService` impl: `render_catalog_block`, `find_by_name`, `read_skill_text`, `resolve_mentions` |
| `src/skill/mention.rs` | `extract_mentions(&str) -> Vec<String>` ($name parser, regex-based) |
| `src/skill/inject.rs` | `render_skill_body_block(name, text, max_chars) -> String` (XML wrapper + truncation) |
| `src/skill/assets/builtin/skill-creator/SKILL.md` | Adapted from Codex (path refs → `{data_dir}/skills`, description tweaked) |
| `src/skill/assets/builtin/skill-creator/scripts/init_skill.py` | Simplified (no openai.yaml generation) |
| `src/skill/assets/builtin/skill-creator/scripts/quick_validate.py` | Verbatim from Codex |
| `src/tools/skill_read.rs` | `create_read_skill_tool(svc: Arc<SkillService>) -> FunctionTool` |

### Modified files

| Path | Change |
|------|--------|
| `Cargo.toml` | + `include_dir`, `serde_yaml`; − `adk-skill` (deletion task) |
| `src/lib.rs` | + `pub mod skill;` |
| `src/tools/mod.rs` | + `pub mod skill_read;` |
| `src/config/mod.rs` | `SkillConfig`: drop `tools_mode` + `auto_match`, add `catalog_token_budget_pct`; keep `max_inject_chars` |
| `src/bootstrap.rs` | `skill_manager` field → `skill_service: Option<Arc<SkillService>>`; init logic rewritten (no DB dep) |
| `src/server/mod.rs` | Drop `pub(crate) mod skill;` (GraphQL handler removed) |
| `src/server/graphql.rs` | Remove 9 skill resolver fns (lines 248-265, 597-648) |
| `src/server/sse.rs` | Replace `skill_manager`/`skill_docs` logic with `extract_mentions` + `resolve_mentions`; drop browser-requires-skill check; add catalog passing |
| `src/agent/custom.rs` | Drop `narrow_tools_by_skills` + `skill_docs` param; add `skill_service` to `AgentContext`; inject catalog into instruction; register `read_skill` tool |
| `src/agent/orchestration.rs` | Drop child-skill resolution (lines 175-181) |
| `src/domain/assistant/models.rs` | Drop `enabled_skills` field (struct + Row + From + CustomAssistantInput + test fixture) |
| `src/domain/assistant/store.rs` | Drop `enabled_skills` column I/O + ALTER + `encode_skills` + `purge_skill_from_assistants` |
| `src/server/assistant.rs` | Drop `enabled_skills` from DTOs + test fixtures |
| `docs/architecture.md` | §11 roadmap entry for skill system migration |

### Deleted files

| Path | Reason |
|------|--------|
| `src/domain/skill/` (all 7 files) | Replaced by `src/skill/` |
| `src/server/skill.rs` | GraphQL skill CRUD handlers removed |
| `migrations/9.sql` | Skill table DDL removed |

---

## Task 1: Add dependencies + module skeleton

**Files:**
- Modify: `Cargo.toml` (add deps, do NOT remove `adk-skill` yet)
- Create: `src/skill/mod.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: empty `crate::skill` module (compilable, no public items yet)

- [ ] **Step 1: Add dependencies to Cargo.toml**

In `D:\code\rust\cortex-agent\Cargo.toml`, find the `[dependencies]` section (around line 50 where `regex = "1"` lives). Add two new lines after `regex = "1"`:

```toml
include_dir = "0.7"
serde_yaml = "0.9"
```

Do NOT remove `adk-skill = "1"` yet (deletion happens in Task 12).

- [ ] **Step 2: Create empty skill module**

Create `D:\code\rust\cortex-agent\src\skill\mod.rs`:

```rust
//! Codex-style skill system — file-system discovery + progressive disclosure injection.
//!
//! See `docs/superpowers/specs/2026-07-28-codex-style-skills-design.md` for full design.
//!
//! Submodules (added incrementally in later tasks):
//! - [`loader`]: filesystem BFS discovery + frontmatter parse
//! - [`catalog`]: `SkillService` — catalog rendering + skill text lookup
//! - [`mention`]: `$skill-name` parser
//! - [`inject`]: XML body-block renderer
```

- [ ] **Step 3: Register module in lib.rs**

In `D:\code\rust\cortex-agent\src\lib.rs`, after line 9 (`pub mod domain;`), add:

```rust
pub mod skill;
```

- [ ] **Step 4: Verify build**

Run: `cargo build`
Expected: PASS (compiles with empty module + new deps downloaded)

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/skill/mod.rs
git commit -m "新增: skill 模块骨架 + include_dir/serde_yaml 依赖"
```

---

## Task 2: Data model + name validation

**Files:**
- Modify: `src/skill/mod.rs`

**Interfaces:**
- Produces: `crate::skill::SkillMetadata`, `crate::skill::SkillScope`, `crate::skill::SkillCatalog`, `crate::skill::is_valid_skill_name`

- [ ] **Step 1: Write the failing tests**

Append to `D:\code\rust\cortex-agent\src\skill\mod.rs` (after the module doc comment):

```rust
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

/// 校验 skill name:仅 `[a-z0-9-]`,1-64 字符,非空。
pub fn is_valid_skill_name(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 {
        return false;
    }
    n.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test --lib skill::tests`
Expected: PASS (4 tests)

- [ ] **Step 3: Verify full build**

Run: `cargo build`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/skill/mod.rs
git commit -m "新增: skill 数据模型 (SkillMetadata/SkillScope/SkillCatalog) + name 校验"
```

---

## Task 3: Frontmatter parser + filesystem discovery (loader.rs)

**Files:**
- Create: `src/skill/loader.rs`
- Modify: `src/skill/mod.rs` (add `pub mod loader;` + re-exports)

**Interfaces:**
- Consumes: `crate::skill::{SkillMetadata, SkillScope, is_valid_skill_name}`, `crate::error::AppError`
- Produces:
  - `crate::skill::loader::parse_frontmatter(content: &str) -> Option<ParsedFrontmatter>`
  - `crate::skill::loader::discover_skills(root: &Path) -> Result<Vec<SkillMetadata>, AppError>`
  - `crate::skill::loader::strip_frontmatter(content: &str) -> &str` (returns body after frontmatter)
  - `crate::skill::ParsedFrontmatter { name, description, short_description }`

- [ ] **Step 1: Write the failing tests**

Create `D:\code\rust\cortex-agent\src\skill\loader.rs`:

```rust
//! Filesystem BFS discovery + SKILL.md frontmatter parsing.
//!
//! 扫描规则(对齐 Codex `discover_skills_under_root`,简化):
//! - 从 skill_dir 起 BFS,最大深度 3 层
//! - 跳过点号开头目录(除了 `.builtin` — 显式白名单)
//! - 跳过非 `SKILL.md` 文件名(大小写敏感)
//! - 同名 skill:User scope 覆盖 Builtin scope

use std::path::{Path, PathBuf};

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
pub fn parse_frontmatter(content: &str) -> Option<ParsedFrontmatter> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
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
pub fn strip_frontmatter(content: &str) -> &str {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some(after_open) = content.strip_prefix("---\n") else {
        return content;
    };
    let Some(end) = after_open.find("\n---\n") else {
        return content;
    };
    &after_open[end + "\n---\n".len()..]
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
            Err(e) => tracing::warn!(
                "[skill] 跳过无效 SKILL.md {}: {}",
                skill_file.display(),
                e
            ),
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
    let content =
        std::fs::read_to_string(skill_file).map_err(|e| format!("读取失败: {e}"))?;
    let parsed = parse_frontmatter(&content)
        .ok_or_else(|| "frontmatter 缺失或格式无效(需 ---\\n...\\n---\\n + description)".to_string())?;
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
        return Err(format!("name '{name}' 非法(需 ^[a-z0-9-]+$,1-64 字符)"));
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
        let base = std::env::temp_dir().join(format!("cortex-skill-test-{}-{}", name, std::process::id()));
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
    fn discover_finds_user_skill() {
        let root = tmp_dir("discover_user");
        write_skill(&root, "", "my-skill", "name: my-skill\ndescription: desc", "body");
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].scope, SkillScope::User);
    }

    #[test]
    fn discover_finds_builtin_skill() {
        let root = tmp_dir("discover_builtin");
        write_skill(&root, ".builtin", "creator", "name: creator\ndescription: d", "b");
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].scope, SkillScope::Builtin);
    }

    #[test]
    fn discover_skips_dot_dirs_except_builtin() {
        let root = tmp_dir("discover_dots");
        write_skill(&root, ".cache", "hidden", "name: hidden\ndescription: d", "b");
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
}
```

- [ ] **Step 2: Register loader in mod.rs**

In `D:\code\rust\cortex-agent\src\skill\mod.rs`, immediately after the module doc comment (before `use std::collections...`), add:

```rust
pub mod loader;

pub use loader::ParsedFrontmatter;
```

- [ ] **Step 3: Run tests to verify they fail-then-pass**

Run: `cargo test --lib skill::loader`
Expected: PASS (11 tests)

- [ ] **Step 4: Verify full build**

Run: `cargo build`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/skill/mod.rs src/skill/loader.rs
git commit -m "新增: skill loader — BFS 文件发现 + frontmatter YAML 解析"
```

---

## Task 4: `$name` mention parser (mention.rs)

**Files:**
- Create: `src/skill/mention.rs`
- Modify: `src/skill/mod.rs`

**Interfaces:**
- Produces: `crate::skill::mention::extract_mentions(text: &str) -> Vec<String>` (deduped, ordered by first appearance)

- [ ] **Step 1: Write the failing tests + implementation together**

Create `D:\code\rust\cortex-agent\src\skill\mention.rs`:

```rust
//! `$skill-name` mention parser.
//!
//! 语法:`$` 后跟 `[a-z0-9-]+`,长度 1-64。
//! 同一 name 多次提及只返回一次;保留首次出现顺序。
//! 不存在的 name 不在此过滤(由调用方与 catalog 交叉校验后丢弃)。

use regex::Regex;
use std::sync::OnceLock;

/// `$name` 匹配正则:name = `(?:[a-z0-9]+-)*[a-z0-9]+`
fn mention_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\$(?:[a-z0-9]+-)*[a-z0-9]+").unwrap())
}

/// 从文本中提取 `$skill-name` 提及,去重并保留首次出现顺序。
pub fn extract_mentions(text: &str) -> Vec<String> {
    let re = mention_regex();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let name = &m.as_str()[1..]; // 去掉前导 $
        if name.len() <= 64 && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_mention() {
        assert_eq!(
            extract_mentions("帮我用 $skill-creator 创建一个 skill"),
            vec!["skill-creator".to_string()]
        );
    }

    #[test]
    fn multiple_mentions() {
        assert_eq!(
            extract_mentions("$foo 和 $bar-bar"),
            vec!["foo".to_string(), "bar-bar".to_string()]
        );
    }

    #[test]
    fn deduplicates() {
        assert_eq!(
            extract_mentions("$foo $foo $foo"),
            vec!["foo".to_string()]
        );
    }

    #[test]
    fn preserves_first_occurrence_order() {
        assert_eq!(
            extract_mentions("$b $a $c $a"),
            vec!["b".to_string(), "a".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn no_mention_returns_empty() {
        assert!(extract_mentions("普通文本无提及").is_empty());
        assert!(extract_mentions("$ Foo 大写不匹配").is_empty());
    }

    #[test]
    fn rejects_uppercase_underscore() {
        // $Foo / $_foo 不匹配(大写/下划线不合法)
        assert!(extract_mentions("use $Foo here").is_empty());
        assert!(extract_mentions("use $_foo here").is_empty());
        // $foo_bar → 正则在 `_` 处终止,匹配出 `foo`(`_` 不在 [a-z0-9-] 字符类)
        assert_eq!(extract_mentions("use $foo_bar"), vec!["foo".to_string()]);
    }

    #[test]
    fn leading_digit_ok() {
        assert_eq!(extract_mentions("$1skill"), vec!["1skill".to_string()]);
    }

    #[test]
    fn rejects_overlong_name() {
        let long = "a".repeat(70);
        let text = format!("${long}");
        assert!(extract_mentions(&text).is_empty());
    }
}
```

- [ ] **Step 2: Register in mod.rs**

In `D:\code\rust\cortex-agent\src\skill\mod.rs`, add after `pub mod loader;`:

```rust
pub mod mention;
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib skill::mention`
Expected: PASS (8 tests)

- [ ] **Step 4: Commit**

```bash
git add src/skill/mod.rs src/skill/mention.rs
git commit -m "新增: skill mention 解析器 — \$name 提及语法"
```

---

## Task 5: Body injection XML renderer (inject.rs)

**Files:**
- Create: `src/skill/inject.rs`
- Modify: `src/skill/mod.rs`

**Interfaces:**
- Consumes: `crate::skill::loader::strip_frontmatter`
- Produces: `crate::skill::inject::render_skill_body_block(name: &str, raw_text: &str, max_chars: usize) -> String`

- [ ] **Step 1: Write the failing tests + implementation**

Create `D:\code\rust\cortex-agent\src\skill\inject.rs`:

```rust
//! Skill 正文注入块渲染器。
//!
//! 输出格式:
//! ```text
//! <skill name="skill-creator">
//! <description>指导创建 skill...</description>
//!
//! (正文,去掉 frontmatter,截断到 max_chars)
//! </skill>
//! ```

use crate::skill::loader::strip_frontmatter;

/// 渲染 skill 正文为 XML 包裹块。
///
/// - `raw_text`:SKILL.md 全文(含 frontmatter);函数内部调用 `strip_frontmatter`
/// - `max_chars`:正文最大字符数;超出则截断并追加截断标记
pub fn render_skill_body_block(name: &str, raw_text: &str, max_chars: usize) -> String {
    let body = strip_frontmatter(raw_text);
    let description = crate::skill::loader::parse_frontmatter(raw_text)
        .map(|p| p.description)
        .unwrap_or_default();

    let mut out = String::with_capacity(128 + body.len().min(max_chars + 64));
    out.push_str("<skill name=\"");
    out.push_str(name);
    out.push_str("\">\n");
    if !description.is_empty() {
        out.push_str("<description>");
        out.push_str(&description);
        out.push_str("</description>\n\n");
    }

    let body_trimmed = body.trim();
    if body_trimmed.chars().count() <= max_chars {
        out.push_str(body_trimmed);
    } else {
        let truncated: String = body_trimmed.chars().take(max_chars).collect();
        let original_len = body_trimmed.chars().count();
        out.push_str(&truncated);
        out.push_str(&format!("\n\n...[截断:原文 {original_len} 字符]"));
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
        assert!(block.starts_with("<skill name=\"foo\">\n"));
        assert!(block.contains("<description>A foo skill</description>"));
        assert!(block.contains("# Foo"));
        assert!(block.ends_with("</skill>"));
    }

    #[test]
    fn truncates_when_over_max() {
        let long_body = "x".repeat(100);
        let content = format!("---\nname: foo\ndescription: d\n---\n\n{long_body}");
        let block = render_skill_body_block("foo", &content, 10);
        assert!(block.contains("...[截断:原文 100 字符]"));
        // 截断后的正文部分应 ≤ 10 字符
        let body_section = block.split("</description>\n\n").nth(1).unwrap();
        let body_only = body_section.strip_suffix("\n</skill>").unwrap();
        assert!(body_only.lines().next().unwrap().chars().count() <= 10);
    }

    #[test]
    fn no_truncation_when_under_max() {
        let block = render_skill_body_block("foo", SAMPLE, 1000);
        assert!(!block.contains("[截断"));
    }

    #[test]
    fn handles_missing_description() {
        let content = "no frontmatter here";
        let block = render_skill_body_block("foo", content, 1000);
        // 无 frontmatter → 无 <description> 标签
        assert!(!block.contains("<description>"));
        assert!(block.contains("no frontmatter here"));
    }
}
```

- [ ] **Step 2: Register in mod.rs**

In `D:\code\rust\cortex-agent\src\skill\mod.rs`, add:

```rust
pub mod inject;
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib skill::inject`
Expected: PASS (4 tests)

- [ ] **Step 4: Commit**

```bash
git add src/skill/mod.rs src/skill/inject.rs
git commit -m "新增: skill inject — 正文 XML 包裹 + 截断渲染"
```

---

## Task 6: SkillService + catalog rendering + builtin install (catalog.rs)

**Files:**
- Create: `src/skill/catalog.rs`
- Modify: `src/skill/mod.rs`
- Create: `src/skill/assets/.gitkeep` (placeholder so the assets dir exists; real builtin skill added in Task 7)

**Interfaces:**
- Consumes: `crate::skill::{SkillCatalog, SkillMetadata, SkillScope}`, `crate::skill::loader::{discover_skills, parse_frontmatter}`, `crate::error::AppError`
- Produces:
  - `crate::skill::SkillService { new(skill_dir) -> Result<Self>, render_catalog_block(budget_pct) -> String, find_by_name, read_skill_text, resolve_mentions }`
  - `crate::skill::BUILTIN_VERSION` (const string for marker file)

- [ ] **Step 1: Create empty assets placeholder**

Create directory `D:\code\rust\cortex-agent\src\skill\assets\builtin\.gitkeep` (empty file) so the `include_dir!` macro in Task 7 has a valid path. For now, `install_builtin_skills` is a no-op stub.

- [ ] **Step 2: Write the implementation**

Create `D:\code\rust\cortex-agent\src\skill\catalog.rs`:

```rust
//! `SkillService` — 持有 catalog,提供目录渲染 + skill 正文查找。
//!
//! 构建后只读(启动时构建一次);`reload` 需 `&mut`,本期不暴露。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::skill::loader::discover_skills;
use crate::skill::mention::extract_mentions;
use crate::skill::{SkillCatalog, SkillMetadata, SkillScope};

/// 内置 skill 版本标记(变更时触发重写 `.builtin/`)。
pub const BUILTIN_VERSION: &str = "v1";

const BUILTIN_DIR_NAME: &str = ".builtin";
const BUILTIN_MARKER: &str = ".cortex-builtin-version";
/// 默认上下文窗口(用于预算计算;真实值应从 model 配置读取,这里给保守默认)
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

/// Skill 服务:持有 catalog,提供目录渲染 + 正文查找。
pub struct SkillService {
    catalog: SkillCatalog,
    skill_dir: PathBuf,
}

impl SkillService {
    /// 启动时构建。
    ///
    /// 步骤:
    /// 1. 确保 `skill_dir` 存在
    /// 2. 安装内置 skill(解压 include_dir 到 `.builtin/`,版本变化时重写)
    /// 3. 扫描所有 SKILL.md(Builtin + User)
    /// 4. 去重(User 覆盖 Builtin),构建 catalog
    pub fn new(skill_dir: PathBuf) -> Result<Self, AppError> {
        std::fs::create_dir_all(&skill_dir).map_err(|e| {
            AppError::FileError(format!(
                "创建 skill 根目录失败 {}: {e}",
                skill_dir.display()
            ))
        })?;

        install_builtin_skills(&skill_dir)?;

        let raw = discover_skills(&skill_dir)?;
        let catalog = build_catalog(raw);

        tracing::info!(
            "[skill] catalog 加载完成: {} 个有效 skill ({} builtin / {} user)",
            catalog.skills.len(),
            catalog.skills.iter().filter(|s| s.scope == SkillScope::Builtin).count(),
            catalog.skills.iter().filter(|s| s.scope == SkillScope::User).count(),
        );

        Ok(Self { catalog, skill_dir })
    }

    /// 渲染 skill 目录到 system prompt 片段。
    ///
    /// `budget_pct`:目录占上下文窗口的百分比(默认 2)。
    /// 超预算时缩短 description,再删除末尾 skill。
    pub fn render_catalog_block(&self, budget_pct: u8) -> String {
        if self.catalog.is_empty() {
            return String::new();
        }
        let budget_chars = (DEFAULT_CONTEXT_WINDOW * budget_pct as usize) / 100;
        render_catalog_inner(&self.catalog.skills, budget_chars)
    }

    /// 按 name 查找元数据。
    pub fn find_by_name(&self, name: &str) -> Option<&SkillMetadata> {
        self.catalog.find_by_name(name)
    }

    /// 读取 skill 正文全文(含 frontmatter,供 inject 层 strip)。
    /// 不存在或读取失败返回 None。
    pub fn read_skill_raw(&self, name: &str) -> Option<String> {
        let meta = self.find_by_name(name)?;
        match std::fs::read_to_string(&meta.path) {
            Ok(text) => Some(text),
            Err(e) => {
                tracing::warn!(
                    "[skill] 读取 SKILL.md 失败 {}: {e}",
                    meta.path.display()
                );
                None
            }
        }
    }

    /// 读取 skill 正文(去掉 frontmatter)。不存在返回 None。
    pub fn read_skill_text(&self, name: &str) -> Option<String> {
        let raw = self.read_skill_raw(name)?;
        Some(crate::skill::loader::strip_frontmatter(&raw).to_string())
    }

    /// 批量解析 `$name` 提及 → 正文注入块。
    ///
    /// - 从 `user_text` 提取 `$name`
    /// - 与 catalog 交叉校验,丢弃不存在的 name
    /// - 返回每个有效 skill 的 `render_skill_body_block` 输出
    pub fn resolve_mentions(&self, user_text: &str, max_chars: usize) -> Vec<String> {
        let mentions = extract_mentions(user_text);
        let mut blocks = Vec::with_capacity(mentions.len());
        for name in &mentions {
            if let Some(raw) = self.read_skill_raw(name) {
                blocks.push(crate::skill::inject::render_skill_body_block(
                    name, &raw, max_chars,
                ));
            } else {
                tracing::debug!("[skill] 提及 '${name}' 在 catalog 中不存在,跳过");
            }
        }
        blocks
    }

    /// 返回 skill 根目录(主要供测试/调试)。
    pub fn skill_dir(&self) -> &Path {
        &self.skill_dir
    }
}

/// 把原始发现结果去重(User 覆盖 Builtin),构建 catalog。
fn build_catalog(mut raw: Vec<SkillMetadata>) -> SkillCatalog {
    // 按 (scope_rank, name) 排序:Builtin(0) 在前,User(1) 在后;同 scope 按 name 字典序
    raw.sort_by(|a, b| {
        a.scope
            .rank()
            .cmp(&b.scope.rank())
            .then_with(|| a.name.cmp(&b.name))
    });

    // 同名去重:保留排序中最后一个(User 在后,会覆盖 Builtin)
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut skills: Vec<SkillMetadata> = Vec::new();
    for meta in raw {
        match seen.get(&meta.name) {
            Some(&idx) => {
                // 覆盖(当前 meta 的 scope 排序更靠后 = 优先级更高)
                skills[idx] = meta;
            }
            None => {
                seen.insert(meta.name.clone(), skills.len());
                skills.push(meta);
            }
        }
    }
    let by_name = seen;
    SkillCatalog { skills, by_name }
}

impl SkillScope {
    fn rank(self) -> u8 {
        match self {
            SkillScope::Builtin => 0,
            SkillScope::User => 1,
        }
    }
}

/// 渲染目录块(带预算截断)。
fn render_catalog_inner(skills: &[SkillMetadata], budget_chars: usize) -> String {
    let header = "## 可用 Skill\n\n\
以下 skill 可在本次对话中使用。使用方式:\n\
- 在消息中写 `$skill-name` 显式触发,对应 skill 正文会自动注入\n\
- 或由你根据任务相关性自主决定,调用 `read_skill` 工具拉取正文\n\n\
### Skill 目录\n";

    let mut lines: Vec<(String, String)> = skills
        .iter()
        .map(|s| {
            let desc = s
                .short_description
                .as_deref()
                .filter(|d| !d.is_empty())
                .unwrap_or(&s.description);
            let scope_tag = match s.scope {
                SkillScope::Builtin => "内置",
                SkillScope::User => "用户",
            };
            (format!("- {}: {}", s.name, desc), format!(" ({})", scope_tag))
        })
        .collect();

    // 计算当前总长度
    let total_len: usize = lines.iter().map(|(l, tag)| l.len() + tag.len() + 1).sum();
    if total_len <= budget_chars {
        // 无需截断
        let mut out = String::from(header);
        for (line, tag) in &lines {
            out.push_str(line);
            out.push_str(tag);
            out.push('\n');
        }
        return out;
    }

    // 超预算:逐字符缩短 description(从最长行开始削),对齐 Codex 策略简化版
    // 简化:按比例截断每个 description 到能装下为止;若仍不够,删除末尾 skill
    let header_len = header.len();
    let available = budget_chars.saturating_sub(header_len);

    // 尝试逐个删除末尾 skill 直到能装下
    while !lines.is_empty() {
        let curr_len: usize = lines.iter().map(|(l, tag)| l.len() + tag.len() + 1).sum();
        if curr_len <= available {
            break;
        }
        lines.pop();
    }

    if lines.is_empty() {
        // 预算太小,连一行都装不下 — 只返回 header(至少让模型知道有 skill)
        return String::from(header);
    }

    let mut out = String::from(header);
    for (line, tag) in &lines {
        out.push_str(line);
        out.push_str(tag);
        out.push('\n');
    }
    out
}

/// 安装内置 skill 到 `{skill_dir}/.builtin/`。
///
/// 当前为 stub(Task 7 接入 `include_dir!`)。这里只确保 `.builtin/` 目录存在,
/// 让 `discover_skills` 的白名单扫描不会因目录缺失报错。
fn install_builtin_skills(skill_dir: &Path) -> Result<(), AppError> {
    let builtin_dir = skill_dir.join(BUILTIN_DIR_NAME);
    std::fs::create_dir_all(&builtin_dir).map_err(|e| {
        AppError::FileError(format!(
            "创建内置 skill 目录失败 {}: {e}",
            builtin_dir.display()
        ))
    })?;
    // Task 7 在此插入 include_dir 解压逻辑
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_skill_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cortex-skill-svc-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill_at(root: &Path, sub: &str, name: &str, desc: &str) {
        let dir = root.join(sub).join(name);
        fs::create_dir_all(&dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nBody text.");
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn new_loads_skills_into_catalog() {
        let dir = tmp_skill_dir("load");
        write_skill_at(&dir, "", "alpha", "Alpha skill");
        write_skill_at(&dir, "", "beta", "Beta skill");
        let svc = SkillService::new(dir).unwrap();
        assert_eq!(svc.catalog.skills.len(), 2);
        assert!(svc.find_by_name("alpha").is_some());
        assert!(svc.find_by_name("beta").is_some());
        assert!(svc.find_by_name("missing").is_none());
    }

    #[test]
    fn user_overrides_builtin_same_name() {
        let dir = tmp_skill_dir("override");
        write_skill_at(&dir, ".builtin", "shared", "Builtin version");
        write_skill_at(&dir, "", "shared", "User version");
        let svc = SkillService::new(dir).unwrap();
        let meta = svc.find_by_name("shared").unwrap();
        assert_eq!(meta.description, "User version");
        assert_eq!(meta.scope, SkillScope::User);
    }

    #[test]
    fn render_catalog_block_includes_names() {
        let dir = tmp_skill_dir("render");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        let svc = SkillService::new(dir).unwrap();
        let block = svc.render_catalog_block(2);
        assert!(block.contains("alpha"));
        assert!(block.contains("Alpha desc"));
        assert!(block.contains("## 可用 Skill"));
    }

    #[test]
    fn render_catalog_empty_returns_empty() {
        let dir = tmp_skill_dir("empty");
        let svc = SkillService::new(dir).unwrap();
        assert_eq!(svc.render_catalog_block(2), "");
    }

    #[test]
    fn read_skill_text_strips_frontmatter() {
        let dir = tmp_skill_dir("read");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        let svc = SkillService::new(dir).unwrap();
        let body = svc.read_skill_text("alpha").unwrap();
        assert!(body.contains("# alpha"));
        assert!(!body.contains("description"));
        assert!(!body.contains("---"));
    }

    #[test]
    fn read_skill_text_missing_returns_none() {
        let dir = tmp_skill_dir("read_missing");
        let svc = SkillService::new(dir).unwrap();
        assert!(svc.read_skill_text("nope").is_none());
    }

    #[test]
    fn resolve_mentions_injects_bodies() {
        let dir = tmp_skill_dir("resolve");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        let svc = SkillService::new(dir).unwrap();
        let blocks = svc.resolve_mentions("use $alpha please", 1000);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("<skill name=\"alpha\">"));
    }

    #[test]
    fn resolve_mentions_drops_nonexistent() {
        let dir = tmp_skill_dir("resolve_missing");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        let svc = SkillService::new(dir).unwrap();
        let blocks = svc.resolve_mentions("use $alpha and $missing", 1000);
        assert_eq!(blocks.len(), 1); // missing 被丢弃
    }
}
```

- [ ] **Step 3: Register in mod.rs**

In `D:\code\rust\cortex-agent\src\skill\mod.rs`, add:

```rust
pub mod catalog;

pub use catalog::SkillService;
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib skill::catalog`
Expected: PASS (8 tests)

- [ ] **Step 5: Verify full build**

Run: `cargo build`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/skill/mod.rs src/skill/catalog.rs src/skill/assets/builtin/.gitkeep
git commit -m "新增: SkillService — catalog 渲染 + 正文查找 + 预算截断"
```

---

## Task 7: Builtin skill-creator assets + install

**Files:**
- Create: `src/skill/assets/builtin/skill-creator/SKILL.md`
- Create: `src/skill/assets/builtin/skill-creator/scripts/init_skill.py`
- Create: `src/skill/assets/builtin/skill-creator/scripts/quick_validate.py`
- Modify: `src/skill/catalog.rs` (wire `include_dir!` into `install_builtin_skills`)
- Delete: `src/skill/assets/builtin/.gitkeep`

**Interfaces:**
- Produces: `include_dir!` embedding of `src/skill/assets/builtin/`; `install_builtin_skills` extracts to `{skill_dir}/.builtin/` with version marker

- [ ] **Step 1: Create adapted skill-creator SKILL.md**

Create `D:\code\rust\cortex-agent\src\skill\assets\builtin\skill-creator\SKILL.md`. Use the content from `D:\code\rust\codex\codex-rs\skills\src\assets\samples\skill-creator\SKILL.md` (416 lines) with these adaptations:
- Frontmatter `description`: replace "extends Codex's capabilities" → "extends cortex-agent's capabilities"
- Body: replace all occurrences of `$CODEX_HOME/skills` and `~/.codex/skills` with `{data_dir}/skills`
- Body: replace `${CODEX_HOME:-$HOME/.codex}/skills` with `{data_dir}/skills`

The exact adapted frontmatter:

```yaml
---
name: skill-creator
description: Guide for creating effective skills. This skill should be used when users want to create a new skill (or update an existing skill) that extends cortex-agent's capabilities with specialized knowledge, workflows, or tool integrations.
metadata:
  short-description: Create or update a skill
---
```

For the body (416 lines), copy verbatim from the Codex source then apply the path substitutions. The key sections that contain path references are "Step 1: Understanding the Skill" (around line 256) and "Step 3: Initializing the Skill" (around lines 292, 305-307).

- [ ] **Step 2: Create simplified init_skill.py**

Create `D:\code\rust\cortex-agent\src\skill\assets\builtin\skill-creator\scripts\init_skill.py`. Base it on the Codex version at `D:\code\rust\codex\codex-rs\skills\src\assets\samples\skill-creator\scripts\init_skill.py`, but **remove all logic that generates `agents/openai.yaml`** (the `--interface` flag, the YAML generation, the `generate_openai_yaml` import). The simplified script should:
- Accept `<skill-name> --path <output-directory> [--resources scripts,references,assets]`
- Create the skill directory at `<output-directory>/<skill-name>/`
- Generate `SKILL.md` with frontmatter template (`name: <skill-name>`, `description: TODO`)
- Optionally create resource subdirs from `--resources`

Read the Codex source to understand the structure, then write the simplified version.

- [ ] **Step 3: Copy quick_validate.py verbatim**

Copy `D:\code\rust\codex\codex-rs\skills\src\assets\samples\skill-creator\scripts\quick_validate.py` verbatim to `D:\code\rust\cortex-agent\src\skill\assets\builtin\skill-creator\scripts\quick_validate.py`. This script validates YAML frontmatter format — it's tool-agnostic.

- [ ] **Step 4: Delete the .gitkeep placeholder**

Delete `D:\code\rust\cortex-agent\src\skill\assets\builtin\.gitkeep` (the builtin skill-creator directory now has real content).

- [ ] **Step 5: Wire include_dir! into catalog.rs**

In `D:\code\rust\cortex-agent\src\skill\catalog.rs`:

1. Add at top of file (after `use` statements):

```rust
use include_dir::include_dir;
use include_dir::Dir;

/// 编译期嵌入的内置 skill 资产。
static BUILTIN_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/skill/assets/builtin");
```

2. Replace the `install_builtin_skills` function body (currently a stub) with:

```rust
fn install_builtin_skills(skill_dir: &Path) -> Result<(), AppError> {
    let builtin_dir = skill_dir.join(BUILTIN_DIR_NAME);
    std::fs::create_dir_all(&builtin_dir).map_err(|e| {
        AppError::FileError(format!(
            "创建内置 skill 目录失败 {}: {e}",
            builtin_dir.display()
        ))
    })?;

    // 版本标记:版本变化时全量重写
    let marker_path = builtin_dir.join(BUILTIN_MARKER);
    let needs_reinstall = match std::fs::read_to_string(&marker_path) {
        Ok(current) => current.trim() != BUILTIN_VERSION,
        Err(_) => true,
    };
    if !needs_reinstall {
        tracing::debug!("[skill] 内置 skill 版本匹配({}),跳过安装", BUILTIN_VERSION);
        return Ok(());
    }

    tracing::info!("[skill] 安装内置 skill 到 {}", builtin_dir.display());
    // 解压嵌入资产到 .builtin/
    extract_dir(&BUILTIN_ASSETS, &builtin_dir)?;

    // 写版本标记
    std::fs::write(&marker_path, BUILTIN_VERSION).map_err(|e| {
        AppError::FileError(format!("写入 builtin 版本标记失败: {e}"))
    })?;
    Ok(())
}

fn extract_dir(dir: &Dir<'_>, target: &Path) -> Result<(), AppError> {
    for entry in dir.entries() {
        let dest = target.join(entry.path());
        match entry {
            include_dir::DirEntry::Dir(d) => {
                std::fs::create_dir_all(&dest).map_err(|e| {
                    AppError::FileError(format!("创建目录失败 {}: {e}", dest.display()))
                })?;
                extract_dir(d, &dest)?;
            }
            include_dir::DirEntry::File(f) => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        AppError::FileError(format!("创建父目录失败: {e}"))
                    })?;
                }
                std::fs::write(&dest, f.contents()).map_err(|e| {
                    AppError::FileError(format!("写入文件失败 {}: {e}", dest.display()))
                })?;
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 6: Add integration test for builtin install**

Append to the `#[cfg(test)] mod tests` block in `catalog.rs`:

```rust
    #[test]
    fn builtin_skill_creator_installed() {
        let dir = tmp_skill_dir("builtin");
        let svc = SkillService::new(dir).unwrap();
        // skill-creator 应被 include_dir 嵌入并解压到 .builtin/
        let meta = svc.find_by_name("skill-creator");
        assert!(meta.is_some(), "skill-creator 未被安装");
        let meta = meta.unwrap();
        assert_eq!(meta.scope, SkillScope::Builtin);
        // 正文应包含关键标题
        let body = svc.read_skill_text("skill-creator").unwrap();
        assert!(body.contains("Skill Creator") || body.contains("skill-creator"));
        // 版本标记文件存在
        assert!(dir.join(".builtin").join(".cortex-builtin-version").exists());
    }

    #[test]
    fn builtin_reinstall_on_version_change() {
        let dir = tmp_skill_dir("reinstall");
        // 首次安装
        let _ = SkillService::new(dir.clone()).unwrap();
        let marker = dir.join(".builtin").join(".cortex-builtin-version");
        assert!(marker.exists());
        // 模拟旧版本(手动改 marker)
        std::fs::write(&marker, "v0-old").unwrap();
        // 再次构建 → 应触发重写
        let _ = SkillService::new(dir.clone()).unwrap();
        let content = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(content.trim(), BUILTIN_VERSION);
    }
```

- [ ] **Step 7: Run tests**

Run: `cargo test --lib skill::catalog`
Expected: PASS (10 tests, including 2 new builtin tests)

- [ ] **Step 8: Commit**

```bash
git add src/skill/assets/ src/skill/catalog.rs
git commit -m "新增: 内置 skill-creator (移植自 Codex) + include_dir 编译期嵌入"
```

---

## Task 8: read_skill tool (skill_read.rs)

**Files:**
- Create: `src/tools/skill_read.rs`
- Modify: `src/tools/mod.rs`

**Interfaces:**
- Consumes: `crate::skill::SkillService` (via `Arc`)
- Produces: `crate::tools::skill_read::create_read_skill_tool(svc: Arc<SkillService>) -> FunctionTool`

- [ ] **Step 1: Create the tool**

Create `D:\code\rust\cortex-agent\src\tools\skill_read.rs`:

```rust
//! `read_skill` 工具 — 让 LLM 主动拉取 skill 正文。
//!
//! 常驻注册在每个 custom agent 上(不受 enabled_tools 白名单约束)。
//! 模型看到 system prompt 里的 skill 目录后,可调用此工具按需拉取未提及 skill 的正文。

use std::sync::Arc;

use adk_rust::tool::FunctionTool;
use adk_rust::{ToolContext, serde_json::{Value, json}};
use schemars::JsonSchema;
use serde::Serialize;

use crate::skill::SkillService;

#[derive(Debug, Serialize, JsonSchema)]
struct ReadSkillParams {
    /// 要读取的 skill 名称(必须是目录中列出的)
    pub name: String,
}

pub fn create_read_skill_tool(svc: Arc<SkillService>) -> FunctionTool {
    FunctionTool::new(
        "read_skill",
        "读取指定 skill 的完整正文。参数 name 必须是目录中列出的 skill 名称。\
         返回 skill 的指令正文(已去掉 frontmatter),按其指示执行任务。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let svc = svc.clone();
            async move {
                let name = args["name"].as_str().unwrap_or("").trim().to_string();
                if name.is_empty() {
                    return Ok(json!({
                        "ok": false,
                        "message": "name 参数不能为空"
                    }));
                }
                match svc.read_skill_text(&name) {
                    Some(text) => Ok(json!({
                        "ok": true,
                        "name": name,
                        "content": text
                    })),
                    None => Ok(json!({
                        "ok": false,
                        "message": format!("skill '{name}' 不存在")
                    })),
                }
            }
        },
    )
}
```

- [ ] **Step 2: Register in tools/mod.rs**

In `D:\code\rust\cortex-agent\src\tools\mod.rs`, add `pub mod skill_read;` alongside the other `pub mod` declarations.

- [ ] **Step 3: Verify build**

Run: `cargo build`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/tools/skill_read.rs src/tools/mod.rs
git commit -m "新增: read_skill 工具 — LLM 主动拉取 skill 正文"
```

---

## Task 9: Wire SkillService into bootstrap + AppState (additive, no removal yet)

**Files:**
- Modify: `src/bootstrap.rs` (add `skill_service` field + init; keep `skill_manager` for now)
- Modify: `src/server/mod.rs` (drop `pub(crate) mod skill;`)

**Note:** This task adds `skill_service` alongside the existing `skill_manager` and deletes `server/skill.rs` module registration. The build will break because `graphql.rs` references `super::skill::*`. Task 10 fixes all downstream references and gets the build green again. **Both tasks are committed together** at the end of Task 10 to avoid a broken intermediate commit.

- [ ] **Step 1: Add skill_service to AppDeps**

In `D:\code\rust\cortex-agent\src\bootstrap.rs`:

1. Add import at top (after `use crate::domain::skill::SkillManager;`):

```rust
use crate::skill::SkillService;
```

2. In the `AppDeps` struct (around line 86-87), add a new field after `skill_manager`:

```rust
    /// 新版文件系统 Skill 服务(Codex 风格,渐进式披露)
    pub skill_service: Option<Arc<SkillService>>,
```

3. In `build_app_deps` (around line 377-399), add after the `skill_manager` block:

```rust
    // ── 新版 Skill 服务(文件系统,Codex 风格)──
    let skill_service = match SkillService::new(cfg.skill_dir()) {
        Ok(svc) => {
            tracing::info!("[infra] Skill 服务初始化成功");
            Some(Arc::new(svc))
        }
        Err(e) => {
            tracing::warn!("[infra] Skill 服务初始化失败({})", e);
            None
        }
    };
```

4. In the `Ok(AppDeps { ... })` return (around line 401-425), add:

```rust
        skill_service,
```

- [ ] **Step 2: Drop server/skill.rs module registration**

In `D:\code\rust\cortex-agent\src\server\mod.rs`, delete line 64:

```rust
pub(crate) mod skill;
```

**Do not commit yet.** Move to Task 10.

---

## Task 10: Rewire injection points + remove old skill_docs/narrow_tools + config changes

**Files:**
- Modify: `src/config/mod.rs` (SkillConfig — drop `tools_mode` + `auto_match`, add `catalog_token_budget_pct`)
- Modify: `src/agent/custom.rs`
- Modify: `src/agent/orchestration.rs`
- Modify: `src/server/sse.rs`
- Modify: `src/server/graphql.rs` (remove 9 skill resolvers + imports)
- Delete: `src/server/skill.rs`

This is the large rewiring task. After it, the build must pass. Committed together with Task 9.

- [ ] **Step 1: Update SkillConfig**

In `D:\code\rust\cortex-agent\src\config\mod.rs`, find `SkillConfig` (line ~422). Replace the struct + Default impl + helper fns with:

```rust
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
    1500
}
fn default_catalog_token_budget_pct() -> u8 {
    2
}
```

- [ ] **Step 1: Rewrite custom.rs injection + remove skill_docs/narrow_tools**

In `D:\code\rust\cortex-agent\src\agent\custom.rs`:

1. **Remove `narrow_tools_by_skills` function** (lines 67-101 entirely). Delete the whole function.

2. **In `AgentContext` struct** (around line 240-259), replace `skill_manager` field with:

```rust
    /// Skill 服务(新版文件系统;DB 不可用时为 None → 无 skill 注入)
    pub skill_service: Option<Arc<crate::skill::SkillService>>,
```

Remove the old `skill_manager: Option<Arc<crate::domain::skill::SkillManager>>` field and its doc comment.

3. **In `AgentRequest` struct** (around line 273-279), remove the `skill_docs` field:

```rust
pub struct AgentRequest {
    pub model_id: Option<String>,
    pub workspace_mode: WorkspaceMode,
    pub browser_toolset: Option<Arc<dyn adk_rust::Toolset>>,
    pub mcp_toolsets: Vec<Arc<dyn adk_rust::Toolset>>,
    // skill_docs 字段已移除 — 改由 skill_service 在构建时注入 catalog + read_skill 工具
}
```

4. **In `build_custom_builder` signature** (line 120-132), remove `skill_docs: Vec<adk_skill::SkillDocument>` param.

5. **In `build_custom_builder` body** (lines 149-159), replace the skill_docs injection loop with catalog injection:

```rust
    // 注入 skill 目录(渐进式披露 L1:元数据常驻 system prompt)
    let mut instruction = assistant.system_prompt.clone();
    if let Some(svc) = skill_service.as_ref() {
        let catalog = svc.render_catalog_block(cfg.skill.catalog_token_budget_pct);
        if !catalog.is_empty() {
            instruction.push_str("\n\n");
            instruction.push_str(&catalog);
        }
    }
    // 额外指令(如 Delegate 模式注入子智能体精确名称清单)
    if let Some(extra) = extra_instruction {
        instruction.push_str("\n\n");
        instruction.push_str(extra);
    }
```

6. **In `build_custom_builder` body** (lines 174-187), replace the `narrow_tools_by_skills` + tool loop with just the tool loop (no narrowing):

```rust
    // 注册助手声明的工具(不再收窄 — skill 不再绑定到 assistant)
    for key in &assistant.enabled_tools {
        builder = push_tool_for_key(
            builder,
            key.as_str(),
            cfg,
            assistant,
            knowledge.clone(),
            catalog.clone(),
            Some(model_store),
        );
    }
```

7. **After tool registration** (after the browser/MCP toolset blocks, before `Ok(builder)`), register `read_skill`:

```rust
    // 注册 read_skill 工具(常驻,让 LLM 主动拉取 skill 正文)
    if let Some(svc) = skill_service.as_ref() {
        builder = builder.tool(Arc::new(
            crate::tools::skill_read::create_read_skill_tool(svc.clone())
        ));
    }
```

8. **Add `skill_service` param to `build_custom_builder`** signature:

```rust
pub fn build_custom_builder(
    cfg: &AppConfig,
    model_store: &ModelProviderStore,
    assistant: &Assistant,
    knowledge: Option<Arc<KnowledgeManager>>,
    catalog: Option<Arc<CatalogCache>>,
    browser_toolset: Option<Arc<dyn adk_rust::Toolset>>,
    mcp_toolsets: Vec<Arc<dyn adk_rust::Toolset>>,
    skill_service: Option<Arc<crate::skill::SkillService>>,
    model_id_override: Option<&str>,
    extra_instruction: Option<&str>,
) -> anyhow::Result<LlmAgentBuilder> {
```

9. **In `build_custom_agent`** (line 210-229), remove `skill_docs` param, add `skill_service` param, pass through.

10. **In `build_agent_for_sub`** (line 305-472), remove `skill_docs` from the destructured `AgentRequest`, and pass `ctx.skill_service.clone()` to every `build_custom_builder` call (there are 4 call sites: lines 350-361, 374-385, 401-412, 439-450).

- [ ] **Step 2: Update orchestration.rs to remove skill resolution**

In `D:\code\rust\cortex-agent\src\agent\orchestration.rs`:

1. Delete lines 167-181 (the `child_skills` block that resolves `enabled_skills`).
2. In `AgentRequest` construction (line 183-189), remove `skill_docs: child_skills`.

- [ ] **Step 3: Update sse.rs injection points**

In `D:\code\rust\cortex-agent\src\server\sse.rs`:

1. **Delete the browser-requires-skill check** (lines 413-428). Replace with a simpler decision:

```rust
    // 判断是否需要浏览器工具集:内置浏览器助手 OR skill 声明需要(新版:扫描 skill 正文里的 browser 关键字)
    // 简化:仅按 agent_type 决定(skill 不再强制浏览器,由模型自主判断)
    let should_use_browser = agent_type == "browser";
```

2. **Delete the `skill_docs` resolution block** (lines 485-494).

3. **After the `workspace_mode` decision block** (after line 542, before `AgentContext` construction at line 545), add `$name` mention resolution:

```rust
    // 解析用户消息中的 $skill-name 提及,注入对应正文
    if let Some(svc) = state.skill_service.as_ref() {
        let blocks = svc.resolve_mentions(&user_text, state.config.skill.max_inject_chars);
        if !blocks.is_empty() {
            user_text.push_str("\n\n");
            user_text.push_str(&blocks.join("\n\n"));
            tracing::info!("[sse] skill 提及注入: {} 个正文块", blocks.len());
        }
    }
```

4. **In `AgentContext` construction** (line 545-559), replace `skill_manager: state.skill_manager.clone()` with:

```rust
        skill_service: state.skill_service.clone(),
```

5. **In `AgentRequest` construction** (line 560-566), remove the `skill_docs` field.

- [ ] **Step 4: Remove GraphQL skill resolvers**

In `D:\code\rust\cortex-agent\src\server\graphql.rs`:

Delete these resolver functions (lines 248-265 for queries, 597-648 for mutations):
- `skills` (line 248-251)
- `skill` (line 253-256)
- `skills_paged` (line 258-266)
- `create_skill` (line 597-605)
- `update_skill` (line 607-615)
- `delete_skill` (line 616-620)
- `duplicate_skill` (line 622-625)
- `batch_set_skill_status` (line 627-641)
- `batch_delete_skills` (line 642-646)
- `reload_skills` (line 647-649)

Also remove any `use` imports of `crate::domain::skill::*` at the top of graphql.rs.

- [ ] **Step 5: Delete src/server/skill.rs**

Delete `D:\code\rust\cortex-agent\src\server\skill.rs` (the GraphQL skill CRUD handler file). It's no longer registered in `mod.rs` (Task 9 Step 2) and no longer referenced by `graphql.rs` (Step 4). Keeping it would cause dead-code warnings.

- [ ] **Step 6: Verify build**

Run: `cargo build`
Expected: PASS (all references now consistent)

If errors remain, grep for `skill_manager`, `skill_docs`, `adk_skill`, `narrow_tools_by_skills`, `crate::domain::skill` and fix any remaining references.

- [ ] **Step 7: Commit (combined Task 9 + Task 10)**

```bash
git add src/config/mod.rs src/bootstrap.rs src/server/mod.rs src/agent/custom.rs src/agent/orchestration.rs src/server/sse.rs src/server/graphql.rs
git add -u src/server/skill.rs
git commit -m "重构: 接线 SkillService 三处注入点, 移除 skill_docs/narrow_tools + 旧 GraphQL skill CRUD"
```

---

## Task 11: Delete old DB skill system + adk-skill dependency

**Files:**
- Delete: `src/domain/skill/` (all 7 files: mod.rs, models.rs, store.rs, manager.rs, materialize.rs, dto.rs, enums.rs)
- Delete: `src/server/skill.rs`
- Delete: `migrations/9.sql`
- Modify: `src/bootstrap.rs` (remove `skill_manager` field + init logic + import)
- Modify: `src/domain/assistant/models.rs` (remove `enabled_skills` field)
- Modify: `src/domain/assistant/store.rs` (remove `enabled_skills` column I/O)
- Modify: `src/server/assistant.rs` (remove `enabled_skills` from DTOs)
- Modify: `Cargo.toml` (remove `adk-skill = "1"`)

- [ ] **Step 1: Remove skill_manager from AppDeps + bootstrap**

In `D:\code\rust\cortex-agent\src\bootstrap.rs`:

1. Remove `use crate::domain::skill::SkillManager;` (line 32).
2. Remove the `skill_manager` field from `AppDeps` (line 86-87).
3. Remove the entire `skill_manager` init block (lines 377-399).
4. Remove `skill_manager,` from the `Ok(AppDeps { ... })` return (line 424).

- [ ] **Step 2: Remove enabled_skills from assistant models**

In `D:\code\rust\cortex-agent\src\domain\assistant\models.rs`:

1. Remove the `pub enabled_skills: Vec<String>,` field from `Assistant` (line 34) and its doc comment (line 33).
2. Remove `pub enabled_skills: Vec<String>,` from `CustomAssistantInput` (line 101) and its doc comment (lines 99-100).
3. Remove `pub enabled_skills: String,` from `AssistantRow` (line 144).
4. In `From<AssistantRow> for Assistant` (lines 167-202): remove `let enabled_skills: Vec<String> = serde_json::from_str(&r.enabled_skills).unwrap_or_default();` (lines 171-172) and `enabled_skills,` from the struct construction (line 189).
5. In `sample_row()` test fixture (line 224): remove `enabled_skills: r#"[]"#.into(),`.

- [ ] **Step 3: Remove enabled_skills from assistant store**

In `D:\code\rust\cortex-agent\src\domain\assistant\store.rs`:

1. Remove the `ALTER TABLE ... ADD COLUMN enabled_skills` block (lines 136-142).
2. Remove the `encode_skills` fn (lines 167-169).
3. In `insert` (lines 175-210): remove `enabled_skills` from the INSERT SQL column list (line 181) and values (line 205-206) — adjust the placeholder count from $23 to $22.
4. In `create_custom` (lines 285-318): remove `enabled_skills: input.enabled_skills.clone(),` (line 306).
5. In `update_custom` (lines 322-361): remove `enabled_skills=$15` → renumber subsequent placeholders; remove the `.bind::<...>(Self::encode_skills(&input.enabled_skills))` line (line 355).
6. In `duplicate_builtin` insert (lines 450-479): remove `enabled_skills` from column list (line 454) and bind (line 478) — adjust placeholder count.
7. Remove the `purge_skill_from_assistants` fn (lines 802-819) — this is in `src/domain/skill/store.rs` which is being deleted, so this is automatic.

**Important**: after removing `enabled_skills` from SQL, recount the `$N` placeholders carefully. The INSERT goes from 23 columns to 22; the UPDATE goes from 17 set clauses to 16.

- [ ] **Step 4: Remove enabled_skills from assistant server DTOs**

In `D:\code\rust\cortex-agent\src\server\assistant.rs`:

1. Remove `pub enabled_skills: Vec<String>,` from `WriteAssistantRequest` (line 50) + doc comment (lines 48-49).
2. Remove `pub enabled_skills: Vec<String>,` from `AssistantDto` (line 137).
3. In `WriteAssistantRequest::to_input` (lines 95-116): remove `enabled_skills: self.enabled_skills.clone(),` (line 110).
4. In `From<Assistant> for AssistantDto` (lines 148-176): remove `enabled_skills: a.enabled_skills,` (line 166).
5. In all test fixtures (lines 710, 734, 758, 790): remove `enabled_skills: vec![],`.

- [ ] **Step 5: Delete old skill domain files**

Delete these files:
- `D:\code\rust\cortex-agent\src\domain\skill\mod.rs`
- `D:\code\rust\cortex-agent\src\domain\skill\models.rs`
- `D:\code\rust\cortex-agent\src\domain\skill\store.rs`
- `D:\code\rust\cortex-agent\src\domain\skill\manager.rs`
- `D:\code\rust\cortex-agent\src\domain\skill\materialize.rs`
- `D:\code\rust\cortex-agent\src\domain\skill\dto.rs`
- `D:\code\rust\cortex-agent\src\domain\skill\enums.rs`
- `D:\code\rust\cortex-agent\src\server\skill.rs`
- `D:\code\rust\cortex-agent\migrations\9.sql`

Then remove the empty `src/domain/skill/` directory.

- [ ] **Step 6: Remove adk-skill dependency**

In `D:\code\rust\cortex-agent\Cargo.toml`, delete the line `adk-skill = "1"` (around line 102).

- [ ] **Step 7: Verify build + tests**

Run: `cargo build`
Expected: PASS (no references to deleted code remain)

Run: `cargo clippy --all-targets -- -D warnings`
Expected: PASS (no warnings)

Run: `cargo test --lib`
Expected: PASS (all tests pass)

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "删除: 旧版 DB skill 系统 (domain/skill + GraphQL CRUD + adk-skill 依赖)"
```

---

## Task 12: Update docs

**Files:**
- Modify: `docs/architecture.md` (§11 roadmap)

- [ ] **Step 1: Add changelog entry**

In `D:\code\rust\cortex-agent\docs\architecture.md`, find §11 (the roadmap table). Add a new row:

```markdown
| 8 | Skill 系统从 DB 迁移到文件系统(Codex 风格) | ✅ v1.3.0 | 渐进式披露:目录进 system prompt + $name 正文注入 + read_skill 工具 |
```

Also mark `docs/design/skill-management.md` as superseded by adding at its top:

```markdown
> **SUPERSEDED**: This design (v1 DB-based skill) has been replaced by the file-system-based skill system in v1.3.0. See `docs/superpowers/specs/2026-07-28-codex-style-skills-design.md` for the current design.
```

- [ ] **Step 2: Commit**

```bash
git add docs/architecture.md docs/design/skill-management.md
git commit -m "文档: 标记 skill 系统迁移完成 (DB → 文件系统)"
```

---

## Self-Review Notes

**Spec coverage check:**
- §3.1 Data model → Task 2 ✓
- §3.2 Loader + frontmatter → Task 3 ✓
- §3.3 Catalog rendering + budget → Task 6 ✓
- §3.4 Mention parser → Task 4 ✓
- §3.5 Body injection → Task 5 ✓
- §3.6 read_skill tool → Task 8 ✓
- §3.7 SkillService → Task 6 ✓
- §3.8 Config → Task 10 ✓
- §3.9 Tool narrowing removal → Task 10 ✓
- §4.1 Catalog injection → Task 10 ✓
- §4.2 $name body injection → Task 10 ✓
- §4.3 read_skill registration → Task 10 ✓
- §5 Deletion list → Task 11 ✓
- §6 Builtin skill-creator → Task 7 ✓
- §7 Error handling → embedded in Tasks 3,6,8 ✓

**Type consistency check:**
- `SkillService` signature consistent across Tasks 6, 8, 9, 10 ✓
- `AgentContext.skill_service: Option<Arc<SkillService>>` consistent ✓
- `render_catalog_block(budget_pct: u8)` consistent ✓
- `resolve_mentions(text, max_chars)` consistent ✓
- `create_read_skill_tool(svc: Arc<SkillService>)` consistent ✓

**Placeholder scan:** None found.
