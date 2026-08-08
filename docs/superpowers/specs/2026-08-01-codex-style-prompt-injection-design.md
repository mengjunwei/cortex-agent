# 对齐 codex 的提示词注入：稳定前缀缓存 + 基线/易变拆分

> 日期：2026-08-01
> 状态：已确认，待实现
> 前置基础：BASE_INSTRUCTION 已下沉为 CortexAgent 固定层（本工作区已落地 —— `cortex_agent.rs::build_system_prompt` 分层注入、`custom.rs` 用户 instruction 为主 + BASE 追加）

## 1. 背景

CortexAgent（`src/agent/runtime/cortex_agent.rs`）当前把 6 层注入（instruction / BASE_INSTRUCTION / environment / permissions / skill_catalog / skill_bodies）全部拼接进**一条 role=system 的 Content（preamble）**，每次 LLM 请求全量重发（项目走无状态协议，`previous_response_id` 在 OpenAI/Anthropic 下恒 None）。

参照原型 codex 的实现（codex 靠 ①`prompt_cache_key` ②world_state diff 注入 ③稳定前缀 三招压低全量重发成本），本设计对齐其中的「稳定前缀缓存」与「基线/易变拆分」两招。第三招 diff 注入经评估对本项目边际收益小，不做（见 §9）。

## 2. 问题根因

当前 system 前缀**无法命中缓存**，根因有二：

1. **时间字段击穿前缀**：environment 层含「Current time」（`render_environment_layer`，每分钟变），它和稳定内容混在同一条 system 字符串里 → 整条 system 跨请求字节变化 → 无论哪种协议缓存都 miss。
2. **Anthropic 断点失效**：`anthropic_custom/convert.rs` 的 `content_to_message` 中 `prompt_caching` 参数被忽略（变量名带下划线 `_prompt_caching`），且 system 合并成顶层字符串后只在整个末尾打断点；system 内含时间 → 缓存内容本身在变 → 即使有断点也 miss。

**关键洞察**：6 层注入里，**会话内真正会变的只有「当前时间」一项**（instruction / BASE / OS / 工作目录 / permissions / skills 在一个会话内基本不变）。因此最优分法是让稳定部分尽可能大、易变部分（时间）尽可能小且独立。

## 3. 目标与非目标

### 目标
- system prompt 的**稳定部分跨请求字节级不变** → 命中厂商缓存（OpenAI 自动前缀缓存 / Anthropic ephemeral）。
- 「固定基线」与「易变上下文」在消息层分离，对齐 codex 的分层注入理念。
- 修复 Anthropic 客户端 `prompt_caching` 参数被忽略的 bug，使断点真正生效。

### 非目标
- **不做跨轮 diff 注入**（codex 第三招）：本项目易变源只有时间，diff 边际收益小、需打破 `run()` 无状态模型，性价比低（见 §9）。
- 不引入 OpenAI Responses API / `prompt_cache_key`：本项目 OpenAI 端走 Chat Completions，无此字段，改造超范围。
- 不把时间注入 user 消息：会污染持久化与前端回显。
- 不注入 cwd（现有 TODO，保持现状）。

## 4. 架构：stable / volatile 消息分层

把当前 `[一条大 system]` 拆成两条 role=system 消息：

```
preamble = [
  stable_system,    # 跨请求字节不变 → 缓存命中
  volatile_system,  # 仅当前时间，每次刷新
]
conv = [stable_system, volatile_system, ...history]
```

**stable_system**（会话内不变）：
1. instruction（调用方特化指令：自定义助手=用户 prompt，内置助手=专业 prompt）
2. BASE_INSTRUCTION
3. environment 静态部分（OS）—— 时间已移出
4. permissions（sandbox + approval）
5. skill_catalog
6. skill_bodies

**volatile_system**（每次刷新）：
- 当前时间（current_date）

## 5. 协议扩展约定

通过现有 `config.extensions`（与 thinking 参数同机制）传递 stable 边界：

```
config.extensions["cortex"]["stable_system_count"] = <n>
```

含义：preamble 中前 `n` 条 role=system 的 Content 属于稳定前缀。
- OpenAI 端：可忽略（靠消息顺序天然命中前缀缓存）。
- Anthropic 端：用于决定 `cache_control` 打断点的位置。

## 6. 客户端层改动（src/llm）

### 6.1 OpenAI 兼容（`open_ai_custom_llm.rs`）
- **基本不动**。`content_to_message`（:399-480）已逐条映射 system，天然支持多条 system 消息、保持顺序。
- 厂商自动前缀缓存靠 `stable_system` 在 messages 数组最前且字节稳定命中。
- 验证点：`build_request_json`（:515-606）输出多条 system 时顺序为 `[stable, volatile, ...history]`。

### 6.2 Anthropic（`anthropic_custom/`）
**核心改动 1 — 修复被忽略的 prompt_caching（bug）**：
- `convert.rs:35-165 content_to_message`：`_prompt_caching` 参数当前被忽略，需恢复其实际作用（或移除该误导性参数，改由上层 `build_message_params` 统一决定断点）。

**核心改动 2 — system 按 stable 边界分 block 打断点**：
- `client.rs:136-305 build_message_params`：合并多条 system 成顶层 system 字段时，按 `extensions["cortex"]["stable_system_count"]` 拆成两个 TextBlock：
  - stable 部分 → `TextBlock::new(...).with_cache_control(CacheControlEphemeral)`（命中缓存）
  - volatile 部分（时间）→ 普通 TextBlock，不打断点
- 现有 `convert.rs:401-408` 的「整个 system 打一个断点」逻辑调整为「只给 stable block 打」。

**断点预算（Anthropic 上限 4 个）**：
- stable system 末：1 个（本设计）
- tools 末：1 个（可选，后续）
- 合计 ≤ 2，远低于上限。

**向后兼容**：`stable_system_count` 缺失或为 0 时，退化为现状行为（system 合并、末尾打断点），不破坏现有调用方。

## 7. agent 层改动（`src/agent/runtime/cortex_agent.rs`）

| 函数 / 位置 | 改动 |
|---|---|
| `build_system_prompt`（:174-218） | 拆成 `build_stable_prefix(...) -> String` + `build_volatile_context() -> String`。时间从 environment 移出。 |
| `render_environment_layer`（:214-226） | 只保留 OS（时间移走）；时间单独 `render_current_time()` 进 volatile。 |
| preamble 构造（:326-331） | `vec![一条 system]` → `vec![stable_system, volatile_system]`；同时写 `config.extensions["cortex"]["stable_system_count"] = 1`。 |
| conv 组装（:364-365） | `conv = [stable, volatile, ...history]`（顺序保证 stable 在最前）。 |
| compaction（:392-468） | `preamble_len = 1`（:394）→ `preamble_len = 2`（stable + volatile 都保护，不被摘要）；split_point 调整逻辑同步。 |

**run() 保持无状态**：每轮仍 `conv.clone()` 全量发；stable 靠字节稳定 + 协议缓存命中，不引入跨轮状态。

**时间刷新策略**：同一 run（一次用户消息）内 preamble 在 loop 外构建一次，时间不逐轮刷新（跨分钟误差可接受）；跨 run（下条消息）重建 CortexAgent 时时间刷新。缓存命中主要依赖跨 run 的 stable 不变。

## 8. 数据流

```
build_custom_agent / build_builtin
  → CortexAgentBuilder.instruction(...)        # 用户/专业 prompt
  → build()

CortexAgent::run()
  ├─ build_stable_prefix()    → instruction + BASE + env(OS) + perm + skills
  ├─ build_volatile_context() → 当前时间
  ├─ preamble = [stable_system, volatile_system]
  ├─ config.extensions["cortex"]["stable_system_count"] = 1
  ├─ conv = [stable, volatile, ...history]
  └─ loop { LlmRequest{contents: conv.clone()} → client }
       ├─ OpenAI: [stable_system, volatile_system, ...history] → 前缀缓存命中 stable
       └─ Anthropic: system = [stable_block(+cache_control), volatile_block] → 缓存命中 stable_block
```

## 9. 为什么不做 diff 注入（codex 第三招）

codex 的 world_state diff 注入是为它「十几个 section 可能各自变化」的场景设计。本项目经 §2 分析，**会话内真正变化的只有时间一项** —— 只要时间移出 stable，其余前缀已天然字节稳定，无需 diff 机制即可命中缓存。引入 diff 需打破 `run()` 无状态模型、新增跨轮状态载体（上一轮快照）、处理重置/并发，改造成本高而收益边际，故不做。若未来易变源增多（如动态注入运行时状态），再评估。

## 10. 错误处理与边界

- **Anthropic 4 断点上限**：本设计最多用 1（stable system 末），安全。
- **OpenAI 厂商不支持缓存**：无副作用，stable 仍正常发送，只是不享受缓存折扣。
- **stable_system_count 缺失/为 0**：Anthropic 退化为现状（合并 system、末尾打断点），向后兼容。
- **instruction 为空**：stable 仍含 BASE_INSTRUCTION，不为空，行为正常。
- **同一 run 跨分钟**：volatile 时间变，stable 不受影响，缓存仍命中 stable 部分。

## 11. 测试策略

### 单元测试（cortex_agent.rs）
- `build_stable_prefix` 确定性：相同输入产出相同字节（且不含时间）→ 证明跨请求稳定。
- `build_volatile_context` 仅含时间。
- preamble 构造后 `config.extensions["cortex"]["stable_system_count"] == 1`。

### Anthropic payload 测试（anthropic_custom）
- 多条 system → 顶层 system 拆成 2 个 TextBlock，数量正确。
- stable block 带 `cache_control: ephemeral`，volatile block 不带。
- `stable_system_count` 缺失时退化为单 block + 末尾断点（向后兼容）。
- 断点总数 ≤ 4。

### OpenAI payload 测试
- 多条 system 顺序保留：`[stable, volatile, ...history]`。

### compaction 测试
- `preamble_len=2`：stable + volatile 均受保护，不被摘要；split_point 不误切。

## 12. 改造落点清单

**客户端层 src/llm**
- `anthropic_custom/convert.rs`：恢复/重写 prompt_caching 实际作用；system block 分段打断点。
- `anthropic_custom/client.rs::build_message_params`：读 `stable_system_count`，按边界拆 system block。
- `open_ai_custom_llm.rs`：验证多条 system 顺序，基本不改。

**agent 层 src/agent/runtime/cortex_agent.rs**
- `build_system_prompt` → 拆 `build_stable_prefix` + `build_volatile_context`。
- `render_environment_layer` → 移除时间；新增 `render_current_time`。
- preamble（:326）→ 两条 system + 写 extensions。
- conv 组装（:364）→ `[stable, volatile, history]`。
- compaction（:394）→ `preamble_len=2`。

## 13. 风险与权衡

| 风险 | 影响 | 缓解 |
|---|---|---|
| Anthropic 改 `content_to_message` 签名波及调用方 | 编译/回归 | 保持向后兼容参数；`stable_system_count` 缺失走旧路径 |
| stable 边界判断错误导致断点打错位 | 缓存不命中或超额 | 单测覆盖 block 数量与断点位置；4 断点上限裕度大 |
| 时间精度影响（同 run 跨分钟） | volatile 变化 | 不影响 stable 缓存；可接受 |
| compaction `preamble_len` 改动引入历史切割 bug | 摘要误伤前缀 | 单测覆盖 split_point；保留现有 FunctionCall/Response 配对调整逻辑 |
