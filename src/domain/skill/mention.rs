//! `$skill-name` mention parser。
//!
//! 对齐 codex `skills/src/mentions.rs`(`extract_tool_mentions_with_sigil`):
//! - sigil 仅 `$`(`@` 是 plugin 命名空间,cortex 不实现 plugin,故不识别;邮箱/handle 不触发)
//! - name 字符集 `[a-zA-Z0-9_:-]`,大小写敏感(不做小写化——codex 提取后原样比对)
//! - 环境变量黑名单按大写比较(codex `is_common_env_var`),过滤 `$PATH`/`$HOME` 等误匹配
//! - 同一 name 多次提及只返回一次;保留首次出现顺序
//! - 不存在的 name 不在此过滤(由调用方与 catalog 交叉校验后丢弃)
//!
//! codex 的 `[$name](path)` 链接式提及与 path 匹配优先级依赖其多根 catalog,
//! cortex 单根无歧义,仅实现裸名提及。

use regex::Regex;
use std::sync::OnceLock;

/// `$name` 匹配正则:name = `[a-zA-Z0-9_:-]+`(对齐 codex `is_mention_name_char`)
fn mention_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\$[a-zA-Z0-9_:-]+").unwrap())
}

/// 常见环境变量名黑名单 —— 正文里的 `$PATH`/`$HOME` 等不应被当成 skill 提及。
/// 对齐 codex `is_common_env_var`:按大写比较(匹配 `$path`/`$Path` 等任意大小写)。
fn is_common_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH" | "HOME" | "USER" | "SHELL" | "PWD" | "TMPDIR" | "TEMP" | "TMP" | "LANG" | "TERM"
    )
}

/// 从文本中提取 `$skill-name` 提及,去重并保留首次出现顺序。
/// 自动过滤常见环境变量名(避免 `$PATH` 之类误匹配)。
/// 大小写敏感:提取后原样返回,由调用方与 catalog 精确比对。
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
        // $PATH/$HOME 等环境变量不识别为 skill(大小写任意,对齐 codex 大写比较)
        assert!(extract_mentions("see $PATH and $HOME").is_empty());
        assert!(extract_mentions("see $Path and $home").is_empty());
        // 混合:仅保留真实 skill 提及
        assert_eq!(
            extract_mentions("use $my-skill then $PATH"),
            vec!["my-skill".to_string()]
        );
    }

    #[test]
    fn case_sensitive_no_normalization() {
        // 对齐 codex:大小写敏感,原样提取,不做小写化
        assert_eq!(extract_mentions("use $Foo here"), vec!["Foo".to_string()]);
        assert_eq!(
            extract_mentions("invoke $Skill-Creator"),
            vec!["Skill-Creator".to_string()]
        );
        // $Skill-Creator 与 $skill-creator 是两个不同提及
        assert_eq!(
            extract_mentions("invoke $Skill-Creator and $skill-creator"),
            vec!["Skill-Creator".to_string(), "skill-creator".to_string()]
        );
    }

    #[test]
    fn underscore_and_colon_chars_allowed() {
        // 对齐 codex is_mention_name_char:[a-zA-Z0-9_:-]
        assert_eq!(
            extract_mentions("use $foo_bar"),
            vec!["foo_bar".to_string()]
        );
        assert_eq!(
            extract_mentions("use $plugin:skill"),
            vec!["plugin:skill".to_string()]
        );
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
