<template>
  <el-dialog
    :model-value="modelValue"
    @update:model-value="$emit('update:modelValue', $event)"
    title="选择会话"
    width="560px"
    :close-on-click-modal="false"
    align-center
    @opened="onOpened"
  >
    <div class="choose-body">
      <p class="step-tip">
        助手「<b>{{ assistantName || '未命名' }}</b>」已有会话，可选择一个继续，或新建会话。
      </p>

      <el-input
        v-model="keyword"
        placeholder="搜索会话标题（支持远程搜索）"
        clearable
        :prefix-icon="Search"
        class="search-input"
        @input="onSearchInput"
        @clear="onSearchClear"
      />

      <div
        ref="listRef"
        class="session-list"
        v-loading="loading"
        @scroll="onListScroll"
      >
        <div
          v-for="s in list"
          :key="s.id"
          class="session-item"
          :class="{ active: s.id === selectedId }"
          @click="selectedId = s.id"
        >
          <el-radio :model-value="selectedId" :label="s.id" class="item-radio">
            <span class="item-title">{{ s.title || s.id }}</span>
          </el-radio>
          <span class="item-time">{{ formatTime(s.created_at) }}</span>
        </div>
        <div v-if="loadingMore" class="list-footer-hint">加载中…</div>
        <div v-else-if="list.length && page >= totalPages" class="list-footer-hint">没有更多了</div>
        <el-empty
          v-if="!loading && !list.length"
          description="没有匹配的会话"
          :image-size="60"
        />
      </div>
    </div>

    <template #footer>
      <el-button @click="$emit('update:modelValue', false)">取消</el-button>
      <el-button :icon="Plus" @click="onCreateNew">新建会话</el-button>
      <el-button
        type="primary"
        :disabled="!selectedId"
        :icon="ChatDotRound"
        @click="onConfirm"
      >
        继续该会话
      </el-button>
    </template>
  </el-dialog>
</template>

<script setup>
import { ref, watch, nextTick } from 'vue'
import { Search, Plus, ChatDotRound } from '@element-plus/icons-vue'
import { fetchSessions } from '../api'

const props = defineProps({
  modelValue: Boolean,
  assistantId: { type: String, required: true },
  assistantName: { type: String, default: '' },
})
const emit = defineEmits(['update:modelValue', 'choose', 'create-new'])

const PAGE_SIZE = 20
const list = ref([])
const loading = ref(false)
const loadingMore = ref(false)
const keyword = ref('')
const page = ref(1)
const totalPages = ref(0)
const selectedId = ref(null)
const listRef = ref(null)
let searchTimer = null

watch(
  () => props.modelValue,
  (v) => {
    if (v) {
      keyword.value = ''
      selectedId.value = null
      list.value = []
      page.value = 1
      totalPages.value = 0
    }
  },
)

async function queryFirst() {
  loading.value = true
  try {
    const { data, code } = await fetchSessions(1, PAGE_SIZE, {
      assistantId: props.assistantId,
      keyword: keyword.value,
    })
    if (code === 0) {
      list.value = data.sessions || []
      totalPages.value = data.total_pages || 0
      page.value = 1
      selectedId.value = list.value[0]?.id || null
    }
  } catch (_) {
  } finally {
    loading.value = false
  }
}

async function queryMore() {
  if (loadingMore.value || page.value >= totalPages.value) return
  loadingMore.value = true
  const next = page.value + 1
  try {
    const { data, code } = await fetchSessions(next, PAGE_SIZE, {
      assistantId: props.assistantId,
      keyword: keyword.value,
    })
    if (code === 0) {
      list.value.push(...(data.sessions || []))
      page.value = next
    }
  } catch (_) {
  } finally {
    loadingMore.value = false
  }
}

function onOpened() {
  queryFirst()
}

function onSearchInput() {
  if (searchTimer) clearTimeout(searchTimer)
  searchTimer = setTimeout(() => {
    queryFirst()
    nextTick(() => {
      if (listRef.value) listRef.value.scrollTop = 0
    })
  }, 300)
}

function onSearchClear() {
  queryFirst()
}

function onListScroll() {
  const el = listRef.value
  if (!el) return
  if (el.scrollTop + el.clientHeight >= el.scrollHeight - 40) {
    queryMore()
  }
}

function onConfirm() {
  if (!selectedId.value) return
  emit('choose', selectedId.value)
  emit('update:modelValue', false)
}

function onCreateNew() {
  emit('create-new')
  emit('update:modelValue', false)
}

function formatTime(ts) {
  if (!ts) return ''
  const d = new Date(ts)
  if (Number.isNaN(d.getTime())) return ''
  return d.toLocaleString('zh-CN', {
    year: 'numeric', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit',
  })
}
</script>

<style scoped>
.choose-body { min-height: 180px; }
.step-tip { font-size: 13px; color: var(--muted); margin-bottom: 12px; line-height: 1.5; }
.step-tip b { color: var(--text); font-weight: 700; }
.search-input { margin-bottom: 12px; }
.session-list {
  max-height: 360px;
  overflow-y: auto;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--card);
}
.session-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 8px 12px;
  cursor: pointer;
  border-bottom: 1px solid var(--border);
  transition: background 0.15s;
}
.session-item:last-child { border-bottom: none; }
.session-item:hover { background: var(--bg-elevated); }
.session-item.active { background: var(--accent-dim); }
.item-radio { margin-right: 0; }
.item-radio :deep(.el-radio__label) { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.item-title { font-size: 13px; color: var(--text); }
.item-time { font-size: 11px; color: var(--muted); flex-shrink: 0; font-family: var(--font-mono); }
.list-footer-hint { text-align: center; font-size: 11px; color: var(--muted); padding: 8px 0; }
</style>
