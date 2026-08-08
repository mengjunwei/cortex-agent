# 三项增强: Excel MCP 种子 + Pattern 权限规则 + Context 用量计

## Feature 1: Config 驱动 MCP 种子机制

### 问题
所有 MCP 服务器都需要手动在 DB 里添加。用户想集成 excel-mcp-server 等 zavora-ai 工具时,
没有自动注册机制。

### 设计
在 `[mcp]` 配置段加 `seeds` 数组,启动时自动 upsert 到 `mcp_servers` 表:

```toml
[[mcp.seeds]]
slug = "excel"
name = "Excel 报表工具"
transport = 1  # 1=stdio, 2=http
endpoint = "excel-mcp-server"
args = "[]"
env = {}
```

`bootstrap.rs` 启动时遍历 seeds,对 DB 做 upsert (按 slug 匹配)。
不覆盖用户手动改过的 status — 只创建/更新配置,不改启用状态。

### 改动
- `src/config/mod.rs`: 加 `McpConfig { seeds: Vec<McpSeedConfig> }`
- `src/bootstrap.rs`: 启动时 upsert seeds
- `config/config_1.toml`: 加注释示例

## Feature 4: Pattern 权限规则

### 问题
shell_safety 的 safelist/dangerous 全是硬编码 `const`。用户不能自定义规则,
每次执行不在 safelist 的命令都要审批,重复操作很烦。

### 设计
新建 `shell_rules` 表 + 扩展 `classify()`:

```sql
CREATE TABLE shell_rules (
    id VARCHAR(36) PRIMARY KEY,
    pattern VARCHAR(512) NOT NULL,   -- glob 模式,如 "git push*"
    decision SMALLINT NOT NULL,       -- 0=allow, 1=deny, 2=ask
    priority INT DEFAULT 0,           -- 高优先级先匹配
    enabled SMALLINT DEFAULT 1,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

`shell_safety::classify()` 流程变为:
1. 先查 DB 规则(带缓存,TTL 60s) — glob 匹配
2. 命中 → 返回对应 decision (Allow/Dangerous/NeedsPrompt)
3. 未命中 → 走现有硬编码 safelist/dangerous 逻辑

### 改动
- `src/domain/shell_rules.rs`: 新建,store + 模型 + glob 匹配
- `src/tools/shell_safety.rs`: `classify()` 增加 DB 规则前置查询
- `src/server/graphql.rs`: 加 shellRules query/mutation
- `frontend/src/views/`: 加权限规则管理页

## Feature 5: Context 用量计

### 问题
LLM 响应里的 `usage_metadata` 被 SSE handler 丢弃了,用户看不到 context 消耗,
也没有超限预警。

### 设计
1. SSE 事件加 `CONTEXT_USAGE` 变体:
```rust
#[serde(rename = "CONTEXT_USAGE")]
ContextUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    threshold: u64,    // intra_token_threshold,用于进度条
},
```

2. SSE handler 读取 `event.llm_response.usage_metadata`,发 `CONTEXT_USAGE` 事件

3. 前端 ChatPage.vue 加 token 用量条:
   - 显示 `total_tokens / threshold`
   - 达 70% 黄色,90% 红色
   - 达阈值时自动触发 compaction (已有 L2 机制,只是前端可见化)

### 改动
- `src/server/sse.rs`: SseEventMsg 加 `ContextUsage` 变体 + SSE loop 读取 usage_metadata
- `frontend/src/stores/chat.js`: 加 contextUsage ref + 事件处理
- `frontend/src/views/ChatPage.vue`: 加 token 用量条 UI

## 实现顺序

1. Feature 5 (Context 用量计) — 改动最小,立竿见影
2. Feature 4 (Pattern 权限) — 中等改动,DB + 前端
3. Feature 1 (MCP 种子) — 改动最小但依赖外部二进制
