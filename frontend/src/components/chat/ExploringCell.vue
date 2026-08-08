<template>
  <!-- codex 风格探索块：连续的 read_file / list_directory / grep 聚合，
       `• 探索` + 每条 `└ <标签> <参数>`，输出默认折叠成 `… +N 行`。 -->
  <div class="exploring" :class="{ active: anyRunning }">
    <!-- 头部：● Exploring（进行中）/ Explored（完成），对齐 codex exec_cell 动词态 -->
    <div class="ex-head">
      <span class="ex-dot" :class="dotClass"></span>
      <span class="ex-title">{{ anyRunning ? 'Exploring' : 'Explored' }}</span>
    </div>

    <!-- 每条探索操作：└ 标签 参数 -->
    <div class="ex-list">
      <div v-for="(it, i) in items" :key="i" class="ex-item" :class="it.cls">
        <div class="ex-item-head" @click="toggle(i)">
          <span class="ex-label">{{ it.label }}</span>
          <span class="ex-arg">{{ it.arg }}</span>
          <span class="ex-st">
            <span v-if="it.running" class="st running"><el-icon class="is-loading"><Loading /></el-icon></span>
            <span v-else-if="it.failed" class="st fail">✗</span>
            <span v-else class="st done">✓</span>
          </span>
          <span v-if="it.outputLines > 0" class="ex-fold" :class="{ expanded: opened[i] }">▸</span>
        </div>
        <!-- 输出折叠：默认 `… +N 行`，点开看全文 -->
        <div v-if="it.outputLines > 0" v-show="opened[i]" class="ex-output">
          <pre>{{ it.output }}</pre>
        </div>
        <div v-else-if="it.running" class="ex-running"><el-icon class="is-loading"><Loading /></el-icon><span>执行中…</span></div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, ref, watch } from 'vue'
import { Loading } from '@element-plus/icons-vue'
import { parseAny } from '../../utils/toolResult'

const props = defineProps({
  items: { type: Array, required: true }, // ToolCallCard msg 数组
})

const opened = ref(props.items.map(() => false))
// 新增探索项时补齐 opened 槽位
watch(() => props.items.length, (n) => {
  while (opened.value.length < n) opened.value.push(false)
})

function toggle(i) {
  // 仅在有输出时允许展开
  if (props.items[i] && outputLines(props.items[i]) > 0) {
    opened.value[i] = !opened.value[i]
  }
}

// 把工具名归一成探索操作类型：read / list / search（兼容 codex 短词、原始英文名、旧中文名）
function exploreKind(m) {
  const n = m.toolName || ''
  if (n === 'read_file' || n === 'Read' || n === '读取文件') return 'read'
  if (n === 'list_directory' || n === 'List' || n === '列出目录') return 'list'
  if (n === 'grep' || n === 'Search' || n === '搜索内容') return 'search'
  return ''
}

// 从 args 提取参数文本（对齐 codex：read->path；list->path(+递归)；search->"pattern" in path）
function argText(m) {
  const a = parseAny(m.args) || {}
  const k = exploreKind(m)
  if (k === 'read') return a.path || ''
  if (k === 'list') {
    const p = a.path || '.'
    return a.recursive ? `${p} (递归)` : p
  }
  if (k === 'search') {
    const q = a.pattern ? `"${a.pattern}"` : ''
    const p = a.path ? ` in ${a.path}` : ''
    return `${q}${p}`
  }
  return ''
}

// 子项标签：codex 风格短词 Read / List / Search
function labelFor(m) {
  const k = exploreKind(m)
  if (k === 'read') return 'Read'
  if (k === 'list') return 'List'
  if (k === 'search') return 'Search'
  return m.toolName || '工具'
}

// 结果输出文本 + 行数（探索类结果是 content/entries/matches 等，统一取可读文本）
function outputOf(m) {
  const r = parseAny(m.result)
  if (!r) return ''
  if (typeof r === 'string') return r
  if (typeof r !== 'object') return String(r)
  if (r.error) return String(r.error)
  // read_file: content
  if (typeof r.content === 'string') return r.content
  // list_directory: entries 数组
  if (Array.isArray(r.entries)) {
    return r.entries.map((e) => `${e.kind === 'dir' ? '📁' : '📄'} ${e.name || ''}`).join('\n')
  }
  // grep: matches 数组
  if (Array.isArray(r.matches)) {
    return r.matches.map((mt) => `${mt.file || ''}${mt.line ? `:${mt.line}` : ''}: ${mt.text || mt.line_text || ''}`).join('\n')
  }
  return JSON.stringify(r, null, 2)
}

function outputLines(m) {
  const t = outputOf(m)
  if (!t) return 0
  return t.split('\n').length
}

function failed(m) {
  const r = parseAny(m.result)
  return !!(r && typeof r === 'object' && r.error)
}

const items = computed(() =>
  props.items.map((m) => ({
    label: labelFor(m),
    arg: argText(m),
    running: m.status === 'running',
    failed: failed(m),
    output: outputOf(m),
    outputLines: outputLines(m),
    cls: failed(m) ? 'st-fail' : m.status === 'running' ? 'st-running' : 'st-ok',
  })),
)

const anyRunning = computed(() => props.items.some((m) => m.status === 'running'))
const dotClass = computed(() => {
  if (anyRunning.value) return 'running'
  if (props.items.some((m) => failed(m))) return 'fail'
  return 'done'
})
</script>

<style scoped>
.exploring {
  max-width: 88%;
  border-left: 2px solid var(--accent);
  padding: 4px 0 4px 10px;
  font-size: 12px;
}
.exploring.active { border-left-color: var(--doing); }

.ex-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 1px 0 4px;
}
.ex-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--done); flex-shrink: 0; }
.ex-dot.running { background: var(--doing); animation: ex-pulse 1.2s ease-in-out infinite; }
.ex-dot.fail { background: var(--error); }
@keyframes ex-pulse { 0%,100%{opacity:1} 50%{opacity:.35} }
.ex-title { font-weight: 700; color: var(--text-h); }

.ex-list { display: flex; flex-direction: column; gap: 2px; }
.ex-item { padding-left: 14px; border-left: 1px solid var(--border); margin-left: 2px; }
.ex-item-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 2px 4px;
  border-radius: 3px;
  cursor: pointer;
}
.ex-item-head:hover { background: rgba(255,255,255,0.03); }
.ex-label { color: var(--accent); font-weight: 600; flex-shrink: 0; }
.ex-item.st-fail .ex-label { color: var(--error); }
.ex-arg {
  color: var(--text);
  font-family: var(--font-mono);
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
  flex: 1;
}
.ex-st { display: inline-flex; }
.st { display: inline-flex; align-items: center; }
.st .el-icon { font-size: 11px; }
.st.running { color: var(--doing); }
.st.done { color: var(--done); }
.st.fail { color: var(--error); }
.ex-fold { font-size: 10px; color: var(--muted); transition: transform 0.2s; }
.ex-fold.expanded { transform: rotate(90deg); }

.ex-output { margin: 3px 0 6px 8px; }
.ex-output pre {
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--muted);
  background: rgba(0,0,0,0.25);
  border-radius: 4px;
  padding: 6px 8px;
  max-height: 260px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-word;
}
.ex-running {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  margin: 2px 0 4px 8px;
  font-size: 11px;
  color: var(--doing);
}
.ex-running .el-icon { font-size: 11px; }
</style>
