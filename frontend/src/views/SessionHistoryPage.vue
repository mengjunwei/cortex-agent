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

        <el-button type="primary" size="small" class="btn-primary-action" @click="openNewSession">
          <el-icon><Plus /></el-icon> 新建会话
        </el-button>
      </div>
    </div>

    <!-- 新建会话对话框：会话名称 + 助手下拉选择 -->
    <el-dialog
      v-model="newSessionDialogOpen"
      title="新建会话"
      width="460px"
      :close-on-click-modal="false"
      align-center
      class="new-session-dialog"
      @opened="onDialogOpened"
    >
      <el-form label-position="top" class="ns-form">
        <el-form-item label="会话名称">
          <el-input
            v-model="newSessionTitle"
            placeholder="留空则自动生成（如：新会话 8/13 14:30）"
            clearable
            maxlength="60"
            show-word-limit
          />
        </el-form-item>

        <el-form-item label="助手">
          <el-select
            v-model="selectedAssistantId"
            placeholder="选择一个助手开始对话"
            filterable
            fit-input-width
            popper-class="asst-select-popper"
            style="width: 100%;"
          >
            <template #prefix>
              <span v-if="selectedAssistant" class="ns-select-avatar">
                {{ selectedAssistant.avatar || '🤖' }}
              </span>
            </template>
            <el-option
              v-for="a in assistantStore.assistants"
              :key="a.id"
              :value="a.id"
              :label="a.name"
            >
              <div class="asst-option">
                <span class="asst-avatar">{{ a.avatar || '🤖' }}</span>
                <div class="asst-option-body">
                  <div class="asst-option-head">
                    <span class="asst-option-name">{{ a.name }}</span>
                    <el-tag size="small" :type="a.kind === 1 ? 'success' : 'info'" effect="plain">
                      {{ a.kind === 1 ? '自定义' : '内置' }}
                    </el-tag>
                  </div>
                  <div class="asst-option-desc">{{ a.description || '暂无描述' }}</div>
                </div>
              </div>
            </el-option>
          </el-select>
        </el-form-item>
      </el-form>

      <template #footer>
        <el-button @click="newSessionDialogOpen = false">取消</el-button>
        <el-button
          type="primary"
          :icon="Plus"
          :loading="creating"
          :disabled="!selectedAssistantId"
          @click="onCreateSession"
        >
          创建会话
        </el-button>
      </template>
    </el-dialog>

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

        <el-table-column prop="title" label="标题" min-width="240" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="cell-title">{{ row.title || '新会话' }}</span>
          </template>
        </el-table-column>

        <el-table-column label="来源" width="80">
          <template #default="{ row }">
            <el-tag v-if="row.assistant_kind != null" size="small" :type="row.assistant_kind === 1 ? 'success' : ''" effect="plain">
              {{ row.assistant_kind === 1 ? '自定义' : '内置' }}
            </el-tag>
            <span v-else class="cell-muted">-</span>
          </template>
        </el-table-column>

        <el-table-column label="助手" min-width="120">
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

        <el-table-column label="创建时间" width="170" prop="created_at">
          <template #default="{ row }">
            <span class="cell-muted">{{ formatTime(row.created_at) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="更新时间" width="170" prop="updated_at">
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
                @confirm="onDeleteSession(row)"
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
import { ref, computed, onMounted, watch } from 'vue'
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
// 新建会话对话框：名称（可选）+ 助手下拉选择
const newSessionDialogOpen = ref(false)
const newSessionTitle = ref('')
const selectedAssistantId = ref(null)
const creating = ref(false)
// 当前选中的助手对象（供下拉框 prefix 头像展示）
const selectedAssistant = computed(
  () => assistantStore.assistants.find((a) => a.id === selectedAssistantId.value) || null
)

// ── 筛选/分页状态同步到 URL ──
function restoreFromQuery() {
  const q = route.query
  if (q.kw) {
    sessionStore.filterKeyword = String(q.kw)
    sessionStore.appliedKeyword = String(q.kw)
  }
  if (q.kind !== undefined && q.kind !== '') sessionStore.filterKind = Number(q.kind)
  if (q.page) sessionStore.currentPage = parseInt(q.page, 10) || 1
}

function syncToQuery() {
  const query = {}
  // URL 记录已提交的筛选（草稿值不该进 URL：刷新会把它变成生效筛选）
  if (sessionStore.appliedKeyword) query.kw = sessionStore.appliedKeyword
  if (sessionStore.filterKind !== null) query.kind = sessionStore.filterKind
  if (sessionStore.currentPage !== 1) query.page = sessionStore.currentPage
  router.replace({ path: '/sessions', query })
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
    // 搜索提交点：把输入框草稿升级为已提交关键词（翻页/刷新只认已提交值）
    await sessionStore.loadSessions(1, { keyword: sessionStore.filterKeyword })
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

function openNewSession() {
  // 预选首个助手，避免每次都要手动选；保留上次选择
  if (!selectedAssistantId.value && assistantStore.assistants.length) {
    selectedAssistantId.value = assistantStore.assistants[0].id
  }
  newSessionDialogOpen.value = true
}

function onDialogOpened() {
  // 助手列表若未加载则补拉
  if (!assistantStore.assistants.length) assistantStore.loadAssistants()
}

async function onCreateSession() {
  const assistant = assistantStore.assistants.find((a) => a.id === selectedAssistantId.value)
  if (!assistant?.id) return
  creating.value = true
  try {
    // 用户可指定名称；留空则传 null，由 store 自动生成默认标题
    const title = newSessionTitle.value.trim() || null
    const id = await sessionStore.createNewSession(assistant.agent_type_key, title, assistant.id)
    newSessionDialogOpen.value = false
    newSessionTitle.value = ''
    // 记住列表页完整 URL（含筛选 query），供会话页返回时恢复
    sessionStorage.setItem('sessions_list_url', route.fullPath)
    chatStore.loadModelForSession(null)
    await refresh()
    router.push({ path: '/chat', query: { session: id } })
  } catch (e) {
    // createNewSession 失败（业务错误/网络异常）会抛错：提示用户而不是静默无反应
    ElMessage.error(e.message || '创建会话失败')
  } finally {
    creating.value = false
  }
}

// 快速连点两个会话时两次 selectSession 并发：慢的旧回包后到会把模型下拉改回
// 旧会话绑定的模型（下一次发消息就以错模型发给新会话），序号保证只有最新者落地
let selectSessionSeq = 0

async function onSelectSession(s) {
  const seq = ++selectSessionSeq
  // selectSession 内部调用 loadHistoryMessages，后者返回后端绑定的 model_id
  const boundModelId = await sessionStore.selectSession(s.id, s.agent_type)
  if (seq !== selectSessionSeq) return // 用户已点了别的会话，丢弃过期回包
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
  let value
  try {
    ;({ value } = await ElMessageBox.prompt('输入新名称', '重命名会话', {
      inputValue: s.title || '',
      confirmButtonText: '保存',
      cancelButtonText: '取消',
      inputPattern: /\S/,
      inputErrorMessage: '名称不能为空',
    }))
  } catch (_) {
    return // 取消（校验失败由 MessageBox 自行拦截，不会落到这）
  }
  try {
    await sessionStore.renameSessionById(s.id, value.trim())
  } catch (e) {
    // 删除/重命名失败必须可见：旧版 store 静默吞掉，行不动且无任何提示
    ElMessage.error(e.message || '重命名失败')
  }
}

async function onDeleteSession(row) {
  try {
    await sessionStore.deleteSessionById(row.id)
  } catch (e) {
    ElMessage.error(e.message || '删除失败')
  }
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
})

// 筛选/分页变化时同步到 URL
watch(
  () => [sessionStore.filterKeyword, sessionStore.filterKind, sessionStore.currentPage],
  () => syncToQuery()
)
</script>

<style scoped>
/* ── 新建会话对话框 ── */
.ns-form {
  padding-top: 4px;
}
.ns-form :deep(.el-form-item) {
  margin-bottom: 18px;
}
.ns-form :deep(.el-form-item__label) {
  font-weight: 600;
  color: var(--text, #1c1c1e);
  padding-bottom: 4px;
  line-height: 1.5;
}
/* 选中态：下拉输入框内显示的助手头像（#prefix slot） */
.ns-select-avatar {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  font-size: 15px;
  margin-right: 4px;
}
/* 下拉选项：头像 + 名称/来源标签 + 描述 */
.asst-option {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 0;
}
.asst-avatar {
  width: 30px;
  height: 30px;
  border-radius: 8px;
  background: linear-gradient(135deg, rgba(0, 212, 255, 0.12), rgba(14, 165, 233, 0.06));
  border: 1px solid rgba(0, 212, 255, 0.18);
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 17px;
  flex-shrink: 0;
}
.asst-option-body {
  min-width: 0;
  flex: 1;
}
.asst-option-head {
  display: flex;
  align-items: center;
  gap: 6px;
}
.asst-option-name {
  font-size: 13.5px;
  font-weight: 600;
  color: var(--text, #1c1c1e);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.asst-option-desc {
  font-size: 12px;
  color: var(--muted, #8e8e93);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
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
