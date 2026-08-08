# 重复退化反思循环设计

> 日期：2026-07-31
> 状态：⚠️ 已废弃（2026-08，commit a2440ad）——本项目已对齐 codex/opencode/claurst **移除所有文本退化检测**，本设计的「检测 + 反思续写」机制未落地且不再采纳（代码中 `REPETITION_MIN_CHARS` / `count_tail_repetitions` / `reflection_count` 等符号均已清除）。模型复读属能力问题，靠换模型 + `max_iterations` 兜底。本文仅作历史快照保留，勿据此实现。
> 前置提交：`f6ee96c`（重复死循环修复 ——「硬中断」版本，本设计将其改造为「反思继续」）

## 1. 背景

`run_report.py` 场景下，LLM 反复输出「让我再次运行 run_report.py」陷入死循环。根因诊断结论：

- **主因**：LLM 单次流式退化（degeneration）—— 自回归采样在低温 + 零重复惩罚 + 上下文正反馈下，token 级重复。
- **放大器**：`cortex_agent` 主循环在「无 FunctionCall 且无结束信号」时 `continue` 重调，每轮把重复文本喂回上下文。

`f6ee96c` 的处理是**硬中断**：SSE 转发层检测到重复 → 发 `[输出被中断]` + `RunFinished` → 对话结束。

## 2. 目标与非目标

### 目标
- 检测到重复退化时，**不终止对话**，而是后端静默注入反思提示，引导模型换方法**继续推进任务**。
- **前端不感知**：用户看不到任何「反思 / 系统」提示，只看到模型换了说法继续输出。
- 反思有上限，避免「反思」本身退化成新的死循环。

### 非目标
- 不追求 100% 消除「用户可见的重复」—— 流式已发往前端的文本无法回收，只能把可见量压到阈值以内。
- 不改变 LLM 本身的采样退化倾向（由 `frequency_penalty` 等参数从源头缓解，属另一条独立措施，本设计与之互补）。

## 3. 架构

**检测 + 反思全部在 `cortex_agent.run()` 主循环内完成；SSE 转发层回归纯转发。**

理由：SSE 层（`sse.rs::create_event_stream`）是 event stream 的**消费者**，只能转发或中断整个流，无法驱动 agent 「清理 → 注入反思 → 重新调用 LLM」。只有主循环能决定继续。因此 `f6ee96c` 放在 SSE 层的「检测 → 中断 → return」必须上移到 agent 层，并改为「检测 → 截断 → 反思 → continue」。

```
                ┌─────────────────────────────────────┐
                │   cortex_agent.run()  主循环         │
                │   （检测 + 截断 + 反思 + 继续）      │
                └──────────────┬──────────────────────┘
                               │ event stream
                ┌──────────────▼──────────────────────┐
                │   sse.rs  纯转发（移除检测/中断）    │
                └─────────────────────────────────────┘
```

## 4. 数据流

```
每轮 iteration：LLM 流式调用
 └ 内层 while 读 chunk：
     · 累积本轮 assistant 文本（acc_text）
     · should_stream 时照常 yield chunk 给前端
     · ★ 实时检测：acc_text.chars().count() > REPETITION_MIN_CHARS（300）
                  && count_tail_repetitions(&acc_text, REPETITION_TAIL_CHARS) > REPETITION_OCCUR_THRESHOLD（3）
        → 设 truncated_by_repetition = true；break（提前截断这次调用，不再读剩余 chunk）
     · chunk.turn_complete / finish_reason → break（正常结束）

 外层判定（fcs = 本轮 FunctionCall 列表）：
  ├ truncated_by_repetition（重复截断）：
  │     · 不 push 本轮重复 content（切断正反馈 —— 关键）
  │     · reflection_count += 1
  │     · if reflection_count > REFLECTION_LIMIT（2）：
  │           yield make_text_event("[已多次反思仍重复，停止生成]")  ← 唯一对前端可见的异常文本
  │           break
  │     · else：
  │           push 反思提示（role=user）到 conv          ← 静默，不 yield 前端
  │           continue                                   ← 模型带反思重新生成
  │
  ├ 正常结束 && fcs.is_empty()：break（纯文本回答完成）
  │
  └ fcs 非空：push content；执行工具；回填；下轮

 注：reflection_count 在「本轮未触发重复截断」时归零（模型换了方法，重新计数）。
```

## 5. 组件

### 5.1 `count_tail_repetitions(text, tail_chars) -> usize`
- **从 `sse.rs` 迁入 `cortex_agent.rs`**（连同其 4 个单元测试）。
- 字符边界安全：用 `char_indices().rev().nth()` 定位字符边界后切片，避免原字节切片在中文 UTF-8 上 panic。
- 内层实时检测与外层判定共用。

### 5.2 常量（置于 `cortex_agent.rs`）
```rust
const REPETITION_MIN_CHARS: usize = 300;   // 累积文本达到此字符数才开始检测
const REPETITION_TAIL_CHARS: usize = 200;  // 取最后 N 个字符作为 tail
const REPETITION_OCCUR_THRESHOLD: usize = 3; // tail 在全文出现次数超过此值判定为重复
const REFLECTION_LIMIT: u32 = 2;           // 连续反思次数上限
```
> 300 字符阈值平衡「可见重复量」与「误判风险」：日常回答（含适度排比）不会误触；明显的 token 级循环会在 ~300 字符内被 tail 重复检测命中。

### 5.3 反思提示（`REFLECTION_PROMPT` 常量，role=user）
```
⚠️ 系统检测到你的上一段输出在重复同一内容（疑似生成退化）。请回顾用户的原始目标与当前任务进度，换一种方法或思路继续推进，避免重复之前的表述。
```
- **静默注入 conv，不 yield 给前端**（前端零感知）。
- 用 role=user（兼容 OpenAI Chat Completions 的多轮 role 规则，比中途插入 system 更安全）。

### 5.4 状态变量（主循环内）
- `acc_text: String` —— 本轮累积文本（每轮 iteration 开始清空）。
- `truncated_by_repetition: bool` —— 本轮是否因重复被截断。
- `reflection_count: u32` —— 连续反思计数（跨轮累积，正常轮归零）。

## 6. SSE 层改动（`sse.rs`）

回退 `f6ee96c` 在 SSE 层引入的检测逻辑，回归纯转发：
- 移除 `count_tail_repetitions` 函数（已迁入 cortex_agent）。
- 移除 `create_event_stream` 内的 `repetition_count` 声明与「检测 → 中断 → return」代码块。
- 移除 `sse.rs` 末尾的 `repetition_tests` 模块（随函数迁入 cortex_agent）。
- `assistant_text`、`agent_author` 等仅用于该检测的变量若不再被他处使用，一并清理。

## 7. 与 `f6ee96c` 其他改动的兼容性

| `f6ee96c` 改动 | 本设计处理 |
|---|---|
| SSE 层重复检测+中断 | **移除**（上移到 agent 层） |
| `cortex_agent` `continue→break` | **再改**：正常结束→`break`；重复截断→反思 `continue` |
| `open_ai_custom_llm` `ended` flag 结束信号补发 | **保留**（修复 provider 不发 finish_reason，与反思独立且互补） |
| `make_gen_config` 默认 penalty | **保留**（源头降低退化概率，与反思互补） |
| `build_request_json` penalty 透传 | **保留** |

## 8. 兜底链

1. 第一道：`frequency_penalty` / `presence_penalty`（默认 0.4 / 0.3）从源头压低退化概率。
2. 第二道：内层流式实时截断（~300 字符即截断），限制单次可见重复。
3. 第三道：反思 + 换方法继续（最多 2 次）。
4. 第四道：反思 2 次仍重复 → 发停止文本 + `break`。
5. 最终：`max_iter = 50` 仍作绝对上限。

## 9. 测试策略

| 测试对象 | 方式 | 状态 |
|---|---|---|
| `count_tail_repetitions` | 单元（随迁移，4 测试） | 复用 |
| 反思决策（给定 `reflection_count` + 是否重复 → continue/stop/break） | 抽纯函数 `decide_after_repetition` 单测 | **新增** |
| 主循环集成（重复 → 反思 → 继续 → 上限停止） | 需 mock `InvocationContext` / LLM stream | **不写**（mock 成本过高，靠逻辑论证 + 编译 + 上面两层测试兜底） |

`decide_after_repetition` 示例签名：
```rust
enum RepetitionAction { Continue, Stop }
fn decide_after_repetition(reflection_count: u32, limit: u32) -> RepetitionAction {
    if reflection_count > limit { RepetitionAction::Stop } else { RepetitionAction::Continue }
}
```

## 10. 风险

- **误判**：阈值 300 字符 + tail 重复 >3 次，正常排比/列举可能误触。缓解：`count_tail_repetitions` 对「正常多样化文本」的测试已验证不误判（`normal_diverse_text_not_flagged`）；`reflection_count` 重置机制避免偶发误判累积。
- **反思无效**：模型带反思仍重复 → 2 次上限兜底停止。
- **流式已发文本无法回收**：用户会看到 ≤ ~300 字符的重复段，属可接受折衷（非目标已声明）。
- **role=user 反思消息在 conv 累积**：若多次反思，conv 里有多条反思消息。反思成功（计数归零）后旧反思消息仍在 conv，但属正常历史，不影响；且 compaction 会清理。

## 11. 不在本设计范围

- 不调整 `max_iter`、compaction 阈值。
- 不改前端消息块逻辑（`message_id` 复用机制保持不变，反思轮无 FunctionCall 故同 id 持续追加）。
- 不把阈值暴露为运行时配置（用常量；后续如需可提取到 `AppConfig`）。
