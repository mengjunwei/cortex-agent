<template>
  <div class="artifact-card">
    <div class="artifact-icon">{{ icon }}</div>
    <div class="artifact-info">
      <div class="artifact-title">{{ a.title || a.filename }}</div>
      <div class="artifact-meta">{{ a.filename }} · {{ sizeText }}</div>
    </div>
    <div class="artifact-actions">
      <a v-if="isHtml" :href="url" target="_blank" rel="noopener" class="artifact-btn open">在线打开</a>
      <a :href="url" :download="a.filename" class="artifact-btn dl">下载</a>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { useSessionStore } from '../../stores/session'

const props = defineProps({
  artifact: { type: Object, required: true },
})
const a = computed(() => props.artifact || {})
const sessionStore = useSessionStore()
const url = computed(() => `/api/sessions/${sessionStore.currentSessionId || ''}/files/${a.value.path || ''}`)
const isHtml = computed(() => (a.value.mime || '').startsWith('text/html'))
const icon = computed(() => {
  const m = a.value.mime || ''
  if (m.startsWith('text/html')) return '🌐'
  if (m.startsWith('image/')) return '🖼️'
  if (m.includes('pdf')) return '📄'
  if (m.includes('sheet')) return '📊'
  return '📎'
})
const sizeText = computed(() => {
  const s = a.value.size || 0
  if (s > 1048576) return (s / 1048576).toFixed(1) + ' MB'
  if (s > 1024) return (s / 1024).toFixed(1) + ' KB'
  return s + ' B'
})
</script>

<style scoped>
.artifact-card {
  display: flex;
  align-items: center;
  gap: 12px;
  background: #f0f7ff;
  border: 1px solid #d0e3ff;
  border-radius: 10px;
  padding: 12px 16px;
  margin: 8px 0;
  max-width: 520px;
}
.artifact-icon {
  font-size: 28px;
  flex-shrink: 0;
}
.artifact-info {
  flex: 1;
  min-width: 0;
}
.artifact-title {
  font-weight: 600;
  color: #1a365d;
  font-size: 14px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.artifact-meta {
  font-size: 12px;
  color: #718096;
  margin-top: 2px;
}
.artifact-actions {
  display: flex;
  gap: 8px;
  flex-shrink: 0;
}
.artifact-btn {
  padding: 6px 14px;
  border-radius: 6px;
  font-size: 13px;
  text-decoration: none;
  cursor: pointer;
  white-space: nowrap;
  transition: background 0.15s;
}
.artifact-btn.open {
  background: #2563eb;
  color: #fff;
}
.artifact-btn.open:hover {
  background: #1d4ed8;
}
.artifact-btn.dl {
  background: #e2e8f0;
  color: #475569;
}
.artifact-btn.dl:hover {
  background: #cbd5e1;
}
</style>
