//! 助手数据存储层（diesel-async）。
//!
//! 范式同 [`crate::domain::auth::store`]：私有 `new_id`/`get_conn`、SMALLINT 枚举、
//! `enabled_tools` 以 TEXT 存 JSON（架构 §8.2）；建表 DDL 见 `migrations/schema.sql`。
//! 事务用手动 BEGIN/COMMIT/ROLLBACK（架构 §8.6，见计划 A10）。

use std::collections::HashMap;
use std::sync::Arc;

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;
use uuid::Uuid;

use crate::domain::assistant::enums::{AgentType, AssistantKind, Visibility};
use crate::domain::assistant::models::{Assistant, AssistantRow, CustomAssistantInput};
use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::{Store, new_id};

pub struct AssistantStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for AssistantStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

/// 删除助手前的引用影响预检结果（只读计数，供前端确认框展示）
#[derive(Debug, Clone)]
pub struct AssistantDeletionImpact {
    /// 绑定该助手的会话数（session_settings.assistant_id），删除时将解绑置 NULL、会话回退默认助手
    pub sessions: i64,
    /// 该助手的助手级记忆数（memories scope=1），删除时将降级为用户级（记忆不丢失）
    pub memories: i64,
    /// 关联该助手的记忆建议数（memory_proposals），删除时一并清理
    pub memory_proposals: i64,
}

/// 删除助手并级联清理引用的执行结果
#[derive(Debug, Clone)]
pub struct AssistantDeletionCleanup {
    /// 主实体是否删除成功
    pub deleted: bool,
    /// 解除绑定的会话数
    pub sessions_unbound: usize,
    /// 降级为用户级的记忆数
    pub memories_downgraded: usize,
    /// 清理的记忆建议数
    pub proposals_removed: usize,
}

impl AssistantStore {
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        let store = Arc::new(Self { pool });
        store.seed_builtin().await?;
        tracing::info!("[assistant] store initialized");
        Ok(store)
    }

    /// 生成 8 位 share_token（数字+字母，避免易混淆字符 0/O/1/I/l）
    /// 熵源：UUIDv7 随机位 + SystemTime 纳秒 + 进程内原子计数器，经 xorshift 混合，
    /// 比 UUIDv7 高位（时间戳）更不可预测
    fn new_share_token() -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        // 混合三路熵源
        let mut state: u64 = {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E3779B97F4A7C15);
            let uuid_rand = (Uuid::now_v7().as_u128() as u64).wrapping_mul(0xFF51AFD7ED558CCD);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            t ^ uuid_rand ^ seq.rotate_left(17)
        };
        if state == 0 {
            state = 0x9E3779B97F4A7C15;
        }
        let mut s = String::with_capacity(8);
        for _ in 0..8 {
            // xorshift64 推进，保证每位都被混合
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            s.push(ALPHABET[(state % ALPHABET.len() as u64) as usize] as char);
        }
        s
    }

    fn encode_tools(tools: &[String]) -> String {
        serde_json::to_string(tools).unwrap_or_else(|_| "[]".to_string())
    }

    fn encode_mcps(mcps: &[String]) -> String {
        serde_json::to_string(mcps).unwrap_or_else(|_| "[]".to_string())
    }

    pub async fn insert(&self, a: &Assistant) -> Result<String, AppError> {
        let mut c = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO assistants
               (id,name,description,avatar,kind,agent_type,system_prompt,model_id,
                 temperature,top_p,max_tokens,thinking_level,enabled_tools,knowledge_enabled,kb_instance_id,enabled_mcps,greeting,
                 share_token,fork_count,creator,visibility,sort_order)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)"#,
        )
        .bind::<sql_types::Text, _>(&a.id)
        .bind::<sql_types::Text, _>(&a.name)
        .bind::<sql_types::Text, _>(&a.description)
        .bind::<sql_types::Text, _>(&a.avatar)
        .bind::<sql_types::Int2, _>(a.kind.as_i16())
        .bind::<sql_types::Int2, _>(a.agent_type.as_i16())
        .bind::<sql_types::Text, _>(&a.system_prompt)
        .bind::<sql_types::Text, _>(&a.model_id)
        .bind::<sql_types::Nullable<sql_types::Float8>, _>(a.temperature)
        .bind::<sql_types::Nullable<sql_types::Float8>, _>(a.top_p)
        .bind::<sql_types::Nullable<sql_types::Int4>, _>(a.max_tokens)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(a.thinking_level.as_deref())
        .bind::<sql_types::Text, _>(Self::encode_tools(&a.enabled_tools))
        .bind::<sql_types::Bool, _>(a.knowledge_enabled)
        .bind::<sql_types::Nullable<sql_types::Varchar>, _>(&a.kb_instance_id)
        .bind::<sql_types::Text, _>(Self::encode_mcps(&a.enabled_mcps))
        .bind::<sql_types::Text, _>(&a.greeting)
        .bind::<sql_types::Text, _>(&a.share_token)
        .bind::<sql_types::Int4, _>(a.fork_count)
        .bind::<sql_types::Text, _>(&a.creator)
        .bind::<sql_types::Int2, _>(a.visibility.as_i16())
        .bind::<sql_types::Int4, _>(a.sort_order)
        .execute(&mut c)
        .await?;
        Ok(a.id.clone())
    }

    pub async fn list_all(&self) -> Result<Vec<Assistant>, AppError> {
        let mut c = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT * FROM assistants ORDER BY kind ASC, sort_order ASC, updated_at DESC",
        )
        .get_results::<AssistantRow>(&mut c)
        .await?;
        Ok(rows.into_iter().map(Assistant::from).collect())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Assistant>, AppError> {
        let mut c = self.get_conn().await?;
        let rows: Vec<AssistantRow> = diesel::sql_query("SELECT * FROM assistants WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .get_results(&mut c)
            .await?;
        Ok(rows.into_iter().next().map(Assistant::from))
    }

    /// 批量查助手（会话列表注入助手名/类型用，避免 N+1）
    pub async fn get_batch(&self, ids: &[String]) -> Result<HashMap<String, Assistant>, AppError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut c = self.get_conn().await?;
        let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let rows: Vec<AssistantRow> =
            diesel::sql_query("SELECT * FROM assistants WHERE id = ANY($1)")
                .bind::<sql_types::Array<sql_types::Text>, _>(&id_refs)
                .get_results(&mut c)
                .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let a: Assistant = r.into();
                (a.id.clone(), a)
            })
            .collect())
    }

    /// 广场列表：visibility > Private 的全部助手（即 Shared + Builtin）
    pub async fn list_public(&self) -> Result<Vec<Assistant>, AppError> {
        let mut c = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT * FROM assistants
               WHERE visibility > 0
               ORDER BY visibility DESC, fork_count DESC, updated_at DESC"#,
        )
        .get_results::<AssistantRow>(&mut c)
        .await?;
        Ok(rows.into_iter().map(Assistant::from).collect())
    }

    /// 按 share_token 查询（用于口令 fork）；未设置 token 的助手不可达
    pub async fn get_by_token(&self, token: &str) -> Result<Option<Assistant>, AppError> {
        if token.is_empty() {
            return Ok(None);
        }
        let mut c = self.get_conn().await?;
        let rows: Vec<AssistantRow> = diesel::sql_query(
            "SELECT * FROM assistants WHERE share_token = $1 AND share_token <> ''",
        )
        .bind::<sql_types::Text, _>(token)
        .get_results(&mut c)
        .await?;
        Ok(rows.into_iter().next().map(Assistant::from))
    }

    /// 创建自定义助手；ID 在 store 内部生成（A5：handler 不接触 ID）
    pub async fn create_custom(
        &self,
        input: &CustomAssistantInput,
        creator: &str,
    ) -> Result<String, AppError> {
        let a = Assistant {
            id: new_id(),
            name: input.name.clone(),
            description: input.description.clone(),
            avatar: if input.avatar.is_empty() {
                "🤖".to_string()
            } else {
                input.avatar.clone()
            },
            kind: AssistantKind::Custom,
            agent_type: AgentType::Custom,
            system_prompt: input.system_prompt.clone(),
            model_id: input.model_id.clone(),
            temperature: input.temperature,
            top_p: input.top_p,
            max_tokens: input.max_tokens,
            thinking_level: input.thinking_level.clone(),
            enabled_tools: input.enabled_tools.clone(),
            knowledge_enabled: input.knowledge_enabled,
            kb_instance_id: input.kb_instance_id.clone(),
            enabled_mcps: input.enabled_mcps.clone(),
            greeting: input.greeting.clone(),
            share_token: String::new(),
            fork_count: 0,
            creator: creator.to_string(),
            visibility: input.visibility,
            sort_order: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        self.insert(&a).await
    }

    /// 更新自定义助手；返回是否命中（kind=Custom 才允许写）
    pub async fn update_custom(
        &self,
        id: &str,
        input: &CustomAssistantInput,
    ) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let aff = diesel::sql_query(
            r#"UPDATE assistants SET
                 name=$2, description=$3, avatar=$4, system_prompt=$5,
                 model_id=$6, temperature=$7, top_p=$8, max_tokens=$9,
                 thinking_level=$10, enabled_tools=$11, knowledge_enabled=$12, greeting=$13,
                 enabled_mcps=$14, visibility=$15, kb_instance_id=$16, updated_at=NOW()
               WHERE id=$1 AND kind=1"#,
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(input.name.trim())
        .bind::<sql_types::Text, _>(input.description.trim())
        .bind::<sql_types::Text, _>(if input.avatar.is_empty() {
            "🤖"
        } else {
            input.avatar.trim()
        })
        .bind::<sql_types::Text, _>(&input.system_prompt)
        .bind::<sql_types::Text, _>(input.model_id.trim())
        .bind::<sql_types::Nullable<sql_types::Float8>, _>(input.temperature)
        .bind::<sql_types::Nullable<sql_types::Float8>, _>(input.top_p)
        .bind::<sql_types::Nullable<sql_types::Int4>, _>(input.max_tokens)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(input.thinking_level.as_deref())
        .bind::<sql_types::Text, _>(Self::encode_tools(&input.enabled_tools))
        .bind::<sql_types::Bool, _>(input.knowledge_enabled)
        .bind::<sql_types::Text, _>(&input.greeting)
        .bind::<sql_types::Text, _>(Self::encode_mcps(&input.enabled_mcps))
        .bind::<sql_types::Int2, _>(input.visibility.as_i16())
        .bind::<sql_types::Nullable<sql_types::Varchar>, _>(input.kb_instance_id.as_deref())
        .execute(&mut c)
        .await?;
        Ok(aff > 0)
    }

    /// 设置助手绑定的知识库实例（builtin/custom 均允许，不检查 kind）
    ///
    /// 内置助手整体只读（system_prompt/model 等不可改），但 `kb_instance_id` 是运行时配置，
    /// 需要单独可改，故此方法不附带 `kind=1` 条件。
    /// 返回是否命中（id 存在即更新）。
    pub async fn set_kb_instance(
        &self,
        id: &str,
        kb_instance_id: Option<&str>,
    ) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let aff = diesel::sql_query(
            "UPDATE assistants SET kb_instance_id=$2, updated_at=NOW() WHERE id=$1",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Nullable<sql_types::Varchar>, _>(kb_instance_id)
        .execute(&mut c)
        .await?;
        Ok(aff > 0)
    }

    /// 删除自定义助手（只删 kind=Custom，内置不可删）
    pub async fn delete_custom(&self, id: &str) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let aff = diesel::sql_query("DELETE FROM assistants WHERE id=$1 AND kind=1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut c)
            .await?;
        Ok(aff > 0)
    }

    /// 预检：统计删除该助手会牵连的引用（只读，不执行删除）。
    ///
    /// 用于删除确认前的「影响清单」——让用户在确认前知道删除会波及哪些数据。
    pub async fn impact_of_delete(&self, id: &str) -> Result<AssistantDeletionImpact, AppError> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = sql_types::BigInt)]
            sessions: i64,
            #[diesel(sql_type = sql_types::BigInt)]
            memories: i64,
            #[diesel(sql_type = sql_types::BigInt)]
            memory_proposals: i64,
        }
        let mut c = self.get_conn().await?;
        let row = diesel::sql_query(
            r#"SELECT
                 (SELECT COUNT(*) FROM session_settings WHERE assistant_id = $1) AS sessions,
                 (SELECT COUNT(*) FROM memories WHERE assistant_id = $1 AND scope = 1) AS memories,
                 (SELECT COUNT(*) FROM memory_proposals WHERE assistant_id = $1) AS memory_proposals"#,
        )
        .bind::<sql_types::Text, _>(id)
        .get_result::<Row>(&mut c)
        .await?;
        Ok(AssistantDeletionImpact {
            sessions: row.sessions,
            memories: row.memories,
            memory_proposals: row.memory_proposals,
        })
    }

    /// 删除自定义助手并级联清理所有引用（单个事务内，任一步失败整体 ROLLBACK）。
    ///
    /// 引用清理策略——保留引用方主体，只解绑指针：
    /// - `session_settings.assistant_id`：置 NULL → 会话回退默认助手
    /// - `memories`(scope=1)：降级为用户级(scope=0, assistant_id=NULL) → 记忆不丢失，继续按用户注入
    /// - `memory_proposals`：删除关联该助手的提议（未确认的临时建议）
    /// - `assistants`：最后删主实体（kind=1 守门，内置不可删）
    pub async fn delete_with_cleanup(
        &self,
        id: &str,
    ) -> Result<AssistantDeletionCleanup, AppError> {
        let mut c = self.get_conn().await?;
        diesel::sql_query("BEGIN").execute(&mut c).await?;

        let tx: Result<AssistantDeletionCleanup, AppError> = async {
            let sessions_unbound = diesel::sql_query(
                "UPDATE session_settings SET assistant_id = NULL, updated_at = NOW() WHERE assistant_id = $1",
            )
            .bind::<sql_types::Text, _>(id)
            .execute(&mut c)
            .await?;

            let memories_downgraded = diesel::sql_query(
                r#"UPDATE memories
                   SET scope = 0, assistant_id = NULL, updated_at = NOW()
                   WHERE assistant_id = $1 AND scope = 1"#,
            )
            .bind::<sql_types::Text, _>(id)
            .execute(&mut c)
            .await?;

            let proposals_removed = diesel::sql_query(
                "DELETE FROM memory_proposals WHERE assistant_id = $1",
            )
            .bind::<sql_types::Text, _>(id)
            .execute(&mut c)
            .await?;

            let aff = diesel::sql_query("DELETE FROM assistants WHERE id = $1 AND kind = 1")
                .bind::<sql_types::Text, _>(id)
                .execute(&mut c)
                .await?;

            Ok(AssistantDeletionCleanup {
                deleted: aff > 0,
                sessions_unbound,
                memories_downgraded,
                proposals_removed,
            })
        }
        .await;

        match tx {
            Ok(res) => {
                diesel::sql_query("COMMIT").execute(&mut c).await?;
                Ok(res)
            }
            Err(e) => {
                // 尽力回滚；忽略回滚本身的错误（原错误优先上报）
                let _ = diesel::sql_query("ROLLBACK").execute(&mut c).await;
                Err(e)
            }
        }
    }

    /// 复制内置助手 → 自定义副本；返回新 id
    ///
    /// 复制策略（设计 §10.2）：
    /// - `kind` 强制改为 `Custom`、`agent_type` 改为 `Custom`
    /// - `visibility` 强制改为 `Private`（副本默认私有）
    /// - `share_token` 清空、`fork_count` 重置为 0
    /// - `creator` 设为调用者、`name` 追加" 副本"
    pub async fn duplicate_builtin(&self, src_id: &str, creator: &str) -> Result<String, AppError> {
        let src = self
            .get(src_id)
            .await?
            .ok_or_else(|| AppError::BusinessError("助手不存在".into()))?;
        // 内置助手不允许复制（只读，禁止任何修改操作）
        if src.kind == AssistantKind::Builtin {
            return Err(AppError::BusinessError("内置助手不支持复制".into()));
        }
        let mut copy = src;
        copy.id = new_id();
        copy.name = format!("{} 副本", copy.name);
        copy.kind = AssistantKind::Custom;
        copy.agent_type = AgentType::Custom;
        copy.visibility = Visibility::Private;
        copy.share_token = String::new();
        copy.fork_count = 0;
        copy.creator = creator.to_string();
        copy.sort_order = 0;
        copy.created_at = chrono::Utc::now();
        copy.updated_at = chrono::Utc::now();
        self.insert(&copy).await
    }

    /// Fork 公开/分享助手 → 自定义副本；返回新 id。
    ///
    /// 与 [`duplicate_builtin`] 区别：源必须是 visibility != Private 的助手；
    /// fork 后会原子地 `fork_count += 1`。
    pub async fn fork(&self, src_id: &str, creator: &str) -> Result<String, AppError> {
        let mut c = self.get_conn().await?;
        diesel::sql_query("BEGIN").execute(&mut c).await?;

        let tx: Result<String, AppError> = async {
            let rows: Vec<AssistantRow> = diesel::sql_query(
                r#"SELECT * FROM assistants WHERE id=$1 AND visibility > 0 FOR UPDATE"#,
            )
            .bind::<sql_types::Text, _>(src_id)
            .get_results(&mut c)
            .await?;
            let src = rows
                .into_iter()
                .next()
                .ok_or_else(|| AppError::BusinessError("助手不存在或未公开".into()))?;
            let src: Assistant = src.into();

            // 内置助手只读，不允许 Fork（用户应通过「复制」生成可编辑的自定义副本）
            if src.kind == AssistantKind::Builtin {
                return Err(AppError::BusinessError(
                    "内置助手不支持 Fork，请使用「复制」创建自定义副本".into(),
                ));
            }

            diesel::sql_query("UPDATE assistants SET fork_count = fork_count + 1 WHERE id=$1")
                .bind::<sql_types::Text, _>(src_id)
                .execute(&mut c)
                .await?;

            let mut forked = src.clone();
            forked.id = new_id();
            forked.name = src.name.clone();
            forked.kind = AssistantKind::Custom;
            forked.agent_type = AgentType::Custom;
            forked.visibility = Visibility::Private;
            forked.share_token = String::new();
            forked.fork_count = 0;
            forked.creator = creator.to_string();
            forked.sort_order = 0;
            forked.created_at = chrono::Utc::now();
            forked.updated_at = chrono::Utc::now();

            diesel::sql_query(
                r#"INSERT INTO assistants
                   (id,name,description,avatar,kind,agent_type,system_prompt,model_id,
                    temperature,top_p,max_tokens,thinking_level,enabled_tools,knowledge_enabled,kb_instance_id,enabled_mcps,greeting,
                    share_token,fork_count,creator,visibility,sort_order)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)"#,
            )
            .bind::<sql_types::Text, _>(&forked.id)
            .bind::<sql_types::Text, _>(&forked.name)
            .bind::<sql_types::Text, _>(&forked.description)
            .bind::<sql_types::Text, _>(&forked.avatar)
            .bind::<sql_types::Int2, _>(forked.kind.as_i16())
            .bind::<sql_types::Int2, _>(forked.agent_type.as_i16())
            .bind::<sql_types::Text, _>(&forked.system_prompt)
            .bind::<sql_types::Text, _>(&forked.model_id)
            .bind::<sql_types::Nullable<sql_types::Float8>, _>(forked.temperature)
            .bind::<sql_types::Nullable<sql_types::Float8>, _>(forked.top_p)
            .bind::<sql_types::Nullable<sql_types::Int4>, _>(forked.max_tokens)
            .bind::<sql_types::Nullable<sql_types::Text>, _>(forked.thinking_level.as_deref())
            .bind::<sql_types::Text, _>(Self::encode_tools(&forked.enabled_tools))
            .bind::<sql_types::Bool, _>(forked.knowledge_enabled)
            .bind::<sql_types::Nullable<sql_types::Varchar>, _>(&forked.kb_instance_id)
            .bind::<sql_types::Text, _>(Self::encode_mcps(&forked.enabled_mcps))
            .bind::<sql_types::Text, _>(&forked.greeting)
            .bind::<sql_types::Text, _>(&forked.share_token)
            .bind::<sql_types::Int4, _>(forked.fork_count)
            .bind::<sql_types::Text, _>(&forked.creator)
            .bind::<sql_types::Int2, _>(forked.visibility.as_i16())
            .bind::<sql_types::Int4, _>(forked.sort_order)
            .execute(&mut c)
            .await?;
            Ok(forked.id)
        }
        .await;

        match tx {
            Ok(id) => {
                diesel::sql_query("COMMIT").execute(&mut c).await?;
                tracing::info!(target: "assistant", "fork src={} → new={}", src_id, id);
                Ok(id)
            }
            Err(e) => {
                let _ = diesel::sql_query("ROLLBACK").execute(&mut c).await;
                Err(e)
            }
        }
    }

    /// 设置 share_token（M8 分享）；返回新 token。若 token 已存在则返回原值。
    pub async fn ensure_share_token(&self, id: &str) -> Result<String, AppError> {
        if let Some(a) = self.get(id).await? {
            if !a.share_token.is_empty() {
                return Ok(a.share_token);
            }
            for _ in 0..5 {
                let token = Self::new_share_token();
                let mut c = self.get_conn().await?;
                let aff = diesel::sql_query(
                    "UPDATE assistants SET share_token=$2, updated_at=NOW() \
                     WHERE id=$1 AND (share_token IS NULL OR share_token='')",
                )
                .bind::<sql_types::Text, _>(id)
                .bind::<sql_types::Text, _>(&token)
                .execute(&mut c)
                .await?;
                if aff > 0 {
                    return Ok(token);
                }
                // CAS 失败：并发请求已写入 token，重新查询获取现有值，避免无谓重试
                if let Some(refreshed) = self.get(id).await? {
                    if !refreshed.share_token.is_empty() {
                        return Ok(refreshed.share_token);
                    }
                }
            }
            Err(AppError::ConflictError(
                "share_token 唯一索引冲突，重试失败".into(),
            ))
        } else {
            Err(AppError::BusinessError("助手不存在".into()))
        }
    }

    /// 关闭分享（清空 share_token，不动 visibility）
    pub async fn clear_share_token(&self, id: &str) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let aff =
            diesel::sql_query("UPDATE assistants SET share_token='', updated_at=NOW() WHERE id=$1")
                .bind::<sql_types::Text, _>(id)
                .execute(&mut c)
                .await?;
        Ok(aff > 0)
    }

    /// 内置助手 seed（幂等，与设计 §4.2 一致）
    ///
    /// ID 固定（UUIDv7 形态的占位），保证启动/重启后内置助手 ID 不变，
    /// 便于前端用固定 ID 直链 `assistant_id=<内置ID>` 创建会话。
    /// `ON CONFLICT (id) DO UPDATE` 保证字段升级（如改了 avatar）幂等生效。
    pub async fn seed_builtin(&self) -> Result<(), AppError> {
        let mut c = self.get_conn().await?;

        // 清理已废弃的内置助手（Auto/Chat 类型已移除；头脑风暴/代码助手已下线）
        diesel::sql_query(
            "DELETE FROM assistants WHERE id IN ('01950000-0000-7000-8000-000000000001','01950000-0000-7000-8000-000000000004','01950000-0000-7000-8000-000000000006')",
        )
        .execute(&mut c)
        .await?;

        // (id, name, agent_type_i16, avatar, system_prompt, greeting, sort_order)
        let seeds: &[(&str, &str, i16, &str, &str, &str, i32)] = &[
            (
                "01950000-0000-7000-8000-000000000003",
                "设备命令助手",
                2,
                "🛠️",
                "",
                "请告诉我厂商和设备类型，我会查询配置命令。",
                1,
            ),
            // 监控插件助手（...005, agent_type=4）已暂下线，不再 seed；如需恢复加回该元组即可
        ];

        for (id, name, at_i16, avatar, sp, greeting, sort_order) in seeds {
            diesel::sql_query(
                r#"INSERT INTO assistants
                   (id,name,description,avatar,kind,agent_type,system_prompt,model_id,
                    temperature,top_p,max_tokens,enabled_tools,knowledge_enabled,greeting,
                    share_token,fork_count,creator,visibility,sort_order)
                   VALUES ($1,$2,'',$3,0,$4,$5,'',NULL,NULL,NULL,'[]',FALSE,$6,'',0,'local',2,$7)
                   ON CONFLICT (id) DO UPDATE SET
                     name=EXCLUDED.name, avatar=EXCLUDED.avatar,
                     agent_type=EXCLUDED.agent_type, system_prompt=EXCLUDED.system_prompt,
                     greeting=EXCLUDED.greeting, sort_order=EXCLUDED.sort_order,
                     kind=0, visibility=2"#,
            )
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(name)
            .bind::<sql_types::Text, _>(avatar)
            .bind::<sql_types::Int2, _>(*at_i16)
            .bind::<sql_types::Text, _>(sp)
            .bind::<sql_types::Text, _>(greeting)
            .bind::<sql_types::Int4, _>(*sort_order)
            .execute(&mut c)
            .await?;
        }
        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn id_gen_for_test() -> String {
        new_id()
    }

    #[cfg(test)]
    fn token_gen_for_test() -> String {
        Self::new_share_token()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_token_format_is_safe_charset_and_length() {
        for _ in 0..20 {
            let t = AssistantStore::token_gen_for_test();
            assert_eq!(t.len(), 8);
            // 排除易混淆字符
            for c in t.chars() {
                assert!(
                    !matches!(c, '0' | 'O' | '1' | 'I' | 'l' | 'o'),
                    "ambiguous char in token: {}",
                    t
                );
            }
        }
    }
}
