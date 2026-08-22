<template>
  <div class="tool-group" :class="{ 'all-done': allDone }">
    <button class="group-head" @click="open = !open">
      <span class="chevron" :class="{ expanded: open }">▸</span>
      <span class="group-icon"><Wrench :size="13" /></span>
      <span class="group-title">{{ items.length }} 个工具调用</span>
      <span class="group-status">
        <span v-if="allDone" class="done-pill">完成</span>
        <span v-else class="running-pill"><el-icon class="is-loading"><Loading /></el-icon>运行中</span>
      </span>
    </button>
    <div v-if="open" class="group-body">
      <ToolCallCard v-for="(m, i) in items" :key="i" :msg="m" />
    </div>
  </div>
</template>

<script setup>
import { ref, computed, watch } from 'vue'
import { Loading } from '@element-plus/icons-vue'
import { Wrench } from 'lucide-vue-next'
import ToolCallCard from './ToolCallCard.vue'

const props = defineProps({
  items: { type: Array, required: true },
})

// 全部完成？
const allDone = computed(() => props.items.every((m) => m.status !== 'running'))
// 运行中默认展开，完成后允许折叠
const open = ref(!allDone.value)

// 全部完成时自动收起（仅 running → done 转变时触发一次，参考 codex ToolCallGroup）
const prevDone = ref(allDone.value)
watch(allDone, (now) => {
  if (now && !prevDone.value) open.value = false
  prevDone.value = now
})
</script>

<style scoped>
.tool-group {
  border: 1px solid var(--border);
  border-left: 3px solid var(--accent);
  border-radius: var(--radius);
  background: rgba(14, 14, 22, 0.5);
  overflow: hidden;
  max-width: 88%;
  transition: border-color 0.2s, box-shadow 0.2s;
}
.tool-group.all-done { border-left-color: var(--done); }
.group-head {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 9px 14px;
  background: transparent;
  border: none;
  cursor: pointer;
  font-size: 12px;
  color: var(--text-h);
  user-select: none;
}
.group-head:hover { background: rgba(255, 255, 255, 0.025); }
.chevron {
  font-size: 11px;
  color: var(--muted);
  transition: transform 0.2s;
}
.chevron.expanded { transform: rotate(90deg); }
.group-icon {
  width: 20px;
  height: 20px;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--accent-dim);
  border-radius: 5px;
  color: var(--accent);
}
.group-title { font-weight: 700; }
.group-status { margin-left: auto; display: flex; align-items: center; gap: 8px; }

/* 与 ToolCallCard 保持一致的状态药丸 */
.running-pill {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  font-weight: 700;
  padding: 2px 10px;
  border-radius: 10px;
  background: var(--doing-dim);
  color: var(--doing);
  border: 1px solid rgba(245, 158, 11, 0.3);
}
.running-pill .el-icon { font-size: 12px; }
.done-pill {
  font-size: 11px;
  font-weight: 700;
  padding: 2px 10px;
  border-radius: 10px;
  background: var(--done-dim);
  color: var(--done);
  border: 1px solid rgba(16, 185, 129, 0.3);
}

.group-body {
  padding: 8px 12px 12px;
  border-top: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  gap: 8px;
}
/* 组内卡片撑满组宽 */
.group-body :deep(.tool-card) { max-width: 100%; }
</style>
