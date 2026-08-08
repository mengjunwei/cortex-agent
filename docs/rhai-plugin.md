# Rhai 监控插件系统

## 概述

cortex-agent 内置一套基于 [Rhai](https://rhai.rs/) 脚本引擎的监控插件运行时，用于让 LLM 在对话中"即写即跑"地生成网络设备监控指标采集逻辑。

与传统的 `.so` 动态库方案相比：

| 维度 | `.so` / `libloading` | **Rhai（本项目）** |
|------|----------------------|--------------------|
| 部署 | 每个插件需 rustc 编译成动态库 | 仅一段字符串脚本 |
| 跨平台 | 受 ABI / 平台限制 | 纯 Rust 解释执行，跨平台一致 |
| 隔离 | 进程内 FFI，崩溃会带崩主进程 | AST 沙箱 + 子进程双层隔离 |
| 加载延迟 | 秒级（编译 + dlopen） | 毫秒级（compile AST） |
| 安全性 | 任意 Rust 代码 | 受限 host function + 操作上限 |

Rhai 是一个纯 Rust 实现的嵌入式脚本语言，编译进二进制后**无任何外部依赖**，天生适合"单二进制 + 可热更新脚本"的部署形态。

> 数据契约完全对齐 nm 项目的 `nm-plugin-api`，生成的脚本未来可无缝迁移到 nm 后端执行。

---

## 一、架构总览

```text
GraphQL（POST /api/graphql）──►  PluginManager（RwLock<HashMap>）
                              │
                              ▼
                       RhaiMonitorPlugin
                       (Engine + AST)
                              │
            ┌─────────────────┼─────────────────┐
            ▼                 ▼                 ▼
     prepare_oids()      parse(json)       check_syntax()
       返回 OID 列表      返回监控结果      仅做语法校验

  ── 同时被 validate_monitor_plugin FunctionTool 调用 ──

            L1 进程内编译        ──► 毫秒级语法检查
            L2 adk-sandbox      ──► spawn rhai-runner 子进程
            L3 adk-code         ──► RustExecutor 完整编译管线（可选）
```

### 核心组件

| 组件 | 文件 | 说明 |
|------|------|------|
| `PluginManager` | [manager.rs](file:///d:/code/rust/cortex-agent/src/monitor/manager.rs) | 按 `plugin_id` 索引插件，`RwLock<HashMap>` 多读单写 |
| `RhaiMonitorPlugin` | [rhai_plugin.rs](file:///d:/code/rust/cortex-agent/src/monitor/rhai_plugin.rs) | 单个插件实例（Engine + AST） |
| `host_fns` | [host_fns.rs](file:///d:/code/rust/cortex-agent/src/monitor/host_fns.rs) | 进程内 Rhai Engine 注册的 host function |
| `rhai-runner` 二进制 | [rhai_runner.rs](file:///d:/code/rust/cortex-agent/src/bin/rhai_runner.rs) | 独立子进程，供 adk-sandbox 隔离调用 |
| `SandboxVerifier` | [sandbox.rs](file:///d:/code/rust/cortex-agent/src/infra/sandbox.rs) | L2 验证层：封装 adk-sandbox |
| `CodeVerifier` | [code_exec.rs](file:///d:/code/rust/cortex-agent/src/infra/code_exec.rs) | L3 验证层：封装 adk-code |
| `validate_monitor_plugin` | [monitor_plugin_validate.rs](file:///d:/code/rust/cortex-agent/src/tools/monitor_plugin/validate.rs) | LLM 自检工具，三层串联 |
| `PluginStore` | [plugin_store.rs](file:///d:/code/rust/cortex-agent/src/monitor/plugin_store.rs) | 插件 + 版本历史的 PostgreSQL 持久化（启动时恢复） |
| HTTP API | [server/monitor.rs](file:///d:/code/rust/cortex-agent/src/server/monitor.rs) + [server/mod.rs](file:///d:/code/rust/cortex-agent/src/server/mod.rs) | 监控插件的注册 / 回滚 / OID 准备 / 采集值解析统一经 GraphQL（`POST /api/graphql`）暴露；`monitor_get_oids` / `monitor_calculate` 等业务函数定义在 `server/mod.rs`，由 GraphQL resolver 复用 |

### 安全限制

每个 Rhai Engine 都会通过 [`apply_safety_limits`](file:///d:/code/rust/cortex-agent/src/monitor/rhai_plugin.rs#L128-L135) 设置操作上限，防止 LLM 生成的恶意/错误脚本拖垮进程：

| 限制项 | 值 | 含义 |
|--------|------|------|
| `max_expr_depths` | 64 / 64 | 表达式 / 函数体最大嵌套深度 |
| `max_call_levels` | 50 | 最大调用栈深度 |
| `max_operations` | 1,000 | 单次执行最大操作数（CPU 上限） |
| `max_string_size` | 1,000,000 | 单个字符串最大字节数 |
| `max_array_size` | 10,000 | 单个数组最大长度 |
| `max_map_size` | 10,000 | 单个 map 最大键数 |

---

## 二、三层验证架构

LLM 生成 Rhai 脚本后，必须通过 `validate_monitor_plugin` 工具自检。该工具按以下三层串联执行：

### Layer 1：进程内语法检查（毫秒级）

调用 [`RhaiMonitorPlugin::check_syntax`](file:///d:/code/rust/cortex-agent/src/monitor/rhai_plugin.rs#L115-L123)，直接 `engine.compile(source)`，只编译不执行。

- **作用**：拦截所有语法错误、未定义函数、未注册的 host function 调用。
- **耗时**：通常 < 5ms。
- **失败行为**：直接终止该测试用例，不进入 L2。

### Layer 2：adk-sandbox 隔离子进程执行（100-500ms）

通过 [`SandboxVerifier`](file:///d:/code/rust/cortex-agent/src/infra/sandbox.rs#L65-L85) spawn `rhai-runner` 子进程：

1. 父进程把 `{script, action, oid_values_json}` 序列化为 JSON 通过 stdin 传入；
2. 子进程注册 host function → 编译 AST → 调用顶层函数 → 把结果通过 stdout 最后一行返回；
3. adk-sandbox 的 `ProcessBackend` 强制 10 秒超时，子进程崩溃 / 死循环 / 爆内存都不会影响主进程。

**为什么需要 L2？** L1 只能发现语法错误，无法发现运行时错误（例如 `unwrap()` 在 None 上 panic、数组越界、除零等）。L2 把这些运行时错误隔离到子进程，安全捕获。

### Layer 3：adk-code 完整 Rust 编译管线（5-15s，可选）

调用 [`CodeVerifier`](file:///d:/code/rust/cortex-agent/src/infra/code_exec.rs#L32-L86)：

1. 把 Rhai 脚本以 Base64 嵌入一段 Rust wrapper 程序源码；
2. 通过 adk-code 的 `RustExecutor` 走完整的 `check → build → execute` 管线（真正调用 rustc）；
3. 编译并运行后得到 JSON 结果。

**为什么需要 L3？** 这一路径演示了完整的代码执行管线，未来可以直接用于验证 Rust 监控插件（`.so` 路线）。当前对 Rhai 脚本是"重炮打蚊子"，所以**默认 mode=fast 跳过 L3**，仅在 `mode=full` 时启用。

### 串联逻辑

```
L1 通过？ ──no──► 用例失败（错误：L1 语法错误）
   │
  yes
   │
   ▼
L2 通过？ ──no──► 用例失败（错误：L2 沙箱执行失败）
   │
  yes
   │
   ▼
断言匹配？ ──no──► 用例失败（错误：断言不匹配）
   │
  yes
   │
   ▼
用例通过 ✅
```

综合判定公式：`passed = L1_ok && L2_ok && 断言匹配`（L3 仅作参考，不影响判定）。

---

## 三、Rhai 脚本契约

### 3.1 顶层函数（缺一不可）

每个监控插件必须定义这两个顶层函数：

```rhai
// 准备阶段：返回 OID 列表的 JSON 字符串（数组）
fn prepare_oids() {
    `[{"oid":"<OID>","method":"get|walk"}]`
}

// 解析阶段：接收 OID 值 JSON 字符串，返回解析结果 JSON 字符串（数组）
fn parse(oid_values_json) {
    // ... 你的逻辑 ...
    // 单值场景不加 label；多值场景（如多核 CPU）按需添加
    `[{"success":true,"value":{"number":<数值>}}]`
}
```

### 3.2 `prepare_oids` 返回值

OID 项结构（与 nm `OidItem` 对齐）：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `oid` | string | 是 | OID 标识符，如 `"1.3.6.1.2.1.1.3.0"` |
| `method` | `"get"` \| `"walk"` | 是 | `get`=snmpget 精确取值；`walk`=snmpwalk 遍历表 |
| `label` | string | 否 | 多值场景的维度标签，格式 `key=value`（如 `core=0`、`if=eth0`）；单值场景不加 |

### 3.3 `parse` 返回值

监控项结果数组（与 nm `MonitorResult` 对齐）：

```json
[
  {
    "success": true,
    "value": { "number": 1234.56 },
    "errors": []
  }
]
```

| 字段 | 类型 | 必填条件 | 说明 |
|------|------|---------|------|
| `success` | bool | 必填 | 本项解析是否成功 |
| `value` | `{number: f64}` \| `{string: string}` | `success=true` 时必填 | 解析出的监控值 |
| `label` | string | 可选（多值场景） | 维度标签，格式 `key=value`（如 `core=0`、`if=eth0`、`sensor=inlet`）。**单值场景不加**，多值场景需用户确认后添加，禁止用纯名称或中文 |
| `errors` | string[] | `success=false` 时必填 | 失败原因列表 |

### 3.4 可用 host function

以下函数在 Engine 启动时由 [`register_host_functions`](file:///d:/code/rust/cortex-agent/src/monitor/host_fns.rs#L51-L130) 注册，所有脚本共享：

| 函数 | 签名 | 说明 |
|------|------|------|
| `parse_json(s)` | `(字符串) -> map` | 把 JSON 字符串解析为 Rhai 对象，失败返回 `()` |
| `to_json(d)` | `(任意值) -> 字符串` | 把任意 Rhai 值序列化为 JSON 字符串 |
| `get_num(map, oid)` | `(map, 字符串) -> OptFloat` | 从 OID 值 map 取数字，找不到返回 `OptFloat::none()` |
| `get_num_str(map, oid)` | `(map, 字符串) -> OptStr` | 从 OID 值 map 取字符串 |
| `log_info(msg)` / `log_warn` / `log_error` | `(字符串) -> ()` | 日志输出，不影响脚本结果 |

**禁止使用未列出的函数。** Rhai 是沙箱语言，无文件 / 网络 / 进程 API。

### 3.5 OID 值 map 结构

`parse(oid_values_json)` 接收的字符串，`parse_json` 后形如：

```rhai
let m = parse_json(oid_values_json);
// m["1.3.6.1.2.1.1.3.0"] == {
//     oid_value_type: 2,        // 1=字符串, 2=数字
//     value_str: "",
//     value_num: 123456.0
// }
let n = get_num(m, "1.3.6.1.2.1.1.3.0");  // OptFloat
```

---

## 四、`OptFloat` / `OptStr` 可空包装类型

### 4.1 为什么需要包装类型

Rhai 有一个重要特性：**自动解包 Rust 的 `Option<T>`**。如果直接把 `Option<f64>` 返回给脚本，Rhai 会把它变成 `f64` 或 `()`，脚本端无法再调用 `is_none()` / `unwrap()`。

为了让 LLM 生成的脚本保持 Rust 风格（与 nm 的 Rust 插件 API 一致），本项目在 [host_fns.rs](file:///d:/code/rust/cortex-agent/src/monitor/host_fns.rs#L17-L39) 定义了两个包装类型：

```rust
pub struct OptFloat(pub Option<f64>);     // 可空数字
pub struct OptStr(pub Option<ImmutableString>); // 可空字符串
```

它们被 `register_type` 注册为 Rhai 自定义类型，并提供以下方法：

| 方法 | 签名 | 说明 |
|------|------|------|
| `is_none()` | `(&mut self) -> bool` | 是否为空 |
| `is_some()` | `(&mut self) -> bool` | 是否有值 |
| `unwrap()` | `(&mut self) -> T` | 取值，None 时 panic（会被 L2 捕获） |
| `unwrap_or(def)` | `(&mut self, T) -> T` | 取值，None 时返回默认值 |

### 4.2 脚本端使用范式

```rhai
let n = get_num(map, "1.3.6.1.2.1.1.3.0");
if n.is_none() {
    return `[{"success":false,"errors":["OID 缺失"]}]`;
}
let seconds = n.unwrap() / 100.0;
// 或者用 unwrap_or 给默认值：
// let seconds = n.unwrap_or(0.0) / 100.0;
```

> ⚠️ 在 Rhai 1.25 中，[`Engine::register_type::<T>()`](file:///d:/code/rust/cortex-agent/src/monitor/host_fns.rs#L53) **不接受 name 参数**，类型名直接使用 Rust 类型名（`OptFloat`/`OptStr`）。这是 1.x 系列一个易踩的坑。

---

## 五、完整示例：sysUpTime 监控插件

采集设备运行时长（`sysUpTime` OID，单位百分之一秒），转换为秒输出。

```rhai
fn prepare_oids() {
    `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]`
}

fn parse(oid_values_json) {
    let map = parse_json(oid_values_json);
    let raw = get_num(map, ".1.3.6.1.2.1.1.3.0");
    if raw.is_none() {
        return `[{"success":false,"errors":["sysUpTime OID 缺失"]}]`;
    }
    let seconds = raw.unwrap() / 100.0;
    `[{"success":true,"value":{"number":${seconds}}}]`
}
```

**对应测试用例**（提交给 `validate_monitor_plugin` 工具）：

```json
[
  {
    "name": "prepare_oids 形状",
    "action": "prepare_oids",
    "expected_contains": ".1.3.6.1.2.1.1.3.0"
  },
  {
    "name": "正常解析",
    "action": "parse",
    "oid_values_json": "{\".1.3.6.1.2.1.1.3.0\":{\"oid_value_type\":2,\"value_str\":\"\",\"value_num\":123456}}",
    "expect_success": true
  },
  {
    "name": "OID 缺失降级",
    "action": "parse",
    "oid_values_json": "{}",
    "expect_success": false
  }
]
```

---

## 六、HTTP API（GraphQL）

监控插件的注册 / 注销 / 查询 / 回滚 / OID 准备 / 采集值解析**统一通过 GraphQL 单入口** `POST /api/graphql` 暴露（详见 [API 文档](./api.md)）。所有返回值均为统一信封 `{ code, message, data }`（`code == 0` 成功）。

### 6.1 Mutation `registerMonitorPlugin`

注册（或覆盖）一个 Rhai 监控插件，注册成功进入版本历史，返回最终插件 id 与新版本号。

**input**:
```json
{
  "plugin_id": "sysuptime-h3c",
  "script": "fn prepare_oids() { ... } fn parse(j) { ... }",
  "description": "可选",
  "change_description": "可选，记入版本历史"
}
```

**data**（成功）:
```json
{ "plugin_id": "sysuptime-h3c", "version": 1 }
```

**失败**（Rhai 编译失败）：返回 `code` 非 0，`message` 形如 `rhai 编译失败 (plugin_id=sysuptime-h3c): Parse error: ...`，HTTP 层对应 `422`。

> 编译失败时，已存在的同名插件**保持原样不替换**。脚本上限 64 KB，`plugin_id` 仅允许 `[a-zA-Z0-9_-]`（1-64 字符）。

### 6.2 Mutation `unregisterMonitorPlugin`

注销插件。参数 `pluginId`。`data` 为 `null`；插件不存在时返回错误码 `NOT_FOUND`。

### 6.3 Query `monitorPlugins` / `monitorPlugin` / `monitorPluginVersions`

| Query | 参数 | 说明 |
|-------|------|------|
| `monitorPlugins` | — | 列出所有已注册插件（含版本信息） |
| `monitorPlugin` | `pluginId` | 插件详情（含源码） |
| `monitorPluginVersions` | `pluginId` | 版本历史 |

### 6.4 Mutation `rollbackMonitorPlugin`

回滚到指定版本。参数 `pluginId` + `version`。

### 6.5 Query `monitorOids`

调用指定插件的 `prepare_oids()`，返回 OID 列表（带进程内缓存，容量 10000）。参数 `pluginId`。

**data**:
```json
{
  "plugin_id": "sysuptime-h3c",
  "oids": [
    { "oid": ".1.3.6.1.2.1.1.3.0", "method": "get" }
  ]
}
```

### 6.6 Query `monitorCalculate`

调用指定插件的 `parse(json)`，传入实际采集到的 OID 值。参数 `pluginId` + `oidValues(JSON)`。

**oidValues**:
```json
{
  ".1.3.6.1.2.1.1.3.0": {
    "oid_value_type": 2,
    "value_str": "",
    "value_num": 123456
  }
}
```

**data**:
```json
{
  "plugin_id": "sysuptime-h3c",
  "results": [
    {
      "success": true,
      "value": { "number": 1234.56 }
    }
  ]
}
```

> GraphQL 示例：
> ```bash
> curl -X POST http://localhost:8090/api/graphql \
>   -H "Content-Type: application/json" \
>   -d '{"query":"{ monitorOids(pluginId:\"sysuptime-h3c\") }"}'
> ```

> 历史的 REST 路由（`/api/monitor/register`、`/api/monitor/unregister`、`/api/monitor/list`、`/api/monitor/plugins/*`、`/api/v1/monitor/oids/*`、`/api/v1/monitor/calculate`）**已全部移除**，仅保留 `/api/v1/monitor/health`（健康检查）。

---

## 七、`rhai-runner` 二进制

### 7.1 用途

`rhai-runner` 是本项目的附属二进制（[`[[bin]]`](file:///d:/code/rust/cortex-agent/Cargo.toml#L119-L121) 声明），编译产物 `rhai-runner.exe`（Windows）/ `rhai-runner`（Unix）。主进程通过 adk-sandbox 的 `ProcessBackend` 以 `Language::Command` 方式 spawn 它，在独立子进程内运行 LLM 生成的 Rhai 脚本。

### 7.2 stdin / stdout 协议

**stdin**（UTF-8 JSON）:
```json
{
  "script": "fn prepare_oids() { ... } fn parse(j) { ... }",
  "action": "prepare_oids",
  "oid_values_json": "..."
}
```

**stdout** 最后一行（UTF-8 JSON）:
```json
{ "result": "..." }
{ "error": "..." }
```

> 结果类型用 `#[serde(untagged)]` 的 enum，**按字段名区分**成功（`result`）/失败（`error`）变体，**无 `ok` 布尔字段**（见 `src/infra/sandbox.rs` 的 `RunnerResponse` / `src/bin/rhai_runner.rs`）。

### 7.3 编译

```bash
cargo build --bin rhai-runner
# 产物：target/debug/rhai-runner[.exe]
```

### 7.4 主进程定位逻辑

[`locate_runner`](file:///d:/code/rust/cortex-agent/src/infra/sandbox.rs#L176-L221) 按以下顺序查找 `rhai-runner` 可执行文件：

1. `CARGO_BIN_EXE_rhai_runner` 环境变量（cargo test 自动注入）
2. 当前 exe 同目录（生产部署：与主程序并列）
3. 当前 exe 上溯两级（`deps` → `target/<profile>`）
4. `CARGO_MANIFEST_DIR/target/{debug,release}`（开发环境兜底）

四步任一命中即返回；全部未命中返回错误，调用方降级为只跑 L1（不进入 L2）。

### 7.5 手动验证

```bash
echo '{"script":"fn prepare_oids() { `[1,2,3]` }","action":"prepare_oids"}' \
  | ./target/debug/rhai-runner
# => {"result":"[1,2,3]"}
```

---

## 八、adk-code + adk-sandbox 集成说明

本项目演示了 [`adk-rust`](https://github.com/zavora-ai/adk-rust) 生态中两个代码执行库的用法：

### 8.1 adk-sandbox（L2）

[`adk-sandbox::ProcessBackend`](file:///d:/code/rust/cortex-agent/src/infra/sandbox.rs#L76) 提供进程级隔离后端：

```rust
let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::default());
let exec_req = ExecRequest {
    language: Language::Command,   // Windows: cmd /C; Unix: sh -c
    code: runner_path_str,          // 注意：不要加字面引号
    stdin: Some(stdin_json),
    timeout: Duration::from_secs(10),
    memory_limit_mb: None,
    env: HashMap::new(),
};
let result = backend.execute(exec_req).await?;
```

**Windows 引号坑**：adk-sandbox 在 Windows 上是 `cmd /C <code>`，`std::process::Command` 会自己处理参数转义。如果 `code` 字段里再加字面引号（如 `format!("\"{}\"", path)`），`cmd /C` 会看到双层引号导致识别失败。**正确做法**：直接传 path 字符串，不加任何引号。

### 8.2 adk-code（L3）

[`adk_code::RustExecutor`](file:///d:/code/rust/cortex-agent/src/infra/code_exec.rs#L37-L39) 提供 check → build → execute 完整管线：

```rust
let executor = RustExecutor::new(backend, RustExecutorConfig::default());
let wrapper = build_rust_wrapper(script, action, oid_values_json); // 嵌入 Rhai 脚本的 Rust 程序
let result = executor.execute(&wrapper, Some(&input), timeout).await?;
// result.exec_result.exit_code == 0 表示成功
// result.output 是程序返回的 JSON
// result.diagnostics 是 rustc 编译诊断信息
```

**Base64 嵌入技巧**：把 Rhai 脚本以 Base64 编码嵌入 Rust wrapper，避开所有引号 / 反斜杠转义问题。wrapper 内自带 `base64_decode` 函数解码。

---

## 九、LLM 工作流

LLM 在对话中生成监控插件的完整工作流：

```text
用户："帮我生成一个 CPU 利用率监控插件"
   │
   ▼
LLM（system prompt 约束）生成 Rhai 脚本 + 测试用例 JSON
   │
   ▼
LLM 调用 validate_monitor_plugin 工具
   │
   ├── L1 通过？── no ──► LLM 读取错误，修正脚本，重试（最多 3 轮）
   │     │
   │    yes
   │     │
   │     ▼
   ├── L2 通过？── no ──► LLM 读取错误，修正脚本，重试
   │     │
   │    yes
   │     │
   │     ▼
   └── 断言通过？── no ──► LLM 修正脚本，重试
         │
        yes
         │
         ▼
LLM 通过 GraphQL registerMonitorPlugin 注册最终脚本
   │
   ▼
LLM 向用户报告：插件已生成并通过三层校验
```

**system prompt** 定义在 [`get_system_prompt`](file:///d:/code/rust/cortex-agent/src/tools/monitor_plugin/mod.rs#L519)，明确约束了：
- 顶层函数签名（`prepare_oids` + `parse`）
- 返回值 JSON 结构
- 可用 host function 白名单
- 模板示例
- 必须调用 `validate_monitor_plugin` 自检的硬性要求

---

## 十、测试覆盖

本项目为 Rhai 插件系统提供了完整的单元测试 + 集成测试，运行 `cargo test --lib` 可全部执行（需先 `cargo build --bin rhai-runner` 以启用 L2 测试）。

| 模块 | 测试数 | 覆盖范围 |
|------|--------|---------|
| `monitor::rhai_plugin` | 7 | compile / check_syntax / prepare_oids / parse / 缺失降级 |
| `monitor::manager` | 6 | register / unregister / list / parse 路由 / 错误处理 |
| `monitor::host_fns` | 2 | JSON roundtrip（object / array） |
| `server::monitor` | 2 | API 层端到端 round-trip |
| `infra::sandbox` | 2 | adk-sandbox L2 隔离执行 |
| `tools::monitor_plugin_validate` | 5 | 三层串联 / 语法错误 / 断言失败 / 参数解析 |
| `tools::monitor_plugin` | 4 | system prompt 内容 / 工具列表 |
| `infra::code_exec` | 3 + 1 ignored | wrapper 构造 / Base64 / 完整 L3 管线 |

运行方式：

```bash
# 编译 rhai-runner（L2 测试依赖）
cargo build --bin rhai-runner

# 运行全部 lib 测试（含 L2 沙箱测试）
cargo test --lib

# 单独运行 L3 完整管线测试（需要 rustc + 依赖，耗时 15s+）
cargo test --lib -- --ignored code_verifier_full_pipeline
```

---

## 十一、开发指南

### 11.1 新增 host function

以新增 `get_bool(map, oid) -> OptBool` 为例：

1. 在 [host_fns.rs](file:///d:/code/rust/cortex-agent/src/monitor/host_fns.rs) 定义 `OptBool` 包装类型 + 方法；
2. 在 `register_host_functions` 中 `register_type::<OptBool>()` + 注册方法；
3. 注册 `get_bool` 函数；
4. **无需手动同步子进程**：`rhai-runner` 子进程直接 `use` 主 crate 的 `cortex_agent::monitor::{apply_safety_limits, register_host_functions}`（见 `src/bin/rhai_runner.rs`），**不存在独立的 `register_minimal_host_fns`**——新增 host function 只需在 `host_fns.rs` 改一处，进程内 / 子进程行为自动一致，从设计上杜绝双注册漂移；
5. 更新 [monitor_plugin.rs](file:///d:/code/rust/cortex-agent/src/tools/monitor_plugin/mod.rs) 的 system prompt host function 白名单表格；
6. 新增单元测试覆盖。

### 11.2 调试技巧

- **脚本运行时报错**：先看 L2 返回的 `stderr`，子进程的 panic 信息会完整透传；
- **L3 编译失败**：看 `diagnostics` 字段，是 rustc 的原始诊断；
- **rhai-runner 找不到**：检查 `target/{debug,release}/rhai-runner[.exe]` 是否存在，必要时手动 `cargo build --bin rhai-runner`；
- **脚本字符串里有中文**：Rhai 源码支持 UTF-8，但建议错误消息以外的字符串值用 ASCII，避免 Base64 编解码意外的字符集问题。

### 11.3 性能基准（参考）

| 操作 | 典型耗时 |
|------|---------|
| L1 进程内编译 | 1-5 ms |
| L2 子进程 prepare_oids | 80-150 ms |
| L2 子进程 parse | 80-150 ms |
| L3 完整 Rust 编译管线（首次） | 5-15 s |
| L3 完整 Rust 编译管线（缓存命中） | 1-2 s |
