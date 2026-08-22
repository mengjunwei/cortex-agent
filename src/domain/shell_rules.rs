//! Shell 命令权限规则 — 用户自定义 allow/deny/ask 模式匹配
//!
//! 存储在 `shell_rules` 表中,启动时加载到内存缓存。
//! `shell_command::execute_shell_command()` 在查硬编码 safelist/dangerous 之前先查此规则集。

use std::sync::Arc;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::infra::db::{DbPool, DbPooledConnection};

/// 决策类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleDecision {
    /// 自动放行（等同 safelist）
    Allow = 0,
    /// 自动阻断（等同 dangerous）
    Deny = 1,
    /// 需要用户审批（等同 needs_prompt）
    Ask = 2,
}

impl RuleDecision {
    pub fn from_i16(v: i16) -> Self {
        match v {
            0 => Self::Allow,
            1 => Self::Deny,
            _ => Self::Ask,
        }
    }
    pub fn as_i16(self) -> i16 {
        self as i16
    }
}

/// 单条规则
#[derive(Debug, Clone, Serialize)]
pub struct ShellRule {
    pub id: String,
    pub pattern: String,
    pub decision: RuleDecision,
    pub priority: i32,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, QueryableByName)]
struct RuleRow {
    #[diesel(sql_type = sql_types::VarChar)]
    id: String,
    #[diesel(sql_type = sql_types::VarChar)]
    pattern: String,
    #[diesel(sql_type = sql_types::SmallInt)]
    decision: i16,
    #[diesel(sql_type = sql_types::Int4)]
    priority: i32,
    #[diesel(sql_type = sql_types::SmallInt)]
    enabled: i16,
    #[diesel(sql_type = sql_types::Timestamptz)]
    created_at: chrono::DateTime<chrono::Utc>,
}

/// 权限规则存储 + 内存缓存
pub struct ShellRuleStore {
    pool: DbPool,
    cache: RwLock<Vec<(String, RuleDecision)>>,
}

impl ShellRuleStore {
    pub async fn new(pool: DbPool) -> anyhow::Result<Arc<Self>> {
        let store = Arc::new(Self {
            pool,
            cache: RwLock::new(Vec::new()),
        });
        store.refresh_cache().await?;
        Ok(store)
    }

    async fn get_conn(&self) -> anyhow::Result<DbPooledConnection> {
        self.pool
            .get()
            .await
            .map_err(|e| anyhow::anyhow!("DB 连接获取失败: {e}"))
    }

    pub async fn refresh_cache(&self) -> anyhow::Result<()> {
        let mut c = self.get_conn().await?;
        let rows: Vec<RuleRow> = diesel::sql_query(
            r#"SELECT id, pattern, decision, priority, enabled, created_at
               FROM shell_rules
               WHERE enabled = 1
               ORDER BY priority DESC, created_at ASC"#,
        )
        .get_results(&mut c)
        .await?;

        let rules: Vec<(String, RuleDecision)> = rows
            .into_iter()
            .map(|r| (r.pattern, RuleDecision::from_i16(r.decision)))
            .collect();

        tracing::info!("[shell_rules] 缓存刷新: {} 条活跃规则", rules.len());
        *self.cache.write().await = rules;
        Ok(())
    }

    pub async fn list(&self) -> anyhow::Result<Vec<ShellRule>> {
        let mut c = self.get_conn().await?;
        let rows: Vec<RuleRow> = diesel::sql_query(
            r#"SELECT id, pattern, decision, priority, enabled, created_at
               FROM shell_rules
               ORDER BY priority DESC, created_at ASC"#,
        )
        .get_results(&mut c)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ShellRule {
                id: r.id,
                pattern: r.pattern,
                decision: RuleDecision::from_i16(r.decision),
                priority: r.priority,
                enabled: r.enabled == 1,
                created_at: r.created_at.to_rfc3339(),
            })
            .collect())
    }

    pub async fn create(
        &self,
        pattern: &str,
        decision: RuleDecision,
        priority: i32,
    ) -> anyhow::Result<ShellRule> {
        let id = uuid::Uuid::now_v7().to_string();
        let now = chrono::Utc::now();
        let mut c = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO shell_rules (id, pattern, decision, priority, enabled, created_at)
               VALUES ($1, $2, $3, $4, 1, $5)"#,
        )
        .bind::<sql_types::VarChar, _>(&id)
        .bind::<sql_types::VarChar, _>(pattern)
        .bind::<sql_types::SmallInt, _>(decision.as_i16())
        .bind::<sql_types::Int4, _>(priority)
        .bind::<sql_types::Timestamptz, _>(now)
        .execute(&mut c)
        .await?;

        drop(c);
        self.refresh_cache().await?;

        Ok(ShellRule {
            id,
            pattern: pattern.to_string(),
            decision,
            priority,
            enabled: true,
            created_at: now.to_rfc3339(),
        })
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let mut c = self.get_conn().await?;
        let count: usize = diesel::sql_query("DELETE FROM shell_rules WHERE id = $1")
            .bind::<sql_types::VarChar, _>(id)
            .execute(&mut c)
            .await?;
        drop(c);
        if count > 0 {
            self.refresh_cache().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 匹配命令 — 遍历缓存,返回第一个命中的决策
    pub async fn match_command(&self, command: &str) -> Option<RuleDecision> {
        let cache = self.cache.read().await;
        for (pattern, decision) in cache.iter() {
            if glob_match(pattern, command) {
                return Some(*decision);
            }
        }
        None
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_helper(pattern.as_bytes(), 0, text.as_bytes(), 0)
}

fn glob_match_helper(pat: &[u8], pi: usize, text: &[u8], ti: usize) -> bool {
    if pi == pat.len() {
        return ti == text.len();
    }
    match pat[pi] {
        b'*' => {
            if glob_match_helper(pat, pi + 1, text, ti) {
                return true;
            }
            if ti < text.len() && glob_match_helper(pat, pi, text, ti + 1) {
                return true;
            }
            false
        }
        b'?' => ti < text.len() && glob_match_helper(pat, pi + 1, text, ti + 1),
        c => ti < text.len() && text[ti] == c && glob_match_helper(pat, pi + 1, text, ti + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn glob_star() {
        assert!(glob_match("git*", "git status"));
        assert!(glob_match("git*", "git push origin main"));
        assert!(glob_match("git*", "git"));
        assert!(!glob_match("git*", "cat file"));
    }

    #[test]
    fn glob_star_middle() {
        assert!(glob_match("rm *", "rm -rf /tmp"));
        assert!(glob_match("pip install*", "pip install pandas"));
    }

    #[test]
    fn glob_question() {
        assert!(glob_match("h?llo", "hello"));
        assert!(glob_match("h?llo", "hallo"));
        assert!(!glob_match("h?llo", "heo"));
    }

    #[test]
    fn glob_case_sensitive() {
        assert!(glob_match("git*", "git status"));
        assert!(!glob_match("git*", "GIT STATUS"));
    }

    #[test]
    fn glob_empty_pattern() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn rule_decision_roundtrip() {
        assert_eq!(RuleDecision::from_i16(0), RuleDecision::Allow);
        assert_eq!(RuleDecision::from_i16(1), RuleDecision::Deny);
        assert_eq!(RuleDecision::from_i16(2), RuleDecision::Ask);
    }
}
