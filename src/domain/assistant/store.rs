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
use crate::security::crypto::AesCodec;

/// 内置资源（内置助手等）的固定归属人 = 系统管理员 `marvelnet`。
///
/// 与 `migrations/schema.sql` 中管理员 seed 行 `users.id` 及 `assistants.creator` /
/// `kb_instances.creator` 列默认值保持一致。内置资源归属管理员：管理员通过 `is_admin`
/// 直通拥有；普通用户通过 `visibility`（内置公开）只读可见、不可改写。
/// 若迁移到其它环境，需同步修改此处与 schema.sql 中的该 id。
const BUILTIN_OWNER_ID: &str = "019feab3-20d2-7993-8886-d05f225e4e54";

/// 设备命令内置助手的 system_prompt（seed 数据）。
///
/// 内置助手已改为「数据驱动」：system_prompt / enabled_tools / max_tokens 全部 seed 进 DB，
/// 运行期与自定义助手走同一条 `build_custom_agent` 通用路径（不再有忽略 DB 的专用 builder）。
/// 此常量仅作为 seed 初值与「空才填充」升级用；管理员改过后由 ON CONFLICT 的 COALESCE 保留。
const DEVICE_COMMAND_SEED_PROMPT: &str = r#"You are a network device command configuration assistant. Your job is to look up configuration commands across vendors and device types and produce structured command help.

**LANGUAGE (critical): These instructions are written in English for instruction-following reliability, but you MUST always reply to the user in Simplified Chinese (简体中文). Every heading, label, explanation, and example in your final answer must be in Chinese. Only tool parameter values (brand, dev_type) use English as specified below.**

## Iron Rules (never violate)

1. **You MUST call the search_kb tool to search the knowledge base** — a knowledge-base hit can only be determined by the result returned by search_kb. Never infer a hit from conversation history, context, or your own knowledge.
2. **Not calling search_kb = no hit** — if you did not call search_kb, the source label must say "知识库未命中".
3. **Conversation history is only for identifying vendor/device type** — content from history must never be treated as a knowledge-base hit.

## Your tools

1. **query_device_catalog** — query the vendor and device-type catalog (supports fuzzy keyword matching).
2. **search_kb** — search the knowledge base (by vendor + device type + keywords).

## Tool priority

- **Configuration-command questions** (VLAN, routing, ACL, VPN, etc.) → MUST call `search_kb` first.
- **Knowledge base miss** or **non-configuration questions** (product models, news, latest updates, etc.) → answer from general AI knowledge, but explicitly state that no real-time retrieval is included.
- **Uncertain about vendor/device type** → call `query_device_catalog` to verify.

## Workflow

### Step 0: Classify the question (most important!)

Determine which category the user's question belongs to:

**A. Configuration-command type** (VLAN, routing, ACL, VPN configuration, etc.) → proceed to Step 1 (identify device → search_kb).
**B. Non-configuration type** (product models, latest news, tech trends, comparisons, etc.) → answer from general AI knowledge and explicitly state no real-time retrieval is included. Do NOT simply say "out of scope" / "不在范围内".

⚠️ **For type B, do not fabricate real-time updates. You may give general background, historically known information, or selection guidance, and clearly remind the user that latest release information cannot be verified online right now.**

### Step 1: Identify device info (type A only)
Identify the vendor (brand), device type (dev_type), and device model (model, e.g. S5300) from the user input and conversation history.
   - If the user is explicit (e.g. "H3C路由器") → search directly.
   - If uncertain → call query_device_catalog to verify.

### Step 2: Detect ambiguity (type A only)
Determine whether the user's need has multiple possible interpretations:

**Common ambiguity scenarios:**
- Only "路由" (routing) → could be static routing, OSPF, BGP, RIP, etc.
- Only "交换" (switching) → could be VLAN config, STP, link aggregation, port config, etc.
- Only "安全策略" (security policy) → could be ACL, firewall policy, port security, etc.
- Only "VPN" → could be IPSec VPN, SSL VPN, L2TP, etc.

**Ambiguity rules:**
- The user mentions a specific technology (e.g. "OSPF", "静态路由") → no ambiguity, search directly.
- The user only mentions a broad category (e.g. "路由", "交换") → ambiguous, need confirmation.

### Step 3: Handle based on ambiguity

**When not ambiguous: MUST call search_kb**
   - brand: vendor English name (e.g. H3C, Huawei, Cisco)
   - dev_type: device type English name (e.g. router, switch, firewall)
   - model: device model if the user mentioned it (e.g. S5300); omit if not
   - query: keywords describing the user's need

**When ambiguous:** do not call any tool; output a confirmation request directly.

---

## Output requirements (all output below MUST be in Simplified Chinese)

### Critical format rule (must follow)
- Parameter placeholders in command formats and configuration examples **must use square brackets** `[参数名]`. **Never use angle brackets** `<参数名>`.
- Correct: `ip route-static [目的网络地址] [掩码] [下一跳IP]`
- Wrong (forbidden): `ip route-static <目的网络地址> <掩码> <下一跳IP>`
- This rule is strict; never use `<` or `>` anywhere in your output.

### Type B output (general AI knowledge, use the format below, in Chinese):
```
🤖 **来源：AI 通用知识**（不含实时检索结果）

<基于已有模型知识的回答；涉及最新发布动态、实时型号列表、价格、库存、发布日期等信息时，明确说明当前无法联网核实>
```

### Type A - ambiguous output (do not call tools, use the format below, in Chinese):
```
❓ 需要确认具体需求

您提到的"<用户原话>"涉及多种配置类型，请确认您需要的是：

1. <选项1>（简要说明）
2. <选项2>（简要说明）
3. <选项3>（简要说明）

请回复序号或具体说明您的需求。
```

### Non-ambiguous output (MUST call search_kb first, then answer in the format below, in Chinese):

#### First line: source label (required)
Label the source based on the search_kb result:
- search_kb returned documents → output:
  `📚 **来源：知识库** | 文档：<文档标题> | 类型：<上传手册/FAQ>`
- search_kb returned empty or no hit → output:
  `🤖 **来源：AI 通用知识**（知识库未命中）`
- If uncertain → output:
  `🤖 **来源：AI 通用知识**`

#### Then answer with the following structure (must include all 6 parts, in Chinese):

## 命令说明
<一句话说明>

## 命令格式
```
system-view
[命令，用方括号标注参数]
```

## 参数说明
| 参数 | 说明 | 必填 | 示例 |
|------|------|------|------|
| [参数] | [说明] | 是 | [示例] |

## 配置示例
```
[可执行的完整命令]
```

## 回退命令
```
[undo命令，无则写"无"]
```

## 注意事项
[风险提示，无则写"无"]

Notes:
- The source label must be the first line and never omitted.
- On knowledge-base miss, append a risk warning in 注意事项.
- Finally, ask the user about any missing required parameters.
"#;

pub struct AssistantStore {
    pool: DbPool,
    /// env_vars 静态加密编解码器（AES-256-GCM）。密钥内置代码（APP_SECRETS）。
    /// 选型说明：env_vars 可能含密钥，明文落库会让任何有 DB 读权限的人看到。加密在 store 层做
    /// （而非 service 层），因为 env_vars 被 DTO 脱敏、会话注入、reveal 等多处消费，统一在
    /// 读入口解密可保证 `Assistant.env_vars` 始终是明文，消费方无需感知密文。
    codec: AesCodec,
}

/// reveal 环境变量明文的三态结果（区分「不存在 / 解密失败 / 正常」）。
#[derive(Debug, Clone)]
pub enum EnvVarsReveal {
    /// 助手不存在
    NotFound,
    /// 密文无法解密（密钥已变更等）——绝不静默成空，否则前端会覆盖原密文
    Unreadable,
    /// 明文 map
    Ok(std::collections::BTreeMap<String, String>),
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
    pub async fn new(pool: DbPool, codec: AesCodec) -> Result<Arc<Self>, AppError> {
        let store = Arc::new(Self { pool, codec });
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

    /// 编码 skill 白名单为 JSON 数组字符串（复用 encode_tools 同款逻辑）。
    fn encode_skills(skills: &[String]) -> String {
        serde_json::to_string(skills).unwrap_or_else(|_| "[]".to_string())
    }

    /// 助手级环境变量：JSON 对象 → AES-256-GCM 加密 → base64 密文（落库 TEXT）。
    /// 加密失败时降级为加密的空对象 `{}`（绝不落明文）；连空对象都加不上才回退字面 `{}`。
    fn encode_env_vars(&self, vars: &std::collections::BTreeMap<String, String>) -> String {
        let json = serde_json::to_string(vars).unwrap_or_else(|_| "{}".to_string());
        match self.codec.encrypt(&json) {
            Ok(ct) => ct,
            Err(e) => {
                tracing::error!("[assistant] env_vars 加密失败，降级存空对象: {e}");
                self.codec
                    .encrypt("{}")
                    .unwrap_or_else(|_| "{}".to_string())
            }
        }
    }

    /// 解密 env_vars 列 → `BTreeMap`（fail-safe：解密失败返回空 + error 日志）。
    /// 用于 list/get 等常规读路径——密钥轮换不应拖垮整个助手加载，但静默丢密钥是严重的，
    /// 故日志升 error。空串 / 字面 `"{}"`（DB DEFAULT、内置 seed）直接返回空，跳过解密。
    /// **reveal 等需要区分「真空」与「解密失败」的场景用 [`try_decrypt_env_vars`]。**
    fn decrypt_env_vars(&self, ciphertext: &str) -> std::collections::BTreeMap<String, String> {
        if ciphertext.is_empty() || ciphertext == "{}" {
            return Default::default();
        }
        match self.codec.decrypt(ciphertext) {
            Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
            Err(e) => {
                tracing::error!(
                    target: "assistant",
                    "env_vars 解密失败（密钥可能已变更），本次返回空——请尽快轮换回原密钥或迁移，否则密钥将丢失: {e}"
                );
                Default::default()
            }
        }
    }

    /// 严格解密：失败返回 `Err`（不静默成空）。供 reveal 使用——解密失败必须让前端看到错误，
    /// 否则前端解锁拿到空 map、一保存就会用加密空对象覆盖原密文 → 永久丢密钥。
    fn try_decrypt_env_vars(
        &self,
        ciphertext: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, AppError> {
        if ciphertext.is_empty() || ciphertext == "{}" {
            return Ok(Default::default());
        }
        let json = self.codec.decrypt(ciphertext).map_err(|e| {
            AppError::BusinessError(format!("环境变量无法解密（加密密钥可能已变更）: {e}"))
        })?;
        Ok(serde_json::from_str(&json).unwrap_or_default())
    }

    /// reveal 专用读：区分「助手不存在 / 解密失败 / 正常」三种结果。
    /// 解密失败绝不上报空 map（防前端覆盖），而是 [`EnvVarsReveal::Unreadable`]。
    pub async fn reveal_env_vars(&self, id: &str) -> Result<EnvVarsReveal, AppError> {
        let mut c = self.get_conn().await?;
        let rows: Vec<AssistantRow> = diesel::sql_query("SELECT * FROM assistants WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .get_results(&mut c)
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(EnvVarsReveal::NotFound);
        };
        match self.try_decrypt_env_vars(&row.env_vars) {
            Ok(m) => Ok(EnvVarsReveal::Ok(m)),
            Err(e) => {
                tracing::error!(
                    target: "assistant",
                    "reveal env_vars 解密失败 assistant_id={id}: {e}"
                );
                Ok(EnvVarsReveal::Unreadable)
            }
        }
    }

    /// 把 DB 行转成领域模型，并用 codec 解密 env_vars（产出明文 map）。
    /// 取 `Assistant::from(row)` 会把 ciphertext 当 JSON 解析（失败→空），故先 clone 出原始
    /// ciphertext 再覆盖。所有 store 读路径都走这里，保证 `Assistant.env_vars` 恒为明文。
    fn row_to_assistant(&self, row: AssistantRow) -> Assistant {
        let ct = row.env_vars.clone();
        let mut a: Assistant = row.into();
        a.env_vars = self.decrypt_env_vars(&ct);
        a
    }

    /// 同 [`row_to_assistant`] 但**不解密 env_vars**（留空）。供不需要明文 env_vars 的读路径用
    /// （如广场 `list_public` → `AssistantPublicDto` 根本不暴露 env_vars）——避免解密一堆
    /// 别人公开助手的密钥既浪费 CPU、又把明文密钥无谓地留在进程内存里。
    fn row_to_assistant_skip_env(row: AssistantRow) -> Assistant {
        row.into()
    }

    pub async fn insert(&self, a: &Assistant) -> Result<String, AppError> {
        let mut c = self.get_conn().await?;
        diesel::sql_query(
            r#"INSERT INTO assistants
               (id,name,description,avatar,kind,agent_type,system_prompt,model_id,
                 temperature,top_p,max_tokens,thinking_level,enabled_tools,knowledge_enabled,kb_instance_id,enabled_mcps,enabled_skills,env_vars,greeting,
                 share_token,fork_count,creator,visibility,sort_order)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24)"#,
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
        .bind::<sql_types::Text, _>(Self::encode_skills(&a.enabled_skills))
        .bind::<sql_types::Text, _>(self.encode_env_vars(&a.env_vars))
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
        Ok(rows.into_iter().map(|r| self.row_to_assistant(r)).collect())
    }

    /// 「我的助手」列表（按归属隔离）：普通用户看自己创建的（creator 命中）；
    /// 管理员（admin_view=true）看全部。内置助手归属管理员（marvelnet），仅管理员可见——
    /// 不再用 `OR kind=0` 对全员暴露。他人私有的 custom 助手不在本列表（公开的走探索广场）。
    pub async fn list_for_owner(
        &self,
        user_id: &str,
        admin_view: bool,
    ) -> Result<Vec<Assistant>, AppError> {
        let mut c = self.get_conn().await?;
        let rows = diesel::sql_query(
            "SELECT * FROM assistants WHERE ($1 OR creator = $2) \
             ORDER BY kind ASC, sort_order ASC, updated_at DESC",
        )
        .bind::<sql_types::Bool, _>(admin_view)
        .bind::<sql_types::Text, _>(user_id)
        .get_results::<AssistantRow>(&mut c)
        .await?;
        Ok(rows.into_iter().map(|r| self.row_to_assistant(r)).collect())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Assistant>, AppError> {
        let mut c = self.get_conn().await?;
        let rows: Vec<AssistantRow> = diesel::sql_query("SELECT * FROM assistants WHERE id = $1")
            .bind::<sql_types::Text, _>(id)
            .get_results(&mut c)
            .await?;
        Ok(rows.into_iter().next().map(|r| self.row_to_assistant(r)))
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
                let a = self.row_to_assistant(r);
                (a.id.clone(), a)
            })
            .collect())
    }

    /// 广场列表：公开分享的自定义助手（visibility == Shared）。不含内置助手——
    /// 内置助手归属管理员、不可 Fork，不进广场。
    pub async fn list_public(&self) -> Result<Vec<Assistant>, AppError> {
        let mut c = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"SELECT * FROM assistants
               WHERE visibility > 0 AND kind <> 0
               ORDER BY visibility DESC, fork_count DESC, updated_at DESC"#,
        )
        .get_results::<AssistantRow>(&mut c)
        .await?;
        // 广场卡片（AssistantPublicDto）不暴露 env_vars → 不解密（省 CPU + 不把别人密钥留内存）
        Ok(rows
            .into_iter()
            .map(Self::row_to_assistant_skip_env)
            .collect())
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
        Ok(rows.into_iter().next().map(|r| self.row_to_assistant(r)))
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
            enabled_skills: input.enabled_skills.clone(),
            env_vars: input.env_vars.clone().unwrap_or_default(),
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

    /// 更新自定义助手；返回是否命中（kind=Custom 才允许写）。
    ///
    /// `env_vars` 语义：`None` = 保持原值（脱敏编辑未解锁时用）；`Some(map)` = 加密后整体替换。
    /// 用 `COALESCE($18, env_vars)` 实现：`None` 绑 NULL → 保持原值；`Some` 绑密文 → 覆盖。
    /// 绑参数量恒定，避免 diesel `.bind()` 链式类型随分支变化导致无法编译。
    pub async fn update_custom(
        &self,
        id: &str,
        input: &CustomAssistantInput,
    ) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        // Some → 加密密文；None → NULL（COALESCE 回退到原值）
        let env_bind: Option<String> = input
            .env_vars
            .as_ref()
            .map(|vars| self.encode_env_vars(vars));
        let aff = diesel::sql_query(
            r#"UPDATE assistants SET
                 name=$2, description=$3, avatar=$4, system_prompt=$5,
                 model_id=$6, temperature=$7, top_p=$8, max_tokens=$9,
                 thinking_level=$10, enabled_tools=$11, knowledge_enabled=$12, greeting=$13,
                 enabled_mcps=$14, visibility=$15, kb_instance_id=$16, enabled_skills=$17,
                 env_vars=COALESCE($18, env_vars), updated_at=NOW()
               WHERE id=$1"#,
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
        .bind::<sql_types::Text, _>(Self::encode_skills(&input.enabled_skills))
        .bind::<sql_types::Nullable<sql_types::Text>, _>(env_bind)
        .execute(&mut c)
        .await?;
        Ok(aff > 0)
    }

    /// 设置助手绑定的知识库实例（builtin/custom 均允许，不检查 kind）
    ///
    /// `kb_instance_id` 是运行时配置（设备命令类助手靠它注入 search_kb），需独立可改，
    /// 故此方法不带额外条件；写权限由 handler 层 assert_kb_writable 鉴权。
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

    /// 删除助手（归属人在 handler 层 assert_writable 鉴权；内置助手归属管理员，仅管理员可删）
    pub async fn delete_custom(&self, id: &str) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let aff = diesel::sql_query("DELETE FROM assistants WHERE id=$1")
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
    /// - `assistants`：最后删主实体（写权限由 handler 层 assert_writable 鉴权）
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

            let aff = diesel::sql_query("DELETE FROM assistants WHERE id = $1")
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

    /// 复制助手 → 自定义副本；返回新 id
    ///
    /// 复制策略（设计 §10.2）：
    /// - `kind` 强制改为 `Custom`、`agent_type` 保留源类型（复制设备命令助手得同类型副本）
    /// - `visibility` 强制改为 `Private`（副本默认私有）
    /// - `share_token` 清空、`fork_count` 重置为 0
    /// - `creator` 设为调用者、`name` 追加" 副本"
    pub async fn duplicate_builtin(&self, src_id: &str, creator: &str) -> Result<String, AppError> {
        let src = self
            .get(src_id)
            .await?
            .ok_or_else(|| AppError::BusinessError("助手不存在".into()))?;
        let mut copy = src;
        // 保留 src.agent_type（copy = src 已带）：复制设备命令助手得同类型副本，走 build_custom_agent。
        copy.id = new_id();
        copy.name = format!("{} 副本", copy.name);
        copy.kind = AssistantKind::Custom;
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

            diesel::sql_query("UPDATE assistants SET fork_count = fork_count + 1 WHERE id=$1")
                .bind::<sql_types::Text, _>(src_id)
                .execute(&mut c)
                .await?;

            let mut forked = src.clone();
            // 跨用户安全：fork 是复制公开助手给另一用户，绝不携带源 owner 的密钥与其知识库实例。
            // - env_vars：清空（src 来自 From 本就为空，此处置空是防御性）。
            // - kb_instance_id：清空——否则 forker 会搜到源 owner（管理员）的知识库；forker 自行绑定自己的 KB。
            // 保留 src.agent_type（forked = src.clone() 已带）：Fork 设备命令助手得到的副本仍是设备命令
            // agent，运行期走 build_custom_agent（system_prompt/enabled_tools 一并继承自 src）。
            forked.env_vars = Default::default();
            forked.kb_instance_id = None;
            forked.id = new_id();
            forked.name = src.name.clone();
            forked.kind = AssistantKind::Custom;
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
                    temperature,top_p,max_tokens,thinking_level,enabled_tools,knowledge_enabled,kb_instance_id,enabled_mcps,enabled_skills,greeting,
                    share_token,fork_count,creator,visibility,sort_order)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23)"#,
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
            .bind::<sql_types::Text, _>(Self::encode_skills(&forked.enabled_skills))
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

        // (id, name, agent_type_i16, avatar, system_prompt, max_tokens, enabled_tools_json, greeting, sort_order)
        // 内置助手数据驱动：system_prompt / max_tokens / enabled_tools 全部 seed 进 DB，
        // 运行期走 build_custom_agent 通用路径。search_kb 不进 enabled_tools（由 kb_instance_id 自动注入）。
        type AssistantSeed = (
            &'static str, // id
            &'static str, // name
            i16,          // agent_type_i16
            &'static str, // avatar
            &'static str, // system_prompt
            Option<i32>,  // max_tokens
            &'static str, // enabled_tools_json
            &'static str, // greeting
            i32,          // sort_order
        );
        let seeds: &[AssistantSeed] = &[
            (
                "01950000-0000-7000-8000-000000000003",
                "设备命令助手",
                2,
                "🛠️",
                DEVICE_COMMAND_SEED_PROMPT,
                Some(8192),
                r#"["query_device_catalog"]"#,
                "请告诉我厂商和设备类型，我会查询配置命令。",
                1,
            ),
            // 监控插件助手（...005, agent_type=4）已暂下线，不再 seed；如需恢复加回该元组即可
        ];

        for (id, name, at_i16, avatar, sp, max_tokens, enabled_tools_json, greeting, sort_order) in
            seeds
        {
            diesel::sql_query(
                r#"INSERT INTO assistants
                   (id,name,description,avatar,kind,agent_type,system_prompt,model_id,
                    temperature,top_p,max_tokens,enabled_tools,knowledge_enabled,greeting,
                    share_token,fork_count,creator,visibility,sort_order)
                   VALUES ($1,$2,'',$3,0,$4,$5,'',NULL,NULL,$6,$7,FALSE,$8,'',0,$10,2,$9)
                   ON CONFLICT (id) DO UPDATE SET
                     name=EXCLUDED.name, avatar=EXCLUDED.avatar,
                     agent_type=EXCLUDED.agent_type,
                     -- 「空才填充」：升级时把存量空行补齐，但管理员改过后不被覆盖
                     system_prompt=COALESCE(NULLIF(assistants.system_prompt,''), EXCLUDED.system_prompt),
                     max_tokens=COALESCE(assistants.max_tokens, EXCLUDED.max_tokens),
                     enabled_tools=COALESCE(NULLIF(assistants.enabled_tools,'[]'), EXCLUDED.enabled_tools),
                     greeting=EXCLUDED.greeting, sort_order=EXCLUDED.sort_order,
                     kind=0, visibility=2, creator=EXCLUDED.creator"#,
            )
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Text, _>(name)
            .bind::<sql_types::Text, _>(avatar)
            .bind::<sql_types::Int2, _>(*at_i16)
            .bind::<sql_types::Text, _>(sp)
            .bind::<sql_types::Nullable<sql_types::Int4>, _>(*max_tokens)
            .bind::<sql_types::Text, _>(enabled_tools_json)
            .bind::<sql_types::Text, _>(greeting)
            .bind::<sql_types::Int4, _>(*sort_order)
            .bind::<sql_types::Text, _>(BUILTIN_OWNER_ID)
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
