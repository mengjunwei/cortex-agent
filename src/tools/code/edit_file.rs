//! `edit_file` 工具 — 替换工作区内文件内容（含 diff 生成）。
//!
//! 设计参考 Zed `crates/agent/src/tools/edit_file_tool.rs`：
//! - `old_text` 必须在文件中**唯一匹配**；多次出现则报错（要求更具体上下文）
//! - 原子写入：先写 `.cortex-tmp`，再 rename（防中途崩溃损坏原文件）
//! - 返回 unified diff（`diff` crate 生成），前端据此渲染 diff 视图
//! - occurrence 参数：当同一文本需多次替换时，指定替换第几次出现（默认 1）

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct EditFileParams {
    /// 相对工作区根目录的文件路径
    pub path: String,
    /// 要替换的旧文本（必须唯一匹配；多次出现请加更多上下文）
    pub old_text: String,
    /// 替换为的新文本
    pub new_text: String,
    /// 替换第几次出现（默认 1；用于同一文本多次出现时精确指定）
    #[serde(default)]
    pub occurrence: Option<u32>,
}

pub fn create_edit_file_tool(root_path: Arc<PathBuf>) -> FunctionTool {
    let root = root_path.clone();
    FunctionTool::new(
        "edit_file",
        "Replace text in a workspace file. `old_text` must uniquely match text in the file (multiple matches error — include more surrounding context). Returns a unified diff of the change. The workspace is the only writable area — make all file changes here, and never modify read-only dependencies (skills, system files).",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let root = root.clone();
            async move {
                let p: EditFileParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(json!({ "ok": false, "error": format!("invalid arguments: {e}") }))
                    }
                };
                Ok(edit_file_impl(&root, &p).await)
            }
        },
    )
    .with_parameters_schema::<EditFileParams>()
}

pub async fn edit_file_impl(root: &Path, p: &EditFileParams) -> Value {
    if p.old_text == p.new_text {
        return json!({
            "ok": false,
            "error": "old_text is identical to new_text — nothing to replace"
        });
    }
    if p.old_text.is_empty() {
        return json!({ "ok": false, "error": "old_text must not be empty" });
    }

    let abs = match super::resolve_safe_write_path(root, &p.path) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    // 同文件写锁：read→替换→write 区间与其他写入者（并行子 agent 的
    // edit_file/create_file append）互斥（锁表见 create_file.rs FILE_WRITE_LOCKS）。
    let lock = super::create_file::path_write_lock(&abs);
    let _guard = lock.lock().await;

    let original = match tokio::fs::read_to_string(&abs).await {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("failed to read file: {e}") }),
    };

    // 查找 old_text 所有出现（字节范围），渐进宽松匹配：精确 → Unicode 标点归一化 → 引号转义归一化。
    // 对齐 codex apply-patch `seek_sequence` 的多 pass 思路（简化为子串级）——
    // 模型常从渲染后的文档里复制带弯引号/破折号/特殊空格的文本，字节级精确匹配会硬失败，
    // 浪费 2-4 轮重读重试。
    let occurrences = fuzzy_find_occurrences(&original, &p.old_text);
    if occurrences.is_empty() {
        // 近似命中诊断：找到与 old_text 编辑距离最小的行，指出第一个差异字符——
        // 让模型一轮内自我纠正（转义/引号/空白类错误），而不是盲目重读重试。
        let hint = closest_mismatch_hint(&original, &p.old_text);
        return json!({
            "ok": false,
            "error": format!(
                "old_text not found in file (tried in order: exact match, ignoring Unicode punctuation differences, ignoring backslashes escaping quotes).{hint} Make sure old_text matches the file exactly, including indentation, line endings and escaping."
            ),
            "path": p.path,
        });
    }

    let occ_idx = p.occurrence.unwrap_or(1) as usize;
    if occ_idx > occurrences.len() {
        return json!({
            "ok": false,
            "error": format!(
                "occurrence={} exceeds the actual number of matches ({})",
                occ_idx,
                occurrences.len()
            ),
        });
    }
    if occurrences.len() > 1 && p.occurrence.is_none() {
        return json!({
            "ok": false,
            "error": format!(
                "old_text matches {} locations in the file and is not unique. Extend old_text with more surrounding context to make it unique, or pass the `occurrence` parameter to select which match to replace.",
                occurrences.len()
            ),
            "occurrences": occurrences.len(),
        });
    }

    let (start, end) = occurrences[occ_idx - 1];
    let mut updated = String::with_capacity(original.len() + p.new_text.len());
    updated.push_str(&original[..start]);
    updated.push_str(&p.new_text);
    updated.push_str(&original[end..]);

    // 原子写入
    if let Err(e) = atomic_write(&abs, &updated).await {
        return json!({ "ok": false, "error": format!("failed to write file: {e}") });
    }

    // 生成 unified diff
    let diff_text = super::diff::make_unified_diff(&original, &updated, &p.path);

    json!({
        "ok": true,
        "path": p.path,
        "applied": true,
        "occurrence_replaced": occ_idx,
        "diff": diff_text,
    })
}

async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("cortex-tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// 在 `content` 中查找 `needle` 的所有出现，返回字节范围 `(start, end)`。
///
/// 入口先走完整多 pass；全部未命中且 `needle` 带 read_file 的行号前缀（`12→content`，
/// 模型直接复制渲染输出的高频错误）时，剥掉前缀重试一轮。
fn fuzzy_find_occurrences(content: &str, needle: &str) -> Vec<(usize, usize)> {
    let found = fuzzy_find_occurrences_inner(content, needle);
    if found.is_empty() {
        if let Some(stripped) = strip_read_prefixes(needle) {
            return fuzzy_find_occurrences_inner(content, &stripped);
        }
    }
    found
}

/// 若 `s` 的行带 read_file 输出的行号前缀（`{n}→`，允许行首空白），返回剥掉前缀的版本；
/// 没有任何一行匹配则返回 None。对齐 Claude Code Edit 的
/// "Strip the Read line prefix (line number + tab) before matching" 容错。
fn strip_read_prefixes(s: &str) -> Option<String> {
    let mut any_stripped = false;
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        let digits_end = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        if digits_end > 0 && trimmed[digits_end..].starts_with('→') {
            any_stripped = true;
            out.push_str(&trimmed[digits_end + '→'.len_utf8()..]);
        } else {
            out.push_str(line);
        }
    }
    any_stripped.then_some(out)
}

/// 多 pass 匹配主体（精确 → Unicode 标点归一化 → 引号转义归一化）。
///
/// 渐进宽松（对齐 codex apply-patch `seek_sequence`，简化为子串级）：
/// 1. **精确**字节匹配——优先，行为与旧实现一致。
/// 2. **Unicode 标点归一化**——把弯引号/破折号/特殊空格等排版字符映射成 ASCII
///    等价物后再匹配。归一化是字符一一映射（每个码点 → 恰好一个字符），
///    故字符偏移在原文与归一化文本间严格可逆，可精确换算回原文字节范围。
/// 3. **引号转义归一化**——在 pass 2 基础上把 `\"` 折叠成 `"`（`\'` → `'` 同理）。
///    模型在 JSON 参数里把 `\"` 双重转义成 `\\\"`（解码后多出反斜杠）是高频错误，
///    文件里是裸引号时 pass 1/2 都会硬失败。反斜杠会改变字符数，此 pass 不保
///    1:1 映射，故换算回原文范围后需按原文字节做合法性防御。
///
/// 仅当上一 pass 无任何命中时才尝试下一 pass（避免精确命中被模糊覆盖计数）。
fn fuzzy_find_occurrences_inner(content: &str, needle: &str) -> Vec<(usize, usize)> {
    // pass 1：精确
    let exact: Vec<(usize, usize)> = content
        .match_indices(needle)
        .map(|(i, _)| (i, i + needle.len()))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    // pass 2：Unicode 标点归一化
    let norm_content = normalize_punctuation(content);
    let norm_needle = normalize_punctuation(needle);
    if !norm_needle.is_empty() {
        let mut out = Vec::new();
        for (norm_byte, _) in norm_content.match_indices(&norm_needle) {
            // 归一化文本里的字节偏移 → 字符偏移（一一映射，与原文字符数一致）
            let char_start = norm_content[..norm_byte].chars().count();
            let char_len = norm_needle.chars().count();
            let start = char_to_byte(content, char_start);
            let end = char_to_byte(content, char_start + char_len);
            // 防御：归一化保 1:1 字符映射，正常 start<end；异常则跳过该候选
            if start < end {
                out.push((start, end));
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    // pass 3：引号前转义反斜杠归一化——原文中的 `\"` 折叠成 `"` 后与 old_text 匹配。
    // 折叠会改变字符数（2→1），折叠文本的偏移无法换算回原文，故直接对原文做贪心扫描。
    let folded_needle = fold_escaped_quotes(&norm_needle);
    if folded_needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let needle_chars: Vec<char> = folded_needle.chars().collect();
    let content_char_count = content.chars().count();
    for (char_idx, (byte_idx, c)) in content.char_indices().enumerate() {
        // 快速剪枝：首字符归一化后须相等，或原文以 `\`+引号 开头（可折叠成首字符）
        if c != '\\' && normalize_char(c) != needle_chars[0] {
            continue;
        }
        if content_char_count - char_idx < needle_chars.len() {
            break; // 剩余字符数不足，剪枝
        }
        if let Some(end) = match_folded_at(content, byte_idx, &needle_chars) {
            out.push((byte_idx, end));
        }
    }
    out
}

/// 从 `content` 的 `start` 字节处开始，尝试把原文片段「标点归一化 + 折叠转义引号」后
/// 与 `needle`（已折叠）对齐。贪心策略：原文出现 `\`+引号 且 needle 期望该引号时，
/// 优先消费两个原文码点（替换范围覆盖完整转义对）。成功返回结束字节位置。
fn match_folded_at(content: &str, start: usize, needle: &[char]) -> Option<usize> {
    let chars: Vec<(usize, char)> = content[start..].char_indices().collect();
    let mut ni = 0usize;
    let mut ci = 0usize;
    while ci < chars.len() {
        let Some(&e) = needle.get(ni) else {
            // needle 已耗尽：匹配完成，结束于当前字符之前
            return Some(start + chars[ci].0);
        };
        let (_rel, c) = chars[ci];
        if c == '\\' {
            match chars.get(ci + 1).map(|&(_, q)| q) {
                // 原文是 `\` + 引号
                Some(q @ ('"' | '\'')) => {
                    if q == e {
                        // needle 期望引号：消费完整转义对（贪心，替换范围含反斜杠）
                        ci += 2;
                        ni += 1;
                    } else if e == '\\' {
                        // needle 期望字面反斜杠：一对一消费
                        ci += 1;
                        ni += 1;
                    } else {
                        return None;
                    }
                }
                _ => {
                    if e == '\\' || e == normalize_char(c) {
                        ci += 1;
                        ni += 1;
                    } else {
                        return None;
                    }
                }
            }
        } else if e == normalize_char(c) {
            ci += 1;
            ni += 1;
        } else {
            return None;
        }
    }
    // 原文耗尽：仅当 needle 同时耗尽才算命中（结尾对齐）
    (ni == needle.len()).then_some(content.len())
}

/// 把 `\"` 折叠成 `"`、`\'` 折叠成 `'`（连续反斜杠+引号只折叠最后一对）。
fn fold_escaped_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('"') | Some('\'') => {
                    out.push(*chars.peek().unwrap());
                    chars.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 未命中时的诊断提示：取 old_text 最长的一行作为探针，在文件里找编辑距离最小的行，
/// 指出第一个差异字符（期望 vs 实际，含码点）。距离太远则认为无意义，返回空串。
/// 目的：让模型一轮内定位「转义多了/引号形制不同/空白差异」这类错误，而非盲目重读。
fn closest_mismatch_hint(content: &str, old_text: &str) -> String {
    // 探针行去掉首尾空白后再比较——缩进差异已在逐字符对比中单独暴露
    let probe = old_text
        .lines()
        .map(str::trim)
        .max_by_key(|l| l.chars().count())
        .unwrap_or("");
    let probe_chars: Vec<char> = probe.chars().collect();
    if probe_chars.is_empty() {
        return String::new();
    }

    let mut best: Option<(usize, usize, &str)> = None; // (distance, line_no, line)
    for (i, line) in content.lines().enumerate() {
        let d = char_levenshtein(&probe_chars, line.trim());
        let better = match best {
            None => true,
            Some((bd, _, _)) => d < bd,
        };
        if better {
            best = Some((d, i + 1, line));
        }
    }
    let Some((dist, line_no, line)) = best else {
        return String::new();
    };
    // 距离超过探针长度 1/3 视为无关行，不给误导性提示
    if dist == 0 || dist > (probe_chars.len() / 3).max(2) {
        return String::new();
    }

    let line_chars: Vec<char> = line.trim().chars().collect();
    let mut idx = 0usize;
    while idx < probe_chars.len() && idx < line_chars.len() {
        let a = normalize_char(probe_chars[idx]);
        let b = normalize_char(line_chars[idx]);
        if a != b {
            break;
        }
        idx += 1;
    }
    let exp = probe_chars.get(idx);
    let act = line_chars.get(idx);
    format!(
        " Closest candidate is line {line_no} ({dist} chars apart). First difference at char {}: old_text has {}, file has {}.",
        idx + 1,
        fmt_char(exp),
        fmt_char(act),
    )
}

/// 字符级 Levenshtein 距离（`b` 逐行调用，行级规模，无需更复杂实现）。
fn char_levenshtein(a: &[char], b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 诊断用字符格式化：`'\\' (U+005C)`；缺失时 `end-of-text`。
fn fmt_char(c: Option<&char>) -> String {
    match c {
        Some(c) => format!("'{}' (U+{:04X})", c.escape_debug(), *c as u32),
        None => "end-of-text".to_string(),
    }
}

/// Unicode 排版字符 → ASCII 等价物（移植自 codex apply-patch `seek_sequence::normalise`，
/// 去掉行级 trim——子串级不需要）。映射是码点一一对应，保证字符偏移可逆。
fn normalize_punctuation(s: &str) -> String {
    s.chars().map(normalize_char).collect()
}

/// [`normalize_punctuation`] 的单字符版本。
fn normalize_char(c: char) -> char {
    match c {
        // 各类连字符/破折号/减号 → '-'
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        // 弯单引号 → '\''
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        // 弯双引号 → '"'
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        // 不间断空格及各类排版空格 → 普通空格
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
        | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
        | '\u{3000}' => ' ',
        other => other,
    }
}

/// 字符索引 → 字节索引（在 `s` 中）。越界返回 `s.len()`。
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    #[tokio::test]
    async fn replaces_unique_match() {
        let ws = TmpWs::new();
        ws.write("e.rs", "fn foo() {\n    let x = 1;\n}\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "e.rs".into(),
                old_text: "let x = 1;".into(),
                new_text: "let x = 2;".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["applied"], true);
        // 验证文件确实被改了
        let content = std::fs::read_to_string(root.join("e.rs")).unwrap();
        assert!(content.contains("let x = 2;"));
        assert!(!content.contains("let x = 1;"));
        // diff 应包含 + 和 -
        let diff = r["diff"].as_str().unwrap();
        assert!(diff.contains("-    let x = 1;"));
        assert!(diff.contains("+    let x = 2;"));
    }

    #[tokio::test]
    async fn rejects_multiple_matches_without_occurrence() {
        let ws = TmpWs::new();
        ws.write("m.rs", "todo\ntodo\ntodo\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "m.rs".into(),
                old_text: "todo".into(),
                new_text: "done".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("3 locations"));
        // 文件应未被修改
        let content = std::fs::read_to_string(root.join("m.rs")).unwrap();
        assert_eq!(content.matches("todo").count(), 3);
    }

    #[tokio::test]
    async fn replaces_specific_occurrence() {
        let ws = TmpWs::new();
        ws.write("o.rs", "todo\ntodo\ntodo\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "o.rs".into(),
                old_text: "todo".into(),
                new_text: "done".into(),
                occurrence: Some(2),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = std::fs::read_to_string(root.join("o.rs")).unwrap();
        assert_eq!(content.matches("todo").count(), 2);
        assert_eq!(content.matches("done").count(), 1);
    }

    #[tokio::test]
    async fn rejects_no_match() {
        let ws = TmpWs::new();
        ws.write("n.rs", "fn foo() {}\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "n.rs".into(),
                old_text: "nonexistent".into(),
                new_text: "whatever".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn rejects_identical_text() {
        let ws = TmpWs::new();
        ws.write("same.rs", "same\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "same.rs".into(),
                old_text: "same".into(),
                new_text: "same".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn rejects_empty_old_text() {
        let ws = TmpWs::new();
        ws.write("empty.rs", "x\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "empty.rs".into(),
                old_text: "".into(),
                new_text: "y".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "../../../etc/passwd".into(),
                old_text: "x".into(),
                new_text: "y".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn occurrence_out_of_range_fails() {
        let ws = TmpWs::new();
        ws.write("or.rs", "dup\ndup\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "or.rs".into(),
                old_text: "dup".into(),
                new_text: "unique".into(),
                occurrence: Some(99),
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[test]
    fn unified_diff_format() {
        let d = crate::tools::code::diff::make_unified_diff("a\nb\nc\n", "a\nB\nc\n", "f.rs");
        assert!(d.starts_with("--- a/f.rs"));
        assert!(d.contains("+++ b/f.rs"));
        assert!(d.contains("@@"));
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
    }

    #[tokio::test]
    async fn fuzzy_matches_unicode_punctuation() {
        // 文件里是 en-dash（U+2013），模型 old_text 误用 ASCII '-'；
        // 应通过 Unicode 归一化命中并整段替换（不引入新 bug：替换范围精确到原文字节）。
        let ws = TmpWs::new();
        ws.write("u.rs", "price: 100\u{2013}200\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "u.rs".into(),
                old_text: "100-200".into(),
                new_text: "100-300".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true, "应通过 Unicode 归一化匹配 en-dash: {r}");
        let content = std::fs::read_to_string(root.join("u.rs")).unwrap();
        assert_eq!(
            content, "price: 100-300\n",
            "en-dash 段应被整段替换为 ASCII 新文本"
        );
    }

    #[tokio::test]
    async fn exact_match_takes_priority_over_fuzzy() {
        // 同时存在精确命中与 Unicode 变体时，精确优先（只算精确那次），不被模糊覆盖计数
        let ws = TmpWs::new();
        ws.write("p.rs", "a-b a\u{2013}b\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "p.rs".into(),
                old_text: "a-b".into(),
                new_text: "X".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true, "精确命中应优先: {r}");
        let content = std::fs::read_to_string(root.join("p.rs")).unwrap();
        // 仅 ASCII 的 "a-b" 被替换；en-dash 的 "a–b" 原样保留
        assert_eq!(content, "X a\u{2013}b\n");
    }

    #[tokio::test]
    async fn fuzzy_matches_escaped_quotes() {
        // 文件里是裸引号（如 JS 源码 `"芯片+算法+数据"`），模型 old_text 被 JSON 双重转义
        // 成 `\"芯片+算法+数据\"`（解码后多出反斜杠）——pass 3 应折叠转义对后命中。
        let ws = TmpWs::new();
        ws.write("q.js", "let s = \"hello world\";\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "q.js".into(),
                old_text: "let s = \\\"hello world\\\";".into(), // 实际内容: \"hello world\"
                new_text: "let s = 'hi';".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true, "应折叠 \\\" 后命中: {r}");
        let content = std::fs::read_to_string(root.join("q.js")).unwrap();
        assert_eq!(content, "let s = 'hi';\n", "替换范围应覆盖完整转义对");
    }

    #[tokio::test]
    async fn escaped_quote_pass_does_not_fire_when_exact_exists() {
        // 精确命中优先：文件里真有 `\"`（源码转义），old_text 一致时走 pass 1，不进 pass 3
        let ws = TmpWs::new();
        ws.write("e.rs", "let s = \"a\\\"b\";\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "e.rs".into(),
                old_text: "a\\\"b".into(), // 实际内容: a\"b
                new_text: "xyz".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = std::fs::read_to_string(root.join("e.rs")).unwrap();
        assert_eq!(content, "let s = \"xyz\";\n");
    }

    #[tokio::test]
    async fn miss_diagnostic_reports_first_diff() {
        // old_text 与文件仅差一个字符且不可被任何 pass 折叠：报错应带最近行诊断
        let ws = TmpWs::new();
        ws.write("h.rs", "fn calculate_total() {}\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "h.rs".into(),
                old_text: "fn calculate_totl() {}".into(), // 少了个 a
                new_text: "fn sum() {}".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        let err = r["error"].as_str().unwrap();
        assert!(
            err.contains("Closest candidate is line 1"),
            "诊断应指出第 1 行: {err}"
        );
    }

    #[tokio::test]
    async fn strips_read_line_number_prefixes() {
        // 模型把 read_file 输出整段复制进 old_text（带 `12→` 行号前缀）——
        // 对齐 Claude Code Edit 的 "strip the Read line prefix" 容错，应剥前缀后命中。
        let ws = TmpWs::new();
        ws.write("p.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "p.rs".into(),
                old_text: "2→fn b() {}\n3→fn c() {}".into(),
                new_text: "fn b2() {}\nfn c2() {}".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true, "应剥行号前缀后命中: {r}");
        let content = std::fs::read_to_string(root.join("p.rs")).unwrap();
        assert_eq!(content, "fn a() {}\nfn b2() {}\nfn c2() {}\n");
    }

    #[tokio::test]
    async fn rejects_writing_into_git_dir() {
        let ws = TmpWs::new();
        ws.write(".git/config", "[origin]\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: ".git/config".into(),
                old_text: "[origin]".into(),
                new_text: "[evil]".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(
            r["error"].as_str().unwrap().contains("VCS metadata"),
            "应拒绝写入 .git: {r}"
        );
        // 原文件未被改动
        let content = std::fs::read_to_string(root.join(".git").join("config")).unwrap();
        assert!(content.contains("[origin]"));
    }
}
