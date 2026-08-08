//! `$skill-name` mention parser.
//!
//! 语法:`$` 后跟 `[a-z0-9-]+`,长度 1-64。
//! 同一 name 多次提及只返回一次;保留首次出现顺序。
//! 不存在的 name 不在此过滤(由调用方与 catalog 交叉校验后丢弃)。
//!
//! 对齐 codex:mention sigil 仅 `$`(`@` 是 plugin 命名空间 sigil,cortex 不实现 plugin,
//! 故不识别)。常见环境变量名(`$PATH`/`$HOME` 等)在 [`is_common_env_var`] 黑名单中,
//! 不会被误识别为 skill 提及。

use regex::Regex;
use std::sync::OnceLock;

/// `$name` 匹配正则:name = `(?:[a-z0-9]+-)*[a-z0-9]+`
fn mention_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\$(?:[a-z0-9]+-)*[a-z0-9]+").unwrap())
}

/// 常见环境变量名黑名单 —— 正文里的 `$PATH`/`$HOME` 等不应被当成 skill 提及。
/// 对齐 codex `is_common_env_var`,并补充 Windows 常见项(`username`/`appdata`/`comspec` 等)。
/// name 已由 mention 正则保证为小写,此处直接小写比较。
fn is_common_env_var(name: &str) -> bool {
    matches!(
        name,
        "path"
            | "home"
            | "user"
            | "username"
            | "logname"
            | "shell"
            | "shlvl"
            | "lang"
            | "lc_all"
            | "lc_ctype"
            | "term"
            | "pwd"
            | "oldpwd"
            | "hostname"
            | "editor"
            | "visual"
            | "ps1"
            | "appdata"
            | "localappdata"
            | "temp"
            | "tmp"
            | "tmpdir"
            | "comspec"
            | "programdata"
    )
}

/// 从文本中提取 `$skill-name` 提及,去重并保留首次出现顺序。
/// 自动过滤常见环境变量名(避免 `$PATH` 之类误匹配)。
pub fn extract_mentions(text: &str) -> Vec<String> {
    let re = mention_regex();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let name = &m.as_str()[1..]; // 去掉前导 $
        if name.len() <= 64 && !is_common_env_var(name) && seen.insert(name.to_string()) {
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
        assert_eq!(extract_mentions("$foo $foo $foo"), vec!["foo".to_string()]);
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
    fn does_not_match_at_sigil() {
        // @ 是 plugin 命名空间 sigil,cortex 不识别;邮箱/handle 不应触发 skill 注入
        assert!(extract_mentions("联系 john@example.com").is_empty());
        assert!(extract_mentions("use @foo here").is_empty());
    }

    #[test]
    fn filters_common_env_vars() {
        // $PATH/$HOME 等环境变量不识别为 skill
        assert!(extract_mentions("see $PATH and $HOME").is_empty());
        // 混合:仅保留真实 skill 提及
        assert_eq!(
            extract_mentions("use $my-skill then $PATH"),
            vec!["my-skill".to_string()]
        );
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
