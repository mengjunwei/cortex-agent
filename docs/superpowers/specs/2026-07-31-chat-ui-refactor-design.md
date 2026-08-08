# 智能体对话界面视觉与组件重构设计

> 日期:2026-07-31
> 范围:`frontend/` 智能体对话页(`ChatPage.vue` 及其依赖)
> 参考实现:`D:\code\rust\codex-webui\web`(React + Tailwind v4 + shadcn)

## 1. 背景与目标

用户反馈当前智能体对话界面"有点丑",希望参考 `codex-webui/web` 的实现来改进。

经探索对比,核心问题不在"没样式",而在**一致性与实现缺陷**:

1. **代码高亮实际失效**:`utils/markdown.js:36-45` 使用 `marked.setOptions({ highlight(...) })` 回调,但项目依赖为 `marked ^18.0.5`,该 `highlight` 选项自 marked v5 起废弃、v12 起彻底移除。结果助手回答中的 fenced code block **不会被 hljs 高亮**,全为单色,视觉上很"扁"。这是最影响观感的真实 bug。
2. **辉光过度、缺少层级克制**:几乎每个按钮/输入/气泡都带 `box-shadow: 0 0 Xpx rgba(0,212,255,...)` 辉光,叠加在 `#06060a` 深底上显得花哨疲劳,留白不足。
3. **图标全靠 emoji**:侧边栏、工具卡片、空状态用 emoji(💬🤖🔧📦🔍),与 Element Plus 线性 SVG 图标混用,风格不统一。
4. **用户/助手消息排版不对称**:用户消息是纯文本 `{{ msg.content }}`,助手走 `renderMd`,贴代码/列表时体验割裂。
5. **单文件臃肿、样式重复**:`ChatPage.vue` 约 2300 行;`.msg / .tool-bubble / .thinking-bubble` 在 `global.css`(L345-385)和 `ChatPage.vue` 的 scoped `<style>` 中重复定义两套,数值略有出入。
6. **复制按钮脆弱**:`markdown.js:131-138` 用正则把内联 `onclick` 字符串注入 HTML,既不符合 CSP 最佳实践也易碎。

**目标**:在**保留现有"深色霓虹科技风"色系**的前提下,吸收 codex 的精致设计手段(层级化表面、克制的毛玻璃、倍数圆角、精致代码块、统一图标、微交互),修复代码高亮 bug,并把对话页拆成职责清晰的子组件。不换框架、不换色系。

## 2. 设计原则

- **保留霓虹科技风**:青蓝霓虹强调色(`--accent: #00d4ff`)不变,只做"精致化",不切换为 codex 的中性蓝/毛玻璃主基调。
- **框架无关地借鉴**:codex 的"好看"主要来自 CSS 设计 token(OKLCH 变量、分级 glass、圆角链),与 React/Tailwind 无关。我们把这些**翻译**进现有的原生 CSS + 变量体系,不引入 Tailwind。
- **最小必要改动(YAGNI)**:不引入 React、不重写前端、不做 token 圆环/可调分栏/虚拟列表等与业务强耦合的交互、暂不加明暗主题切换。
- **先修 bug,再美化**:代码高亮失效是最高优先级。
- **不破坏后台页面**:`global.css:422-607` 的全局表格规范、以及 Element Plus 暗色覆盖(L75-283)被运维后台复用,改动需回归验证。

## 3. 技术选型与取舍

| 决策点 | 选定方案 | 备选(否决理由) |
|---|---|---|
| CSS 方案 | **保留原生 CSS + CSS 变量**,把 codex token 翻译进来 | 引入 Tailwind v4:需全局改造、与 Element Plus 共存复杂、风险高 |
| 代码高亮 | **`marked-highlight` 扩展**(继续用已装的 hljs,同步、改动最小) | Shiki:质量最高但体积大、需异步加载、Vue 中要处理 loading;marked 已因 API 变动坑过一次,marked-highlight 是官方维护的稳定补救方案 |
| 代码块组件化 | **不单独造 `CodeBlock.vue`**,改用"事件委托"消除内联 onclick(见 §4.3) | 独立 `CodeBlock.vue`:marked 输出 HTML 字符串经 `v-html` 注入,不是组件树,强行 SFC 化别扭且收益低 |
| Markdown 库 | **保留 marked + marked-highlight** | 迁 markdown-it:API 更稳但需重写 renderMd 与回归 GFM;留作未来想要行号/diff 代码块时的选项 |
| 图标 | **新增 `lucide-vue-next`**(与 codex 风格统一,tree-shake) | 仅用 Element Plus 自带图标:零新依赖但风格/数量有限 |

> 说明:策略③原拟"独立 `CodeBlock.vue`",细化后发现与 marked 字符串架构不匹配,调整为事件委托方案。功能目标(深色恒定背景 + 语言标签 + hover 显隐复制按钮、消除内联 onclick)不变。

## 4. 改动方案

### 4.1 设计 token 升级(`src/styles/global.css`)

聚焦 `:root`(L1-59)与对话相关样式,不动 L422-607 表格规范。

**(a) 圆角改为倍数链**(借鉴 codex 的"单一基准 + 倍数"),统一全站圆角、消除硬编码:

```css
--radius: 12px;                                  /* 基准,由 10px 提至 12px,更柔和 */
--radius-sm: calc(var(--radius) * 0.5);          /* 6px,兼容现有引用 */
--radius-md: var(--radius);                       /* 12px */
--radius-lg: calc(var(--radius) * 1.33);          /* 16px */
--radius-xl: calc(var(--radius) * 1.66);          /* 20px,用于大卡片/输入区 */
```

**(b) 新增分级毛玻璃工具类**(借鉴 codex,但用霓虹色调),用于 `chat-header` / `input-area` / 助手气泡,用"层级"替代部分实色边框:

```css
--glass-bg: rgba(14, 14, 22, 0.6);
--glass-bg-strong: rgba(14, 14, 22, 0.78);
--glass-blur: 12px;
--glass-saturate: 160%;
--glass-border: rgba(0, 212, 255, 0.08);
--glass-inset: inset 0 1px 0 rgba(255,255,255,0.04), inset 0 -1px 0 rgba(0,0,0,0.3);

.glass {
  background: var(--glass-bg);
  backdrop-filter: blur(var(--glass-blur)) saturate(var(--glass-saturate));
  -webkit-backdrop-filter: blur(var(--glass-blur)) saturate(var(--glass-saturate));
  border: 1px solid var(--glass-border);
  box-shadow: var(--shadow-sm), var(--glass-inset);
}
```

**(c) 收敛过度辉光**(精致化关键):
- 降低 `--glow-accent`(L58)强度与使用频次;
- Element Plus primary 按钮(`.el-button--primary` L81-90)的 `box-shadow` 辉光减弱、`hover` 位移去掉或减小;
- 消息气泡(`.msg` L346-361)阴影改为克制的 elevation,去掉过强彩色辉光。

**(d) 留白与节奏**:消息行间距、消息区内边距适度增大,降低信息密度造成的压迫感。

### 4.2 代码高亮修复(`src/utils/markdown.js`)

替换失效的 `highlight` 选项为 `marked-highlight` 扩展:

```js
import { marked } from 'marked'
import { markedHighlight } from 'marked-highlight'
import hljs from 'highlight.js/lib/core'
// …原有 registerLanguage 注册 13 种语言保持不变…

marked.use(markedHighlight({
  langPrefix: 'hljs language-',
  highlight(code, lang) {
    const language = lang && hljs.getLanguage(lang) ? lang : 'plaintext'
    try { return hljs.highlight(code, { language }).value } catch (_) { return code }
  },
}))
marked.setOptions({ breaks: false, gfm: true })
```

> `plaintext` 回退需注册 hljs plaintext 或手动转义;实现时确认 hljs 是否自带 plaintext,否则用 `hljs.highlightAuto` 或escape 兜底。

`renderMd` 中复制按钮的**内联 `onclick` 移除**,改为纯 class,复制逻辑走事件委托(§4.3)。

### 4.3 复制按钮:事件委托(替代内联 onclick)

marked 输出 HTML 字符串经 `v-html` 注入,无法直接绑 Vue 事件。在渲染 markdown 的容器(`AssistantMessage.vue` / `UserBubble.vue` / `ThinkingCard.vue` 根元素)上做 click 委托:

```js
function onMdClick(e) {
  const btn = e.target.closest('.copy-btn')
  if (!btn) return
  const code = btn.closest('pre')?.querySelector('code')?.textContent || ''
  navigator.clipboard.writeText(code).then(() => {
    btn.textContent = '已复制'
    setTimeout(() => (btn.textContent = '复制'), 1500)
  })
}
```

`.copy-btn` 样式由"常显"改为 `opacity:0`,在 `pre:hover` 时 `opacity:1`(借鉴 codex 的 hover 显隐)。

### 4.4 组件拆分(`src/views/ChatPage.vue` → `src/components/chat/`)

把消息渲染从 `ChatPage.vue`(约 2300 行)拆出,`ChatPage.vue` 只保留页面骨架(header / 滚动容器 / context-usage-bar / input-area)与 SSE、审批等业务逻辑。

| 新组件 | 职责 | 主要 props |
|---|---|---|
| `MessageList.vue` | 遍历 `messages`,按 role 分发到下列子组件;处理滚动到底 | `messages`, `isStreaming` |
| `UserBubble.vue` | 用户消息,**也走 markdown 渲染**(统一排版);渲染附件缩略图 | `content`, `attachments` |
| `AssistantMessage.vue` | 已落盘的助手消息,`renderMd` + 代码块事件委托 | `content` |
| `ToolCallCard.vue` | 工具调用折叠卡片,保留现有结构化特化(Rhai/validate/截图/诊断/grep/目录/JSON) | `msg`(含 `name`,`status`,`args`,`result`,`_expanded`,`_diagnostics`,`_grepSummary`,`_collapsedEntries` 等) |
| `ThinkingCard.vue` | 思考过程折叠卡片 | `text`, `defaultCollapsed` |
| `StreamingBubble.vue` | 流式输出气泡(`streamingText`)+ 打字光标 | `text` |

**消除重复样式**:`.msg / .tool-bubble / .thinking-bubble / .code-header / .copy-btn` 统一只保留在 `global.css` 一处,删除 `ChatPage.vue` scoped 中的重复定义。

**图标替换**:对话区与侧边栏的关键 emoji 替换为 `lucide-vue-next`(如 `Wrench`/`Search`/`Terminal`/`Image`/`Bot`/`User`/`Brain`/`ChevronDown`),保持线性风格统一。

### 4.5 数据流(不变)

SSE 链路(`stores/chat.js` 的 `doSse`/`handleEvent`)、消息累加逻辑、审批/确认流程**完全不动**。本次只改"渲染层",`chatStore` 暴露的 `streamingText` / `thinkingText` / `messages` 形态保持不变,子组件通过 props 消费。

## 5. 影响范围 / 文件清单

**新增依赖**(`frontend/package.json`):`marked-highlight`、`lucide-vue-next`。

**修改**:
- `frontend/src/styles/global.css` — token 升级(§4.1)、代码块/消息/思考/工具样式收敛与去重。
- `frontend/src/utils/markdown.js` — 高亮修复(§4.2)、复制按钮去 onclick(§4.3)。
- `frontend/src/views/ChatPage.vue` — 移除内联消息渲染与重复 scoped 样式,改用子组件。
- `frontend/src/main.js` — 可能无需改(继续引 github-dark);若 hljs 主题切换需配合再动。

**新增**:`frontend/src/components/chat/` 下 6 个子组件(§4.4)。

**不触碰**:`global.css:422-607` 表格规范;`stores/chat.js` 及 `api/`;后端任何代码;Element Plus 暗色覆盖中后台专用部分(仅做克制化微调并回归)。

## 6. 不做(YAGNI)

- 不换色系、不引入 React、不引入 Tailwind。
- 不做 token 用量圆环、可调整分栏、消息虚拟列表、单 turn 单卡片信息架构重做。
- 暂不加明暗主题切换(科技风以暗色为主,用户未要求)。

## 7. 风险与回滚

| 风险 | 应对 |
|---|---|
| `marked-highlight` 与 marked 18 兼容性 | marked-highlight 为 marked 官方扩展,适配 v5+;实现后用含多种语言代码块的样本回归 |
| 代码块事件委托误伤其他点击 | 委托仅匹配 `.copy-btn`,且挂在 md 容器内,作用域受限 |
| 收敛辉光/改圆角影响后台页面 | 改动聚焦对话相关变量;EP 覆盖微调后必须打开运维后台表格页回归 |
| 组件拆分引入回归(工具结果特化渲染复杂) | `ToolCallCard` 保留原有所有特化分支,仅搬运不改逻辑;逐组件迁移并对比渲染结果 |
| 回滚 | 全部为前端改动,`git revert` 即可;无数据库/后端变更 |

## 8. 验证清单

- [ ] 助手回答中的代码块(rust/js/json/bash/sql 等)出现正确语法高亮色。
- [ ] 代码块右上角"复制"按钮 hover 显隐,点击后变"已复制"并 1.5s 复位,无内联 onclick。
- [ ] 用户消息支持 markdown(代码/列表/加粗),与助手排版一致。
- [ ] 消息间距、圆角、留白视觉协调,辉光明显收敛但不失科技感。
- [ ] emoji 已替换为统一线性图标,侧边栏 + 对话区一致。
- [ ] 思考卡片、工具调用卡片折叠/展开、结构化结果(诊断/grep/截图)渲染正常。
- [ ] 流式输出逐字渲染正常,中文不被逐字断行(原有 `normalizeStreamingNewlines` 仍生效)。
- [ ] 运维后台表格/表单/弹窗页面视觉无回归。
- [ ] `pnpm build`(或项目所用命令)构建通过,产物正确输出到 `../static`。
