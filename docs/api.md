# Cortex Agent API 文档

## 概述

Cortex Agent 是一个基于 RAG 架构的网络设备运维智能助手，提供 SSE 流式对话、助手管理、知识库管理、设备目录查询、监控插件管理、模型供应商管理、MCP Server 管理、跨会话记忆、文件系统 Skill（渐进式披露注入 + 目录热重载 + 管理页）、写操作审计日志等功能。

**Base URL**: `http://localhost:8090`（实际端口以 `config.toml` 的 `[server].port` 为准）

---

## 接口形态

系统采用 **「GraphQL 单入口 + 少量保留 REST」** 的形态：

| 形态 | 入口 | 覆盖范围 |
|------|------|---------|
| **GraphQL** | `POST /api/graphql` | 所有业务接口（助手、会话、知识库、设备检索、监控、模型供应商、MCP、Shell 权限规则、任务取消、目录/模型/工具查询等） |
| **REST（SSE）** | `POST /api/run_sse` | 流式对话（SSE 不适合走 GraphQL） |
| **REST（认证）** | `/api/auth/*` | SSO 跳转 / 回调 / Cookie 签发，REST 更自然 |
| **REST（其他）** | `/api/health`、`/api/v1/monitor/health`、`/api/shell-approve`、`/api/uploads`、`/api/skills/install`、`/api/skills/upload`、`/api/sessions/{id}/files/*`、`/assets/*`、`/api/screenshots/*` | 健康检查、Shell 审批、图片上传、Skill 安装/上传、会话文件下载、静态资源、截图 |

> 历史的 REST 业务路由（`/api/sessions`、`/api/kb/*`、`/api/device/search`、`/api/monitor/*`、`/api/v1/monitor/oids`、`/api/v1/monitor/calculate`、`/api/catalog`、`/api/agents`、`/api/models`、`/api/cancel` 等）**已全部迁移到 GraphQL**，原路径不再存在。

---

## 认证

认证在**数据库可用时强制启用、不可关闭**（历史的 `[auth].enabled` 开关已移除）：

- 数据库不可用：认证降级关闭，所有接口无需登录（仅本地开发 / 演示场景）；
- 数据库可用：支持「本地账号（用户名密码）」+ 「SSO（飞书 / 微信 / OIDC）」，登录成功后通过 HttpOnly Cookie 签发 JWT。未登录请求以软身份 `user_id="user"` 放行 **GraphQL**（GraphQL 业务接口级硬 401 暂未强制；前端靠 `/api/auth/me` 跳登录页）；但 REST 的 `/api/uploads`、`/api/screenshots/*` 已强制登录（未登录返回 `UNAUTHORIZED` 1002 / HTTP 401）。

### 认证路由（REST，挂载到 `/api/auth/*`）

| 路由 | 方法 | 说明 |
|------|------|------|
| `/api/auth/providers` | GET | 已配置身份提供商列表 + 本地登录可用性（登录页展示） |
| `/api/auth/login/{key}` | GET | SSO 授权跳转（`{key}` = `{kind}-{name}`，写 CSRF state Cookie 后 302） |
| `/api/auth/callback/{key}` | GET | SSO 回调（校验 state、换身份、签发会话 Cookie） |
| `/api/auth/register` | POST | 本地账号注册（`{username, password, name?}`，首个用户自动成为管理员） |
| `/api/auth/login/local` | POST | 本地账号登录（`{username, password}`） |
| `/api/auth/me` | GET | 当前登录用户（未登录返回 `authenticated:false`） |
| `/api/auth/logout` | POST | 注销（JWT jti 加入 Redis 黑名单 + 清除 Cookie） |

**Cookie 安全策略**：会话 Cookie 为 `HttpOnly; SameSite=Lax; Path=/; Max-Age=TTL`，阻止 XSS 读取；OAuth state Cookie 有效期 5 分钟，仅用于跨请求 CSRF 校验。

### API Token（访问令牌）

除会话 Cookie 外，系统支持**账户 API Token**：用户在「账户设置」页为自己创建多个令牌，外部系统/脚本凭令牌以 `Authorization: Bearer <令牌>` 调用接口，等价登录身份。令牌可设名称、备注、生效时间段（开始-结束）、启用/禁用，可删除。

- **安全模型**：明文令牌 `cxat_<43字符>`（256 bit 熵）**仅在创建那一刻返回一次**，库内只存 SHA-256 哈希（不可逆），列表只展示脱敏前缀。令牌丢失只能删除后重建（与 GitHub / OpenAI PAT 一致）。
- **生效规则**：校验时同时检查令牌 `enabled`、`valid_from`/`expires_at`（留空分别表示立即生效 / 永不过期）、所属用户未被禁用；任一不满足统一返回 401（不区分原因，防探测）。
- **适用范围**：所有挂载认证提取器的接口（`/api/graphql`、`/api/run_sse` 等）均支持 Bearer 令牌；令牌管理接口本身走浏览器会话 Cookie（强制登录）。
- **删除权限受限**：为防止程序化令牌误删核心资源，**通过 Bearer 令牌认证的请求仅允许删除会话**（`deleteSession`）；删除助手 / 模型 / 供应商 / MCP（含批量） / 知识库实例一律返回 `BUSINESS`（2001）拒绝（提示改用账号登录）。账号登录（Cookie JWT）与未登录不受此限。

#### 管理 REST 接口（挂载到 `/api/auth/tokens`，需会话 Cookie 登录）

| 路由 | 方法 | 说明 |
|------|------|------|
| `/api/auth/tokens` | GET | 列出当前用户的全部令牌（脱敏，无明文/哈希） |
| `/api/auth/tokens` | POST | 创建令牌，**返回一次性明文**；body：`{ name, remark?, valid_from?, expires_at? }`（时间 ISO 8601） |
| `/api/auth/tokens/{id}` | PATCH | 更新令牌；body：`{ name, remark?, valid_from?, expires_at?, enabled }` |
| `/api/auth/tokens/{id}` | DELETE | 删除令牌 |

创建成功响应（`data.token` 为仅此一次的明文，前端须立即提示用户复制保存）：

```json
{ "code": 0, "message": "", "data": { "token": "cxat_aB3dXy...", "id": "...", "name": "数据看板接入", "prefix": "cxat_aB3dXy", "enabled": true, "valid_from": null, "expires_at": "2026-12-31T23:59:59+00:00", "last_used_at": null, "created_at": "2026-08-02T10:00:00+00:00" } }
```

外部系统调用示例（令牌认证）：

```bash
curl -H "Authorization: Bearer cxat_aB3dXy..." http://localhost:8090/api/graphql \
     -d '{"query":"{ models }"}'
```

---

## 统一响应信封

所有 GraphQL 业务接口（及内部业务函数）统一返回信封：

```json
{ "code": 0, "message": "", "data": { /* 业务 payload */ } }
```

- `code == 0` 表示成功；非 0 表示错误（见 [错误码](#错误码)）。
- `message`：成功时为空字符串，失败时为可展示给人的错误描述。
- `data`：业务 payload；失败时为 `null`。

> GraphQL resolver 返回 `JSON` 标量，其内部值即为本信封。前端 `gql()` 解包 GraphQL `{ data, errors }` 后，再从信封中拆出 `{ data, code, message }`。

---

## GraphQL 约定

- **入口**：`POST /api/graphql`
- **标量**：所有入参 / 返回值使用 `JSON` 标量（即 `serde_json::Value`）透传，避免为每个字段定义 GraphQL 类型。
- **State 注入**：`AppState` 通过 `Schema::build().data(state)` 注入 GraphQL Context。

请求示例：

```bash
curl -X POST http://localhost:8090/api/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"query { sessions(page:1, pageSize:20) }"}'
```

---

## SSE 流式对话

### POST `/api/run_sse`

SSE 流式对话接口。从请求体读取 `assistant_id` 加载助手配置后，统一走 `build_agent_for_session` 构建 Agent（内置助手与自定义助手同一入口），并写回「会话-助手」绑定。

**Request Body**:
```json
{
  "thread_id": "string",            // 会话 ID（必填，对应 adk-rust SessionId）
  "assistant_id": "string",         // 助手 ID（必填，决定 Agent 构建路径与启用工具）
  "run_id": "string",               // 运行 ID（可选，不填自动生成 UUID）
  "messages": [                     // 消息列表（必填）
    {
      "id": "string",
      "role": "user",
      "content": "string",          // 用户输入内容
      "mentions": [],               // 可选，@ 引用的上下文（文件/符号/选区），由前端构造
      "attachments": [              // 可选，多模态图片附件
        {
          "url": "https://...",  // https://...（<10MB 经 /api/uploads 上传后的 presigned URL，或任意外链）；亦兼容 data:<mime>;base64,... 内联格式
          "mime_type": "image/png"
        }
      ]
    }
  ],
  "tool_decisions": {               // 可选，工具确认决策（工具名 → approve/deny）
    "tool_name": "approve|deny"
  },
  "model_id": "default"             // 可选，模型 ID（请求级覆盖；缺省 / default / auto 走默认模型）
}
```

> **模型选择**（四级优先级，高 → 低）：请求体显式 `model_id` → 会话级绑定（`session_settings.model_id`）→ 助手默认模型（`assistant.model_id`）→ DB 全局默认模型。`model_id` 缺省 / 为空 / `default` / `auto` 时按此顺序回落；否则按 DB 中模型的 `id` 精确匹配。匹配失败时通过 SSE `RUN_ERROR` 事件返回错误。模型解析的唯一数据源是数据库（见 [模型供应商管理](#模型供应商管理)）。

**Response** (SSE Stream):
```
event: message
data: {"type":"RUN_STARTED","thread_id":"...","run_id":"..."}

event: message
data: {"type":"TEXT_MESSAGE_START","message_id":"..."}

event: message
data: {"type":"TEXT_MESSAGE_CONTENT","message_id":"...","delta":"..."}

event: message
data: {"type":"RUN_FINISHED","thread_id":"...","run_id":"...","reason":"complete"}
```

**SSE 事件类型**:
| 事件 | 字段 | 说明 |
|------|------|------|
| `RUN_STARTED` | `thread_id`, `run_id` | 任务开始 |
| `TEXT_MESSAGE_START/CONTENT/END` | `message_id`, `delta` | 文本消息（流式） |
| `THINKING_MESSAGE_START/CONTENT/END` | `message_id`, `delta` | 模型思考过程（流式） |
| `TOOL_CALL_START` | `tool_call_id`, `tool_call_name`, `server_name?` | 工具调用开始（MCP 工具时携带来源 server 名，内置工具缺省） |
| `TOOL_CALL_ARGS` | `tool_call_id`, `delta` | 工具参数（流式） |
| `TOOL_CALL_END` | `tool_call_id` | 工具调用结束 |
| `TOOL_CALL_RESULT` | `tool_call_id`, `tool_name`, `content` | 工具返回结果（`tool_name` 为空时序列化跳过） |
| `TOOL_CONFIRMATION` | `tool_name`, `function_call_id`, `args` | 需要用户确认 |
| `SHELL_APPROVAL_REQUEST` | `approval_id`, `command`, `session_id` | shell 命令需用户审批（前端用 `/api/shell-approve` 回应） |
| `CONTEXT_USAGE` | `prompt_tokens`, `completion_tokens`, `total_tokens`, `threshold` | Token 用量上报（接近 `threshold` 触发上下文压缩） |
| `CONTEXT_COMPACTED` | `compaction_count` | 上下文已自动压缩（本次 run 累计压缩次数；前端可提示「上下文已整理」，≥2 可建议新建会话） |
| `FILE_ARTIFACT` | `path`, `filename`, `title`, `mime`, `size` | 会话工作区产物文件就绪（shell 工具输出 `[[ARTIFACT:...]]` 标记触发，前端据此出文件卡片，下载 URL `/api/sessions/{sid}/files/{path}`） |
| `CHILD_AGENT_ACTIVITY` | `task_name`, `kind`, `tool_call_id?`, `name?`, `delta?`, `args?`, `content?`, `ok?`, `result?` | 子 agent（`spawn_agent`）活动事件，按 `task_name` 聚合渲染「子任务」面板；`kind` ∈ `started` / `text` / `tool_call` / `tool_result` / `finished` |
| `RUN_FINISHED` | `thread_id`, `run_id`, `reason` | 任务完成 |
| `RUN_ERROR` | `message` | 任务出错 |

---

## REST 接口（非 GraphQL）

除 GraphQL 单入口外，保留以下 REST 路由（流式 / 认证 / 健康检查 / 静态资源 / 截图见 [接口形态](#接口形态)）。

### POST `/api/shell-approve`

响应 `SHELL_APPROVAL_REQUEST` 事件，对 shell 命令审批请求给出决策（由 `shell_approval_registry` 解析挂起的审批）。

**Request Body**:
```json
{
  "approval_id": "string",   // 审批请求 ID（来自 SHELL_APPROVAL_REQUEST 事件）
  "decision": "approved"     // approved/approve/yes/true → 通过；其余任意值 → 拒绝
}
```

**Response**: 成功 `{ "code": 0, "data": { "resolved": true } }`；审批请求不存在或已过期返回 `NOT_FOUND`（2002）。

### POST `/api/uploads`

上传图片附件（`multipart/form-data`，字段名 `file`），**上传至对象存储并返回 presigned URL**（带签名有效期），供 `/api/run_sse` 请求体的 `messages[].attachments` 直接引用。

- 单文件 ≤ 10MB（传输层 `DefaultBodyLimit` + 应用层双重校验）；MIME 白名单 `image/png`、`image/jpeg`、`image/webp`、`image/gif`。
- **鉴权**：认证启用时强制登录，未登录返回 `UNAUTHORIZED`（1002）；与 `/api/screenshots/*` 鉴权基线一致。
- 对象存储未启用时返回 `INTERNAL`（5001）"对象存储未启用，无法上传图片"。

**Response**:
```json
{
  "code": 0,
  "data": {
    "url": "https://<对象存储域名>/uploads/<uid>/<uuid>.png?X-Amz-...",
    "filename": "upload.png",
    "mime_type": "image/png",
    "size": 12345
  }
}
```

### GET `/api/screenshots/{session_id}/{filename}`

浏览器 / agent 截图查看接口。截图按会话隔离存储（object key `screenshots/{session_id}/{filename}`），后端从对象存储**代理读取**（不直接暴露对象存储内部）。

- **URL 形态**：必须为 `{session_id}/{filename}` 两段；历史单段扁平格式已失效，直接 404。
- **鉴权**：认证启用时强制登录，且校验当前用户拥有该会话（adk session 按 `user_id` 归属）——未登录 401、无该会话归属 403；认证未启用（单机本地模式）放行。路径段做防穿越校验（拒 `/`、`\`、`..`）。
- **存储不可用**：对象存储未启用时返回 503 `Object Storage Unavailable`。

> 截图的 `image_url`（相对路径 `/api/screenshots/{session_id}/{filename}`）由截图工具 / SSE 层在 `TOOL_CALL_RESULT.content` 中注入，前端凭此相对路径走浏览器会话 Cookie 拉取。

### POST `/api/skills/install` / `/api/skills/upload`

安装 Skill 到磁盘目录（文件系统 Skill，安装后调 `reloadSkills` 热重载即对新会话生效）：

- `/api/skills/install`：body `{ source }`，`source` 为 `cargo install` 包名或 Git 仓库地址，后台编译安装。
- `/api/skills/upload`：`multipart/form-data` 上传 Skill 压缩包，**自动识别 zip / tar / tar.gz** 格式解压安装。

均需会话 Cookie 登录。

### GET `/api/sessions/{session_id}/files/{path}`

会话工作区文件下载：shell 工具输出的 `[[ARTIFACT:path|title|mime]]` 标记文件经 `FILE_ARTIFACT` SSE 事件通知前端，前端凭此路径（相对工作区，多段 `{*path}`）拉取下载。鉴权同截图（登录态 + 会话归属校验）。

---

## GraphQL Query 参考

> 返回值均为 `JSON` 标量，内部为 [统一响应信封](#统一响应信封)。下表只列字段名与语义，具体 payload 形状请参考各领域文档 / 前端调用点。

### 通用

| Query | 参数 | 说明 |
|-------|------|------|
| `models` | — | 可用模型列表（含默认模型 id，数据源为 DB 模型供应商） |
| `catalog` | — | 设备目录（厂商 + 设备类型，`system_builtin` 缓存） |

### 助手（Assistant）

| Query | 参数 | 说明 |
|-------|------|------|
| `assistants` | — | 全部助手（内置 + 自定义） |
| `assistant` | `id` | 单个助手详情 |
| `exploreAssistants` | — | 广场列表（公开助手，脱敏，不含 system_prompt 全文） |
| `assistantByToken` | `token` | 按分享口令查询助手（公开，脱敏） |
| `tools` | — | 自定义助手可勾选工具清单（工具注册表白名单） |

### 会话（Session）

| Query | 参数 | 说明 |
|-------|------|------|
| `sessions` | `page?`, `pageSize?`, `keyword?`, `agentType?`, `kind?`, `assistantId?` | 会话列表（分页 / 关键词 / agent_type / kind / 助手筛选） |
| `sessionHistory` | `id` | 会话历史（含消息序列、待确认项、绑定模型） |
| `sessionThinkingLevel` | `id` | 会话级思考级别（未设置时默认 high） |
| `sessionPermissionPolicy` | `id` | 会话级审批方式（沙箱模式 + 审批策略） |

### 记忆（Memory）

> 跨会话记忆：`memories`（已确认，`scope=0` 用户级 / `scope=1` 助手级）+ `memory_proposals`（agent 经 `propose_memory` 工具写入的待确认建议，前端确认后转正入 `memories`）。会话内信息走 conversation_history，不参与记忆隔离。

| Query | 参数 | 说明 |
|-------|------|------|
| `memories` | — | 当前用户的已确认记忆列表 |
| `memoryProposals` | — | 当前用户的待确认记忆建议列表 |

### 知识库（Knowledge Base）

> 知识库为「多 provider 多实例」架构：实例（Dify 外挂 / 内置 Qdrant）存 `kb_instances` 表，助手绑 `kb_instance_id`，文档操作按实例路由到对应 provider。

| Query | 参数 | 说明 |
|-------|------|------|
| `kbInstances` | — | 知识库实例列表（含 provider 类型、状态） |
| `kbProviderSchema` | — | 各 provider 的 ConfigFieldSpec（前端动态表单渲染用） |
| `kbInstanceDocuments` | `input(JSON)` | 指定实例的文档列表（按 `instance_id` 路由） |
| `kbInstanceSegments` | `instanceId`, `docId` | 指定实例的文档分段预览 |

> 旧接口 `kbDocuments` / `kbDocumentSegments`（dify 直连、无 instance 维度）已废弃删除。

### 设备检索

| Query | 参数 | 说明 |
|-------|------|------|
| `deviceSearch` | `input(JSON)` | 设备语义检索（先 LLM 查询理解，再语义检索） |

### 监控插件（Monitor）

| Query | 参数 | 说明 |
|-------|------|------|
| `monitorPlugins` | — | 列出所有监控插件 |
| `monitorPlugin` | `pluginId` | 获取插件详情（含源码） |
| `monitorPluginVersions` | `pluginId` | 获取插件版本历史 |
| `monitorOids` | `pluginId` | 获取插件 OID 列表（带进程内缓存，容量 10000） |
| `monitorCalculate` | `pluginId`, `oidValues(JSON)` | 计算监控结果（解析采集值） |

> 监控插件的 OID 准备与采集值解析即原「高性能监控 API」，已统一到 GraphQL `monitorOids` / `monitorCalculate`。

### 模型供应商（Model Provider）

| Query | 参数 | 说明 |
|-------|------|------|
| `modelProviders` | — | 供应商列表（含嵌套模型） |

### MCP Server

| Query | 参数 | 说明 |
|-------|------|------|
| `mcpServers` | `page?`, `pageSize?`, `keyword?` | MCP Server 列表（含健康状态） |
| `mcpServer` | `id` | 单个 MCP Server 详情 |
| `mcpTools` | `input(JSON)` | MCP 工具清单查询 |

### Shell 权限规则

| Query | 参数 | 说明 |
|-------|------|------|
| `shellRules` | — | Shell 命令权限规则列表（`decision`：0=Allow 自动放行 / 1=Deny 自动阻断 / 2=Ask 需审批；DB 不可用时返回空数组） |

### Skill

> 文件系统 Skill（Codex 风格）：磁盘目录扫描加载，运行时以渐进式披露方式注入 system prompt（`src/skill/`）。新增 / 修改 Skill 后无需重启，调用 `reloadSkills` 重新扫描即可让**新会话**生效（已建立的会话不会热替换）。Skill 可经 REST `/api/skills/install`（`cargo install` 包名 / Git 仓库）或 `/api/skills/upload`（zip / tar / tar.gz 压缩包）安装到磁盘。

| Query | 参数 | 说明 |
|-------|------|------|
| `skills` | — | 已加载的 Skill 目录列表（`name` / `description` / `short_description` / `scope`，`scope` 为 `builtin` 内置或 `user` 用户目录；Skill 服务未初始化返回 `BUSINESS` 2001） |

> 旧接口 `createSkill` / `updateSkill` 等 DB 持久化的 Skill 管理面已下线（Skill 改为文件系统驱动，只读枚举 + 热重载，不再支持 GraphQL 创建 / 编辑）。

---

## GraphQL Mutation 参考

### 会话（Session）

| Mutation | 参数 | 说明 |
|----------|------|------|
| `createSession` | `input(JSON)` | 创建会话（`assistant_id` 决定会话类型，返回助手 greeting 作为欢迎语） |
| `deleteSession` | `id` | 删除会话（同时清理关联截图） |
| `renameSession` | `id`, `title` | 重命名会话 |
| `updateSessionModel` | `id`, `modelId?` | 更新会话绑定的模型（会话级覆盖） |
| `updateSessionThinkingLevel` | `id`, `level` | 更新会话级思考级别（low/medium/high/xhigh/max） |
| `updateSessionPermissionPolicy` | `id`, `sandboxMode`, `approvalPolicy` | 更新会话级审批方式（沙箱模式 + 审批策略） |

### 流式控制

| Mutation | 参数 | 说明 |
|----------|------|------|
| `cancelRun` | `threadId` | 取消正在运行的 Agent 任务 |

### 记忆（Memory）

| Mutation | 参数 | 说明 |
|----------|------|------|
| `createMemory` | `input(JSON)` | 手动新建记忆（scope / assistant_id / 内容） |
| `updateMemory` | `id`, `input(JSON)` | 更新记忆内容 |
| `deleteMemory` | `id` | 删除记忆 |
| `acceptMemoryProposal` | `id` | 采纳记忆建议（claim + 转正写入 `memories`） |
| `rejectMemoryProposal` | `id` | 拒绝记忆建议 |

### 助手（Assistant）

| Mutation | 参数 | 说明 |
|----------|------|------|
| `createAssistant` | `input(JSON)` | 创建自定义助手 |
| `generateAssistant` | `input(JSON)` | AI 智能生成助手草稿（不落库，仅返回字段供前端填充表单） |
| `updateAssistant` | `id`, `input(JSON)` | 更新自定义助手（内置返回 BUSINESS 2001，HTTP 恒 200，无 403） |
| `deleteAssistant` | `id`, `force?` | 删除自定义助手（内置返回 BUSINESS 2001）。`force` 省略 / false = 仅预检返回影响清单；`force=true` = 事务级联清理（解绑引用）+ 删除。详见 [删除预检与级联清理](#删除预检与级联清理force) |
| `duplicateAssistant` | `id` | 复制助手为自定义副本（仅 custom 生效；内置返回 BUSINESS 2001 被拒） |
| `shareAssistant` | `id` | 生成 / 续用分享口令（仅设置 `share_token`，**不改 `visibility`**） |
| `unshareAssistant` | `id` | 关闭分享口令（仅清空 `share_token`，**不改 `visibility`**） |
| `forkAssistant` | `id` | Fork 公开 / 分享助手到本地（源助手 fork_count+1） |
| `importAssistant` | `input(JSON)` | 导入助手模板 JSON |
| `exportAssistant` | `id` | 导出助手为模板 JSON |
| `bindAssistantKbInstance` | `assistantId`, `kbInstanceId?` | 绑定/解绑助手的知识库实例（内置助手配置知识库用；`kb_instance_id` 空串/None 视为解绑） |

### 知识库（Knowledge Base）

| Mutation | 参数 | 说明 |
|----------|------|------|
| `kbInstanceCreate` | `input(JSON)` | 新建知识库实例（provider 类型 + config JSON，secret 字段加密落库） |
| `kbInstanceUpdate` | `input(JSON)` | 编辑实例配置（`id` 在 `input` 内） |
| `kbInstanceDelete` | `id`, `force?` | 删除实例。`force` 省略 / false = 仅预检返回影响清单（绑定该实例的助手数）；`force=true` = 解绑助手引用 + 删除。详见 [删除预检与级联清理](#删除预检与级联清理force) |
| `kbInstanceTest` | `id` | 连通性探测（health check） |
| `kbInstanceUpload` | `input(JSON)` | 上传文档到指定实例（按 `instance_id` 路由，JSON 文本） |
| `kbInstanceDeleteDocument` | `instanceId`, `docId` | 删除指定实例的文档 |
| `kbLearn` | `input(JSON)` | 从会话生成 FAQ 候选（前端审查，不写库；`instance_id` 不传则取首个启用实例） |
| `kbLearnRegenerate` | `input(JSON)` | 对指定主题重新生成 FAQ 候选（`instance_id` 规则同 `kbLearn`） |
| `kbLearnCommit` | `input(JSON)` | 提交勾选的 FAQ 写入指定实例（重名删旧重建，`instance_id` 规则同 `kbLearn`） |

> 旧接口 `kbUpload` / `kbFeedback` / `deleteDocument`（dify 直连、无 instance 维度）已废弃删除；文档反馈（点赞 / 点踩）能力随 `kbFeedback` 一并移除。

### 监控插件（Monitor）

| Mutation | 参数 | 说明 |
|----------|------|------|
| `registerMonitorPlugin` | `input(JSON)` | 注册 / 覆盖 Rhai 监控插件（返回版本号；`plugin_id` 非法 / 脚本超 64KB 返回 INVALID_PARAMS 1001，编译失败返回 BUSINESS 2001，**无 HTTP 422**） |
| `unregisterMonitorPlugin` | `pluginId` | 注销插件 |
| `rollbackMonitorPlugin` | `pluginId`, `version` | 回滚到指定版本 |

> 监控插件脚本契约、三层校验与版本管理详见 [Rhai 监控插件系统](./rhai-plugin.md)。

### 模型供应商（Model Provider）

| Mutation | 参数 | 说明 |
|----------|------|------|
| `createModelProvider` | `input(JSON)` | 新建供应商 |
| `updateModelProvider` | `id`, `input(JSON)` | 编辑供应商（不含密钥） |
| `deleteModelProvider` | `id`, `force?` | 删除供应商。`force` 省略 / false = 仅预检返回影响清单（其下模型数 + 绑定助手 / 会话数）；`force=true` = 事务级联删除其下全部模型并解绑引用（含 embedding 引用）。详见 [删除预检与级联清理](#删除预检与级联清理force) |
| `resetModelProviderKey` | `id`, `input(JSON)` | 重置供应商 API Key |
| `createModel` | `providerId`, `input(JSON)` | 新建模型 |
| `updateModel` | `id`, `input(JSON)` | 编辑模型 |
| `deleteModel` | `id`, `force?` | 删除模型。`force` 省略 / false = 仅预检返回影响清单（绑定该模型的助手 / 会话数 + 用其做 embedding 的内置知识库数）；`force=true` = 事务级联解绑助手 / 会话引用，并从内置知识库 config 移除 `embedding_model_id`（回退默认 embedding，旧向量维度可能不匹配、需重新向量化）。详见 [删除预检与级联清理](#删除预检与级联清理force) |
| `setDefaultModel` | `id` | 设为默认模型 |
| `setEmbeddingDefaultModel` | `id` | 设为默认 embedding 模型（内置 KB provider 检索用） |
| `probeModels` | `input(JSON)` | 批量探测模型存活（`input.ids` 为模型 id 数组，可跨供应商；返回 `data.results`，全并发、单模型 30s 超时、不落库实时返回；详见 [probeModels 返回结构](#probemodels-返回结构)） |

### MCP Server

| Mutation | 参数 | 说明 |
|----------|------|------|
| `createMcpServer` | `input(JSON)` | 新建 MCP Server |
| `updateMcpServer` | `id`, `input(JSON)` | 编辑 MCP Server |
| `deleteMcpServer` | `id`, `force?` | 删除 MCP Server。`force` 省略 / false = 仅预检返回影响清单（绑定该 Server 的助手数）；`force=true` = 解绑助手引用 + 删除。详见 [删除预检与级联清理](#删除预检与级联清理force) |
| `probeMcpServer` | `id` | 手动探测（强制重连 + 工具发现） |
| `batchSetMcpStatus` | `input(JSON)` | 批量设置状态（ids 为 null 表示全选匹配项） |
| `batchDeleteMcpServers` | `input(JSON)` | 批量删除（ids 为 null 表示全选匹配项）；**API Token 认证拒绝**（见 [认证 · API Token](#api-token访问令牌)） |
| `batchProbeMcpServers` | `input(JSON)` | 批量探测（仅支持指定 ID 列表） |

### Shell 权限规则

| Mutation | 参数 | 说明 |
|----------|------|------|
| `createShellRule` | `input(JSON)` | 新建规则（input：`pattern`、`decision` 0=Allow/1=Deny/2=Ask、`priority?` 默认 0；DB 不可用返回 BUSINESS 2001） |
| `deleteShellRule` | `id` | 删除规则（不存在返回 NOT_FOUND 2002） |

### Skill

| Mutation | 参数 | 说明 |
|----------|------|------|
| `reloadSkills` | — | 热重载 Skill 目录（重新扫描磁盘、替换内存 catalog）。新增 / 修改 Skill 后点一次即可让**新会话**生效，无需重启；Skill 服务未初始化返回 `BUSINESS` 2001。返回 `{ "reloaded": true }` |

---

## 删除预检与级联清理（force）

删除助手 / 模型 / 供应商 / MCP Server / 知识库实例等资源可能存在悬挂引用，直接硬删会破坏关联数据。这些删除 mutation 统一采用 **「预检 → 确认 → 执行」** 两段式，由可选 `force` 参数控制：

| `force` | 行为 | 返回 |
|---------|------|------|
| 省略 / `false` | **仅预检**，不删除 | `{ "deleted": false, "impact": { ... }, "summary": "人类可读摘要" }` |
| `true` | **事务级联清理** + 删除 | `{ "deleted": true, "cleanup": { ... } }` 或目标不存在返回 `NOT_FOUND` 2002 |

**前端约定流程**：首次调用不带 `force` → 渲染 `summary` 提示影响 + 二次确认 → 用户确认后带 `force: true` 再调一次完成删除。预检本身不改任何数据，可安全重复调用。

各资源的影响维度（预检返回的 `impact` 字段）：

| 资源 | `impact` 字段 | `force=true` 级联动作 |
|------|--------------|----------------------|
| 模型 `deleteModel` | `assistants`、`sessions`（结构化计数；用其做 embedding 的内置知识库数仅体现在 `summary`） | 解绑助手 / 会话模型引用（回退默认模型）；从内置知识库 config 移除 `embedding_model_id`（回退默认 embedding，**需重新向量化**） |
| 供应商 `deleteModelProvider` | `models`、`assistants`、`sessions` | 级联删除其下全部模型（含上述模型级清理） |
| 助手 `deleteAssistant` | 绑定该助手的会话数等 | 清理关联会话引用 + 删除 |
| MCP Server `deleteMcpServer` | 绑定该 Server 的助手数 | 解绑助手 MCP 引用 + 删除 |
| 知识库实例 `kbInstanceDelete` | 绑定该实例的助手数 | 解绑助手 `kb_instance_id` + 删除 |

> `deleteModel` / `deleteModelProvider` / `deleteAssistant` / `deleteMcpServer` / `kbInstanceDelete` 五个删除均受 **API Token 删除限制**（见 [认证 · API Token](#api-token访问令牌)）：Bearer 令牌认证一律拒绝，需账号登录。

---

## 审计日志

系统对所有**增删改类写操作**统一记录审计日志（落 `audit_logs` 表），用于追溯「谁、何时、做了什么、结果如何」。

- **覆盖范围**：
  - **GraphQL**：`graphql_handler` 统一拦截所有 mutation（解析 AST 判定写操作，55+ 个 mutation 自动全覆盖，无需逐个埋点）；
  - **REST**：认证接口的登录 / 注册 / 注销（成功与失败均记，失败时 `user_id` 为空、靠 `actor` 记 username + IP）、`/api/shell-approve`（Shell 审批决策）、`/api/uploads`（图片上传）。
- **记录字段**：`user_id`（操作者）、`actor`（显示名 / username）、`source`（`web` 账号登录 / `api_token` 程序化 Bearer）、`operation`（mutation 名如 `deleteSession`，或 REST 动作如 `login` / `upload_image`）、`target_id`（被操作对象 id，批量操作取 `ids` 拼接）、`success`（GraphQL 执行层是否成功）、`detail`（**脱敏后**的参数 JSON）、`ip`、`created_at`。
- **脱敏**：`password` / `api_key` / `apikey` / `token` / `secret` / `authorization` 等敏感 key 的值递归替换为 `"***"`，明文绝不入库。
- **可靠性**：审计写入 **异步**（`tokio::spawn`，不阻塞业务响应）；DB 不可用时静默跳过；写入失败仅丢弃日志、绝不影响业务主流程。

> 审计日志目前仅供后端 / DB 查询，暂未暴露 GraphQL 查询接口。建表 DDL 在 `migrations/schema.sql` 的 `audit_logs`（部署时执行，启动不自动建表）。

---

## 模型供应商管理

> **重要变更**：模型（LLM）配置**不再从配置文件 `[llm]` 段读取**，统一由数据库「模型供应商」管理（`model_provider` 模块）。`ModelProviderStore` 由 `bootstrap::build_app_deps` 装配、经 `AppDeps` 注入（**不再是进程级全局**），作为模型解析的唯一数据源。

### 工作方式

1. 在「模型供应商」页面（`modelProviders` / `createModelProvider` 等）配置供应商（名称、base_url、API Key）与其下的模型；
2. API Key 经 `AesCodec`（AES-256-GCM）加密后以 base64 存储，密钥来自 `[security].aes_key`（可用环境变量 `MODEL_AES_KEY` 覆盖）；
3. 其中一个模型可标记为「默认模型」；
4. `GET models`（GraphQL `models`）返回供应商下的所有模型，第一项为默认模型；
5. 对话时 `model_id` 缺省 / `default` / `auto` → 默认模型；否则按 DB 模型 `id` 精确匹配，未匹配到时 SSE 立即返回 `RUN_ERROR`。

### `models` 返回示例

```json
{
  "code": 0,
  "message": "",
  "data": {
    "default_model_id": "<默认模型 id>",
    "models": [
      {
        "id": "<模型 id>",
        "name": "GLM-5.2",
        "model": "GLM-5.2",
        "provider_name": "openai-compatible",
        "is_default": true
      }
    ]
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `id` | string | 模型唯一标识，对话 `model_id` 使用此值 |
| `name` | string | 前端展示名 |
| `model` | string | 实际下发给 OpenAI 兼容接口的 `model` 参数 |
| `provider_name` | string | 模型提供方标识，用于日志 / 遥测 |
| `is_default` | bool | 是否为默认模型 |

> 数据库不可用时，模型供应商存储为 `None`，`models` 返回错误码 `LLM`（4002）。

### `probeModels` 返回结构

探测模型存活（`probeModels(input: { ids: [...] })`）。`input.ids` 为模型 id 数组，可跨供应商批量探测；全并发执行，单模型 30s 超时，结果不落库、实时返回。被禁用的模型同样会被探测到其本身（解析绕过启用缓存与回退，详见 architecture.md §2.5 模型探测）。

返回 `data.results` 为 `ProbeResult` 数组：

```json
{
  "code": 0,
  "message": "",
  "data": {
    "results": [
      {
        "model_id": "<模型 id>",
        "model": "deepseek-chat",
        "provider_name": "openai-compatible",
        "status": "ok",
        "latency_ms": 320,
        "probe_kind": "chat",
        "error": null,
        "probed_at": "2026-08-02T10:00:00Z"
      }
    ]
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `model_id` | string | 模型唯一标识（与 `models[].id` 一致） |
| `model` | string | 实际下发给上游的 `model` 参数 |
| `provider_name` | string | 供应商名称 |
| `status` | string | 探测结果：`ok` / `fail` |
| `latency_ms` | number | 单次探测耗时（毫秒） |
| `probe_kind` | string | 探测分流（由模型 tags 决定）：`chat` / `embedding` / `rerank` |
| `error` | string \| null | 失败时的可操作错误信息（成功为 `null`） |
| `probed_at` | string | 探测时间（RFC3339） |

> `probe_kind` 由模型能力标签 `tags` 决定：含 `chat` 走对话探测，否则含 `embedding` 走向量探测，否则含 `rerank` 走重排探测，否则兜底 `chat`。Anthropic 协议仅支持 `chat` 探测，`embedding` / `rerank` 直接判 `fail`。

---

## 错误码

业务错误码（信封中的 `code`，按千位分段）：

| code | 常量 | 说明 |
|------|------|------|
| `0` | `OK` | 成功 |
| `1001` | `INVALID_PARAMS` | 参数校验失败（缺失、非法值） |
| `1002` | `UNAUTHORIZED` / `PARSE_ERROR` | 未认证（未登录）；入参反序列化 / 解析失败（**两者当前共用 1002 码位，待后续拆分**） |
| `2001` | `BUSINESS` | 通用业务规则错误 |
| `2002` | `NOT_FOUND` | 目标资源不存在 |
| `2003` | `CONFLICT` | 冲突（唯一约束、重复操作） |
| `3001` | `DATABASE` | 数据库（连接 / 查询 / 持久化）错误 |
| `4001` | `NETWORK` | 网络 / 上游 HTTP（如 Dify）错误 |
| `4002` | `LLM` | LLM 相关错误（模型解析、调用失败） |
| `4003` | `TIMEOUT` | 超时 |
| `5001` | `INTERNAL` | 内部错误（配置、文件、初始化） |
| `5999` | `UNKNOWN` | 未知兜底 |

> 监控插件接口已 100% GraphQL 化（除 `/api/v1/monitor/health` 健康检查外无 REST 监控路由），校验失败统一走信封错误码（`INVALID_PARAMS` 1001 / `BUSINESS` 2001），**不再产生 HTTP 400/413/422**。

---

## 示例

### GraphQL 示例

```bash
# 1. 创建会话（绑定助手）
curl -X POST http://localhost:8090/api/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"mutation { createSession(input: {assistant_id:\"01950000-0000-7000-8000-000000000001\"}) }"}'

# 2. 查询可用模型
curl -X POST http://localhost:8090/api/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"{ models }"}'

# 3. 获取设备目录
curl -X POST http://localhost:8090/api/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"{ catalog }"}'

# 4. 设备语义检索
curl -X POST http://localhost:8090/api/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"{ deviceSearch(input: {query:\"静态路由\", brand:\"H3C\", dev_type:\"router\"}) }"}'

# 5. 获取插件 OID 列表（原高性能 API）
curl -X POST http://localhost:8090/api/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"{ monitorOids(pluginId:\"sysuptime-h3c\") }"}'
```

### SSE 流式对话示例（JavaScript）

```javascript
async function chat(messages, modelId = 'default') {
  const response = await fetch('http://localhost:8090/api/run_sse', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      thread_id: 'session-' + Date.now(),
      messages: messages,
      model_id: modelId
    })
  });

  const reader = response.body.getReader();
  const decoder = new TextDecoder();

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    const text = decoder.decode(value);
    const lines = text.split('\n');

    for (const line of lines) {
      if (line.startsWith('data: ')) {
        const data = JSON.parse(line.slice(6));
        console.log('Event:', data.type, data);
      }
    }
  }
}

chat([{ id: '1', role: 'user', content: 'H3C静态路由怎么配置' }]);
```

---

## 前端模型选择

前端在会话页顶部提供模型下拉框，行为如下：

1. 进入聊天页时调用 GraphQL `models` 拉取列表并渲染下拉；
2. 默认选中：
   - 当前会话绑定的模型（服务端持久化于 `session_settings.model_id`，经 `updateSessionModel` mutation 写入）；
   - 未绑定时回落 `models` 返回的 `default_model_id`；
3. 用户手动切换模型：写入会话级绑定（`updateSessionModel`），对后续消息生效；
4. 发送消息（`/api/run_sse`）的 `model_id` 为请求级覆盖（缺省走会话绑定 / 默认模型）；
5. 切换会话时，按会话绑定重新挑选并刷新下拉选中项。

> 历史的「localStorage 键 `cortex_session_model_<sessionId>` / `cortex_selected_model_id`」方案已废弃——模型选择改为**服务端持久化**（`session_settings.model_id` + `updateSessionModel`），不再依赖浏览器本地存储。
