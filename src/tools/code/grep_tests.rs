//! grep 工具测试 — 从 grep.rs 拆出(`#[cfg(test)] mod tests` 的体外形式)。

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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
        },
    );
    assert_eq!(r["ok"], true);
    assert_eq!(r["mode"], "symbol");
    assert_eq!(r["total_matches"], 2);
}

#[test]
fn files_with_matches_returns_file_list_only() {
    // 对齐 Claude Code Grep output_mode=files_with_matches：只返回去重文件列表，省 token
    let ws = TmpWs::new();
    ws.write("a_match.rs", "target_word\nmore target_word\n");
    ws.write("b_match.rs", "target_word\n");
    ws.write("c_other.rs", "nothing\n");
    let root = ws.canon();
    let r = grep_impl(
        &root,
        &GrepParams {
            pattern: "target_word".into(),
            is_regex: Some(false),
            path: None,
            context: None,
            case_sensitive: None,
            mode: None,
            output_mode: Some(OutputMode::FilesWithMatches),
            head_limit: None,
            glob: None,
        },
    );
    assert_eq!(r["ok"], true);
    assert_eq!(r["output_mode"], "files_with_matches");
    let files = r["files"].as_array().unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.as_str().unwrap()).collect();
    // a_match.rs（2 次命中）排前，b_match.rs 次之；c_other.rs 不在
    assert!(names.contains(&"a_match.rs"));
    assert!(names.contains(&"b_match.rs"));
    assert!(!names.iter().any(|n| n.contains("c_other")));
    assert_eq!(r["total_matches"], 3, "total_matches 仍是命中行数");
}

#[test]
fn count_mode_returns_per_file_counts() {
    let ws = TmpWs::new();
    ws.write("cnt.rs", "hit\nhit\nhit\n");
    ws.write("cnt2.rs", "hit\n");
    let root = ws.canon();
    let r = grep_impl(
        &root,
        &GrepParams {
            pattern: "hit".into(),
            is_regex: Some(false),
            path: None,
            context: None,
            case_sensitive: None,
            mode: None,
            output_mode: Some(OutputMode::Count),
            head_limit: None,
            glob: None,
        },
    );
    assert_eq!(r["output_mode"], "count");
    let counts = r["counts"].as_array().unwrap();
    assert_eq!(counts.len(), 2);
    assert_eq!(counts[0]["file"], "cnt.rs");
    assert_eq!(counts[0]["count"], 3);
    assert_eq!(counts[1]["count"], 1);
}

#[test]
fn head_limit_caps_content_results() {
    // 60 条命中 + head_limit=10：返回 10 条 + truncated 标记（模型据此缩小范围，
    // 而不是被旧的 50 条摘要断崖直接换格式）
    let ws = TmpWs::new();
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
            output_mode: None,
            head_limit: Some(10),
            glob: None,
        },
    );
    assert_eq!(r["ok"], true);
    assert_eq!(r["matches"].as_array().unwrap().len(), 10);
    assert_eq!(r["total_matches"], 10);
    assert_eq!(r["truncated"], true);
}

#[test]
fn default_content_mode_returns_all_lines() {
    // 无 head_limit：默认 content 模式返回全部命中（不再有 50 条摘要断崖）
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
            output_mode: None,
            head_limit: None,
            glob: None,
        },
    );
    assert_eq!(r["ok"], true);
    assert_eq!(r["output_mode"], "content");
    assert_eq!(r["total_matches"], 30);
    assert!(r["matches"].is_array());
}

#[test]
fn glob_filters_files_by_pattern() {
    // "*.rs" 应命中任意深度的 .rs 文件（basename 匹配），跳过 .txt
    let ws = TmpWs::new();
    ws.write("src/deep/nested.rs", "glob_target\n");
    ws.write("plain.txt", "glob_target\n");
    let root = ws.canon();
    let r = grep_impl(
        &root,
        &GrepParams {
            pattern: "glob_target".into(),
            is_regex: Some(false),
            path: None,
            context: None,
            case_sensitive: None,
            mode: None,
            output_mode: Some(OutputMode::FilesWithMatches),
            head_limit: None,
            glob: Some("*.rs".into()),
        },
    );
    assert_eq!(r["ok"], true);
    let files = r["files"].as_array().unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.as_str().unwrap()).collect();
    assert!(
        names
            .iter()
            .any(|f| f.replace('\\', "/").ends_with("nested.rs"))
    );
    assert!(!names.iter().any(|f| f.contains(".txt")));
}

#[test]
fn files_scanned_excludes_glob_filtered_files() {
    // files_scanned 只统计真正被读取的文件：带 glob 过滤时不应把被滤掉的 .txt 计入
    let ws = TmpWs::new();
    ws.write("g.rs", "scan_target\n");
    ws.write("g.txt", "scan_target\n");
    let root = ws.canon();
    let mk = |glob: Option<String>| GrepParams {
        pattern: "scan_target".into(),
        is_regex: Some(false),
        path: None,
        context: None,
        case_sensitive: None,
        mode: None,
        output_mode: Some(OutputMode::FilesWithMatches),
        head_limit: None,
        glob,
    };
    let with_glob = grep_impl(&root, &mk(Some("*.rs".into())));
    let no_glob = grep_impl(&root, &mk(None));
    assert!(
        with_glob["files_scanned"].as_u64() < no_glob["files_scanned"].as_u64(),
        "glob 过滤后 files_scanned 应更小: {} vs {}",
        with_glob["files_scanned"],
        no_glob["files_scanned"]
    );
}

#[test]
fn glob_with_directory_prefix_requires_path_match() {
    // 含 / 的模式按完整路径匹配：src/**/*.rs 不命中顶层 top.rs
    let ws = TmpWs::new();
    ws.write("top.rs", "glob_dir_target\n");
    ws.write("src/lib_x.rs", "glob_dir_target\n");
    let root = ws.canon();
    let r = grep_impl(
        &root,
        &GrepParams {
            pattern: "glob_dir_target".into(),
            is_regex: Some(false),
            path: None,
            context: None,
            case_sensitive: None,
            mode: None,
            output_mode: Some(OutputMode::FilesWithMatches),
            head_limit: None,
            glob: Some("src/**/*.rs".into()),
        },
    );
    assert_eq!(r["ok"], true);
    let files = r["files"].as_array().unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|f| f.as_str().unwrap().replace('\\', "/"))
        .collect();
    assert!(names.iter().any(|f| f == "src/lib_x.rs"));
    assert!(!names.iter().any(|f| f == "top.rs"));
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
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
            output_mode: None,
            head_limit: None,
            glob: None,
        },
    );
    assert_eq!(
        r_grep["files_scanned"], r_smart["files_scanned"],
        "smart 模式两轮扫描的 files_scanned 不应被双重计数"
    );
}
