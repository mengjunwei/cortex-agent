<template>
  <!-- codex 风格 spawn/wait 扁平行：`● Spawned <name>` + `└ prompt 摘要`。
       进行中由调用方过滤不渲染，这里只展示已完成/失败态。 -->
  <div class="sw-row" :class="statusClass">
    <div class="sw-head" @click="open = !open">
      <span class="sw-dot"></span>
      <span class="sw-title">{{ title }}</span>
      <span class="sw-state">
        <span v-if="failed" class="st fail">✗ 失败</span>
        <span v-else class="st done">✓</span>
      </span>
      <span v-if="!open && hasDetail" class="sw-arrow">▸</span>
      <span v-else-if="hasDetail" class="sw-arrow expanded">▾</span>
    </div>

    <!-- 摘要行（codex 的 `└ ` 详情）：默认显示一行；点开看完整 -->
    <div v-if="summary" class="sw-detail" :class="{ clamped: !open }" @click="open = !open">
      {{ summary }}
    </div>
  </div>
</template>

<script setup>
import { computed, ref } from 'vue'
import { parseAny } from '../../utils/toolResult'

const props = defineProps({
  msg: { type: Object, required: true },
})

const open = ref(false)

const isSpawn = computed(() => props.msg.toolName === 'spawn_agent')
const args = computed(() => parseAny(props.msg.args) || {})
const taskName = computed(() => String(args.value.task_name || '').trim())

const resultObj = computed(() => parseAny(props.msg.result))
const failed = computed(() => {
  const r = resultObj.value
  return !!(r && typeof r === 'object' && r.error)
})

const title = computed(() => {
  const label = isSpawn.value ? 'Spawned 子任务' : '等待子任务'
  return taskName.value ? `${label} · ${taskName.value}` : label
})

// 摘要：失败显错误；spawn 显 prompt；wait 显结果文本
const summary = computed(() => {
  if (failed.value) return String(resultObj.value.error || '')
  if (isSpawn.value) {
    return String(args.value.message || args.value.prompt || '').replace(/\s+/g, ' ').trim()
  }
  const r = resultObj.value
  if (!r) return ''
  return (typeof r === 'string' ? r : JSON.stringify(r)).replace(/\s+/g, ' ').trim()
})

const hasDetail = computed(() => summary.value.length > 90)
const statusClass = computed(() => (failed.value ? 'st-fail' : 'st-ok'))
</script>

<style scoped>
.sw-row {
  max-width: 88%;
  padding: 3px 2px;
  font-size: 12px;
  user-select: none;
}
.sw-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 2px 6px;
  border-radius: var(--radius-sm);
  cursor: pointer;
  transition: background 0.15s;
}
.sw-head:hover { background: rgba(255, 255, 255, 0.03); }

.sw-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  flex-shrink: 0;
  background: var(--done);
}
.sw-row.st-fail .sw-dot { background: var(--error); }

.sw-title { font-weight: 700; color: var(--text-h); }
.sw-row.st-fail .sw-title { color: var(--error); }

.sw-state { display: inline-flex; align-items: center; }
.st { font-size: 11px; font-weight: 600; }
.st.done { color: var(--done); }
.st.fail { color: var(--error); }

.sw-arrow { font-size: 11px; color: var(--muted); margin-left: auto; }

/* codex 的 `└ ` 详情：左侧竖线缩进；默认单行截断，点开展开 */
.sw-detail {
  margin: 2px 0 2px 14px;
  padding-left: 12px;
  border-left: 1px solid var(--border);
  color: var(--muted);
  font-size: 12px;
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-word;
  cursor: pointer;
}
.sw-detail.clamped {
  display: -webkit-box;
  -webkit-line-clamp: 1;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
</style>
