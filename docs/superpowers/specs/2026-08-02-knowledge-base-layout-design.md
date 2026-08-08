# 知识库布局重构：列表 → 详情 两级导航

- 日期：2026-08-02
- 范围：前端 `frontend/src`（Vue 3 + vue-router + Element Plus，dark 主题），后端零改动
- 状态：已与用户确认方案，待写实现计划

## 1. 背景与目标

当前知识库是**单页** `KnowledgePage.vue`，把三件事挤在一起：

1. 顶部下拉选知识库实例 + 「管理知识库」按钮
2. `el-tabs` 两个 Tab：「文档列表」与「上传文档」
3. 「管理知识库」弹窗：实例表格（测试 / 编辑 / 删除）+ 嵌套新建/编辑表单（schema 驱动）

实例与文档混在一页、靠下拉切换，层级不清。

**目标**：改成清晰的**两级导航**——

- 第一层 `/knowledge`：知识库**列表**（实例 CRUD 在此完成）
- 第二层 `/knowledge/:id`：点进某个知识库后，看它**下面的文档**（列表 / 上传 / 分段）

## 2. 交互设计

### 2.1 第一层：知识库列表页 `/knowledge`

表格列出所有知识库实例：

| 列 | 内容 |
|---|---|
| 名称 | 实例名（点击进入详情） |
| 类型 | Dify / 内置（`providerLabel`） |
| 状态 | 启用 / 禁用（`el-tag` success / info） |
| 创建时间 | `created_at` |
| 操作 | 测试 / 编辑 / 删除 |

- 顶部工具栏：「新建知识库」按钮（`type="primary"` 实心）→ schema 驱动表单弹窗（复用现有 `editForm` / `currentSchemaFields` 逻辑）
- **进入详情**：操作列前置一个 `type="primary"` 实心「文档管理」按钮（主入口，最醒目），实例名称同时做成可点击链接（辅助入口）→ 跳 `/knowledge/:id`。操作列顺序：文档管理 / 测试 / 编辑 / 删除，列宽相应加宽
- 空状态：无实例时 `el-empty` + 引导新建

### 2.2 第二层：知识库详情页 `/knowledge/:id`

- 顶部：`「← 返回」` 按钮（回 `/knowledge`） + 当前知识库名称 / 类型徽标
- 主体：**该实例下的文档列表**（搜索 / 展开分段 / 批量删除 / 单行删除 / 分页），即现 `KnowledgePage.vue` 的 Tab1 内容
- 「上传文档」：从独立 Tab 改为详情页工具栏的按钮 → `el-dialog` 弹窗（上传是低频操作，不常驻页面）。表单内容（标题 / 厂商 / 设备类型 / 内容）从现 Tab2 整体搬入

### 2.3 实例名解析（详情页如何拿到「当前是哪个知识库」）

详情页 `onMounted` 调 `fetchKbInstances()` 拉全量，本地 `find(id)`：

- 命中：顶部显示该实例名 / 类型
- 未命中（实例被删 / 非法 id）：显示「实例不存在或已删除」空状态 + 返回按钮

> 实例总数通常很少，一次性拉取 + 本地 find 简单可靠，不引入新的「查单个实例」后端接口。

## 3. 视觉与可访问性规范（用户重点要求）

**核心原则：每个按钮在 dark 主题下文字与背景对比度必须达标，禁止「浅底浅字」等看不清的组合；遵循 commit 9043cf3 的决策——「默认状态醒目，无需 hover 即可识别」。**

按钮沿用项目已有的 Element Plus `type` 语义：

| 按钮 | 类型 | 说明 |
|---|---|---|
| 新建知识库 / 上传文档 / 保存 | `type="primary"` 实心 | 主操作，深底白字，最醒目 |
| 进入详情（「文档管理」入口） | `type="primary"` 实心 或 可点击行名 | 进入下一层的入口必须一眼可见 |
| 删除（实例 / 文档 / 批量） | `type="danger"` | 红色，文字清晰可辨 |
| 测试 / 编辑 / 重置 | 默认描边（无 type） | dark 主题下彩色文字足够清晰 |
| 搜索 | 默认描边 | 次要操作 |

实现约束：

- 不自定义低对比的浅色背景按钮；如需自定义 class（如现有 `btn-primary-action`），必须保证文字色与背景色对比度达到 WCAG AA（≥ 4.5:1）
- 凡是「引导用户进入下一步 / 完成核心动作」的按钮一律实心，不要 `plain`
- 详情页「返回」用图标 + 文字按钮，保证可识别

## 4. 技术方案

### 4.1 文件拆分

| 动作 | 文件 |
|---|---|
| 新建 | `frontend/src/views/KnowledgeListPage.vue` —— 实例表格 + 新建/编辑/测试/删除弹窗 |
| 新建 | `frontend/src/views/KnowledgeDetailPage.vue` —— 文档列表 + 上传弹窗 + 分段展开 |
| 删除 | `frontend/src/views/KnowledgePage.vue` —— 旧单页，路由不再引用 |
| 修改 | `frontend/src/router/index.js` —— 路由表 |
| 修改 | `frontend/src/App.vue` —— 菜单激活态 |
| 修改 | `frontend/src/api/index.js` —— 清理废弃封装 |

代码搬迁来源（现 `KnowledgePage.vue`）：

- 列表页的实例表格 + 弹窗表单 ← 旧文件 153–229 行（`mgrOpen` 弹窗及嵌套 `editOpen`）
- 详情页的文档表格 ← 旧文件 29–116 行（Tab1）
- 详情页的上传弹窗 ← 旧文件 119–149 行（Tab2）改造为 `el-dialog`
- 状态与方法（`loadDocuments` / `onExpandChange` / `handleDelete` / `handleUpload` / `loadInstances` / `loadSchema` / 实例 CRUD 等）按归属拆到对应页面

### 4.2 路由（参考助手范式 `/assistants`、`/assistants/:id/edit`）

`router/index.js` 把现有单条知识库路由替换为两条平铺路由：

```js
{ path: '/knowledge', name: 'KnowledgeList', component: () => import('../views/KnowledgeListPage.vue'), meta: { title: '知识库管理' } },
{ path: '/knowledge/:id', name: 'KnowledgeDetail', component: () => import('../views/KnowledgeDetailPage.vue'), meta: { title: '知识库详情' } },
```

详情页从 `useRoute().params.id` 取 `instanceId`，替代旧的下拉 `currentInstanceId`。

### 4.3 菜单激活态

`App.vue` 的 `isItemActive`（约 207–212 行）目前只对 `/assistants` 做了父路径前缀匹配。补一条：

```js
if (item.path === '/knowledge' && route.path.startsWith('/knowledge/')) return true
```

让详情页时「知识库管理」菜单保持高亮。

### 4.4 API 复用与清理

- **复用**：`fetchKbInstances` / `fetchKbProviderSchema` / `createKbInstance` / `updateKbInstance` / `deleteKbInstance` / `testKbInstance`（列表页）；`fetchInstanceDocuments` / `fetchInstanceSegments` / `deleteInstanceDocument` / `uploadInstanceDocument`（详情页）。均位于 `api/index.js`，无需改动。
- **清理**：删除 `api/index.js` 约 253–285 行的废弃 Dify 直连封装（`fetchDocuments` / `fetchSegments` / `deleteDocument` / `uploadDocument` / `submitFeedback`，对应后端已移除的 `kbDocuments`/`kbUpload`/`kbFeedback`）。实现时先 `grep` 确认全项目无引用再删。

### 4.5 后端

**零改动**。文档按 `instance_id` 过滤的 GraphQL 链路（`kbInstanceDocuments` → `KbListFilter` → `DocumentStore::list` 的 `WHERE kb_instance_id = $1`）已完整就绪。

## 5. 不做的事（YAGNI）

- 不新增 Pinia knowledge store —— 两个页面各自 `ref` 管理即可，与现有 chat 之外的页面风格一致
- 不新增后端「查单个实例详情」接口 —— 本地 find 足够
- 列表页**不显示文档数列** —— 避免每个实例一次 count 的 N+1 查询；文档数在详情页自然可见
- 不进一步把文档表格抽成独立子组件 —— 先完成页面拆分，若详情页过大再考虑

## 6. 验收标准

1. 访问 `/knowledge` 看到知识库列表表格，可新建 / 测试 / 编辑 / 删除实例
2. 点击操作列「文档管理」按钮（或实例名链接）跳转到 `/knowledge/:id`，顶部显示该实例名，主体是其文档列表
3. 详情页可搜索 / 展开分段 / 批量删除 / 单行删除 / 分页 / 上传文档（弹窗）
4. 详情页「返回」回到列表；刷新 `/knowledge/:id` 仍能正确加载（本地 find 兜底重拉）
5. 详情页时侧边栏「知识库管理」保持高亮
6. 列表页无实例时显示空状态引导；详情页 id 非法时显示「实例不存在」
7. 所有按钮在 dark 主题下文字清晰可辨，主操作 / 进入入口为实心高对比
8. 旧 `KnowledgePage.vue` 已删除，废弃 API 封装已清理，无残留引用
9. `npm run build` 通过（输出到 `../static`）
