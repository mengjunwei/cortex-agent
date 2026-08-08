# Cortex-Agent 自定义助手（Assistant）设计方案

> 状态：**待评审** · 版本：v1.1 · 日期：2026-06-26
> 范围：一步到位（核心三件套 + 工具 + 知识库 + 生成参数 + 复制 + 导入导出 + 广场分享/口令 fork）

> ⚠️ **实现状态（2026-06-30 更新）**：本方案已落地。实现期有两处与本文档不一致的关键演进，阅读时请以下列为准：
> - **接口形态**：助手 / 会话 / 模型等接口已从 REST（`/api/sessions`、`/api/models` 等）**迁移到 GraphQL 单入口** `POST /api/graphql`（详见 [docs/api.md](../api.md)）。
> - **模型管理**：模型不再来自配置文件 `[llm]` 段 / `LlmConfig::resolve_model`（该代码已删除），统一由数据库「模型供应商」管理（`model_provider` 模块），模型解析以 DB 为唯一数据源。
> - 文中出现的 `src/config/mod.rs:214-247`、`resolve_model`、`/api/...` REST 路径均为设计期假设，仅作设计意图参考。
> - **AgentType 收敛**：内置助手已从设计期设想的 5 类（device_command / command_brainstorm / monitor_plugin / browser / code_assistant）收敛为 **2 类**——`device_command` 与 `monitor_plugin`。`Auto`(0)/`Chat`(1) 及头脑风暴/浏览器/代码助手均已移除；`AgentType` 现仅 `DeviceCommand=2`、`MonitorPlugin=4`、`Custom=9`（见 `src/domain/assistant/enums.rs`，未知值兜底为 `Custom`）。

---

## 1. 背景与目标

### 1.1 现状

当前系统的"会话类型"由 `agent_type` 硬编码为 5 个枚举值（`device_command` / `command_brainstorm` / `monitor_plugin` / `browser` / `chat`），存在三处限制：

| 维度 | 现状位置 | 问题 |
|------|----------|------|
| 会话类型 | `crate::agent::build_agent_with_model` 的 `match agent_type` | 用户无法新建自己的会话类型 |
| 系统提示词 | `src/tools/chat.rs::get_chat_system_prompt()` 等代码常量 | 用户不能自定义人设/指令 |
| 模型选择 | `config.llm.resolve_model(model_id)`（已支持多模型） | 选择绑在 session 而非可复用的"助手"上，无法沉淀 |

模型多选基础设施**已经具备**（[config/mod.rs:214-247](../../src/config/mod.rs) 的 `resolve_model` + `/api/models`），缺的是把"系统提示词 + 模型 + 工具 + 知识库 + 参数"沉淀为**用户可创建、可复用、可分享的"助手"实体**。

### 1.2 目标

引入业界标准的 **"助手（Assistant）模板 / 会话（Session）实例"** 分层，让用户能：

1. **自定义**：创建/编辑/删除自己的助手；
2. **选模型**：每个助手绑定一个默认模型；
3. **定义系统提示词**：填写人设与指令；
4. **挂工具/知识库**：勾选联网搜索、知识库检索等能力；
5. **调参数**：温度、top_p、最大输出 token；
6. **复用/分享**：复制自定义助手、导入导出助手模板、广场公开、分享口令 fork；
7. **会话**：选定助手即开即聊。

### 1.3 非目标（本期不做）

- 旧会话/旧 `agent_type` 兼容（系统无历史会话负担，会话类型一律以 `assistant_id` 为准）；
- 多租户/用户鉴权（系统当前无 auth，归属判断用 `[assistant].current_user` 软身份，非真实账号体系）；
- 账号体系下的在线助手市场（如 GPT Store 那种带登录/审核/计费的发布平台）；分享仅限本部署内广场 + 跨部署口令 fork；
- ~~多 Dify 知识库实例选择（当前知识库为全局单一 dataset，助手级仅"开关"，多库留待后续）~~ — **已落地**：知识库改为多 provider 多实例（`kb_instances`，Dify 外挂 + 内置 Qdrant 并存），助手绑 `kb_instance_id`（见 [§6](#6-知识库绑定)）；
- 浏览器 MCP、监控插件三层校验等专业工具向自定义助手开放（强绑定内置助手，不暴露）。

---

## 2. 市场最佳实践调研

调研 OpenAI GPTs、Claude Projects、扣子 Coze、Dify、LobeChat、Open WebUI、Cherry Studio 七家，**共性结论**：

> **配置（Assistant/Bot/Model 模板）≠ 会话（Session/Chat 实例）。** 配置是可复用、可分享的模板；会话是使用该模板的一次实例，持有独立消息历史。

| 平台 | 配置单元 | 系统提示词 | 模型 | 工具 | 知识库 | 分享 |
|------|---------|-----------|------|------|--------|------|
| OpenAI GPTs | GPT | Instructions | 可选 | Code/Browse/Actions | 文件 | GPT Store |
| Claude Projects | Project | Custom instructions | Opus/Sonnet/Haiku | 有限 | 文件/Artifacts | 团队 |
| 扣子 Coze | Bot | 人设与提示词 | 可选 | 插件 | 知识库 | 商店/多渠道 |
| Dify | App | 提示词模板 | 供应商可选 | 工具节点 | 知识库 | Web/API 发布 |
| LobeChat | Assistant | systemRole | 可选 | 插件 | 知识库 | Agent 市场 |
| Open WebUI | Model | system prompt | base 模型 | 工具 | 知识库 | 公开/私有 |
| Cherry Studio | Assistant | 系统提示词 | 可选 | MCP | 知识库 | 助手市场 |

**对 Cortex-Agent 的启发**：引入独立 `assistants` 表与助手 CRUD，会话挂载到 `assistant_id`；内置专业助手只读、不支持复制；自定义助手走通用构建路径。

---

## 3. 核心设计：助手模板 / 会话实例分层

```
┌─────────────────────────────────────────────────────────────┐
│  助手层 (Assistant) —— 模板 / 定义（可复用、可分享）            │
│  name + system_prompt + model_id + gen_params                 │
│    + enabled_tools + knowledge_enabled + greeting             │
└────────────┬─────────────────────────────────┬──────────────┘
             │ 一个助手可派生无数会话             │
             ▼                                 ▼
┌───────────────────────────────┐  ┌───────────────────────────────┐
│ 会话 Session A                 │  │ 会话 Session B                 │
│ state: { assistant_id, ... }   │  │ state: { assistant_id, ... }   │
│ + 独立消息历史                 │  │ + 独立消息历史                 │
└───────────────────────────────┘  └───────────────────────────────┘
```

### 3.1 两类助手

| 类型 | 来源 | 可编辑 | 说明 |
|------|------|--------|------|
| **内置助手（builtin）** | 启动时 seed，由代码定义 | **只读** | 对应现有 2 个内置 agent_type（device_command / monitor_plugin），保留复杂工具链（监控三层校验等）。**不支持复制**（`duplicate_builtin` 拒绝内置助手）。 |
| **自定义助手（custom）** | 用户创建 | 完全可编辑 | 用户填系统提示词、选模型/工具/知识库/参数。走通用构建路径。 |

**关键原则**：内置助手**不做破坏性改动**（`build_device_command_agent_with_model` 等函数保持原样），自定义助手走新增的 `build_custom_agent` 通用路径。

---

## 4. 数据模型

### 4.1 新增 `assistants` 表（`migrations/schema.sql`）

```sql
CREATE TABLE IF NOT EXISTS assistants (
    id                VARCHAR(36)   PRIMARY KEY,             -- UUIDv7（时间有序，字符串存储，36 字符）
    name              VARCHAR(128)  NOT NULL,
    description       TEXT          NOT NULL DEFAULT '',
    avatar            VARCHAR(64)   NOT NULL DEFAULT '🤖',   -- emoji 或图片 URL
    kind              SMALLINT      NOT NULL DEFAULT 1,      -- 枚举 0=builtin 1=custom（见 §4.4）
    agent_type        SMALLINT      NOT NULL DEFAULT 9,      -- 枚举，见 §4.4；自定义助手固定 9
    system_prompt     TEXT          NOT NULL DEFAULT '',
    model_id          VARCHAR(128)  NOT NULL DEFAULT '',     -- 对应 [[llm.models]].id；空=默认模型
    -- 生成参数（NULL 表示用模型默认）
    temperature       DOUBLE PRECISION,
    top_p             DOUBLE PRECISION,
    max_tokens        INTEGER,
    -- 能力（enabled_tools / enabled_mcps 以 TEXT 存 JSON 字符串，架构 §8.2 禁 JSONB）
    thinking_level    TEXT,                                   -- 思考级别 low/medium/high/...（NULL=模型默认）
    enabled_tools     TEXT          NOT NULL DEFAULT '[]',   -- ["search_kb","query_device_catalog","shell_command"]
    enabled_mcps      TEXT          NOT NULL DEFAULT '[]',   -- 启用的 MCP Server id 列表
    knowledge_enabled BOOLEAN       NOT NULL DEFAULT FALSE,
    kb_instance_id    VARCHAR(36),                            -- 绑定的知识库实例 id（NULL=不绑定）
    greeting          TEXT          NOT NULL DEFAULT '',
    -- 分享（详见 §12）
    share_token       VARCHAR(16)   NOT NULL DEFAULT '',     -- 非空=已生成分享口令（base62，8 位）
    fork_count        INTEGER       NOT NULL DEFAULT 0,      -- 被fork次数（热度）
    -- 元数据
    creator           VARCHAR(128)  NOT NULL DEFAULT 'local',
    visibility        SMALLINT      NOT NULL DEFAULT 0,      -- 枚举 0=private 1=shared 2=builtin（见 §4.4）
    sort_order        INTEGER       NOT NULL DEFAULT 0,
    created_at        TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    -- 主键为 UUIDv7 字符串，校验规范格式（带连字符 36 字符）
    CONSTRAINT chk_assistants_id
        CHECK (id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    -- 枚举取值范围约束（防御非法值）
    CONSTRAINT chk_assistants_kind       CHECK (kind IN (0,1)),
    CONSTRAINT chk_assistants_visibility CHECK (visibility IN (0,1,2))
);

-- 索引设计（依据实际查询模式）
-- 1) 列表页主查询：按 kind 分 tab + sort_order 排序 + 最近更新倒序（覆盖最常见读取路径）
CREATE INDEX IF NOT EXISTS idx_assistants_list
    ON assistants (kind, sort_order, updated_at DESC);
-- 2) 广场页查询：visibility=1(shared)/2(builtin) 的公开助手，按热度+更新排序（见 §12）
CREATE INDEX IF NOT EXISTS idx_assistants_explore
    ON assistants (visibility, fork_count DESC, updated_at DESC);
-- 3) 分享口令反查：口令登录场景的高频点查，仅对非空口令建唯一部分索引
CREATE UNIQUE INDEX IF NOT EXISTS uq_assistants_share_token
    ON assistants (share_token) WHERE share_token <> '';
-- 说明：
-- - 主键 id（UUIDv7 单调递增）自带 B-tree，插入无页分裂，无需额外干预；
-- - 单列 kind 索引被复合索引 (kind, sort_order, updated_at) 最左前缀覆盖，不再单建；
-- - share_token 用"部分唯一索引"（WHERE share_token<>''），既保证口令全局唯一，又避免空值占用索引。
```

### 4.2 内置助手 seed（启动迁移时写入）

> 内置助手 `id` 为**预留固定 UUIDv7 字面量**（启动 seed 时硬编码），保证升级幂等。`agent_type` 列为数字编码（见 §4.4）。

| id（固定 UUIDv7） | name | agent_type（编码） | avatar |
|------------------|------|-------------------|--------|
| `01950000-0000-7000-8000-000000000003` | 设备命令助手 | 2 (device_command) | 🛠️ |
| `01950000-0000-7000-8000-000000000005` | 监控插件助手 | 4 (monitor_plugin) | 📈 |

> 注：内置助手已收敛为上述 2 类。`Auto`(0)/`Chat`(1) 及 `command_brainstorm`(3)/`browser`(5)/`code_assistant`(6) 均已移除——启动 seed 时按 `DELETE FROM assistants WHERE id IN ('01950000-0000-7000-8000-000000000001','01950000-0000-7000-8000-000000000004','01950000-0000-7000-8000-000000000006')` 清理旧记录（见 `src/domain/assistant/store.rs::seed_builtin`），旧数据统一按 `Custom`(9) 处理。

> 内置助手 `system_prompt` 字段仅作展示，从代码常量同步；运行时仍走代码构建函数，保证复杂工具链不被破坏。

### 4.3 主键与 ID 生成（UUIDv7）

- **存储**：`VARCHAR(36)` 存 UUIDv7 带连字符规范字符串（如 `01950000-7c2a-7000-9aaa-abcdef012345`），并加 `CHECK` 约束防脏数据；
- **生成**：后端 Rust 用 `uuid` crate 的 `Uuid::now_v7().to_string()` 生成；
- **为什么用 UUIDv7 而非 v4**：v7 时间有序 → 主键 B-tree 单调递增插入，**无页分裂、写放大低**，天然聚簇友好；
- **为什么用字符串而非原生 `uuid` 类型**：满足"字符串存储"约定，前后端序列化零歧义（JSON 直出字符串），Rust 侧无需 `Uuid`/`String` 来回转换。

### 4.4 枚举编码与前后端翻译

枚举字段统一**数字存储**（SMALLINT），前后端各自维护映射表翻译为可读值。

| 字段 | 编码 | 名称（后端/前端共用） | 说明 |
|------|------|----------------------|------|
| `kind` | 0 | builtin | 内置助手（只读） |
| `kind` | 1 | custom | 自定义助手 |
| `agent_type` | 2 | device_command | 设备命令 |
| `agent_type` | 4 | monitor_plugin | 监控插件 |
| `agent_type` | 9 | custom | 自定义助手（走 build_custom_agent） |

> 注：`agent_type` 现仅 2/4/9 三个取值；0(auto)/1(chat) 及 3(command_brainstorm)/5(browser)/6(code_assistant) 均已废弃删除，旧数据统一按 9(custom) 处理（`AgentType::from_i16` 对未知值兜底为 `Custom`）。

| `visibility` | 0 | private | 私有（仅归属者可见可编辑） |
| `visibility` | 1 | shared | 公开到广场，全部署可见，他人可 fork（见 §12） |
| `visibility` | 2 | builtin | 内置（系统预置，只读） |

**后端 Rust（数字 ↔ 业务枚举）**：用 `#[repr(i16)]` 枚举 + `TryFrom<i16>` / `Into<i16>`，在 DB 边界（diesel）存取数字，DTO 对外也用数字；业务层用强类型枚举，分发时再转字符串 match key：

```rust
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantKind { Builtin = 0, Custom = 1 }

impl AssistantKind {
    pub fn as_i16(self) -> i16 { self as i16 }
    pub fn try_from_i16(v: i16) -> Option<Self> {
        match v { 0 => Some(Self::Builtin), 1 => Some(Self::Custom), _ => None }
    }
}
// 同理 AgentType（含 as_dispatch_key() -> &'static str 用于 build_agent_with_model）
// 同理 Visibility
```

**前端（数字 ↔ 中文标签）**：常量映射对象，列表/编辑页双向翻译：

```js
// frontend/src/utils/assistantEnums.js
export const KIND = { BUILTIN: 0, CUSTOM: 1 };
export const KIND_LABEL = { 0: '内置', 1: '自定义' };
// 注：Auto(0)/Chat(1) 已废弃删除，旧数据统一按 Custom(9) 处理
export const AGENT_TYPE = { DEVICE_COMMAND:2, MONITOR_PLUGIN:4, CUSTOM:9 };
export const AGENT_TYPE_LABEL = { 2:'设备命令', 4:'监控插件', 9:'自定义' };
export const VISIBILITY = { PRIVATE:0, SHARED:1, BUILTIN:2 };
export const VISIBILITY_LABEL = { 0:'私有', 1:'共享', 2:'内置' };
```

**接口约定**：`/api/assistants` 系列请求/响应体中 `kind`/`agent_type`/`visibility` **一律用数字**（与库一致，零转换）；导入导出模板 JSON 为人类可读使用字符串（见 §11，后端导入时翻译）。

### 4.5 会话 state 扩展

`server/session.rs::create_session` 的 `initial_state` 以 `assistant_id` 作为会话类型的**唯一来源**（系统无历史会话负担，不保留 `agent_type` 回退）：

```rust
initial_state.insert("assistant_id".into(), serde_json::Value::String(assistant_id));
```

- 每个会话**必须**绑定一个 `assistant_id`；
- `agent_type` 由助手记录派生（仅用于运行时分发到对应构建器），不在会话层独立存储。

---

## 5. 工具注册表（Tool Registry）

自定义助手只能从"通用安全工具"中勾选；专业工具（监控校验、浏览器 MCP、头脑风暴）仅内置助手可用。

### 5.1 工具清单

| key | 名称 | 依赖 | 自定义可选 | 说明 |
|-----|------|------|-----------|------|
| `search_kb` | 知识库检索 | KnowledgeManager（需 `knowledge_enabled`） | ✅ | 复用 device_command 的 search_kb 构造 |
| `query_device_catalog` | 设备目录查询 | CatalogCache | ✅ | 厂商/设备类型查询 |
| `shell_command` | 命令执行 | 沙箱白名单 + 审批 | ✅ | 沙箱内执行 shell（白名单放行 + 危险拦截 + 其余审批） |
| `validate_monitor_plugin` | 监控插件校验 | PluginManager + 沙箱 | ❌（仅内置） | 专业工具 |
| `browser_*` | 浏览器 MCP 工具集 | zendriver + lease | ❌（仅内置） | 需互斥租约 |
| `command_brainstorm` | 头脑风暴 | 独立接口 | ❌（仅内置） | 独立流程 |

> **注（实现期演进，commit 4eb40b9）**：自定义助手工具**固定为 `shell_command`**，编辑页不再开放工具多选（前端 `AssistantEditPage.vue` 默认 `DEFAULT_ENABLED_TOOLS=['shell_command']`）。注册表中 `search_kb`/`query_device_catalog` 仍保留 `custom_enabled=true` 以备后续开放，但当前自定义助手构建仅挂 `shell_command`。

### 5.2 注册表实现

新增 `src/tools/registry.rs`：

```rust
pub struct ToolDescriptor {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub custom_enabled: bool,   // 自定义助手是否可勾选
}

pub fn registry() -> &'static [ToolDescriptor] { /* 静态切片 */ }

/// 暴露给前端的可选项（custom_enabled=true）
pub fn custom_options() -> Vec<&'static ToolDescriptor> { ... }
```

`GET /api/tools` 返回 `custom_options()`，供助手编辑页渲染工具开关。

---

## 6. 知识库绑定

> **演进（多 provider 改造后）**：知识库不再是"全局单一 Dify dataset"，已升级为「多 provider 多实例」（`kb_instances` 表，Dify 外挂 + 内置 Qdrant 并存）。助手级绑定从单一 `knowledge_enabled` 开关改为绑定具体实例 `kb_instance_id`（旧 `knowledge_enabled` 字段保留作兼容标记）。

- 助手存 `kb_instance_id`（指向 `kb_instances.id`）；绑定了实例的助手挂载 `search_kb` 工具，工具内部按 `kb_instance_id` 路由到对应 provider（`KnowledgeManager::search_instance`）；
- 未绑定实例 → 不挂载 `search_kb`。

> 设计详见 [多 provider 知识库改造设计](../superpowers/specs/2026-08-02-kb-multi-provider-design.md)。

---

## 7. 生成参数传导

现状：`src/llm/mod.rs::make_gen_config` 硬编码 `temperature=0.3`。改造为**助手参数优先**：

```rust
pub fn make_gen_config_from(
    max_tokens: Option<i32>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    thinking_level: Option<&str>,   // 会话级思考级别（实现期新增的第 4 参数）
) -> GenerateContentConfig {
    GenerateContentConfig {
        max_output_tokens: max_tokens.or(Some(16384)),   // 默认 16384（对齐 codex「给够预算」，4096 会截断高思考级别 thinking）
        temperature: temperature.map(|v| v as f32).or(Some(0.3)),   // 助手未设则用默认 0.3
        top_p: top_p.map(|v| v as f32),
        // + 保守的重复惩罚（frequency_penalty / presence_penalty，对齐 make_gen_config）
        ..Default::default()
    };
    // 思考级别写入 extensions（双协议键）：extensions["anthropic"].effort / extensions["openai"].reasoning_effort
}
```

自定义助手构建时把该 config 注入 `CortexAgentBuilder`（见 `src/agent/custom.rs::build_custom_agent`）；内置助手（device_command / monitor_plugin）同样经 `make_gen_config_from` 注入思考级别，原有 max_tokens（如 device_command 的 8192）等参数维持不变。

> **思考级别（会话级，实现期演进）**：经 `make_gen_config_from` 第 4 参数 `thinking_level` 传导，取值 `low` / `medium` / `high` / `xhigh` / `max`（5 档），默认 `high`，按会话存于 `session_thinking_levels` 表（`src/domain/session/mod.rs`）；发送消息时从会话读取并透传至 `AgentRequest.session_thinking_level`（见 `src/server/sse.rs`、`src/agent/custom.rs`）。双协议：Anthropic 全 5 档，OpenAI 兼容仅前 4 档（无 `max`，见 `src/llm/mod.rs::make_gen_config_from` 与前端 `ChatPage.vue`）。

---

## 8. 后端架构

### 8.1 新增/改动模块

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/assistant/mod.rs` | 新增 | `Assistant` 结构、DTO、校验 |
| `src/assistant/store.rs` | 新增 | PostgreSQL CRUD（diesel-async）+ 启动 seed 内置助手 |
| `src/tools/registry.rs` | 新增 | 工具注册表 |
| `src/server/assistant.rs` | 新增 | Axum handlers（CRUD/复制/导入导出） |
| `crate::agent`（新增 `custom` 分支） | 改动 | 分发新增 `custom` 类型 → `build_custom_agent` |
| `src/server/session.rs` | 改动 | `create_session` 接收 `assistant_id`，写入 state，按助手 greeting 返回欢迎语 |
| `src/server/sse.rs` | 改动 | 发消息时按 `assistant_id` 加载助手配置构建 Agent |
| `src/server/mod.rs` | 改动 | 注册 `/api/assistants`、`/api/tools` 路由；`AppState` 增加 `assistant_store` |
| `src/llm/mod.rs` | 改动 | 新增参数化 `make_gen_config_from` |
| `migrations/schema.sql` | 更新 | `assistants` 表建表 DDL（历史增量已合并至最终状态） |

### 8.2 自定义助手构建器（`crate::agent` 内新增）

```rust
pub fn build_custom_agent(
    cfg: &AppConfig,
    assistant: &Assistant,
    knowledge: Option<Arc<KnowledgeManager>>,
    catalog: Option<Arc<CatalogCache>>,
    browser_toolset: Option<Arc<dyn adk_rust::Toolset>>,
) -> anyhow::Result<Arc<dyn Agent>> {
    let resolved = cfg.llm.resolve_model(assistant.model_id.as_deref())?;
    let model = crate::llm::make_model_from_resolved(&resolved)?;
    let gen_cfg = crate::llm::make_gen_config_from(
        assistant.max_tokens.map(|v| v as i32),
        assistant.temperature,
        assistant.top_p,
    );

    let mut builder = LlmAgentBuilder::new(&assistant.name)
        .description(&assistant.description)
        .instruction(assistant.system_prompt.clone())   // 用户自定义提示词
        .model(model)
        .generate_content_config(gen_cfg);

    for key in &assistant.enabled_tools {
        match key.as_str() {
            "web_search"              => builder = builder.tool(web_search::build_tool()?),
            "search_kb" if knowledge.is_some() && assistant.knowledge_enabled =>
                builder = builder.tool(device_command::build_search_kb_tool(knowledge.clone())?),
            "query_device_catalog" if catalog.is_some() =>
                builder = builder.tool(device_command::build_query_catalog_tool(catalog.clone())?),
            _ => {}
        }
    }
    Ok(Arc::new(builder.build()?))
}
```

> 工具构造函数需从现有 `device_command` 模块抽取为独立 `pub fn`（见 §12 重构点）。

### 8.3 分发改造（`build_agent_for_session`）

新增统一入口 `build_agent_for_session(state, assistant) -> Arc<dyn Agent>`：根据助手 `kind`/`agent_type` 分发，**无 `agent_type` 回退链**。

```rust
// kind/agent_type 为数字枚举（见 §4.4），分发按枚举变体匹配，无 agent_type 回退链
// 注：Auto(0)/Chat(1) 已废弃删除，旧数据（值 0/1）统一按 Custom(9) 处理（from_i16 中 _ => Self::Custom）
match (assistant.kind, assistant.agent_type) {
    (AssistantKind::Builtin, other) =>
        build_agent_with_model(cfg, other.dispatch_key(), ...), // 现有内置构建器（不变）
    (AssistantKind::Custom, _) =>
        build_custom_agent(cfg, assistant, ...),             // 通用自定义路径
    _ => build_chat_agent_with_model(cfg, None, ...),        // 兜底走 Custom 路径（旧值 0/1 统一按 Custom 处理）
}
// AgentType::dispatch_key() 返回 "device_command"/... 喂给现有 match
```

> SSE 直接调用 `build_agent_for_session`：从 session state 读 `assistant_id` → 加载助手 → 分发。`build_agent_with_model` 内部 `match agent_type` 不再需要 `"custom"` 分支（由上层封装统一处理），避免参数膨胀。

### 8.4 REST API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/assistants` | 列出所有（builtin + custom） |
| POST | `/api/assistants` | 创建自定义助手 |
| GET | `/api/assistants/{id}` | 详情 |
| PUT | `/api/assistants/{id}` | 更新（builtin 返回 403） |
| DELETE | `/api/assistants/{id}` | 删除（builtin 返回 403） |
| POST | `/api/assistants/{id}/duplicate` | 复制（仅 custom 生效；builtin 被拒） |
| POST | `/api/assistants/import` | 导入助手模板 JSON |
| GET | `/api/assistants/{id}/export` | 导出助手模板 JSON |
| POST | `/api/assistants/{id}/share` | 开启分享：置 visibility=1、生成 share_token，返回口令/链接 |
| DELETE | `/api/assistants/{id}/share` | 关闭分享：置 visibility=0、清空 share_token |
| GET | `/api/explore` | 广场：列出公开助手（shared+builtin），分页 + 热度排序 |
| GET | `/api/assistants/shared/{token}` | 按分享口令获取助手模板（公开、脱敏） |
| POST | `/api/assistants/shared/{token}/fork` | 按口令 fork 为本地自定义助手 |
| GET | `/api/tools` | 自定义助手可选工具清单 |
| GET | `/api/models` | 已有，供助手下拉 |

> **GraphQL 新增 mutation**（实现期，见 [docs/api.md](../api.md)）：`generateAssistant`（AI 生成助手草稿，见 `src/agent/assistant_generator.rs`）、`bindAssistantKbInstance`（为内置/自定义助手绑定知识库实例 `kb_instance_id`，对应 `AssistantStore::set_kb_instance`）。

### 8.5 降级

延续现有策略：`assistant_store` 的 PG 不可用时降级为**内存内置助手**（仅 builtin），自定义助手相关接口返回 503 但对话仍可用内置助手——与 Session/Memory 降级一致。

---

## 9. 前端架构

### 9.1 新增/改动文件

| 文件 | 动作 | 职责 |
|------|------|------|
| `frontend/src/views/AssistantPage.vue` | 新增 | 助手管理列表（卡片网格，区分内置/自定义/公开） |
| `frontend/src/views/AssistantEditPage.vue` | 新增 | 创建/编辑表单 |
| `frontend/src/views/ExplorePage.vue` | 新增 | 广场页：浏览 shared+builtin 助手，可 fork/直接开聊 |
| `frontend/src/components/AssistantPicker.vue` | 新增 | "新建会话"时的助手选择器 |
| `frontend/src/components/ShareDialog.vue` | 新增 | 分享对话框：展示口令 + 复制 + 链接 + 关闭分享 |
| `frontend/src/components/ForkByTokenDialog.vue` | 新增 | 输入口令 → 预览 → fork 为本地助手 |
| `frontend/src/stores/assistant.js` | 新增 | Pinia：列表/CRUD/分享/广场/当前选中 |
| `frontend/src/api/index.js` | 改动 | 新增助手相关接口（含 share/explore/fork） |
| `frontend/src/router/index.js` | 改动 | 新增 `/assistants`、`/assistants/new`、`/assistants/:id/edit`、`/explore` |
| `frontend/src/views/ChatPage.vue` | 改动 | 顶栏显示当前助手；保留"本会话临时切换模型" |
| `frontend/src/App.vue` / 侧边栏 | 改动 | 新增"助手管理"+"广场"菜单；会话列表显示助手头像+名 |

### 9.2 助手编辑表单字段

- **基础**：名称、头像（emoji 选择器）、描述
- **核心**：系统提示词（多行文本 + 模板按钮：翻译/写作/代码/客服/运维…）
- **模型**：下拉（`/api/models`）+ 高级折叠：temperature / top_p / max_tokens
- **能力**：工具开关（`/api/tools` 渲染）、知识库开关
- **可见性**：单选 私有/公开（visibility 0/1）；选"公开"时提示"将出现在广场，他人可 fork"
- **体验**：开场白 greeting

### 9.3 交互流程

- **新建会话**：点"新对话" → `AssistantPicker` 展示助手卡片 → 选定 → `POST /api/sessions {assistant_id}` → 进入对话
- **会话列表**：每条显示助手头像 + 名称，便于区分
- **对话页**：顶栏显示助手名/头像；模型下拉默认取助手配置，可临时覆盖（仅本会话）
- **管理页**：卡片右上角——自定义助手有"编辑/删除/导出/分享"；内置助手仅有"导出"（只读，不支持复制/编辑）
- **导入**：管理页"导入助手"→ 上传 JSON → `POST /api/assistants/import`
- **分享（我的助手）**：卡片点"分享" → `POST /api/assistants/{id}/share` → `ShareDialog` 展示口令+链接（可一键复制）→ 关闭即 `DELETE .../share`
- **广场**：侧边栏"广场" → `GET /api/explore` 卡片瀑布流（公开+内置，按 fork_count 排序）→ 卡片可"直接开聊"或"Fork 到我的助手"
- **口令 fork**：任意页"按口令导入" → `ForkByTokenDialog` 输入口令 → `GET /api/assistants/shared/{token}` 预览 → 确认 `POST .../fork` 落地为本机自定义助手

---

## 10. 内置助手：只读（不支持复制）

- **只读**：`PUT`/`DELETE` 对 `kind = 0 (builtin)` 返回 `403`；
- **不支持复制**：设计期设想的"内置助手复制为自定义"**已在实现期取消**——`duplicate_builtin` 直接拒绝内置助手（错误"内置助手不支持复制"，见 `src/domain/assistant/store.rs`）；`POST /api/assistants/{id}/duplicate` 仅对自定义助手（custom）生效（深拷贝为 `kind=1`/`agent_type=9` 新记录）。用户可基于内置助手在编辑页**新建**自定义助手，但无法一键深拷贝；
- **升级安全**：内置助手 `system_prompt` 由代码常量在 seed 时写入，代码升级后以代码值为准（`ON CONFLICT (id) DO UPDATE` 仅刷新 builtin 展示字段，不影响 custom）。

---

## 11. 导入 / 导出（助手模板）

> 模板 JSON 面向**人类阅读与跨系统分享**，故 `kind`/`agent_type`/`visibility` 使用**可读字符串**；与 API 的数字编码区分。后端导入时翻译为数字落库。

导出 JSON（不含 id/created_at/updated_at/creator）：

```json
{
  "schema": "cortex-agent.assistant.v1",
  "name": "网络排错专家",
  "description": "...",
  "avatar": "🔧",
  "kind": "custom",
  "agent_type": "custom",
  "visibility": "private",
  "system_prompt": "...",
  "model_id": "glm-5.2",
  "temperature": 0.2,
  "top_p": null,
  "max_tokens": 4096,
  "enabled_tools": ["web_search", "search_kb"],
  "knowledge_enabled": true,
  "greeting": "您好，我是网络排错专家"
}
```

导入时：
- 校验 `schema == "cortex-agent.assistant.v1"`；
- `kind`/`agent_type`/`visibility` 按字符串映射回数字（非法值拒绝并返回 400）；
- 强制覆盖 `kind=1 (custom)`、`agent_type=9 (custom)`、`visibility` 归一为 0/1（导入的模板不可能是 builtin）；
- 校验 `system_prompt` 长度上限（硬编码 **8000 字**，见 `src/server/assistant.rs::WriteAssistantRequest::to_input`）；
- `model_id` 不在 `/api/models` 中则置空（用默认模型）；
- `enabled_tools` 过滤为注册表内合法 key；
- 生成新 UUIDv7 id 落库。

---

## 12. 助手分享（广场 + 分享口令）

> 系统无真实账号体系（见 §1.3），分享采用**部署级软归属**模型：`creator` 对齐 `[assistant].current_user`，仅作归属判断，非安全边界。

### 12.1 两种分享机制

| 机制 | 范围 | 触发 | 适用 |
|------|------|------|------|
| 广场公开 (`visibility=1`) | 本部署内全员可见 | 编辑页选"公开" / 卡片"分享" | 团队内复用 |
| 分享口令 (`share_token`) | 跨部署，凭 8 位口令 | "分享"生成口令 | 跨环境/离线传递 |

二者联动：开启分享即 `visibility=1` 且生成 `share_token`；关闭则二者皆清。

### 12.2 数据与索引
- `assistants.visibility`：0/1/2（见 §4.4），广场查询 `visibility IN (1,2)`，走 `idx_assistants_explore`；
- `assistants.share_token`：8 位 base62，全局唯一（部分唯一索引 `uq_assistants_share_token`，见 §4.1）；
- `assistants.fork_count`：每次被 fork 自增，用于广场热度排序。

### 12.3 口令生成

实现见 `src/domain/assistant/store.rs::new_share_token`（非 `rand::Alphanumeric`）：自定义 **xorshift64** 混合三路熵源，字母表**剔除易混淆字符** `0/O/1/I/l/o`（56 字符，非 base62 全集）。

```rust
fn new_share_token() -> String {
    // 56 字符表（base62 剔除易混淆 0/O/1/I/l/o），8 位 ≈ 56^8 ≈ 1.97e13，碰撞概率极低；
    // 熵源：UUIDv7 随机位 ^ SystemTime 纳秒 ^ 进程内原子计数器（rotate_left 混入），xorshift64 推进
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    // ...每轮 state ^= state<<13; state ^= state>>7; state ^= state<<17; 取 ALPHABET[state % 56]...
}

// ensure_share_token：CAS 写入（WHERE share_token IS NULL OR share_token=''），
// 冲突重试最多 5 次（store.rs:424 的 `for _ in 0..5`），并兜底 uq_assistants_share_token 唯一索引。
```

> 注：字母表为 **56 字符**（剔除大写 `I`/`O`、小写 `l`/`o`、数字 `0`/`1` 共 6 个），并非 62 字符 base62 全集。

### 12.4 权限规则（软归属，无 auth）

| 操作 | 谁可执行 |
|------|---------|
| 编辑/删除/开启分享/关闭分享 | 仅 `creator == current_user`；builtin 一律 403 |
| 广场浏览 (`GET /api/explore`) | 全员 |
| 按口令查看 (`GET /api/assistants/shared/{token}`) | 任何人（凭口令） |
| Fork (`POST /api/assistants/shared/{token}/fork`) | 任何人 → 生成新 custom，归属改为 `current_user`，`fork_count++`、`share_token` 清空 |

> **脱敏**：`GET /api/assistants/shared/{token}` 与广场列表**不返回** `system_prompt` 全文，仅返回 name/description/avatar/greeting/工具列表/参数；fork 后才落地完整 prompt，避免提示词被爬。

### 12.5 时序
- **开启分享**：`POST /api/assistants/{id}/share` → 校验归属 → `visibility=1`、生成 token → 返回 `{token, url}`；
- **广场**：`GET /api/explore?keyword=&page=&size=` → `WHERE visibility IN (1,2) ORDER BY fork_count DESC, updated_at DESC`；
- **口令查看**：`GET /api/assistants/shared/{token}` → 命中 `uq_assistants_share_token` → 返回脱敏卡片；
- **口令 fork**：`POST /api/assistants/shared/{token}/fork` → 事务内：读源助手 → 新 UUIDv7 → `kind=1/agent_type=9/visibility=0/creator=current_user/share_token=''` → 源助手 `fork_count++`。

---

## 13. 关键流程时序

### 13.1 创建助手
```
前端表单 → POST /api/assistants → store.insert(custom) → 返回助手
```

### 13.2 新建会话
```
AssistantPicker 选定 → POST /api/sessions {assistant_id}
  → 加载助手 → initial_state 写 assistant_id（agent_type 由助手记录派生，不在会话层存储）
  → 返回 assistant.greeting 作为欢迎语
```

### 13.3 发送消息（SSE）
```
POST /api/run_sse {session_id, messages, model_id?}
  → 读 session.state.assistant_id → 加载助手配置
  → resolve_model(助手 model_id 或请求 model_id 覆盖)
  → build_agent_for_session：
       builtin → 原 build_*_agent_with_model（不变）
       custom  → build_custom_agent(助手 system_prompt/model/工具/参数)
  → create_event_stream 流式响应
```

### 13.4 编辑助手后对已有会话生效
助手配置在**每次发消息时实时加载**（非创建会话时快照），故编辑助手后，其所有会话的下一条消息立即采用新提示词/模型/参数——与 OpenAI GPTs / Coze 行为一致。

### 13.5 重构点（工具构造函数化）
将 `device_command`、`web_search` 中内联的 `FunctionTool` 构造抽取为：
- `pub fn build_search_kb_tool(km: Arc<KnowledgeManager>) -> FunctionTool`
- `pub fn build_query_catalog_tool(cat: Arc<CatalogCache>) -> FunctionTool`
- `pub fn build_web_search_tool() -> FunctionTool`

供 `build_custom_agent` 与原内置构建器共用（DRY）。

---

## 14. 分发策略（Auto/Chat 已废弃）

> 注：原 `Auto`(0)/`Chat`(1) 内置助手及 `route_agent_by_llm` 智能路由机制已废弃删除。

当前分发策略简化为两种入口：

1. 选具体内置助手（device_command / monitor_plugin）→ 直接该助手会话；
2. 选自定义助手（Custom）→ 走 `build_custom_agent`（使用 DB 中的配置）。

> 现状核实（`src/agent/custom.rs`）：`build_agent_for_session` 按 `assistant.kind` 分流——`Builtin` 走 `build_builtin`，`Custom` 走 `build_custom_agent`；`build_builtin` 仅匹配 `device_command` / `monitor_plugin`，其余 `agent_type` 一律 `bail!("不支持的内置 agent_type")`。

旧数据中 agent_type 值为 0(auto) 或 1(chat) 的记录，统一按 Custom(9) 处理。

---

## 15. 迁移

> 本系统无历史会话负担，**不做旧数据兼容**，会话类型以 `assistant_id` 为唯一来源。

1. `migrations/schema.sql` 建表（含分享字段 `share_token`/`fork_count`）+ seed 内置助手（`ON CONFLICT (id) DO NOTHING`；builtin 展示字段用 `DO UPDATE` 刷新）；
2. `POST /api/sessions`（实现期为 GraphQL `createSession`）的 `assistant_id` **必填**；旧设计期设想的 `builtin_auto` 兜底已随 `Auto`/`Chat` 内置助手一并删除（见 §4.2、§14），无内置助手时由前端强制选择；
3. `/api/run_sse` 直接以 `session.state.assistant_id` 为唯一来源，无 `agent_type` 回退链；
4. 现有 2 个内置 agent 构建函数（device_command / monitor_plugin）保留，仅外层加 `build_agent_for_session` 分发判断。

---

## 16. 配置项变更

`[assistant]` 段当前**无任何配置项**——`AssistantConfig` 实为**空占位结构**（`#[derive(Debug, Clone, Default, Deserialize)] pub struct AssistantConfig {}`，见 `src/config/mod.rs`），保留以备后续扩展；`AppConfig.assistant` 字段仍以 `#[serde(default)]` 反序列化。

> 相关默认值均**硬编码在代码中**（不在 `config.toml`）：
> - 系统提示词长度上限 **8000 字**（`src/server/assistant.rs::WriteAssistantRequest::to_input`，校验 `system_prompt.chars().count() > 8000` 即拒绝）；
> - 自定义助手 `max_tokens` 默认 **16384**（`src/llm/mod.rs::make_gen_config_from`，`max_tokens.or(Some(16384))`）。

---

## 17. 风险与权衡

| 风险 | 影响 | 缓解 |
|------|------|------|
| 助手 system_prompt 过长 | 上下文膨胀 | 入库前限长 + 编辑页字符计数 |
| 自定义助手误删 | 配置丢失 | 软删除（`is_deleted` 字段）或导出提示；本期硬删 + 导出兜底 |
| 模型 id 失效（config 删除了某模型） | 构建失败 | `resolve_model` 失败时回退默认模型并告警 |
| 编辑助手影响存量会话语义 | 用户预期 | 明确"编辑即时生效"（与主流一致），UI 提示 |
| 工具构造函数化重构 | 回归风险 | 先重构 + 补单测，再接自定义路径；内置助手回归验证 |
| 知识库全局单 dataset | 多业务隔离弱 | 本期仅开关；多库留后续 |
| **分享口令碰撞**（8 位 base62 ≈ 2.18e14 空间，实际并发生成仍可能极小概率撞） | 生成失败 / 唯一索引报错 | 入库前 SELECT 探测，冲突重生成（最多 5 次，见 §12.3）；用 `uq_assistants_share_token` 兜底唯一性（见 §4.1） |
| **广场/口令暴露后被爬取提示词**（prompt scraping） | 用户提示词被批量爬走 | 广场与 `GET /api/assistants/shared/{token}` **脱敏返回**（不回 system_prompt 全文，见 §12.4）；完整 prompt 仅在 fork 后落到本地 |
| **fork_count 并发自增竞态** | 计数少加 | fork 事务内 `UPDATE assistants SET fork_count = fork_count + 1` 走行锁；接受最终一致，不强一致 |
| **跨部署口令被恶意刷 fork** | fork_count 被灌水 | 同 IP/会话对同一 token 短时间限流（Redis 计数器，见后续可选项）；本期仅记录日志，不做强制限流 |

---

## 18. 实施里程碑（高层，TDD 细节由实现计划给出）

| 里程碑 | 内容 | 产出 |
|--------|------|------|
| M1 数据层 | `5.sql`（含分享字段 `share_token`/`fork_count`/`visibility` + 索引）+ `assistant/store.rs` + seed 内置助手 + 单测 | 助手可持久化 |
| M2 工具重构 | 抽取 `build_*_tool` 公共函数 + 注册表 + `/api/tools` | 工具可复用 |
| M3 自定义构建 | `build_custom_agent` + 参数化 gen_config + 分发改造 + 单测 | 自定义助手可跑通对话 |
| M4 会话接入 | `create_session`/`run_sse` 接 `assistant_id`（无 agent_type 回退链）| 会话挂载助手 |
| M5 助手 API | CRUD/复制/导入导出 + 路由注册 + 集成测试 | 后端 API 完整 |
| M6 前端 | 管理页/编辑页/选择器/Store/路由/对话页接入 | 用户可端到端使用 |
| M7 打磨 | 软删/降级/限长/文档更新 | 生产可用 |
| M8 分享 | `gen_share_token` + 开启/关闭分享 + `/api/explore` + 口令查看脱敏 + 口令 fork（事务自增 fork_count）+ 前端 `ShareDialog`/`ExplorePage`/`ForkByTokenDialog` + 集成测试 | 广场浏览、口令 fork、脱敏生效 |

---

## 19. 验收标准

### 19.1 基础能力
- [ ] 可在管理页创建自定义助手（提示词+模型+工具+知识库+参数+开场白），持久化重启不丢；
- [ ] 选定助手新建会话后，对话采用该助手的提示词/模型/参数；
- [ ] 编辑助手后，其已有会话的下一条消息立即生效；
- [ ] 内置助手只读，且**不支持复制**（`duplicate` 仅对自定义助手生效，内置助手被拒）；
- [ ] 工具开关真实生效（开 web_search 能联网，关则不能）；
- [ ] 导出/导入助手 JSON 往返一致；
- [ ] `/api/assistants` CRUD 对 builtin 拒绝写操作（403）；
- [ ] `cargo fmt && cargo clippy && cargo test` 全绿；前端 `pnpm build` 通过。

### 19.2 分享能力（M8）
- [ ] 我的自定义助手可"开启分享"，返回 8 位 base62 口令与可分享链接；同一助手重复开启返回同一口令；
- [ ] "关闭分享"后该助手 `visibility=0`、`share_token` 清空，不再出现在广场，旧口令失效（404）；
- [ ] 广场页 `GET /api/explore` 只列出 `visibility IN (1,2)` 的助手，按 `fork_count DESC, updated_at DESC` 排序，分页可用；
- [ ] `GET /api/assistants/shared/{token}` 与广场列表**不返回 `system_prompt` 全文**（脱敏生效），仅返回 name/description/avatar/greeting/工具列表/参数；
- [ ] `POST /api/assistants/shared/{token}/fork` 落地为本地自定义助手（`kind=1`、`agent_type=9`、`visibility=0`、归属 `current_user`、`share_token` 清空），且源助手 `fork_count + 1`；
- [ ] 分享/关闭分享/口令 fork 对 builtin 助手或非归属者一律 403；fork 后的副本可正常编辑与会话；
- [ ] 口令 fork 计数在并发场景下不丢（事务内 `fork_count = fork_count + 1`）。

---

## 附录 A：与现有代码的接触面

| 关注点 | 位置 |
|--------|------|
| Agent 分发 | `crate::agent::build_agent_for_session`（按 `kind`/`agent_type` 分发到 `build_builtin` / `build_custom_agent`，见 `src/agent/custom.rs`） |
| 会话创建 | `src/server/session.rs::create_session` |
| SSE 对话 | `src/server/sse/mod.rs::handle_run_sse` / `create_event_stream`（已拆 `src/server/sse/` 目录） |
| 模型解析 | `ModelProviderStore`（DB 唯一数据源）+ `src/llm/mod.rs::make_model_by_id`（取代已删除的 `LlmConfig::resolve_model`） |
| 模型实例 | `src/llm/mod.rs::make_model_from_resolved` |
| 生成配置 | `src/llm/mod.rs::make_gen_config`（内置基线）/ `make_gen_config_from`（自定义 + 思考级别传导） |
| 知识检索 | `src/domain/knowledge/mod.rs::KnowledgeManager::search_instance` |
| ~~聊天提示词~~ | `src/tools/chat.rs::get_chat_system_prompt` **已删除**（Chat 助手已下线） |
| HTTP 路由 | `src/server/mod.rs::run` |
| 数据迁移 | `migrations/*.sql` |
| 前端会话 | `frontend/src/stores/session.js` |
| 前端对话 | `frontend/src/stores/chat.js` / `views/ChatPage.vue` |
| 前端 API | `frontend/src/api/index.js` |
