//! FAQ 提取的纯函数辅助：候选构建、主题归一化、内容规范化、命令推断、文档名解析等。
//!
//! 这些函数无状态、可独立单元测试，被 [`crate::domain::knowledge::KnowledgeManager`]
//! 的 FAQ 流程及其单元测试复用。从 `mod.rs` 抽出，便于 `mod.rs` 聚焦业务编排。

use super::FaqCandidate;

pub(crate) fn build_candidate(
    title: &serde_json::Value,
    content: &serde_json::Value,
) -> Option<FaqCandidate> {
    let title = title.as_str()?.trim().to_string();
    let raw_content = content.as_str()?.trim();
    if title.is_empty() || raw_content.is_empty() {
        return None;
    }
    let content = normalize_faq_content(raw_content);
    if content.is_empty() {
        return None;
    }
    Some(FaqCandidate {
        char_count: content.chars().count(),
        title,
        content,
        duplicate: false,
    })
}

/// 主题归一化键：用于将「端口IPv6配置」「接口IPv6地址配置」这类描述同一件事
/// 但用词略有差别的 FAQ 标题映射到同一个 key，从而合并候选并跨别名查重。
///
/// 处理规则：
/// 1. 转小写，去空白与全角空白；
/// 2. 删除“配置/设置/方法/示例/操作/管理/教程/说明/案例/详解”等通用后缀词；
/// 3. 把同义词归一：接口/端口 → port、地址/ip → ip 等；
/// 4. 按 Unicode 字符排序后拼接，使得词序差异不影响匹配。
pub(crate) fn normalize_topic_key(title: &str) -> String {
    let mut s: String = title
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();

    let stop_words = [
        "配置", "设置", "方法", "示例", "实例", "实战", "实践", "操作", "管理", "教程", "说明",
        "案例", "详解", "详细", "命令", "指令", "地址",
        // 描述程度/范围的修饰词，不影响主题
        "高级", "进阶", "初级", "基础", "基本", "入门", "简单", "快速", "常用", "常见", "常规",
        "通用", "简明", "完整", "全面", "深入", "扩展",
    ];
    for w in stop_words {
        s = s.replace(w, "");
    }

    let synonyms: [(&str, &str); 8] = [
        ("接口", "port"),
        ("端口", "port"),
        ("路由", "route"),
        ("静态", "static"),
        ("动态", "dynamic"),
        ("子网", "subnet"),
        ("掩码", "mask"),
        ("网关", "gw"),
    ];
    for (from, to) in synonyms {
        s = s.replace(from, to);
    }

    let mut chars: Vec<char> = s.chars().collect();
    chars.sort_unstable();
    chars.into_iter().collect()
}

/// 合并候选中归一化主题键相同的条目，保留 content 较长的一条，
/// 以避免同主题不同描述被同时上传形成重复文档。
pub(crate) fn merge_similar_candidates(items: Vec<FaqCandidate>) -> Vec<FaqCandidate> {
    let mut keys: Vec<String> = Vec::with_capacity(items.len());
    let mut merged: Vec<FaqCandidate> = Vec::with_capacity(items.len());

    for item in items {
        let key = normalize_topic_key(&item.title);
        if key.is_empty() {
            merged.push(item);
            keys.push(String::new());
            continue;
        }

        if let Some(idx) = keys.iter().position(|k| k == &key) {
            let existing = &merged[idx];
            tracing::info!(
                "[merge_similar_candidates] 合并近义候选: 「{}」 与 「{}」",
                existing.title,
                item.title
            );
            if item.content.chars().count() > existing.content.chars().count() {
                merged[idx] = item;
            }
        } else {
            keys.push(key);
            merged.push(item);
        }
    }

    merged
}

pub(crate) fn normalize_faq_content(raw: &str) -> String {
    let raw = replace_angle_params(raw);
    let sections = [
        "命令说明",
        "命令格式",
        "参数说明",
        "配置示例",
        "回退命令",
        "注意事项",
    ];
    let mut bodies = vec![String::new(); sections.len()];
    let mut current: Option<usize> = None;
    let mut matched = false;

    for line in raw.lines() {
        if let Some(idx) = section_index(line, &sections) {
            current = Some(idx);
            matched = true;
            continue;
        }
        if let Some(idx) = current {
            if !bodies[idx].is_empty() {
                bodies[idx].push('\n');
            }
            bodies[idx].push_str(line);
        }
    }

    if !matched && !raw.trim().is_empty() {
        bodies[0] = raw.trim().to_string();
    }

    let inferred_command = infer_command_line(&raw).unwrap_or_else(|| "[命令 [参数]]".to_string());
    let defaults = [
        "按会话整理的网络设备命令知识。".to_string(),
        inferred_command.clone(),
        "| 参数 | 说明 | 必填 | 示例 |\n|------|------|------|------|\n| [参数] | 按实际命令填写 | 是 | [示例] |".to_string(),
        inferred_command,
        "无".to_string(),
        "变更前请确认设备型号、版本及业务影响。".to_string(),
    ];

    sections
        .iter()
        .enumerate()
        .map(|(idx, name)| {
            let body = bodies[idx].trim();
            let body = if body.is_empty() || (idx == 3 && body == "无") {
                defaults[idx].as_str()
            } else {
                body
            };
            format!("## {}\n{}", name, body)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn section_index(line: &str, sections: &[&str]) -> Option<usize> {
    let normalized = line
        .trim()
        .trim_start_matches('#')
        .trim()
        .trim_end_matches(':')
        .trim_end_matches('：')
        .trim();
    sections.iter().position(|name| normalized == *name)
}

pub(crate) fn replace_angle_params(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut buf = String::new();
    let mut in_angle = false;

    for ch in text.chars() {
        match ch {
            '<' if !in_angle => {
                in_angle = true;
                buf.clear();
            }
            '>' if in_angle => {
                in_angle = false;
                out.push('[');
                out.push_str(buf.trim());
                out.push(']');
                buf.clear();
            }
            _ if in_angle => buf.push(ch),
            _ => out.push(ch),
        }
    }

    if in_angle {
        out.push('<');
        out.push_str(&buf);
    }

    out
}

pub(crate) fn infer_command_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('|')
                && !line.starts_with("##")
                && (line.contains(' ') || line.contains('['))
                && !line.contains('。')
                && !line.contains('，')
        })
        .map(|line| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_candidate_counts_chars_and_skips_empty() {
        let t = serde_json::json!("静态路由配置");
        let c = serde_json::json!("内容");
        let item = build_candidate(&t, &c).expect("非空应构造成功");
        assert_eq!(item.title, "静态路由配置");
        assert!(item.content.contains("## 命令说明"));
        assert!(item.content.contains("## 配置示例"));
        assert_eq!(item.char_count, item.content.chars().count());
        assert!(!item.duplicate);

        // 标题为空应跳过
        assert!(build_candidate(&serde_json::json!(""), &c).is_none());
        // 内容为空应跳过
        assert!(build_candidate(&t, &serde_json::json!("")).is_none());
        // 非 string 类型应返回 None
        assert!(build_candidate(&serde_json::json!(123), &c).is_none());
    }

    #[test]
    fn normalize_faq_content_fills_missing_template_sections() {
        let raw = "## 命令说明\n配置静态路由\n\n## 命令格式\nip route-static <目标网段> <掩码> <下一跳IP>\n\n## 参数说明\n| 参数 | 说明 | 必填 | 示例 |";
        let out = normalize_faq_content(raw);

        assert!(out.contains("## 命令说明"));
        assert!(out.contains("## 命令格式"));
        assert!(out.contains("## 参数说明"));
        assert!(out.contains("## 配置示例"));
        assert!(out.contains("## 回退命令"));
        assert!(out.contains("## 注意事项"));
        assert!(out.contains("[目标网段]"));
        assert!(!out.contains("<目标网段>"));
        assert!(!out.contains("## 配置示例\n无"));
    }

    #[test]
    fn normalize_topic_key_unifies_synonyms() {
        let a = normalize_topic_key("端口IPv6配置");
        let b = normalize_topic_key("接口IPv6地址配置");
        assert!(!a.is_empty());
        assert_eq!(a, b);

        let unrelated = normalize_topic_key("OSPF区域配置");
        assert_ne!(a, unrelated);
    }

    #[test]
    fn normalize_topic_key_strips_qualifier_words() {
        // 同主题但加了「高级/进阶/详解/常见」等修饰，应归一到同一个 key
        let basic = normalize_topic_key("OSPF配置");
        let advanced = normalize_topic_key("OSPF高级配置");
        let detail = normalize_topic_key("OSPF详解");
        let advance2 = normalize_topic_key("OSPF进阶配置");
        assert!(!basic.is_empty());
        assert_eq!(basic, advanced);
        assert_eq!(basic, detail);
        assert_eq!(basic, advance2);

        // 真正不同的主题（如「区域」「邻居」）不应被合并
        let area = normalize_topic_key("OSPF区域配置");
        let neighbor = normalize_topic_key("OSPF邻居配置");
        assert_ne!(basic, area);
        assert_ne!(basic, neighbor);
        assert_ne!(area, neighbor);
    }

    #[test]
    fn merge_similar_candidates_dedupes_and_keeps_longest() {
        let c1 = FaqCandidate {
            title: "端口IPv6配置".to_string(),
            content: "短内容".to_string(),
            duplicate: false,
            char_count: "短内容".chars().count(),
        };
        let c2 = FaqCandidate {
            title: "接口IPv6地址配置".to_string(),
            content: "更完整的命令文档内容，比第一条长。".to_string(),
            duplicate: false,
            char_count: "更完整的命令文档内容，比第一条长。".chars().count(),
        };
        let c3 = FaqCandidate {
            title: "OSPF区域配置".to_string(),
            content: "ospf 配置".to_string(),
            duplicate: false,
            char_count: "ospf 配置".chars().count(),
        };

        let merged = merge_similar_candidates(vec![c1, c2.clone(), c3.clone()]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].title, "接口IPv6地址配置");
        assert!(merged[0].content.contains("更完整"));
        assert_eq!(merged[1].title, "OSPF区域配置");
    }
}
