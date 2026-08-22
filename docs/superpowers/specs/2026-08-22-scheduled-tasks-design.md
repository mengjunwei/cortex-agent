# 定时任务（Scheduled Tasks）设计文档

> 日期：2026-08-22
> 状态：已确认，待实现
> 需求：用户可基于某个助手创建定时任务（如"每天出一份报表"），支持界面配置与会话内自然语言创建两种方式，结果落成持久会话，调度与权限体系与现有保持一致。

## 1. 背景与目标

cortex-agent 已具备完整的 agent 执行链路（`/api/run_sse`：加载助手 → 装配 → 构建 Agent → `Runner.run`）与持久会话体系（`adk_session_service` + `session_settings`）。定时任务的本质是：**到点以后台身份复用同一套执行链路跑一次 agent，把结果写成会话**。

调研结论：`../codex/codex-rs` 无本地通用定时任务实现（`cloud-tasks` 是云端任务客户端 SDK，非本地调度器）。业界（ChatGPT Tasks / Codex Automations / Claude Code scheduled tasks）共同形态：cron 调度 + 到点起 run + 结果落会话 + 超时/补偿约束。

## 2. 关键决策（已与用户对齐）

| 决策点 | 结论 |
|---|---|
| 调度引擎 | **`tokio-cron-scheduler` v0.15.1 + `postgres_storage` feature**（不自己维护轮询） |
| cron 解析/算下次时间 | 库内置（底层 `croner`），时区用 `chrono-tz`，默认 `Asia/Shanghai` |
| 业务数据存储 | **自建 `scheduled_tasks` 表**（库的 `job` 表只有调度元数据，无业务字段） |
| 结果交付 | 每次触发新建会话，标题 `任务名 · YYYY-MM-DD HH:mm`，前端在任务详情查看 |
| 会话隔离 | `session_settings` 加 `source_type`（0手动/1定时），普通列表过滤掉定时会话 |
| 数据保留 | 每任务保留近 30 天，**运行成功后顺手清理**该任务旧会话（不另起清理作业） |
| 执行身份 | 以**创建者**身份与权限执行（`can_read` 校验助手可见性，失败则停用任务） |
| 超时/重试 | 单次 30 分钟上限强杀，**失败不重试**（下次到点自然再跑） |
| 重启补偿 | 库自动恢复调度元数据；启动时重注册统一执行闭包 + 补跑错过的任务 |
| 创建方式 | ① 前端界面配置 ② 会话内自然语言（新增 agent 工具） |
| 权限 | CRUD 按 `user_id` 归属校验，admin 可管所有；API Token 删除走 `reject_api_token_delete` 同款守卫 |

## 3. 数据模型

### 3.1 新表 `scheduled_tasks`（业务实体，唯一数据源）

| 字段 | 类型 | 说明 |
|---|---|---|
| id | TEXT PK | UUID v7 |
| user_id | TEXT NOT NULL | 创建者（归属/鉴权/记忆隔离） |
| assistant_id | TEXT NOT NULL | 基于哪个助手执行（FK → assistants.id） |
| name | TEXT NOT NULL | 任务名（会话标题前缀） |
| instruction | TEXT NOT NULL | 触发时发给 agent 的指令 |
| schedule_cron | TEXT NOT NULL | 标准 cron 表达式（NL 转换后落库的成品） |
| timezone | TEXT NOT NULL DEFAULT 'Asia/Shanghai' | 时区 |
| enabled | BOOLEAN NOT NULL DEFAULT true | 启停开关 |
| scheduler_job_id | TEXT | tokio-cron-scheduler 返回的 job UUID（增删改同步用） |
| next_run_at | TIMESTAMPTZ | 下次触发时间（详情页展示，由库计算后回填） |
| last_run_at | TIMESTAMPTZ | 最近运行时间 |
| last_run_status | SMALLINT | 0成功/1失败/2超时 |
| last_session_id | TEXT | 最近运行产生的会话，详情页直达 |
| created_at / updated_at | TIMESTAMPTZ | |

索引：`(user_id)`、`(enabled, next_run_at)`。

### 3.2 `session_settings` 加两列（复用大表，不新建会话表）

- `source_type` SMALLINT NOT NULL DEFAULT 0 — 0=手动 1=定时任务（普通会话列表加 `WHERE source_type = 0` 过滤）
- `schedule_task_id` TEXT NULL — 归属任务 id（按任务清理 30 天数据的依据；`GET /{id}/runs` 查询依据）

### 3.3 tokio-cron-scheduler 自建表（库内部，不直接读写）

库通过 `POSTGRES_INIT_METADATA=true` 等环境变量自动建 `job` / `notification` / `notification_state` 三张表，仅存调度元数据（UUID + cron + next_tick + job_type）。**业务层不直接读写这些表**，只通过库的 API 增删 job。

## 4. 调度引擎接入（tokio-cron-scheduler + postgres_storage）

### 4.1 依赖

```toml
tokio-cron-scheduler = { version = "0.15.1", features = ["postgres_storage"] }
chrono-tz = "0.10"
```

`postgres_storage` 间接依赖 `tokio-postgres 0.7`（与现有 `Cargo.toml` 已有版本一致，无冲突）+ `prost`（`has_bytes`）。

### 4.2 初始化（bootstrap 阶段）

- 用 `new_with_storage_and_code(PostgresMetadataStore, PostgresNotificationStore, 自定义JobCode, SimpleNotificationCode, channel_size)` 构造。
- **闭包重注册**：库的 `SimpleJobCode` 只在内存存闭包，重启后元数据恢复但闭包丢失（官方 issue #84）。因所有任务共用同一执行逻辑，实现一个**自定义 `JobCode`**：对所有 job UUID 返回同一个"按 task_id 查库跑 agent"的统一闭包。job 的 `extra` 字段或 job UUID ↔ `scheduled_tasks.scheduler_job_id` 映射用于定位任务。
- 构造后 `start()`，库每 500ms 内部 tick 触发到期 job（无需我们轮询）。

### 4.3 任务生命周期 ↔ 库 job 同步

| 业务操作 | 库操作 |
|---|---|
| 创建任务 | 写 `scheduled_tasks` → `Job::new_cron_job_async_tz(cron, tz, 统一闭包)` → `sched.add()` → 回填 `scheduler_job_id` + `next_run_at` |
| 启用 | `sched.add()`（重新注册） |
| 停用 | `sched.remove(scheduler_job_id)` |
| 改 cron/时区 | `sched.remove()` + `sched.add()`（库不支持原地改 cron） |
| 删除任务 | `sched.remove()` + 删 `scheduled_tasks` 行 |
| 启动恢复 | 库自动恢复调度元数据；启动时遍历 `enabled=true` 任务，确保闭包已注册 + 补跑 `next_run_at < now()` 的遗漏 |

### 4.4 时区

统一用 `Job::new_cron_job_async_tz(schedule, timezone, run)`，timezone 从 `scheduled_tasks.timezone` 解析为 `chrono_tz::Tz`。避免库默认 UTC 的坑。

## 5. 执行链路（核心复用，零重写 agent 逻辑）

### 5.1 提取公共执行函数

把 `server/sse/mod.rs::handle_run_sse` 中「加载助手 → `can_read` 校验 → 模型解析（`resolve_run_model`）→ `build_agent_request` → `build_agent_for_session` → Runner 装配」这段提取为公共函数：

```
server::runner_core::execute_agent_run(ctx, assistant, thread_id, user_id, is_admin,
                                       user_text, model_id, cancel_token, event_sink) 
                                       -> Result<RunOutcome>
```

- `handle_run_sse`（交互）与调度器（后台）都调它，保证两条路径 agent 行为完全一致。
- **事件汇抽 trait**：交互场景 sink 推 SSE channel；定时场景 sink 只累计文本/工具调用/usage 用于落库与状态记录，不产生 SSE。

### 5.2 触发执行流程（统一闭包内）

```
按 task_id 查 scheduled_tasks（不存在/停用→跳过）
  → 加载助手 + can_read 校验（失败→记 last_run_status=失败，停用任务，写审计）
  → create_session(source_type=1, schedule_task_id, title="任务名 · 本地时间")
  → resolve_run_model（助手模型 → DB 默认）
  → execute_agent_run(user_id=创建者, content=instruction)
  → cancel_token 30min 超时强杀；ActiveRunGuard 兜底注销 run_registry
  → 更新 last_run_at/last_run_status/last_session_id
  → 异步清理该任务 30 天前旧会话（adk 会话 + session_settings 行）
```

### 5.3 超时与失败

- 单次 30 分钟 `cancel_token` 强杀，标记 `last_run_status=2`（超时）。
- 失败（助手不可见 / agent 构建失败 / Runner 错误）标记 `last_run_status=1`，**不重试**。
- 进程重启：库恢复调度；启动时补跑 `next_run_at < now()` 的启用任务一次。

## 6. NL 转 cron

- `POST /api/scheduled-tasks/parse-schedule`：输入自然语言（"每天早上9点"），用**默认模型轻量调用**（低 thinking、短输出）转成 cron + 人话描述 + 未来 3 次触发时间。
- **前端预览确认后才创建**；落库的永远是 cron 不是自然语言（调度不依赖 LLM）。
- 会话内自然语言创建时（见 §7），agent 工具内部也走同一转换逻辑。

## 7. 两种创建方式

### 7.1 界面配置（REST + 前端页）

REST（挂 `/api/scheduled-tasks/*`，走现有 Cookie/Bearer 鉴权 + 归属校验）：

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/parse-schedule` | NL→cron 预览 |
| POST | `/` | 创建 |
| GET | `/` | 列表（归属人；admin 见所有） |
| GET | `/{id}` | 详情 |
| PATCH | `/{id}` | 改（启停/指令/cron/名称） |
| DELETE | `/{id}` | 删除（API Token 拒绝，走 reject_api_token_delete 守卫） |
| GET | `/{id}/runs` | 近 30 天运行会话列表 |
| POST | `/{id}/run-now` | 立即手动触发一次（调试） |

前端新增「定时任务」管理页：列表（名称/cron 人话/下次运行/最近状态/开关）+ 创建编辑表单（选助手、填指令、NL 输入预览 cron）+ 详情页（运行历史，每条点进是该次会话）。普通会话列表查询加 `source_type=0` 过滤。按钮配色沿用高对比约定（commit 9043cf3）。

### 7.2 会话内自然语言创建（agent 工具）

新增内置工具 `manage_scheduled_task`（对齐现有工具注册体系），LLM 在对话中被引导调用：

- `action=create`：参数 `{name, assistant_id, instruction, schedule_nl 或 schedule_cron}`。内部先 NL→cron（复用 §6），创建任务并返回确认（含下次运行时间）。
- `action=list / update / delete / toggle`：会话内管理自己的任务。
- 工具层用 `ctx.session_id()` / 当前 `user_id` 做归属（对齐截图/子代理的会话上下文穿透惯例）。
- 系统提示词补充该工具的使用引导（何时主动建议创建定时任务）。

## 8. 安全与权限（与现有体系一致）

- 所有 CRUD 按 `user_id` 归属校验；admin 可管所有。
- 执行时 `can_read(assistant, user_id, is_admin)` 失败 → 停用任务 + 记失败，不静默跑已下线助手。
- API Token（Bearer）可读写任务，但**删除被拒**（复用 `reject_api_token_delete` 同款逻辑，对齐 skill 删除审查四条铁律）。
- 审计：`scheduled_task` 增删改 + 每次触发/完成/失败写 audit 表（source 分流）。
- `source_type` 默认 0，存量会话不受影响。

## 9. 错误处理

| 场景 | 处理 |
|---|---|
| cron 非法 | 创建/更新时校验，返回 400 + 中文错误 |
| NL 无法解析 | parse-schedule 返回无法识别 + 建议，不创建 |
| 助手被删/不可见 | 触发时记失败并停用任务，详情页提示 |
| 模型解析失败 | 记失败，沿用 `resolve_run_model` 错误语义 |
| 执行超时 30min | cancel 强杀，记超时 |
| 库与业务表不一致（job 在库但业务表删了/反之） | 启动时对账：业务表 enabled 但库无 job → 重建；库有 job 但业务表无/停用 → remove |

## 10. 测试

- 单元：cron 解析/下次时间计算（chrono-tz 时区）、NL→cron prompt 输出解析、source_type 过滤 SQL、30 天清理 SQL。
- 集成：创建任务→库注册→到点触发→落会话（source_type=1）→普通列表不可见→详情 runs 可见→30 天清理。
- 权限：越权读写他人任务被拒、API Token 删除被拒、助手不可见时任务停用。
- 重启恢复：注册后重启进程，调度不丢、闭包重注册成功、遗漏补跑。

## 11. 范围外（YAGNI）

- 多实例分布式调度/分布式锁（当前单实例）。
- 飞书/邮件/webhook 通知渠道（结果只看会话）。
- 失败自动重试。
- 每个 job 独立不同执行函数（所有任务共用统一 agent 执行闭包）。
