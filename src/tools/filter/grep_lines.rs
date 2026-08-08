//! grep / find 类多文件检索输出的行聚合过滤
//!
//! 借鉴 ecotokens 的 filter/grep.rs。`grep -rn` 的原始输出形如
//! `path:line:content`，动辄数百上千行。本过滤器按文件分组，每文件
//! 最多保留 N 条匹配，保留首次出现顺序，并对超长行做 UTF-8 安全截断。

use std::collections::{HashMap, HashSet};

use super::floor_char_boundary;

/// 每个文件最多保留的匹配行数
const MAX_MATCHES_PER_FILE: usize = 10;
/// 单行内容截断阈值
const MAX_LINE_LEN: usize = 120;

/// 过滤 grep 风格输出
///
/// 无法解析为 `file:line:content` 或 `file:content` 的行原样附在末尾。
pub(crate) fn filter(text: &str) -> String {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    // order 保序、seen 做 O(1) 成员判定（回归 B7：此前 order.contains 是 O(n²)）
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut leftovers: Vec<String> = Vec::new();

    for line in text.lines() {
        if let Some((file, rest)) = split_file_prefix(line) {
            // seen 做 O(1) 首次出现判定；groups.entry 消费 file 作为 key
            if seen.insert(file.clone()) {
                order.push(file.clone());
            }
            let entry = groups.entry(file).or_default();
            if entry.len() < MAX_MATCHES_PER_FILE {
                entry.push(truncate_line(&rest));
            }
        } else {
            leftovers.push(line.to_string());
        }
    }

    if order.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(4 * 1024);
    for file in &order {
        out.push_str(&format!("── {} ──\n", file));
        for l in &groups[file] {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !leftovers.is_empty() {
        out.push_str("\n[未分组行]\n");
        for l in &leftovers {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// 拆分 `file:line:content` 或 `file:content`，返回 (file, 余下内容)
///
/// 兼容 Windows 盘符路径（回归 B5）：`C:\Users\foo\bar.rs:10:content` 的首个
/// `:` 属盘符分隔符，不能当作路径/内容分隔点。检测到 `^[A-Za-z]:[\\/]` 时
/// 从盘符后开始查找真正的分隔冒号。
fn split_file_prefix(line: &str) -> Option<(String, String)> {
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }

    // 起始查找位置：跳过 Windows 盘符 `X:\` 前缀（2 字节）
    let start = windows_drive_prefix_len(line);
    // 至少要求含一个 ':'，且冒号前部分像路径（非空白、不含中文标点）
    let colon = line[start..].find(':')? + start;
    let file = &line[..colon];
    if file.is_empty() || file.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    let rest = &line[colon + 1..];
    // 尝试剥离行号 `123:` 前缀
    let rest = strip_line_number(rest);
    Some((file.to_string(), rest.to_string()))
}

/// 检测行首的 Windows 盘符前缀长度：`C:\` / `c:/` 返回 2，否则返回 0
fn windows_drive_prefix_len(line: &str) -> usize {
    let bytes = line.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        2
    } else {
        0
    }
}

/// 若 `rest` 以 `digits:` 开头，剥离行号；否则原样返回
fn strip_line_number(rest: &str) -> &str {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b':' {
        &rest[i + 1..]
    } else {
        rest
    }
}

fn truncate_line(s: &str) -> String {
    if s.len() <= MAX_LINE_LEN {
        s.to_string()
    } else {
        let cut = floor_char_boundary(s, MAX_LINE_LEN);
        let mut out = s[..cut].to_string();
        out.push_str(" …");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_by_file() {
        let text = "src/a.rs:10:foo\n\
                    src/a.rs:20:bar\n\
                    src/b.rs:5:baz\n";
        let out = filter(text);
        assert!(out.contains("── src/a.rs ──"));
        assert!(out.contains("── src/b.rs ──"));
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
        assert!(out.contains("baz"));
    }

    #[test]
    fn caps_matches_per_file() {
        let mut text = String::new();
        for i in 0..30 {
            text.push_str(&format!("main.c:{}:line{}\n", i, i));
        }
        let out = filter(&text);
        // 仍属于同一文件组，最多 MAX_MATCHES_PER_FILE 条
        let main_lines = out.lines().filter(|l| l.starts_with("line")).count();
        assert_eq!(main_lines, MAX_MATCHES_PER_FILE);
    }

    #[test]
    fn preserves_first_occurrence_order() {
        let text = "z.rs:1:a\na.rs:1:b\nz.rs:2:c\na.rs:2:d";
        let out = filter(text);
        let z_pos = out.find("── z.rs").unwrap();
        let a_pos = out.find("── a.rs").unwrap();
        assert!(z_pos < a_pos);
    }

    #[test]
    fn unparseable_lines_go_to_leftovers() {
        let text = "src/x.rs:1:match\nrandom line without colon structure";
        let out = filter(text);
        assert!(out.contains("[未分组行]"));
        assert!(out.contains("random line"));
    }

    #[test]
    fn long_content_truncated_utf8_safe() {
        let content = "中".repeat(200);
        let text = format!("f.txt:1:{}", content);
        let out = filter(&text);
        assert!(out.contains("…"));
        // 截断点必在字符边界
        let cut_line = out.lines().find(|l| l.contains('…')).unwrap();
        let _ = cut_line; // 仅验证不 panic
    }

    #[test]
    fn empty_input_passthrough() {
        assert_eq!(filter(""), "");
    }

    #[test]
    fn windows_drive_path_parsed_correctly() {
        // 回归 B5：盘符冒号不能当作路径/内容分隔点
        let text = r"C:\Users\foo\bar.rs:10:fn main() {}";
        let out = filter(text);
        // 整个路径应作为一个文件分组名，而非被切作 "C"
        assert!(out.contains("── C:\\Users\\foo\\bar.rs ──"));
        assert!(out.contains("fn main() {}"));
    }

    #[test]
    fn windows_drive_path_multiple_files() {
        let text = r"C:\a.rs:1:foo
C:\b.rs:2:bar
D:\proj\src\c.rs:3:baz";
        let out = filter(text);
        assert!(out.contains("── C:\\a.rs ──"));
        assert!(out.contains("── C:\\b.rs ──"));
        assert!(out.contains("── D:\\proj\\src\\c.rs ──"));
        // 三个不同文件，分组数量正确（未被盘符冒号误切）
        let group_count = out.lines().filter(|l| l.contains("──")).count();
        assert_eq!(group_count, 3);
    }
}
