# 模型供应商「模型探测」功能设计

- 日期：2026-08-02
- 范围：模型供应商管理页（`ModelProviderPage.vue`）新增「探测模型存活」能力，支持单模型、批量（可跨供应商）、全供应商三种粒度。
- 状态：设计待评审

## 1. 背景与目标

### 1.1 现状

模型供应商管理页可配置供应商（`llm_providers`）及其下的模型（`llm_models`），支持 OpenAI 兼容与 Anthropic 双协议（见 memory：`anthropic-provider-support`）。但**配好之后无法快速验证「这个模型到底能不能通」**——用户只能新建会话发消息试探，失败时难以区分是 key 失效、base_url 错、模型名错、还是供应商本身挂了。

代码侧已具备探测的全部原料：
- `ModelProviderStore` 持有 AES 解密后的明文 api_key（内存缓存，仅启用模型）。
- `make_model_from_resolved` 已能按 `protocol` 分发到真实 LLM 客户端。
- 项目已有成熟的「探测/连通性测试」范式：MCP 的 `probeMcpServer` / `batchProbeMcpServers`、知识库实例的 `kbInstanceTest`。

### 1.2 目标

在供应商管理页提供「模型探测」按钮：
- 探测单个模型、批量探测勾选的部分模型（可跨供应商）、探测某供应商下全部模型。
- 按「模型能力标签」分流探测（chat / embedding / rerank），结果可信。
- 探测结果集中展示在独立面板，含状态、耗时、错误详情，错误信息可一键复制。

### 1.3 非目标

- 不做结果持久化（不落库，实时探测实时展示；刷新页面清空）。
- 不做定时/自动探测、不做健康巡检。
- 不探测供应商本身（探测粒度始终是「模型」；供应商级「探测全部」= 其下所有模型逐个探测）。

## 2. 关键约束（决定方案形态）

### 2.1 探测必须绕过启用缓存与回退

现有 `ModelProviderStore::resolve_model` 有两个对探测致命的行为：
1. **只缓存「供应商启用 且 模型启用」的条目**——但探测的最高频场景恰恰是「刚配好还没启用」「疑似挂掉被禁用」的模型。
2. **自动回退**——指定的模型不可用时回退到默认模型。

若探测复用 `resolve_model`，测一个被禁用的模型时实际探测的会是默认模型，**误报「存活」**，功能失去意义。

**对策**：新增探测专用解析 `resolve_for_probe`，绕过缓存与回退，直接按 model_id 从 DB 取该模型+其供应商（解密凭证），不判断启用状态、不回退。

### 2.2 探测用「轻量 reqwest」，不复用 Llm 客户端

`make_model_from_resolved` 产出的客户端带默认重试配置（5 次重试 + 指数退避，见 `src/llm/mod.rs`）。探测场景下：
- 模型真挂时会重试 5 次每次退避，**30s 超时根本兜不住**，单次探测可能耗时数分钟。
- 错误被 retry 层包装，难以取回原始 HTTP 状态码/响应体，无法给出「401 鉴权失败」这类可操作信息。

**对策**：三类执行器统一用项目已依赖的 `reqwest`（`Cargo.toml`，0.13 + json feature）发一次性最小请求，不重试。错误信息直接取 HTTP 状态码 + 响应体。reqwest 已是直接依赖，且 `OpenAiCompatibleEmbeddingProvider` 已采用同款风格，零新依赖。

### 2.3 rerank 是项目首次直连

项目当前的 reranking 能力全部由 Dify 后端代理完成（见 `src/domain/knowledge/dify_client.rs`），**项目自身从未直连过任何 rerank API**。各厂商 rerank 接口路径/格式不统一，本设计采用 SiliconFlow/Jina/Cohere 的事实标准格式 `POST {base_url}/rerank` 兜底，少数厂商接口不同时可能误报失败。结果面板对 rerank 失败给出诚实提示（见 §6.3），不假装 100% 准确。

## 3. 架构分层

```
前端 ModelProviderPage.vue
  ├─ 嵌套模型表：复选框列 + 每行探测状态徽标
  ├─ 工具栏：探测选中(N) / 探测本供应商全部
  └─ 结果面板：el-drawer（逐项：状态/耗时/probe_kind/错误可复制）
        ↓ gql()
GraphQL MutationRoot
  └─ probeModels(input:{ids})        统一单/批（探测纯只读，单批语义一致）
        ↓
后端 server/model_provider.rs
  └─ probe_models(state, ids)        编排：解析→分流→全并发→30s 超时→收集
        ↓
ModelProviderStore::resolve_for_probe(model_id)   【新增】绕过缓存/回退
        ↓ ResolvedForProbe（解密 api_key + protocol + base_url + model + tags）
探测执行器 src/model_provider/probe.rs  【新增】
  ├─ probe_chat()      openai_compat→POST /chat/completions ; anthropic→POST /v1/messages
  ├─ probe_embedding() POST /embeddings
  └─ probe_rerank()    POST /rerank
```

## 4. 数据结构

### 4.1 ResolvedForProbe（store 新增产出，供执行器使用）

```rust
/// 探测专用的模型解析结果——不过滤启用状态、不回退。
pub struct ResolvedForProbe {
    pub id: String,
    pub name: String,            // 模型显示名
    pub model: String,           // API 模型 ID，如 "deepseek-chat"
    pub provider_name: String,   // 供应商显示名（结果面板用）
    pub vendor_name: String,
    pub base_url: String,
    pub api_key: String,         // 解密后明文
    pub protocol: ProviderProtocol,
    pub tags: Vec<String>,       // 分流判定依据
}
```

### 4.2 ProbeKind（分流结果枚举）

```rust
pub enum ProbeKind { Chat, Embedding, Rerank }
```

### 4.3 ProbeResult（单模型探测结果）

```rust
pub struct ProbeResult {
    pub model_id: String,
    pub model: String,           // 回显，便于面板不依赖列表查询
    pub provider_name: String,
    pub status: ProbeStatus,     // Ok | Fail
    pub latency_ms: u64,
    pub probe_kind: ProbeKind,
    pub error: Option<String>,   // 失败时的可操作错误信息；成功为 None
    pub probed_at: String,       // ISO8601（每个模型各自在 probe_one 完成时打）
}
```

前端透传结构（JSON 同名字段，snake_case）：
```json
{ "model_id":"...", "model":"...", "provider_name":"...",
  "status":"ok", "latency_ms":832, "probe_kind":"chat",
  "error": null, "probed_at":"2026-08-02T10:00:00Z" }
```

## 5. 探测执行器细节

### 5.1 分流判定（tags → ProbeKind）

确定性规则，无歧义（对话为核心能力）：

```
tags 含 "chat"              → Chat        （对话优先；embedding/rerank 模型一般不标 chat）
否则 tags 含 "embedding"    → Embedding
否则 tags 含 "rerank"       → Rerank
否则（无标签/仅修饰标签）   → Chat        （兜底；dto 默认 tags=["chat"]）
```

修饰标签（reasoning/vision/tool_use）不影响分流，随 chat 走。

### 5.2 三类请求构造

公共：`reqwest::Client`（新建或复用一个模块级 lazy client，带合理 connect 超时）。api_key 为空时不发鉴权头（兼容 Ollama 等本地端点，对齐 `OpenAiCompatibleEmbeddingProvider` 逻辑）。

| 执行器 | 协议 | 方法+URL | Headers | Body |
|--------|------|----------|---------|------|
| Chat | openai_compat | `POST {base_url}/chat/completions` | `Authorization: Bearer {key}` | `{"model","messages":[{"role":"user","content":"hi"}],"max_tokens":1}` |
| Chat | anthropic | `POST {base_url}/v1/messages`（base_url 空则 `https://api.anthropic.com`） | `x-api-key: {key}` + `anthropic-version: 2023-06-01` | `{"model","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}` |
| Embedding | openai_compat | `POST {base_url}/embeddings` | `Authorization: Bearer {key}` | `{"model","input":["hi"]}` |
| Rerank | openai_compat | `POST {base_url}/rerank` | `Authorization: Bearer {key}` | `{"model","query":"a","documents":["b","c"],"top_n":1}` |

base_url 结尾的 `/` 容错：构造 URL 前统一 `trim_end_matches('/')`。

### 5.3 协议 × 能力的不合法组合

Anthropic 协议不提供 embedding / rerank（Claude 仅对话）。若分流到 Embedding/Rerank 但 protocol=anthropic，**不发请求**，直接返回 `Fail`，error="Anthropic 协议不支持 {kind} 探测，请检查协议/标签配置"。

### 5.4 判活与计时

- 计时：`Instant::now()` 起始，请求返回（成功或失败）截止，记 `latency_ms`。
- 判活：HTTP 状态码 `2xx` → `Ok`；其余 → `Fail` + 进入 §6 错误分类。
- 不解析响应体内容（探测只关心可达 + key 有效 + 模型存在，响应体各异且无关）。

## 6. 错误处理

### 6.1 超时

每个执行器调用被 `tokio::time::timeout(Duration::from_secs(30), ...)` 包裹。超时 → `Fail`，error="探测超时（30s），请检查 base_url 是否可达或模型是否响应过慢"。

### 6.2 并发

`futures::future::join_all` 全并发发起。单个失败/超时不影响其他。`resolve_for_probe` 失败（模型不存在）的项也并发生成 `Fail` 结果，不阻断整体。

### 6.3 可操作错误分类（error 字段内容）

每条都告诉用户该查什么，最佳实践：

| 触发 | error 文案 |
|------|-----------|
| `resolve_for_probe` 模型不存在 | 模型不存在（可能已被删除） |
| reqwest 连接失败（DNS/拒绝/超时底层） | 无法连接到 base_url，请检查地址与网络 |
| HTTP 401 / 403 | 鉴权失败（{code}），请检查 API Key |
| HTTP 404 | 端点不存在（404），请检查 base_url / model 是否正确 |
| HTTP 429 | 触发限流（429），模型可能存活但被限速，稍后重试 |
| HTTP 5xx | 服务端错误（{code}），供应商异常 |
| 其他非 2xx | 探测失败（HTTP {code}）：{响应体摘要，截断 200 字符} |
| 超时 | 探测超时（30s），请检查 base_url 可达性/模型响应速度 |
| rerank 探测失败 | 透传厂商错误 + 追加：「rerank 采用通用 /rerank 格式，部分厂商接口不同可能导致误报，建议以实际业务调用为准」 |

HTTP 错误体优先解析 OpenAI 风格 `{"error":{"message":...}}`，失败回退原始 body。

## 7. 后端 API

### 7.1 GraphQL

新增单一 mutation（探测纯只读、无副作用，单模型与批量语义一致，合并为一个字段，不照搬 MCP 的「单+批」两个字段）：

```graphql
# src/server/graphql.rs MutationRoot 新增
async fn probe_models(&self, ctx: &Context<'_>, input: Json) -> Json {
    let req: ProbeModelsInput = match serde_json::from_value(input.0) {
        Ok(r) => r, Err(e) => return parse_err(e),
    };
    Json(super::model_provider::probe_models(state_of(ctx), req).await)
}
```

`ProbeModelsInput { ids: Vec<String> }`（放 `model_provider/dto.rs`）。`ids` 为空 → `err(INVALID_PARAMS, "ids 不能为空")`。

### 7.2 编排函数

```rust
// src/server/model_provider.rs
pub async fn probe_models(state: &AppState, req: ProbeModelsInput) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else { return db_unavailable(); };
    if req.ids.is_empty() { return response::err(code::INVALID_PARAMS, "ids 不能为空"); }

    // 全并发：每个 id 独立解析+探测+超时
    let futs = req.ids.iter().map(|id| probe_one(store, id));
    let results = futures::future::join_all(futs).await;
    ok(json!({ "results": results }))
}

async fn probe_one(store: &ModelProviderStore, id: &str) -> ProbeResult {
    // 1. 解析（绕过缓存/回退；失败也产出 Fail 结果）
    // 2. 按 tags 分流 ProbeKind
    // 3. tokio::time::timeout(30s, executor) 执行
    // 4. 在超时包裹内完成时打各自 probed_at 时间戳返回
}
```

## 8. store 改造

`src/model_provider/store/cache.rs`（或新建 `probe.rs` 子模块）新增：

```rust
impl ModelProviderStore {
    /// 探测专用解析：不走 cache、不过滤启用状态、不回退。
    /// 模型不存在 → Err；模型/供应商被禁用 → 仍返回（探测的核心场景）。
    pub async fn resolve_for_probe(&self, model_id: &str) -> Result<ResolvedForProbe, AppError> {
        // 1. SELECT llm_models 行 by id（不论 status）
        // 2. JOIN 其 llm_providers 行（不论 status），取 base_url/protocol/encrypted_key
        // 3. codec.decrypt(encrypted_key) 解密 api_key
        // 4. 组装 ResolvedForProbe 返回
    }
}
```

复用现有 `ProviderRow`/`ModelRow` 行结构、`codec.decrypt`、`ProviderProtocol::parse`、`parse_tags`，不引入新机制。SQL 用一条 JOIN 查询（参照 `models.rs`/`providers.rs` 现有的 `diesel::sql_query` 风格）。

## 9. 前端交互

文件：`frontend/src/views/ModelProviderPage.vue` + `frontend/src/api/index.js`。

### 9.1 API 封装

```js
// api/index.js
export const probeModels = (ids) =>
  gql(`mutation($input: JSON!) { probeModels(input: $input) }`, { input: { ids } })
```

### 9.2 选中状态（跨供应商）

嵌套模型表每个供应商是一张独立 `el-table`，内置 `type="selection"` 无法跨表汇总。改用**自定义复选框列**（`el-checkbox`），绑定前端 `selectedIds`（`Set<string>`，key=模型 id），天然跨供应商。展开/折叠/搜索不丢选中。

### 9.3 UI 元素

- 嵌套模型表首列：`el-checkbox`（绑 selectedIds）。
- 每行状态列旁新增**探测徽标**：未探测（无）| 探测中（转圈 loading）| 成功（绿色 ✅ + 耗时）| 失败（红色 ❌）。徽标状态存前端 `probeStatusMap: Map<id, {status,latency,error,kind}>`，按 id 匹配行。
- 嵌套表头（供应商展开区）：`探测本供应商全部`（取该供应商下所有模型 id 调接口；该供应商无模型时按钮 `disabled`）。
- 顶部工具栏右侧：`探测选中(N)`（`N = selectedIds.size`，0 时 `disabled`）。探测中按钮 loading。
- 结果面板：`el-drawer`（右侧滑出，标题「探测结果 N 项」），`el-table` 列：模型（name + model）/ 供应商 / 类型(probe_kind) / 状态 / 耗时 / 错误（失败行展示 error 文本 + 复制按钮）。

### 9.4 选中列表与刷新的一致性

`reload()` 刷新列表后，`selectedIds` 与当前可见模型取交集（删除已不存在的 id），避免探测到已删除模型。

### 9.5 探测流程

1. 收集要探测的 id 列表（选中 / 本供应商全部 / 单个）。
2. 对每个 id 置 `probeStatusMap[id] = {status:'probing'}`，立即渲染转圈。
3. `await probeModels(ids)`，拿 `results`。
4. 回填 `probeStatusMap`（按 `model_id` 对应），打开结果面板。
5. 全部成功 → `ElMessage.success`；有失败 → `ElMessage.warning` 提示「N 个失败，详见结果面板」。

## 10. 测试策略（TDD，不依赖真实 LLM）

### 10.1 单元测试

- **分流判定**（`probe.rs`）：tags → ProbeKind 矩阵——
  `["chat"]→Chat`、`["chat","reasoning"]→Chat`、`["embedding"]→Embedding`、`["rerank"]→Rerank`、`[]→Chat`、`["vision"]→Chat`、`["embedding","rerank"]→Embedding`。
- **请求构造**（`probe.rs`）：openai chat / anthropic chat / embedding / rerank 四种 body 与 URL 断言（构造后断言 JSON 字段、header 存在性、base_url trim 行为）。
- **协议×能力冲突**：anthropic + embedding → 直接 Fail（不发请求），断言不产生 HTTP 调用（用 trait mock 或注入 client）。
- **错误分类**：构造各类 HTTP 响应（mock `reqwest` 或抽出「响应→ProbeResult」纯函数测），断言 error 文案命中分类。
- **store**（`cache.rs`/`probe.rs`）：`resolve_for_probe` 对启用模型、禁用模型、已删除模型的返回（禁用→正常返回；删除→Err）。需 DB fixture。

### 10.2 编排测试

`probe_models`：注入 mock 执行器（执行器抽象为可注入闭包/trait），验证——全并发、单个超时不阻断其他、resolve 失败项产出 Fail、ids 为空报错。

### 10.3 不做的事

不写需要真实 key/网络的端到端探测测试（脆弱且 CI 不可复现）；连通性留给手动验证。

## 11. 落点清单（实现指引）

| 层 | 文件 | 改动 |
|----|------|------|
| DTO | `src/model_provider/dto.rs` | 新增 `ProbeModelsInput`；执行器结果结构（或放 `probe.rs`） |
| store | `src/model_provider/store/cache.rs` 或新建 `probe.rs` | `resolve_for_probe` + `ResolvedForProbe` |
| 执行器 | `src/model_provider/probe.rs`（新建） | `ProbeKind`/`ProbeResult`/分流/三类请求/错误分类 |
| 编排 | `src/server/model_provider.rs` | `probe_models` / `probe_one` |
| GraphQL | `src/server/graphql.rs` | MutationRoot `probeModels` |
| 前端 API | `frontend/src/api/index.js` | `probeModels` |
| 前端 UI | `frontend/src/views/ModelProviderPage.vue` | 复选框列 + 徽标 + 工具栏按钮 + 结果抽屉 |

## 12. 风险与取舍

- **rerank 通用格式兜底**（§2.3）：可能对少数厂商误报失败，结果面板诚实提示。若后续某厂商成为主流，可扩展按 vendor_name 路由不同 rerank 格式。
- **探测消耗 token**：chat 探测 `max_tokens=1`，消耗极小；embedding/rerank 输入极短。批量探测全并发对供应商有瞬时并发压力，但单次请求数据量极小，可接受；如未来模型数极大（>50），可再加并发上限（当前 YAGNI）。
- **错误体解析假设 OpenAI 风格**：非 OpenAI 风格错误体回退原始 body，不丢失信息。
