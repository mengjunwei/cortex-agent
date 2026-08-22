//! `grep` 工具 — 在工作区内正则搜索文件内容。
//!
//! 设计参考 Zed `crates/agent/src/tools/grep_tool.rs`：
//! - 支持 正则 / 字面量、大小写敏感、上下文行
//! - 返回结构化命中（file/line/line_no/context）
//! - 命中数硬上限（防 token 爆炸）
//!
//! 实现差异：Zed 调系统 `rg` 二进制；我们用纯 Rust `regex` 遍历，
//! 避免部署环境依赖 ripgrep。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_MATCHES: usize = 200;
const MAX_FILE_SIZE_BYTES: u64 = 2 * 1024 * 1024; // 跳过 >2MB 的文件

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// 普通 grep（默认）：全文搜索
    Grep,
    /// 仅搜索符号（函数、类、结构体、方法、接口）
    Symbol,
    /// 智能模式：先搜符号，命中不足再搜全文
    Smart,
}

/// 输出模式（对齐 Claude Code Grep 的 `output_mode`）：
/// 让模型自己控制结果粒度——找文件用 files_with_matches（省 token），
/// 看命中行用 content，只要量级用 count。
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// 逐行命中内容（默认）
    Content,
    /// 仅返回命中文件列表
    FilesWithMatches,
    /// 返回每个文件的命中行数
    Count,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GrepParams {
    /// 搜索模式（正则或字面量）
    pub pattern: String,
    /// 是否作为正则解释（默认 true；false 时按字面量转义）
    #[serde(default)]
    pub is_regex: Option<bool>,
    /// 限定搜索的子目录（相对路径），默认整个工作区
    #[serde(default)]
    pub path: Option<String>,
    /// 上下文行数（前后各 N 行），默认 0
    #[serde(default)]
    pub context: Option<u32>,
    /// 大小写敏感（默认 false）
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// 搜索模式：grep（默认）/ symbol（仅符号）/ smart（智能）
    #[serde(default)]
    pub mode: Option<SearchMode>,
    /// 输出模式：content（默认，逐行命中）/ files_with_matches（仅文件列表）/ count（每文件计数）
    #[serde(default)]
    pub output_mode: Option<OutputMode>,
    /// 最多返回多少条结果（默认 200，硬上限 200）。命中过多时配合 output_mode=files_with_matches 使用
    #[serde(default)]
    pub head_limit: Option<u32>,
    /// 仅搜索匹配此 glob 的文件，如 "*.rs"、"src/**/*.ts"（`*` 不跨目录，`**` 跨任意深度）
    #[serde(default)]
    pub glob: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct Match {
    file: String,
    line_no: u32,
    line: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    context_before: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    context_after: Vec<String>,
}

pub fn create_grep_tool(root_path: Arc<PathBuf>, extra_read_roots: Vec<PathBuf>) -> FunctionTool {
    // 允许根：工作区根 + 额外只读根（skill 目录等），对齐 shell_command 只读可见范围
    let mut roots: Vec<PathBuf> = vec![root_path.as_ref().clone()];
    roots.extend(extra_read_roots);
    let roots = Arc::new(roots);
    FunctionTool::new(
        "grep",
        "Search file contents across the workspace (regex by default). Control result granularity with `output_mode`: `content` (default — matching file/line/content), `files_with_matches` (file list only, cheapest — use when locating files), or `count` (per-file match counts). Use `head_limit` to cap result count (default/hard max 200), `glob` to restrict to matching files (e.g. \"*.rs\", \"src/**/*.ts\"), and `path` to scope to a subdirectory. When results are truncated, narrow the pattern or switch to files_with_matches instead of re-running the same search.",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let roots = roots.clone();
            async move {
                let p: GrepParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("invalid arguments: {e}") })),
                };
                let rel = p.path.as_deref().unwrap_or(".").trim();
                let Some(root) = super::match_safe_root(&roots, rel) else {
                    return Ok(json!({ "ok": false, "error": "path is outside the workspace (and not in any read-only root)" }));
                };
                Ok(grep_impl(root, &p))
            }
        },
    )
    .with_parameters_schema::<GrepParams>()
}

/// 按文件分组（files_with_matches / count 模式的后处理）。返回 (file, 该文件命中数)，
/// 按命中数倒序、同数按文件名排序——输出稳定，便于模型扫描与跨调用比对。
fn group_by_file(matches: &[Match]) -> Vec<(String, usize)> {
    use std::collections::HashMap;
    let mut per_file: HashMap<String, usize> = HashMap::new();
    for m in matches {
        *per_file.entry(m.file.clone()).or_default() += 1;
    }
    let mut entries: Vec<(String, usize)> = per_file.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries
}

/// 把 glob 模式编译为正则（不引 globset 依赖）：
///
/// - `**` → `.*`（跨目录）
/// - `/**/` 或开头的 `**/` → `(?:.*/)?`（匹配**零**或多层目录，对齐 gitignore 语义：
///   `src/**/*.rs` 同时命中 `src/a.rs` 与 `src/x/a.rs`）
/// - `*` → `[^/]*`；`?` → `[^/]`；其余按字面量转义
///
/// 不含 `/` 的模式额外对 basename 匹配（对齐 rg --glob "*.rs" 命中任意深度的 .rs）。
pub(crate) fn compile_glob(pattern: &str) -> Result<regex::Regex, String> {
    let mut re = String::from("(?s)^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                let prev_slash = i == 0 || chars[i - 1] == '/';
                if prev_slash && chars.get(i + 2) == Some(&'/') {
                    if i == 0 {
                        re.push_str("(?:.*/)?");
                    } else {
                        // 前导 '/' 已入 re，替换为可选目录层级
                        re.pop();
                        re.push_str("(?:.*/)?");
                    }
                    i += 2; // 连同末尾的 i += 1，共跳过 `**/` 三个字符
                } else {
                    re.push_str(".*");
                    i += 1;
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| format!("invalid glob pattern '{pattern}': {e}"))
}

/// glob 匹配：先试完整相对路径，再试 basename（仅当模式不含 `/` 时 basename 才可能命中）。
pub(crate) fn glob_matches(re: &regex::Regex, rel_path: &str) -> bool {
    if re.is_match(rel_path) {
        return true;
    }
    let base = rel_path.rsplit('/').next().unwrap_or(rel_path);
    re.is_match(base)
}

/// 符号定义检测（轻量级，不引入 tree-sitter）
/// 仅用 starts_with 前缀匹配常见语言的函数/类/结构体等定义行
/// 注意：不使用 contains，避免注释/字符串中的关键字被误判
fn is_symbol_definition(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Rust
    trimmed.starts_with("fn ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub(crate) fn ")
        || trimmed.starts_with("async fn ")
        || trimmed.starts_with("struct ")
        || trimmed.starts_with("pub struct ")
        || trimmed.starts_with("enum ")
        || trimmed.starts_with("pub enum ")
        || trimmed.starts_with("trait ")
        || trimmed.starts_with("pub trait ")
        || trimmed.starts_with("impl ")
        || trimmed.starts_with("mod ")
        // TypeScript / JS：function/class/interface/type 定义
        || trimmed.starts_with("function ")
        || trimmed.starts_with("export function ")
        || trimmed.starts_with("export default function ")
        || trimmed.starts_with("class ")
        || trimmed.starts_with("export class ")
        || trimmed.starts_with("export default class ")
        || trimmed.starts_with("interface ")
        || trimmed.starts_with("export interface ")
        // JS/TS type alias：仅匹配 `type X =` 形式（排除 `type Foo` 作为类型注解）
        || is_type_alias(trimmed)
        // JS/TS const 函数/类定义：仅匹配 const X = function/class/(...) =>
        || is_const_definition(trimmed)
        // Python
        || trimmed.starts_with("def ")
        || trimmed.starts_with("async def ")
        || trimmed.starts_with("class ")
        // Go
        || trimmed.starts_with("func ")
        // Java / Kotlin / C++（粗粒度，仅 public/private/protected + class/interface 开头）
        || is_java_definition(trimmed)
}

/// 检测 `type Name = ...` 形式的类型别名定义（排除类型注解 `let x: type Foo`）
fn is_type_alias(trimmed: &str) -> bool {
    if !trimmed.starts_with("type ") && !trimmed.starts_with("export type ") {
        return false;
    }
    // 必须含 `=` 才是定义（否则可能是类型注解或引用）
    trimmed.contains('=')
}

/// 检测 JS/TS 的 `const NAME = function/class/(...) =>` 形式
/// 排除普通变量赋值 `const x = 1`
fn is_const_definition(trimmed: &str) -> bool {
    let rest = if let Some(r) = trimmed.strip_prefix("export const ") {
        r
    } else if let Some(r) = trimmed.strip_prefix("const ") {
        r
    } else {
        return false;
    };
    // const NAME = function / const NAME = (...) => / const NAME = class
    // 注意：contains("=>") 已覆盖所有箭头函数（含 `= (x) =>`），
    // 不再需要 `= (` && `=>` 这条被短路的冗余分支
    rest.contains("= function")
        || rest.contains("=>")
        || rest.contains("= class")
        || rest.contains("= async function")
}

/// 检测 Java/Kotlin 的访问修饰符 + class/interface 定义
fn is_java_definition(trimmed: &str) -> bool {
    let after_mod = trimmed
        .strip_prefix("public ")
        .or_else(|| trimmed.strip_prefix("private "))
        .or_else(|| trimmed.strip_prefix("protected "))
        .unwrap_or(trimmed);
    after_mod.starts_with("class ")
        || after_mod.starts_with("interface ")
        || after_mod.starts_with("final class ")
        || after_mod.starts_with("abstract class ")
        || after_mod.starts_with("enum ")
}

pub fn grep_impl(root: &Path, p: &GrepParams) -> Value {
    let rel = p.path.as_deref().unwrap_or(".").trim();
    let search_root = match super::resolve_safe_path(root, rel) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    // 构造正则
    let pattern_str = if p.is_regex.unwrap_or(true) {
        p.pattern.clone()
    } else {
        regex::escape(&p.pattern)
    };
    let re = match Regex::new(&pattern_str) {
        Ok(r) => r,
        Err(e) => return json!({ "ok": false, "error": format!("invalid regex: {e}") }),
    };
    // 大小写敏感：不敏感时套 (?i)
    let re = if p.case_sensitive.unwrap_or(false) {
        re
    } else {
        match Regex::new(&format!("(?i){pattern_str}")) {
            Ok(r) => r,
            Err(e) => return json!({ "ok": false, "error": format!("invalid regex: {e}") }),
        }
    };

    let mode = match &p.mode {
        Some(SearchMode::Symbol) => "symbol",
        Some(SearchMode::Smart) => "smart",
        Some(SearchMode::Grep) | None => "grep",
    };
    let symbol_only = mode == "symbol";
    let smart_mode = mode == "smart";

    // glob 过滤（编译失败早退，不静默忽略——模型需知道过滤条件本身错了）
    let glob_re = match &p.glob {
        Some(g) if !g.trim().is_empty() => match compile_glob(g.trim()) {
            Ok(r) => Some(r),
            Err(e) => return json!({ "ok": false, "error": e }),
        },
        _ => None,
    };

    // 结果上限：head_limit 可调小，硬顶 MAX_MATCHES（防 token 爆炸）
    let limit = p
        .head_limit
        .unwrap_or(MAX_MATCHES as u32)
        .clamp(1, MAX_MATCHES as u32) as usize;

    let output_mode = match &p.output_mode {
        Some(OutputMode::FilesWithMatches) => "files_with_matches",
        Some(OutputMode::Count) => "count",
        Some(OutputMode::Content) | None => "content",
    };

    let context = p.context.unwrap_or(0) as usize;
    let mut matches: Vec<Match> = Vec::new();
    let mut truncated = false;
    let mut files_scanned = 0u64;

    // 执行扫描策略：
    // - symbol: 仅扫符号定义行
    // - smart: 第一轮只扫符号；若命中不足（<10），第二轮补扫全文（追加，不重复）
    // - grep: 全文扫描
    const SMART_FALLBACK_THRESHOLD: usize = 10;
    if symbol_only {
        scan(
            &search_root,
            &re,
            context,
            true,
            limit,
            glob_re.as_ref(),
            &mut matches,
            &mut truncated,
            &mut files_scanned,
            true,
        );
    } else if smart_mode {
        // 第一轮：只搜符号定义
        scan(
            &search_root,
            &re,
            context,
            true,
            limit,
            glob_re.as_ref(),
            &mut matches,
            &mut truncated,
            &mut files_scanned,
            true,
        );
        // 第二轮：符号命中不足才补搜全文（不重复计数 files_scanned）
        if matches.len() < SMART_FALLBACK_THRESHOLD && !truncated {
            // 记录已有的 (file, line_no) 集合，避免重复
            let mut seen: std::collections::HashSet<(String, u32)> = matches
                .iter()
                .map(|m| (m.file.clone(), m.line_no))
                .collect();
            let mut full_matches = Vec::new();
            let mut full_truncated = false;
            scan(
                &search_root,
                &re,
                context,
                false,
                limit,
                glob_re.as_ref(),
                &mut full_matches,
                &mut full_truncated,
                &mut files_scanned,
                false,
            );
            for m in full_matches {
                if matches.len() >= limit {
                    truncated = true;
                    break;
                }
                let key = (m.file.clone(), m.line_no);
                if seen.insert(key) {
                    matches.push(m);
                }
            }
            if full_truncated {
                truncated = true;
            }
        }
    } else {
        scan(
            &search_root,
            &re,
            context,
            false,
            limit,
            glob_re.as_ref(),
            &mut matches,
            &mut truncated,
            &mut files_scanned,
            true,
        );
    }

    // 按 output_mode 组装结果（对齐 Claude Code Grep：粒度由模型选择，不再强制摘要断崖）
    let base = json!({
        "ok": true,
        "pattern": p.pattern,
        "output_mode": output_mode,
        "total_matches": matches.len(),
        "truncated": truncated,
        "files_scanned": files_scanned,
        "mode": mode,
    });
    let mut out = base.as_object().cloned().unwrap_or_default();
    match output_mode {
        "files_with_matches" => {
            let files: Vec<String> = group_by_file(&matches)
                .into_iter()
                .map(|(f, _)| f)
                .collect();
            out.insert("files".into(), json!(files));
        }
        "count" => {
            let counts: Vec<Value> = group_by_file(&matches)
                .into_iter()
                .map(|(file, count)| json!({ "file": file, "count": count }))
                .collect();
            out.insert("counts".into(), json!(counts));
        }
        _ => {
            let matches_json: Vec<Value> = matches.iter().map(|m| json!(m)).collect();
            out.insert("matches".into(), json!(matches_json));
        }
    }
    Value::Object(out)
}

// ===== grep 扫描辅助函数（从 grep_impl 提取的纯函数，模块级私有） =====

/// 对单行尝试匹配；命中返回带上下文的 Match，否则 None。
/// `symbols_only`=true 时仅匹配符号定义行。
fn try_match_line(
    line: &str,
    idx: usize,
    lines: &[&str],
    rel_file: &str,
    re: &Regex,
    context: usize,
    symbols_only: bool,
) -> Option<Match> {
    // 符号模式：仅在符号定义行搜索
    if symbols_only && !is_symbol_definition(line) {
        return None;
    }
    if !re.is_match(line) {
        return None;
    }
    let context_before = if context > 0 {
        let start = idx.saturating_sub(context);
        lines[start..idx].iter().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    };
    let context_after = if context > 0 {
        let end = (idx + 1 + context).min(lines.len());
        lines[idx + 1..end].iter().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    };
    Some(Match {
        file: rel_file.to_string(),
        line_no: idx as u32 + 1,
        line: line.to_string(),
        context_before,
        context_after,
    })
}

/// 扫描单个文件：读取内容，逐行匹配收集结果。
/// 命中数达到 `limit` 时停止并置 truncated=true。
#[allow(clippy::too_many_arguments)]
fn scan_file(
    path: &Path,
    search_root: &Path,
    re: &Regex,
    context: usize,
    symbols_only: bool,
    limit: usize,
    matches: &mut Vec<Match>,
    truncated: &mut bool,
) {
    let rel_file = path
        .strip_prefix(search_root)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if matches.len() >= limit {
            *truncated = true;
            break;
        }
        if let Some(m) = try_match_line(line, i, &lines, &rel_file, re, context, symbols_only) {
            matches.push(m);
        }
    }
}

/// 单轮扫描：遍历文件树，收集匹配行。
/// `symbols_only`=true 时仅匹配符号定义行；`limit` 为本轮命中上限。
/// `count_files`=false 时不累加 files_scanned（用于 smart 第二轮，避免双重计数）。
#[allow(clippy::too_many_arguments)]
fn scan(
    search_root: &Path,
    re: &Regex,
    context: usize,
    symbols_only: bool,
    limit: usize,
    glob_re: Option<&Regex>,
    matches: &mut Vec<Match>,
    truncated: &mut bool,
    files_scanned: &mut u64,
    count_files: bool,
) {
    let mut stack = vec![search_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if matches.len() >= limit {
            *truncated = true;
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if matches.len() >= limit {
                *truncated = true;
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // 跳过隐藏目录 / .git / node_modules / target
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            // 安全：用 symlink_metadata 不跟随符号链接，防止符号链接逃逸工作区
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                if meta.len() > MAX_FILE_SIZE_BYTES {
                    continue;
                }
                // glob 过滤（统一 '/' 分隔，跨平台一致）——放在 files_scanned 计数之前，
                // 该字段只统计真正被读取的文件，否则带 glob 时虚报覆盖面误导模型
                if let Some(g) = glob_re {
                    let rel_file = path
                        .strip_prefix(search_root)
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| path.to_string_lossy().to_string());
                    if !glob_matches(g, &rel_file) {
                        continue;
                    }
                }
                if count_files {
                    *files_scanned += 1;
                }
                scan_file(
                    &path,
                    search_root,
                    re,
                    context,
                    symbols_only,
                    limit,
                    matches,
                    truncated,
                );
            }
        }
    }
}


#[cfg(test)]
#[path = "grep_tests.rs"]
mod tests;
