# 设计:移植 Codex 风格 Skill 系统

**状态**:Draft
**日期**:2026-07-28
**作者**:brainstorming 会话产出
**关联文档**:
- 替代 `docs/design/skill-management.md`(v1 DB-based skill 系统)
- 参考实现:`D:\code\rust\codex\codex-rs\core-skills\`
- 架构约束:`docs/architecture.md`

---

## 1. 背景与动机

### 1.1 现状

cortex-agent 已有一套 DB-based skill 系统(`src/domain/skill/`),具备:
- PostgreSQL 存储 + GraphQL CRUD
- 手动绑定到 assistant(`enabled_skills` 字段)
- 构建时**静态全量注入** system prompt(`custom.rs:149-159`)
- 工具收窄(`narrow_tools_by_skills`)

### 1.2 问题

现有系统的核心缺陷(设计文档 `skill-management.md` §1.4 / §16 自承):
1. **无自动匹配** — 用户必须手动绑定 skill 到 assistant;`auto_match` 配置项预留为 `false`,从未实现
2. **无渐进式披露** — 所有绑定的 skill 正文一次性全量塞进 system prompt,token 成本随绑定数线性增长
3. **无提及语法** — 不支持 `$skill-name` 显式触发
4. **绑定耦合** — skill 只能通过 `assistant.enabled_skills` 激活;一个 skill 要复用到 N 个 assistant 必须绑 N 次
5. **DB 依赖** — 数据库不可用时整个 skill 能力失效;对 serverless / 单机场景过重

### 1.3 Codex 的解法

Codex 的 skill 系统走完全不同的路线:
- **文件系统存储** — `$CODEX_HOME/skills/<name>/SKILL.md`,无 DB
- **目录常驻 system prompt** — 所有 skill 的 name + description 渲染成精简目录进 system prompt(预算 2% 上下文窗口)
- **模型自主选择** — 模型看到目录后,根据用户请求语义匹配决定使用哪个 skill
- **`$name` 提及语法** — 用户/模型显式触发某 skill
- **渐进式正文注入** — 触发时把 SKILL.md 正文(截断到上限)注入对话,而非 system prompt
- **`read_skill` 工具** — 模型可主动拉取未提及 skill 的正文

### 1.4 目标

把 Codex 的 skill 机制层完整移植到 cortex-agent,**替换**现有 DB-based 系统,而非并存。保留 cortex-agent 的多租户 server 架构(ADK Runner + SSE),只替换 skill 的存储/发现/注入机制。

### 1.5 非目标(本期不做)

- Codex 的四层 scope(User/Repo/System/Admin) — cortex-agent 只做 **全局 + 内置** 两层
- Codex 的 plugin 命名空间(`namespace:name`)
- Codex 的 remote skill API(hazelnuts)
- Codex 的 `agents/openai.yaml` UI 元数据 — cortex-agent 无对应 UI 芯片系统
- Codex 的 symlink 策略、`fswatch` 文件监听 — 改为启动加载 + 手动 reload
- skill 的 `references/` / `assets/` 子目录自动加载 — 只做 `SKILL.md` 正文;子目录文件由模型通过 `read_file` 工具按需读取

---

## 2. 总体架构

### 2.1 核心决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 存储 | 纯文件系统 | 贴近 Codex;消除 DB 依赖 |
| 目录布局 | 全局 `{data_dir}/skills/` + 编译期嵌入内置 | 简化 Codex 四层为两层;适配 server |
| 触发机制 | 目录常驻 + `$name` 提及 + `read_skill` 工具 三合一 | Codex 完整体验 |
| 注入时机 | system prompt(目录)+ user 消息(正文)+ 工具调用(按需) | 渐进式披露 |
| 数据模型 | 自建 `SkillMetadata`,不依赖 `adk-skill` crate | cortex-agent 注入点和 Codex fragment 系统差异大,套抽象层增阻抗 |
| 现有 DB 系统 | 全部移除 | 干净替换,无死代码 |

### 2.2 模块边界

```
src/skill/                          (新增)
├── mod.rs          模块入口 + 公开类型 (SkillMetadata, SkillScope, SkillCatalog)
├── loader.rs       文件系统发现 + frontmatter 解析 + include_dir 内置 skill
├── catalog.rs      SkillService: 持有索引, render_catalog_block(budget), find_by_name()
├── mention.rs      extract_mentions(&str) -> Vec<String>  ($name 解析)
├── inject.rs       render_skill_body_block(name, text, max_chars) -> String  (XML 包裹)
└── assets/
    └── builtin/
        └── skill-creator/          (移植自 Codex,适配后)
            ├── SKILL.md
            └── scripts/
                ├── init_skill.py
                └── quick_validate.py
```

### 2.3 数据流(单轮对话)

```
┌─────────────────────────────────────────────────────────────────┐
│ 启动阶段                                                          │
│                                                                   │
│  bootstrap.rs                                                     │
│    └─ SkillService::new(cfg.skill_dir())                          │
│         ├─ install_builtin_skills()   解压 include_dir 到磁盘      │
│         ├─ discover_skills(root)      BFS 扫描 */SKILL.md         │
│         ├─ parse_frontmatter()        解析 name/description       │
│         └─ build SkillCatalog { vec, by_name }                    │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ 会话构建(sse.rs handle_run_sse)                                  │
│                                                                   │
│  1. catalog_block = skill_service.render_catalog_block(budget)   │
│     → "## 可用 Skill\n- skill-creator: 指导创建 skill...\n..."      │
│                                                                   │
│  2. mentions = extract_mentions(&user_text)                      │
│     → ["skill-creator"]  (解析 $skill-creator)                    │
│                                                                   │
│  3. mentioned_bodies = mentions.map(|n| skill_service             │
│        .read_skill_text(n)                                        │
│        .map(|t| render_skill_body_block(n, t, max_chars)))        │
│                                                                   │
│  4. user_text += mentioned_bodies   (XML 块追加到用户消息)         │
│                                                                   │
│  5. AgentContext { ..., skill_service, catalog_block }            │
│                                                                   │
│  6. build_agent_for_session(...)                                   │
│       └─ build_custom_builder                                     │
│            ├─ instruction += catalog_block     (目录进 system)     │
│            └─ builder.tool(read_skill_tool)    (工具注册)          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ 运行时(ADK Runner)                                               │
│                                                                   │
│  模型看到 system prompt 里的 skill 目录 + user 消息里的正文块       │
│  ↓                                                                │
│  (a) 若用户写了 $name → 正文已在 user 消息,直接用                 │
│  (b) 若模型自主判断需要某 skill → 调 read_skill(name) 工具拉取     │
│  (c) 模型按 skill 正文里的指令执行                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## 3. 详细设计

### 3.1 数据模型(`src/skill/mod.rs`)

```rust
/// Skill 的运行时元数据(从 frontmatter 解析)
#[derive(Debug, Clone)]
pub struct SkillMetadata {
    /// 目录名 / frontmatter name;格式 `^[a-z0-9-]+$`,1-64 字符
    pub name: String,
    /// frontmatter description(必填);用于目录渲染 + 模型相关性判断
    pub description: String,
    /// frontmatter metadata.short-description(可选);目录渲染时的简短描述
    pub short_description: Option<String>,
    /// SKILL.md 绝对路径
    pub path: PathBuf,
    /// 来源层级;User 覆盖同名的 Builtin
    pub scope: SkillScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// 编译期嵌入,启动时解压到 `{data_dir}/skills/.builtin/`
    Builtin,
    /// 用户在 `{data_dir}/skills/` 下手动放置
    User,
}

/// 全量 skill 索引(启动时构建,只读)
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    /// 去重后的有效 skill 列表(同名时 User 覆盖 Builtin),按 scope + name 排序
    skills: Vec<SkillMetadata>,
    /// name → skills 索引(快速查找)
    by_name: HashMap<String, usize>,
}
```

**设计要点**:
- 不依赖 `adk-skill` crate 的 `SkillDocument` — 它的模型是 DB 导向的(带 id/version/license/tags/allowed_tools/references/metadata/body 等字段),文件系统场景只需要 name/description/path/scope
- `SkillMetadata` 刻意精简 — frontmatter 只解析 `name` + `description` + `metadata.short-description` 三个字段,其余忽略(对齐 Codex `SkillFrontmatter`)
- `SkillCatalog` 是值类型(Copy 不cheap 但 Clone 足够;启动时构建一次,后续只读)

### 3.2 文件发现与 frontmatter 解析(`src/skill/loader.rs`)

#### 目录布局

```
{data_dir}/skills/                   ← skill_dir (config 可配)
├── .builtin/                        ← 内置 skill(启动时解压,用户勿改)
│   └── skill-creator/
│       ├── SKILL.md
│       └── scripts/
│           ├── init_skill.py
│           └── quick_validate.py
└── <user-skill>/                    ← 用户自定义 skill
    └── SKILL.md
```

**扫描规则**(对齐 Codex `discover_skills_under_root`,简化):
- 从 `skill_dir` 起 BFS,最大深度 3 层(覆盖 `<root>/<name>/SKILL.md` 和 `<root>/.builtin/<name>/SKILL.md`)
- 跳过点号开头目录(除了 `.builtin` — 显式白名单)
- 跳过非 `SKILL.md` 文件名(大小写敏感)
- 同名 skill:User scope 覆盖 Builtin scope(目录扫描时 User 后处理)

#### frontmatter 格式

```yaml
---
name: skill-creator
description: 指导创建 skill。当用户想创建或更新 skill 时使用此 skill...
metadata:
  short-description: 创建或更新 skill
---

# Skill 正文(触发后注入)
...
```

**解析逻辑**:
1. 读文件全文,定位首个 `---\n` 和次个 `\n---\n`
2. YAML 解析 frontmatter(用 `serde_yaml`)
3. 校验:`name` 必填且匹配 `^[a-z0-9-]+$`、1-64 字符;`description` 必填非空
4. `name` 缺失时 fallback 到目录名(Codex 行为)
5. 解析失败记 warn 并跳过该 skill(不阻塞其他 skill 加载)

#### 内置 skill 安装(`install_builtin_skills`)

- 用 `include_dir!("$CARGO_MANIFEST_DIR/src/skill/assets/builtin")` 编译期嵌入
- 启动时解压到 `{skill_dir}/.builtin/`,用 marker 文件(`.cortex-builtin-version`)记录版本
- 版本变化时重写(用户对 `.builtin/` 的修改会被覆盖,符合"内置"语义)
- 解压后 `.builtin/` 和用户 skill 一起进 BFS 扫描(scope 标记为 Builtin)

### 3.3 目录渲染(`src/skill/catalog.rs`)

`SkillService::render_catalog_block(&self, budget_pct: u8) -> String`

输出格式:

```
## 可用 Skill

以下 skill 可在本次对话中使用。使用方式:
- 在消息中写 `$skill-name` 显式触发,对应 skill 正文会自动注入
- 或由你根据任务相关性自主决定,调用 `read_skill` 工具拉取正文

### Skill 目录
- skill-creator: 指导创建 skill。当用户想创建或更新 skill 时使用此 skill (来源: .builtin)
- my-custom-skill: <description...> (来源: user)
...
```

**Token 预算**:
- `budget_pct` 默认 2(配置项 `catalog_token_budget_pct`)
- 预算 = `context_window * budget_pct / 100`(context_window 从 model 配置读取,默认 128000)
- 超预算时:先缩短 description(逐字符截断,对齐 Codex `render_lines_with_description_budget`),再删除末尾 skill
- Builtin 排在前,User 排在后;同 scope 按 name 字典序

### 3.4 提及解析(`src/skill/mention.rs`)

`pub fn extract_mentions(text: &str) -> Vec<String>`

**语法**:`$` 后跟 `[a-z0-9-]+`,长度 1-64

**示例**:
- `帮我用 $skill-creator 创建一个新 skill` → `["skill-creator"]`
- `$foo 和 $bar-bar` → `["foo", "bar-bar"]`
- `$Foo` / `$_foo` / `$foo_bar` → 不匹配(大写/下划线不合法)

**去重**:同一 name 多次提及只返回一次;保留首次出现顺序

**实现**:正则 `\$(?:[a-z0-9]+-)*[a-z0-9]+`,Collect + 去重

**与 catalog 的交叉校验**:解析出的 name 在 catalog 中不存在时静默丢弃(记 debug 日志),不报错 — 用户可能 `$` 引用其他东西

### 3.5 正文注入(`src/skill/inject.rs`)

`pub fn render_skill_body_block(name: &str, text: &str, max_chars: usize) -> String`

输出格式:

```
<skill name="skill-creator">
<description>指导创建 skill。当用户想创建或更新 skill 时使用此 skill</description>

(此处为 SKILL.md 正文,frontmatter 之后的部分,截断到 max_chars)
</skill>
```

**截断策略**:
- 读 SKILL.md 全文,去掉 frontmatter 部分(从次个 `\n---\n` 之后开始)
- 超过 `max_chars` 时按字符截断,追加 `\n...[截断:原文 N 字符]`
- `max_chars` 来自 `cfg.skill.max_inject_chars`(默认 1500,保留现有配置)

### 3.6 `read_skill` 工具(`src/tools/skill_read.rs`)

新增 ADK `FunctionTool`,让模型主动拉取 skill 正文:

```rust
pub fn create_read_skill_tool(skill_service: Arc<SkillService>) -> FunctionTool {
    FunctionTool::new(
        "read_skill",
        "读取指定 skill 的完整正文。参数 name 必须是目录中列出的 skill 名称。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let svc = skill_service.clone();
            async move {
                let name = args["name"].as_str().unwrap_or("");
                match svc.read_skill_text(name) {
                    Some(text) => Ok(json!({ "ok": true, "name": name, "content": text })),
                    None => Ok(json!({ "ok": false, "message": format!("skill '{name}' 不存在") })),
                }
            }
        },
    )
}
```

**参数 schema**:`{ name: String }` — 必填,skill 名称

**注册点**:`build_custom_builder`(`custom.rs:168` 之后)— 每个自定义 agent 都挂上此工具,无需在 `enabled_tools` 白名单里声明(常驻工具,类似 `transfer_to_agent`)

**不进 registry**:`tools/registry.rs` 是前端可选工具列表,`read_skill` 是框架内置常驻,不暴露给用户勾选

### 3.7 SkillService(`src/skill/catalog.rs`)

```rust
pub struct SkillService {
    catalog: SkillCatalog,
    skill_dir: PathBuf,
}

impl SkillService {
    /// 启动时构建(bootstrap 调用)
    pub fn new(skill_dir: PathBuf) -> Result<Self, AppError>;

    /// 强制重新扫描(未来 reload API 用;本期只启动时调一次)
    pub fn reload(&mut self) -> Result<(), AppError>;

    /// 渲染 skill 目录到 system prompt 片段
    pub fn render_catalog_block(&self, budget_pct: u8) -> String;

    /// 按 name 查找元数据
    pub fn find_by_name(&self, name: &str) -> Option<&SkillMetadata>;

    /// 读取 skill 正文(去掉 frontmatter);不存在返回 None
    pub fn read_skill_text(&self, name: &str) -> Option<String>;

    /// 批量解析 $name 提及 → 正文注入块(供 sse.rs 调用)
    pub fn resolve_mentions(&self, mentions: &[String], max_chars: usize) -> Vec<String>;
}
```

**线程安全**:`SkillService` 构建后只读,放在 `Arc<SkillService>` 里共享;`reload` 需要 `&mut`,本期不暴露(server 重启即可重新加载)

### 3.8 配置变化(`src/config/mod.rs`)

```toml
[skill]
max_inject_chars = 1500                 # 单 skill 正文注入上限(保留)
catalog_token_budget_pct = 2            # 目录占上下文窗口百分比(新增,默认 2)
# 删除 tools_mode       — 工具收窄机制移除(见 §3.9)
# 删除 auto_match       — 目录常驻即自动匹配
```

**`skill_dir` 路径**:保留现有派生方式(`AppConfig::skill_dir()` 方法,`config/mod.rs:646-648`,返回 `{data_dir}/skills`)— **不新增配置字段**,与 `workspace_session_dir` 等其他目录派生方法一致。

### 3.9 工具收窄机制移除

现有 `narrow_tools_by_skills`(`custom.rs:73-101`)基于 `skill.allowed_tools` 收窄 `assistant.enabled_tools`。新系统:
- frontmatter 不解析 `allowed-tools`(Codex 也不读此字段做触发)
- skill 不再绑定到 assistant → 无 per-assistant 工具收窄
- **删除** `narrow_tools_by_skills` + `cfg.skill.tools_mode`

替代方案:skill 正文里可以写"请只使用 web_search 工具"之类的指令,由模型遵守(软约束,对齐 Codex)

---

## 4. 注入点改造

### 4.1 注入点 1:目录进 system prompt

**位置**:`src/agent/custom.rs:150`(现有 instruction 拼接处)

**改造前**:
```rust
let mut instruction = assistant.system_prompt.clone();
for doc in &skill_docs {
    instruction.push_str("\n\n");
    instruction.push_str(&doc.engineer_prompt_block(cfg.skill.max_inject_chars));
}
```

**改造后**:
```rust
let mut instruction = assistant.system_prompt.clone();
// 注入 skill 目录(渐进式披露 L1:元数据常驻)
if let Some(svc) = ctx.skill_service.as_ref() {
    let catalog = svc.render_catalog_block(cfg.skill.catalog_token_budget_pct);
    if !catalog.is_empty() {
        instruction.push_str("\n\n");
        instruction.push_str(&catalog);
    }
}
```

### 4.2 注入点 2:`$name` 正文进 user 消息

**位置**:`src/server/sse.rs`(handle_run_sse,构建 user_text 之后、create_event_stream 之前)

**新增逻辑**:
```rust
// 解析用户消息中的 $skill-name 提及,注入对应正文
if let Some(svc) = state.skill_service.as_ref() {
    let mentions = crate::skill::extract_mentions(&user_text);
    if !mentions.is_empty() {
        let blocks = svc.resolve_mentions(&mentions, state.config.skill.max_inject_chars);
        if !blocks.is_empty() {
            user_text.push_str("\n\n");
            user_text.push_str(&blocks.join("\n\n"));
        }
        tracing::info!("[sse] skill 提及解析: {:?} → 注入 {} 个正文块", mentions, blocks.len());
    }
}
```

**位置精确**:在 `workspace_mode` 判别块结束(`sse.rs:542`)之后、`AgentContext` 构建(`sse.rs:545`)之前,**顶层位置**(不在 code_assistant 条件块内,对所有 agent_type 生效)

### 4.3 注入点 3:`read_skill` 工具

**位置**:`src/agent/custom.rs:168`(builder 创建后)

**新增**:
```rust
let mut builder = LlmAgentBuilder::new(&agent_name)
    .description(&assistant.description)
    .instruction(instruction)
    .model(model)
    .generate_content_config(gen_cfg);

// 注册 read_skill 工具(常驻,不受 enabled_tools 白名单约束)
if let Some(svc) = ctx.skill_service.as_ref() {
    builder = builder.tool(Arc::new(
        crate::tools::skill_read::create_read_skill_tool(svc.clone())
    ));
}
```

### 4.4 影响范围:builtin / orchestration agent

**Builtin agent**(device_command / code_assistant / monitor_plugin / command_brainstorm):
- **本期不加 skill 注入** — 这些 agent 有独立的 instruction 构建逻辑,且职责单一(查设备/写代码/生成插件/brainstorm),skill 目录对它们价值低
- 未来如需,可在各自 builder 里加 `catalog_block` 拼接(改动模式同 §4.1)

**Orchestration 父 agent**(delegate / parallel / sequential / router):
- **delegate** — 通过 `build_custom_builder` 构建,自动获得目录注入 + read_skill 工具(§4.1 + §4.3)
- **parallel / sequential** — 只包装子 agent,父无 instruction,不加
- **router** — 有 instruction(`orchestration.rs:240-246`),但用 assistant.system_prompt 原文,本期不加

---

## 5. 删除清单

### 5.1 代码文件

| 文件 | 处理 |
|------|------|
| `src/domain/skill/mod.rs` | **删除** |
| `src/domain/skill/models.rs` | **删除** |
| `src/domain/skill/store.rs` | **删除** |
| `src/domain/skill/manager.rs` | **删除** |
| `src/domain/skill/materialize.rs` | **删除** |
| `src/domain/skill/dto.rs` | **删除** |
| `src/domain/skill/enums.rs` | **删除** |
| `src/server/skill.rs` | **删除**(GraphQL skill CRUD handler) |
| `migrations/9.sql` | **删除**(skill 表 DDL) |

### 5.2 依赖

| 项 | 处理 |
|----|------|
| `Cargo.toml` 的 `adk-skill = "1"` | **删除** |
| `Cargo.toml` 新增 `include_dir = "0.7"` | 编译期嵌入内置 skill |
| `Cargo.toml` 新增 `serde_yaml = "0.9"`(或 `serde_yml`)| frontmatter YAML 解析;现有 Cargo.toml 无 YAML 解析依赖(已确认 `grep yaml` 无结果) |
| `regex = "1"` | **已存在**(Cargo.toml:50),无需新增 |

### 5.3 代码引用清理(grep `enabled_skills` / `skill_manager` / `skill_docs` / `adk_skill` / `SkillDocument` / `crate::domain::skill`)

| 文件 | 改动 |
|------|------|
| `src/lib.rs` | `pub mod domain::skill` → `pub mod skill` |
| `src/bootstrap.rs:32,87,377-399,424` | `SkillManager` → `SkillService`;初始化逻辑改写(不依赖 db_pool) |
| `src/server/mod.rs:64` | `pub(crate) mod skill` **删除** — 新系统无 HTTP/GraphQL API(skill 通过文件系统管理) |
| `src/server/graphql.rs:248-265,597-648` | 删除 9 个 skill GraphQL 字段(skills / skill / skillsPaged / createSkill / updateSkill / deleteSkill / duplicateSkill / batchSetSkillStatus / batchDeleteSkills / reloadSkills) |
| `src/server/sse.rs:414-426,485-494,554,565` | 删 `requires_browser_toolset` + `resolve_for_agent` + `skill_docs`;新增 `extract_mentions` + `resolve_mentions` + `catalog_block` |
| `src/agent/custom.rs:67-101,121-206,240-279,288-472` | 删 `narrow_tools_by_skills`;`skill_docs` 参数全删;`build_custom_builder` 加 `skill_service` + catalog 注入 + read_skill 工具 |
| `src/agent/orchestration.rs:82,167-189` | 删子助手 skill 解析(enabled_skills → resolve_for_agent) |
| `src/agent/custom.rs:251-252` | `AgentContext.skill_manager` → `skill_service: Option<Arc<SkillService>>` |
| `src/agent/custom.rs:278` | `AgentRequest.skill_docs` 字段删除 |
| `src/domain/assistant/models.rs:34,101,144,171-172,189,224` | 删 `enabled_skills` 字段(领域模型 + DB 行) |
| `src/domain/assistant/store.rs:136-139,181,205,306,333,355,454,478` | 删 `enabled_skills` 列读写 + ALTER TABLE;删 `encode_skills` |
| `src/server/assistant.rs:50,110,137,166,710,734,758,790` | 删 DTO 的 `enabled_skills` 字段 + 默认值 |
| `src/config/mod.rs:422-450` | `SkillConfig` 重写(见 §3.8) |

### 5.4 DB schema 变更

- **不写 migration 删表** — `migrations/9.sql` 文件删除即可(项目用 idempotent `CREATE TABLE IF NOT EXISTS` + inline `ensure_schema`,删 migration 文件不影响已部署实例的已存在表)
- **`assistants.enabled_skills` 列** — 同样不写 DROP COLUMN migration;`assistant store.rs` 的 ALTER TABLE 语句(`store.rs:136-139`)删除,代码不再读写此列;已部署实例的残留列无害(SQL `SELECT` 不再引用它)
- **文档说明**:在 `docs/architecture.md` §11 路线图新增条目,记录 v1.3.0 skill 系统从 DB 迁移到文件系统

---

## 6. 内置 skill:skill-creator

### 6.1 来源与适配

从 `D:\code\rust\codex\codex-rs\skills\src\assets\samples\skill-creator\` 移植,做以下适配:

| 项 | 处理 |
|----|------|
| `SKILL.md` 正文(416 行) | **保留**,核心内容通用(skill 设计原则 / 渐进式披露 / 命名规范 / 创建流程) |
| SKILL.md 里 `$CODEX_HOME/skills` / `~/.codex/skills` | **改写**为 `{data_dir}/skills`(约 5 处,在 Step 1 / Step 3) |
| SKILL.md frontmatter description | "extends Codex's capabilities" → "extends cortex-agent's capabilities" |
| `scripts/init_skill.py` | **保留**,简化掉生成 `agents/openai.yaml` 的逻辑(无对应 UI) |
| `scripts/quick_validate.py` | **保留**,校验 frontmatter 格式(通用) |
| `scripts/generate_openai_yaml.py` | **删除**(目标是 openai.yaml,无对应 UI) |
| `agents/openai.yaml` | **删除**(Codex UI 芯片元数据,不适用) |
| `references/openai_yaml.md` | **删除**(同上) |
| `license.txt` | **删除**(内置 skill 无需单独 license) |
| SKILL.md §Forward-testing(subagent 相关) | **保留**,cortex-agent 有 multi-agent orchestration,概念对应 |

### 6.2 放置位置

```
src/skill/assets/builtin/
└── skill-creator/
    ├── SKILL.md              (适配后)
    └── scripts/
        ├── init_skill.py     (简化后)
        └── quick_validate.py (原样)
```

编译期 `include_dir!` 嵌入,启动时解压到 `{skill_dir}/.builtin/skill-creator/`

### 6.3 触发方式

- 用户消息含 `$skill-creator` → 正文自动注入(§4.2)
- 用户说"帮我创建一个 skill" / "我想做一个新 skill" → 模型看目录 description 自主判断 → 调 `read_skill("skill-creator")`(§4.3)
- 两种路径等价,都会让模型获得 skill-creator 的完整指令

---

## 7. 错误处理

| 场景 | 处理 |
|------|------|
| `skill_dir` 不存在 | 启动时 `create_dir_all`,然后只装内置 skill |
| `skill_dir` 无写权限 | `install_builtin_skills` 失败 → 记 error,内置 skill 不可用,用户 skill 仍可加载(只读扫描) |
| frontmatter 解析失败 | 记 warn,跳过该 skill,不阻塞其他 |
| `$name` 引用的 skill 不存在 | 静默丢弃(记 debug),用户消息原样传给模型 |
| `read_skill` 工具调用不存在的 name | 返回 `{ ok: false, message: "skill 'xxx' 不存在" }` |
| SKILL.md 正文读取 IO 错误 | `read_skill_text` 返回 None,等同 skill 不存在 |
| catalog 为空(无任何 skill) | `render_catalog_block` 返回空串,instruction 不追加 skill 段 |

**核心原则**:skill 系统的任何故障都不应阻塞对话 — 降级为"无 skill"继续运行

---

## 8. 测试策略

### 8.1 单元测试(`src/skill/` 内 `#[cfg(test)] mod tests`)

| 模块 | 测试点 |
|------|--------|
| `loader.rs` | frontmatter 解析(正常/缺 name/缺 description/非法 name/无 frontmatter);BFS 发现(嵌套目录/点号目录跳过/.builtin 白名单);同名覆盖(User > Builtin) |
| `catalog.rs` | `render_catalog_block` 预算截断(超预算时缩短 description / 删末尾);`find_by_name` 命中/未命中;`read_skill_text` 去掉 frontmatter;`resolve_mentions` 批量解析 |
| `mention.rs` | `extract_mentions` 合法/非法 name;多提及去重;无提及返回空 |
| `inject.rs` | `render_skill_body_block` XML 格式;超 max_chars 截断 |

### 8.2 集成测试

- **端到端**:构建临时 skill_dir,放 2 个测试 skill(一个合法一个非法),验证 `SkillService::new` 加载结果
- **注入链路**:mock `SkillService`,验证 `build_custom_builder` 的 instruction 包含 catalog 块 + read_skill 工具已注册

### 8.3 验证命令

```bash
cargo build
cargo clippy --all-targets -- -D warnings
cargo test --lib
cargo test --test error_test
```

---

## 9. 实施顺序(高层面,详细 plan 由 writing-plans 产出)

1. **新增 `src/skill/` 模块**(loader / catalog / mention / inject + 内置 skill 资产)— 独立可测,不改现有代码
2. **新增 `src/tools/skill_read.rs`** — 独立可测
3. **改 `bootstrap.rs`** — 接线 `SkillService`;旧 `SkillManager` 暂保留(保证编译通过)
4. **改 `custom.rs` 注入点** — catalog 进 instruction + read_skill 工具;旧 `skill_docs` 参数暂保留
5. **改 `sse.rs`** — `$name` 提及解析;旧 `resolve_for_agent` 暂保留
6. **验证**:手动测试目录渲染 / `$name` 触发 / read_skill 工具调用

   > 注:步骤 3-5 是**代码编辑顺序**(让每个 commit 都能编译),不是运行时双系统并存。步骤 7 删除旧系统后,新系统才真正生效。

7. **删除旧系统**:按 §5 清单逐项删除;移除 `adk-skill` 依赖;清理 `enabled_skills` / GraphQL CRUD / `domain/skill/`
8. **最终验证**:`cargo build` + `cargo clippy` + `cargo test` 全绿
9. **更新文档**:`docs/architecture.md` §11 路线图;标记 `docs/design/skill-management.md` 为 superseded

---

## 10. 开放问题

### 10.1 文件监听(本期不做,记录待评估)

Codex 用 `fswatch` 监听 skill 目录变更,实时刷新索引。cortex-agent 是 server 进程,文件变更频率低(用户手动增删 skill),**本期不做监听,改靠重启或未来加 `reload_skills` API**。

若未来需要:`notify` crate(Linux/macOS/Windows 跨平台),在 `SkillService` 里 spawn 监听任务,变更时 `reload()`。需要把 `Arc<SkillService>` 改为 `Arc<RwLock<SkillService>>` 或 `Arc<ArcSwap<SkillCatalog>>`。

### 10.2 多租户隔离(本期不做)

当前 `skill_dir` 是全局的,所有用户共享同一份 skill。未来若需 per-user / per-workspace 隔离:
- 扩展 `SkillScope` 加 `User(tenant_id)` / `Workspace(ws_id)`
- `SkillService::new` 接收多个 root,按 scope 合并
- 这会改变 `SkillService` 的签名,但 catalog 渲染/注入逻辑不变

### 10.3 builtin agent 的 skill 注入(本期不做)

§4.4 决定本期不给 device_command / code_assistant 等 builtin agent 注入 skill 目录。若未来需要:
- 各 builtin builder 加 `catalog_block` 参数
- `build_builtin` 签名加 `skill_service: Option<&SkillService>`
- 调用方(`build_agent_for_sub`)从 `ctx` 传入

---

## 11. 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| 移除 `adk-skill` 依赖后,其他代码引用 `SkillDocument` / `SkillIndex` 编译失败 | 高 | grep 确认所有引用都在 `src/domain/skill/` 内(已验证);删除该目录即清除所有引用 |
| `enabled_skills` 字段移除后,前端 GraphQL schema 不兼容 | 中 | 前端需同步移除 skill 绑定 UI;DB 残留列无害;在 release note 标注 breaking change |
| 目录渲染占 system prompt token,长对话挤压可用上下文 | 中 | `catalog_token_budget_pct` 默认 2%(128k 窗口 = 2560 token ≈ 30 个 skill);用户可调 |
| `$name` 提及被模型误解(把 `$` 当普通字符) | 低 | 目录块里明确说明 `$name` 语法;模型遵循 instruction 能力足够 |
| 内置 skill 的 Python 脚本在用户环境无 Python | 低 | `init_skill.py` / `quick_validate.py` 是辅助工具,非必需;skill 正文本身不依赖脚本执行 |

---

## 12. 决策记录

- **为何不移植 Codex 的 `core-skills` crate?** — 它依赖 Codex 的 `AbsolutePathBuf` / `codex_fs` / `ContextualUserFragment` 抽象,与 ADK Runner 模型阻抗大;且其复杂度(scope 四层 / plugin 命名空间 / remote API / symlink 策略)在本期两层布局下用不到。自建 ~800 行核心逻辑可控且精确贴合注入点。
- **为何删除而非并存 DB 系统?** — 用户明确选择"全部移除"。两套系统并存会导致 skill 来源歧义(模型不知道哪个 skill 来自 DB 哪个来自文件)和维护负担。
- **为何 frontmatter 只解析 3 个字段?** — 对齐 Codex `SkillFrontmatter`(只读 name/description/short-description 做 triggering)。其他字段(allowed_tools/tags/version/...)对触发无贡献,Codex 也不读。
- **为何 builtin agent 不注入 skill?** — 职责单一 + 改动面控制。skill 的价值在于给通用 agent 补充领域知识;builtin agent 已是专用(device_command 只查设备)。未来按需扩展。

---

**End of Design**
