# cortex-agent 部署与使用说明

## 一、系统架构

### 1.1 架构概览

```text
HTTP 请求 (Axum)
     │
     ▼
业务接口统一走 GraphQL（POST /api/graphql）；SSE 流式 / 健康检查 / 认证 / 上传 / Shell 审批保留 REST
     │
     ▼
会话运行（/api/run_sse）：读 session 绑定的 assistant_id → 加载助手 → 分发构建 Agent
  ├─ 内置助手：device_command / monitor_plugin
  ├─ 自定义助手：build_custom_agent（用户系统提示词 + shell_command 工具 + 生成参数）
  ├─ 工具调用：search_kb（知识库检索）、query_device_catalog（设备目录）、
  │            validate_monitor_plugin（监控插件校验）、shell_command（命令执行，带会话级 shell 环境快照复用 PATH/venv）、
  │            代码工具（read_file / list_directory / grep / edit_file / create_file，沙箱内）、
  │            propose_memory（记忆建议）、read_skill（Skill 渐进式披露）、
  │            get_context_remaining（上下文剩余查询）、spawn_agent/wait_agent（多智能体子任务）、
  │            screenshot（截图，落对象存储）、MCP 工具（外部按前缀接入）等
  ├─ 知识检索：多 provider（Dify 外挂 + 内置 Qdrant，按 kb_instance 路由）
  └─ 会话持久化：PostgreSQL（Session + 业务表）+ Redis（Memory / SNMP 队列 / JWT 黑名单）
     │
     ▼
SSE 流式响应 ──► 前端
```

> 模型（LLM）配置统一由数据库「模型供应商」管理，不再写于配置文件。系统还内置 Rhai 监控插件运行时（LLM 即写即跑生成网络设备监控采集逻辑）、文件系统 Skill（渐进式披露注入 + 目录热重载）、以及写操作审计日志（增删改统一记录、敏感字段脱敏）。

### 1.2 核心组件

| 组件 | 技术 | 说明 |
|------|------|------|
| Agent 框架 | `adk-rust` | Agent / Runner / Session / Tool / Memory |
| HTTP 服务 | `axum` | GraphQL 单入口 + REST（SSE / 认证 / 上传 / 健康检查） |
| 知识库 | 多 provider | Dify 外挂 + 内置 Qdrant（adk-rag），按知识库实例路由 |
| 向量库 | `qdrant-client`（gRPC :6334） | 内置 KB provider 的向量后端（独立部署） |
| 会话存储 | PostgreSQL | 多轮对话持久化（adk-rust Session）+ 全部业务表 |
| 长期记忆 | Redis | Agent 记忆存储（adk-rust Memory）+ JWT 黑名单 |
| 设备目录 | PostgreSQL | `system_builtin` schema（厂商/设备类型缓存） |
| 跨会话记忆 | PostgreSQL | `memories`（已确认）+ `memory_proposals`（待确认） |
| LLM | OpenAI 兼容 / Anthropic 双协议 | 查询理解、FAQ 提取、对话生成（按供应商 `protocol` 分发） |
| 监控插件运行时 | `rhai` + `rhai-runner` | 进程内 Rhai 引擎 + 子进程隔离执行；详见 [Rhai 监控插件系统](./docs/rhai-plugin.md) |
| 代码校验 | `adk-sandbox` / `adk-code` | L2 子进程沙箱 / L3 完整 Rust 编译管线（可选） |
| SNMP 采集 | Redis 队列 | 监控插件生成时对真实设备做 OID 实测：`LPUSH device:cmd:exec` / `RPOP snmp:res:data:chan:test{task_id}`（可选） |
| 审计日志 | PostgreSQL `audit_logs` | 所有增删改写操作（GraphQL mutation + REST 登录/注册/注销/shell-approve/upload）统一异步记录，敏感字段递归脱敏；DB 不可用时静默跳过 |

## 二、环境要求

### 2.1 基础环境

- **Rust** 最新稳定版（edition 2024）
- **PostgreSQL** 14+（会话持久化 + 全部业务表 + 设备目录）
- **Redis** 6+（Agent 长期记忆 + JWT 黑名单；可选 SNMP 采集队列）
- **Qdrant** 1.x（内置知识库 provider 的向量后端，gRPC `:6334`；仅在使用内置 KB 时必需）
- **Dify**（可选）：若使用 Dify 外挂知识库 provider 才需部署；内置 provider 不依赖 Dify
- **对象存储（S3 兼容）**：RustFS / MinIO / AWS S3 任一。**默认启用**，承载截图 / 上传图 / 沙箱快照 / artifact 共享存储；多节点部署必须配齐。本地验证可用 `docker run -p 9000:9000 <rustfs/minio 镜像>`，或按 [RustFS 部署指南](./docs/rustfs-deploy.md) 做单机二进制部署（systemd 托管）；仅单机不需要共享文件时可设 `[object_storage].enabled = false` 关闭（截图/上传图/沙箱快照随之不可用）。

### 2.2 依赖安装

```bash
# Ubuntu/Debian
sudo apt-get install postgresql redis-server

# CentOS/RHEL
sudo yum install postgresql-server redis
sudo systemctl enable --now postgresql redis

# macOS (Homebrew)
brew install postgresql redis
brew services start postgresql redis
```

> Qdrant 推荐用 Docker 部署：`docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant`（6334 为 gRPC 端口，内置 KB provider 使用）。

### 2.3 Rust 工具链

```bash
# 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Windows: 下载 rustup-init.exe
# https://rustup.rs/
```

## 三、项目配置

### 3.1 配置文件

修改 `config/config_1.toml`（或通过 `CORTEX_AGENT_CONFIG` / `--config` 指定）。以下为可配置段（均有默认值，不配也能跑）：

```toml
[server]
port = "8090"

[log]
debug = false           # true=控制台输出，false=滚动文件输出
path = "./logs"
level = "INFO"          # DEBUG / INFO / ERROR

[db]
db_type = "postgres"    # postgres / mysql
host = "localhost"
port = 5432
user = "your_user"
password = "your_password"
db = "your_database"
# 可选：connect_timeout(10) / statement_timeout(30) / pool_max_size(10) / pool_timeout(5)

[redis]
host = "localhost"
port = 6379
password = ""
# 监控插件 OID 实测通过 Redis 下发 SNMP 采集任务：
# LPUSH device:cmd:exec，RPOP snmp:res:data:chan:test{task_id}
# 认证启用时还承载 JWT 黑名单（注销失效）

[security]
aes_key = ""            # AES-256-GCM 密钥（模型供应商 API Key / OAuth secret 加密）；可被环境变量 MODEL_AES_KEY 覆盖；留空则启动随机生成（重启后历史密文无法解密，生产务必固定）

[auth]
jwt_secret = ""         # JWT 签名密钥
token_ttl_secs = 86400  # 访问令牌有效期（秒）
# 认证在数据库可用时强制启用、不可关闭（[auth].enabled 开关已移除）
# 身份提供商（可选）：本地账号 + SSO（feishu / wechat / oidc），通过 [[auth.providers]] 数组配置，详见 config/config_1.toml

[skill]
# Codex 风格文件系统 Skill（渐进式披露注入 system prompt）
max_inject_chars = 20000        # 单个 skill 正文注入的最大字符数（超出截断）
catalog_token_budget_pct = 2    # skill catalog 占上下文窗口的百分比（超出按优先级截断）

[workspace]
enable_session_sandbox = true   # 是否为会话自动创建临时沙箱目录（开启后可用文件读写工具）

[shell]
# shell_command 工具配置（自定义助手 + 代码助手共用）
default_timeout_ms = 30000      # 命令执行默认超时（毫秒）
max_timeout_ms = 120000         # 命令执行超时上限（毫秒）
approval_timeout_secs = 120     # 审批等待超时（秒，用户不响应自动拒绝）

[object_storage]
# 对象存储（S3 兼容，接 RustFS/MinIO/AWS S3）——截图/上传图/沙箱快照/artifact 共享存储
# 多节点负载均衡必须配齐；本地验证可用 docker 起一个 RustFS/MinIO
enabled = true                  # 默认 true；关 false 则截图/上传图/沙箱快照不可用（降级，非致命）
endpoint = "http://localhost:9000"
region = "us-east-1"
bucket = "cortex"
access_key = "cortex"
secret_key = "cortex12345"       # 敏感：不入日志
path_style = true                # RustFS/MinIO 用 true；AWS S3 虚拟主机风格用 false
presign_ttl_secs = 604800        # presigned URL 有效期（秒），默认 7 天

[context]
# 上下文治理（对齐 codex：仅按模型 context_window 阈值触发压缩，不再按轮数/单轮 token）
# 压缩阈值固化为常量（cortex_agent）：软闸 ×0.9 / 硬闸 ×0.95 / 上下文剩余提醒 ×0.15
# 其余字段未配时走代码默认：chars_per_token=4 / fallback_context_window=128000 / compact_model_id=None / max_spawn_depth=3 / max_concurrent_children=3
tool_max_output_bytes = 20480        # 工具输出截断阈值（字节，超出语义过滤/硬截断）
# 可选覆盖：chars_per_token / fallback_context_window / compact_model_id / max_spawn_depth / max_concurrent_children

[kb]
# 内置知识库 provider（Qdrant）配置；使用 Dify 外挂 provider 时不依赖此段
# 以下数值为 config_1.toml 示例值；未配置时代码默认：default_chunk_size=1024 / default_top_k=6 / default_similarity_threshold=0.35
qdrant_url = "http://localhost:6334"        # Qdrant gRPC 地址
qdrant_api_key = ""                         # Qdrant 鉴权（未启用鉴权留空）
default_chunk_size = 800                    # 默认切片大小
default_chunk_overlap = 100                 # 默认切片重叠
default_top_k = 5                           # 默认检索返回条数
default_similarity_threshold = 0.5          # 默认相似度阈值

# [mcp] MCP 预配置服务器种子（启动时自动 upsert 到 DB，在助手管理中勾选使用）
# 接入外部 MCP = cargo install <bin> 后加 [[mcp.seeds]]，或直接在界面新建，零代码。
# excel-mcp-server 曾编译期内置，因 write_cells 在某些 cell 组合下卡死（zavora-xlsx 重算 bug）已移除内置。

# 统一数据根目录（所有本地持久化数据子目录派生自此）
data_dir = "./data"
```

> **模型（LLM）配置不在配置文件中**：LLM 供应商、模型、API Key、默认模型统一由数据库「模型供应商」管理（启动后通过 GraphQL `modelProviders` / `createModelProvider` 等维护，API Key 经 AES-256-GCM 加密存储）。详见 [API 文档 · 模型供应商管理](./docs/api.md#模型供应商管理)。
>
> **知识库也不在配置文件中**：知识库为多 provider 多实例，通过 GraphQL `kbInstanceCreate` 创建实例（provider 类型 + config JSON，secret 字段加密落库），助手绑 `kb_instance_id`。见 [API 文档](./docs/api.md)。
>
> `[server]`、`[log]`、`[db]`、`[redis]` 为必填段；`[security]`、`[auth]`、`[skill]`、`[workspace]`、`[shell]`、`[context]`、`[kb]`、`[mcp]`、`[object_storage]` 缺省时使用各自默认值。
>
> **可选：浏览器自动化 MCP**：如需让助手具备「打开网页 / 点击 / 截图 / 抓取内容 / 生成 PDF」等浏览器能力，可按 [Playwright MCP 安装指南](./docs/playwright-mcp-install.md) 在服务器（支持无图形界面的麒麟 / Linux）部署 Playwright MCP，再以 `transport=streamable_http`、`endpoint=http://127.0.0.1:8931/mcp` 注册为外部 MCP（详见指南 §6）。

### 3.2 环境变量

| 环境变量 | 作用 |
|---------|---------|
| `CORTEX_AGENT_CONFIG` | 配置文件路径（优先于 `--config` 参数） |
| `OTEL_EXPORTER_OTLP_HEADERS` | OTLP 遥测上报所需的 Authorization / organization / stream-name |
| `MODEL_AES_KEY` | 覆盖 `[security].aes_key`，用于模型供应商 API Key / OAuth secret 加密 |

> 配置加载只读 TOML 文件（`AppConfig::load`），上述以外的配置项均以配置文件为准，无其他环境变量注入。

### 3.3 数据库准备

#### 业务表（执行 schema.sql）

所有业务表建表 DDL 统一在 `migrations/schema.sql`（18 张表，含 `users` / `user_identities` / `api_tokens` / `assistants` / `mcp_servers` / `llm_providers` / `llm_models` / **`session_settings`**（会话级配置合并大表：标题/agent_type/模型绑定/思考级别/沙箱+审批/助手绑定，取代旧的 `session_models`/`session_assistants`/`session_thinking_levels`/`session_permission_policies` 4 张小表，已 DROP） / `shell_rules` / `kb_instances` / `kb_documents` / `kb_chunks` / `memories` / `memory_proposals` / `monitor_plugins` / `monitor_plugin_versions` / `audit_logs` 等）。cortex-agent 启动时**不再自动建表**，首次部署**必须先执行**：

```bash
psql -d <your_database> -f migrations/schema.sql
```

脚本全部使用 `IF NOT EXISTS` 幂等语句，可重复执行，对已有库无破坏；老库升级由脚本末尾的「幂等升级」段自动补列/清理。

> `kb_doc_meta` 为遗留表（旧 Dify 文档映射），新架构不再读写，行为表为 `kb_instances` / `kb_documents` / `kb_chunks`。

#### 设备目录表（需预先存在）

设备目录数据需存在于 PostgreSQL 的 `system_builtin` schema 中：

```sql
-- 厂商表
CREATE SCHEMA IF NOT EXISTS system_builtin;

CREATE TABLE system_builtin.device_brand (
    id      SERIAL PRIMARY KEY,
    name_ch VARCHAR(64) NOT NULL,   -- 中文名（如 "华为"）
    name_en VARCHAR(64) NOT NULL    -- 英文名（如 "Huawei"）
);

-- 设备类型表
CREATE TABLE system_builtin.device_type (
    id      SERIAL PRIMARY KEY,
    name_ch VARCHAR(64) NOT NULL,   -- 中文名（如 "路由器"）
    name_en VARCHAR(64) NOT NULL    -- 英文名（如 "router"）
);
```

### 3.4 知识库配置

知识库为「多 provider 多实例」架构，**不通过配置文件**，而是在运行时通过 GraphQL 创建实例：

1. 选择 provider 类型：内置 Qdrant（需先部署 Qdrant，见 §2.2）或 Dify 外挂；
2. GraphQL `kbInstanceCreate`（`input` 内含 provider 类型 + config JSON，secret 字段加密落库）：
   ```bash
   curl -X POST http://localhost:8090/api/graphql \
     -H "Content-Type: application/json" \
     -d '{"query":"mutation { kbInstanceCreate(input: {name:\"内置库\", provider_kind:0, config:{}}) }"}'
   ```
   - `provider_kind=0` 内置 Qdrant，`provider_kind=1` Dify 外挂（Dify 的 `base_url` / `api_key` / `dataset_id` 放入实例 `config` JSON）；
3. 上传文档：GraphQL `kbInstanceUpload`（按 `instance_id` 路由到对应 provider）；
4. 助手绑定知识库实例：助手编辑页下拉选择，或 GraphQL `bindAssistantKbInstance`。

> 详见 [API 文档 · 知识库](./docs/api.md)。历史 `[dify]` 配置段已移除。

## 四、启动服务

### 4.1 开发模式

```bash
# 设置 PostgreSQL 库路径（Windows）
set PQ_LIB_DIR=C:\pg_lib

# 启动（项目有 3 个 bin，无 default-run，必须显式指定 --bin）
cargo run --bin cortex-agent -- --config config/config_1.toml
```

> 另两个 bin：`rhai-runner`（监控插件 L2 沙箱子进程）、`reset_llm_tables`（清空模型表工具）。裸 `cargo run`（不带 `--bin`）会报 "could not determine which binary to run"。

> 监控插件 L2 沙箱校验依赖 `rhai-runner` 子进程，单独编译：
> ```bash
> cargo build --bin rhai-runner
> # 产物：target/debug/rhai-runner[.exe]
> ```
> 若 `rhai-runner` 缺失，主程序仍可启动，仅监控插件校验会降级为只跑 L1（进程内语法检查）。

### 4.2 生产模式

```bash
# 一次性编译所有 bin（cortex-agent + rhai-runner + reset_llm_tables）
cargo build --release

# 运行（确保 rhai-runner 与主程序在同一目录，或位于 target/release 下）
./target/release/cortex-agent --config config/config_1.toml
```

**部署目录结构**（生产推荐）：

```text
/opt/cortex-agent/
├── cortex_agent           # 主程序
├── rhai-runner            # 监控插件 L2 沙箱二进制（必须与主程序同目录）
├── config/config_1.toml
└── data/                  # 仅承载本地必需资产：skills/（Skill 物化）、workspaces/sessions/{sid}/（沙箱工作区，多节点需会话亲和保持本地）、artifacts/。截图与上传图已迁对象存储，不再占本地。
```

`rhai-runner` 查找顺序（[实现细节](./src/infra/sandbox.rs)）：
1. `CARGO_BIN_EXE_rhai_runner` 环境变量（测试场景）
2. 当前 exe 同目录（生产部署）
3. 上溯两级（`deps` → `target/<profile>`）
4. `CARGO_MANIFEST_DIR/target/{debug,release}`（开发兜底）

### 4.3 作为系统服务 (Linux)

创建 `/etc/systemd/system/cortex-agent.service`：

```ini
[Unit]
Description=Cortex Agent - Network Device OPS RAG System
After=network.target postgresql.service redis.service

[Service]
User=appuser
WorkingDirectory=/opt/cortex-agent
ExecStart=/opt/cortex-agent/target/release/cortex-agent --config config/config_1.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable cortex-agent
sudo systemctl start cortex-agent
```

> 部署时记得把 `rhai-runner` 与 `cortex_agent` 放在同一目录（参考 4.2 节目录结构），否则监控插件 L2 沙箱校验会降级。

### 4.4 多节点部署（会话亲和）

对象存储（截图/上传图/沙箱快照/artifact）已解耦到 S3，多节点可共享；但**沙箱工作目录必须留本地 SSD**（`bwrap` 依赖 namespace + bind mount，**不能上 NFS / 对象存储**），故多节点需配置**会话亲和**——保证同一会话始终路由到同一节点；节点故障时新节点从对象存储拉取最新 `workspaces/{sid}/snapshot.tar.zst` 快照恢复续跑（见 [architecture.md §2.4](./docs/architecture.md) `workspace_snapshot`）。

- **负载均衡器会话亲和**：按 `session_id`（cookie 或 query）做一致性哈希到固定节点；这是网关 / Nginx / Ingress 配置，非 cortex 代码。
- **健康检查与摘除**：节点故障时 LB 摘除，后续请求由其他节点接管 + 拉快照续跑。
- **滚动更新**：旧节点先 drain（停接新会话、等当前会话 `RUN_FINISHED` 落快照）再下线。
- **本地目录不可替换**：`data/workspaces/sessions/` 必须本地 SSD，切勿换 NFS / 对象存储；`screenshots/`、`uploads/` 已迁对象存储，不再占本地。

Nginx 一致性哈希示例（按 `session_id` cookie）：

```nginx
upstream cortex_backend {
    hash $cookie_session_id consistent;
    server 10.0.0.1:8090;
    server 10.0.0.2:8090;
}
```

## 五、API 接口说明

> 完整的请求/响应字段、错误码与示例见 [API 文档](./docs/api.md)。本节仅列要点。

### 5.1 SSE 流式对话

**POST `/api/run_sse`**

请求体：

```json
{
    "thread_id": "session-uuid",
    "assistant_id": "01950000-0000-7000-8000-000000000003",
    "messages": [
        {"id": "msg-1", "role": "user", "content": "H3C静态路由怎么配置"}
    ],
    "model_id": "default"
}
```

- **`assistant_id` 必填**（决定 Agent 构建路径与启用工具；缺省请求会被拒绝 400）。
- 会话运行的 Agent 类型由绑定的助手决定，**不再支持请求级 `agent_type`**。
- `model_id`：可选，缺省/空/`default`/`auto` 时使用数据库「模型供应商」中的默认模型，否则按 DB 模型 `id` 精确匹配。

响应：SSE 事件流（`text/event-stream`），事件类型包括：

| 事件类型 | 说明 |
|---------|------|
| `RUN_STARTED` | 任务开始 |
| `TEXT_MESSAGE_START/CONTENT/END` | 文本消息（流式） |
| `THINKING_MESSAGE_START/CONTENT/END` | 模型思考过程（流式） |
| `TOOL_CALL_START/ARGS/END` | 工具调用 |
| `TOOL_CALL_RESULT` | 工具返回结果 |
| `TOOL_CONFIRMATION` | 需要用户确认 |
| `SHELL_APPROVAL_REQUEST` | shell 命令需用户审批（用 `/api/shell-approve` 回应） |
| `CONTEXT_USAGE` | Token 用量上报（接近阈值触发上下文压缩） |
| `RUN_FINISHED` | 任务完成 |
| `RUN_ERROR` | 任务出错 |

### 5.2 业务接口（GraphQL 单入口）

> 除下列保留的 REST 路由外，**所有业务接口统一走 `POST /api/graphql`**。完整 Query / Mutation 列表见 [API 文档](./docs/api.md)。

**保留的 REST 路由**：

| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/run_sse` | POST | SSE 流式对话 |
| `/api/shell-approve` | POST | 响应 `SHELL_APPROVAL_REQUEST`，对 shell 命令审批给出决策 |
| `/api/uploads` | POST | 上传图片附件（`multipart/form-data`，字段名 `file`；单文件 ≤ 10MB；MIME 白名单 png/jpeg/webp/gif；上传至对象存储 `uploads/{user_id}/...`，返回 presigned URL 直链；认证启用时强制登录） |
| `/api/skills/install` | POST | 安装 Skill（`cargo install` 远程包名或 Git 仓库地址，安装后热重载生效） |
| `/api/skills/upload` | POST | 上传 Skill 压缩包安装（`multipart/form-data`，自动识别 zip / tar / tar.gz） |
| `/api/sessions/{session_id}/files/{path}` | GET | 会话工作区文件下载（shell 工具输出的 `[[ARTIFACT:...]]` 标记文件，前端文件卡片直链；登录态校验会话归属） |
| `/api/health` | GET | 健康检查 |
| `/api/v1/monitor/health` | GET | 监控健康检查 |
| `/api/auth/*` | GET/POST | 认证（SSO 跳转 / 回调 / 本地登录 / 注销 + API Token 管理等） |
| `/assets/*` | GET | 前端静态资源 |
| `/api/screenshots/{session_id}/{filename}` | GET | 截图（按会话隔离存于对象存储，后端代理读；登录态校验会话归属，无权 403 / 未登录 401） |

**GraphQL 覆盖的能力**（节选，原 REST 路径已移除）：

| 能力 | GraphQL 入口（节选） |
|------|------|
| 会话管理 | `createSession` / `sessions` / `sessionHistory` / `deleteSession` / `renameSession` / `updateSessionModel` / `updateSessionThinkingLevel` / `updateSessionPermissionPolicy` |
| 助手管理 | `assistants` / `createAssistant` / `generateAssistant` / `updateAssistant` / `duplicateAssistant` / `shareAssistant` / `forkAssistant` / `importAssistant` / `exportAssistant` / `bindAssistantKbInstance` |
| 知识库管理 | `kbInstances` / `kbInstanceCreate` / `kbInstanceUpdate` / `kbInstanceUpload` / `kbInstanceDocuments` / `kbInstanceSegments` / `kbLearn` / `kbLearnCommit` |
| 跨会话记忆 | `memories` / `memoryProposals` / `createMemory` / `acceptMemoryProposal` / `rejectMemoryProposal` |
| 设备检索 | `deviceSearch` |
| 模型供应商 | `models` / `modelProviders` / `createModelProvider` / `createModel` / `setDefaultModel` / `setEmbeddingDefaultModel` 等 |
| 监控插件 | `monitorPlugins` / `registerMonitorPlugin` / `rollbackMonitorPlugin` / `monitorOids` / `monitorCalculate` 等 |
| MCP Server | `mcpServers` / `createMcpServer` / `probeMcpServer` / `batchProbeMcpServers` 等 |
| Shell 权限规则 | `shellRules` / `createShellRule` / `deleteShellRule` |
| 目录 / 模型 / 工具查询 | `catalog` / `models` / `tools` |
| 流式控制 | `cancelRun`（取消任务） |
| Skill | `skills`（只读枚举已加载 Skill 目录）/ `reloadSkills`（热重载磁盘目录，新增 Skill 后点一次即可对新会话生效，无需重启） |

> 历史 REST 路由（`/api/sessions`、`/api/kb/*`、`/api/device/search`、`/api/monitor/*`、`/api/catalog`、`/api/agents`、`/api/models`、`/api/cancel`、`/api/brainstorm/*` 等）已全部移除。`web_search` 联网搜索工具、浏览器自动化（zendriver）、头脑风暴、代码助手均已下线/移除。**Skill** 改为文件系统驱动：旧的 DB 持久化管理面（`createSkill` / `updateSkill` 等创建编辑能力）已下线，但保留只读枚举 `skills` 与热重载 `reloadSkills`（见上表）。
>
> **删除操作统一为「预检 → 确认 → 执行」两段式**：删除助手 / 模型 / 供应商 / MCP / 知识库实例等资源时，首次调用返回影响清单（`force` 省略 / false 仅预检，不删数据），前端二次确认后带 `force: true` 才真正事务级联清理 + 删除。详见 [API 文档 · 删除预检与级联清理](./docs/api.md#删除预检与级联清理force)。
>
> **API Token 删除受限**：通过 `Authorization: Bearer` 认证的程序化令牌请求**仅允许删除会话**，删除助手 / 模型 / 供应商 / MCP（含批量） / 知识库一律拒绝（需账号登录）。

创建会话示例（GraphQL）：

```bash
curl -X POST http://localhost:8090/api/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"mutation { createSession(input: {assistant_id:\"<助手 id>\", title:\"H3C路由配置\"}) }"}'
```

所有业务返回统一信封 `{ code, message, data }`（`code == 0` 成功）。

### 5.3 监控插件接口

进程内 Rhai 监控插件运行时，完整契约与脚本格式请参考 [Rhai 监控插件系统](./docs/rhai-plugin.md)。
其中插件注册 / 回滚 / OID 准备 / 采集值解析均通过 GraphQL 暴露（`registerMonitorPlugin` /
`rollbackMonitorPlugin` / `monitorOids` / `monitorCalculate`）。

注册示例（GraphQL `registerMonitorPlugin`，`input` 内含 `plugin_id` + `script`）：

```json
{
  "plugin_id": "sysuptime-h3c",
  "script": "fn prepare_oids() { `[{\"oid\":\".1.3.6.1.2.1.1.3.0\",\"method\":\"get\"}]` } fn parse(j) { /* ... */ }"
}
```

## 六、助手与会话路由

系统采用 **「会话绑定助手」** 模型：创建会话时指定 `assistant_id`，运行时（`/api/run_sse`）
读取该助手配置后分发构建 Agent。内置助手与对应 Agent 能力如下：

| 内置助手 | 对应 Agent | 能力 | 示例 |
|---------|-----------|------|------|
| 设备命令助手 | `device_command` | 知识检索 + 结构化命令生成 | "H3C静态路由配置" |
| ~~监控插件助手~~（已下线） | `monitor_plugin` | 生成 Rhai 监控插件脚本（三层自动校验） | "帮我生成CPU利用率监控插件" |

> **监控插件助手已暂下线**：`seed_builtin` 不再 seed 该助手（`...005`）、前端「运维工具」菜单组已隐藏（`/monitor` 路由保留）。Agent 构建代码（`build_monitor_plugin_agent`）仍在，恢复时在 `domain/assistant/store.rs` 加回种子元组 + 取消 App.vue 菜单注释即可。故当前默认内置助手仅「设备命令助手」一个。

> 内置助手只读、**不支持复制**（`duplicate_builtin` 拒绝内置助手）；自定义助手（`Custom=9`）走通用构建路径 `build_custom_agent`（用户自由组合系统提示词 + shell_command 工具 + 生成参数 + 知识库实例）。

> 历史「请求级 `agent_type`」已移除：会话类型由绑定的助手决定。`auto`(0) / `chat`(1) 及 `command_brainstorm`(3) / `browser`(5) / `code_assistant`(6) 内置助手已废弃删除，旧数据统一按自定义助手 `Custom`(9) 处理；分发行为完全由助手配置决定，不再有跨类型智能路由。

## 七、降级策略

系统在设计上保证了高可用性，各组件不可用时的降级行为：

| 组件 | 正常模式 | 降级模式 |
|------|---------|---------|
| Session 服务 | PostgreSQL 持久化 | InMemory（重启丢失） |
| Artifact 服务 | 文件系统 | InMemory |
| Memory 服务 | Redis | InMemory |
| 设备目录缓存 | 定期刷新 | 空缓存（不阻塞启动） |
| 监控插件加载 | 从 PG 恢复已注册插件 | 内存态（重启丢失，不阻塞启动） |
| 监控插件校验 | L1+L2（rhai-runner 子进程） | 仅 L1（rhai-runner 缺失时） |
| 模型供应商 | DB 持久化 | 不可用（对话不可用） |
| 对象存储 | RustFS/MinIO/S3 | None（截图返回 503、上传图报错、沙箱快照不可用；主程序仍可启动） |

## 八、常见问题

### 8.1 启动失败

检查：
- PostgreSQL 和 Redis 是否正常运行
- 配置文件路径是否正确（`--config` 参数或 `CORTEX_AGENT_CONFIG` 环境变量）
- 数据库连接参数是否正确
- 业务表是否已执行 `migrations/schema.sql`
- 是否用 `cargo run --bin cortex-agent`（裸 `cargo run` 会因多 bin 报错）

### 8.2 检索无结果

检查：
- 知识库实例是否已创建并上传文档（GraphQL `kbInstances` / `kbInstanceUpload`）
- 内置 provider：Embedding 模型是否在「模型供应商」标记为 embedding（`setEmbeddingDefaultModel`）
- Dify provider：Dify 知识库是否已配置 Embedding 模型、检索 `score_threshold` 是否过高；文档名遵循 `brand_dev_type_title` 格式（仅 Dify provider 约束；内置 provider 用 brand/dev_type metadata 字段）

### 8.3 设备目录为空

检查：
- PostgreSQL `system_builtin.device_brand` 和 `system_builtin.device_type` 表是否有数据
- 数据库用户是否有权限访问 `system_builtin` schema

### 8.4 SSE 连接断开

- 检查网络代理是否支持 SSE（禁用 nginx buffering）
- 保持连接的心跳间隔为 5 秒
