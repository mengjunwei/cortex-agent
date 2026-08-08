//! 工具输出脱敏模块 — 工具输出进入 LLM 上下文前的敏感信息擦除
//!
//! 设计借鉴 ecotokens 的 masking/patterns.rs，并复用 adk-guardrail 的通用 PII 能力，
//! 形成三层互补的脱敏管线：
//!
//! ```text
//! 原始字符串
//!   ├─[1] 云/API 凭证正则 + URL 嵌入凭证  → AWS / OpenAI / GitHub / JWT / PEM ...
//!   │      ※ 须先于 PII：`user:pass@host` 形似 email，若 PII 先跑会把
//!   │        `pass@host` 误判为邮箱从而吞掉 `@`，导致 URL 凭证正则失配。
//!   ├─[2] 网络运维凭证正则                 → Cisco type-7 / SNMP community / sshpass ...
//!   └─[3] adk_guardrail::PiiRedactor      → Email / Phone（通用 PII）
//!          ※ 故意排除 IpAddress：设备 IP 是网络运维的核心业务数据
//! ```
//!
//! 三层覆盖范围零重叠，共同覆盖工具输出中可能泄露的敏感信息。
//!
//! ## ⚠️ 安全边界声明（best-effort，非安全边界）
//!
//! 本模块是**防御性 best-effort**，不是安全边界。正则脱敏存在已知盲区：
//! - 未覆盖的密钥形态、轮换后的新格式、跨行凭证、非 UTF-8 字节流
//! - base64/十六进制 blob 形态的凭证（无法与正常数据可靠区分）
//! - 键名未脱敏（仅脱敏值，以保持 JSON 结构）
//!
//! **正确做法**：敏感数据应在**源头**控制，不进入工具返回；本模块仅作
//! "兜底擦除已知的明文凭证格式"，降低误泄漏面。切勿依赖本模块作为唯一防护。
//!
//! ## 关于替换后文本长度
//!
//! 替换标记（如 `[REDACTED:URL_WITH_CREDS]`，25 字节）可能比最小匹配
//! （如 `a://b:c@d`，8 字节）更长，**脱敏后文本可能增长**。本模块在
//! [`crate::tools::truncating`] 的"脱敏 → 截断"管线中位于截断之前，
//! 后续硬截断会吸收这一增量，故整体输出仍受字节预算约束。
//!
//! 归属：应用层（`src/tools/`）。作为 `TruncatingToolset` 输出管线的第一环节，
//! 与 [`crate::tools::truncating`] 强耦合，不涉及具体业务概念。

use std::borrow::Cow;
use std::sync::OnceLock;

use adk_rust::guardrail::{PiiRedactor, PiiType};
use regex::Regex;
use serde_json::Value;

/// 一条脱敏规则：名称 + 编译好的正则 + 替换标记
struct SecretPattern {
    #[allow(dead_code)]
    name: &'static str,
    regex: Regex,
    replacement: &'static str,
}

/// 取全局缓存的运维密钥正则表（首次调用时编译，之后零开销）
fn secret_patterns() -> &'static [SecretPattern] {
    static PATTERNS: OnceLock<Vec<SecretPattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // INVARIANT: 每条 pattern 均为字面量字符串，且配套单元测试 `secret_patterns_compile`
        // 已在编译期/测试期验证可被 `regex::Regex::new` 接受，故此处 expect 不会 panic。
        let mk = |name: &'static str, pat: &'static str, replacement: &'static str| SecretPattern {
            name,
            regex: Regex::new(pat).expect("invalid secret regex pattern"),
            replacement,
        };
        vec![
            // ── 云平台 / API 凭证 ──
            mk(
                "aws_access_key_id",
                r"\bAKIA[0-9A-Z]{16}\b",
                "[REDACTED:AWS_AKID]",
            ),
            mk(
                "aws_secret_key",
                r"(?i)aws.{0,20}(?:secret|sk)[^A-Za-z0-9/+=]{0,5}([A-Za-z0-9/+=]{40})\b",
                "[REDACTED:AWS_SECRET]",
            ),
            mk(
                "anthropic_key",
                r"\bsk-ant-[A-Za-z0-9_-]{20,}",
                "[REDACTED:ANTHROPIC_KEY]",
            ),
            mk("openai_key", r"\bsk-[A-Za-z0-9_-]{20,}", "[REDACTED:OPENAI_KEY]"),
            mk(
                "github_token",
                r"\bgh[pousr]_[A-Za-z0-9]{36,}",
                "[REDACTED:GITHUB_TOKEN]",
            ),
            mk(
                "gitlab_token",
                // 量词 {20,}：旧格式精确 20 字符，新格式更长；用 ≥ 避免尾部明文残留
                r"\bglpat-[A-Za-z0-9_-]{20,}",
                "[REDACTED:GITLAB_TOKEN]",
            ),
            mk(
                "slack_token",
                r"\bxox[baprs]-[A-Za-z0-9-]{10,}",
                "[REDACTED:SLACK_TOKEN]",
            ),
            mk(
                "stripe_key",
                r"\bsk_(?:live|test)_[A-Za-z0-9]{16,}",
                "[REDACTED:STRIPE_KEY]",
            ),
            mk(
                "bearer_token",
                r"(?i)\bBearer\s+[A-Za-z0-9\-._~+/]+=*",
                "Bearer [REDACTED:TOKEN]",
            ),
            mk(
                "jwt",
                r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
                "[REDACTED:JWT]",
            ),
            mk(
                "pem_private_key",
                r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY-----",
                "[REDACTED:PEM_KEY]",
            ),
            mk(
                "url_with_credentials",
                r#"\b[a-z][a-z0-9+\-.]*://[^:@/\s]+:[^@/\s]+@[^/\s"<>]+"#,
                "[REDACTED:URL_WITH_CREDS]",
            ),
            mk(
                "password_assignment",
                // regex crate 不支持反向引用，故用 alternation 分别匹配
                // 单引号 / 双引号 / 裸值三种形式
                r#"(?i)(?:password|passwd|pwd)\s*[=:]\s*(?:'[^']{4,}'|"[^"]{4,}"|[^\s;'"]{4,})"#,
                "[REDACTED:PASSWORD]",
            ),
            mk(
                "secret_token_assignment",
                // token/secret/api_key 等:值要求 12+ 字符(真实令牌都很长),
                // 避免误伤代码里 token="abc" 这类短变量。覆盖配置文件里的
                // token: / api_key: / client_secret = 等(如 InfluxDB token、云 API key)。
                r#"(?i)(?:token|secret|api[_-]?key|access[_-]?key|auth[_-]?token|client[_-]?secret)\s*[=:]\s*(?:'[^']{12,}'|"[^"]{12,}"|[^\s;'"]{12,})"#,
                "[REDACTED:SECRET]",
            ),
            // ── 网络运维特有凭证 ──
            mk(
                "cisco_type7",
                r"(?i)(?:password|enable\s+password|username\s+\S+\s+password)\s+7\s+[0-9A-Fa-f]{4,}",
                "[REDACTED:CISCO_TYPE7]",
            ),
            mk(
                "cisco_type5",
                r"\$1\$[A-Za-z0-9./]{0,8}\$[A-Za-z0-9./]{22}",
                "[REDACTED:CISCO_TYPE5]",
            ),
            mk(
                "snmp_community",
                r"(?i)\bsnmp-server\s+community(?:-string)?\s+[A-Za-z0-9_]{4,}",
                "[REDACTED:SNMP_COMMUNITY]",
            ),
            mk(
                "juniper_encrypted",
                r"\$9\$[A-Za-z0-9-]{20,}",
                "[REDACTED:JUNIPER_PW]",
            ),
            mk(
                "sshpass",
                r"(?i)\bsshpass\s+-p\s+\S+",
                "[REDACTED:SSHPASS]",
            ),
        ]
    })
}

/// 取全局缓存的 PII 脱敏器（仅 Email/Phone，故意排除 IpAddress）
fn pii_redactor() -> &'static PiiRedactor {
    static REDACTOR: OnceLock<PiiRedactor> = OnceLock::new();
    REDACTOR.get_or_init(|| PiiRedactor::with_types(&[PiiType::Email, PiiType::Phone]))
}

/// 对单段文本依次应用凭证脱敏 + PII 脱敏
///
/// **执行顺序**：凭证先于 PII。原因见模块文档：URL 的 `user:pass@host`
/// 结构会被 PiiRedactor 误判为 email，必须先擦除。凭证替换标记（如
/// `[REDACTED:XXX]`）不含 `@`，不会干扰后续 email 识别。
///
/// 空字符串直接返回，避免无谓的正则匹配开销。凭证阶段使用 [`Cow`]：
/// 全部规则零命中时复用原借用，避免无谓的整段克隆（热路径优化）。
pub(crate) fn redact_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    // [1][2] 云/API 凭证 + 网络运维凭证（含 URL 嵌入凭证）
    let mut secrets: Cow<str> = Cow::Borrowed(text);
    for p in secret_patterns() {
        if p.regex.is_match(&secrets) {
            // 命中时 replace_all 实际返回 Owned（已分配新串），into_owned 对 Owned 是 no-op，
            // 但能打破"返回值借用 secrets"的类型约束，从而允许赋值回 secrets（零额外开销）。
            secrets = Cow::Owned(p.regex.replace_all(&secrets, p.replacement).into_owned());
        }
    }
    // [3] 通用 PII：仅 Email/Phone（IP 是业务核心数据，故意不脱敏）
    let (result, _) = pii_redactor().redact(&secrets);
    result
}

/// 递归脱敏 JSON `Value` 中的所有字符串字段
///
/// - `String` → 脱敏后的字符串
/// - `Array`  → 逐元素递归
/// - `Object` → 逐值递归（键名不脱敏，避免破坏 JSON 结构）
/// - 其他类型原样返回
///
/// 内置递归深度保护 [`MAX_REDACT_DEPTH`]：对抗性深层嵌套 JSON 不会爆栈，
/// 超过深度的子树原样返回（脱敏是 best-effort，见模块安全声明）。
pub(crate) fn redact_secrets(value: Value) -> Value {
    redact_secrets_inner(value, 0)
}

/// 递归深度上限：足以容纳任何合理的工具返回结构，同时防御恶意深层嵌套
const MAX_REDACT_DEPTH: u32 = 64;

fn redact_secrets_inner(value: Value, depth: u32) -> Value {
    if depth >= MAX_REDACT_DEPTH {
        return value;
    }
    match value {
        Value::String(s) => Value::String(redact_text(&s)),
        Value::Array(arr) => Value::Array(
            arr.into_iter()
                .map(|v| redact_secrets_inner(v, depth + 1))
                .collect(),
        ),
        Value::Object(obj) => Value::Object(
            obj.into_iter()
                .map(|(k, v)| (k, redact_secrets_inner(v, depth + 1)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_patterns_compile() {
        // 编译期校验：所有正则模式合法（保证 OnceLock 初始化不会 panic）
        assert!(!secret_patterns().is_empty());
    }

    #[test]
    fn redacts_email() {
        let out = redact_text("联系管理员 admin@example.com 或 noc@isp.cn");
        assert!(out.contains("[EMAIL REDACTED]"));
        assert!(!out.contains("admin@example.com"));
    }

    #[test]
    fn redacts_password_and_token_assignments() {
        // 配置文件常见形态:password= / password: / token: 都要脱敏
        let ini = "PSQL_PASSWORD = '12839cb24eb16b500b410b0f1f92b3d521648a36aa0ecdb7b4999bf1834baf12'\nREDIS_PASSWORD = 'Nsf0cus*crc'";
        let o1 = redact_text(ini);
        assert!(o1.contains("[REDACTED:PASSWORD]"));
        assert!(!o1.contains("12839cb24eb16b"));
        assert!(!o1.contains("Nsf0cus*crc"));
        let yml = "password: MarVelNet@123\ntoken: lhvuTDlV9VFrSkV0jg6fHTR89v2JV-0I8JC6YpW8EySI3IAFLHqY76F4agPcsOgXIZojtZgYTEKELW9n9euI1A==";
        let o2 = redact_text(yml);
        assert!(o2.contains("[REDACTED:PASSWORD]"));
        assert!(o2.contains("[REDACTED:SECRET]"));
        assert!(!o2.contains("MarVelNet@123"));
        assert!(!o2.contains("lhvuTDlV9VFrSkV0"));
        // 短 token(代码里 token="abc" 这类)不应误伤
        assert_eq!(redact_text(r#"token = "abc""#), r#"token = "abc""#);
    }

    #[test]
    fn redacts_phone() {
        let out = redact_text("值班电话 555-123-4567");
        assert!(out.contains("[PHONE REDACTED]"));
    }

    #[test]
    fn preserves_ip_address() {
        // IP 是网络运维核心业务数据，绝不能脱敏
        let out = redact_text("目标设备 10.20.30.40 / 192.168.1.1");
        assert!(out.contains("10.20.30.40"));
        assert!(out.contains("192.168.1.1"));
    }

    #[test]
    fn redacts_aws_access_key() {
        let out = redact_text("AWS_KEY=AKIAIOSFODNN7EXAMPLE");
        assert!(out.contains("[REDACTED:AWS_AKID]"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_openai_and_anthropic_keys() {
        let anthropic = format!("sk-ant-api03-{}", "A".repeat(40));
        let openai = format!("sk-{}", "B".repeat(30));
        let out = redact_text(&format!("{} and {}", anthropic, openai));
        assert!(out.contains("[REDACTED:ANTHROPIC_KEY]"));
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn redacts_github_and_gitlab_tokens() {
        let github = format!("ghp_{}", "a".repeat(36));
        // 回归 B3：旧格式精确 20 字符
        let gitlab_old = format!("glpat-{}", "x".repeat(20));
        // 回归 B3：新格式更长，{20} 量词会泄漏尾部明文
        let gitlab_new = format!("glpat-{}", "y".repeat(40));
        let out = redact_text(&format!("{} {} {}", github, gitlab_old, gitlab_new));
        assert!(out.contains("[REDACTED:GITHUB_TOKEN]"));
        assert!(out.contains("[REDACTED:GITLAB_TOKEN]"));
        // 长格式尾部不得残留明文
        assert!(!out.contains(&"y".repeat(40)));
    }

    #[test]
    fn redacts_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_text(jwt);
        assert!(out.contains("[REDACTED:JWT]"));
    }

    #[test]
    fn redacts_url_with_credentials() {
        let out = redact_text("curl https://admin:s3cret@device.local/api");
        assert!(out.contains("[REDACTED:URL_WITH_CREDS]"));
        assert!(!out.contains("s3cret"));
    }

    #[test]
    fn redacts_password_assignment() {
        let out = redact_text(r#"password = "supersecret123""#);
        assert!(out.contains("[REDACTED:PASSWORD]"));
        assert!(!out.contains("supersecret123"));
    }

    #[test]
    fn redacts_cisco_type7() {
        let out = redact_text("enable password 7 060506324F41594B021F2535765478");
        assert!(out.contains("[REDACTED:CISCO_TYPE7]"));
    }

    #[test]
    fn redacts_snmp_community() {
        let out = redact_text("snmp-server community public RO");
        assert!(out.contains("[REDACTED:SNMP_COMMUNITY]"));
    }

    #[test]
    fn redacts_sshpass() {
        let out = redact_text("sshpass -p MyP@ssw0rd ssh user@host");
        assert!(out.contains("[REDACTED:SSHPASS]"));
        assert!(!out.contains("MyP@ssw0rd"));
    }

    #[test]
    fn redacts_pem_block_multiline() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----";
        let out = redact_text(pem);
        assert!(out.contains("[REDACTED:PEM_KEY]"));
        assert!(!out.contains("MIIEpAIBAAKCAQEA"));
    }

    #[test]
    fn redacts_value_recursively() {
        let v = json!({
            "device": "r1",
            "contacts": ["admin@example.com", "555-000-1234"],
            "config": { "snmp": "snmp-server community private RO" }
        });
        let out = redact_secrets(v);
        let s = serde_json::to_string(&out).expect("serialize ok");
        assert!(s.contains("[EMAIL REDACTED]"));
        assert!(s.contains("[PHONE REDACTED]"));
        assert!(s.contains("[REDACTED:SNMP_COMMUNITY]"));
        // 键名保留
        assert!(s.contains("\"device\""));
        assert!(s.contains("\"r1\""));
    }

    #[test]
    fn empty_text_short_circuits() {
        assert_eq!(redact_text(""), "");
    }

    #[test]
    fn non_string_value_passthrough() {
        assert_eq!(redact_secrets(json!(42)), json!(42));
        assert_eq!(redact_secrets(json!(true)), json!(true));
        assert_eq!(redact_secrets(json!(null)), json!(null));
    }

    #[test]
    fn redact_secrets_depth_guard_no_overflow() {
        // 回归 B9：构造远超 MAX_REDACT_DEPTH 的深层嵌套，不得爆栈
        let mut leaf = json!("admin@example.com");
        for _ in 0..(MAX_REDACT_DEPTH + 50) {
            leaf = json!({ "nested": leaf });
        }
        // 能正常返回即说明深度保护生效（不 panic / 不栈溢出）
        let out = redact_secrets(leaf);
        let s = serde_json::to_string(&out).expect("serialize ok");
        // 最内层可能被脱敏也可能因超深原样保留，二者均可接受；关键是没爆栈
        let _ = s;
    }
}
