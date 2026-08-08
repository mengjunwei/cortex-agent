<template>
  <!-- 轻量 skill 提示条（对齐 Claude Code 的 `Skill(名字) 状态`）：
       模型主动 read_skill 拉取 skill 正文时显示，单行、不展开。 -->
  <div class="skill-row" :class="statusClass">
    <span class="skill-icon">✦</span>
    <span class="skill-name">Skill({{ name }})</span>
    <span class="skill-state">
      <span v-if="running" class="st running"><el-icon class="is-loading"><Loading /></el-icon> 加载中</span>
      <span v-else-if="failed" class="st fail">✗ 加载失败</span>
      <span v-else class="st done">✓ 已加载</span>
    </span>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { Loading } from '@element-plus/icons-vue'
import { parseAny } from '../../utils/toolResult'

const props = defineProps({
  msg: { type: Object, required: true },
})

const args = computed(() => parseAny(props.msg.args) || {})
// skill 名：优先取 result.name（后端回显），其次 args.name
const resultObj = computed(() => parseAny(props.msg.result))
const name = computed(() =>
  String((resultObj.value && resultObj.value.name) || args.value.name || '').trim() || '未命名',
)

const running = computed(() => props.msg.status === 'running')
// 失败：result.ok === false 或带 message 错误
const failed = computed(() => {
  const r = resultObj.value
  return !!(r && typeof r === 'object' && r.ok === false)
})
const statusClass = computed(() => (failed.value ? 'st-fail' : running.value ? 'st-running' : 'st-ok'))
</script>

<style scoped>
.skill-row {
  display: flex;
  align-items: center;
  gap: 7px;
  max-width: 88%;
  padding: 2px 6px;
  font-size: 12px;
  user-select: none;
}
.skill-icon { color: var(--accent); font-size: 12px; flex-shrink: 0; }
.skill-row.st-fail .skill-icon { color: var(--error); }
.skill-name {
  font-weight: 600;
  color: var(--text-h);
  font-family: var(--font-mono);
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}
.skill-row.st-fail .skill-name { color: var(--error); }
.skill-state { display: inline-flex; align-items: center; flex-shrink: 0; }
.st { display: inline-flex; align-items: center; gap: 4px; font-size: 11px; font-weight: 600; }
.st .el-icon { font-size: 11px; }
.st.running { color: var(--doing); }
.st.done { color: var(--done); }
.st.fail { color: var(--error); }
</style>
