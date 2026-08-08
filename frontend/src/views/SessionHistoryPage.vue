<template>
  <div class="page-root">
    <!-- 顶部操作栏 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <el-input
          v-model="sessionStore.filterKeyword"
          placeholder="搜索会话标题"
          clearable
          size="small"
          style="width: 260px;"
          @keyup.enter="onFilterChange"
          @clear="onFilterChange"
        >
          <template #prefix>
            <el-icon><Search /></el-icon>
          </template>
        </el-input>
        <el-button size="small" @click="onFilterChange">
          <el-icon><Search /></el-icon> 搜索
        </el-button>
        <el-button size="small" text @click="resetFilter">
          <el-icon><Refresh /></el-icon> 重置
        </el-button>
      </div>

      <div class="page-toolbar-right">
        <el-select
          v-model="sessionStore.filterKind"
          placeholder="来源筛选"
          clearable
          size="small"
          style="width: 130px;"
          @change="onFilterChange"
        >
          <el-option label="内置" :value="0" />
          <el-option label="自定义" :value="1" />
        </el-select>

        <el-tooltip content="选择助手创建新对话" placement="bottom" :show-after="400">
          <el-button type="primary" size="small" class="btn-primary-action" @click.stop="dropdownOpen = !dropdownOpen">
            <el-icon><Plus /></el-icon> 新建会话
          </el-button>
        </el-tooltip>
      </div>
    </div>

    <!-- 新建会话下拉面板 -->
    <transition name="dropdown">
      <div v-if="dropdownOpen" class="session-dropdown" v-click-outside="closeDropdown">
        <div class="dropdown-title">选择助手</div>
        <div class="dropdown-list">
          <div v-if="assistantStore.assistants.length === 0" class="dropdown-empty">
            暂无可用助手
          </div>
          <div
            v-for="a in assistantStore.assistants"
            :key="a.id"
            class="dropdown-item"
            @click="onNewSession(a)"
          >
            <div class="dropdown-item-icon">
              {{ a.avatar || '🤖' }}
            </div>
            <div class="dropdown-item-body">
              <div class="dropdown-item-title">
                {{ a.name }}
                <el-tag size="small" :type="a.kind === 1 ? 'success' : ''" effect="plain" class="dropdown-item-tag">
                  {{ a.kind === 1 ? '自定义' : '内置' }}
                </el-tag>
              </div>
              <div class="dropdown-item-desc">{{ a.description || '暂无描述' }}</div>
            </div>
          </div>
        </div>
      </div>
    </transition>

    <!-- 表格 -->
    <div class="data-table-wrapper" v-loading="loading">
      <el-table
        class="data-table"
        :data="sessionStore.sessions"
        row-key="id"
        height="100%"
        stripe
        highlight-current-row
        :row-class-name="rowClassName"
        @row-click="onSelectSession"
      >
        <template #empty>
          <div class="table-empty">
            <div class="empty-icon">💬</div>
            <div class="empty-title">暂无会话记录</div>
            <div class="empty-hint">点击上方「新建会话」开始对话</div>
          </div>
        </template>

        <el-table-column prop="title" label="标题" min-width="240" sortable show-overflow-tooltip>
          <template #default="{ row }">
            <span class="cell-title">{{ row.title || '新会话' }}</span>
          </template>
        </el-table-column>

        <el-table-column label="来源" width="80" sortable :sort-method="sortByKind">
          <template #default="{ row }">
            <el-tag v-if="row.assistant_kind != null" size="small" :type="row.assistant_kind === 1 ? 'success' : ''" effect="plain">
              {{ row.assistant_kind === 1 ? '自定义' : '内置' }}
            </el-tag>
            <span v-else class="cell-muted">-</span>
          </template>
        </el-table-column>

        <el-table-column label="助手" min-width="120" sortable :sort-method="sortByAssistantName">
          <template #default="{ row }">
            <span v-if="row.assistant_name">{{ row.assistant_name }}</span>
            <span v-else class="cell-muted">-</span>
          </template>
        </el-table-column>

        <!-- 归属列：仅管理员可见（管理员能看到所有用户的会话，需标明是谁的） -->
        <el-table-column v-if="userStore.user?.is_admin" label="归属" width="120">
          <template #default="{ row }">
            <el-tag v-if="row.owner" size="small" type="warning" effect="plain">{{ row.owner }}</el-tag>
            <span v-else class="cell-muted">-</span>
          </template>
        </el-table-column>

        <el-table-column label="创建时间" width="170" sortable prop="created_at">
          <template #default="{ row }">
            <span class="cell-muted">{{ formatTime(row.created_at) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="更新时间" width="170" sortable prop="updated_at">
          <template #default="{ row }">
            <span class="cell-muted">{{ formatTime(row.updated_at) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="操作" width="180" align="center" fixed="right">
          <template #default="{ row }">
            <div class="row-actions" @click.stop>
              <el-button size="small" text type="primary" @click="onRename(row)">重命名</el-button>
              <el-popconfirm
                title="确定删除此会话？"
                confirm-button-text="删除"
                cancel-button-text="取消"
                @confirm="sessionStore.deleteSessionById(row.id)"
              >
                <template #reference>
                  <el-button size="small" type="danger" plain>删除</el-button>
                </template>
              </el-popconfirm>
            </div>
          </template>
        </el-table-column>
      </el-table>
    </div>

    <!-- 分页 -->
    <div class="page-pagination" v-if="sessionStore.totalCount > 0">
      <span class="page-total">共 {{ sessionStore.totalCount }} 条</span>
      <el-pagination
        background
        layout="sizes, prev, pager, next, jumper"
        :total="sessionStore.totalCount"
        :page-size="sessionStore.pageSize"
        :page-sizes="[10, 20, 50]"
        :current-page="sessionStore.currentPage"
        @current-change="onPageChange"
        @size-change="onSizeChange"
      />
    </div>
  </div>
</template>

<script setup>
import { ref, onMounted, onBeforeUnmount, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessageBox, ElMessage } from 'element-plus'
import { useSessionStore } from '../stores/session'
import { useChatStore } from '../stores/chat'
import { useAssistantStore } from '../stores/assistant'
import { useUserStore } from '../stores/user'
import {
  Refresh, Search, Plus,
} from '@element-plus/icons-vue'

const route = useRoute()
const router = useRouter()
const sessionStore = useSessionStore()
const chatStore = useChatStore()
const userStore = useUserStore()
const assistantStore = useAssistantStore()

const loading = ref(false)
const dropdownOpen = ref(false)

// ── 筛选/分页状态同步到 URL ──
function restoreFromQuery() {
  const q = route.query
  if (q.kw) sessionStore.filterKeyword = String(q.kw)
  if (q.kind !== undefined && q.kind !== '') sessionStore.filterKind = Number(q.kind)
  if (q.page) sessionStore.currentPage = parseInt(q.page, 10) || 1
}

function syncToQuery() {
  const query = {}
  if (sessionStore.filterKeyword) query.kw = sessionStore.filterKeyword
  if (sessionStore.filterKind !== null) query.kind = sessionStore.filterKind
  if (sessionStore.currentPage !== 1) query.page = sessionStore.currentPage
  router.replace({ path: '/sessions', query })
}

function sortByKind(a, b) {
  const ka = a.assistant_kind ?? 999
  const kb = b.assistant_kind ?? 999
  if (ka !== kb) return ka - kb
  return (a.assistant_name || '').localeCompare(b.assistant_name || '')
}

function sortByAssistantName(a, b) {
  return (a.assistant_name || '').localeCompare(b.assistant_name || '')
}

function formatTime(iso) {
  if (!iso) return ''
  const d = new Date(iso)
  const now = new Date()
  if (d.toDateString() === now.toDateString()) {
    return '今天 ' + d.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })
  }
  return d.toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
}

async function refresh() {
  loading.value = true
  try {
    await sessionStore.loadSessions(sessionStore.currentPage)
  } finally {
    loading.value = false
  }
}

function onFilterChange() {
  refreshPage1()
}

function resetFilter() {
  sessionStore.filterKeyword = ''
  sessionStore.filterKind = null
  refreshPage1()
}

async function refreshPage1() {
  loading.value = true
  try {
    await sessionStore.loadSessions(1)
  } finally {
    loading.value = false
  }
}

async function onPageChange(page) {
  loading.value = true
  try {
    await sessionStore.loadSessions(page)
  } finally {
    loading.value = false
  }
}

async function onSizeChange(size) {
  sessionStore.pageSize = size
  loading.value = true
  try {
    await sessionStore.loadSessions(1)
  } finally {
    loading.value = false
  }
}

async function onNewSession(assistant) {
  dropdownOpen.value = false
  if (!assistant?.id) return
  const id = await sessionStore.createNewSession(assistant.agent_type_key, null, assistant.id)
  if (id) {
    // 记住列表页完整 URL（含筛选 query），供会话页返回时恢复
    sessionStorage.setItem('sessions_list_url', route.fullPath)
    chatStore.loadModelForSession(null)
    await refresh()
    router.push({ path: '/chat', query: { session: id } })
  }
}

async function onSelectSession(s) {
  // selectSession 内部调用 loadHistoryMessages，后者返回后端绑定的 model_id
  const boundModelId = await sessionStore.selectSession(s.id, s.agent_type)
  // list 里也可能带了 model_id，优先用 history 返回的
  const modelId = boundModelId || s.model_id || null
  chatStore.loadModelForSession(modelId)
  // 记住列表页完整 URL（含筛选 query），供会话页返回时恢复
  sessionStorage.setItem('sessions_list_url', route.fullPath)
  router.push({ path: '/chat', query: { session: s.id } })
}

function rowClassName({ row }) {
  return row.id === sessionStore.currentSessionId ? 'current-row' : ''
}

async function onRename(s) {
  try {
    const { value } = await ElMessageBox.prompt('输入新名称', '重命名会话', {
      inputValue: s.title || '',
      confirmButtonText: '保存',
      cancelButtonText: '取消',
    })
    if (value?.trim()) await sessionStore.renameSessionById(s.id, value.trim())
  } catch (_) {}
}

function closeDropdown() {
  dropdownOpen.value = false
}

function onKeydown(e) {
  if (e.key === 'Escape') dropdownOpen.value = false
}

onMounted(async () => {
  restoreFromQuery()
  loading.value = true
  try {
    await Promise.all([
      sessionStore.loadSessions(sessionStore.currentPage || 1),
      assistantStore.loadAssistants(),
    ])
  } finally {
    loading.value = false
  }
  document.addEventListener('keydown', onKeydown)
})

// 筛选/分页变化时同步到 URL
watch(
  () => [sessionStore.filterKeyword, sessionStore.filterKind, sessionStore.currentPage],
  () => syncToQuery()
)

onBeforeUnmount(() => {
  document.removeEventListener('keydown', onKeydown)
})
</script>

<script>
export default {
  directives: {
    clickOutside: {
      mounted(el, binding) {
        el._clickOutside = (e) => {
          if (!el.contains(e.target)) binding.value()
        }
        document.addEventListener('click', el._clickOutside)
      },
      unmounted(el) {
        document.removeEventListener('click', el._clickOutside)
      },
    },
  },
}
</script>

<style scoped>
/* 下拉面板（浅灰分层，与深色页面区分） */
.session-dropdown {
  position: absolute;
  top: 66px;
  right: 24px;
  z-index: 2000;
  width: 340px;
  background: #f5f5f7;
  border: 1px solid #e5e5e8;
  border-radius: 16px;
  padding: 16px;
  box-shadow: 0 12px 40px rgba(0, 0, 0, 0.22);
}
.dropdown-title {
  font-size: 11px;
  font-weight: 800;
  color: #8e8e93;
  text-transform: uppercase;
  letter-spacing: 0.8px;
  margin-bottom: 10px;
  padding: 0 4px;
}
.dropdown-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.dropdown-empty {
  padding: 20px 14px;
  text-align: center;
  font-size: 13px;
  color: #8e8e93;
}
.dropdown-item {
  display: flex;
  align-items: flex-start;
  gap: 12px;
  padding: 12px 14px;
  border-radius: 12px;
  cursor: pointer;
  transition: all 0.2s ease;
  background: #fff;
  border: 1px solid transparent;
}
.dropdown-item:hover {
  border-color: #d1d1d6;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.06);
  transform: translateY(-1px);
}
.dropdown-item-icon {
  width: 36px;
  height: 36px;
  border-radius: 10px;
  background: linear-gradient(135deg, rgba(0, 212, 255, 0.1) 0%, rgba(14, 165, 233, 0.06) 100%);
  border: 1px solid rgba(0, 212, 255, 0.15);
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  font-size: 20px;
  line-height: 1;
}
.dropdown-item-title {
  font-size: 14px;
  font-weight: 700;
  color: #1c1c1e;
  line-height: 1.3;
  display: flex;
  align-items: center;
  gap: 6px;
}
.dropdown-item-tag {
  transform: scale(0.85);
  transform-origin: left center;
}
.dropdown-item-desc {
  font-size: 12px;
  color: #8e8e93;
  line-height: 1.4;
}
.dropdown-enter-active,
.dropdown-leave-active {
  transition: all 0.2s cubic-bezier(0.4, 0, 0.2, 1);
}
.dropdown-enter-from,
.dropdown-leave-to {
  opacity: 0;
  transform: translateY(-6px) scale(0.98);
}

/* 分页左侧总条数 */
.page-total {
  font-size: 13px;
  color: var(--muted);
  font-weight: 500;
}

/* 当前行高亮 */
:deep(.el-table .current-row > td) {
  background: rgba(0, 212, 255, 0.08) !important;
}
:deep(.el-table__row) {
  cursor: pointer;
}
</style>
