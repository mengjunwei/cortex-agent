# 对齐 codex 提示词注入（稳定前缀缓存 + 基线/易变拆分）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 CortexAgent 的 system prompt 拆成 stable 前缀（跨请求不变，命中厂商缓存）+ volatile 段（仅时间），并让 Anthropic 客户端按 stable 边界打 `cache_control` 断点，降低长对话 token 成本。

**Architecture:** agent 层（`cortex_agent.rs`）把 6 层注入拆成 `build_stable_prefix`（instruction+BASE+env+perm+skills，无时间）+ `build_volatile_context`（仅时间），preamble 改为两条 role=system 消息，并通过 `config.extensions["cortex"]["stable_system_count"]` 告知客户端 stable 边界。Anthropic 客户端（`convert.rs` + `client.rs`）按该边界把 system 拆成两个 TextBlock，stable block 打 `cache_control: ephemeral`，volatile 不打。OpenAI 端无需改动（已天然支持多条 system 顺序，靠前缀自动缓存）。

**Tech Stack:** Rust，adk-rust（`Content`/`Part`/`GenerateContentConfig.extensions`），adk-anthropic（`TextBlock`/`CacheControlEphemeral`/`SystemPrompt`），项目自有客户端 `src/llm`。

## Global Constraints

- 仅改动自有代码：`src/agent/runtime/cortex_agent.rs`、`src/llm/anthropic_custom/convert.rs`、`src/llm/anthropic_custom/client.rs`。不碰外部 adk-rust / adk-anthropic crate。
- 中文注释，对齐项目既有风格。
- 向后兼容：`stable_system_count` 缺失时 Anthropic 端退化为"全部 system 一个 block"（等价旧行为）。
- `run()` 保持无状态（每轮 `conv.clone()` 全量发），不引入跨轮状态。
- 频繁提交，每个 Task 结束提交一次。

---

## File Structure

| 文件 | 职责 | 本计划改动 |
|---|---|---|
| `src/agent/runtime/cortex_agent.rs` | CortexAgent 运行时、system prompt 分层注入、compaction | 拆 stable/volatile 渲染；preamble 双消息；写 extensions；compaction `preamble_len=2` |
| `src/llm/anthropic_custom/convert.rs` | ADK ↔ adk-anthropic 类型转换、`build_message_params` 组装 | 新增 `SystemPromptSegments` 类型；system 按 stable/volatile 分 block 打 cache |
| `src/llm/anthropic_custom/client.rs` | Anthropic 客户端、`build_message_params` 消息分流 | 新增 `split_system_segments` 纯函数；读 `stable_system_count`；构造 segments |
| `src/llm/open_ai_custom_llm.rs` | OpenAI 兼容客户端 | **不改**（已支持多条 system 顺序） |

---

## Task 1: CortexAgent stable/volatile 消息分层

**Files:**
- Modify: `src/agent/runtime/cortex_agent.rs`（`build_system_prompt` :174-218、`render_environment_layer` :223-235、preamble :316-331、compaction `preamble_len` :394、compaction 重建 :451-454）
- Test: `src/agent/runtime/cortex_agent.rs`（新增 `prompt_injection_tests` mod）

**Interfaces:**
- Produces: `fn build_stable_prefix(instruction: &Option<String>, skill_catalog: &Option<String>, skill_bodies: &Option<String>, policy: PermissionPolicy) -> String`；`fn build_volatile_context() -> String`；preamble 写入 `config.extensions["cortex"]["stable_system_count"] = 1`（Task 2 的 client 读取此键）。

- [ ] **Step 1: 写失败测试 —— stable/volatile 渲染确定性**

在 `src/agent/runtime/cortex_agent.rs` 文件末尾（现有 `mod repetition_tests` 之后）新增测试模块：

```rust
#[cfg(test)]
mod prompt_injection_tests {
    use super::*;
    use crate::domain::permissions::PermissionPolicy;

    #[test]
    fn stable_prefix_is_deterministic_and_time_free() {
        // 相同输入两次构建必须字节一致（跨请求稳定 → 缓存命中前提）
        let a = build_stable_prefix(&None, &None, &None, PermissionPolicy::default());
        let b = build_stable_prefix(&None, &None, &None, PermissionPolicy::default());
        assert_eq!(a, b, "stable prefix 必须跨调用字节一致");
        assert!(!a.contains("Current time"), "stable 前缀不得含时间");
        assert!(a.contains("Operating System"), "stable 前缀应含 OS");
    }

    #[test]
    fn volatile_context_contains_only_time() {
        let v = build_volatile_context();
        assert!(v.contains("Current time"), "volatile 段应含时间");
        assert!(!v.contains("Operating System"), "volatile 段不得含 OS");
    }

    #[test]
    fn stable_prefix_includes_instruction_first() {
        let instr = Some("你是翻译助手".to_string());
        let s = build_stable_prefix(&instr, &None, &None, PermissionPolicy::default());
        assert!(s.starts_with("你是翻译助手"), "instruction 必须在 stable 前缀最前");
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test --bin cortex-agent prompt_injection`
Expected: 编译失败 —— `build_stable_prefix` / `build_volatile_context` 未定义（函数还不存在）。

- [ ] **Step 3: 拆分渲染函数 —— 替换 `build_system_prompt`**

用以下两个函数**整体替换**现有 `build_system_prompt`（:174-218，从 `fn build_system_prompt(` 到对应 `}` 结束，含上方文档注释）：

```rust
/// 构建 stable 前缀（跨请求字节不变 → 命中厂商缓存）。
///
/// 层次：
/// 1. instruction — 调用方特化指令（自定义助手=用户人设，内置助手=专业 prompt）；为空则跳过
/// 2. BASE_INSTRUCTION — CortexAgent 固定注入的通用行为基线（始终追加）
/// 3. environment — OS（不含时间，时间移至 volatile 避免击穿缓存前缀）
/// 4. permissions — sandbox_mode + approval_policy
/// 5. skill catalog — 可用 skill 的 name + desc
/// 6. skill bodies — 用户 @ 提及的 skill 正文
fn build_stable_prefix(
    instruction: &Option<String>,
    skill_catalog: &Option<String>,
    skill_bodies: &Option<String>,
    policy: PermissionPolicy,
) -> String {
    let mut layers: Vec<String> = Vec::new();

    if let Some(i) = instruction {
        if !i.trim().is_empty() {
            layers.push(i.clone());
        }
    }
    layers.push(crate::prompts::BASE_INSTRUCTION.to_string());
    layers.push(render_environment_layer());
    layers.push(render_permissions_layer(policy));
    if let Some(catalog) = skill_catalog {
        if !catalog.is_empty() {
            layers.push(catalog.clone());
        }
    }
    if let Some(bodies) = skill_bodies {
        if !bodies.is_empty() {
            layers.push(bodies.clone());
        }
    }
    layers.join("\n\n")
}

/// 构建 volatile 段（每次刷新；当前仅时间）。单独成 system 消息，不进 stable 前缀，
/// 保证 stable 字节级稳定以命中厂商缓存。
fn build_volatile_context() -> String {
    render_current_time()
}
```

- [ ] **Step 4: 拆分 environment —— 时间移出，新增 `render_current_time`**

替换现有 `render_environment_layer`（:220-235，含文档注释）为：

```rust
/// 渲染 environment 静态层（仅 OS；时间已移至 volatile，避免击穿缓存前缀）。
///
/// TODO: 工作区路径(cwd)需从会话注入（CortexAgent 当前不持有 sandbox_dir），留待后续。
fn render_environment_layer() -> String {
    let os = if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Unknown"
    };
    format!("## Environment\n\nOperating System: {os}")
}

/// 渲染 volatile 层（当前时间）。每次请求刷新，单独成段，不进 stable 前缀。
fn render_current_time() -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!("## Current time\n\n{now}")
}
```

- [ ] **Step 5: 运行测试，确认通过**

Run: `cargo test --bin cortex-agent prompt_injection`
Expected: 3 个测试 PASS。

- [ ] **Step 6: 改 preamble 为双消息 + 写 extensions**

替换 `run()` 内的 system prompt 构建 + preamble 段（:316-331，从 `// Build system prompt` 注释到 `preamble` 的 `];`）：

```rust
        // Build system prompt: stable 前缀（跨请求不变，命中缓存）+ volatile 段（时间，每次刷新）
        let stable_prompt = build_stable_prefix(
            &self.instruction,
            &self.skill_catalog,
            &self.skill_bodies,
            self.policy,
        );
        let volatile_prompt = build_volatile_context();

        // preamble = [stable_system, volatile_system]；stable 在最前保证前缀缓存命中。
        let preamble = vec![
            Content {
                role: "system".to_string(),
                parts: vec![Part::Text { text: stable_prompt }],
            },
            Content {
                role: "system".to_string(),
                parts: vec![Part::Text { text: volatile_prompt }],
            },
        ];

        // 告知 Anthropic 客户端 stable 边界（用于 system 分 block 打 cache_control）。
        // OpenAI 端忽略此键，靠消息顺序天然命中前缀缓存。
        if let Some(c) = config.as_mut() {
            let cortex = c
                .extensions
                .entry("cortex".to_string())
                .or_insert_with(|| json!({}));
            if let Some(obj) = cortex.as_object_mut() {
                obj.insert("stable_system_count".to_string(), json!(1u64));
            }
        }
```

> 注：`json!` 与 `Value` 已在文件顶部 `use adk_rust::serde_json::{Value, json};` 导入。`config` 是 `run()` 开头的 `let mut config = ...`（:281），在此处可变借用合法。

- [ ] **Step 7: 改 compaction 保护两条前缀**

修改 compaction 段两处。

第一处（:394）：
```rust
                    let preamble_len = 1; // system message
```
改为：
```rust
                    let preamble_len = 2; // stable_system + volatile_system
```

第二处（:451-454）：
```rust
                        // 重建：[preamble, summary?, ...retained_users, ...tail]
                        let preamble_msg = conv[0].clone();
                        conv.clear();
                        conv.push(preamble_msg);
```
改为：
```rust
                        // 重建：[preamble(stable+volatile), summary?, ...retained_users, ...tail]
                        let preamble_msgs: Vec<Content> = conv[..preamble_len].to_vec();
                        conv.clear();
                        conv.extend(preamble_msgs);
```

> `split_point` 调整循环（:400 `while split_point > preamble_len + 1`）与 `older` 切片（:419 `conv[preamble_len..split_point]`）均按 `preamble_len` 计算，自动适配为 2，无需改动。

- [ ] **Step 8: 编译并运行全部测试**

Run: `cargo test --bin cortex-agent`
Expected: 编译通过，所有测试 PASS（含既有 `repetition_tests` 与新 `prompt_injection_tests`）。

- [ ] **Step 9: 提交**

```bash
git add src/agent/runtime/cortex_agent.rs
git commit -m "feat(prompt): CortexAgent 拆分 stable/volatile system 注入

stable 前缀(instruction+BASE+env+perm+skills, 无时间)跨请求字节不变以命中缓存;
volatile 段(仅时间)单独成 system 消息。preamble 改双消息, 写 extensions
stable_system_count 告知客户端边界, compaction preamble_len=2 保护两条前缀。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Anthropic 客户端按 stable 边界分 block 打 cache_control

**Files:**
- Modify: `src/llm/anthropic_custom/convert.rs`（新增 `SystemPromptSegments` 类型；`build_message_params` 签名 :382 + system 处理 :401-408）
- Modify: `src/llm/anthropic_custom/client.rs`（`build_message_params` system 构造 :190-196；新增 `split_system_segments` 纯函数）
- Test: `src/llm/anthropic_custom/client.rs`（新增 `prompt_cache_tests` mod）

**Interfaces:**
- Consumes: Task 1 写入的 `config.extensions["cortex"]["stable_system_count"]`。
- Produces: `pub struct SystemPromptSegments { pub stable: String, pub volatile: Option<String> }`（convert.rs）；`fn split_system_segments(parts: &[String], stable_count: Option<usize>) -> convert::SystemPromptSegments`（client.rs）。

- [ ] **Step 1: 写失败测试 —— system 段分组纯函数**

在 `src/llm/anthropic_custom/client.rs` 文件末尾新增测试模块：

```rust
#[cfg(test)]
mod prompt_cache_tests {
    use super::*;

    #[test]
    fn split_with_stable_count_separates_segments() {
        let parts = vec!["STABLE".to_string(), "VOLATILE".to_string()];
        let seg = split_system_segments(&parts, Some(1));
        assert_eq!(seg.stable, "STABLE");
        assert_eq!(seg.volatile.as_deref(), Some("VOLATILE"));
    }

    #[test]
    fn split_missing_count_puts_all_in_stable() {
        // 向后兼容：count 缺失 → 全部归 stable，volatile=None（等价旧"整段"行为）
        let parts = vec!["A".to_string(), "B".to_string()];
        let seg = split_system_segments(&parts, None);
        assert_eq!(seg.stable, "A\nB");
        assert_eq!(seg.volatile, None);
    }

    #[test]
    fn split_count_exceeding_len_is_clamped() {
        let parts = vec!["A".to_string()];
        let seg = split_system_segments(&parts, Some(5));
        assert_eq!(seg.stable, "A");
        assert_eq!(seg.volatile, None);
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test --bin cortex-agent split_`
Expected: 编译失败 —— `split_system_segments` 未定义。

- [ ] **Step 3: 在 convert.rs 新增 `SystemPromptSegments` 类型**

在 `src/llm/anthropic_custom/convert.rs` 的 `build_message_params` 函数**上方**（:375 `/// Build MessageCreateParams` 注释前）插入：

```rust
/// system prompt 的 stable/volatile 分段（用于 Anthropic 分 block 打 cache_control）。
/// stable 段打 cache_control（命中缓存），volatile 段（如时间）不打（每次刷新）。
pub struct SystemPromptSegments {
    pub stable: String,
    pub volatile: Option<String>,
}
```

- [ ] **Step 4: 改 convert.rs `build_message_params` 签名与 system 处理**

签名（:382）：
```rust
    system_prompt: Option<String>,
```
改为：
```rust
    system_prompt: Option<SystemPromptSegments>,
```

system 处理（:401-408）：
```rust
    if let Some(sys) = system_prompt {
        if prompt_caching {
            let block = TextBlock::new(sys).with_cache_control(CacheControlEphemeral::new());
            params.system = Some(SystemPrompt::from_blocks(vec![block]));
        } else {
            params.system = Some(SystemPrompt::from_string(sys));
        }
    }
```
改为：
```rust
    if let Some(sys) = system_prompt {
        let mut blocks = vec![];
        // stable 段：prompt_caching 时打 cache_control（缓存命中到此 block 末尾）
        let stable_block = if prompt_caching {
            TextBlock::new(sys.stable).with_cache_control(CacheControlEphemeral::new())
        } else {
            TextBlock::new(sys.stable)
        };
        blocks.push(stable_block);
        // volatile 段（时间等）：不打 cache_control，每次刷新
        if let Some(v) = sys.volatile {
            blocks.push(TextBlock::new(v));
        }
        params.system = Some(SystemPrompt::from_blocks(blocks));
    }
```

- [ ] **Step 5: 在 client.rs 新增 `split_system_segments` 纯函数**

在 `src/llm/anthropic_custom/client.rs` 的 `build_message_params` 函数**下方**（:305 `Ok(convert::build_message_params(...))` 的 `}` 之后）插入：

```rust
/// 按 `stable_count` 把 system 文本段分成 stable / volatile。
///
/// stable 段（前 `stable_count` 条）打 cache_control 命中缓存；volatile 段（剩余，如时间）
/// 不打、每次刷新。`stable_count` 缺失 → 全部归 stable（volatile=None），向后兼容旧行为。
fn split_system_segments(
    parts: &[String],
    stable_count: Option<usize>,
) -> convert::SystemPromptSegments {
    let n = stable_count.unwrap_or(parts.len()).min(parts.len());
    let stable = parts[..n].join("\n");
    let volatile = if n < parts.len() {
        let v = parts[n..].join("\n");
        if v.is_empty() { None } else { Some(v) }
    } else {
        None
    };
    convert::SystemPromptSegments { stable, volatile }
}
```

- [ ] **Step 6: 改 client.rs `build_message_params` 读取边界并构造 segments**

替换 system_prompt 构造段（:190-196）：
```rust
        // Requirement 1.3: Concatenate multiple system entries with newline separators
        // Requirement 1.4: Omit system parameter when no system content found
        let system_prompt = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n"))
        };
```
改为：
```rust
        // 读 stable 边界：extensions["cortex"]["stable_system_count"]（由 CortexAgent 写入）。
        // 缺失时 split_system_segments 把全部 system 归 stable，向后兼容。
        let stable_count = request
            .config
            .as_ref()
            .and_then(|c| c.extensions.get("cortex"))
            .and_then(|v| v.get("stable_system_count"))
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let system_prompt = if system_parts.is_empty() {
            None
        } else {
            Some(split_system_segments(&system_parts, stable_count))
        };
```

- [ ] **Step 7: 运行测试，确认通过**

Run: `cargo test --bin cortex-agent split_`
Expected: 3 个 `split_*` 测试 PASS。

- [ ] **Step 8: 编译并运行全部测试**

Run: `cargo test --bin cortex-agent`
Expected: 编译通过（convert 签名变更已由 client 适配），所有测试 PASS。

- [ ] **Step 9: 提交**

```bash
git add src/llm/anthropic_custom/convert.rs src/llm/anthropic_custom/client.rs
git commit -m "feat(llm): Anthropic system 按 stable 边界分 block 打 cache_control

convert 新增 SystemPromptSegments(stable+volatile), build_message_params 把 system
拆成两个 TextBlock, stable 打 cache_control 命中缓存、volatile(时间)不打。
client 读 extensions stable_system_count 分组, 缺失时全归 stable 向后兼容。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: 验证、clippy、集成确认

**Files:**
- 验证（不改代码）：`src/llm/open_ai_custom_llm.rs`（多条 system 顺序由 `content_to_message` :399-480 逐条映射保证，已确认无需改动）

**Interfaces:**
- 无新接口。确认 Task 1 + Task 2 协作：CortexAgent 写 `stable_system_count=1` → Anthropic client 读取并分 block → stable 命中缓存。

- [ ] **Step 1: 全量测试**

Run: `cargo test --bin cortex-agent`
Expected: 全部 PASS（含 `prompt_injection_tests`、`prompt_cache_tests`、既有 `repetition_tests` 及 domain 层测试）。

- [ ] **Step 2: clippy 无警告**

Run: `cargo clippy --bin cortex-agent`
Expected: 无 warning / error。若有 `clippy::too_many_arguments`（`build_message_params` 本就 `#[allow]`），保持既有 allow，不新增警告。

- [ ] **Step 3: 确认 OpenAI 端多条 system 顺序（代码核对）**

核对 `src/llm/open_ai_custom_llm.rs::content_to_message`（:399-480）：role="system" 逐条映射为 `ChatCompletionRequestSystemMessageArgs`，`build_request_json`（:515-606）按 `contents` 顺序输出 → `[stable_system, volatile_system, ...history]` 顺序保留。结论：**无需改动**，靠厂商对 messages 数组前缀的自动缓存命中 stable。

- [ ] **Step 4: 端到端编译确认（release）**

Run: `cargo build --bin cortex-agent`
Expected: 编译成功。

- [ ] **Step 5: 提交（如有 clippy 微调）**

若 Step 2/3 触发任何小修：
```bash
git add -A
git commit -m "chore(prompt): clippy/验证微调

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```
若无改动则跳过本步。

---

## 完成判据

- `build_stable_prefix` 跨调用字节一致且不含时间；`build_volatile_context` 仅含时间。
- CortexAgent preamble 为 `[stable_system, volatile_system]`，写入 `stable_system_count=1`。
- Anthropic system 拆成两个 TextBlock，stable 打 `cache_control`、volatile 不打；`stable_system_count` 缺失时退化为全 stable（兼容）。
- compaction `preamble_len=2`，stable+volatile 均受保护。
- `cargo test --bin cortex-agent` 全绿，`cargo clippy --bin cortex-agent` 无警告。
