# Codex 多智能体机制：全链路深度分析

> **调研对象**：OpenAI Codex 仓库 `/mnt/d/code/rust/codex`（Rust 实现，核心代码在 `codex-rs/`）
> **调研日期**：2026-08-14
> **调研方法**：按链路拆 5 个专项并行深挖（配置发现 / 工具暴露 / 运行时 fork / 协议版本 / 扩展与编排），交叉验证后缝合
> **目的**：作为 `cortex-agent` 借鉴多智能体设计的参考；厘清 Codex 的真实机制，避免被命名误导
> **落地状态**：cortex 已于 2026-08-15 在 `src/agent/runtime/cortex_agent/multi_agent.rs` 落地 V2 六工具集（spawn_agent / send_message / followup_task / wait_agent / interrupt_agent / list_agents）、内置角色 default/explorer/worker（`role.rs`，`[agents.roles]` 可自定义）、昵称池 `agent_names.txt`、canonical 路径寻址；另有 `max_concurrent_children`（默认 3）并发限流与 `multi_agent_mode`（`explicit`/`proactive`/`auto`，默认 `explicit`——`auto` 按思考级别推导）prompt 引导层。与本文 codex 机制一一对应；一处**有意差异**：codex V2 忽略 depth，cortex 保留 `validate_spawn_depth` + `max_spawn_depth`（默认 3）护栏。
> （注：落地过程经仓库全量初始提交 `e71d345` 压缩，早期审查轮次的 commit 号（如 `881c8c9`）已不可追溯，以代码为准。）

---

## 目录

- [〇、先纠正三个最容易把全局带歪的认知](#〇先纠正三个最容易把全局带歪的认知)
- [一、心智模型：四条派生路径 × 两个版本](#一心智模型四条派生路径--两个版本)
- [二、端到端主链路（路径 A / V2 主线）](#二端到端主链路路径-a--v2-主线)
- [三、配置层：AgentRole 从声明到应用](#三配置层agentrole-从声明到应用)
- [四、工具层：spawn_agent 的 V1/V2 双生](#四工具层spawn_agent-的-v1v2-双生)
- [五、运行时层：fork 机制与继承](#五运行时层fork-机制与继承)
- [六、协议 / 追踪层](#六协议--追踪层)
- [七、扩展层：AgentSpawner 与 host/extension 边界](#七扩展层agentspawner-与-hostextension-边界)
- [八、编排注入层：静态烘焙 + 动态 V2](#八编排注入层静态烘焙--动态-v2)
- [九、易遗漏点 / 反直觉设计](#九易遗漏点--反直觉设计)
- [十、设计哲学](#十设计哲学)
- [附录 A：关键文件索引（按子系统）](#附录-a关键文件索引按子系统)
- [附录 B：核心数据结构速查](#附录-b核心数据结构速查)

---

## 〇、先纠正三个最容易把全局带歪的认知

这三个点不先掰清楚，整条链路会理解错：

### 1. `SubAgentSource` 有两个"死变体"

`Compact` 和 `MemoryConsolidation` 在当前代码里**没有任何构造点**——真正的内存整合走 `SessionSource::Internal(InternalSessionSource::MemoryConsolidation)`，不是 `SubAgent` 变体。这两个留在枚举里纯粹为 rollout 迁移兼容。实际活跃的只有：

- **`Review`**（代码 diff 审查）
- **`ThreadSpawn`**（V1 + V2 共用，多智能体派生）
- **`Other(String)`**（guardian 审批 reviewer）

### 2. "review"在 Codex 里有三种同名异物的东西

| | 触发 | `SubAgentSource` | prompt 来源 | 拉起方式 |
|---|---|---|---|---|
| **guardian 审批审查** | 遇需审批动作 + `approvals_reviewer=auto_review` | `Other("guardian")` | `core/src/guardian/policy.md` | `run_codex_thread_interactive` **绕过** `spawn_subagent` |
| **代码 diff 审查** | `Op::Review`（review 命令） | `Review` | `prompts/templates/review/rubric.md` | `run_codex_thread_one_shot` |
| **多智能体派生** | LLM 调 `spawn_agent` 工具 | `ThreadSpawn` | role config + usage hint | `spawn_agent_internal → spawn_thread` |

### 3. `core/templates/**/*.md` 运行时不被 Rust 加载

`orchestrator` / `personalities` / `model_instructions` 这些 `.md` 是**托管模型注册表（registry）的写作源头**，发布后才生效；本地二进制只信 `models.json` + `prompt.md`。**改本地 `.md` 不改运行时行为**。

所以"orchestrator.md 何时注入"要拆成两条：
- **静态编排文本**（"prefer multiple sub-agents / wait before yielding / one agent per step"）→ 烘焙进 registry 的 `instructions_template`，运行时经 `get_model_instructions()` 取出
- **动态多智能体 hint**（spawn_agent 用法、并发槽、proactive 模式）→ 纯运行时、config 驱动、由 world-state 段注入，**仅 `MultiAgentVersion::V2`**

---

## 一、心智模型：四条派生路径 × 两个版本

Codex 没有像 Claude Code 那样"一个 `Agent` 工具 + 一套机制"。它是**四条独立的派生路径**，大部分最终汇入 `ThreadManagerState::spawn_thread`，但 guardian 例外：

```
┌─ 路径 A：LLM 工具派生（主力，V1/V2 两套）────────────────────────┐
│  LLM 调 spawn_agent → handler → AgentControl::spawn_agent_internal │
│  → spawn_thread                                                    │
├─ 路径 B：宿主编程派生（AgentRunner）──────────────────────────────┤
│  app-server turn_processor(detached review 等) → AgentRunner.start │
│  → thread_manager.spawn_subagent → start_thread_inner → spawn_thread│
├─ 路径 C：guardian 审批 review（绕过 spawn_thread）─────────────────┤
│  主 turn 遇审批 → run_codex_thread_interactive 同进程直连           │
│  SubAgentSource::Other("guardian")                                 │
├─ 路径 D：代码 diff 审查（one-shot）────────────────────────────────┤
│  Op::Review → run_codex_thread_one_shot                            │
│  SubAgentSource::Review                                            │
└────────────────────────────────────────────────────────────────────┘
```

两个版本 `MultiAgentVersion::{Disabled, V1, V2}`（`codex-rs/protocol/src/protocol.rs:2821`）只影响**路径 A**——工具 schema、注册、通信模型、并发模型都不同；B/C/D 与版本无关。

---

## 二、端到端主链路（路径 A / V2 主线）

```
① 配置加载（启动时，一次性）
   config.toml [agents.xxx] + agents/*.toml
     → load_agent_roles (agent_roles.rs:18)
       → 声明式 read_declared_role + 发现式 discover_agent_roles_in_dir
       → ConfigLayerStack 低→高合并（merge_missing_role_fields，field-level Option::or）
       → validate_required_agent_role_description
     → Config.agent_roles: BTreeMap<String, AgentRoleConfig>

② 工具注册（每轮 turn 重建）
   build_core_tool_registry (spec_plan.rs:251)
     → add_collaboration_tools (spec_plan.rs:1124)
        ├─ collab_tools_enabled? (spec_plan.rs:599)   ← Disabled→不注册；V1→查 depth；V2→查 root/模型能力
        ├─ V2 分支：注册 spawn_agent(裸名) + send_message/followup_task/wait_agent/interrupt_agent/list_agents
        └─ V1 分支：注册 multi_agent_v1.spawn_agent + send_input/resume_agent/wait_agent/close_agent
     注：是静态注册，不是 dynamic_tools 机制

③ LLM 决策并调用 spawn_agent
   工具 description 自带详尽策略（V1）：
     "默认不 spawn" / "depth/research 请求不算许可" / "wait_agent sparingly" / "按不相交写集切片并行"
   V2 的编排 hint 由 world_state 注入（见第八节）

④ handler 解析 + 应用 role（multi_agents_v2/spawn.rs）
   parse SpawnAgentArgs{task_name(必填), message(必填,加密), fork_turns, agent_type, model...}
     → build_agent_spawn_config (common.rs:178)     ← 从 turn effective config 刷新 model/provider/sandbox/cwd
     → apply_spawn_agent_role (common.rs:385)       ← 查 AgentRoleConfig，apply_role_to_config (agent/role.rs:39)
        → role 的 config_file 作为 SessionFlags 层插入 ConfigLayerStack（优先级 30）
        → "粘性保留"：role 没写的字段继承父 agent 当前值（不回退默认）
     → thread_spawn_source (multi_agents_common.rs:112) ← 算 agent_path = 父path.join(task_name)、depth=父+1

⑤ AgentControl::spawn_agent_internal (control/spawn.rs:382) —— 版本无关的统一内核
     → effective_multi_agent_version_for_spawn (thread_manager.rs:1384)  ← 版本从父继承
     → V2: ensure_execution_capacity + 预留 residency slot（LRU，容量 = max_concurrent-1）
       V1: reserve_spawn_slot（AgentRegistry::total_count，CAS）
     → 检查 depth：V1 exceeds_thread_spawn_depth_limit（默认 max_depth=1）；V2 忽略
     → 继承 environments + exec_policy（SpawnAgentThreadInheritance，control/spawn.rs:421）
     → state.spawn_thread(ThreadSpawnRequest)

⑥ fork + 起子线程（thread_manager.rs:1682 spawn_thread）
     → Session::spawn 起独立异步 task（共享 auth/mcp/environment 的 Arc，独立 history/turn loop/config）
     → 子线程拿新 thread_id（不复用父 conversation_id）
     → V2 fork：fork_turns 决定 FullHistory / LastNTurns(N) / none
     → 擦除父 usage hint + developer instructions，注入子 agent 专用（spawn.rs:680-830）

⑦ 子 agent 跑 turn，结果回传
     V1: maybe_start_completion_watcher → 子完成时注入一条 user message 到父线程
     V2: 发送结构化 InterAgentCommunication(Result) 给父 agent_path
        （父用 wait_agent 只拿到"哪个子 agent 有更新"的摘要，不含内容；内容异步投递）

⑧ 协议/追踪/analytics 记录
     → 子线程 SessionMeta.source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn{...})
     → agent-graph-store: upsert_thread_spawn_edge(parent, child, Open|Closed)
     → analytics: emit_subagent_session_started → SubAgentThreadStartedInput
     → rollout-trace reducer: AgentOrigin::Spawned + spawn edge (edge:spawn:{parent}:{child})
```

---

## 三、配置层：AgentRole 从声明到应用

**核心洞察：role 定义故意做得很"薄"——只有 3 个元数据字段，真正的配置内容在 `config_file` 指向的完整 `ConfigToml` 里。**

### 3.1 数据结构（逐层）

```
AgentRoleToml (config_toml.rs:699)     ← config.toml 里的 [agents.xxx] 声明
  ├ description?           必填（除非 config_file 提供）
  ├ config_file?           指向角色配置层（相对声明它的 config.toml）
  └ nickname_candidates?

RawAgentRoleFileToml (agent_roles.rs:214)  ← agents/*.toml 文件解析中间态
  ├ name?                  可覆盖声明时的 key 名
  ├ description?
  ├ nickname_candidates?
  └ #[serde(flatten)] config: ConfigToml   ★ 文件本质是"带3个元数据的完整 config.toml"

AgentRoleConfig (mod.rs:2328)           ← 最终运行时产物（只存元数据）
  ├ description? / config_file? / nickname_candidates?
```

### 3.2 加载链路（`agent_roles.rs:18 load_agent_roles`）

- 有 ConfigLayerStack → 逐层（低→高）每层做：
  1. **声明式**：读 `[agents.xxx]`（`read_declared_role`，若有 `config_file` 则 `read_resolved_agent_role_file` 解析）
  2. **发现式**：扫描 `agents/*.toml`（`discover_agent_roles_in_dir`，跳过已 declared 的文件防重复）
  3. **层间合并**：`merge_missing_role_fields`（field-level `Option::or`，高优先级层字段为 None 时才从低层补）
- 无 layer → 降级路径 `load_agent_roles_without_layers`（不扫描目录，重名是硬错误而非 warning）
- 最终必过 `validate_required_agent_role_description`（description 仍为 None 则 warning 丢弃）

### 3.3 ConfigLayerStack 优先级（低→高）

```
PackagedDefaults(-10) → Mdm(0) → System(10) → EnterpriseManaged(15)
→ User(20/21) → Project(25) → SessionFlags(30)   ← role 的 config_file 层插这里
→ LegacyManagedConfigToml(40) → LegacyManagedConfigTomlFromMdm(50)
```

### 3.4 运行时应用（`agent/role.rs:39 apply_role_to_config`）

- `resolve_role_config`（`:157`）：**先查用户 role，再查内置 role**（`builtins/explorer.toml`、`awaiter.toml` 等，`include_str!` 编译期嵌入）→ 所以 Codex **有内置 role**，但它们是 spawn 工具 `agent_type` 参数可引用的预设配置，不是用户从列表选的具名人格
- `apply_role_to_config_inner`（`:92`）：把 role 的 `config_file` 内容作为 `SessionFlags` 层（优先级 30）插入 ConfigLayerStack，`reload::build_next_config` 重建
- **"粘性保留"**（`:177-181`）：role 没写 model/provider/effort/instructions → 继承父 agent 当前值，**不回退系统默认**。关键安全默认
- V2 特殊：`PreserveCallerInstructions`（`:188`）——role 没 developer_instructions 时保留父的，而非回退配置层

**能改什么**：`config_file` 里能写任意 `ConfigToml` 字段——model、sandbox、tools、developer_instructions（≈prompt）、skills 禁用、personality……不限于 role 字段。

---

## 四、工具层：spawn_agent 的 V1/V2 双生

### 4.1 开关与版本决策

三个开关，优先级从高到低（`config/mod.rs:1523 multi_agent_version_override`）：

```
features.multi_agent_v2 = true   →  V2（最高，压制一切）
agents.enabled = false            →  Disabled
features.collab (默认 true)       →  V1   ← 所以默认就是 V1 开着
```

> ⚠️ `agents.enabled=false` **不是总开关**——被 `multi_agent_v2=true` 压制。想彻底关多智能体必须同时 `agents.enabled=false` 且不开 `multi_agent_v2`。

fork 时版本**从父继承**（`thread_manager.rs:908`），不从子 config 重算。

### 4.2 V1 vs V2 工具对比（路径 A 内部）

| 维度 | V1 | V2 |
|---|---|---|
| 工具名 | `multi_agent_v1.spawn_agent`（命名空间） | `spawn_agent`（裸名，namespace 可配） |
| 配套工具 | send_input / resume_agent / wait_agent / close_agent | send_message / followup_task / wait_agent / interrupt_agent / list_agents |
| 必填参数 | message 或 items | **task_name** + message（加密） |
| fork | `fork_context: bool`（全 fork 或不 fork） | `fork_turns`: `"none"/"all"/"N"`（精确 N 轮） |
| 派生入口 | `spawn_agent_with_metadata`（纯 UserInput） | `spawn_agent_with_communication`（InterAgentCommunication） |
| 并发模型 | `agent_max_threads`，agent 常驻 | **LRU residency**（idle agent 可卸载重载，`residency.rs`），容量 `max_concurrent-1` |
| depth | 强制（默认 max_depth=1，只允许 1 层嵌套） | **忽略**（靠 residency 容量限并发） |
| 完成通知 | 注入 user message 到父线程 | 结构化 `InterAgentCommunication(Result)`；wait_agent **只返回"有更新"摘要，不含内容** |
| 编排 hint | 工具 description 自带详尽策略 | world_state 动态注入 root/subagent hint + MultiAgentMode |
| metadata | 不隐藏 | `hide_spawn_agent_metadata`（默认 true） |

---

## 五、运行时层：fork 机制与继承

### 5.1 fork 的本质（`spawn_subagent` 路径，`thread_manager.rs:888`）

```
ensure_rollout_materialized  ← 内存排队项落盘（失败仅 warn）
flush_rollout                ← durability barrier（失败 ? 向上传播）
read_thread                  ← 读持久化快照（不是内存对象！）
stored_thread_to_initial_history → InitialHistory::Resumed
fork_history_from_snapshot(ForkSnapshot::Interrupted, ...)
  → 若快照末尾 mid-turn：追加中断标记 + TurnAborted(Interrupted) 事件
  → 若已在 turn 边界：原样
→ InitialHistory::Forked   ← Resumed 被 unwrap 成 Forked（身份剥离：新 thread_id、新 rollout）
start_thread_inner → spawn_thread
```

`ForkSnapshot::Interrupted` 语义：**"假装父线程此刻被中断"**，把当前持久化历史作为子线程起点。

### 5.2 继承关系（关键差异，易踩坑）

| | 路径 A（工具） | 路径 B（AgentRunner/SDK） |
|---|---|---|
| 继承 environments/exec_policy | ✅ `SpawnAgentThreadInheritance` | ❌ 硬设 None |
| 继承父历史 | ✅（V1 全量 / V2 fork_turns） | ✅（`ForkSnapshot::Interrupted` 全量 fork） |
| 继承 auth/mcp/environment runtime | ✅ Arc 共享 | ✅ Arc 共享 |
| 检查 depth/concurrency | ✅ | ❌ **完全不查**（SDK 调用方自负） |
| 独立 history/turn loop/config | ✅ | ✅ |

> ⚠️ **最大坑**：`spawn_subagent`（路径 B）自身不做任何限制 enforce。所有限制只挂在路径 A 的 tool handler / 工具可见性 gate 上。SDK 直调可绕过 `agent_max_depth`——这是设计意图，但极易误以为运行时兜底。

### 5.3 限制 enforce（只挂在路径 A）

- **depth（仅 V1）**：`next_thread_spawn_depth = 父 depth + 1`（`registry.rs:72`），`exceeds_thread_spawn_depth_limit` 在 `tools/handlers/multi_agents/spawn.rs:67` 检查；超限连工具都不注册（`spec_plan.rs:599 collab_tools_enabled`）
- **并发 V1**：`AgentRegistry::total_count`（AtomicUsize）+ `reserve_spawn_slot`（CAS），`SpawnReservation::Drop` 时未 commit 则 -1
- **并发 V2**：`AgentExecutionLimiter`（`agent/control/execution.rs`），`ensure_execution_capacity`，容量 = `max_concurrent_threads_per_session - 1`（root 占一个槽）

---

## 六、协议 / 追踪层

### 6.1 三层身份编码

```
SessionSource（身份，protocol.rs:2569）
  Cli / VSCode / Exec / Mcp / Custom / Unknown(other)
  Internal(InternalSessionSource)   ← 内存整合等走这里
  SubAgent(SubAgentSource)          ← 子 agent
    ├ Review / ThreadSpawn{parent,depth,agent_path,nickname,role} / Other(String)
    └ Compact / MemoryConsolidation  ← 死变体（迁移兼容）

ThreadSource（analytics 粗分类，protocol.rs:2586）
  User / Subagent / Feature / MemoryConsolidation
```

`is_non_root_agent()` = `Internal(_) || SubAgent(_)`，用于 resume 时判断是否需要重建 agent 图。

### 6.2 持久化两层

1. **Rollout 文件**：子线程 `SessionMeta.source = SubAgent(ThreadSpawn{...})`
2. **agent-graph-store**（`codex-rs/agent-graph-store/`）：`upsert_thread_spawn_edge(parent, child, Open|Closed)`，resume 时重建拓扑（`restore_v2_agent_metadata`）

### 6.3 trace 回放

`rollout-trace` reducer 把 `SubAgentSource::ThreadSpawn` 投影成 `ThreadSpawnMetadata`（**丢弃 depth/nickname**，只留 parent/agent_path/task_name/agent_role），构建 `AgentOrigin::Spawned` + spawn edge（确定性 ID `edge:spawn:{parent}:{child}`）。V2 还有 `PendingAgentInteractionEdge`（通信投递边）、`ObservedAgentResultEdge`（完成通知）。

### 6.4 analytics

`emit_subagent_session_started` → `SubAgentThreadStartedInput`（含完整 `subagent_source`）→ `ThreadInitializedEvent`（`event_type = "codex_thread_initialized"`）。另有 `codex.multi_agent.spawn` counter（标签 `[role, version]`）、`subagent_tool_call_count`、`approval_routed_from_subagent`。

---

## 七、扩展层：AgentSpawner 与 host/extension 边界

### 7.1 为什么 spawn 能力必须 trait 化

`extension-api` 是 host 与 ext 的**唯一共享依赖**，且不含 `codex_core` 类型。扩展 crate（`ext/guardian`）不能 `use codex_core::ThreadManager`（会循环依赖 + 泄露核心类型）。所以派生能力由 host 在构造扩展时**注入**（constructor injection）：

```rust
// ext/extension-api/src/capabilities/agent.rs
trait AgentSpawner<R>: Send + Sync {       // R 泛型，无命名请求结构体，刻意最薄
    type Spawned; type Error;
    fn spawn_subagent(&self, forked_from_thread_id: ThreadId, request: R)
        -> AgentSpawnFuture<'_, Self::Spawned, Self::Error>;  // Pin<Box<dyn Future+Send>>
}
// blanket impl：任意 Fn(ThreadId,R)->Future 自动满足（便于闭包装配/测试）
```

### 7.2 两条 spawn 路径并存（都叫 spawn，机制不同）

- **`ext/agent` 的 `AgentRunner`**：虽在 `ext/` 下，但是 **host 侧 helper**，**不用 AgentSpawner trait**，直接 `Weak::upgrade()` 后调 `thread_manager.spawn_subagent`。供 app-server `turn_processor`（detached review 等）用
- **`ext/guardian` 的 `GuardianExtension`**：**用 AgentSpawner trait**（host 经 `guardian_agent_spawner` 闭包注入）。但它本身只是个**转发壳 + thread-context 注入器**——78 行代码只做：① `on_thread_start` 记 `forked_from_thread_id` 进 `GuardianThreadContext` ② 转发 spawn 请求。**生命周期里从不主动 spawn review**

装配：`app-server/src/extensions.rs:101` `codex_guardian::install(&mut builder, guardian_agent_spawner(thread_manager))`。用 `Weak` 而非 `Arc` 避免循环引用（TM→registry→guardian→spawner）。

### 7.3 ⚠️ guardian review 引擎完全在别处

真正的审批 review 引擎在 `core/src/guardian/`（~6000 行），**绕过 `thread_manager.spawn_subagent`**，用 `run_codex_thread_interactive` 同进程直连，`SubAgentSource::Other("guardian")`。与 `ext/guardian` 的转发器**无调用关系**。

- **触发**：主 turn 遇需审批动作 + `approvals_reviewer=auto_review`（旧名 `guardian_subagent`）
- **主 turn 同步阻塞**等结果（`tokio::select!`，`GUARDIAN_REVIEW_TIMEOUT=90s` 超时 fail-closed，连续 deny `MAX_CONSECUTIVE_GUARDIAN_DENIALS_PER_TURN=3`/cyber 模型 1 次熔断 `InterruptTurn`）
- **prompt**：`core/src/guardian/policy.md` + `policy_template.md`（注入 config 优先级：父 `guardian_policy_config` → model catalog `auto_review.policy` → 内置 `BUNDLED_GUARDIAN_POLICY`）；review session 禁用 skills/memories/collab/multi-agent/hooks/MCP，设 `approval_policy=Never`、`read_only`
- **session 复用**：`GuardianReviewSessionManager` 缓存 trunk session，按 `GuardianReviewSessionReuseKey`（~20 字段）判断复用/ephemeral fork；prompt 模式首次 `Full`、后续 `Delta`

### 7.4 host/extension 边界（两个方向相反的轴）

- **capabilities**（host→ext 注入）：`AgentSpawner` / `ExtensionEventSink` / `ExtensionMetrics` / `ResponseItemInjector`
- **contributors**（ext→host 注册，13 个 trait）：`ThreadLifecycleContributor`(guardian 用) / `TurnLifecycleContributor` / `ToolContributor` / `ContextContributor` / `ApprovalReviewContributor` / `ConfigContributor` / `TokenUsageContributor` / ...
- **没有顶层 `Extension` trait**：扩展 = `pub fn install(builder, deps...)` 自由函数；`ExtensionData` 是类型擦除的三级 store（session/thread/turn）

---

## 八、编排注入层：静态烘焙 + 动态 V2

### 8.1 链路 A：base instructions（静态，含"orchestrator 语气"）

`orchestrator.md` 的 "prefer multiple sub-agents / wait before yielding" 是**烘焙进 registry 的 `instructions_template`**，运行时经 `get_model_instructions()`（`openai_models.rs:491`）取出。人格（friendly/pragmatic）填 `{{ personality }}` 占位符内联。

base instructions 三级优先级（`session/mod.rs:640`）：
1. `config.base_instructions` 覆盖
2. 历史 `session_meta.base_instructions`
3. `model_info.get_model_instructions(personality)`（静态文本是**最低优先级默认**）

### 8.2 链路 B：动态多智能体编排（运行时，硬门控 V2）

组装在 `build_world_state_for_step`（`world_state.rs:33`）末尾（`:286-298`）。**门控**（`multi_agents.rs:9-19`）：

```rust
if turn_context.multi_agent_version != MultiAgentVersion::V2 { return None; }
```

只有 V2 才注入 spawn_agent hint。按 `session_source` 区分文案：

- `SubAgent(ThreadSpawn)` → `subagent_usage_hint_text`
- `Cli/VSCode/Exec`（root）→ `root_agent_usage_hint_text`
- `Internal(_)` / 其它 `SubAgent(_)`（review 子 agent）→ **None**（不吃这层 hint）

模式段（`effective_multi_agent_mode`）：有 hint → Custom；`Ultra` effort → Proactive（主动派生）；否则 ExplicitRequestOnly。

world-state 是**结构化 diff**（13 个 section 按序拼接，按上轮快照决定是否重发，非每轮字符串拼接）。

### 8.3 world-state 组装顺序（assembly recipe）

`build_world_state_for_step` 按序 `add_section`：① ModelInstructionsState → ② PersonalityState → ③④ TokenBudget/ContextWindow → ⑤ RealtimeState → ⑥ AgentsMdState → ⑦ Permissions → ⑧ CollaborationModeState → ⑨⑩⑪ Environments/Apps/Plugins → ⑫ ToolsState → ⑬ **extension 贡献段**（遍历 context_contributors）→ ⑭ **MultiAgentUsageHint + Mode（仅 V2）**

---

## 九、易遗漏点 / 反直觉设计

1. **`spawn_subagent`（SDK 路径）不检查 depth/concurrency**——限制只挂 tool 路径。SDK 直调可绕过。
2. **三种 review 同名异物**——guardian 审批（policy.md）/ 代码 diff（rubric.md）/ ThreadSpawn 派生。别拿 rubric.md 当 guardian 的 prompt。
3. **`.md` 模板运行时不加载**——改 `core/templates/*.md` 不改本地行为，须走 registry 发布。
4. **`agents.enabled=false` 不是总开关**——被 `multi_agent_v2=true` 压制。
5. **默认就是 V1 开着**（`features.collab` 默认 true）。
6. **V2 `wait_agent` 不返回内容**——只返回"有更新"摘要，内容经 `InterAgentCommunication` 异步投递（V1→V2 重大语义变化）。
7. **V2 并发上限要 -1**（root 自己占一个槽），配 1 = 不能 spawn。
8. **V1 默认 max_depth=1**（只允许 1 层嵌套，子不能再 spawn）。
9. **fork 读的是持久化快照不是内存对象**——所以必须先 materialize+flush。
10. **fork 身份剥离**——子拿新 thread_id、新 rollout，不复用父 conversation_id（中间态 Resumed 最终被改成 Forked）。
11. **role 的 config_file 能改任意 ConfigToml 字段**——不限于 role 字段，可改 sandbox/tools/skills/personality。
12. **多智能体版本从父继承**（`thread_manager.rs:908`），不从子 config 重算——子想用 V2 但父是 V1，中断标记仍走 V1 的 ContextualUser。
13. **`ext/agent` 的 AgentRunner 不用 AgentSpawner trait**——直接调 ThreadManager::spawn_subagent。两条 spawn 路径并行存在。
14. **discovered role 的 config_file 指向自己**（`agent_roles.rs:508`）——apply 时会重新读自己。
15. **role 文件的 `name` 字段可覆盖声明时的 key 名**（`[agents.researcher]` 指向的文件若写 `name = "archivist"`，最终 role 名是 "archivist"）。
16. **guardian review 主 turn 同步阻塞**——不是 fire-and-forget；超时 fail-closed；连续 deny 熔断。

---

## 十、设计哲学

对比 Claude Code 的"**预定义具名目录 + 一个 Agent 工具**"，Codex 走的是完全不同的路线：

1. **基建而非目录**。Codex 不内置"开箱即用的子智能体"，而是提供一套**派生隔离线程的基建**（fork + 分层 AgentPath + role 配置层叠加 + V1/V2 两套通信模型）+ 一段编排系统提示驱动主 agent 主动并行。想要 Claude Code 的体验，得自己用 `[agents.xxx]` / `agents/*.toml` 声明。

2. **配置层叠加（layer composition）而非 flat 替换**。role 不是"替换"父配置，而是作为一层插入 ConfigLayerStack。没写的字段继承父值（粘性保留）。这解释了为什么 role 定义能这么薄——复用整个已有的 ConfigToml 配置体系。

3. **声明与定义分离**。`[agents.xxx]`（声明：名字/描述/配置在哪）与 `agents/*.toml`（定义：具体配置内容）解耦，description/nickname 可跨层互补。

4. **能力注入解耦**。`AgentSpawner` trait 让扩展能派生子 agent 而不依赖核心类型；闭包 blanket impl 让装配极简。这是 Rust 里"依赖反转 + 类型擦除"的教科书式用法。

5. **V1→V2 的演进**：从"agent 常驻 + 布尔 fork + user message 通信"演进到"LRU residency + 精确 fork_turns + 结构化 InterAgentCommunication + 多模式编排"，V2 明显是面向大规模、长时间存活的多智能体协作（proactive 模式）设计的。

---

## 附录 A：关键文件索引（按子系统）

> 所有路径相对 `/mnt/d/code/rust/codex/`。

### 配置层（AgentRole 定义与加载）
| 概念 | 文件:行 |
|---|---|
| `AgentsToml` / `AgentRoleToml` | `codex-rs/config/src/config_toml.rs:662 / :699` |
| `AgentRoleConfig`（运行时产物） | `codex-rs/core/src/config/mod.rs:2328` |
| 加载入口 `load_agent_roles` | `codex-rs/core/src/config/agent_roles.rs:18` |
| 发现 `discover_agent_roles_in_dir` / `collect_agent_role_files` | `agent_roles.rs:470 / :517` |
| 解析 `read_declared_role` / `read_resolved_agent_role_file` | `agent_roles.rs:142 / :314` |
| `RawAgentRoleFileToml` / `ResolvedAgentRoleFile` | `agent_roles.rs:214 / :225` |
| 层间合并 `merge_missing_role_fields` | `agent_roles.rs:161` |
| 运行时应用 `apply_role_to_config` | `codex-rs/core/src/agent/role.rs:39` |
| `resolve_role_config`（先用户后内置） | `role.rs:157` |
| 内置 role（explorer/awaiter，include_str!） | `codex-rs/core/src/agent/builtins/*.toml` |
| ConfigLayerStack 优先级 | `codex-rs/config/src/config_layer_source.rs:33` |

### 工具层（spawn_agent 暴露与开关）
| 概念 | 文件:行 |
|---|---|
| `MultiAgentVersion` 枚举 | `codex-rs/protocol/src/protocol.rs:2821` |
| 版本决策 `multi_agent_version_override` | `codex-rs/core/src/config/mod.rs:1523` |
| Feature flags（Collab/MultiAgentV2） | `codex-rs/features/src/lib.rs:1069-1080` |
| 工具注册 `add_collaboration_tools` | `codex-rs/core/src/tools/spec_plan.rs:1124` |
| 工具可见性 gate `collab_tools_enabled` | `spec_plan.rs:599` |
| V1 工具定义 `create_spawn_agent_tool_v1` | `codex-rs/core/src/tools/handlers/multi_agents_spec.rs:67` |
| V2 工具定义 `create_spawn_agent_tool_v2` | `multi_agents_spec.rs:102` |
| 工具 description（V1 详尽策略） | `multi_agents_spec.rs:682` |
| V1 handler `handle_spawn_agent` | `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs:44` |
| V2 handler | `codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs` |
| `build_agent_spawn_config` / `apply_spawn_agent_role` | `multi_agents_common.rs:178 / :385` |
| `thread_spawn_source`（agent_path/depth 计算） | `multi_agents_common.rs:112` |

### 运行时层（fork 与子线程生命周期）
| 概念 | 文件:行 |
|---|---|
| `spawn_subagent`（SDK 入口） | `codex-rs/core/src/thread_manager.rs:888` |
| `start_thread_inner` | `thread_manager.rs:863` |
| `spawn_thread`（统一内核） | `thread_manager.rs:1682` |
| `fork_history_from_snapshot` / `ForkSnapshot` | `thread_manager.rs:2057 / :166` |
| `stored_thread_to_initial_history` | `thread_manager.rs:1916` |
| `ThreadSpawnRequest` / `NewThread` | `thread_manager.rs:255 / :150` |
| `AgentRunner::start` / `AgentInvocation` | `codex-rs/ext/agent/src/lib.rs:45 / :20` |
| `ForkPersistence` | `codex-rs/core/src/session/mod.rs:378` |
| `InterruptedTurnHistoryMarker` | `codex-rs/core/src/tasks/mod.rs:78` |
| depth 计数 `next_thread_spawn_depth` / `exceeds_thread_spawn_depth_limit` | `codex-rs/core/src/agent/registry.rs:72 / :76` |
| V1 并发 `reserve_spawn_slot` / `SpawnReservation::Drop` | `registry.rs:81 / :334` |
| V2 并发 `AgentExecutionLimiter` / `ensure_execution_capacity` | `codex-rs/core/src/agent/control/execution.rs:14 / :44` |
| V2 residency LRU `try_unload_one_resident` | `codex-rs/core/src/agent/control/residency.rs:117` |
| 统一派生内核 `spawn_agent_internal` | `codex-rs/core/src/agent/control/spawn.rs:382` |
| `SpawnAgentThreadInheritance`（继承 env/exec_policy） | `control/spawn.rs:421` |

### 协议 / 追踪层
| 概念 | 文件:行 |
|---|---|
| `SubAgentSource` / `SessionSource` / `ThreadSource` | `codex-rs/protocol/src/protocol.rs:2647 / :2569 / :2586` |
| `AgentPath`（分层路径 /root/...） | `codex-rs/protocol/src/agent_path.rs:15` |
| `ThreadSpawnMetadata`（reducer 投影） | `codex-rs/rollout-trace/src/reducer/thread.rs:257` |
| `AgentOrigin`（trace 模型） | `codex-rs/rollout-trace/src/model/session.rs:55` |
| spawn edge / 通信边 / 结果边 | `codex-rs/rollout-trace/src/reducer/tool/agents.rs:31,50,66` |
| agent-graph-store（拓扑持久化） | `codex-rs/agent-graph-store/src/types.rs:7` |
| completion watcher（V1 注 msg / V2 发通信） | `codex-rs/core/src/agent/control.rs:511` |
| analytics `SubAgentThreadStartedInput` | `codex-rs/analytics/src/facts.rs:365` |
| analytics 事件构建 | `codex-rs/analytics/src/events.rs:1407` |

### 扩展层与编排注入
| 概念 | 文件:行 |
|---|---|
| `AgentSpawner` trait | `codex-rs/ext/extension-api/src/capabilities/agent.rs:13` |
| 13 个 contributor trait | `codex-rs/ext/extension-api/src/contributors.rs:57` |
| `ext/agent` AgentRunner（host 侧，不用 trait） | `codex-rs/ext/agent/src/lib.rs:35` |
| `ext/guardian`（薄壳 78 行） | `codex-rs/ext/guardian/src/lib.rs` |
| guardian 真引擎（~6000 行） | `codex-rs/core/src/guardian/{mod,review,review_session,prompt}.rs` |
| guardian policy prompt | `codex-rs/core/src/guardian/policy.md` + `policy_template.md` |
| guardian 装配 `guardian_agent_spawner` | `codex-rs/app-server/src/extensions.rs:269 / :101` |
| base instructions 取出 `get_model_instructions` | `codex-rs/protocol/src/openai_models.rs:491` |
| base instructions 优先级 | `codex-rs/core/src/session/mod.rs:640` |
| world-state 组装 `build_world_state_for_step` | `codex-rs/core/src/session/world_state.rs:33` |
| V2 动态 hint 门控 | `codex-rs/core/src/session/multi_agents.rs:9` |
| MultiAgentMode（Proactive/Explicit/Custom） | `codex-rs/core/src/session/multi_agents.rs:39` |
| 代码 diff 审查 prompt | `codex-rs/prompts/templates/review/rubric.md` |
| 代码 diff 审查引擎 | `codex-rs/core/src/tasks/review.rs` |

---

## 附录 B：核心数据结构速查

```rust
// 版本（只影响路径 A）
enum MultiAgentVersion { Disabled, V1, V2 }                          // protocol.rs:2821

// 子 agent 身份
enum SessionSource { Cli, VSCode(…), Exec, Mcp, Custom(String),
                     Internal(InternalSessionSource),
                     SubAgent(SubAgentSource), Unknown }            // protocol.rs:2569

enum SubAgentSource {                                               // protocol.rs:2647
    Review,                                                          //   活跃：代码 diff 审查
    Compact,                                                         //   死变体
    ThreadSpawn { parent_thread_id, depth, agent_path,
                  agent_nickname, agent_role },                      //   活跃：V1+V2 多智能体派生
    MemoryConsolidation,                                             //   死变体
    Other(String),                                                   //   活跃：guardian("guardian")
}

enum ThreadSource { User, Subagent, Feature(String), MemoryConsolidation }  // protocol.rs:2586

// 分层路径（运行时构建，非配置加载）
struct AgentPath(String);   // "/root" → "/root/researcher" → "/root/researcher/worker"，"/morpheus"

// role 配置（声明）
struct AgentRoleToml { description?, config_file?, nickname_candidates? }   // config_toml.rs:699
struct AgentsToml { enabled?, max_concurrent_threads_per_session?, max_depth?,
                    default_subagent_model?, default_subagent_reasoning_effort?,
                    interrupt_message?, #[flatten] roles: BTreeMap<String, AgentRoleToml> }  // :662

// role 配置（运行时）
struct AgentRoleConfig { description?, config_file?, nickname_candidates? }  // mod.rs:2328

// 派生请求/产物
struct ThreadSpawnRequest { options, auth_manager, agent_control, parent_thread_id?,
    forked_from_thread_id?, fork_persistence, inherited_environments?,
    inherited_exec_policy?, user_shell_override? }                          // thread_manager.rs:255
struct NewThread { thread_id, thread: Arc<CodexThread>, session_configured } // :150
struct AgentInvocation { config, prompt, parent_trace }                     // ext/agent/lib.rs:20
struct AgentRun { thread_id, turn_id, thread: Arc<CodexThread> }            // :27
```
