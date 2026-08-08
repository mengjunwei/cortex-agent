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
const SUMMARY_THRESHOLD: usize = 50; // 超过 50 条命中自动生成摘要

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

pub fn create_grep_tool(root_path: Arc<PathBuf>) -> FunctionTool {
    let root = root_path.clone();
    FunctionTool::new(
        "grep",
        "Search file contents across the workspace (regex by default). Returns matching file/line/content. At most 200 matches are returned.",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let root = root.clone();
            async move {
                let p: GrepParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("参数错误: {e}") })),
                };
                Ok(grep_impl(&root, &p))
            }
        },
    )
    .with_parameters_schema::<GrepParams>()
}

/// 按文件分组生成搜索摘要（命中数超过阈值时使用）
fn summarize_matches(matches: &[Match]) -> Vec<Value> {
    use std::collections::HashMap;
    let mut per_file: HashMap<String, Vec<&Match>> = HashMap::new();
    for m in matches {
        per_file.entry(m.file.clone()).or_default().push(m);
    }
    let mut entries: Vec<(String, Vec<&Match>)> = per_file.into_iter().collect();
    // 按命中数倒序（优先展示命中多的文件）
    entries.sort_by_key(|(_, v)| v.len());
    entries.reverse();
    entries
        .into_iter()
        .map(|(file, file_matches)| {
            let lines: Vec<u32> = file_matches.iter().map(|m| m.line_no).collect();
            let preview = if let Some(first) = file_matches.first() {
                first.line.clone()
            } else {
                String::new()
            };
            json!({
                "file": file,
                "count": file_matches.len(),
                "lines": lines,
                "preview": preview,
            })
        })
        .collect()
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
        Err(e) => return json!({ "ok": false, "error": format!("正则编译失败: {e}") }),
    };
    // 大小写敏感：不敏感时套 (?i)
    let re = if p.case_sensitive.unwrap_or(false) {
        re
    } else {
        match Regex::new(&format!("(?i){pattern_str}")) {
            Ok(r) => r,
            Err(e) => return json!({ "ok": false, "error": format!("正则编译失败: {e}") }),
        }
    };

    let mode = match &p.mode {
        Some(SearchMode::Symbol) => "symbol",
        Some(SearchMode::Smart) => "smart",
        Some(SearchMode::Grep) | None => "grep",
    };
    let symbol_only = mode == "symbol";
    let smart_mode = mode == "smart";

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
                &mut full_matches,
                &mut full_truncated,
                &mut files_scanned,
                false,
            );
            for m in full_matches {
                if matches.len() >= MAX_MATCHES {
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
            &mut matches,
            &mut truncated,
            &mut files_scanned,
            true,
        );
    }

    // 命中数超过阈值：生成摘要，减少 token 占用
    let (matches_out, summary) = if matches.len() > SUMMARY_THRESHOLD {
        let summary = summarize_matches(&matches);
        let top_n = summary.iter().take(10).cloned().collect::<Vec<_>>();
        (
            top_n,
            Some(json!({
                "summary_enabled": true,
                "total_files": summary.len(),
                "threshold": SUMMARY_THRESHOLD,
                "note": format!("结果已摘要，共 {} 个文件命中，展示前 10 个。如需更多细节请缩小搜索范围", summary.len()),
            })),
        )
    } else {
        let matches_json: Vec<Value> = matches.iter().map(|m| json!(m)).collect();
        (matches_json, None)
    };

    json!({
        "ok": true,
        "pattern": p.pattern,
        "matches": matches_out,
        "summary": summary,
        "total_matches": matches.len(),
        "truncated": truncated,
        "files_scanned": files_scanned,
        "mode": mode,
    })
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
/// 命中数达到 MAX_MATCHES 时停止并置 truncated=true。
fn scan_file(
    path: &Path,
    search_root: &Path,
    re: &Regex,
    context: usize,
    symbols_only: bool,
    matches: &mut Vec<Match>,
    truncated: &mut bool,
) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    let rel_file = path
        .strip_prefix(search_root)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    for (i, line) in lines.iter().enumerate() {
        if matches.len() >= MAX_MATCHES {
            *truncated = true;
            break;
        }
        if let Some(m) = try_match_line(line, i, &lines, &rel_file, re, context, symbols_only) {
            matches.push(m);
        }
    }
}

/// 单轮扫描：遍历文件树，收集匹配行。
/// `symbols_only`=true 时仅匹配符号定义行。
/// `count_files`=false 时不累加 files_scanned（用于 smart 第二轮，避免双重计数）。
#[allow(clippy::too_many_arguments)]
fn scan(
    search_root: &Path,
    re: &Regex,
    context: usize,
    symbols_only: bool,
    matches: &mut Vec<Match>,
    truncated: &mut bool,
    files_scanned: &mut u64,
    count_files: bool,
) {
    let mut stack = vec![search_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if matches.len() >= MAX_MATCHES {
            *truncated = true;
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if matches.len() >= MAX_MATCHES {
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
                if count_files {
                    *files_scanned += 1;
                }
                scan_file(
                    &path,
                    search_root,
                    re,
                    context,
                    symbols_only,
                    matches,
                    truncated,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    #[test]
    fn finds_literal_match() {
        let ws = TmpWs::new();
        ws.write("a.rs", "fn foo() {}\nfn bar() {}\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "foo".into(),
                is_regex: Some(false),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["ok"], true);
        assert_eq!(r["total_matches"], 1);
        assert_eq!(r["matches"][0]["line_no"], 1);
    }

    #[test]
    fn finds_regex_match() {
        let ws = TmpWs::new();
        ws.write("b.rs", "let x = 123;\nlet y = abc;\nlet z = 456;\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: r"\d+".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["total_matches"], 2);
    }

    #[test]
    fn case_insensitive_by_default() {
        let ws = TmpWs::new();
        ws.write("c.rs", "Hello\nHELLO\nhello\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "hello".into(),
                is_regex: Some(false),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["total_matches"], 3);
    }

    #[test]
    fn case_sensitive_when_requested() {
        let ws = TmpWs::new();
        ws.write("d.rs", "Hello\nHELLO\nhello\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "Hello".into(),
                is_regex: Some(false),
                path: None,
                context: None,
                case_sensitive: Some(true),
                mode: None,
            },
        );
        assert_eq!(r["total_matches"], 1);
    }

    #[test]
    fn includes_context_lines() {
        let ws = TmpWs::new();
        ws.write("e.rs", "l1\nl2\nMATCH\nl4\nl5\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "MATCH".into(),
                is_regex: Some(false),
                path: None,
                context: Some(1),
                case_sensitive: None,
                mode: None,
            },
        );
        let m = &r["matches"][0];
        assert_eq!(m["context_before"][0], "l2");
        assert_eq!(m["context_after"][0], "l4");
    }

    #[test]
    fn skips_git_and_node_modules() {
        let ws = TmpWs::new();
        ws.write(".git/config", "secret_in_git\n");
        ws.write("node_modules/pkg.js", "secret_in_nm\n");
        ws.write("src/main.rs", "real_secret\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "secret".into(),
                is_regex: Some(false),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["total_matches"], 1);
        // 平台无关：归一化路径分隔符后比较
        let file = r["matches"][0]["file"].as_str().unwrap().replace('\\', "/");
        assert_eq!(file, "src/main.rs");
    }

    #[test]
    fn scoped_to_subdirectory() {
        let ws = TmpWs::new();
        ws.write("a.rs", "target_word\n");
        ws.write("sub/b.rs", "target_word\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "target_word".into(),
                is_regex: Some(false),
                path: Some("sub".into()),
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["total_matches"], 1);
        let file = r["matches"][0]["file"].as_str().unwrap().replace('\\', "/");
        assert_eq!(file, "b.rs");
    }

    #[test]
    fn rejects_search_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "x".into(),
                is_regex: Some(false),
                path: Some("../".into()),
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["ok"], false);
    }

    #[test]
    #[cfg(unix)]
    fn skips_symlink_files_to_prevent_escape() {
        use std::os::unix::fs::symlink;
        let ws = TmpWs::new();
        // 创建一个指向 /etc/passwd 的符号链接（工作区外敏感文件）
        symlink("/etc/passwd", ws.root.join("evil_link")).ok();
        ws.write("real.rs", "secret_pattern\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "secret_pattern".into(),
                is_regex: Some(false),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        // 应只命中 real.rs，且不读 evil_link 的内容
        assert_eq!(r["total_matches"], 1);
        let file = r["matches"][0]["file"].as_str().unwrap().replace('\\', "/");
        assert_eq!(file, "real.rs");
    }

    #[test]
    #[cfg(unix)]
    fn skips_symlink_dirs_to_prevent_cycle() {
        use std::os::unix::fs::symlink;
        let ws = TmpWs::new();
        // 创建指向祖先的符号链接（形成循环）
        symlink(&ws.root, ws.root.join("loop")).ok();
        ws.write("normal.rs", "findme\n");
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "findme".into(),
                is_regex: Some(false),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        // 应正常结束，只命中一次（不会因循环无限扫描）
        assert_eq!(r["total_matches"], 1);
    }

    #[test]
    fn symbol_mode_finds_only_symbol_definitions() {
        let ws = TmpWs::new();
        ws.write(
            "lib.rs",
            "// 注释行，不应命中\nfn foo() {} // 符号行应命中\nlet x = 1; // 普通行\nstruct Bar; // 符号行应命中\n",
        );
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "foo|Bar".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: Some(SearchMode::Symbol),
            },
        );
        assert_eq!(r["ok"], true);
        assert_eq!(r["mode"], "symbol");
        assert_eq!(r["total_matches"], 2);
    }

    #[test]
    fn summary_generated_when_matches_exceed_threshold() {
        let ws = TmpWs::new();
        // 生成超过 SUMMARY_THRESHOLD (50) 条命中
        let content = (0..60)
            .map(|i| format!("target_{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        ws.write("many_hits.rs", &content);
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "target_".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["ok"], true);
        assert!(r["summary"].is_object());
        assert_eq!(r["summary"]["summary_enabled"], true);
        // matches 只展示前 10 个文件的摘要（实际只有 1 个文件）
        assert!(r["matches"][0]["count"].is_number());
    }

    #[test]
    fn no_summary_when_matches_below_threshold() {
        let ws = TmpWs::new();
        let content = (0..30)
            .map(|i| format!("target_{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        ws.write("few_hits.rs", &content);
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "target_".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: None,
            },
        );
        assert_eq!(r["ok"], true);
        assert_eq!(r["summary"], Value::Null);
        assert_eq!(r["total_matches"], 30);
        assert!(r["matches"].is_array());
    }

    #[test]
    fn symbol_mode_does_not_match_comments() {
        let ws = TmpWs::new();
        // 用独特文件名 + 独特符号名，避免与 TmpWs 默认创建的 main.rs/lib.rs 命中冲突
        ws.write(
            "sym_test.rs",
            "// this is my class definition\n// call my fn here\nfn real_fn() {}\n",
        );
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "real_fn".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: Some(SearchMode::Symbol),
            },
        );
        // 只应命中 real_fn 定义行；注释里的 class/fn 不应命中
        assert_eq!(r["total_matches"], 1);
    }

    #[test]
    fn smart_mode_falls_back_to_full_when_symbols_insufficient() {
        let ws = TmpWs::new();
        ws.write(
            "code.rs",
            "// hello in comment\nlet hello = 1;\n// hello again\nlet x = hello;\n// more hello\n",
        );
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "hello".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: Some(SearchMode::Smart),
            },
        );
        // 没有符号定义命中（<10），应回退全文扫描，命中普通行
        assert!(r["total_matches"].as_u64().unwrap() >= 3);
    }

    #[test]
    fn smart_mode_does_not_duplicate_symbol_hits() {
        let ws = TmpWs::new();
        // 一个符号定义行 + 多个普通行
        ws.write(
            "sym_dedup.rs",
            "fn hello() {}\nlet a = hello;\nlet b = hello;\nlet c = hello;\n",
        );
        let root = ws.canon();
        let r = grep_impl(
            &root,
            &GrepParams {
                pattern: "hello".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: Some(SearchMode::Smart),
            },
        );
        // 符号命中不足 10 → 回退全文；hello 定义行不应被重复计数
        assert_eq!(r["total_matches"], 4);
    }

    #[test]
    fn smart_mode_does_not_double_count_files() {
        let ws = TmpWs::new();
        // 多个文件，让 smart 模式触发第二轮全文扫描
        for i in 0..5 {
            ws.write(
                &format!("file{}.rs", i),
                "// comment about hello\nlet hello = 1;\n",
            );
        }
        let root = ws.canon();
        // grep 模式（单轮扫描）的 files_scanned 作为基准
        let r_grep = grep_impl(
            &root,
            &GrepParams {
                pattern: "hello".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: Some(SearchMode::Grep),
            },
        );
        // smart 模式（两轮扫描）的 files_scanned 应与 grep 一致，不被双计
        let r_smart = grep_impl(
            &root,
            &GrepParams {
                pattern: "hello".into(),
                is_regex: Some(true),
                path: None,
                context: None,
                case_sensitive: None,
                mode: Some(SearchMode::Smart),
            },
        );
        assert_eq!(
            r_grep["files_scanned"], r_smart["files_scanned"],
            "smart 模式两轮扫描的 files_scanned 不应被双重计数"
        );
    }
}
