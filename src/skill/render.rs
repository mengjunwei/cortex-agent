//! `SkillService` — 持有 catalog,提供目录渲染 + skill 正文查找。
//!
//! 启动时构建一次 catalog;支持运行时热重载(见 [`reload`](Self::reload),不重新安装内置 skill)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use include_dir::Dir;
use include_dir::include_dir;

use crate::error::AppError;
use crate::skill::loader::discover_skills;
use crate::skill::mention::extract_mentions;
use crate::skill::{SkillCatalog, SkillMetadata, SkillScope};

/// 编译期嵌入的内置 skill 资产。
static BUILTIN_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/skill/assets/builtin");

/// 内置 skill 版本标记(变更时触发重写 `.builtin/`)。
pub const BUILTIN_VERSION: &str = "v2";

const BUILTIN_DIR_NAME: &str = ".builtin";
const BUILTIN_MARKER: &str = ".cortex-builtin-version";
/// 默认上下文窗口(用于预算计算;真实值应从 model 配置读取,这里给保守默认)
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

/// Skill 服务:持有 catalog,提供目录渲染 + 正文查找。
pub struct SkillService {
    catalog: RwLock<SkillCatalog>,
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

        // 解析为绝对路径(对齐 codex):catalog/body 注入给模型时用绝对路径,避免模型在
        // 沙箱 cwd 里拿到相对路径(如 ./data/skills)而被迫 dir ../../../ 探索。
        // Windows canonicalize 会加 \\?\ 前缀(PowerShell 不认),去掉;失败则降级原路径。
        let skill_dir = match std::fs::canonicalize(&skill_dir) {
            Ok(abs) => {
                let s = abs.to_string_lossy().into_owned();
                let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
                PathBuf::from(stripped)
            }
            Err(_) => skill_dir,
        };

        install_builtin_skills(&skill_dir)?;

        let raw = discover_skills(&skill_dir)?;
        let catalog = build_catalog(raw);

        tracing::info!(
            "[skill] catalog 加载完成: {} 个有效 skill ({} builtin / {} user)",
            catalog.skills.len(),
            catalog
                .skills
                .iter()
                .filter(|s| s.scope == SkillScope::Builtin)
                .count(),
            catalog
                .skills
                .iter()
                .filter(|s| s.scope == SkillScope::User)
                .count(),
        );

        Ok(Self {
            catalog: RwLock::new(catalog),
            skill_dir,
        })
    }

    /// 渲染 skill 目录到 system prompt 片段。
    ///
    /// `budget_pct`:目录占上下文窗口的百分比(默认 2)。
    /// 超预算时缩短 description,再删除末尾 skill。
    pub fn render_catalog_block(&self, budget_pct: u8) -> String {
        let cat = match self.catalog.read() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("[skill] catalog 读锁获取失败: {e}");
                return String::new();
            }
        };
        if cat.is_empty() {
            return String::new();
        }
        let budget_chars = (DEFAULT_CONTEXT_WINDOW * budget_pct as usize) / 100;
        render_catalog_inner(&cat.skills, budget_chars)
    }

    /// 按 name 查找元数据(返回 owned clone——catalog 在 RwLock 后无法返回借用)。
    pub fn find_by_name(&self, name: &str) -> Option<SkillMetadata> {
        self.catalog.read().ok()?.find_by_name(name).cloned()
    }

    /// 返回当前 catalog 的全部 skill(克隆),供管理页展示。
    pub fn list_skills(&self) -> Vec<SkillMetadata> {
        self.catalog
            .read()
            .map(|g| g.skills.clone())
            .unwrap_or_default()
    }

    /// 热重载:重新扫描磁盘并替换内存 catalog。
    ///
    /// 与 [`new`](Self::new) 的区别:不重新安装内置 skill(内置是编译期嵌入,
    /// 启动时已按版本标记装好;reload 只重扫磁盘,`.builtin` 目录会被 discover_skills 正常扫到)。
    /// 失败时保留旧 catalog(先 discover 成功才 write)。
    pub fn reload(&self) -> Result<(), AppError> {
        let raw = discover_skills(&self.skill_dir)?;
        let catalog = build_catalog(raw);
        let mut guard = self.catalog.write().map_err(|e| {
            AppError::FileError(format!("skill catalog 写锁获取失败: {e}"))
        })?;
        let count = catalog.skills.len();
        *guard = catalog;
        tracing::info!("[skill] catalog 重新加载完成: {count} 个有效 skill");
        Ok(())
    }

    /// 读取 skill 正文全文(含 frontmatter,供 inject 层 strip)。
    /// 不存在或读取失败返回 None。
    pub fn read_skill_raw(&self, name: &str) -> Option<String> {
        let meta = self.find_by_name(name)?;
        match crate::skill::loader::read_skill_file_text(&meta.path) {
            Ok((text, fallback)) => {
                if fallback {
                    tracing::info!(
                        "[skill] 读取 {} 非 UTF-8,已回退 GB18030 解码",
                        meta.path.display()
                    );
                }
                Some(text)
            }
            Err(e) => {
                tracing::warn!("[skill] 读取 SKILL.md 失败 {}: {e}", meta.path.display());
                None
            }
        }
    }

    /// 读取 skill 正文(去掉 frontmatter)。不存在返回 None。
    pub fn read_skill_text(&self, name: &str) -> Option<String> {
        let raw = self.read_skill_raw(name)?;
        Some(crate::skill::loader::strip_frontmatter(&raw))
    }

    /// 读取 skill 正文为带 `<path>` 的统一注入块(对齐 codex body 格式)。
    ///
    /// 与 [`resolve_mentions`] 走同一渲染路径([`render_skill_body_block_with_path`]),
    /// 带 `<path>` 标签 + `{data_dir}` 替换 + 正文相对路径→绝对路径,确保模型无论通过
    /// `$name` 提及还是 `read_skill` 工具拉取,拿到的都是可定位脚本的同款正文块。
    /// 不存在 / 读取失败返回 None。
    pub fn read_skill_block(&self, name: &str, max_chars: usize) -> Option<String> {
        let meta = self.find_by_name(name)?;
        let raw = self.read_skill_raw(name)?;
        let skill_dir = canonical_skill_dir(&meta);
        Some(crate::skill::inject::render_skill_body_block_with_path(
            name,
            skill_dir.as_deref(),
            self.skill_dir.parent(),
            &raw,
            max_chars,
        ))
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
            let Some(meta) = self.find_by_name(name) else {
                tracing::debug!("[skill] 提及 '${name}' 在 catalog 中不存在,跳过");
                continue;
            };
            let Some(raw) = self.read_skill_raw(name) else {
                continue;
            };
            let skill_dir = canonical_skill_dir(&meta);
            let skill_dir_ref = skill_dir.as_deref();
            blocks.push(crate::skill::inject::render_skill_body_block_with_path(
                name,
                skill_dir_ref,
                self.skill_dir.parent(),
                &raw,
                max_chars,
            ));
        }
        blocks
    }

    /// 返回 skill 根目录(主要供测试/调试)。
    pub fn skill_dir(&self) -> &Path {
        &self.skill_dir
    }
}

/// 取 skill 目录的规范绝对路径(供 `<path>` 标签 + 正文相对路径替换)。
///
/// `meta.path.parent()` → `canonicalize` → 去 Windows `\\?\` 前缀。canonicalize 失败
/// (如临时目录已删)时返回 None,调用方降级为不传 path。供 `resolve_mentions` 与
/// `read_skill_block` 共用,消除重复。
fn canonical_skill_dir(meta: &SkillMetadata) -> Option<PathBuf> {
    let p = meta.path.parent()?;
    let candidate = std::fs::canonicalize(p).ok().or_else(|| {
        let abs = std::env::current_dir().ok()?.join(p);
        std::fs::canonicalize(&abs).ok()
    })?;
    // Windows canonicalize 加 \\?\ 前缀，PowerShell 不认，去掉
    let cleaned = candidate.to_string_lossy();
    let stripped = cleaned.strip_prefix(r"\\?\").unwrap_or(&cleaned);
    Some(PathBuf::from(stripped))
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

/// 单行渲染长度:"- {name}: {desc}{tag}\n"。
/// 固定部分 "- " + ": " + "\n" = 5 字节(全 ASCII),故 = name + desc + tag + 5。
fn catalog_line_len(name: &str, desc: &str, tag: &str) -> usize {
    name.len() + desc.len() + tag.len() + 5
}

/// 把 lines 渲染成最终字符串(header + 每行 "- {name}: {desc}{tag}\n")。
fn render_catalog_lines(header: &str, lines: &[(String, String, String)]) -> String {
    let mut out = String::from(header);
    for (name, desc, tag) in lines {
        out.push_str(&format!("- {name}: {desc}{tag}\n"));
    }
    out
}

/// 渲染目录块(带预算三级降级,对齐 codex render.rs)。
///
/// 降级顺序:
/// 1. 全量(预算够)
/// 2. 超预算 → 跨 skill 均摊逐字符削 description(保留最小长度,避免削空失去辨识)
/// 3. 仍不够 → 逐个删末尾 skill
fn render_catalog_inner(skills: &[SkillMetadata], budget_chars: usize) -> String {
    // 复用 prompts::SKILLS_CATALOG_HEADER 常量（消除硬编码重复，与权限层等模板统一管理）
    let header: &str = crate::prompts::SKILLS_CATALOG_HEADER;

    // lines: (name, desc, tag) —— desc 独立可变,供逐字符削减
    let mut lines: Vec<(String, String, String)> = skills
        .iter()
        .map(|s| {
            let desc = s
                .short_description
                .as_deref()
                .filter(|d| !d.is_empty())
                .unwrap_or(&s.description)
                .to_string();
            let scope_tag = match s.scope {
                SkillScope::Builtin => "内置",
                SkillScope::User => "用户",
            };
            (s.name.clone(), desc, format!(" ({scope_tag})"))
        })
        .collect();

    let header_len = header.len();
    // 全量判断:header + lines <= 总预算(与下方 available 语义一致,避免漏算 header 误判全量)
    let total_len: usize = header_len
        + lines
            .iter()
            .map(|(n, d, t)| catalog_line_len(n, d, t))
            .sum::<usize>();
    if total_len <= budget_chars {
        return render_catalog_lines(header, &lines);
    }

    let available = budget_chars.saturating_sub(header_len);

    // 阶段 1:跨 skill 均摊逐字符削 description(每轮所有可削的各削 1,对齐 codex render.rs)。
    // 保留最小长度,避免削空让模型失去辨识线索。
    const MIN_DESC_CHARS: usize = 8;
    loop {
        let curr: usize = lines
            .iter()
            .map(|(n, d, t)| catalog_line_len(n, d, t))
            .sum();
        if curr <= available {
            break;
        }
        let mut any_trimmed = false;
        for (_, desc, _) in &mut lines {
            if desc.chars().count() > MIN_DESC_CHARS {
                // 削末尾 1 字符(用 chars 处理多字节边界)
                let chars: Vec<char> = desc.chars().collect();
                *desc = chars[..chars.len() - 1].iter().collect();
                any_trimmed = true;
            }
        }
        if !any_trimmed {
            break; // 所有 desc 已到最小,无法再削 → 进阶段 2
        }
    }

    // 阶段 2:削到极限仍超 → 逐个删末尾 skill(兜底)
    while !lines.is_empty() {
        let curr: usize = lines
            .iter()
            .map(|(n, d, t)| catalog_line_len(n, d, t))
            .sum();
        if curr <= available {
            break;
        }
        lines.pop();
    }

    if lines.is_empty() {
        // 预算太小,连一行都装不下 — 只返回 header(至少让模型知道有 skill)
        return String::from(header);
    }

    render_catalog_lines(header, &lines)
}

/// 安装内置 skill 到 `{skill_dir}/.builtin/`。
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
    // 版本变化时先清空 .builtin/ 再解压(对齐 codex uninstall 语义)——
    // extract_dir 是覆盖式只增不删,新版本删掉的 skill 会残留并被 discover_skills 重新扫进 catalog。
    if builtin_dir.exists() {
        std::fs::remove_dir_all(&builtin_dir).map_err(|e| {
            AppError::FileError(format!(
                "清理内置 skill 目录失败 {}: {e}",
                builtin_dir.display()
            ))
        })?;
    }
    std::fs::create_dir_all(&builtin_dir).map_err(|e| {
        AppError::FileError(format!(
            "重建内置 skill 目录失败 {}: {e}",
            builtin_dir.display()
        ))
    })?;
    // 解压嵌入资产到 .builtin/
    extract_dir(&BUILTIN_ASSETS, &builtin_dir)?;

    // 写版本标记
    std::fs::write(&marker_path, BUILTIN_VERSION)
        .map_err(|e| AppError::FileError(format!("写入 builtin 版本标记失败: {e}")))?;
    Ok(())
}

fn extract_dir(dir: &Dir<'_>, target: &Path) -> Result<(), AppError> {
    // include_dir 的 DirEntry.path() 返回相对 include_dir! 根的【完整】路径(含中间目录),
    // 不是相对当前 Dir。故递归时 target 必须保持根目录,由 entry.path() 拼出正确单层路径;
    // 若递归下钻 target(传 dest),会与 path() 的前缀重复 → 多套一层目录(此前 builtin
    // 解压出 .builtin/skill-creator/skill-creator/ 的根因)。
    for entry in dir.entries() {
        let dest = target.join(entry.path());
        match entry {
            include_dir::DirEntry::Dir(d) => {
                std::fs::create_dir_all(&dest).map_err(|e| {
                    AppError::FileError(format!("创建目录失败 {}: {e}", dest.display()))
                })?;
                extract_dir(d, target)?; // 递归,但 target 保持根
            }
            include_dir::DirEntry::File(f) => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| AppError::FileError(format!("创建父目录失败: {e}")))?;
                }
                std::fs::write(&dest, f.contents()).map_err(|e| {
                    AppError::FileError(format!("写入文件失败 {}: {e}", dest.display()))
                })?;
            }
        }
    }
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
        let content =
            format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nBody text.");
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn new_loads_skills_into_catalog() {
        let dir = tmp_skill_dir("load");
        write_skill_at(&dir, "", "alpha", "Alpha skill");
        write_skill_at(&dir, "", "beta", "Beta skill");
        let svc = SkillService::new(dir).unwrap();
        // 2 用户 skill + 1 内置 skill-creator
        assert_eq!(svc.catalog.read().unwrap().skills.len(), 3);
        assert!(svc.find_by_name("alpha").is_some());
        assert!(svc.find_by_name("beta").is_some());
        assert!(svc.find_by_name("skill-creator").is_some());
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
        assert!(block.contains("## Available Skills"));
    }

    #[test]
    fn render_catalog_with_only_builtin_renders_it() {
        let dir = tmp_skill_dir("only_builtin");
        let svc = SkillService::new(dir).unwrap();
        let block = svc.render_catalog_block(2);
        assert!(block.contains("skill-creator"));
        assert!(block.contains("## Available Skills"));
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
    fn read_skill_text_handles_gbk() {
        // GBK skill 能进 catalog(loader 兜底)后,read_skill_raw 也需兜底,
        // 否则 read_skill_text/read_skill_block/resolve_mentions 全都读不到正文。
        let dir = tmp_skill_dir("read_gbk");
        let skill_dir = dir.join("gbk-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let (gb_bytes, _, _) = encoding_rs::GB18030
            .encode("---\nname: gbk-skill\ndescription: 中文\n---\n\n# 标题\n\n正文内容");
        fs::write(skill_dir.join("SKILL.md"), &*gb_bytes).unwrap();

        let svc = SkillService::new(dir).unwrap();
        assert!(svc.find_by_name("gbk-skill").is_some(), "GBK skill 应进 catalog");
        let body = svc.read_skill_text("gbk-skill").expect("GBK 正文应能读取");
        assert!(body.contains("正文内容"), "GBK 正文应正确解码: {body}");
        assert!(!body.contains("---"), "应去掉 frontmatter");
    }

    #[test]
    fn read_skill_block_contains_path() {
        let dir = tmp_skill_dir("read_block");
        write_skill_at(&dir, "", "alpha", "Alpha skill");
        let svc = SkillService::new(dir).unwrap();
        let block = svc.read_skill_block("alpha", 1000).unwrap();
        assert!(block.contains("<skill name=\"alpha\">"));
        assert!(
            block.contains("<path>"),
            "read_skill_block 应带 <path> 标签"
        );
    }

    fn fake_meta(name: &str, desc: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: desc.to_string(),
            short_description: None,
            path: std::path::PathBuf::from(format!("/x/{name}/SKILL.md")),
            scope: SkillScope::User,
        }
    }

    #[test]
    fn render_catalog_trims_description_before_dropping() {
        // 3 个 skill,description 各 50 字符。略超预算 → 应削 desc 保留全部,而非删 skill
        let skills = vec![
            fake_meta("alpha", &"A".repeat(50)),
            fake_meta("beta", &"B".repeat(50)),
            fake_meta("gamma", &"G".repeat(50)),
        ];
        let full = render_catalog_inner(&skills, usize::MAX);
        let budget = full.len() - 6; // 略超 6 字符,迫使削 desc
        let block = render_catalog_inner(&skills, budget);
        assert!(
            block.contains("alpha") && block.contains("beta") && block.contains("gamma"),
            "应削 desc 保留全部 skill: {block}"
        );
        assert!(
            block.len() <= budget,
            "削后应装进预算: {} > {budget}",
            block.len()
        );
    }

    #[test]
    fn render_catalog_drops_tail_when_trimming_insufficient() {
        // 极小预算:削到最小也装不下 3 个 → 删末尾,仅留首个
        let skills = vec![
            fake_meta("alpha", &"A".repeat(50)),
            fake_meta("beta", &"B".repeat(50)),
            fake_meta("gamma", &"G".repeat(50)),
        ];
        let header_len = crate::prompts::SKILLS_CATALOG_HEADER.len();
        // 1 行最小 ≈ name(5)+desc_min(8)+tag(9)+固定(5) = 27 字节;预算恰够 1 行
        let block = render_catalog_inner(&skills, header_len + 27);
        assert!(block.contains("alpha"), "应至少保留首个: {block}");
        assert!(!block.contains("gamma"), "末尾应被删: {block}");
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

    #[test]
    fn builtin_skill_creator_installed() {
        let dir = tmp_skill_dir("builtin");
        let svc = SkillService::new(dir.clone()).unwrap();
        // skill-creator 应被 include_dir 嵌入并解压到 .builtin/
        let meta = svc.find_by_name("skill-creator");
        assert!(meta.is_some(), "skill-creator 未被安装");
        let meta = meta.unwrap();
        assert_eq!(meta.scope, SkillScope::Builtin);
        // 正文应包含关键标题
        let body = svc.read_skill_text("skill-creator").unwrap();
        assert!(body.contains("Skill Creator") || body.contains("skill-creator"));
        // 版本标记文件存在
        assert!(
            dir.join(".builtin")
                .join(".cortex-builtin-version")
                .exists()
        );
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

    #[test]
    fn builtin_uninstall_removes_stale_skill() {
        let dir = tmp_skill_dir("uninstall");
        // 首次安装(skill-creator)
        let _ = SkillService::new(dir.clone()).unwrap();
        let builtin = dir.join(".builtin");
        // 模拟旧版本遗留的 stale skill(新版本已删除)
        let stale = builtin.join("stale-skill");
        fs::create_dir_all(&stale).unwrap();
        fs::write(
            stale.join("SKILL.md"),
            "---\nname: stale-skill\ndescription: d\n---\n\nbody",
        )
        .unwrap();
        // 改 marker 为旧版本触发重装
        fs::write(builtin.join(BUILTIN_MARKER), "v0-old").unwrap();
        // 重装 → 应清理 stale-skill(对齐 codex uninstall 语义)
        let svc = SkillService::new(dir.clone()).unwrap();
        assert!(
            svc.find_by_name("stale-skill").is_none(),
            "stale skill 应被清理"
        );
        assert!(!stale.exists(), "stale skill 目录应被删除");
        // skill-creator 仍在
        assert!(svc.find_by_name("skill-creator").is_some());
    }
}
