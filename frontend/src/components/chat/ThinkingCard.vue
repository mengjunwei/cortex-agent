<template>
  <!-- 思考展示（对齐 codex webui reasoning-item）：极简一行 + 默认收起 + 浅灰文本块。
       刻意弱化视觉重量——思考是「可忽略的旁注」，不抢正文焦点。 -->
  <div class="reasoning" @click="expanded = !expanded">
    <div class="r-head">
      <span class="r-chevron" :class="{ expanded }">▾</span>
      <span class="r-label">Thinking</span>
      <span v-if="running" class="r-spin"><el-icon class="is-loading"><Loading /></el-icon></span>
      <span v-else-if="!expanded" class="r-hint">(点击展开)</span>
    </div>
    <div v-if="expanded" class="r-body">
      <pre class="r-text">{{ text }}</pre>
    </div>
  </div>
</template>

<script setup>
import { ref } from 'vue'
import { Loading } from '@element-plus/icons-vue'

const props = defineProps({
  text: { type: String, default: '' },
  // 流式实时思考：默认收起（对齐 codex），点开才看，避免满屏刷屏抢焦点
  defaultCollapsed: { type: Boolean, default: true },
})

const expanded = ref(!props.defaultCollapsed)
// 有正文即视为进行中（流式场景）；props 不带状态，靠是否在流判定由父级控制更显隐
const running = ref(true)
</script>

<style scoped>
.reasoning {
  max-width: 88%;
  cursor: pointer;
  user-select: none;
}
.r-head {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--muted);
}
.r-chevron {
  display: inline-block;
  font-size: 10px;
  transition: transform 0.2s ease;
}
.r-chevron.expanded { transform: rotate(0deg); }
.r-chevron:not(.expanded) { transform: rotate(-90deg); }
.r-label { font-weight: 600; }
.r-hint { font-size: 11px; opacity: 0.6; }
.r-spin { display: inline-flex; align-items: center; }
.r-spin .el-icon { font-size: 12px; }

.r-body {
  margin-top: 4px;
  padding: 8px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm, 6px);
  background: rgba(255, 255, 255, 0.03);
  overflow: hidden;
}
.r-text {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
  font-family: var(--font-mono, monospace);
  font-size: 12px;
  line-height: 1.6;
  color: var(--muted);
  max-height: 320px;
  overflow-y: auto;
}
</style>
