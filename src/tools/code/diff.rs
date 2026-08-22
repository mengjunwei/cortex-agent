//! 工作区文件改动的 unified diff 生成（edit_file / create_file 共用）。
//!
//! codex 用 `similar::TextDiff`（apply-patch/lib.rs `unified_diff_from_chunks`）；
//! 本项目已有 `diff` crate 依赖，行级 LCS 足够（对齐 codex 的输出形态：
//! `--- a/path` / `+++ b/path` 头 + `@@ -l,c +l,c @@` hunk + `-`/`+`/` ` 行）。
//!
//! 实现要点（审查修复后）：
//! 1. **公共前后缀剥离**：`diff` crate 是 O(n·m) 全表 DP，万行文件直接做会分配
//!    数百 MB。先剥掉公共前缀/后缀（append/小改动场景几乎全等，剩余区间极小），
//!    只对中间差异段做 LCS。
//! 2. **3 行 context 窗口**：对齐标准 unified diff（git 默认），不再把变更前的
//!    全部上下文塞进 hunk（旧实现 500 行文件改末行会输出 499 行 context）。
//! 3. **行号正确推进**：old/new 行号随扫描逐行推进，flush 后从新位置起算，
//!    多 hunk 的 `@@` 头行号准确。
//! 4. **总行数封顶**：diff 是给模型/前端展示的，不是给 git apply 的——超限时
//!    保留头尾、中间标记截断（对齐 read_file 的 middle_truncate 惯例），防大
//!    文件 diff 把上下文/界面撑爆。

/// hunk 的 context 半径（变更行前后各保留几行上下文），对齐 git 默认 3。
const CONTEXT_RADIUS: usize = 3;
/// diff 总行数封顶（含 +/-/context/hunk 头），超限保留头尾并插入截断标记。
const MAX_DIFF_LINES: usize = 400;
/// 单行字节封顶：超长单行（minified JS / base64 / 单行 JSON 数 MB）在行数封顶
/// 之下整体穿透——diff 是给模型/前端展示的，单行超限截断中间并加省略标记。
const MAX_LINE_BYTES: usize = 2_000;
/// LCS 输入规模上限（行数）：中间段行数超限时不再做 O(n·m) 全表 DP（20000×20000
/// 全变文件会分配 1.6GB），退化为「整段删除 + 整段新增」的粗粒度 diff——
/// 正确性不丢（仍是完整差异），只损失逐行对齐精度，且输出有 MAX_DIFF_LINES 兜底。
const MAX_LCS_LINES: usize = 5_000;

/// 生成 unified diff（`--- a/path` / `+++ b/path` 头 + hunk）。
pub(crate) fn make_unified_diff(old: &str, new: &str, path: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // 公共前后缀剥离：只对中间差异段做 O(n·m) LCS（万行 append 场景剩余段≈1 行）
    let common_prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();
    // 公共后缀必须两侧都从尾部倒序对齐。曾经写成 old 正序 zip new 倒序——old 中段
    // 正序恰好等于 new 中段倒序（回文式尾部重排）时 suffix 被高估，被剥掉的行
    // 静默丢差异（脚本已复现：尾部 wxyz→zyxw 时错算 4、正确 0）。
    let common_suffix = old_lines[common_prefix..]
        .iter()
        .rev()
        .zip(new_lines[common_prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    // 前后缀各回退 CONTEXT_RADIUS 行进入 diff 段：剥得过净会让 hunk 失去变更前的
    // context 行（git diff 有 context，剥净后没有）。
    let mid_start = common_prefix.saturating_sub(CONTEXT_RADIUS);
    let mid_old_end = (old_lines.len() - common_suffix + CONTEXT_RADIUS).min(old_lines.len());
    let mid_new_end = (new_lines.len() - common_suffix + CONTEXT_RADIUS).min(new_lines.len());

    // 剥离后中间段做行级 diff；段超 MAX_LCS_LINES 时退化为整段删+增（防 O(n·m) OOM）
    let mid_old = &old_lines[mid_start..mid_old_end];
    let mid_new = &new_lines[mid_start..mid_new_end];
    let hunks: Vec<diff::Result<&str>> =
        if mid_old.len() > MAX_LCS_LINES || mid_new.len() > MAX_LCS_LINES {
            mid_old
                .iter()
                .map(|l| diff::Result::Left(*l))
                .chain(mid_new.iter().map(|l| diff::Result::Right(*l)))
                .collect()
        } else {
            diff::slice(mid_old, mid_new)
                .into_iter()
                .map(|r| match r {
                    diff::Result::Left(l) => diff::Result::Left(*l),
                    diff::Result::Both(l, _) => diff::Result::Both(*l, *l),
                    diff::Result::Right(r) => diff::Result::Right(*r),
                })
                .collect()
        };

    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));

    // 把变更序列转成 (kind, old_idx, new_idx, line) 流，再按 context 窗口分组 hunk
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Ctx,
        Del,
        Add,
    }
    let mut items: Vec<(Kind, usize, usize, &str)> = Vec::new();
    let mut oi = mid_start; // 旧文件行号（0 基）
    let mut ni = mid_start; // 新文件行号（0 基）
    for change in &hunks {
        match change {
            diff::Result::Left(l) => {
                items.push((Kind::Del, oi, ni, l));
                oi += 1;
            }
            diff::Result::Both(l, _) => {
                items.push((Kind::Ctx, oi, ni, l));
                oi += 1;
                ni += 1;
            }
            diff::Result::Right(r) => {
                items.push((Kind::Add, oi, ni, r));
                ni += 1;
            }
        }
    }

    // 找出所有变更项的下标，按「间隔 > 2*CONTEXT_RADIUS 的相邻变更」切分 hunk
    let changed: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, (k, _, _, _))| !matches!(k, Kind::Ctx))
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return out.trim_end().to_string();
    }
    let mut hunk_spans: Vec<(usize, usize)> = Vec::new(); // [start, end) items 下标
    let mut span_start = changed[0];
    let mut prev = changed[0];
    for &c in &changed[1..] {
        if c - prev > CONTEXT_RADIUS * 2 + 1 {
            hunk_spans.push((
                span_start.saturating_sub(CONTEXT_RADIUS),
                (prev + CONTEXT_RADIUS + 1).min(items.len()),
            ));
            span_start = c;
        }
        prev = c;
    }
    hunk_spans.push((
        span_start.saturating_sub(CONTEXT_RADIUS),
        (prev + CONTEXT_RADIUS + 1).min(items.len()),
    ));

    // 渲染 hunk（带总行数封顶）
    let mut emitted = 0usize; // 已输出的正文行数（不含头）
    let mut truncated = false;
    for (si, (start, end)) in hunk_spans.iter().enumerate() {
        // hunk 头行号（git 语义）：起始 0 仅允许配 count 0（空侧）——
        // 纯新增 hunk 旧侧是 @@ -0,0 +1,N @@；非空侧起始至少为 1。
        // 首项是 Add/Del 时对侧行号是「插入/删除点」，非空侧起点取 max(1, 点位)。
        let old_count = items[*start..*end]
            .iter()
            .filter(|(k, _, _, _)| !matches!(k, Kind::Add))
            .count();
        let new_count = items[*start..*end]
            .iter()
            .filter(|(k, _, _, _)| !matches!(k, Kind::Del))
            .count();
        let (old_start1, new_start1) = match items[*start] {
            (Kind::Ctx, o, n, _) => (o + 1, n + 1),
            // Add 开头：插入点 o（旧侧行号）；纯新增（old_count=0）旧侧 0，
            // 否则从插入点前一行起（max(1, o)）；新侧从插入位置 n+1 起
            (Kind::Add, o, n, _) => (if old_count == 0 { 0 } else { o.max(1) }, n + 1),
            // Del 开头：删除点 n（新侧行号）；纯删除（new_count=0）新侧 0，
            // 否则从删除点前 max(1, n) 起；旧侧从 n+1 起
            (Kind::Del, o, n, _) => (o + 1, if new_count == 0 { 0 } else { n.max(1) }),
        };

        let body_lines = end - start;
        if emitted + body_lines > MAX_DIFF_LINES {
            // 超限：本 hunk 不再输出，标记截断
            truncated = true;
            break;
        }
        out.push_str(&format!(
            "@@ -{old_start1},{old_count} +{new_start1},{new_count} @@\n"
        ));
        for (k, _, _, line) in &items[*start..*end] {
            let prefix = match k {
                Kind::Ctx => ' ',
                Kind::Del => '-',
                Kind::Add => '+',
            };
            out.push(prefix);
            push_line_capped(&mut out, line);
            out.push('\n');
        }
        emitted += body_lines;
        let _ = si;
    }
    if truncated {
        out.push_str(&format!(
            "@@ ... diff truncated ({} lines shown, over {} line limit) ... @@\n",
            emitted, MAX_DIFF_LINES
        ));
    }
    out.trim_end().to_string()
}

/// 追加一行到 diff 输出，超长单行截断中间（在 char 边界切，防切坏 UTF-8）。
fn push_line_capped(out: &mut String, line: &str) {
    if line.len() <= MAX_LINE_BYTES {
        out.push_str(line);
        return;
    }
    // 前后各留一半，中间省略标记带原始长度
    let half = MAX_LINE_BYTES / 2;
    let head_end = char_boundary_at(line, half);
    let tail_start = char_boundary_at(line, line.len().saturating_sub(half));
    out.push_str(&line[..head_end]);
    out.push_str(&format!(" …[{} bytes truncated]… ", line.len()));
    out.push_str(&line[tail_start.min(line.len())..]);
}

/// 在 `s` 中找 ≤ `target` 的最大字符边界。
fn char_boundary_at(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut i = target;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_format() {
        let d = make_unified_diff("a\nb\nc\n", "a\nB\nc\n", "f.rs");
        assert!(d.starts_with("--- a/f.rs"));
        assert!(d.contains("+++ b/f.rs"));
        assert!(d.contains("@@"));
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
    }

    #[test]
    fn empty_to_content_is_all_additions() {
        // 新建文件（旧内容为空）：全部是 + 行
        let d = make_unified_diff("", "line1\nline2\n", "new.txt");
        assert!(d.contains("+line1"));
        assert!(d.contains("+line2"));
        // 无删除正文行（diff 头 --- 不算）
        assert!(
            !d.lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---"))
        );
    }

    #[test]
    fn content_to_empty_is_all_deletions() {
        let d = make_unified_diff("line1\nline2\n", "", "gone.txt");
        assert!(d.contains("-line1"));
        assert!(d.contains("-line2"));
        assert!(
            !d.lines()
                .any(|l| l.starts_with('+') && !l.starts_with("+++"))
        );
    }

    #[test]
    fn append_shows_only_tail_additions() {
        // 追加：旧内容行都是上下文/保留，仅新增尾部 + 行
        let d = make_unified_diff("a\nb\n", "a\nb\nc\nd\n", "app.log");
        assert!(d.contains("+c"));
        assert!(d.contains("+d"));
        // 无删除正文行
        assert!(
            !d.lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---"))
        );
    }

    #[test]
    fn no_change_produces_header_only() {
        // old==new（如 append 空内容）：只有头两行，无 hunk
        let d = make_unified_diff("a\nb\n", "a\nb\n", "same.txt");
        assert!(!d.contains("@@"));
        assert!(d.contains("--- a/same.txt"));
    }

    #[test]
    fn far_apart_changes_get_separate_hunks_with_correct_line_numbers() {
        // 两处远隔修改：拆成两个 hunk，行号各自准确（此前实现的回归点）
        let mut old = Vec::new();
        for i in 1..=100 {
            old.push(format!("line{i}"));
        }
        let mut new = old.clone();
        new[4] = "CHANGED_A".into(); // 第 5 行
        new[94] = "CHANGED_B".into(); // 第 95 行
        let old_s = old.join("\n");
        let new_s = new.join("\n");
        let d = make_unified_diff(&old_s, &new_s, "m.txt");
        let hunk_headers: Vec<&str> = d.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(hunk_headers.len(), 2, "应拆成两个 hunk: {d}");
        // 第一个 hunk 从第 2 行（5-3 context）开始
        assert!(
            hunk_headers[0].contains("-2,"),
            "第一 hunk 旧起始行应为 2（5-3 上下文）: {}",
            hunk_headers[0]
        );
        // 第二个 hunk 从第 92 行（95-3）开始
        assert!(
            hunk_headers[1].contains("-92,"),
            "第二 hunk 旧起始行应为 92（95-3 上下文）: {}",
            hunk_headers[1]
        );
        assert!(d.contains("-line5"));
        assert!(d.contains("+CHANGED_A"));
        assert!(d.contains("-line95"));
        assert!(d.contains("+CHANGED_B"));
    }

    #[test]
    fn leading_context_is_capped_to_radius() {
        // 500 行文件只改最后一行：context 只留前 3 行，不是 499 行
        let mut old = Vec::new();
        for i in 1..=500 {
            old.push(format!("l{i}"));
        }
        let mut new = old.clone();
        new[499] = "changed".into();
        let d = make_unified_diff(&old.join("\n"), &new.join("\n"), "t.txt");
        // 正文行数 = 3 context + 1 del + 1 add = 5，加 diff 头 2 行 + hunk 头 1 行 = 8
        assert!(d.lines().count() <= 10, "context 应被 3 行窗口限制: {d}");
        assert!(d.contains("-l500"));
        assert!(d.contains("+changed"));
    }

    #[test]
    fn huge_file_truncated() {
        // 万行全换：diff 超封顶被截断且带标记（防上下文撑爆）
        let old: String = (0..10_000).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..10_000).map(|i| format!("new{i}\n")).collect();
        let d = make_unified_diff(&old, &new, "big.txt");
        assert!(
            d.contains("diff truncated"),
            "超限应带截断标记: {}...",
            &d[..d.len().min(200)]
        );
        assert!(d.lines().count() <= MAX_DIFF_LINES + 5);
    }

    #[test]
    fn common_prefix_strips_efficiently() {
        // 万行文件末尾追加 1 行：前后缀剥离后剩余段极小，正确输出尾部 + 行
        let old: String = (0..10_000).map(|i| format!("l{i}\n")).collect();
        let new = format!("{old}appended\n");
        let d = make_unified_diff(&old, &new, "app.log");
        assert!(d.contains("+appended"));
        // context 3 行 + 1 add
        assert!(d.lines().count() <= 10, "只应有尾部小 hunk: {d}");
    }

    #[test]
    fn palindrome_tail_rearrangement_not_overstripped() {
        // 回文式尾部重排（old 尾 wxyz vs new 尾 zyxw）：错误的后缀算法（old 正序
        // zip new 倒序）会把 suffix 高估成 4、静默吞掉整段重排差异——两侧都倒序
        // 对齐后 suffix=0，LCS=abcz，变更行 -w/-x/-y 与 +y/+x/+w 全部出现
        // （z 是公共行留作 ctx，回归锁）。
        let old = "a\nb\nc\nw\nx\ny\nz\n";
        let new = "a\nb\nc\nz\ny\nx\nw\n";
        let d = make_unified_diff(old, new, "pal.txt");
        for expect in ["-w", "-x", "-y", "+y", "+x", "+w"] {
            assert!(
                d.lines().any(|l| l == expect),
                "重排差异行 {expect} 应出现在 diff: {d}"
            );
        }
    }

    #[test]
    fn pure_addition_hunk_header_matches_git() {
        // 新建文件：git 语义 hunk 头是 @@ -0,0 +1,N @@
        let d = make_unified_diff("", "l1\nl2\nl3\n", "new.txt");
        assert!(
            d.contains("@@ -0,0 +1,3 @@"),
            "新建 hunk 头应为 -0,0 +1,3: {d}"
        );
    }

    #[test]
    fn pure_deletion_hunk_header_matches_git() {
        // 清空文件：git 语义 hunk 头是 @@ -1,N +0,0 @@
        let d = make_unified_diff("l1\nl2\n", "", "gone.txt");
        assert!(
            d.contains("@@ -1,2 +0,0 @@"),
            "清空 hunk 头应为 -1,2 +0,0: {d}"
        );
    }

    #[test]
    fn insertion_at_file_start_has_legal_header() {
        // 文件开头插入一行：非空侧起始至少 1（git: @@ -1,2 +1,3 @@），
        // 不得出现 -0,2 这种「0 起始配非零 count」的非法头（此前实现回归点）
        let d = make_unified_diff("a\nb\n", "x\na\nb\n", "ins.txt");
        assert!(
            d.contains("@@ -1,2 +1,3 @@"),
            "开头插入 hunk 头应为 -1,2 +1,3: {d}"
        );
    }

    #[test]
    fn deletion_at_file_start_has_legal_header() {
        // 文件开头删除一行：git: @@ -1,3 +1,2 @@（非 +0,2）
        let d = make_unified_diff("x\na\nb\n", "a\nb\n", "del.txt");
        assert!(
            d.contains("@@ -1,3 +1,2 @@"),
            "开头删除 hunk 头应为 -1,3 +1,2: {d}"
        );
    }

    #[test]
    fn huge_single_line_truncated_by_bytes() {
        // 超长单行（minified JS/base64）：行数封顶之下也要按字节截断，
        // 防 MB 级 diff 直送模型上下文与前端 DOM（此前穿透点）
        let huge = "x".repeat(5_000_000);
        let old = format!("{huge}\n");
        let new = format!("{huge}tail\n");
        let d = make_unified_diff(&old, &new, "min.js");
        assert!(
            d.len() < 100_000,
            "MB 级单行 diff 应被字节截断（实际 {} 字节）",
            d.len()
        );
        assert!(
            d.contains("bytes truncated"),
            "应带截断标记: {}…",
            &d[..d.len().min(300)]
        );
    }
}
