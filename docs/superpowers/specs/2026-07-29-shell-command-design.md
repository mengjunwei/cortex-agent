# shell_command 工具 — 统一命令执行能力（codex 格式）

## 背景与问题

codex-style skill 系统已上线，但 monitor-report skill 无法生成报表。

根因：monitor-report 的工作流要求 LLM 执行 `python scripts/run_report.py`，但 custom agent
没有注册任何命令执行工具。LLM 读到 SKILL.md 后试图调用 shell，因工具不存在只能以文本
"假装执行"。

现有 `run_command` 工具（`src/tools/code/run_command.rs`）存在两个问题：
1. 只有 code_assistant 能用，custom agent 无法注册
2. 审批机制是确认令牌（LLM 自动回传绕过），不是真正的用户审批

**决策：删掉 `run_command`，统一为 codex 格式的 `shell_command`，code_assistant 和
custom agent 共用同一工具 + 同一三层审批流程。**

## 设计决策（用户确认）

1. **工作目录**：per-session 沙箱（`{data_dir}/workspaces/sessions/{session_id}/`）
2. **安全策略**：codex 式三层审批（safelist 自动放行 + dangerous 阻断 + 其余需用户审批）
3. **完全统一**：删除 `run_command.rs`，code_assistant 和 custom agent 共用 `shell_command`

## 架构

```
用户消息 → handle_run_sse
               │
    ┌──────────┴──────────┐
    │ 创建 tx/rx channel   │  ← 提前到 agent build 之前
    │ 创建 session 沙箱    │
    │ 构建 ShellToolDeps   │  ← sandbox_dir + registry + tx
    │   (code_assistant    │
    │    和 custom 共用)    │
    └──────────┬──────────┘
               │
        create_event_stream(rx)
               │
        agent runner 调用 shell_command
               │
    ┌──────────┴──────────┐
    │ 三层安全判定         │
    │ Allow → 直接执行     │
    │ Dangerous → 阻断     │
    │ Prompt → SSE 审批    │
    │   await oneshot      │
    │   (120s 超时)        │
    └──────────┬──────────┘
               │
     前端收到 SHELL_APPROVAL_REQUEST
     → 弹窗 → 用户点击
     → POST /api/shell-approve/{id}
     → registry 唤醒 oneshot
     → 命令执行 / 拒绝
     → 结果返回 LLM
```

## 组件清单

### 新增文件

| 文件 | 职责 |
|------|------|
| `src/tools/shell_command.rs` | shell_command FunctionTool + 执行逻辑 |
| `src/tools/shell_safety.rs` | 命令安全分类（safelist / dangerous / needs_prompt） |
| `src/server/shell_approval.rs` | ShellApprovalRegistry + HTTP 审批端点 |

### 删除文件

| 文件 | 原因 |
|------|------|
| `src/tools/code/run_command.rs` | 功能合并到 `shell_command.rs` |

### 修改文件

| 文件 | 改动 |
|------|------|
| `src/tools/mod.rs` | 加 `pub mod shell_command; pub mod shell_safety;` |
| `src/tools/code/mod.rs` | 删 `pub mod run_command;` + 删 re-export |
| `src/tools/registry.rs` | 加 `shell_command` ToolDescriptor（custom_enabled=true） |
| `src/agent/custom.rs` | `push_tool_for_key` 加 `"shell_command"` 分支；`build_custom_builder`/`build_builtin` 接收 shell_deps |
| `src/agent/code_assistant.rs` | 删 `create_run_command_tool` import → 改用 `create_shell_command_tool`；`build_code_assistant_agent` 接收 shell_deps |
| `src/server/sse.rs` | `handle_run_sse`：tx channel 提前 + 构建沙箱 + 传 shell_deps；`create_event_stream` 接收已有 rx |
| `src/server/mod.rs` | 注册 `/api/shell-approve` 路由 |
| `src/server/sse.rs` SseEventMsg | 加 `SHELL_APPROVAL_REQUEST` 变体 |
| `src/bootstrap.rs` | AppState 加 `shell_approval_registry` 字段 |
| `src/config/mod.rs` | `[workspace]` 段：删 `enable_run_command`/`run_command_max_timeout_secs`，改为 `[shell]` 段 |
| `frontend/src/api/index.js` | 加 `approveShellCommand` 函数 |
| `frontend/src/components/` | 加 ShellApprovalDialog 组件 |
| `frontend/src/views/ChatPage.vue` | SSE 事件处理 shell 审批 |

## 详细设计

### 1. shell_command 工具（`src/tools/shell_command.rs`）

#### LLM 参数

```rust
struct ShellCommandParams {
    command: String,           // 必填，shell 命令
    timeout_ms: Option<u64>,   // 可选，默认 30000，上限由配置决定
}
```

不暴露 `workdir` — cwd 固定为 session 沙箱，防越权。

#### 返回值

**成功执行：**
```json
{"ok": true, "exit_code": 0, "stdout": "...", "stderr": "...", "duration_ms": 1250}
```

**需要审批（当轮返回给 LLM）：**
```json
{"ok": false, "needs_approval": true, "approval_id": "01J...", "command": "pip install pandas"}
```

**审批被拒绝：**
```json
{"ok": false, "denied": true, "command": "pip install pandas", "reason": "用户拒绝了此命令"}
```

**审批超时：**
```json
{"ok": false, "timeout": true, "command": "...", "reason": "审批超时（120秒无响应）"}
```

**危险命令：**
```json
{"ok": false, "blocked": true, "command": "rm -rf /", "reason": "危险命令被阻止"}
```

#### 工具创建函数签名

```rust
pub fn create_shell_command_tool(
    deps: Arc<ShellToolDeps>,
) -> FunctionTool
```

`ShellToolDeps` 封装所有运行时依赖：

```rust
pub struct ShellToolDeps {
    /// session 沙箱目录（命令的 cwd）
    pub sandbox_dir: Arc<PathBuf>,
    /// 超时上限（毫秒），来自配置
    pub max_timeout_ms: u64,
    /// 审批注册表（全局共享，存在 AppState）
    pub approval_registry: Arc<ShellApprovalRegistry>,
    /// SSE 事件发送端（session 级，每次对话新建）
    pub sse_tx: tokio::sync::mpsc::Sender<SseEvent>,
}
```

#### 执行流程

```text
1. 解析参数，校验 command 非空
2. 安全判定：shell_safety::classify(command)
   ├─ Safety::Allowed → 执行（步骤 4）
   ├─ Safety::Dangerous → 返回 {blocked: true}
   └─ Safety::NeedsPrompt → 步骤 3
3. 审批流程：
   a. 生成 approval_id（UUIDv7）
   b. 创建 oneshot channel
   c. registry.register(approval_id, oneshot_tx)
   d. sse_tx.send(SHELL_APPROVAL_REQUEST { approval_id, command })
   e. tokio::time::timeout(120s, oneshot_rx).await
      ├─ Ok(Approved) → 执行（步骤 4）
      ├─ Ok(Rejected) → 返回 {denied: true}
      └─ Err(Timeout) → 返回 {timeout: true}
4. 执行命令（从 run_command.rs 迁移的底层逻辑）
```

### 2. 底层命令执行（从 run_command.rs 迁移）

将 `run_command.rs` 的执行逻辑迁移到 `shell_command.rs` 内部函数 `execute_command()`：

- Unix：`sh -c "<command>"`，Windows：`cmd /C "<command>"`
- cwd = sandbox_dir
- `env_clear()` + 白名单（PATH/HOME/USERPROFILE/TEMP/TMP/LANG/LC_ALL/SystemRoot/WINDIR/APPDATA/LOCALAPPDATA）
- stdin=null，stdout/stderr=piped
- `kill_on_drop(true)`
- 超时 kill 子进程
- stdout/stderr 各截断到 20000 字符

**迁移的函数：**
- `is_dangerous_command()` → 移到 `shell_safety.rs`（或保持 pub 引用）
- `confirmation_token()` / `constant_time_eq()` → **删除**（不再需要令牌机制）
- `parse_diagnostics()` → **删除**（code_assistant 不再单独解析诊断，LLM 直接读 stdout/stderr）
- `truncate_str()` → 移到 `shell_command.rs`

### 3. 安全分类（`src/tools/shell_safety.rs`）

```rust
pub enum Safety {
    Allowed,       // safelist 命中，自动放行
    Dangerous,     // dangerous 命中，自动阻断
    NeedsPrompt,   // 其余，需用户审批
}

pub fn classify(command: &str) -> Safety
```

#### safelist（自动放行）

只读 / 无副作用命令。取第一个 token 判断命令名。

**跨平台通用：**
`ls, cat, grep, head, tail, wc, echo, pwd, cd, find, stat, which, whoami, env, date`

**开发工具（只读子命令）：**
`git status, git log, git diff, git show, git branch, git remote`

**脚本执行（skill 场景必需）：**
`python, python3, node, npm run, npx, cargo check, cargo test, cargo build, cargo clippy`

> `python`/`node` 放行是因为 monitor-report 等 skill 的核心工作流就是执行 Python 脚本。

#### dangerous（自动阻断）

从 `run_command.rs::is_dangerous_command` 迁移：
`rm -rf /`、`mkfs`、`dd if=`、`sudo`、`shutdown`、`reboot`、`curl`/`wget`（外泄）、
`fork bomb`、`chmod -R 777 /` 等。

#### NeedsPrompt（需用户审批）

不在上述两类的命令。典型例子：
- `pip install pandas`（安装软件包）
- `npm install express`（安装依赖）
- 任何不确定的命令

#### 复合命令处理

命令含 `&&` / `||` / `;` / `|` 时，拆分为子命令逐个判定，取**最严格**结果：
- `ls && rm -rf /` → Dangerous（第二个子命令命中）
- `echo hi && pip install x` → NeedsPrompt（第二个子命令不在 safelist）
- `ls && cat file` → Allowed（两个子命令都在 safelist）

拆分后对每个子命令调用 `classify_single()`，合并规则：
`Dangerous > NeedsPrompt > Allowed`

### 4. 审批注册表（`src/server/shell_approval.rs`）

```rust
pub struct ShellApprovalRegistry {
    pending: tokio::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<ApprovalDecision>>
    >,
}

pub enum ApprovalDecision {
    Approved,
    Rejected,
}

impl ShellApprovalRegistry {
    pub fn new() -> Self;

    /// 注册一个待审批项，返回 receiver
    pub async fn register(
        &self,
        approval_id: &str,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision>;

    /// 用户审批结果回填（由 HTTP 端点调用）
    pub async fn resolve(
        &self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> bool;
}
```

存在 `AppState`，全局单例。

### 5. SSE 事件变体

```rust
// SseEventMsg 新增：
#[serde(rename = "SHELL_APPROVAL_REQUEST")]
ShellApprovalRequest {
    approval_id: String,
    command: String,
    session_id: String,
},
```

前端收到后弹出审批弹窗。

### 6. HTTP 审批端点

```
POST /api/shell-approve
Body: { "approval_id": "01J...", "decision": "approved" | "rejected" }
```

从 AppState 取 `shell_approval_registry`，调用 `resolve()`。

### 7. Config 改动

**删除** `[workspace]` 段的：
```toml
enable_run_command = true           # ← 删
run_command_max_timeout_secs = 120  # ← 删
```

**新增** `[shell]` 段：
```toml
[shell]
# 命令执行默认超时（毫秒）
default_timeout_ms = 30000
# 命令执行超时上限（毫秒）
max_timeout_ms = 120000
# 审批等待超时（秒，用户不响应时自动拒绝）
approval_timeout_secs = 120
```

`WorkspaceConfig` 保留 `enable_session_sandbox`（沙箱目录创建开关），
删除 `enable_run_command` / `run_command_max_timeout_secs`。
新增 `ShellConfig` 结构体。

### 8. code_assistant 统一改造（`src/agent/code_assistant.rs`）

```rust
// 之前（删除）：
// builder.tool(Arc::new(create_run_command_tool(root, max_timeout)));

// 之后：
if let Some(deps) = shell_deps.as_ref() {
    builder = builder.tool(Arc::new(
        crate::tools::shell_command::create_shell_command_tool(deps.clone())
    ));
}
```

`build_code_assistant_agent` 新增参数 `shell_deps: Option<Arc<ShellToolDeps>>`。
当 deps 存在时注册 shell_command（替代原来的 run_command）。

### 9. custom agent 工具注册（`src/agent/custom.rs`）

`push_tool_for_key` 新增分支：

```rust
"shell_command" => {
    if let Some(deps) = shell_deps.as_ref() {
        builder.tool(Arc::new(
            crate::tools::shell_command::create_shell_command_tool(deps.clone())
        ))
    } else {
        builder
    }
}
```

`build_custom_builder` 新增参数 `shell_deps: Option<Arc<ShellToolDeps>>`。
`build_builtin` 新增参数 `shell_deps`，透传给 `build_code_assistant_agent`。

### 10. sse.rs 改动

#### handle_run_sse 流程调整

当前流程：
```
build agent → create_event_stream(内部创建 tx/rx)
```

新流程：
```
1. 提前创建 tx/rx channel
2. 创建 session 沙箱目录（code_assistant 已有，custom agent 新增）
3. 构建 ShellToolDeps（sandbox_dir + registry + tx.clone()）
4. build_agent_for_session（传入 shell_deps）
5. create_event_stream 改为接收已有 rx（不再内部创建 channel）
```

session 沙箱目录创建逻辑扩展：
- code_assistant：已有（`agent_type == "code_assistant" && enable_session_sandbox`）
- custom agent：新增（`enabled_tools 含 "shell_command"`）
- 两者共用 `{data_dir}/workspaces/sessions/{session_id}/`

### 11. 前端改动

#### ShellApprovalDialog 组件

弹出条件：收到 `SHELL_APPROVAL_REQUEST` SSE 事件。

弹窗内容：
- 命令文本（等宽字体，可复制）
- "允许执行" / "拒绝" 按钮
- 倒计时（120s 超时提示）

点击后调用 `POST /api/shell-approve`。

#### ChatPage.vue SSE 处理

在 SSE 事件 switch 中加 `SHELL_APPROVAL_REQUEST` case。

### 12. 删除清单

以下代码随 `run_command.rs` 一并删除：
- `src/tools/code/run_command.rs` 整个文件
- `src/tools/code/mod.rs` 的 `pub mod run_command;` 和 re-export
- `run_command.rs` 的 5 个测试（迁移等价测试到 `shell_command.rs`）
- `confirmation_token()` / `constant_time_eq()` — 令牌机制不再需要
- `parse_diagnostics()` — 诊断解析不再需要（LLM 直接读输出）
- `RunCommandParams` / `create_run_command_tool` — 被 `shell_command` 替代
- config 的 `enable_run_command` / `run_command_max_timeout_secs` 字段

## 迁移注意

### code_assistant 行为变化

| 维度 | 旧（run_command） | 新（shell_command） |
|------|-------------------|---------------------|
| 审批 | 令牌（LLM 自动回传） | 用户弹窗（codex 式） |
| 安全分级 | dangerous 二选一 | safelist/dangerous/prompt 三层 |
| 超时 | 秒（默认 30s） | 毫秒（默认 30000ms） |
| 参数 | `{command, timeout_secs, confirm_token}` | `{command, timeout_ms}` |
| 诊断解析 | 有（cargo/tsc/eslint/pytest） | 无（LLM 直接读输出） |

> 诊断解析功能移除后，code_assistant 的编译错误检查依赖 LLM 自身能力读 stdout/stderr
> 判断。这是可接受的简化 — codex 本身也不做诊断解析。

### 测试迁移

`run_command.rs` 的 5 个测试迁移到 `shell_command.rs`（适配新参数）：
- `runs_echo_successfully` → 保留
- `reports_nonzero_exit` → 保留
- `requires_confirmation_for_dangerous_command` → 改为 `dangerous_command_blocked`
- `rejects_empty_command` → 保留
- `times_out_long_running_command` → 保留（timeout_ms 替代 timeout_secs）

`is_dangerous_command` 的 8 个测试迁移到 `shell_safety.rs`。

## 测试策略

### 单元测试

1. **shell_safety.rs**：
   - safelist 命令分类正确（`ls` → Allowed，`python x.py` → Allowed）
   - dangerous 命令分类正确（`rm -rf /` → Dangerous）
   - 其余命令分类正确（`pip install x` → NeedsPrompt）
   - 复合命令（`ls && rm -rf /`）→ 取最严格判定

2. **shell_command.rs**：
   - Allowed 命令直接执行（`echo hello` → stdout 含 hello）
   - Dangerous 命令被阻断（返回 blocked=true）
   - 超时正确触发（`sleep 10` + timeout_ms=1000 → timeout=true）
   - stdout/stderr 截断生效
   - env 隔离（`env` 输出不含敏感变量）

3. **shell_approval.rs**：
   - register + resolve 正确唤醒
   - resolve 不存在的 approval_id 返回 false
   - 超时后 receiver 返回 Err

### 集成测试

4. shell_command 工具注册到 custom agent（enabled_tools 含 shell_command）
5. shell_command 工具注册到 code_assistant（沙箱模式）
6. session 沙箱目录创建

### 浏览器回归测试

7. monitor-report skill 端到端：
   - `$monitor-report` 触发后 LLM 调用 shell_command
   - safelist 命令（python 执行脚本）自动放行
   - 非 safelist 命令弹出审批弹窗
   - 审批后命令执行结果返回 LLM

## 不做的事情

- 不做 OS 级沙箱（seatbelt/landlock/bwrap）— 安全靠 safelist + 审批 + dangerous 拦截
- 不做 PTY 交互式 shell（codex 的 unified exec）— 第一版用 pipe 模式足够
- 不做命令缓存（codex 的 with_cached_approval）— 简化首版
- 不做 execpolicy Starlark 规则引擎 — safelist + dangerous 足够

## 参考实现

| 关注点 | codex 位置 | cortex 对应 |
|--------|-----------|-------------|
| 工具 JSON schema | `core/src/tools/handlers/shell_spec.rs:154` | `src/tools/shell_command.rs` |
| shell argv 构造 | `core/src/shell.rs:22` | shell_command.rs 内部 |
| 进程 spawn | `core/src/spawn.rs:51` | shell_command.rs 内部（简化版） |
| safelist | `shell-command/src/command_safety/is_safe_command.rs:12` | `src/tools/shell_safety.rs` |
| dangerous 检测 | `shell-command/src/command_safety/is_dangerous_command.rs:7` | 从 `run_command.rs` 迁移 |
| 审批判定 | `core/src/exec_policy.rs:269` | `src/server/shell_approval.rs` |
| 输出截断 | `core/src/exec.rs:1100` | shell_command.rs 内部 |
