# 向 adk-rust 提交 PR 指南

> **状态（2026-08-19 核实）**：原计划两处修改中，**修改点 2（空白文本保留）已上游合并**（adk-rust commit `cacbd185`「fix(model): preserve whitespace-only OpenAI-compatible stream deltas」，PR #444/#446），当前 git 依赖 rev `9a0fa46c` 已包含——**本指南现仅剩修改点 1（MIN_PREFIX_LEN）待提交**，以下提交/PR 模板已相应收窄。修改点 2 小节保留作历史记录。

## 仓库信息

- **GitHub**: https://github.com/zavora-ai/adk-rust
- **crates.io**: `adk-rust`（本项目 2026-08-13 起不再用 crates.io 版本，改用 git 依赖 rev `9a0fa46c71231a01af1a7c071fbb679b90b7aa27`，另有 `[patch]` 将 adk-sandbox 指向本地 `vendor/adk-sandbox`）
- **开发指南**: https://www.adk-rust.com/en/docs/development/development-guidelines

## 一、Fork & Clone

```bash
# 1. 在 GitHub 上 Fork https://github.com/zavora-ai/adk-rust
# 2. Clone 你的 fork
git clone https://github.com/<你的用户名>/adk-rust.git
cd adk-rust

# 3. 添加上游远程
git remote add upstream https://github.com/zavora-ai/adk-rust.git
git fetch upstream
```

## 二、创建分支

```bash
git checkout -b fix/tool-call-buffer-partial-prefix
```

## 三、修改的文件

只需修改 **1 个文件**：

```
adk-model/src/tool_call_parser.rs
```

### 修改点 1：`has_partial_prefix()` — 提升最小匹配长度

```rust
// 找到 has_partial_prefix 方法（约 L470-L490）

// 修改前：
fn has_partial_prefix(&self) -> bool {
    let buf = &self.buffer;
    for prefix in TOOL_CALL_PREFIXES {
        let prefix_chars: Vec<char> = prefix.chars().collect();
        for i in 1..prefix_chars.len() {                    // ← 改这里
            let partial: String = prefix_chars[..i].iter().collect();
            if buf.ends_with(&partial) {
                return true;
            }
        }
    }
    false
}

// 修改后：
/// 最小前缀匹配长度。
/// 值为 3 时，只有 `<to` / `[TO` / `<|t` 等才触发缓冲，
/// 避免普通文本中的单字符 `<` / `[` 误触发缓冲导致流式内容延迟。
const MIN_PREFIX_LEN: usize = 3;

fn has_partial_prefix(&self) -> bool {
    let buf = &self.buffer;
    for prefix in TOOL_CALL_PREFIXES {
        let prefix_chars: Vec<char> = prefix.chars().collect();
        for i in MIN_PREFIX_LEN..prefix_chars.len() {       // ← 从 MIN_PREFIX_LEN 开始
            let partial: String = prefix_chars[..i].iter().collect();
            if buf.ends_with(&partial) {
                return true;
            }
        }
    }
    false
}
```

### 修改点 2：`flush_as_emit()` 和 `flush()` — 保留空白文本 ✅ 已上游合并

```rust
// 找到 flush_as_emit 方法（约 L505-L512）

// 修改前：
if text.trim().is_empty() {

// 修改后：
if text.is_empty() {
```

> **✅ 已合并**：上游 commit `cacbd185`（PR #444/#446）已实现等价修复，当前 pinned rev `9a0fa46c` 中 `flush_as_emit` 即为 `text.is_empty()`（附带注释说明历史行为）。**无需再提交**，本节仅作记录。

## 四、验证

```bash
# 编译
cargo build -p adk-model

# 测试
cargo test -p adk-model

# Clippy（CI 要求零 warning）
cargo clippy -p adk-model --all-targets --all-features

# 格式化（CI 要求）
cargo fmt -p adk-model
```

## 五、提交

```bash
git add adk-model/src/tool_call_parser.rs
git commit -m "fix: raise ToolCallBuffer min prefix length to 3

The has_partial_prefix() method started matching from i=1, causing
single characters like '<' or '[' in normal text to trigger buffering.
This delayed streaming output and could appear as truncation.

Raised the minimum match length to 3 (MIN_PREFIX_LEN), so only
meaningful prefixes like '<to', '[TO', '<|t' trigger buffering.
All 6 supported tool call formats remain correctly detected."
```

## 六、推送 & 创建 PR

```bash
git push origin fix/tool-call-buffer-partial-prefix
```

然后在 GitHub 上创建 Pull Request：
- **Base repository**: `zavora-ai/adk-rust`
- **Base branch**: `main`（或对应的开发分支）
- **Title**: `fix: raise ToolCallBuffer min prefix length to avoid false buffering`
- **Description** 模板：

```markdown
## Problem

`ToolCallBuffer::has_partial_prefix()` matches prefix fragments starting from length 1.
This means a single `<` or `[` in normal streaming text triggers buffering mode,
delaying output until the next chunk resolves the ambiguity.

In practice, this causes:
- Streaming content containing `<` or `[` to appear delayed or truncated
- Network device commands (e.g., `enable<cr>configure terminal`) to lose formatting
- Poor UX where text "hangs" on angle brackets

## Solution

1. **`has_partial_prefix()`**: Changed minimum match length from `1` to `3` (`MIN_PREFIX_LEN`).
   - All 6 tool call prefixes are still detected (shortest meaningful prefix is 3 chars: `<to`, `[TO`, `<|t`)
   - Normal text with `<`, `[`, `<t`, `[T` no longer triggers false buffering

## Testing

- [x] `cargo test -p adk-model` passes
- [x] `cargo clippy -p adk-model --all-targets --all-features` — zero warnings
- [x] Manually verified streaming output with text containing `<` and `[` characters
- [x] Verified tool call detection still works for all 6 formats

## Impact

- **Breaking**: No — tool call detection behavior is unchanged
- **Performance**: Negligible — fewer false buffer entries means less unnecessary buffering
- **Compatibility**: All existing tool call formats remain fully supported
```

## 七、注意事项

1. **先开 Issue 再开 PR**：建议先在 https://github.com/zavora-ai/adk-rust/issues 开一个 Issue 描述问题，
   附上复现步骤，维护者确认后再提 PR，成功率更高。

2. **保持分支聚焦**：只改 `tool_call_parser.rs` 这一个文件，不要夹带其他改动。

3. **附测试用例**（加分项）：如果能在 `adk-model/tests/` 下加一个测试，
   验证包含 `<` 和 `[` 的文本不被误缓冲，PR 更容易被接受。

4. **CI 检查项**：
   - `cargo fmt --all` — 格式必须通过
   - `cargo clippy --all-targets --all-features` — 零 warning
   - `cargo test --all` — 所有测试通过

5. **Commit message 风格**：参考仓库已有的 commit 历史，使用 conventional commits 格式。
