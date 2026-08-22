<template>
  <!-- codex 风格子任务行：默认扁平一行（● 标题 + 状态 + 摘要），点击展开内部细节 -->
  <div class="sub-agent" :class="[statusClass, { expanded: msg._expanded }]">
    <!-- 主行：状态点 + 标题 + 结果摘要 + 展开箭头 -->
    <div class="sa-row" @click="toggle">
      <span class="sa-dot"></span>
      <span class="sa-title">子任务 · {{ msg.taskName }}</span>
      <span class="sa-status">
        <span v-if="msg.status === 'running'" class="st running">
          <el-icon class="is-loading"><Loading /></el-icon>运行中
        </span>
        <span v-else-if="msg.status === 'completed'" class="st done">✓ 完成</span>
        <span v-else class="st fail">✗ 失败</span>
        <span v-if="toolCount" class="sa-tools">{{ toolCount }} 工具</span>
      </span>
      <span v-if="summary" class="sa-summary">{{ summary }}</span>
      <span class="sa-arrow" :class="{ expanded: msg._expanded }">▸</span>
    </div>

    <!-- 展开细节：codex 的 └ 缩进块，承载子 agent 的工具流 + 文本输出 -->
    <div v-if="msg._expanded" class="sa-detail">
      <div v-for="(tc, i) in msg.toolCalls" :key="i" class="sa-tool">
        <div class="sa-tool-head">
          <Wrench :size="12" />
          <span class="sa-tool-name">{{ tc.name || 'tool' }}</span>
          <span v-if="tc.status === 'running'" class="st running">
            <el-icon class="is-loading"><Loading /></el-icon>运行中
          </span>
          <span v-else class="st done">完成</span>
        </div>
        <pre v-if="tc.args && tc.args !== '{}'" class="sa-pre">{{ formatArgs(tc.args) }}</pre>
        <!-- 文件写入类工具：红绿 diff 视图（对齐主流程 ToolCallCard 的渲染） -->
        <div v-if="toolDiffs[i]" class="sa-diff">
          <div
            v-for="(l, li) in toolDiffs[i].lines"
            :key="li"
            class="sa-diff-line"
            :class="l.type"
          ><span class="sa-diff-gutter">{{ l.type === 'add' ? '+' : l.type === 'del' ? '−' : '' }}</span><span>{{ l.text }}</span></div>
        </div>
        <pre v-else-if="tc.result" class="sa-pre result">{{ tc.result }}</pre>
      </div>
      <div v-if="msg.text" class="sa-text">{{ msg.text }}</div>
      <div v-else-if="!toolCount && msg.status === 'running'" class="sa-empty">子任务启动中…</div>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { Loading } from '@element-plus/icons-vue'
import { Wrench } from 'lucide-vue-next'
import { getDiffResult } from '../../utils/toolResult'

const props = defineProps({
  msg: { type: Object, required: true },
})

// 文件写入类工具（edit_file/create_file）的子 agent 结果走红绿 diff（对齐主流程）；
// 其余工具维持 JSON 摘要展示。computed 缓存：展开态流式输出时避免每个 delta 对
// 同一 diff 重复 JSON.parse（对齐主流程 ToolCallCard 的 diffView 模式）。
const toolDiffs = computed(() =>
  (props.msg.toolCalls || []).map((tc) => {
    const n = tc.name || ''
    if (n !== 'edit_file' && n !== 'create_file' && n !== 'Edit' && n !== 'Write') return null
    return getDiffResult(tc.result)
  })
)

const toolCount = computed(() => (props.msg.toolCalls ? props.msg.toolCalls.length : 0))

const statusClass = computed(() => {
  const s = props.msg.status
  if (s === 'completed') return 'st-ok'
  if (s === 'failed') return 'st-fail'
  return 'st-running'
})

// 结果摘要：codex 的 `Completed - <preview>`，取最终文本单行截断
const summary = computed(() => {
  const t = (props.msg.text || '').replace(/\s+/g, ' ').trim()
  if (!t || props.msg.status === 'running') return ''
  return t.length > 120 ? t.slice(0, 120) + '…' : t
})

function toggle() {
  props.msg._expanded = !props.msg._expanded
}

function formatArgs(args) {
  if (!args) return ''
  if (typeof args === 'string') {
    try {
      return JSON.stringify(JSON.parse(args), null, 2)
    } catch {
      return args
    }
  }
  try {
    return JSON.stringify(args, null, 2)
  } catch {
    return String(args)
  }
}
</script>

<style scoped>
/* ── 扁平子任务行（codex 风格）：无边框卡片，仅一行 + 左侧状态点 ── */
.sub-agent {
  max-width: 88%;
  padding: 4px 2px;
  font-size: 12px;
  user-select: none;
}
.sa-row {
  display: flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  padding: 3px 6px;
  border-radius: var(--radius-sm);
  transition: background 0.15s;
}
.sa-row:hover { background: rgba(255, 255, 255, 0.03); }

/* 状态点（codex 的 • / 彩色圆点） */
.sa-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  flex-shrink: 0;
  background: var(--doing);
}
.sub-agent.st-ok .sa-dot { background: var(--done); }
.sub-agent.st-fail .sa-dot { background: var(--error); }
.sub-agent.st-running .sa-dot {
  animation: sa-pulse 1.2s ease-in-out infinite;
}
@keyframes sa-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.35; }
}

.sa-title {
  font-weight: 700;
  color: var(--text-h);
  flex-shrink: 0;
}

/* 状态文字（轻量，不用药丸背景，靠色点已够） */
.sa-status {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
}
.st {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  font-size: 11px;
  font-weight: 600;
}
.st .el-icon { font-size: 11px; }
.st.running { color: var(--doing); }
.st.done { color: var(--done); }
.st.fail { color: var(--error); }
.sa-tools {
  font-size: 11px;
  color: var(--muted);
}

/* 结果摘要：codex `Completed - preview`，灰色单行截断 */
.sa-summary {
  color: var(--muted);
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
  flex: 1;
}
.sub-agent.expanded .sa-summary { display: none; }

.sa-arrow {
  font-size: 11px;
  color: var(--muted);
  flex-shrink: 0;
  transition: transform 0.2s;
}
.sa-arrow.expanded { transform: rotate(90deg); }

/* ── 展开细节（codex 的 └ 缩进块）── */
.sa-detail {
  margin: 6px 0 4px 15px;
  padding-left: 12px;
  border-left: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.sa-tool {
  font-size: 12px;
}
.sa-tool-head {
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--text-h);
}
.sa-tool-name { font-weight: 600; }
.sa-tool-head .st { margin-left: auto; }
.sa-pre {
  margin-top: 5px;
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--text);
  background: rgba(0, 0, 0, 0.25);
  border-radius: 4px;
  padding: 6px 8px;
  max-height: 220px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-word;
}
.sa-pre.result { color: var(--muted); }
/* ── 子 agent 工具 diff（红绿行，对齐主流程 ToolCallCard 的 diff 视图） ── */
.sa-diff {
  margin-top: 5px;
  border: 1px solid var(--border);
  border-radius: 4px;
  overflow: hidden;
  font-family: var(--font-mono);
  font-size: 11px;
  line-height: 1.5;
  max-height: 220px;
  overflow-y: auto;
}
.sa-diff-line {
  display: flex;
  align-items: baseline;
  white-space: pre-wrap;
  word-break: break-all;
}
.sa-diff-gutter {
  flex: 0 0 22px;
  text-align: center;
  color: var(--muted);
  user-select: none;
  background: rgba(128, 128, 128, 0.06);
}
.sa-diff-line.add { background: rgba(46, 160, 67, 0.13); }
.sa-diff-line.add .sa-diff-gutter { color: #3fb950; }
.sa-diff-line.del { background: rgba(248, 81, 73, 0.13); }
.sa-diff-line.del .sa-diff-gutter { color: #f85149; }
.sa-diff-line.hunk {
  color: #7aa2f7;
  background: rgba(122, 162, 247, 0.08);
  padding: 1px 6px 1px 0;
}
.sa-diff-line.hunk .sa-diff-gutter { visibility: hidden; }
.sa-diff-line.meta {
  color: var(--muted);
  padding: 1px 6px 1px 0;
}
.sa-diff-line.meta .sa-diff-gutter { visibility: hidden; }
.sa-diff-line.ctx { color: var(--text); }
.sa-text {
  font-size: 13px;
  line-height: 1.6;
  color: var(--text);
  white-space: pre-wrap;
  word-break: break-word;
}
.sa-empty {
  font-size: 12px;
  color: var(--muted);
  font-style: italic;
}
</style>
