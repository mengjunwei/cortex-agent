# 助手级 Skill 白名单（硬隔离）+ 编辑页分步向导重做 — 设计文档

- 日期：2026-08-22
- 状态：已评审对齐，待实现
- 关联：`docs/superpowers/specs/2026-07-28-codex-style-skills-design.md`（skill 系统）、`docs/architecture.md` §8.2（禁 JSONB）

## 1. 背景与目标

当前 skill 是**全局**的：每个助手构建时注入同一份全量 catalog（`src/agent/builder.rs`），并挂常驻 `read_skill` 工具，没有按助手过滤的能力。

目标：让用户能给某个助手配置「可用 skill 白名单」——

- **不配置（默认）= 全部 skill 可见**（向后兼容现状）。
- **配置了 = 仅白名单内的 skill 可见**，且是**硬隔离**：被隐藏的 skill，模型无论通过 catalog、`read_skill` 工具还是 `$mention` 都拿不到。
- 配置入口放在**前端助手编辑页**（可视化多选）。

附带两项本次一起做的工作：

- **MCP 加固核对**：确认 `enabled_mcps` 白名单已是硬隔离，补漏（见 §6）。
- **助手编辑页改分步向导**：把现有长页两栏布局重做为可自由跳步的分步向导，提升专业度（见 §7）。

## 2. 关键现状结论（实现前已核实）

- 「助手」= DB 表 `assistants`，每行已有 `enabled_tools` / `enabled_mcps`（TEXT 存 JSON 数组）等助手级白名单列 —— `enabled_skills` 完全照搬这条已踩平的路。
- skill 暴露给模型的三个出口都在 `src/domain/skill/render.rs` 的 `SkillService`：
  1. `render_catalog_block` — catalog 目录注入 system prompt；
  2. `read_skill_block` / `read_skill_text` — `read_skill` 工具主动拉取正文；
  3. `resolve_mentions` — `$name` 提及注入正文。
- MCP 白名单已是硬隔离：`src/server/sse/mod.rs` 按 `assistant.enabled_mcps` 调 `McpManager::build_toolsets`；`build_one_toolset`（`src/domain/mcp/manager/mod.rs`）对 server 做「存在 + 已启用 + 归属调用者」三重校验，未挂载的 MCP 不会成为 toolset 进入 agent。
- 前端 skill 列表已有现成接口：`query { skills }`（`fetchSkills`，见 `frontend/src/api/index.js`），多选步骤直接复用，无需新接口。

## 3. 数据模型与迁移

### 3.1 新列

`assistants` 表新增：

```sql
ALTER TABLE assistants ADD COLUMN IF NOT EXISTS enabled_skills text DEFAULT '[]'::text NOT NULL;
COMMENT ON COLUMN public.assistants.enabled_skills IS '可用 Skill 白名单（JSON 数组，存 skill name）；空数组=不限制=全部可见';
```

- 写进 `migrations/schema.sql`（本库为 pg_dump 式单文件迁移，含建表 + COPY 数据）。新增列需同时：① 在 `CREATE TABLE assistants` 加列定义；② 给存量库提供 `ALTER TABLE ... IF NOT EXISTS` 升级语句；③ 若 `COPY public.assistants (...)` 列清单为显式枚举，需把 `enabled_skills` 加入（或确认 COPY 用的是与表定义一致的列序）。
- 遵循架构 §8.2：TEXT 存 JSON，**禁 JSONB**。

### 3.2 语义

| `enabled_skills` 值 | 含义 |
|---|---|
| `[]`（默认） | 不限制，全部 skill 可见（现状，向后兼容） |
| `["a","b"]` | 白名单，仅 a、b 可见 |

### 3.3 Rust 模型（照搬 enabled_mcps 模式）

以下结构各加 `enabled_skills: Vec<String>`：

- `src/domain/assistant/models.rs`：`Assistant`（领域模型）、`AssistantRow`（`#[diesel(sql_type = sql_types::Text)]`，存 JSON 字符串）、`CustomAssistantInput`。
- `From<AssistantRow> for Assistant`：`serde_json::from_str(&r.enabled_skills).unwrap_or_default()`。
- `src/server/assistant/mod.rs`：`WriteAssistantRequest`（`#[serde(default)]`）、`AssistantDto`（序列化回显给编辑页）。
- **不进** `AssistantPublicDto` / 广场卡片（可见性配置，对齐 `enabled_mcps` 同样不进）。

### 3.4 校验

- 写入时（`WriteAssistantRequest::to_input`）：用 `crate::domain::skill::is_valid_skill_name` 过滤非法名；**不存在的名字不报错**——容忍 skill 后续被删除，渲染时自动消失，避免「skill 被删导致助手保存失败」。

### 3.5 SQL 写入点

`src/domain/assistant/store.rs`：

- `insert`：列清单 + VALUES 加 `enabled_skills`，bind `Self::encode_tools(&a.enabled_skills)`（`encode_tools` 是通用 JSON 数组编码，可复用或新增同义 `encode_skills`）。
- `update_custom`：SET 子句加 `enabled_skills=$N`（整体替换语义，与 enabled_mcps 一致；注意重排占位符编号）。
- `fork` 的 INSERT：加 `enabled_skills` 列（fork/duplicate 时随其它配置一并复制——它是可见性配置非密钥，可安全复制）。
- 内置助手 seed（`seed_builtin`）：不插入该列则走列默认值 `'[]'`=全量，不受影响。

## 4. 运行时硬隔离（核心）

在 `SkillService` 加白名单维度过滤，三个出口全部收口，**不在白名单 = 当作不存在**：

### 4.1 catalog 渲染

- `render_catalog_block` 增加白名单参数（建议新增 `render_catalog_block_filtered(budget_pct, context_window, allowed: Option<&[String]>)`，或给原方法加参）。
- 实现：先按 `allowed` 过滤 `catalog.skills`（`None` 或空切片 = 不过滤=全量），再走现有预算渲染 `render_catalog_inner`。
- 过滤应在预算计算**之前**做（白名单先收窄集合，再对收窄后集合做预算降级）。

### 4.2 正文拉取（read_skill 工具 + $mention）

- `read_skill_block` / `read_skill_text` / `resolve_mentions` 增加白名单校验：name 不在白名单内时按「不存在」处理（返回 `None` / 跳过），与现有「catalog 查不到」走同一分支。
- 这样模型用 `read_skill` 工具或 `$name` 提及被隐藏的 skill 时，得到的是「不存在」，**无法绕过**。

### 4.3 builder 接线（`src/agent/builder.rs`）

- 从 `assistant.enabled_skills` 取白名单：
  - 空 → 传 `None`（全量，现状不变）；
  - 非空 → 传 `Some(&enabled_skills)`。
- 传给 catalog 渲染调用（当前 `svc.render_catalog_block(...)` 处）。
- 创建 `read_skill` 工具（`create_read_skill_tool`）时把白名单一并传入，工具内部校验 name。

## 5. API 层

- 助手 CRUD 走 GraphQL（`src/server/assistant/mod.rs`），`WriteAssistantRequest` / `AssistantDto` 已列于 §3.3，handler 透传到 `CustomAssistantInput`，无需新增路由。
- 前端 skill 选项列表复用现有 `query { skills }`（`fetchSkills`），无需新接口。

## 6. MCP 加固（核对 + 补漏）

> 结论先行：MCP 白名单**已是硬隔离**，本次不重做，只核对 + 补一条排查日志。

- 核对确认：`build_toolsets` / `build_one_toolset` 三重校验（存在 + 已启用 + 归属）维持现状。
- 补漏：`build_toolsets` 已有「请求 N 个 / 成功 M 个」info 日志；补一条针对「`enabled_mcps` 引用了已删除 / 未启用 / 未授权 server」的显式 `warn`（当前散落在 `build_one_toolset` 的 error/warn，可在汇总处点明差集），便于排查「配了却没生效」。
- 删除联动 `purge_mcp_from_assistants`（`src/domain/mcp/store/helpers.rs`）已存在，不动。

## 7. 前端：编辑页改分步向导 + 「可用 Skill」步骤

把 `frontend/src/views/AssistantEditPage.vue` 从「长页两栏」重做为**可自由跳步的分步向导**（顶部步骤条可点击跳步 + 单区内容 + 上一步/下一步/保存）。创建与编辑共用同一向导。

### 7.1 步骤划分

| 步骤 | 内容 |
|---|---|
| Step 1 基础与提示词 | AI 智能生成 banner（生成后回填本步的名称/简介/开场白 + 系统提示词）、名称、头像、简介、开场白、系统提示词 |
| Step 2 能力挂载 | MCP 服务（现有开关列表）、知识库、**可用 Skill（新增）** |
| Step 3 模型与高级 | 模型、温度、Top-P、最大输出 Tokens、可见性、环境变量（含加密解锁） |

> AI 智能生成回填说明：`generateAssistantDraft` 产出 `name` / `description` / `system_prompt` / `greeting` 四字段，全部落在 Step 1（名称/简介/开场白是基础信息，系统提示词是同一步的提示词区）。Step 2/3 不受 AI 生成影响。AI 生成入口 banner 置于 Step 1 顶部。

### 7.2 交互规则

- **可自由跳步**：顶部步骤条任意点击跳转（编辑已有助手时尤其需要）。
- 校验：第 1 步「名称」必填，未填时阻止离开/保存并提示；保存走现有全部校验（system_prompt 长度、环境变量等）。
- 保留既有交互：AI 智能生成对话框、环境变量加密解锁（密码验证）等逻辑不变，仅迁入对应步骤。
- 布局：步骤内容单区展示，底部「上一步 / 下一步 /（末步）保存」，顶部仍保留「返回 / 重置 / 保存」全局操作。

### 7.3 「可用 Skill」多选（Step 2 能力挂载内）

- 形态：**开关列表**（与现有 MCP 服务同款视觉——每行：名称 + 描述 + 作用域标签（内置/用户），右侧 `el-switch`）。
- 数据源：`fetchSkills()`（`query { skills }`）。
- 语义提示文案：**「留空 = 全部 skill 可用；勾选后仅勾选的 skill 对该助手可用」**。
- 绑定 `form.enabled_skills: string[]`，保存进 payload。
- 编辑态加载时回填 `enabled_skills`；可参考 MCP 的「清理已失效绑定」逻辑——白名单里已不存在的 skill 名在编辑态加载时可剔除（或容忍保留，后端本就不报错，二者取其一并在实现时统一）。

### 7.4 前端改动文件

- `frontend/src/views/AssistantEditPage.vue`：重做为分步向导 + 新增 Skill 步骤。
- `frontend/src/api/index.js`：复用 `fetchSkills`，无需新增（若需补 `enabled_skills` 字段到助手 payload 类型注释则一并）。
- 全局样式沿用现有 CSS 变量（`--card`/`--border`/`--accent` 等），保持 dark 主题一致。

## 8. 兼容与边界

- **向后兼容**：存量助手 `enabled_skills` 默认 `[]` = 全量可见，行为不变。
- 内置助手（设备命令助手）seed 不写该列 → 全量，不受影响。
- fork / duplicate 复制 `enabled_skills`（非密钥）。
- 白名单里的 skill 被删除后：渲染时自动消失，残留无效条目无副作用。
- 硬隔离保证：catalog / `read_skill` / `$mention` 三路都过滤，模型无法绕过。

## 9. 测试要点

- 后端单测：
  - `SkillService`：白名单过滤 catalog（None=全量 / Some=仅白名单 / 空切片=全量）；`read_skill_block`/`resolve_mentions` 对非白名单 name 返回 None。
  - `From<AssistantRow>`：`enabled_skills` JSON 解析（合法 / 非法→空 / 缺省）。
- 集成：构建某助手（enabled_skills 非空）→ catalog 仅含白名单内 skill；`read_skill` 拉白名单外 skill 返回「不存在」。
- 前端：编辑页新建/编辑回填与保存 `enabled_skills`；分步向导跳步与校验。

## 10. 影响文件清单

后端：
- `migrations/schema.sql`（新列 + 注释 + COPY 列清单）
- `src/domain/assistant/models.rs`（Assistant / AssistantRow / CustomAssistantInput）
- `src/domain/assistant/store.rs`（insert / update_custom / fork / seed 兼容）
- `src/domain/skill/render.rs`（白名单过滤：catalog + read + mentions）
- `src/agent/builder.rs`（接线 enabled_skills → catalog 渲染 + read_skill 工具）
- `src/tools/skill_read.rs`（read_skill 工具白名单校验）
- `src/server/assistant/mod.rs`（WriteAssistantRequest / AssistantDto）
- `src/domain/mcp/manager/mod.rs`（补「失效引用」warn 日志）

前端：
- `frontend/src/views/AssistantEditPage.vue`（分步向导重做 + Skill 步骤）
- `frontend/src/api/index.js`（复用 fetchSkills，按需补字段注释）
