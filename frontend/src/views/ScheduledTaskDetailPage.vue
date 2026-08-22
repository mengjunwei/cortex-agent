<template>
  <div class="page-root">
    <!-- 顶部：返回 + 任务信息 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left detail-header">
        <el-button @click="goBack"><el-icon><ArrowLeft /></el-icon> 返回</el-button>
        <span class="detail-title">{{ task ? task.name : '加载中…' }}</span>
        <el-tag v-if="task" size="small" :type="task.enabled ? 'success' : 'info'">
          {{ task.enabled ? '已启用' : '已停用' }}
        </el-tag>
        <el-tag v-if="task" size="small" type="info">{{ task.assistant_name || task.assistant_id }}</el-tag>
      </div>
      <div class="page-toolbar-right">
        <el-button size="small" @click="loadRuns" :loading="runsLoading">
          <el-icon><Refresh /></el-icon> 刷新
        </el-button>
      </div>
    </div>

    <!-- 任务概要 -->
    <div v-if="task" class="task-meta">
      <div class="meta-item">
        <span class="meta-label">调度</span>
        <code class="cron-text">{{ task.schedule_cron }}</code>
        <span class="cell-muted">（{{ task.timezone }}）</span>
      </div>
      <div class="meta-item">
        <span class="meta-label">执行内容</span>
        <span>{{ task.instruction }}</span>
      </div>
      <div class="meta-item">
        <span class="meta-label">下次运行</span>
        <span>{{ task.enabled && task.next_run_at ? fmtTime(task.next_run_at) : '—' }}</span>
      </div>
      <div class="meta-item">
        <span class="meta-label">保留</span>
        <span class="cell-muted">近 30 天运行记录</span>
      </div>
    </div>

    <!-- 运行记录表格 -->
    <el-table :data="runs" v-loading="runsLoading" empty-text="暂无运行记录" stripe style="width: 100%;">
      <el-table-column label="会话" min-width="240">
        <template #default="{ row }">
          <el-link type="primary" @click="openSession(row.session_id)">{{ row.title }}</el-link>
        </template>
      </el-table-column>
      <el-table-column label="触发方式" width="110" align="center">
        <template #default="{ row }">
          <el-tag v-if="row.trigger_kind === 'catchup'" size="small" type="warning">补跑</el-tag>
          <el-tag v-else-if="row.trigger_kind === 'manual'" size="small" type="info">手动</el-tag>
          <span v-else class="cell-muted">定时</span>
        </template>
      </el-table-column>
      <el-table-column label="运行时间" width="180">
        <template #default="{ row }">{{ fmtTime(row.updated_at) }}</template>
      </el-table-column>
    </el-table>

    <!-- 运行记录分页 -->
    <div class="page-pagination" v-if="total > 0">
      <el-pagination
        background
        layout="total, sizes, prev, pager, next"
        :total="total"
        :page-size="pageSize"
        :page-sizes="[10, 20, 50]"
        :current-page="page"
        @current-change="onPageChange"
        @size-change="onPageSizeChange"
      />
    </div>
  </div>
</template>

<script setup>
import { ref, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { ArrowLeft, Refresh } from '@element-plus/icons-vue'
import { fetchScheduledTask, fetchScheduledTaskRuns } from '../api'

const route = useRoute()
const router = useRouter()
const taskId = route.params.id

const task = ref(null)
const runs = ref([])
const runsLoading = ref(false)
const page = ref(1)
const pageSize = ref(10)
const total = ref(0)
// 运行记录请求序号（快速翻页时丢弃过期回包）
let loadRunsSeq = 0

onMounted(async () => {
  await Promise.all([loadTask(), loadRuns()])
})

async function loadTask() {
  try {
    const { data, code, message } = await fetchScheduledTask(taskId)
    if (code === 0) task.value = data
    else ElMessage.error(message || '加载任务失败')
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  }
}

async function loadRuns() {
  // 请求序号守卫：快速翻页时旧请求后到会覆盖新页数据，只认最后一次
  const seq = ++loadRunsSeq
  runsLoading.value = true
  try {
    const { data, code, message } = await fetchScheduledTaskRuns(taskId, page.value, pageSize.value)
    if (seq !== loadRunsSeq) return
    if (code === 0) {
      runs.value = (data && data.runs) || []
      total.value = (data && data.total) || 0
    } else {
      ElMessage.error(message || '加载运行记录失败')
    }
  } catch (e) {
    if (seq === loadRunsSeq) ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    if (seq === loadRunsSeq) runsLoading.value = false
  }
}

function onPageChange(p) { page.value = p; loadRuns() }
function onPageSizeChange(s) { pageSize.value = s; page.value = 1; loadRuns() }

function fmtTime(iso) {
  if (!iso) return '—'
  const d = new Date(iso)
  if (isNaN(d)) return iso
  const p = (n) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

function openSession(sessionId) {
  router.push({ path: '/chat', query: { session: sessionId } })
}

function goBack() { router.push('/scheduled-tasks') }
</script>

<style scoped>
.page-root { padding: 12px 16px; }
.page-toolbar { display: flex; justify-content: space-between; align-items: center; margin-bottom: 14px; }
.detail-header { display: flex; align-items: center; gap: 10px; }
.detail-title { font-size: 16px; font-weight: 700; color: var(--el-text-color-primary); }
.task-meta {
  display: flex; flex-direction: column; gap: 8px; padding: 14px 16px; margin-bottom: 14px;
  background: var(--el-bg-color-overlay, rgba(255,255,255,0.03)); border: 1px solid var(--el-border-color, rgba(255,255,255,0.08)); border-radius: 8px;
}
.meta-item { display: flex; gap: 10px; align-items: baseline; font-size: 13px; }
.meta-label { color: var(--el-text-color-secondary); min-width: 64px; flex-shrink: 0; }
.cron-text { font-family: var(--el-font-family-mono, monospace); font-size: 12px; }
.cell-muted { color: var(--el-text-color-secondary); }
.page-pagination { margin-top: 14px; display: flex; justify-content: flex-end; }
</style>
