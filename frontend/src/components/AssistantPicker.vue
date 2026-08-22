<template>
  <el-dialog
    :model-value="modelValue"
    @update:model-value="$emit('update:modelValue', $event)"
    title="选择助手开启对话"
    width="680px"
    :close-on-click-modal="false"
    align-center
  >
    <div class="picker-body">
      <el-input
        v-model="keyword"
        placeholder="搜索助手名称或描述…"
        clearable
        :prefix-icon="Search"
        class="picker-search"
      />
      <div v-loading="assistantStore.loading" class="picker-grid">
        <div
          v-for="a in filtered"
          :key="a.id"
          class="picker-card"
          :class="{ selected: selectedId === a.id }"
          @click="selectedId = a.id"
          @dblclick="confirm"
        >
          <div class="pc-avatar">{{ a.avatar || '🤖' }}</div>
          <div class="pc-info">
            <div class="pc-name">
              {{ a.name }}
              <el-tag size="small" :type="a.kind === 0 ? 'info' : 'success'" effect="plain" round>
                {{ a.kind === 0 ? '内置' : '自定义' }}
              </el-tag>
            </div>
            <div class="pc-desc">{{ a.description || a.greeting || '暂无描述' }}</div>
          </div>
          <el-icon v-if="selectedId === a.id" class="pc-check"><CircleCheckFilled /></el-icon>
        </div>
        <el-empty v-if="!filtered.length && !assistantStore.loading" description="没有匹配的助手" />
      </div>
    </div>
    <template #footer>
      <el-button @click="$emit('update:modelValue', false)">取消</el-button>
      <el-button type="primary" :disabled="!selectedId" @click="confirm">开始对话</el-button>
    </template>
  </el-dialog>
</template>

<script setup>
import { ref, computed, watch } from 'vue'
import { Search, CircleCheckFilled } from '@element-plus/icons-vue'
import { useAssistantStore } from '../stores/assistant'

const props = defineProps({
  modelValue: Boolean,
})
const emit = defineEmits(['update:modelValue', 'select'])

const assistantStore = useAssistantStore()
const keyword = ref('')
const selectedId = ref(null)

const filtered = computed(() => {
  const kw = keyword.value.trim().toLowerCase()
  if (!kw) return assistantStore.assistants
  return assistantStore.assistants.filter(
    (a) =>
      a.name.toLowerCase().includes(kw) ||
      (a.description || '').toLowerCase().includes(kw),
  )
})

watch(
  () => props.modelValue,
  (v) => {
    if (v) {
      assistantStore.loadAssistants()
      selectedId.value = assistantStore.currentAssistantId
      // 重开时清掉上次搜索词，否则列表停留在上次的过滤结果
      keyword.value = ''
    }
  },
)

function confirm() {
  if (!selectedId.value) return
  emit('select', selectedId.value)
  emit('update:modelValue', false)
}
</script>

<style scoped>
.picker-search { margin-bottom: 14px; }
.picker-grid {
  display: grid; grid-template-columns: 1fr 1fr; gap: 10px;
  max-height: 420px; overflow-y: auto; padding: 2px;
}
.picker-card {
  display: flex; gap: 12px; padding: 12px; border-radius: 10px;
  border: 1px solid var(--border); background: var(--card);
  cursor: pointer; transition: all .2s; position: relative;
}
.picker-card:hover { border-color: var(--accent); background: var(--accent-dim); }
.picker-card.selected {
  border-color: var(--accent);
  box-shadow: 0 0 0 2px rgba(0, 212, 255, 0.25);
}
.pc-avatar {
  width: 42px; height: 42px; border-radius: 10px; flex-shrink: 0;
  display: flex; align-items: center; justify-content: center; font-size: 22px;
  background: rgba(255, 255, 255, 0.04); border: 1px solid var(--border);
}
.pc-info { flex: 1; min-width: 0; }
.pc-name {
  font-size: 14px; font-weight: 700; color: var(--text-h);
  display: flex; align-items: center; gap: 6px; margin-bottom: 4px;
}
.pc-desc {
  font-size: 12px; color: var(--muted); line-height: 1.4;
  overflow: hidden; text-overflow: ellipsis; display: -webkit-box;
  -webkit-line-clamp: 2; -webkit-box-orient: vertical;
}
.pc-check {
  position: absolute; top: 8px; right: 8px; color: var(--accent); font-size: 18px;
}
</style>
