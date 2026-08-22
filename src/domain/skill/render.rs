//! `SkillService` — 持有 catalog,提供目录渲染 + skill 正文查找。
//!
//! 启动时构建一次 catalog;支持运行时热重载(见 [`reload`](Self::reload),不重新安装内置 skill)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use include_dir::Dir;
use include_dir::include_dir;

use crate::error::AppError;
use crate::domain::skill::loader::{discover_skills, truncate_catalog_skill_description};
use crate::domain::skill::mention::extract_mentions;
use crate::domain::skill::{SkillCatalog, SkillMetadata, SkillScope};

/// 编译期嵌入的内置 skill 资产。
static BUILTIN_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/domain/skill/assets/builtin");

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
    /// - `budget_pct`:目录占上下文窗口的百分比(默认 2)。
    /// - `context_window`:当前模型的上下文窗口(token 数)。0 或异常时回退默认。
    ///
    /// 预算按**真实** context_window 计算（对齐 codex：catalog 预算随模型窗口缩放），
    /// 而非硬编码 128k——否则 32k 模型会被撑爆、1M 模型被无谓截断。
    /// 预算口径对齐 codex `SkillMetadataBudget::Tokens`(context window × pct,单位 token);
    /// 行成本用近似 token 数(≈ bytes/4,对齐 codex `approx_token_count` +
    /// APPROX_BYTES_PER_TOKEN=4)计量,而非字符——字符口径比 token 紧约 4 倍,
    /// 会让 codex 能放下的 catalog 在这里被无谓削掉。
    /// 超预算时缩短 description,再删除末尾 skill 并标注省略数。
    pub fn render_catalog_block(&self, budget_pct: u8, context_window: usize) -> String {
        self.render_catalog_block_filtered(budget_pct, context_window, None)
    }

    /// 渲染 skill 目录到 system prompt 片段（带白名单过滤）。
    ///
    /// - `allowed`：助手级 skill 白名单（skill name）。`None` 或空切片 = 不限制（全量）；
    ///   非空 = 仅列出的 skill 进目录（硬隔离的第一道：模型在目录里看不到被隐藏的 skill）。
    ///
    /// 过滤在预算计算**之前**做：先按白名单收窄集合，再对收窄后集合做预算降级。
    ///
    /// - `budget_pct`:目录占上下文窗口的百分比(默认 2)。
    /// - `context_window`:当前模型的上下文窗口(token 数)。0 或异常时回退默认。
    ///
    /// 预算按**真实** context_window 计算（对齐 codex：catalog 预算随模型窗口缩放），
    /// 而非硬编码 128k——否则 32k 模型会被撑爆、1M 模型被无谓截断。
    /// 预算口径对齐 codex `SkillMetadataBudget::Tokens`(context window × pct,单位 token);
    /// 行成本用近似 token 数(≈ bytes/4,对齐 codex `approx_token_count` +
    /// APPROX_BYTES_PER_TOKEN=4)计量,而非字符——字符口径比 token 紧约 4 倍,
    /// 会让 codex 能放下的 catalog 在这里被无谓削掉。
    /// 超预算时缩短 description,再删除末尾 skill 并标注省略数。
    pub fn render_catalog_block_filtered(
        &self,
        budget_pct: u8,
        context_window: usize,
        allowed: Option<&[String]>,
    ) -> String {
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
        // 白名单过滤：None/空 = 全量；非空 = 仅列出的 skill
        let filtered: Vec<SkillMetadata> = match allowed {
            Some(list) if !list.is_empty() => {
                let allow: std::collections::HashSet<&str> =
                    list.iter().map(|s| s.as_str()).collect();
                cat.skills
                    .iter()
                    .filter(|s| allow.contains(s.name.as_str()))
                    .cloned()
                    .collect()
            }
            _ => cat.skills.clone(),
        };
        if filtered.is_empty() {
            return String::new();
        }
        let cw = if context_window == 0 {
            DEFAULT_CONTEXT_WINDOW
        } else {
            context_window
        };
        // 预算(token)→ 行计费口径:行字节数换算近似 token(向上取整,对齐 codex
        // cost_from_counts:bytes/4 向上取整)。用字节×4 对比预算×4 避免浮点:
        // 等价做法是预算放大 4 倍、行按字节数直接比。
        let budget_tokens = (cw * budget_pct as usize) / 100;
        let budget_bytes = budget_tokens.saturating_mul(4);
        render_catalog_inner(&filtered, budget_bytes)
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
        let mut guard = self
            .catalog
            .write()
            .map_err(|e| AppError::FileError(format!("skill catalog 写锁获取失败: {e}")))?;
        let count = catalog.skills.len();
        *guard = catalog;
        tracing::info!("[skill] catalog 重新加载完成: {count} 个有效 skill");
        Ok(())
    }

    /// 读取 skill 正文全文(含 frontmatter,供 inject 层 strip)。
    /// 不存在或读取失败返回 None。
    pub fn read_skill_raw(&self, name: &str) -> Option<String> {
        let meta = self.find_by_name(name)?;
        match crate::domain::skill::loader::read_skill_file_text(&meta.path) {
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
        Some(crate::domain::skill::loader::strip_frontmatter(&raw))
    }

    /// 读取 skill 正文为带 `<path>` 的统一注入块(对齐 codex body 格式)。
    ///
    /// 与 [`resolve_mentions`] 走同一渲染路径([`render_skill_body_block_with_path`]),
    /// 带 `<path>` 标签 + `{data_dir}` 替换 + 正文相对路径→绝对路径,确保模型无论通过
    /// `$name` 提及还是 `read_skill` 工具拉取,拿到的都是可定位脚本的同款正文块。
    /// 不存在 / 读取失败返回 None。
    pub fn read_skill_block(&self, name: &str, max_chars: usize) -> Option<String> {
        self.read_skill_block_filtered(name, max_chars, None)
    }

    /// [`read_skill_block`] 的白名单过滤版：name 不在 `allowed` 内时按「不存在」返回 None
    /// （硬隔离：模型用 read_skill 工具也拿不到被隐藏的 skill）。
    pub fn read_skill_block_filtered(
        &self,
        name: &str,
        max_chars: usize,
        allowed: Option<&[String]>,
    ) -> Option<String> {
        if !name_allowed(name, allowed) {
            return None;
        }
        let meta = self.find_by_name(name)?;
        let raw = self.read_skill_raw(name)?;
        let skill_dir = canonical_skill_dir(&meta);
        Some(crate::domain::skill::inject::render_skill_body_block_with_path(
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
        self.resolve_mentions_filtered(user_text, max_chars, None)
    }

    /// [`resolve_mentions`] 的白名单过滤版：name 不在 `allowed` 内时按「不存在」跳过
    /// （硬隔离：用户 `$mention` 被隐藏的 skill 也不会注入正文）。
    pub fn resolve_mentions_filtered(
        &self,
        user_text: &str,
        max_chars: usize,
        allowed: Option<&[String]>,
    ) -> Vec<String> {
        let mentions = extract_mentions(user_text);
        let mut blocks = Vec::with_capacity(mentions.len());
        for name in &mentions {
            if !name_allowed(name, allowed) {
                tracing::debug!("[skill] 提及 '${name}' 不在白名单内,跳过");
                continue;
            }
            let Some(meta) = self.find_by_name(name) else {
                tracing::debug!("[skill] 提及 '${name}' 在 catalog 中不存在,跳过");
                continue;
            };
            let Some(raw) = self.read_skill_raw(name) else {
                continue;
            };
            let skill_dir = canonical_skill_dir(&meta);
            let skill_dir_ref = skill_dir.as_deref();
            blocks.push(crate::domain::skill::inject::render_skill_body_block_with_path(
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

/// 白名单判定：`allowed` 为 None 或空切片 = 不限制（全部允许）；非空 = 仅列出的 name 允许。
/// 这是「助手级 skill 硬隔离」的统一判定入口（catalog 渲染用集合过滤，read/mention 用此单点判定）。
fn name_allowed(name: &str, allowed: Option<&[String]>) -> bool {
    match allowed {
        Some(list) if !list.is_empty() => list.iter().any(|s| s == name),
        _ => true,
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

/// 被削减的 desc 渲染时的额外 "..." 后缀成本(削减判定需计入,对齐 codex
/// `DescriptionBudgetLine::extra_costs` 预先算好渲染成本的语义)。
/// desc 为空(codex render_minimum 语义)或未被削减(等于原文)时无后缀。
fn suffix_cost(desc: &str, s: &SkillMetadata) -> usize {
    let full_desc = s
        .short_description
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or(&s.description);
    if !desc.is_empty() && desc.len() < full_desc.len() {
        3 // "..."
    } else {
        0
    }
}

/// 把 lines 渲染成最终字符串(header + 每行 "- {name}: {desc}{tag}\n")。
fn render_catalog_lines(header: &str, lines: &[(String, String, String)]) -> String {
    let mut out = String::from(header);
    for (name, desc, tag) in lines {
        out.push_str(&format!("- {name}: {desc}{tag}\n"));
    }
    out
}

/// 渲染目录块(带预算降级,对齐 codex render.rs)。
///
/// 预算对齐 codex:
/// - 2% 只约束 skill 行(`allocate_skill_lines` 只分配 skill lines),
///   header(intro + 使用说明)固定注入不占预算——否则说明文本超预算会把所有行挤光。
/// - `budget_bytes` 为字节口径的预算(token×4;行按字节数计,等价于 codex 的
///   近似 token 计费 bytes/4 向上取整)。
/// - desc 超预算时允许削到 0(codex `render_minimum` 即空 description,行保留
///   `- name: (tag)` 形式),只有整行最小成本都装不下才整行省略(Omitted)。
///
/// 降级顺序:
/// 1. 全量(预算够)
/// 2. 超预算 → 跨 skill 均摊逐字符削 description(可削到 0,对齐 codex)
/// 3. 仍不够 → 逐个删末尾 skill
fn render_catalog_inner(skills: &[SkillMetadata], budget_bytes: usize) -> String {
    // 复用 prompts::SKILLS_CATALOG_HEADER 常量（消除硬编码重复，与权限层等模板统一管理）
    let header: &str = crate::prompts::SKILLS_CATALOG_HEADER;

    // lines: (name, desc, tag) —— desc 独立可变,供逐字符削减
    let mut lines: Vec<(String, String, String)> = skills
        .iter()
        .map(|s| {
            // desc 先做 1024 上限截断(对齐 codex SkillLine::with_locator 渲染前
            // truncate_catalog_skill_description),再参与预算分配
            let desc = truncate_catalog_skill_description(
                s.short_description
                    .as_deref()
                    .filter(|d| !d.is_empty())
                    .unwrap_or(&s.description),
            );
            let scope_tag = match s.scope {
                SkillScope::Builtin => "内置",
                SkillScope::User => "用户",
            };
            (s.name.clone(), desc, format!(" ({scope_tag})"))
        })
        .collect();

    // 全量判断:skill 行总长 <= 预算(header 不占预算,对齐 codex)
    let available = budget_bytes;
    let curr: usize = lines
        .iter()
        .map(|(n, d, t)| catalog_line_len(n, d, t))
        .sum();
    if curr <= available {
        return render_catalog_lines(header, &lines);
    }

    // 阶段 1:跨 skill 均摊逐字符削 description(每轮所有可削的各削 1,对齐 codex render.rs
    // `allocate_description_chars` round-robin)。允许削到 0(codex `render_minimum`
    // 即空 desc,行仍保留名字)。desc 被削减时渲染为 `{削减后desc}...`(对齐 codex
    // TRUNCATED_SKILL_DESCRIPTION_SUFFIX)——后缀计入行成本参与削减判定
    // (对齐 codex DescriptionBudgetLine::extra_costs 预先算好每前缀渲染成本的语义),
    // 否则补后缀后超预算会误触发阶段 2 删行。
    loop {
        let curr: usize = lines
            .iter()
            .zip(skills.iter())
            .map(|((n, d, t), s)| catalog_line_len(n, d, t) + suffix_cost(d, s))
            .sum();
        if curr <= available {
            break;
        }
        let mut any_trimmed = false;
        for (s, (_, desc, _)) in skills.iter().zip(&mut lines) {
            let full_desc = s
                .short_description
                .as_deref()
                .filter(|d| !d.is_empty())
                .unwrap_or(&s.description);
            if desc.is_empty() || full_desc.is_empty() {
                continue; // 已削空(codex render_minimum 语义)/原本就无 desc
            }
            // 削末尾 1 字符(chars 处理多字节边界),允许削到 0
            let new_len = desc
                .char_indices()
                .nth(desc.chars().count() - 1)
                .map(|(i, _)| i)
                .unwrap_or(0);
            desc.truncate(new_len);
            any_trimmed = true;
        }
        if !any_trimmed {
            break; // 所有 desc 已削空,无法再削 → 进阶段 2
        }
    }

    // 阶段 2:削到极限仍超 → 逐个删末尾 skill(兜底)。
    // 成本仍按"削减后 desc + '...'"口径(suffix_cost),但**不能**先补后缀再算
    // ——补完 desc.len() 已含 "..."、suffix_cost 会再加一次 3(重复计费),
    // 误判超预算而删行。故补后缀放在阶段 2 之后(codex 同序:allocation 定 → render)。
    while !lines.is_empty() {
        let curr: usize = lines
            .iter()
            .zip(skills.iter())
            .map(|((n, d, t), s)| catalog_line_len(n, d, t) + suffix_cost(d, s))
            .sum();
        if curr <= available {
            break;
        }
        lines.pop();
    }

    // 因预算被删的 skill 数（仅阶段 2 会删；阶段 1 只削 description 不删 skill）
    let mut omitted = skills.len().saturating_sub(lines.len());

    // 补 "..." 后缀(阶段 2 之后):被削减(desc < 原文)且未削空的 desc。
    // 放在阶段 2 后避免 suffix_cost 对已含后缀的 desc 重复计费。
    for (s, (_, desc, _)) in skills.iter().zip(&mut lines) {
        let full_desc = s
            .short_description
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or(&s.description);
        if desc.len() < full_desc.len() && !desc.is_empty() {
            desc.push_str("...");
        }
    }

    // 省略提示行(对齐 codex omission_marker 原文,且该行本身占预算:
    // 装不下 marker 时继续 pop 行、omitted 递增——codex render_catalog 同款循环)。
    // desc 削减态的行成本需含 suffix_cost(行索引对齐:lines 是 skills 的前缀)。
    while omitted > 0 {
        let marker = omission_marker(omitted);
        let marker_cost = marker.len() + 1; // + newline
        let used: usize = lines
            .iter()
            .zip(skills.iter())
            .map(|((n, d, t), s)| catalog_line_len(n, d, t) + suffix_cost(d, s))
            .sum();
        if used + marker_cost <= available {
            return render_catalog_lines_with_marker(header, &lines, &marker);
        }
        if lines.pop().is_none() {
            break;
        }
        omitted = omitted.saturating_add(1);
    }

    if lines.is_empty() {
        // 预算太小,连一行都装不下 — 至少让模型知道有 skill，并提示还有多少未列出
        let mut out = String::from(header);
        if omitted > 0 {
            out.push_str(&omission_marker(omitted));
            out.push('\n');
        }
        return out;
    }

    render_catalog_lines(header, &lines)
}

/// 省略提示行(对齐 codex `omission_marker` 原文)。
fn omission_marker(omitted: usize) -> String {
    let skill_word = if omitted == 1 { "skill" } else { "skills" };
    format!("- {omitted} additional {skill_word} omitted from this bounded skills list.")
}

/// 把 lines 渲染成最终字符串(header + 每行 + 省略提示行)。
fn render_catalog_lines_with_marker(
    header: &str,
    lines: &[(String, String, String)],
    marker: &str,
) -> String {
    let mut out = render_catalog_lines(header, lines);
    out.push_str(marker);
    out.push('\n');
    out
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
        let block = svc.render_catalog_block(2, 128_000);
        assert!(block.contains("alpha"));
        assert!(block.contains("Alpha desc"));
        assert!(block.contains("## Available Skills"));
    }

    #[test]
    fn render_catalog_with_only_builtin_renders_it() {
        let dir = tmp_skill_dir("only_builtin");
        let svc = SkillService::new(dir).unwrap();
        let block = svc.render_catalog_block(2, 128_000);
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
        assert!(
            svc.find_by_name("gbk-skill").is_some(),
            "GBK skill 应进 catalog"
        );
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
        assert!(block.contains("<name>alpha</name>"));
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
        // 略超 12 字节迫使削 desc:每行最多被削 4 字符(12/3 行)。
        // 预算只算 skill 行(header 不占预算),full.len() 含 header 需先减去;
        // "..." 后缀(3 字节/行)在削减判定之后追加,故预算余量需覆盖 9 字节后缀开销。
        let header_len = crate::prompts::SKILLS_CATALOG_HEADER.len();
        let budget = full.len() - header_len - 12;
        let block = render_catalog_inner(&skills, budget);
        assert!(
            block.contains("alpha") && block.contains("beta") && block.contains("gamma"),
            "应削 desc 保留全部 skill: {block}"
        );
        // 被削减的 desc 带 "..." 后缀
        assert!(
            block.contains("..."),
            "削减后的 desc 应带 ... 后缀: {block}"
        );
    }

    #[test]
    fn render_catalog_drops_tail_when_trimming_insufficient() {
        // 极小预算:3 行最小成本(削空 desc)总和 57 > 预算 40 → 逐行累加只容 2 行,
        // 第 3 行 Omitted;随后 marker(约 49 字节)挤不下 2 行(38+49>40)→ 继续删,
        // 最终全删 → header-only + 全量省略提示(与 codex render_catalog 的
        // marker-pop 循环一致:marker 装不下就继续 pop 行)。
        let skills = vec![
            fake_meta("alpha", &"A".repeat(50)),
            fake_meta("beta", &"B".repeat(50)),
            fake_meta("gamma", &"G".repeat(50)),
        ];
        let block = render_catalog_inner(&skills, 40);
        assert!(
            block.contains("3 additional skills omitted"),
            "极小预算应全删并标注: {block}"
        );
        assert!(!block.contains("gamma"), "任何 skill 行不应保留: {block}");
    }

    #[test]
    fn render_catalog_marks_omitted_count() {
        // 超预算删末尾后,按 codex omission_marker 原文标注省略数(行本身占预算)。
        // 5 行 desc="desc":full=100 > 76;最小(desc 削空)=80 > 76 → 逐行累加只容 4 行,
        // Omit 1;marker(60)挤不下 4 行(64+60>76)→ pop 到 1 行(16+60=76<=76)。
        // 最终:s0 一行 + "4 additional skills omitted" marker。
        let skills: Vec<SkillMetadata> = (0..5)
            .map(|i| fake_meta(&format!("s{i}"), "desc"))
            .collect();
        // marker(4 omitted,复数)=61 字节;1 行(desc 削空)=16;16+61=77 <= 77 ✓
        let block = render_catalog_inner(&skills, 77);
        assert!(block.contains("s0"), "应保留首个: {block}");
        assert!(
            block.contains("4 additional skills omitted"),
            "应按 codex 原文标注省略数: {block}"
        );
        assert!(!block.contains("s4"), "末尾应被删: {block}");
    }

    #[test]
    fn render_catalog_header_only_when_budget_too_small() {
        // 预算连一行最小都装不下：返回 header + 省略提示（让模型至少知道有 N 个 skill）
        let skills: Vec<SkillMetadata> = (0..3)
            .map(|i| fake_meta(&format!("s{i}"), "desc"))
            .collect();
        let block = render_catalog_inner(&skills, 0); // available = 0
        assert!(block.contains("## Available Skills"));
        assert!(
            block.contains("3 additional skills omitted"),
            "应标注全部省略(codex 原文): {block}"
        );
        // 不应渲染任何完整行（无 "- s0:" 这种）
        assert!(!block.contains("s0:"), "预算不足不应渲染完整行: {block}");
    }

    #[test]
    fn resolve_mentions_injects_bodies() {
        let dir = tmp_skill_dir("resolve");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        let svc = SkillService::new(dir).unwrap();
        let blocks = svc.resolve_mentions("use $alpha please", 1000);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("<name>alpha</name>"));
    }

    #[test]
    fn resolve_mentions_drops_nonexistent() {
        let dir = tmp_skill_dir("resolve_missing");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        let svc = SkillService::new(dir).unwrap();
        let blocks = svc.resolve_mentions("use $alpha and $missing", 1000);
        assert_eq!(blocks.len(), 1); // missing 被丢弃
    }

    // ===== 助手级 skill 白名单（硬隔离）=====

    #[test]
    fn catalog_filtered_none_or_empty_shows_all() {
        let dir = tmp_skill_dir("wl_all");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        write_skill_at(&dir, "", "beta", "Beta desc");
        let svc = SkillService::new(dir).unwrap();
        // None = 全量
        let all = svc.render_catalog_block_filtered(2, 128_000, None);
        assert!(all.contains("alpha") && all.contains("beta"), "None 应全量: {all}");
        // 空切片 = 全量
        let empty = svc.render_catalog_block_filtered(2, 128_000, Some(&[]));
        assert!(empty.contains("alpha") && empty.contains("beta"), "空切片应全量: {empty}");
    }

    #[test]
    fn catalog_filtered_whitelist_narrows() {
        let dir = tmp_skill_dir("wl_narrow");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        write_skill_at(&dir, "", "beta", "Beta desc");
        let svc = SkillService::new(dir).unwrap();
        let allow = vec!["alpha".to_string()];
        let block = svc.render_catalog_block_filtered(2, 128_000, Some(&allow));
        assert!(block.contains("alpha"), "白名单内应可见: {block}");
        assert!(!block.contains("beta"), "白名单外应隐藏: {block}");
    }

    #[test]
    fn read_skill_block_filtered_blocks_non_whitelisted() {
        let dir = tmp_skill_dir("wl_read");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        write_skill_at(&dir, "", "beta", "Beta desc");
        let svc = SkillService::new(dir).unwrap();
        let allow = vec!["alpha".to_string()];
        assert!(
            svc.read_skill_block_filtered("alpha", 1000, Some(&allow)).is_some(),
            "白名单内应可读"
        );
        assert!(
            svc.read_skill_block_filtered("beta", 1000, Some(&allow)).is_none(),
            "白名单外应读不到(硬隔离)"
        );
        // None/空 = 全量可读
        assert!(svc.read_skill_block_filtered("beta", 1000, None).is_some());
        assert!(svc.read_skill_block_filtered("beta", 1000, Some(&[])).is_some());
    }

    #[test]
    fn resolve_mentions_filtered_blocks_non_whitelisted() {
        let dir = tmp_skill_dir("wl_mention");
        write_skill_at(&dir, "", "alpha", "Alpha desc");
        write_skill_at(&dir, "", "beta", "Beta desc");
        let svc = SkillService::new(dir).unwrap();
        let allow = vec!["alpha".to_string()];
        let blocks = svc.resolve_mentions_filtered("use $alpha and $beta", 1000, Some(&allow));
        assert_eq!(blocks.len(), 1, "仅白名单内应注入");
        assert!(blocks[0].contains("<name>alpha</name>"));
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
