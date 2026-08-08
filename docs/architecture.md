# Cortex Agent 项目结构与分层规范

> **本文件是项目代码组织的唯一权威标准。**
>
> 所有新需求、新模块、新功能的代码归属，**必须**按本规范判定。
> 当历史代码与本规范冲突时，以本规范为准，并在最近的重构窗口内对齐。
> Code Review 时，审阅者 **应当** 引用本文件的条款来裁决争议。

---

## 0. 如何使用本文件

| 你正在做的事 | 直接看哪一节 |
|---|---|
| 新增一个功能/模块，不知道放哪 | [§3 决策树](#3-决策树新代码该放哪里) |
| 已知道放哪层，想确认该层的细则 | [§2 各层职责](#2-各层职责) |
| 涉及共享服务、全局状态 | [§5 依赖注入规范](#5-依赖注入规范appdeps) |
| 设计新表 / 写 DDL / 持久化数据 | [§8 数据库设计规范](#8-数据库设计规范) |
| 不确定某写法是否被允许 | [§9 反模式清单](#9-反模式清单禁止) |
| 提交 PR 前自检 | [§10 Code Review Checklist](#10-code-review-checklist) |

**约定级别**（参考 RFC 2119）：
- **必须 (MUST)**：违反即拒绝合入
- **应该 (SHOULD)**：违反需在 PR 中给出充分理由
- **可以 (MAY)**：建议，按场景取舍

---

## 1. 分层架构总览

项目采用 **5 层 + 横切** 结构。依赖方向 **必须** 单向自上而下，**禁止** 反向依赖。

```text
┌─────────────────────────────────────────────────────┐
│  传输层 (Transport)        src/server/              │
│  Axum / GraphQL / SSE  ←  HTTP 边界，DTO 转换        │
├─────────────────────────────────────────────────────┤
│  应用层 (Application)      src/agent/  src/tools/   │
│  Agent 构建、用例流程、工具定义                        │
├─────────────────────────────────────────────────────┤
│  领域层 (Domain)           src/domain/  src/model_provider/  │
│  业务规则、领域模型、领域服务、Repository 接口        │
├─────────────────────────────────────────────────────┤
│  基础设施层 (Infrastructure) src/infra/             │
│  DB / Redis / 对象存储 / 日志 / 沙箱 / 外部驱动封装  │
└─────────────────────────────────────────────────────┘
        ↑  横切 (Cross-Cutting) 被所有层引用 ↑
        │  src/config/  src/error.rs  src/llm/         │
```

### 依赖方向铁律

- `server → agent/tools → domain → infra`，**必须** 单向。
- 横切（config / error / llm）**可以** 被任意层引用，但 **不得** 反向依赖业务层。**唯一例外**：`src/llm/` 的模型工厂 `make_model*` 接收 `&model_provider::store::ModelProviderStore` 参数做纯转换（自身无状态、不持有 domain），见 [ADR-008](#adr-008model_provider-归类与-llmmodel_provider-依赖)。
- `src/model_provider/` 是**模型供应商领域上下文**（store / dto / enums / crypto），逻辑上属领域层，因历史与规模原因独立成顶层目录、未挂在 `domain/` 下。
- 同层兄弟模块 **可以** 互相引用，但 **应该** 谨慎，避免循环。

**验证命令**：CR 时用 `grep -r "use crate::server" src/` 确认 `use crate::server` 只出现在 `src/server/` 内部。

---

## 2. 各层职责

### 2.1 传输层 — `src/server/`

| 项 | 内容 |
|---|---|
| **职责** | HTTP 路由、请求反序列化、响应序列化、GraphQL Schema、SSE 流转换 |
| **允许** | 调用应用层/领域层、定义 DTO struct、做 `Result → IntoResponse` 映射 |
| **禁止** | 写业务规则、直接访问 DB/Redis、持有领域模型以外的状态、`use crate::infra::*` 直接调用底层 API（必须通过领域服务或 AppDeps） |
| **目标目录** | `src/server/routes/<feature>.rs`、`src/server/dto/<feature>.rs` |

**规则**：
- Handler 签名 **必须** 使用强类型 `Input/Output` struct，**禁止** 用 `serde_json::Value -> Value`（GraphQL 边界除外）。
- Handler **必须** 通过 Axum `State` 或 `FromRef` 获取依赖，**禁止** 通过模块级全局或 `thread_local` 获取。
- 错误 **必须** 实现 `IntoResponse`，统一映射为 HTTP 状态码。

### 2.2 应用层 — `src/agent/` + `src/tools/`

| 项 | 内容 |
|---|---|
| **职责** | Agent 构建与编排、用例流程（如"查询理解 → 检索 → 生成"）、FunctionTool 定义 |
| **允许** | 组合多个领域服务、定义 Agent 指令 prompt、声明工具 schema |
| **禁止** | 关心 HTTP 细节（状态码、SSE 格式）、直接读写 DB（必须通过领域服务或 Repository） |
| **目标目录** | `src/agent/<use_case>.rs`、`src/tools/<feature>.rs` |

**规则**：
- 每个 Agent **必须** 接收 `&AppDeps`（或其子集），**禁止** 读取 `OnceLock` 全局。
- 同一业务簇的多个 Agent（如 `device_command` / `command_brainstorm`）**应该** 共置在同一子目录。
- Agent 运行时基础组件归 `src/agent/runtime/`：`CortexAgent`（adk `Agent` trait 实现 + system prompt 分层构建）、`WorkspaceMode`（沙箱编排模式），被各业务助手复用，本身不绑定具体业务。`CortexAgent` 按职责拆为子目录 `runtime/cortex_agent/`：`mod.rs`（`run` 主循环 + 对外导出 `CortexAgent`/`CortexAgentBuilder` + 上下文窗口预算/软硬闸压缩编排）、`builder.rs`（结构体与链式装配）、`prompt.rs`（prompt 分层）、`compaction.rs`（上下文压缩 LLM 摘要）、`window.rs`（token 窗口预算 + 软闸 0.9 / 硬闸 0.95 / 提醒 0.15 阈值常量）、`context_tool.rs`（`get_context_remaining` 内建工具，查上下文剩余比例）、`multi_agent.rs`（`spawn_agent`/`wait_agent` 多智能体子任务 + `ChildAgentFactory` 并发限流）、`soft_landing.rs`（压缩软着陆话术）、`thinking.rs`（思考参数兜底）、`llm_call.rs`（LLM 重试）、`tool_exec.rs`（工具执行 + `ToolCtx`）、`hook.rs`/`analytics.rs`/`trim.rs`（运行钩子 / 用量统计 / 输出裁剪）；对外 import 路径不变。`get_context_remaining` 常驻；`spawn_agent`/`wait_agent` 仅当 `[context].max_spawn_depth > 0`（默认 3）时注入，`max_concurrent_children`（默认 3）限并发。
- `src/tools/code/` 是 Codex 风格的沙箱内代码工具集（`read_file` / `list_directory` / `grep` / `edit_file` / `create_file`），根路径由 `WorkspaceMode` 注入并经 `resolve_safe_path` 做安全校验。
- **shell 环境快照**：`src/infra/shell_snapshot.rs` 为每个会话捕获一次 shell 环境（`PATH` / `venv` 等 export 行），物化为 `{data_dir}/shell_snapshots/{session_id}.sh`；`shell_command` 工具每条命令 `source` 该快照复用环境，免重复探测。Windows / 无沙箱 / 构建失败时降级为 `None`。
- `src/tools/` 下同簇多文件的工具 **应该** 聚成子目录：`code/`（沙箱代码工具）、`filter/`（输出语义过滤）、`monitor_plugin/`（Rhai 监控插件工具 + 私有 `validate`）、`shell_command/`（命令执行 + 私有 `safety`）；单一文件的工具（`device_command` / `propose_memory` / `skill_read` 等）保持平铺。
- `src/skill/` 是 Codex 风格文件系统 Skill（loader 发现 + render 渲染目录 + `$name` mention + inject 注入），以渐进式披露方式注入 system prompt。catalog 用 `RwLock` 保护，暴露 `reload()`（重扫磁盘替换内存 catalog）与 `list_skills()`（枚举目录），经 GraphQL `skills` query / `reloadSkills` mutation 暴露给前端管理页（新增 / 修改 Skill 后热重载即可对新会话生效，无需重启）；其他独立 FunctionTool（`propose_memory` / `skill_read` 等）归 `src/tools/<feature>.rs` 平铺（联网搜索工具 `web_search` 已移除）。

### 2.3 领域层 — `src/domain/`

| 项 | 内容 |
|---|---|
| **职责** | 领域模型（struct/enum）、领域服务（业务规则封装）、Repository trait 与实现、外部网关客户端 |
| **允许** | 定义业务规则、聚合多个基础设施工具、暴露纯领域 API |
| **禁止** | 知道 HTTP 框架存在、引用 `axum` / `tokio` runtime 细节（`tokio::sync` 等基础异步原语除外） |
| **目标目录** | `src/domain/<bounded_context>/` |

**领域簇划分**（按业务能力，**不是** 按技术类型）：

```text
src/domain/
  mod.rs
  meta.rs                    # 跨簇共享的领域模型（DeviceMeta 等）
  enum_def.rs                # 跨簇共享的枚举
  permissions.rs             # 命令沙箱/审批策略模型（SandboxMode / ApprovalPolicy，对齐 codex）
  shell_rules.rs             # Shell 命令权限规则（allow/deny/ask 模式匹配 + ShellRuleStore）
  knowledge/                 # 知识库上下文（多 provider 多实例）
    mod.rs                   #   KnowledgeManager（路由器：持 provider 缓存，按 kb_instance_id 分发）
    backend/                 #   知识库检索后端（KnowledgeProvider trait + DifyProvider + BuiltinProvider + schema）
    kb_instance_store.rs     #   kb_instances 表 CRUD（实例配置 + api_key 加密）
    document_store.rs        #   kb_documents 表 CRUD（内置 provider 文档元数据）
    chunk_store.rs           #   kb_chunks 表 CRUD（内置分段预览）
    embedding.rs             #   OpenAiCompatibleEmbeddingProvider
    qdrant_store.rs          #   KnowledgeVectorStore（封装 Qdrant，payload filter 下推）
    uuid_chunker.rs          #   UuidChunker（chunk.id 改 UUID v7）
    dify_client.rs           #   DifyProvider 内部用的 HTTP 网关（局部 DifyConfig 也在此）
  device_catalog/            # 设备目录上下文（厂商/型号缓存，对齐 device 家族）
    mod.rs                   #   CatalogCache（从 system_builtin.device_brand/device_type 加载）
  session/                   # 会话级配置合并存储上下文
    mod.rs                   #   SessionSettingsStore（session_settings 大表：标题 / agent_type / 模型绑定 / 思考级别 / 沙箱+审批 / 助手绑定；取代旧的 session_models / session_assistants / session_thinking_levels / session_permission_policies 4 张小表；列表 SQL 真分页）
  assistant/                 # 自定义助手上下文（助手定义 / 分享 / fork / 模板导入导出）
    mod.rs                   #   AssistantManager + 持久化映射
  mcp/                       # MCP Server 上下文（外部工具桥接）
    mod.rs
  auth/                      # 认证上下文（用户 / 身份提供商 / JWT）
    mod.rs
  memory/                    # 跨会话记忆上下文（memories 已确认 + memory_proposals 待确认建议）
    mod.rs                   #   MemoryStore + MemoryProposalStore（agent 调 propose_memory 工具写入建议 → 前端确认转正）
  audit.rs                   # 审计日志上下文（AuditStore：写操作统一记录——graphql_handler 拦截全部 mutation + REST 登录/注册/注销/shell-approve/upload；仅 INSERT，不缓存、不建表）
```

> 监控（Rhai 插件）不单独成簇：其 Agent 逻辑在 `src/agent/`，**Rhai 插件运行时在顶层 `src/monitor/`**（`PluginManager` / `RhaiMonitorPlugin` / `host_fns` / `plugin_store`，原 `src/plugins/` 已更名以消除「通用插件系统」歧义），进程隔离沙箱在 `src/infra/`，插件脚本与版本持久化复用领域层 Repository，三者协作完成「监控插件」能力。
>
> Skill 系统不在此列：它是文件系统发现 + 渐进式披露注入（Codex 风格），位于顶层 `src/skill/`，属应用层范畴（见 [§2.2](#22-应用层--srcagent--srctools)），不持有 DB 持久化的领域模型，故不作为 domain 簇。

**规则**：
- 同一业务能力的代码 **必须** 放在同一个子目录，**禁止** 拆散到 `src/` 根。
- Repository（DB CRUD 封装）**必须** 与它服务的领域服务共置，**禁止** 平铺在 `src/` 根（历史平铺代码 `session_model_store.rs` 已迁入 `domain/session/`，见 [§11](#11-重构路线图历史代码对齐)；`doc_meta_store.rs` 迁入 `domain/knowledge/` 后随多 provider 改造删除，被 `document_store` + `chunk_store` 取代）。

### 2.4 基础设施层 — `src/infra/`

| 项 | 内容 |
|---|---|
| **职责** | DB 连接池、Redis 客户端、对象存储（S3 兼容：RustFS/MinIO/AWS S3，统一封装截图/上传图/artifact/沙箱快照）、日志/遥测初始化、代码沙箱、外部 MCP 驱动（按工具名前缀接入）封装 |
| **允许** | 暴露通用底层 API、被上层调用 |
| **禁止** | 引用任何业务概念（不知道"设备""知识库""会话"是什么）、反向依赖 `domain`/`agent`/`server` |
| **目标目录** | `src/infra/<tech>.rs`（如 `db.rs` / `redis.rs` / `object_store.rs` / `sandbox.rs`） |

**判定原则**：如果一个模块讲的是"PostgreSQL 连接池"，它属于 infra；如果它讲的是"文档元数据怎么存"，它属于 domain（即使内部用 DB）。**技术栈归 infra，业务数据归 domain。**

> `src/infra/store_base.rs` 是 **Store 基座机制**（`Store` trait 连接池样板 + `new_id()` + `is_unique_violation()`），供各业务 `<Entity>Store` 复用（见 [§8.6](#86-数据访问方式)）；它命名带 `_base` 正是为与 `domain/<context>/<x>_store.rs` 的**业务仓储**区分，避免「store」一词在机制层与业务层撞名。

> **infra 关键模块**（随对象存储接入演进）：
> - `src/infra/object_store.rs` — 对象存储基础设施（基于 `opendal` 封装 S3 兼容：RustFS/MinIO/AWS S3），统一承载截图 / 上传图 / artifact / 沙箱快照的读写与 presigned URL 签发；经 `AppDeps.object_store` 注入，`[object_storage].enabled = false` 或连通自检失败时降级为 `None`（非致命，主程序仍起）。
> - `src/infra/workspace_snapshot.rs` — 沙箱工作目录**会话亲和容灾**：本地 SSD 跑 + 对象存储存全量 `tar.zst` 快照（`workspaces/{sid}/snapshot.tar.zst`）；节点故障切换时新节点拉取最新快照恢复（本地非空即原节点续跑、跳过）；512MB 打包前预估上限防 OOM；解包走临时目录 + 全成功后原子 rename，逐条目拒 `..` / 绝对路径 / symlink / hardlink 防 tar slipping。SSE 流程开跑前 `restore`、`RUN_FINISHED` 后 `upload`、删会话时 `delete`。
> - `src/infra/screenshot_cleanup.rs` — 截图按会话前缀清理（删会话时调 `delete_session_screenshots`，含路径穿越防护与归属校验）；孤儿对象回收交对象存储（RustFS）生命周期规则，不再启动后台扫描任务。
> - `src/infra/shell_snapshot.rs` — 会话级 shell 环境快照：首次执行命令时捕获 `PATH`/`venv` 等 export 行，物化为 `{data_dir}/shell_snapshots/{session_id}.sh`，后续命令 `source` 复用免重复探测；删会话时清理（Windows / 无沙箱 / 构建失败降级 `None`）。
> - 截图存储约定：object key 统一 `screenshots/{session_id}/{file}`；工具层（`tools/screenshot.rs`）在输出截断前把 base64 上传对象存储并替换为 `image_url`，避免巨大 base64 被 UTF-8 硬截断后 JSON 断裂；`/api/screenshots/{session_id}/{file}` serve 时强制登录 + 校验会话归属（adk 按 `user_id` 过滤），对象存储不可用返回 503。

### 2.5 横切 — `src/config/` + `src/error.rs` + `src/llm/`

| 模块 | 职责 |
|---|---|
| `src/config/` | 配置结构定义与加载 |
| `src/error.rs` | 统一错误类型 `AppError` |
| `src/llm/` | LLM 协议客户端 + 工厂（`openai_custom` / `anthropic_custom` 两个自研 client + `make_model*`） |

**规则**：
- 横切模块 **必须** 保持"无状态、无副作用"，仅提供定义和纯函数。
- `src/llm/` 中的工厂函数 **必须** 接收 `&AppDeps` 而非读取全局（见 [§5](#5-依赖注入规范appdeps)）。

**LLM 双协议分发**：`src/llm/` 内置两个自研模型客户端 — `openai_custom::OpenAICustomCompatible`（OpenAI Compatible 协议，`/chat/completions`）与 `anthropic_custom::AnthropicClient`（Anthropic Messages 协议，`/v1/messages`，本地修复了 base_url 与 SSE UTF-8 分包 bug）。走哪条链路由模型**供应商**级的 `model_provider::enums::ProviderProtocol`（`openai_compat` / `anthropic`）决定，在 `make_model_from_resolved` 中按 `resolved.protocol` 分发。新增接入协议时，**必须** 在 `ProviderProtocol` 扩枚举值，并在该分发函数补对应 client 构造分支。

**模型探测（probe）**：`src/model_provider/probe.rs` 是模型存活探测执行器，按模型能力标签 `tags` 分流为 `chat` / `embedding` / `rerank` 三类探测，支持 openai_compat 与 anthropic 双协议（anthropic 仅 `chat`，`embedding` / `rerank` 不支持，直接判 `fail`）。与上文 `make_model_from_resolved` 不同，探测 **不复用** 带重试退避的 LLM 客户端，而是用模块级轻量 `reqwest` 客户端发一次性最小请求 —— 既保证单模型 30s 超时可控，又能在失败时取回原始 HTTP 状态码与上游错误文案。探测专用解析 `ModelProviderStore::resolve_for_probe`（实现在 `store/cache.rs`）**绕过启用状态缓存与自动回退**，直接按 `model_id` 从 DB 取模型 + 供应商（含解密 `api_key`），使被禁用的模型也能被探测到其本身，而非回退到默认模型误报存活。编排层 `server::model_provider::probe_models`（GraphQL `probeModels`）对全部 id **全并发**（`futures::future::join_all`）执行 `resolve_for_probe → probe_one`，每个模型由 `tokio::time::timeout` 包裹 30s 超时（`probe::PROBE_TIMEOUT`），结果不落库、实时返回。

---

## 3. 决策树：新代码该放哪里

**每次新增文件或代码块前，按顺序回答下列问题，命中即停。**

```
[Q1] 这段代码处理的是 HTTP/GraphQL/SSE 协议细节吗？
      ├─ 是 → src/server/                          ✅
      │        （路由、handler、DTO、SSE 转换、IntoResponse）
      └─ 否 → Q2

[Q2] 这段代码定义了某个 Agent 的行为或某个 FunctionTool 吗？
      ├─ 是 → src/agent/ 或 src/tools/             ✅
      │        （prompt、agent 构建、工具 schema、用例编排）
      └─ 否 → Q3

[Q3] 这段代码包含业务规则、领域模型，或封装了业务数据的读写吗？
      ├─ 是 → src/domain/<bounded_context>/        ✅
      │        （领域服务、Repository、领域 struct/enum、外部网关）
      │        └─ 子问题：属于哪个业务簇？
      │             知识库 → domain/knowledge/
      │             设备目录 → domain/device_catalog/
      │             会话 → domain/session/
      │             助手 → domain/assistant/
      │             Skill → src/skill/（文件系统、渐进式披露，非 domain 簇，见 §2.2）
      │             MCP → domain/mcp/
      │             认证 → domain/auth/
      │             记忆 → domain/memory/
      │             审计日志 → domain/audit.rs
      │             Shell 权限规则 → domain/shell_rules.rs
      │             命令沙箱/审批策略 → domain/permissions.rs
      │             监控 → agent/（逻辑）+ monitor/（Rhai 运行时）+ infra/（隔离沙箱），不单独成 domain 簇
      │             新簇 → domain/<new_context>/  （需在 PR 中说明）
      └─ 否 → Q4

[Q4] 这段代码是与具体业务无关的通用技术能力吗？
      ├─ 是 → src/infra/                           ✅
      │        （连接池、客户端、日志、沙箱、第三方驱动）
      └─ 否 → Q5

[Q5] 这段代码是配置、错误类型，或无状态的工厂函数吗？
      ├─ 是 → 横切层
      │        配置 → src/config/
      │        错误 → src/error.rs
      │        LLM 工厂 → src/llm/
      └─ 否 → Q6

[Q6] 这段代码是组合根（装配依赖、启动服务）吗？
      ├─ 是 → src/bootstrap.rs（或 main.rs）       ✅
      └─ 否 → 你大概率误判了，重新走 Q1。
```

### 3.1 常见归属速查表

| 代码类型 | 归属 |
|---|---|
| GraphQL mutation 实现 | `src/server/routes/<feature>.rs` |
| 请求/响应结构体 | `src/server/dto/<feature>.rs` |
| Agent 指令 prompt | `src/agent/<use_case>.rs` |
| FunctionTool（`search_kb` 等） | `src/tools/<feature>.rs` |
| 领域模型 struct（`DeviceMeta`） | `src/domain/meta.rs` |
| 跨簇共享 enum | `src/domain/enum_def.rs` |
| DB CRUD 封装 | `src/domain/<context>/<x>_store.rs` |
| Store 基座机制（连接池样板 / `new_id` / `is_unique_violation`） | `src/infra/store_base.rs`（`impl Store` 复用，勿重复实现） |
| 模型供应商管理（store / dto / enums / AesCodec / probe） | `src/model_provider/`（模型供应商领域上下文，见 ADR-008） |
| LLM 协议客户端 / 模型工厂 `make_model*` | `src/llm/`（`openai_custom` / `anthropic_custom`） |
| Rhai 监控插件运行时（`PluginManager` 等） | `src/monitor/` |
| 外部 API 客户端（Dify 等） | `src/domain/<context>/<x>_client.rs` |
| Shell 命令权限规则（allow/deny/ask） | `src/domain/shell_rules.rs` |
| 命令沙箱/审批策略模型 | `src/domain/permissions.rs` |
| 审计日志（写操作统一记录） | `src/domain/audit.rs`（`AuditStore`：graphql_handler 拦截 mutation + REST 写操作，仅 INSERT） |
| Skill 目录渲染 / `$name` 注入 / `read_skill` 工具 / 热重载 | `src/skill/`（loader / render / inject / mention；`reload()` 重扫磁盘 + `skills`/`reloadSkills` GraphQL） |
| PostgreSQL 连接池初始化 | `src/infra/db.rs` |
| Redis 连接池 | `src/infra/redis.rs` |
| 对象存储（S3 兼容，截图/上传图/artifact/沙箱快照共用，presigned URL 直链） | `src/infra/object_store.rs`（经 `AppDeps.object_store` 注入） |
| 沙箱工作目录会话亲和快照容灾（tar.zst 打包 / 原子解包 / tar slipping 防护） | `src/infra/workspace_snapshot.rs` |
| 截图按会话前缀清理（孤儿对象交对象存储生命周期规则） | `src/infra/screenshot_cleanup.rs` |
| 会话级 shell 环境快照（PATH/venv 复用，免重复探测） | `src/infra/shell_snapshot.rs` |
| 加密工具（AES） | 服务于哪个簇就放哪个簇，**或** `src/infra/crypto.rs`（若跨簇复用） |
| 配置项新增 | `src/config/<section>.rs` |
| 错误变体新增 | `src/error.rs` |
| 共享服务装配 | `src/bootstrap.rs` |

### 3.2 边界争议裁决原则

两个层都说得通时，按下列优先级裁决：

1. **业务优先于技术**：既涉及业务规则又涉及 DB 的代码，放领域层（业务规则）。
2. **边界优先于内部**：既涉及 HTTP 又涉及业务的代码，放传输层（DTO + 调用领域服务）。
3. **共置优先于分散**：同一业务簇的所有代码（含 store / client / service）放同一子目录。
4. **下沉优先于上浮**：能放在下层（更通用）的，不要放在上层（更专用），前提是不违反依赖方向。

---

## 4. 文件与命名规则

1. **模块入口**：每个目录 **必须** 有 `mod.rs`，包含模块文档注释（`//!`）说明职责。
2. **粒度阈值（文件 + 函数）**：
   - **文件**：单文件超过 500 行 **应该** 考虑拆分；超过 1000 行 **必须** 拆分。
   - **函数**：单个函数超过 ~80 行 **应该** 考虑拆分；超过 ~120 行 **必须** 拆分（提取私有子函数，或按职责下放到子模块）。
   - 阈值来源：Rust 社区经验 + 2026-08 后端重构（Phase C）确立的标准。`sse::create_event_stream`(829 行)、`session::list_sessions`(189)、`grep::scan`(202)、`mcp::store::update`(109) 等超长函数均因此被拆。
3. **拆分方向（文件超阈值时）**：**优先** 按职责拆为子模块（子目录），`mod.rs` 只保留入口、路由/handler 编排与跨子模块共享的类型；**禁止** 把单一职责的膨胀文件平铺拆成互不相干的兄弟文件。已落地范例：
   - `model_provider/store.rs`(1292) → `model_provider/store/{mod,providers,models,cache}.rs`
   - `domain/knowledge/mod.rs`(1289) → 先拆 `knowledge/{mod,search,document,faq,faq_helpers,compress}.rs`，后随多 provider 改造 `search`/`document` 并入检索后端 `{dify,builtin}.rs`，并新增 `backend/`、`kb_instance_store`、`document_store`、`chunk_store`、`embedding`、`qdrant_store`、`uuid_chunker`
   - `server/sse.rs`(1657) → `server/sse/{mod,types,tool_display,child_agent,attachment,error,screenshot,stream}.rs`：`mod.rs`(545) 仅留 handler 入口（`handle_run_sse`/`cancel`）+ agent 构建编排（`resolve_run_model`/`build_agent_request`）；原 730 行 `create_event_stream` 事件循环拆至 `stream.rs`，用 `EventSink` 状态机收敛可变状态（text/thinking 块 id、纯下载标记抑制集合、累积正文）并按 Part 类型拆出 emit 子方法；DTO/事件枚举、工具名展示、子 agent 桥接、多模态附件、错误流各独立成模块。对外 `crate::server::sse::{handle_run_sse, cancel, tool_display_name, SseEventMsg}` 路径不变。
   - `agent/runtime/cortex_agent.rs`(1116) → `agent/runtime/cortex_agent/{mod,builder,prompt,compaction,thinking,llm_call,tool_exec}.rs`（`mod.rs` 仅留 `run` 主循环 + 对外导出；对外 import 路径 `runtime::cortex_agent::{CortexAgent, CortexAgentBuilder}` 不变）
   - **同簇平铺文件聚合**（功能簇该聚成子目录，而非带共同前缀的兄弟文件）：`tools/monitor_plugin.rs` + `monitor_plugin_validate.rs` → `tools/monitor_plugin/{mod,validate}.rs`（`validate` 仅内部用，降为私有 `mod validate`）；`tools/shell_command.rs` + `shell_safety.rs` → `tools/shell_command/{mod,safety}.rs`（`safety` 仅内部用，降为私有 `mod safety`）。判定信号：辅助文件只被同簇主文件 `use`，即可聚进子目录并降私有，收敛对外可见面。
4. **拆分约束**：重构 / 拆分 **必须** 保持行为不变（纯移动 + 改 import），**禁止** 顺手改动外部接口；每批拆分独立提交，单独跑 `cargo check` / `cargo test`，独立可回滚。
5. **命名风格**：
   - 业务服务：`<Name>Manager` / `<Name>Service`（如 `KnowledgeManager`）
   - Repository：`<Entity>Store`（如 `DocMetaStore`）
   - 外部客户端：`<Vendor>Client`（如 `DifyClient`）
   - Agent 构建函数：`build_<type>_agent`
6. **可见性**：模块内部用 `pub(crate)`，跨 crate 才用 `pub`；**禁止** `pub` 暴露实现细节。
7. **目录命名**：业务簇用蛇形复数或单数（`knowledge` / `device_catalog`），技术模块用单数（`db` / `redis`）。

---

## 5. 依赖注入规范（AppDeps）

### 5.1 核心规则

> **跨切服务（被多个无关模块读取的服务）必须通过显式依赖注入传递，禁止使用进程级全局变量。**

进程级全局（`OnceLock` / `lazy_static` / `static mut`）只允许在 [§5.4](#54-例外允许使用全局的场景) 列举的场景使用。

### 5.2 标准方案：AppDeps 结构体

```rust
// src/bootstrap.rs
#[derive(Clone)]
pub struct AppDeps {
    pub models: Arc<ModelResolver>,
    pub knowledge: Arc<KnowledgeManager>,
    pub catalog: Arc<CatalogCache>,
    pub plugins: Arc<PluginManager>,   // 来自 src/monitor/（Rhai 监控插件运行时）
    pub db: DbPool,
    pub redis: Option<SharedRedisPool>,
    // 以后新增的跨切服务都加到这里
}

impl AppDeps {
    pub fn make_model(&self, id: Option<&str>) -> anyhow::Result<Arc<dyn Llm>> {
        self.models.resolve(id)
    }
}
```

> **注**：上方为简化示意（字段名取短名）。实际 `AppDeps` 当前含 **24 个字段**（如 `knowledge_manager` / `catalog` / `plugin_manager` / `db_pool` / `redis_pool` / `model_provider_store` / `session_settings_store` / `assistant_store` / `mcp_manager` / `skill_service` / `memory_store` / `memory_proposal_store` / `audit_store` / `object_store` 等，完整列表与按业务簇的 section 注释见 `src/bootstrap.rs`），权威以代码为准。

### 5.3 演进路径

| 阶段 | 触发条件 | 做法 |
|---|---|---|
| **Level 1** 单参数注入 | 跨切依赖 ≤ 2 个 | 直接作为函数参数 |
| **Level 2** AppDeps（默认） | 跨切依赖 ≥ 3 个 | 收敛到一个 struct（当前项目） |
| **Level 3** 子 struct 切分 | AppDeps 字段 ≥ 10 个 | 按业务簇拆 `KnowledgeDeps` / `MonitorDeps` 等 |
| **Level 4** Trait 抽象 | 同一代码跑在多个完全不同的容器 | `trait HasModels` 等泛型约束（本项目一般不需要） |

**当前项目处于 Level 2 → Level 3 的过渡期。**

**新增依赖时的判定流程**：

```
新服务 X 需要被读取
  ├─ 是否被 ≥2 个无关模块（不同业务簇）读取？
  │    ├─ 是 → 加入 AppDeps
  │    └─ 否 → 只在所属业务簇内注入，不进 AppDeps
  └─ AppDeps 已有 ≥10 字段？
       ├─ 是 → 考虑拆子 struct（按业务簇）
       └─ 否 → 直接加字段
```

### 5.4 例外：允许使用全局的场景

仅以下三类 **可以** 使用 `OnceLock` / `task_local!`：

1. **观测类全局**：logger handle、OTLP tracer。生命周期 = 进程生命周期，无业务语义。
2. **请求级上下文**：`trace_id`、`current_user_id`。用 `tracing::Span::current()` 或 `task_local!`，作用域限于单次请求。
3. **不可变进程级常量**：编译期已知、永不变化。

> **`ModelProviderStore` 不属于以上任何一类**（它是可变业务服务），**必须** 走 AppDeps。历史代码 `static GLOBAL_STORE` 需在重构窗口内移除。

### 5.5 与 Axum 的接缝

Handler 层用 `FromRef` 让每个路由只提取它需要的子依赖：

```rust
#[derive(Clone)]
pub struct WebState { pub deps: Arc<AppDeps> }

#[derive(Clone)]
pub struct ModelApi(pub Arc<ModelResolver>);
impl FromRef<WebState> for ModelApi {
    fn from_ref(s: &WebState) -> Self { ModelApi(s.deps.models.clone()) }
}

// 该 handler 编译期就只能拿到 ModelApi，碰不到其他依赖
async fn list_models(State(api): State<ModelApi>) -> impl IntoResponse { ... }
```

**禁止**：让每个 handler 都能 `state.deps.xxx` 摸到全部依赖（God State 反模式）。

> **注**：当前 GraphQL 单入口（60+ resolver）+ 重量级 SSE handler 架构下，`AppDeps` 字段拆分（Level 3）暂缓实施，理由与未来路径见 [ADR-006](#adr-006appstate-字段拆分暂缓--graphql-单入口下的依赖注入策略)。当出现轻量级 REST handler 只需 1-2 个依赖的场景时，应优先用 `FromRef` 提取子依赖，避免回到 God State。

---

## 6. 错误处理规范

| 层 | 错误类型 | 规则 |
|---|---|---|
| 领域层 / 基础设施层 | `AppError`（thiserror） | **必须** 用 `AppError`，定义清晰的变体 |
| 应用层 | `AppError` 或 `anyhow::Error` | 内部用 `?` 传播；对外暴露 `AppError` |
| 传输层 | `IntoResponse` | **必须** 把 `AppError` 映射成 HTTP 状态码 |
| 组合根（bootstrap / main） | `anyhow::Result` | 装配失败可直接 panic / 退出 |

**规则**：
- **禁止** 在领域/基础设施层使用 `anyhow`（丢失错误类型信息）。
- **禁止** 用 `String` 作为错误类型。
- **禁止** `unwrap()` / `expect()` 出现在非测试代码中，除非有 `// SAFETY:` 或 `// INVARIANT:` 注释证明不会 panic。

---

## 7. 日志与可观测性规范

1. **统一使用 `tracing`**：`tracing::info!` / `warn!` / `error!`。
2. **禁止** 在业务代码中使用 `log::xxx!`（`log` crate 仅作为第三方库的桥接）。
3. 结构化字段优先：`info!(user_id = %id, "session created");` 而非字符串拼接。
4. 跨函数调用链 **应该** 使用 `#[tracing::instrument(skip(deps))]` 自动建立 span。
5. 敏感信息（API Key、密码、Token）**绝对禁止** 出现在日志中。

---

## 8. 数据库设计规范

> **本节是所有持久化设计的硬约束，目标是保证从 PostgreSQL 平滑迁移到 MySQL。**
>
> 当前数据库为 PostgreSQL，但 **必须** 在 DDL 层面保持与 MySQL 的兼容性，禁止引入任一方的专有特性。任何违反本节的 DDL 在 PR 中 **必须** 被拒绝。本节只规定方向性约束，不涉及具体库表设计。

### 8.1 主键与 ID 生成

- 主键 **必须** 使用 `VARCHAR(36)` 存储 UUID v7 字符串，**禁止** 使用数据库原生 `UUID` 类型。
- ID **必须** 在应用层通过 `Uuid::now_v7().to_string()` 生成，**禁止** 依赖数据库默认值或生成函数（如 `gen_random_uuid()`、`SERIAL`、`AUTO_INCREMENT`）。
  - 理由：UUID v7 兼具时间有序性与全局唯一性；应用层生成可避免迁移时更换生成策略，并便于在落库前组装外键关系。
- 外键 **应该** 同样使用 `VARCHAR(36)`，并显式声明 `REFERENCES ... ON DELETE` 策略。

### 8.2 类型可迁移性（PostgreSQL → MySQL）

所有列类型 **必须** 在 MySQL 中存在等价物，禁止使用任一方专有类型：

| 场景 | **必须** 使用 | **禁止** 使用 | 原因 |
|---|---|---|---|
| 主键 / 外键 | `VARCHAR(36)` | 原生 `UUID` 类型 | MySQL 无原生 UUID |
| 枚举状态 | `SMALLINT` | `ENUM(...)` / 原生枚举类型 | DB ENUM 修改成本高，且两库行为不一致 |
| JSON / 复杂结构 | `TEXT`（应用层 serde 序列化） | `JSONB` / `JSON` 列类型 | MySQL JSON 行为不一致；`TEXT` 跨库通用 |
| 自增 ID | 应用层 UUID v7 | `SERIAL` / `AUTO_INCREMENT` | 迁移时生成策略冲突 |
| 数组型数据 | 关联表 / `TEXT`（应用层分隔） | `T[]` 数组列类型 | MySQL 不支持数组列 |

### 8.3 枚举字段

- 枚举值 **必须** 以 `SMALLINT` 数字存储，**禁止** 使用字符串或数据库原生 ENUM。
- Rust 侧 **必须** 定义 `enum` 并提供 `as_i16()` / `from_i16()` 双向映射（参考 `model_provider::enums::Status`）。
- 数值约定：`0` 表示禁用/异常态，`1` 表示启用/正常态；新增枚举值 **应该** 追加在末尾，**禁止** 复用已废弃的数字。

### 8.4 时间字段

- 时间列 **必须** 使用 `TIMESTAMPTZ`，默认值 `NOW()`。
- **禁止** 使用 `TIMESTAMP WITHOUT TIME ZONE`（迁移后时区语义会丢失，导致跨时区数据错乱）。

### 8.5 建表方式与 Schema 演进

- 建表 DDL **必须** 统一写入 `migrations/schema.sql`（项目已合并为单一 schema 文件，是 schema 的**权威来源**）。cortex-agent 启动时**不**自动建表，新部署**必须**先执行 `psql -d <db> -f migrations/schema.sql` 再启动程序。
- 所有 DDL **必须** 幂等（`CREATE TABLE IF NOT EXISTS` / `ADD COLUMN IF NOT EXISTS`）：重复执行不报错、不破坏既有数据。`schema.sql` 末尾的「幂等升级」段用于老库补列/清理，新库由 `CREATE TABLE` 直接建出最终状态。
- 各 `<Entity>Store` **禁止** 内联 `ensure_schema()` 之类的建表逻辑；Store 的 `new()` 只负责接收连接池（及必要的 seed 数据初始化）。
- Schema 变更 **应该** 遵循"先兼容（加列）→ 迁移数据 → 再清理（删旧列）"的过渡流程；破坏性 `DROP COLUMN` 必须以 `IF EXISTS` 幂等形式写入 `schema.sql` 升级段。

### 8.6 数据访问方式

- 持久化代码 **必须** 归属领域层，以 `<Entity>Store` 形式封装，共置于 `src/domain/<context>/`（见 [§2.3](#23-领域层--srcdomain)）。
- 新增 `<Entity>Store` **必须** 复用公共基础设施（[`src/infra/store_base.rs`](../src/infra/store_base.rs)）：实现 `crate::infra::store_base::Store` trait（只需实现 `pool()`，即获得默认 `get_conn()`）；ID 生成调 `infra::store_base::new_id()`，唯一键冲突判定调 `infra::store_base::is_unique_violation()`。**禁止** 各 store 重复实现 `get_conn` / `new_id` / `is_unique_violation` 样板。store 的 `new()` 只负责接收连接池（及必要的 seed 数据初始化），不建表（见 [§8.5](#85-建表方式与-schema-演进)）。
- 查询 **必须** 通过 `diesel::sql_query` + `QueryableByName` 执行原生 SQL；行映射结构体 **必须** 派生 `QueryableByName` 并显式标注 `#[sql_type = "..."]`。
- **禁止** 在领域层之外（传输层 / 应用层）直接拼 SQL 或调用 `sql_query`。

### 8.7 敏感数据存储

- 密钥、Token、第三方 `client_secret` 等敏感字段 **必须** 经 `AesCodec`（AES-256-GCM）加密后以 base64 字符串存储，**禁止** 明文落库。
- 加密密钥 **必须** 来自配置（`SecurityConfig.aes_key`），**禁止** 硬编码在源码中。
- 解密后的明文 **只能** 存在于内存（运行时缓存），**禁止** 持久化或写入日志。

---

## 9. 反模式清单（禁止）

以下写法在 PR 中 **必须** 被拒绝：

| # | 反模式 | 正确做法 |
|---|---|---|
| 1 | 在 `src/` 根平铺业务文件（如 `xxx_store.rs`） | 归入 `src/domain/<context>/` |
| 2 | 用 `static GLOBAL_XXX: OnceLock<...>` 存业务服务 | 通过 `AppDeps` 注入 |
| 3 | Handler 用 `Value -> Value` 签名 | 定义强类型 `Input/Output` struct（**仅 REST handler**；GraphQL resolver 的 `JSON` 标量透传见 §2.1 豁免 + [ADR-007](#adr-007graphql-json-标量透传保留--前端契约优先)） |
| 4 | `AppState` 堆 15+ 字段（God Struct） | 按 `FromRef` 切分子依赖 |
| 5 | `use crate::server` 出现在 `server/` 之外的模块 | 依赖方向倒置，立即纠正 |
| 6 | 业务代码 `use crate::infra::*` 直接调用底层 API | 通过领域服务或 Repository 间接调用 |
| 7 | 业务代码里 `log::info!` | 改用 `tracing::info!` |
| 8 | 领域/基础设施层用 `anyhow` 作返回类型 | 改用 `AppError` |
| 9 | 非测试代码 `unwrap()` / `expect()` 无注释 | 用 `?` 或显式错误处理 |
| 10 | 同业务簇代码拆散到多个顶层目录 | 共置在同一 `domain/<context>/` |
| 11 | `pub` 暴露本应内部的实现细节 | 用 `pub(crate)` |
| 12 | 组合根逻辑散落在 `server::run` 中 | 集中到 `src/bootstrap.rs` |
| 13 | 主键用 `SERIAL` / `AUTO_INCREMENT` / 原生 `UUID` / `gen_random_uuid()` | `VARCHAR(36)` + 应用层 `Uuid::now_v7().to_string()`（见 [§8.1](#81-主键与-id-生成)） |
| 14 | 用 `JSONB` / `JSON` 列类型 / 数据库原生 `ENUM` | `TEXT` 存 JSON + `SMALLINT` 存枚举（见 [§8.2](#82-类型可迁移性postgresql--mysql)、[§8.3](#83-枚举字段)） |
| 15 | 在领域层之外直接拼 SQL / 调用 `sql_query` | 通过 `<Entity>Store` 封装（见 [§8.6](#86-数据访问方式)） |
| 16 | 敏感字段明文落库 / 加密密钥硬编码 | `AesCodec` 加密 + 密钥来自配置（见 [§8.7](#87-敏感数据存储)） |
| 17 | 在 Store 代码内联建表 DDL（`ensure_schema()` 等） | DDL 统一写入 `migrations/schema.sql`（见 [§8.5](#85-建表方式与-schema-演进)） |
| 18 | 新 store 重复实现 `get_conn` / `new_id` / `is_unique_violation` 样板 | `impl crate::infra::store_base::Store` + 复用 `infra::store_base::{new_id, is_unique_violation}`（见 [§8.6](#86-数据访问方式)） |
| 19 | 单个函数超过 ~120 行未拆分（或超 ~80 行未评估拆分） | 提取私有子函数 / 按职责拆子模块（见 [§4](#4-文件与命名规则)） |

---

## 10. Code Review Checklist

提 PR 前，作者与审阅者 **应该** 逐项确认：

- [ ] 新代码归属符合 [§3 决策树](#3-决策树新代码该放哪里)
- [ ] 依赖方向未倒置（无 `use crate::server` 出现在下层）
- [ ] 未引入新的进程级全局（或符合 [§5.4](#54-例外允许使用全局的场景) 例外）
- [ ] Handler 使用强类型 DTO，未用 `Value -> Value`
- [ ] 新增错误变体已加入 `AppError`，未用 `anyhow` / `String`
- [ ] 日志使用 `tracing`，未用 `log`
- [ ] 无未注释的 `unwrap()` / `expect()`
- [ ] 敏感信息未出现在日志或错误消息中
- [ ] 新模块有 `//!` 文档注释说明职责
- [ ] 单文件未超过 1000 行
- [ ] 单个函数未超过 ~120 行（超过 ~80 行应已评估拆分，见 [§4](#4-文件与命名规则)）
- [ ] 新增 `<Entity>Store` 已 `impl Store` 并复用 `infra::store_base::{new_id, is_unique_violation}`，未重复实现连接获取样板（见 [§8.6](#86-数据访问方式)）
- [ ] 新的跨切依赖已加入 `AppDeps`（而非新建全局）
- [ ] DDL 符合 [§8 数据库设计规范](#8-数据库设计规范)：`VARCHAR(36)` 主键、应用层生成 ID、`SMALLINT` 枚举、`TEXT` 存 JSON、无 PostgreSQL 专有类型
- [ ] DDL 已写入 `migrations/schema.sql`（程序启动不再自动建表，新部署须先执行 `schema.sql`）；Store `new()` 未内联建表逻辑
- [ ] 敏感字段经 `AesCodec` 加密后存储，密钥来自配置而非硬编码
- [ ] `cargo clippy -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过

---

## 11. 重构路线图（历史代码对齐）

以下历史代码与本规范不一致，**应该** 在对应的重构窗口内对齐。重构 **必须** 保持行为不变（纯移动 + 改 import）。
> 状态截至 2026-08-01（与当前代码库对齐）。

| 优先级 | 当前位置 | 目标位置 | 状态 | 涉及条款 |
|---|---|---|---|---|
| P0 | `src/knowledge.rs` | `src/domain/knowledge/mod.rs` | ✅ 已完成 | §2.3、§3 |
| P0 | `src/dify_client.rs` | `src/domain/knowledge/dify_client.rs` | ✅ 已完成 | §2.3 |
| P0 | `src/doc_meta_store.rs` | `src/domain/knowledge/doc_meta_store.rs` | ✅ 已完成（后随多 provider 改造删除，被 `document_store` + `chunk_store` 取代） | §2.3 |
| P0 | `src/meta.rs` | `src/domain/meta.rs` | ✅ 已完成 | §2.3 |
| P0 | `src/enum_def.rs` | `src/domain/enum_def.rs` | ✅ 已完成 | §2.3 |
| P0 | `src/catalog.rs` | `src/domain/catalog/mod.rs` | ✅ 已完成 | §2.3 |
| P0 | `src/session_model_store.rs` | `src/domain/session/mod.rs` | ✅ 已完成 | §2.3 |
| P0 | `src/app_error.rs` | `src/error.rs`（改名） | ✅ 已完成（v1.2） | §2.5 |
| P1 | `model_provider::GLOBAL_STORE` | 删除，改为 `AppDeps.model_provider_store` 字段 + 参数注入 | ✅ 已完成（v1.2）；`make_model*` 改为接收 `&ModelProviderStore` 参数 | §5.1、§5.4 |
| P1 | `server::run` 中的装配代码 | `src/bootstrap.rs::build_app_deps` | ✅ 已完成（v1.2）；`server::run` 只做路由注册 + TCP 监听 | §2.1、§3 Q6 |
| P1 | `AppState` God Struct | 按 `FromRef` 拆分 | ⚠️ 评估完成，**暂缓实施**（见 [ADR-006](#adr-006appstate-字段拆分暂缓--graphql-单入口下的依赖注入策略)） | §5.5、§9 #4 |
| P1 | Handler 的 `Value -> Value` | 强类型 DTO | ⚠️ 评估完成，**暂缓实施**（见 [ADR-007](#adr-007graphql-json-标量透传保留--前端契约优先)） | §2.1、§9 #3 |
| P2 | 代码中 `log::xxx!` | `tracing::xxx!` | ✅ 已完成（v1.2）；全量替换，`tracing-log` 桥接已启用 | §7 |
| P2 | 仓库根的调试输出文件 | 删除 | ✅ 已完成（v1.2）；`clippy_out.txt` / `clippy2.txt` 已删除 | — |
| P0 | Skill 系统：DB 持久化（`skills` 表 + `assistant.enabled_skills`） | 文件系统 Skill（Codex 风格，`.skills/<name>.md`） | ✅ 已完成（v1.3.0）；渐进式披露 — 目录进 system prompt + `$name` 正文注入 + `read_skill` 工具；旧 DB 持久化设计文档已移除，新设计见 [codex-style-skills-design.md](./superpowers/specs/2026-07-28-codex-style-skills-design.md) | §2.2、§2.3、§3 |

---

## 12. 历史决策记录（ADR 摘要）

记录关键架构决策的背景，避免后续反复讨论。

### ADR-001：采用 5 层分层而非 3 层
- **背景**：项目从单文件 `main.rs` 演化而来，初期所有代码平铺在 `src/` 根。
- **决策**：拆分为 5 层（传输/应用/领域/基础设施/横切），明确依赖方向。
- **理由**：多智能体 + 多数据源 + 多外部依赖的复杂度，3 层（MVC）不足以隔离关注点。

### ADR-002：依赖注入用 AppDeps 而非 DI 框架
- **背景**：早期用 `OnceLock` 全局共享 `ModelProviderStore`。
- **决策**：移除全局，改为 `AppDeps` struct 显式注入。
- **理由**：Rust 无成熟 DI 框架，手动 struct 注入是社区主流；显式依赖提升可测试性且编译期可检。
- **替代方案**：trait 抽象（Level 4）被否决，理由是当前规模过度设计。

### ADR-003：统一用 tracing 而非 log
- **背景**：`log` + `tracing-subscriber`（tracing-log 桥接）双轨并存。
- **决策**：业务代码统一 `tracing`，`log` 仅用于第三方库桥接。
- **理由**：`tracing` 原生支持 span/结构化字段，与 OTLP 遥测一致。

### ADR-004：传输层 GraphQL 统一入口
- **背景**：早期 REST 路由零散。
- **决策**：除流式/健康检查/静态资源外，业务接口统一走 GraphQL 单入口。
- **理由**：减少路由表碎片化，前端解构方式不变。

### ADR-005：数据库保持 PostgreSQL → MySQL 可迁移性
- **背景**：初期按 PostgreSQL 便利特性（原生 UUID、JSONB、ENUM）设计存在专有耦合风险。
- **决策**：所有 DDL 在类型层面保持与 MySQL 兼容（`VARCHAR(36)` 主键、`SMALLINT` 枚举、`TEXT` 存 JSON），ID 在应用层用 UUID v7 生成。
- **理由**：在不牺牲功能的前提下，保留未来切换或双写 MySQL 的能力，避免迁移期重写 schema。
- **替代方案**：全面拥抱 PostgreSQL 专有特性被否决，理由是会锁定数据库选型。

### ADR-006：AppState 字段拆分暂缓 — GraphQL 单入口下的依赖注入策略
- **背景**：架构 §5.5 提出"按 `FromRef` 拆分 AppState 子依赖"，§11 路线图将其列为 P1 待办。`AppDeps` 当前聚合 24 个字段，超过 §5.3 Level 3 阈值（10 个）。
- **决策**：**暂缓实施**字段级拆分（Level 3），保留当前平铺结构。已完成 Level 2（收敛到 `AppDeps` + `bootstrap::build_app_deps` 装配 + 移除 `GLOBAL_STORE` 全局）。
- **理由**：
  1. **GraphQL 单入口模式**：60+ resolver 通过 `ctx.data_unchecked::<Arc<AppDeps>>()` 取整体 state，再传给业务函数。`FromRef` 的"handler 只提取子依赖"模式与 GraphQL Context 注入不兼容 — 字段拆分只会让访问路径变长（`state.adk.session_service` vs `state.adk_session_service`），不会改变 resolver 拿整体的事实。
  2. **REST handler 都是重量级**：保留的 REST 路由（`/api/run_sse`、`/api/brainstorm/generate`、`/api/screenshots/*`、`/api/auth/*`）大多需要 10+ 个依赖（SSE handler 需要几乎所有字段），不构成 §5.5 禁令针对的"只需要 1 个依赖却拿到全部"反模式。
  3. **改动量与收益不匹配**：字段拆分会破坏 100+ 处 `state.xxx` 访问，而收益主要是可读性（按业务簇分组），非依赖隔离。当前 `bootstrap.rs` 已用 section 注释按业务簇组织字段。
- **未来路径**：当出现"轻量级 REST handler 只需 1-2 个依赖"的场景时，再引入 `WebState { deps: Arc<AppDeps> }` + 针对性 `FromRef` impl；或当 REST 路由全部 GraphQL 化后，AppState 退化为 GraphQL Context 内部细节，拆分必要性消失。
- **状态**：§11 标记为 ⚠️ 评估完成，暂缓实施。§5.5 的禁令在当前架构下不构成违反。

### ADR-007：GraphQL `JSON` 标量透传保留 — 前端契约优先
- **背景**：架构 §2.1 写"Handler 签名 **必须** 使用强类型 `Input/Output` struct，**禁止** 用 `serde_json::Value -> Value`（**GraphQL 边界除外**）"，但 §9 #3 与 §11 又把 GraphQL 的 `Value -> Value` 列为反模式/待办，**文档自相矛盾**。
- **决策**：**保留** GraphQL `JSON` 标量透传设计（入参 + 返回值均用 `Json(serde_json::Value)`），确认 §2.1 的"GraphQL 边界除外"为正确决策；§11 该项标记为 ⚠️ 评估完成、暂缓实施。
- **理由**：
  1. **前端契约依赖**：前端 `gql('{ models }')` 直接选取 GraphQL 根字段，根字段值即 `JSON` 标量，内部是统一信封 `{ code, message, data }`（见 `frontend/src/api/index.js` 的 `gql()` 解包逻辑）。返回值 DTO 化（`Json` → `SimpleObject`）会改变字段解构方式，破坏 35 个 Vue 文件的所有 GraphQL 调用。
  2. **入参类型强耦合**：入参 DTO 化（`input: Json` → `input: XxxInput`）会改变 GraphQL schema 类型，前端 `$input: JSON!` 变量声明失配，需同步修改前端（不可控工作量）。
  3. **业务层已强类型**：GraphQL resolver 内部已用强类型 struct 接收业务函数（`KbUploadRequest`、`CreateProviderRequest`、`CreateSkillInput` 等），仅在边界做一次 `serde_json::from_value`；这等价于 `InputObject` 派生的效果，只是多了一行样板代码。
  4. **统一信封是有意设计**：`{ code, message, data }` 信封让前端按 `code` 判定成败、`data` 取业务 payload，业务错误不抛 GraphQL `errors`（避免前端 try/catch 样板）。强类型化会破坏这个契约。
- **未来路径**：当前端具备类型同步能力（如迁移到 codegen 工具链 `graphql-codegen`）时，可考虑渐进 DTO 化：先对返回值定义 `Envelope<T>` 泛型 `SimpleObject`，再对入参派生 `InputObject`，前端类型由 codegen 自动生成。
- **状态**：§11 标记为 ⚠️ 评估完成，暂缓实施。§2.1 的"GraphQL 边界除外"豁免确认为长期设计决策。

### ADR-008：`model_provider` 归类与 `llm`→`model_provider` 依赖
- **背景**：v1.7 命名梳理发现 `src/llm/`（横切）的工厂 `make_model*` 依赖 `src/model_provider/`（`use crate::model_provider::store::ModelProviderStore` 解析模型配置），与 §1「横切不得反向依赖业务层」字面相抵触；且 `model_provider` 此前未在任何分层位置归类。
- **决策**：
  1. `src/model_provider/` 明确归类为**模型供应商领域上下文**（领域层）。它含 `<Entity>Store` 持久化、DTO、数字枚举、AesCodec 加密，完全符合领域簇特征；因历史与规模原因独立成顶层目录，未挂在 `domain/` 下。
  2. `llm`→`model_provider` 的依赖**保留为例外**：`make_model*` 仅**接收** `&ModelProviderStore` 参数做纯转换（store 由 `AppDeps` 注入），`llm` 自身无状态、不持有任何 domain 对象、无副作用，符合横切「无状态纯函数」本质。该例外已在 §1「依赖方向铁律」中显式标注。
- **理由**：工厂函数需要模型配置才能构造 client，而配置的唯一数据源是 `ModelProviderStore`；参数注入使该依赖显式、可测试（与 §5 DI 规范一致）。强行把 `make_model*` 下沉进 `model_provider`、或把 `model_provider` 并入 `llm`，都只是挪动 `use` 路径、不改变「工厂需要 store」这一事实，且牵连全部调用点，行为变更风险大于收益。
- **状态**：已落地（v1.7 文档归类 + §1 例外标注），代码依赖方向不变。

---

## 13. 变更本规范

- 本文件的所有修改 **必须** 经过至少一名 maintainer 审阅。
- 涉及分层调整、依赖注入策略变更的修改 **应该** 附带 ADR 记录。
- 历史变更记录在文件末尾维护。

### 变更记录

| 日期 | 版本 | 变更摘要 |
|---|---|---|
| 2026-06-27 | v1.0 | 首次制定：5 层架构、决策树、AppDeps 规范、反模式清单、CR checklist、重构路线图 |
| 2026-06-27 | v1.1 | 新增 §8 数据库设计规范（PG→MySQL 可迁移性、UUID v7 主键、SMALLINT 枚举、TEXT 存 JSON、幂等建表、AesCodec 加密）；补充 §9 反模式 #13-#16、§10 CR 条目、ADR-005；原 §8-§12 顺延为 §9-§13 |
| 2026-07-28 | v1.2 | 重构落地：`app_error.rs → error.rs` 改名；移除 `GLOBAL_STORE` 全局，`make_model*` 改为参数注入；新增 `src/bootstrap.rs` 集中装配（`server::run` 只做路由）；`log::xxx!` 全量替换为 `tracing::xxx!`；删除仓库根调试文件。§11 路线图 5/7 项 ✅，2 项（AppState 拆分 / GraphQL DTO 化）经评估暂缓（见 ADR-006、ADR-007）。 |
| 2026-08-01 | v1.3 | 对齐代码演进：订正 `AppDeps` 字段数为 21（ADR-006）；§2.3 领域簇补 `permissions.rs`（SandboxMode/ApprovalPolicy）、`shell_rules.rs`（ShellRuleStore）两个限界上下文，移除已不存在的 `domain/skill/`；Skill 归属统一改为顶层 `src/skill/`（§2.2、§3、§3.1）；§2.2 补述 `src/agent/runtime/`（CortexAgent + WorkspaceMode）、`src/tools/code/`（Codex 风格代码工具）、`web_search` 工具；§2.5 补 LLM 双协议分发（`ProviderProtocol` 供应商级决定，`make_model_from_resolved` 分发 OpenAI/Anthropic 两条自研 client 链路）；§8.5/§10 改为「DDL 变更统一写入 `migrations/schema.sql`」（已合并单一文件）；移除指向已删除的 `skill-management.md` 的失效链接。 |
| 2026-08-01 | v1.4 | §8.5 反转：建表 DDL 统一由 `migrations/schema.sql` 管理，移除各 store 的 `ensure_schema()`（9 处：auth/assistant/mcp/session×2/shell_rules/model_provider/plugin/doc_meta），cortex-agent 启动不再自动建表（新部署须先执行 `schema.sql`）；补全 `schema.sql` 缺失的 5 张表（`session_models`/`session_assistants`/`shell_rules`/`llm_providers`/`llm_models`）与增量列；§9 新增反模式 #17、§10 CR 同步更新。 |
| 2026-08-01 | v1.5 | 后端重构 Phase B/C 固化为规范（避免每次开发又写出需重构的代码）：§4 补**函数粒度阈值**（~80 行应拆 / ~120 行必拆）与「文件超阈值优先按职责拆子模块」指引（附 `model_provider/store`·`knowledge`·`sse` 三处落地范例），并要求拆分保持行为不变、独立提交；§8.6 补 **Store 公共基础设施**规则（新 store 必须 `impl crate::infra::store::Store`，复用 `new_id()` / `is_unique_violation()`，禁止重复样板）；§9 新增反模式 #18（store 样板重复）/ #19（长函数未拆）；§10 checklist 同步。依据 [后端重构设计 spec](./superpowers/specs/2026-08-01-backend-refactor-design.md) Phase B/C。 |
| 2026-08-02 | v1.6 | 对齐知识库多 provider 改造：§2.3 `domain/knowledge/` 文件清单更新（删 `doc_meta_store.rs`，补 `provider/`、`kb_instance_store`/`document_store`/`chunk_store`/`embedding`/`qdrant_store`/`uuid_chunker`，`KnowledgeManager` 注明为多 provider 路由器、`dify_client.rs` 注明为 `DifyProvider` 内部用）；§4 拆分范例补述 `search`/`document` 后续并入 `provider/`；§2.3 共置规则注释与 §11 路线图均补注 `doc_meta_store.rs` 迁入后已删除。 |
| 2026-08-02 | v1.7 | **命名歧义梳理**（纯改名/移动，行为不变，`git mv` 保留历史）：① `domain/knowledge/provider/` → `backend/`（消除与 `model_provider`、`auth/provider` 的 provider 四义）；② `infra/store.rs` → `store_base.rs`（区分「Store 基座机制」与各业务 `<Entity>Store`）；③ 顶层 `plugins/` → `monitor/`（消除「通用插件系统」误导，实为 Rhai 监控插件运行时）；④ `domain/catalog/` → `device_catalog/`（对齐 device 家族，正名为设备厂商/型号缓存）；⑤ `skill/catalog.rs` → `render.rs`（消除与 `device_catalog` 的 catalog 双义）；⑥ `llm/open_ai_custom_llm.rs` → `openai_custom.rs`（与 `anthropic_custom` 对称）；⑦ `server/kb.rs`/`kb_instances.rs` → `knowledge.rs`/`knowledge_instances.rs`（对齐 server 全称风格）。§1 明确 `model_provider` 为模型供应商领域上下文并补 `llm`→`model_provider` 例外；§2.1/§2.3/§2.5/§3/§4/§8.6/§9 #18/§10 同步路径；新增 [ADR-008](#adr-008model_provider-归类与-llmmodel_provider-依赖)。验证：`cargo check`/`clippy -D warnings`/`cargo test --lib`（386 passed）全绿。 |
| 2026-08-02 | v1.8 | **tools/ 同簇平铺文件聚合为子目录**（纯移动，行为不变，`git mv` 保留历史）：`monitor_plugin.rs` + `monitor_plugin_validate.rs` → `monitor_plugin/{mod,validate}.rs`，`shell_command.rs` + `shell_safety.rs` → `shell_command/{mod,safety}.rs`；`validate`/`safety` 均只被同簇主文件 `use`，降为私有子模块以收敛对外可见面。§2.2 补 tools 子目录组织规则；§4.3 拆分范例补「同簇平铺文件聚合」判定信号。验证：`cargo check`/`clippy -D warnings`/`cargo test --lib`（386 passed）全绿。 |
| 2026-08-02 | v1.9 | **拆分超长文件 `agent/runtime/cortex_agent.rs`**（1116 行，超 §4.2 的 1000 行必拆红线）：按 §4.3 拆为 `runtime/cortex_agent/{mod,builder,prompt,compaction,thinking,llm_call,tool_exec}.rs` 子目录，`mod.rs` 只留 `run` 主循环 + 对外导出（464 行，已回落阈值内）；`CortexAgent` 字段转 `pub(crate)` 供兄弟子模块访问，对外 import 路径不变、调用方零改动。§2.2/§4.3 同步。验证：`cargo check`/`clippy -D warnings`/`cargo test --lib`（386 passed）全绿。 |
| 2026-08-02 | v1.10 | 对齐代码演进（文档全量审查）：补 `domain/memory/` 跨会话记忆簇（§2.3 领域簇清单 + §3 决策树）；§2.2 移除已删除的 `web_search` 工具示例；§5.2 `AppDeps` 示例补现实注（实际 23 字段，权威以代码为准）；ADR-006 字段数订正 21→23。 |
| 2026-08-02 | v1.11 | 同步「模型供应商探测」功能到规范：§2.5 补模型探测执行器说明（`model_provider/probe.rs` 按 `tags` 分流 chat/embedding/rerank + 双协议；不复用带重试退避的 LLM 客户端，改用模块级轻量 `reqwest` 发一次性请求以保障 30s 超时可控与原始 HTTP 错误可取；`ModelProviderStore::resolve_for_probe` 绕过启用缓存与自动回退、含解密 `api_key`，使禁用模型也能被探测到本身；编排层 `server::model_provider::probe_models` 全并发 `join_all` + 单模型 `tokio::time::timeout(30s)`、不落库）；§3.1 速查表 model_provider 模块清单补 `probe`。零新依赖/零新配置，DEPLOY 无需改动。 |
| 2026-08-03 | v1.12 | 文档全量同步近期功能演进（代码为权威）：① **审计日志**：新增 `domain/audit.rs`（`AuditStore` 仅 INSERT，`graphql_handler` 解析 AST 拦截全部 mutation + REST 登录/注册/注销/shell-approve/upload 写操作，异步 spawn、DB 不可用静默跳过、敏感 key 递归脱敏）；落 `audit_logs` 表。§2.3 领域簇补 `audit.rs`、§3 决策树 / §3.1 速查表补审计归属；`AppDeps` 新增 `audit_store` 字段，字段数订正 **23→25**（§5.2 示例、ADR-006 背景）。② **Skill 热重载**：`SkillService` catalog 改 `RwLock`，新增 `reload()` 重扫磁盘 + `list_skills()`；GraphQL 新增 `skills` query / `reloadSkills` mutation（只读枚举 + 热重载，非旧 DB 创建编辑面）；§2.2 / §3.1 同步。③ **删除预检 + 事务级联清理**：删除助手 / 模型 / 供应商 / MCP / 知识库实例统一两段式（`force` 省略=预检返回影响清单，`force=true`=级联清理+删除）；删除模型/供应商额外解绑内置知识库 `embedding_model_id` 引用（回退默认 embedding，需重新向量化）。④ **API Token 删除限制**：Bearer 令牌认证请求仅允许 `deleteSession`，其余删除 resolver 顶部 `reject_api_token_delete` 守卫拒绝。均零新依赖/零新分层，仅 §2.2/§2.3/§3/§3.1/§5.2 + ADR 计数同步，权威以 `src/` 代码为准。 |
| 2026-08-05 | v1.13 | 对齐对象存储（RustFS/S3）接入（代码为权威）：§1 架构图基础设施层补「对象存储」；§2.4 职责/目标目录补 `object_store`，并新增「infra 关键模块」说明（`object_store` 统一承载截图/上传图/artifact/沙箱快照 + presigned URL；`workspace_snapshot` 会话亲和快照容灾 + 512MB OOM 防护 + 原子 rename + tar slipping 防护；`screenshot_cleanup` 按会话前缀清理、孤儿交对象存储生命周期；截图 `{session_id}` 隔离 + serve 鉴权）；§3.1 速查表补三条 infra 归属；§4 `server/sse` 拆分范例订正为 `{mod,compaction,screenshot}.rs`（`repetition_guard` 随退化检测移除）；§5.2 `AppDeps` 举例补 `object_store` 字段；§2.4 移除代码中已不存在的 `zendriver`（浏览器能力改走外部 MCP `browser_*` 按前缀接入）。零新分层，权威以 `src/` 代码为准。 |
| 2026-08-08 | v1.14 | 对齐 session_settings 合并 + 上下文压缩对齐 codex + 多智能体演进（代码为权威）：① **session_settings 合并大表**：`session_models`/`session_assistants`/`session_thinking_levels`/`session_permission_policies` 4 张小表合并为单一 `session_settings`（标题/agent_type/模型绑定/思考级别/沙箱+审批/助手绑定），旧表已 DROP；列表查询改真分页；§2.3 会话簇 `SessionModelStore` → `SessionSettingsStore`。② **上下文压缩对齐 codex**：移除按轮数 / 单轮 token 触发，仅按模型 `context_window` 阈值（软闸 ×0.9 / 硬闸 ×0.95 / 上下文剩余提醒 ×0.15，固化为常量）；`[context]` 段移除 `compaction_interval`/`compaction_overlap`/`intra_token_threshold`/`intra_overlap_events`，仅留 `tool_max_output_bytes`。③ **多智能体**：`spawn_agent`/`wait_agent` 内建工具 + `ChildAgentFactory` 并发限流器（`max_spawn_depth`/`max_concurrent_children` 默认 3）+ `CHILD_AGENT_ACTIVITY` SSE 事件。④ **新增内建工具/事件/REST**：`get_context_remaining` 始终开启；`FILE_ARTIFACT` 事件（shell 输出 `[[ARTIFACT:...]]` 标记 → 文件卡片）；`/api/sessions/{id}/files/{path}` 文件下载；`/api/skills/install`、`/api/skills/upload`（zip/tar/tar.gz）。⑤ **shell 环境快照**：会话级 PATH/venv 复用（`infra/shell_snapshot.rs`）。⑥ **CortexAgent 子模块扩充**（§2.2）：补 `window`/`context_tool`/`multi_agent`/`soft_landing`/`hook`/`analytics`/`trim`。⑦ **AppDeps 字段数订正 25→24**（§5.2 + ADR-006）。⑧ **修复 session_settings 迁移残留**：删除模型/助手/供应商的预检 COUNT 与级联 DELETE 原指向已 DROP 的 `session_models`/`session_assistants`，运行时崩溃；改查 `session_settings.model_id`/`assistant_id`（语义等价：解引用指针而非删行）。零新分层，权威以 `src/` 代码为准。 |
