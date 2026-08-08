# 移除多 Agent 编排功能 — 设计文档

> 日期：2026-07-31
> 状态：待实现
> 范围：从 cortex-agent 彻底移除「自定义助手挂子助手 + 4 种编排模式」功能，并还原到编排功能加入前的代码结构

---

## 1. 背景与目标

### 1.1 现状
项目已完整实现「多 Agent 编排」功能（设计文档 `docs/design/multi-agent-orchestration.md`）：自定义助手可挂载多个子助手，支持 `Delegate` / `Parallel` / `Sequential` / `Router` 四种编排模式，含环检测（`CycleGuard`）与深度限制（`max_nesting_depth`）。该功能已接入会话构建入口（`build_agent_for_session`）、助手数据模型、DTO、前端编辑页。

### 1.2 目标
彻底移除该功能：删除编排逻辑、数据字段、DB 列、前端 UI、相关测试与文档，并把为编排而引入的代码脚手架（`build_custom_builder` 拆分、`build_agent_for_sub` 递归入口、`AgentContext` 编排专用字段）一并还原，使代码回到编排加入前的干净状态，不留死抽象。

### 1.3 非目标
- 不改动助手 CRUD 本身（`assistant.creator` 字段、`AppState.assistant_store`、`server/assistant.rs::current_creator()` 均保留，它们与编排无关）
- 不改其他功能（skill / mcp / knowledge / 内置助手构建 / 会话机制等）
- 不重写历史 `docs/superpowers/**` 的 spec/plan 文档（时间快照，保持原样）

---

## 2. 关键决策

| 决策点 | 选择 | 说明 |
|--------|------|------|
| 删除彻底度 | **彻底删除** | 含 DB `DROP COLUMN`，破坏性，会丢已配置的子助手关系（用户已认可） |
| 代码结构 | **还原脚手架** | 合并/删除为编排而拆出的函数与字段，不留过度抽象 |
| DB 迁移方式 | `DROP COLUMN IF EXISTS` | 放入 `ensure_schema`，幂等，下次启动自动清理 |

---

## 3. 删除清单（按层）

### 3.1 编排核心
- **删整文件** `src/agent/orchestration.rs`（`validate_orchestration` / `CycleGuard` / `build_sub_agents` / `build_router_agent` + 单测）
- `src/agent/mod.rs` 删 `pub mod orchestration;`

### 3.2 构建 `src/agent/custom.rs`（核心还原）
- 删 `build_agent_for_sub`（递归入口不再需要）
- `build_agent_for_session` 还原为直连分发：内置助手 → `build_builtin`，自定义助手 → `build_custom_agent`；去掉 `CycleGuard` 与递归调用
- `build_custom_builder` 合并回 `build_custom_agent`，去掉仅 delegate 使用的 `extra_instruction` 参数
- `AgentContext` 删三个编排专用字段：`assistant_store`、`mcp_manager`、`current_creator`（已 grep 确认仅 `orchestration.rs` 使用）
- 删 `use` 的 `ParallelAgent`、`SequentialAgent`
- `normalize_agent_name` 保留（agent name 规范化本身健壮有用），更新注释去掉 `transfer_to_agent` 语境

### 3.3 枚举 `src/domain/assistant/enums.rs`
- 删 `Orchestration` 枚举（含 `Default` / `as_i16` / `from_i16` / `try_from_i16` / `dispatch_key` / `Serialize` / `Deserialize` 实现）
- 删 `orchestration_tests` 模块

### 3.4 模型 `src/domain/assistant/models.rs`
- `Assistant` 删 `sub_agent_ids`、`orchestration`
- `AssistantRow` 删 `sub_agent_ids`、`orchestration`
- `CustomAssistantInput` 删 `sub_agent_ids`、`orchestration`
- `From<AssistantRow> for Assistant` 删对应映射
- 删相关单测（`row_to_assistant_parses_sub_agent_ids` 等）

### 3.5 存储 `src/domain/assistant/store.rs`
- `ensure_schema`：`ADD COLUMN IF NOT EXISTS sub_agent_ids/orchestration` → 替换为 `DROP COLUMN IF EXISTS`
- `insert` / `update_custom` / `fork` / `duplicate_builtin`：去 `sub_agent_ids` / `orchestration` 列绑定
- 删 `encode_sub_agent_ids`

### 3.6 DTO `src/server/assistant.rs`
- `WriteAssistantRequest` 删 `sub_agent_ids`、`orchestration`（及 `default_orchestration` 辅助函数）
- `AssistantDto` 删 `sub_agent_ids`、`orchestration`
- `to_input` 删 `validate_orchestration` 调用与字段透传
- 删相关单测

### 3.7 配置
- `src/config/mod.rs`：`AssistantConfig` 删 `max_nesting_depth` + `default_max_nesting_depth`
- `config/config.toml`、`config/config.local.toml`、`config/config_1.toml`：删 `max_nesting_depth` 行（逐一确认是否存在）

### 3.8 SSE `src/server/sse.rs`
- 构造 `AgentContext` 处删 `assistant_store` / `mcp_manager` / `current_creator` 三行赋值

### 3.9 前端
- `frontend/src/utils/assistantEnums.js`：删 `ORCHESTRATION` / `ORCHESTRATION_LABEL` / `ORCHESTRATION_HINT`
- `frontend/src/views/AssistantEditPage.vue`：删编排 section、`form` 两字段、`applyAssistant` 同步、候选子助手 / 环检测 / 拖拽排序、保存透传、`ORCHESTRATION_KEYS_ALL` 及 import
- `frontend/src/stores/assistant.js`：**不改**（`create/update` 仅透传 payload，无字段定义）

### 3.10 文档
- 删 `docs/design/multi-agent-orchestration.md`
- 清理 `docs/architecture.md`、`docs/design/custom-assistant*.md` 等活文档里的编排引用
- `docs/superpowers/**` 历史快照不动

---

## 4. 数据库迁移
`ensure_schema` 用 `DROP COLUMN IF EXISTS sub_agent_ids` / `orchestration` 幂等删除两列，下次启动自动生效。无需手写单独迁移脚本。

---

## 5. 验证标准
- `cargo fmt && cargo clippy -- -D warnings && cargo test` 全绿
- `cd frontend && pnpm build` 通过
- 启动验收：助手编辑页无「多 Agent 编排」入口、助手可正常对话、DB `assistants` 表两列已 DROP

---

## 6. 风险与兜底
- DB `DROP COLUMN` 不可逆，已配置子助手的记录会丢失（用户已认可）
- 前端若有除 `AssistantEditPage.vue` 外的页面引用 `ORCHESTRATION*` 常量，实现时全局 grep 兜底清理
- 删除后 `git grep orchestration` 应只剩历史 `docs/superpowers/**` 快照与无关词（如 `parallel_tool_calls`、设备 `dev_type=router`）
