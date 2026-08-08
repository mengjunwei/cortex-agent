# 后端重构设计 — 2026-08-01

## 1. 背景与现状

cortex-agent 后端为单 crate（Rust 2024），约 3.1 万行（已排除 `src/llm`——该目录为 adk-rust 标准库副本,本次不动）。分层骨架清晰（`domain`/`server`/`infra`/`agent`/`tools`/`model_provider`/`plugins`/`skill`/`config`),问题集中在三处:

### 1.1 建表机制:代码内联,无迁移
- 各 store 的 `ensure_schema()` 在代码里内联 `CREATE TABLE IF NOT EXISTS` 建表(权威)
- `migrations/schema.sql` 是**纯文档快照,不被程序加载**,且**未覆盖全部表**(`session_models`/`session_assistants`/`shell_rules`/`llm_providers`/`llm_models` 等只在代码里)
- 项目**无迁移执行器**

### 1.2 文件 / 函数过大(Rust 经验阈值 ≈ 文件 500–800 行、函数 80 行)
| 文件 | 行数 | 最长函数 |
|---|---|---|
| `server/sse.rs` | 1556 | `create_event_stream` 829 行 / `handle_run_sse` 365 行 |
| `model_provider/store.rs` | 1292 | `resolve_model` 82 / `update_provider` 84 |
| `domain/knowledge/mod.rs` | 1289 | `extract_faqs` 134 / `search` 93 |
| `agent/runtime/cortex_agent.rs` | 1274 | `run` 500(⚠️ 暂不动) |
| `domain/mcp/manager.rs` / `store.rs` | 983 / 866 | `build_toolsets` 85 / `update` 109 |
| `tools/code/grep.rs` / `tools/monitor_plugin.rs` | 847 / 823 | `scan` 202 / 多个 120 |
| `server/assistant.rs` / `session.rs` | 751 / 706 | `list_sessions` 189 / `get_session_history` 126 |

### 1.3 store 层样板重复
6 个 store(`model_provider`/`auth`/`assistant`/`mcp`/`plugin`/`session`)各自重复实现 `get_conn`/`new_id`/`is_unique_violation`/`new`/`ensure_schema`;连接池 `.get().await` 散落 11 个文件;`model_provider` 与 `mcp` 还各抄一遍 AES 密钥初始化。

---

## 2. 目标与非目标

**目标**
- 建表 DDL 统一由 sql 文件管理,cortex-agent 不管建表
- 消除 store 层样板(抽公共基础设施)
- 拆分上帝文件/函数,单个文件 < ~800 行、单个函数 < ~80 行

**非目标(YAGNI)**
- 不动 `src/llm`(标准库副本,需与上游对比)
- 不动 `agent/runtime/cortex_agent.rs`(正处 4 阶段重构中,避免冲突)
- 不引入迁移执行器/版本表(建表由外部 sql 管)
- 不做分层架构重排(骨架已合理)

---

## 3. Phase A — 建表 DDL 外移

**动机**:cortex-agent 不应承担建表职责;schema 由 sql 文件权威管理,程序启动假设库已建好。

**步骤**
1. 从各 store `ensure_schema()` 提取全部 DDL(`CREATE TABLE`/`CREATE INDEX`/`ALTER TABLE ADD COLUMN`)
2. 补全 `migrations/schema.sql`:加入缺失表(`session_models`/`session_assistants`/`shell_rules`/`llm_providers`/`llm_models`/`assistants.enabled_mcps` 等),全部 `IF NOT EXISTS` 幂等,保留按域分节注释
3. 删除各 store 的 `ensure_schema()` 函数及 `new()` 中的调用
4. **保留 seed 数据**:`model_provider::seed_default_if_empty`、`assistant::seed_builtin`、`bootstrap::upsert_mcp_seed` 是业务数据初始化(非 schema),留在代码里
5. 更新文档:`DEPLOY.md`(新部署须先执行 `schema.sql`)、`architecture.md §8`(建表改为 sql 管,不再是代码 `ensure_schema`)

**影响范围**:`model_provider/store.rs`、`domain/{auth,assistant,mcp,session,knowledge,catalog}/*.rs`、`plugins/plugin_store.rs`

**行为变化(须显式确认接受)**:启动不再自愈建表;新部署必须先跑 `schema.sql` 再启动程序。对已有库无破坏(全 `IF NOT EXISTS`)。

---

## 4. Phase B — store 公共基础设施抽离

**前置**:Phase A 完成(`ensure_schema` 已删,store 更干净)。

**设计**
- 新建 `infra/store.rs`,提供公共能力:
  - `pub fn new_id() -> String`(`Uuid::now_v7().to_string()`)
  - `pub fn is_unique_violation(e: &diesel::result::Error) -> bool`
  - `trait Store { fn pool(&self) -> &DbPool; async fn get_conn(&self) -> Result<DbPooledConnection, AppError> }`(默认实现 `self.pool.get().await.map_err(AppError::from)`)
- 各 store 删除重复的 `get_conn`/`new_id`/`is_unique_violation`,`impl Store for XxxStore`
- AES 密钥初始化(`model_provider` + `mcp` 重复的 `aes_raw`/`codec`/空密钥告警)抽到 `infra/crypto.rs` 公共 helper

**影响范围**:全部 store 文件。外部接口不变。

---

## 5. Phase C — 上帝文件 / 函数拆分

原则:按职责拆子模块 / 提取私有函数;**不改外部接口**;目标文件 < ~800 行、函数 < ~80 行。

### 5.1 `server/sse.rs` (1556) → `server/sse/` 子模块
`create_event_stream`(829 行,24 个 match 分支)塞了 6 件事,按职责拆:
- `sse/mod.rs`:路由 + handler 编排(`handle_run_sse` 简化)
- `sse/stream.rs`:`create_event_stream` 主循环骨架
- `sse/context.rs`:L1/L2 上下文压缩策略
- `sse/attachment.rs`:多模态附件注入(InlineData/FileData)
- ~~`sse/repetition_guard.rs`~~：正文 + thinking 重复退化检测（推送层兜底）——**重复退化检测已整体移除（commit a2440ad），不再有此模块**
- `sse/usage.rs`:token 用量上报
- `sse/screenshot.rs`:截图保存

### 5.2 `domain/knowledge/mod.rs` (1289) → `knowledge/` 子模块
- `knowledge/mod.rs`:入口 + 公共类型
- `knowledge/search.rs`、`upload.rs`、`faq.rs`(`learn_single_faq`+`extract_faqs`)、`compress.rs`

### 5.3 `server/session.rs` 长函数
- `list_sessions`(189):拆 `query + enrich_assistant + enrich_model + filter + paginate`(消除 5 段 N+1 注入揉一起)
- `create_session`(114)、`get_session_history`(126):提取私有子函数

### 5.4 其它长函数内部拆分(提取私有函数,文件不变或轻拆)
- `tools/code/grep.rs::scan`(202)
- `tools/monitor_plugin.rs` 多个 120 行函数(`create_snmp_test_collect_tool`/`parse`/`create_lookup_device_tool`)
- `domain/mcp/store.rs::update`(109)、`domain/mcp/manager.rs::build_toolsets`(85)
- `model_provider/store.rs`(1292):Phase A 删 DDL 后,按 `providers`/`models`/`cache` 拆子模块

### 5.5 暂不动
`agent/runtime/cortex_agent.rs`(run 500,4 阶段重构中)

---

## 6. 提交顺序与验证

**顺序**:Phase A → B → C。每个 Phase 内**分批提交**,每批独立 `cargo check`/`cargo test`/`cargo build`,独立可回滚。

**验证**
- 每批 `cargo check --bin cortex-agent` 与 `cargo build --bin cortex-agent` 通过
- 现有 `tests/` + `cargo test` 通过
- Phase A 完成后:空库跑 `schema.sql` → 启动 → 验证所有表齐全 + seed 正常
- 关键路径手测:SSE 对话、知识库检索/FAQ、MCP CRUD、助手 CRUD、会话列表

---

## 7. 风险与回滚

| 风险 | 缓解 |
|---|---|
| Phase A 删 `ensure_schema` 后老库失去启动自愈 | sql 全 `IF NOT EXISTS` 对老库无破坏;`DEPLOY.md` 强调先跑 sql |
| Phase B/C 拆分引入编译或行为偏差 | 小批提交 + `cargo check/test` 把关;不改外部接口 |
| 单批出问题 | 每批独立 git 提交,`git revert` 单批即可回滚 |
