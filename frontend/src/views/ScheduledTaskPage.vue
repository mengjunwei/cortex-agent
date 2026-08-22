<template>
  <div class="page-root">
    <!-- 顶部操作栏 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <span class="toolbar-title">定时任务</span>
      </div>
      <div class="page-toolbar-right">
        <el-button type="primary" size="small" @click="openCreate">
          <el-icon><Plus /></el-icon> 新建任务
        </el-button>
        <el-button size="small" @click="loadTasks" :loading="loading">
          <el-icon><Refresh /></el-icon> 刷新
        </el-button>
      </div>
    </div>

    <!-- 说明条 -->
    <div class="info-banner">
      <el-icon class="info-icon"><InfoFilled /></el-icon>
      <span>
        定时任务基于某个助手按周期自动执行（如「每天出一份报表」），结果生成独立会话，
        点击任务行的「运行记录」查看。每个任务仅保留最近 30 天的运行记录。
      </span>
    </div>

    <!-- 任务列表 -->
    <el-table :data="tasks" v-loading="loading" empty-text="暂无定时任务" stripe style="width: 100%;">
      <el-table-column label="任务名" min-width="160">
        <template #default="{ row }">
          <div class="task-name">{{ row.name }}</div>
          <div class="task-assistant cell-muted">助手：{{ row.assistant_name || row.assistant_id }}</div>
        </template>
      </el-table-column>
      <el-table-column label="调度" min-width="150">
        <template #default="{ row }">
          <div>{{ cronHuman(row) }}</div>
          <code class="cron-text">{{ row.schedule_cron }}</code>
        </template>
      </el-table-column>
      <el-table-column label="下次运行" min-width="150">
        <template #default="{ row }">
          <span v-if="row.enabled && row.next_run_at">{{ fmtTime(row.next_run_at) }}</span>
          <span v-else class="cell-muted">—</span>
        </template>
      </el-table-column>
      <el-table-column label="最近运行" min-width="150">
        <template #default="{ row }">
          <div v-if="row.last_run_at">
            <el-tag :type="statusType(row.last_run_status)" size="small">{{ statusText(row.last_run_status) }}</el-tag>
            <div class="cell-muted" style="font-size:12px;margin-top:2px">{{ fmtTime(row.last_run_at) }}</div>
          </div>
          <span v-else class="cell-muted">未运行</span>
        </template>
      </el-table-column>
      <el-table-column label="启用" width="80" align="center">
        <template #default="{ row }">
          <el-switch
            :model-value="row.enabled"
            :loading="togglingIds.has(row.id)"
            @change="(v) => handleToggle(row, v)"
          />
        </template>
      </el-table-column>
      <el-table-column label="操作" width="220" align="center">
        <template #default="{ row }">
          <el-button size="small" text type="primary" @click="openDetail(row)">运行记录</el-button>
          <el-button size="small" text type="primary" :disabled="!row.enabled" :loading="runningIds.has(row.id)" @click="handleRunNow(row)">立即运行</el-button>
          <el-button size="small" text type="primary" @click="openEdit(row)">编辑</el-button>
          <el-button size="small" text type="danger" :loading="deletingIds.has(row.id)" @click="handleDelete(row)">删除</el-button>
        </template>
      </el-table-column>
    </el-table>

    <!-- 任务列表分页 -->
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

    <!-- 创建 / 编辑对话框 -->
    <el-dialog
      v-model="dialogVisible"
      :title="editingId ? '编辑定时任务' : '新建定时任务'"
      width="560px"
      :close-on-click-modal="false"
    >
      <el-form label-width="92px" label-position="left">
        <el-form-item label="任务名" required>
          <el-input v-model="form.name" placeholder="如：每日销售报表" maxlength="60" />
        </el-form-item>
        <el-form-item label="助手" required>
          <el-select v-model="form.assistant_id" placeholder="选择执行任务的助手" style="width:100%" filterable>
            <el-option v-for="a in assistants" :key="a.id" :label="a.name" :value="a.id" />
          </el-select>
        </el-form-item>
        <el-form-item label="执行内容" required>
          <el-input
            v-model="form.instruction"
            type="textarea"
            :rows="3"
            placeholder="每次触发要助手做什么，如：汇总昨天的销售数据并生成报表保存到工作区"
          />
        </el-form-item>
        <el-form-item label="调度" required>
          <el-input
            v-model="form.schedule_nl"
            placeholder="用大白话描述，如：每天早上9点 / 每5分钟 / 每周一上午8点半"
            @input="scheduleDirty = true"
          >
            <template #append>
              <el-button @click="handleParse" :loading="parsing">解析</el-button>
            </template>
          </el-input>
          <div v-if="form.schedule_cron" class="cron-preview">
            <code>{{ form.schedule_cron }}</code>
            <span class="cell-muted">{{ parsedHuman }}</span>
            <div v-if="parsedNext.length" class="next-list">
              <div v-for="(t, i) in parsedNext" :key="i" class="cell-muted">下次{{ i + 1 }}：{{ fmtTime(t) }}</div>
            </div>
          </div>
        </el-form-item>
        <el-form-item label="时区">
          <el-input v-model="form.timezone" placeholder="Asia/Shanghai" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" :disabled="!form.schedule_cron" @click="handleSave">
          {{ editingId ? '保存' : '创建' }}
        </el-button>
      </template>
    </el-dialog>

  </div>
</template>

<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Plus, Refresh, InfoFilled } from '@element-plus/icons-vue'
import {
  fetchScheduledTasks,
  createScheduledTask,
  updateScheduledTask,
  deleteScheduledTask,
  runScheduledTaskNow,
  parseSchedule,
  fetchAssistants,
} from '../api'

const router = useRouter()
const tasks = ref([])
const assistants = ref([])
const loading = ref(false)
const togglingIds = ref(new Set())
const runningIds = ref(new Set())
const deletingIds = ref(new Set())

// 分页
const page = ref(1)
const pageSize = ref(10)
// 列表请求序号（快速翻页时丢弃过期回包）
let loadTasksSeq = 0
// 立即运行后的延迟刷新定时器（组件卸载时清理，防幽灵请求/跨页 toast）
let refreshTimer = null
const total = ref(0)

// 创建/编辑
const dialogVisible = ref(false)
const editingId = ref(null)
const saving = ref(false)
const parsing = ref(false)
const scheduleDirty = ref(false)
const parsedHuman = ref('')
const parsedNext = ref([])
const form = ref(emptyForm())

function emptyForm() {
  return { name: '', assistant_id: '', instruction: '', schedule_nl: '', schedule_cron: '', timezone: 'Asia/Shanghai' }
}

onMounted(async () => {
  await Promise.all([loadTasks(), loadAssistants()])
})

onUnmounted(() => {
  if (refreshTimer) { clearTimeout(refreshTimer); refreshTimer = null }
})

async function loadTasks() {
  // 请求序号守卫：快速翻页时旧请求后到会覆盖新页数据，只认最后一次
  const seq = ++loadTasksSeq
  loading.value = true
  try {
    const { data, code, message } = await fetchScheduledTasks(page.value, pageSize.value)
    if (seq !== loadTasksSeq) return
    if (code === 0) {
      tasks.value = (data && data.tasks) || []
      total.value = (data && data.total) || 0
    } else {
      ElMessage.error(message || '加载任务失败')
    }
  } catch (e) {
    if (seq === loadTasksSeq) ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    if (seq === loadTasksSeq) loading.value = false
  }
}

function onPageChange(p) { page.value = p; loadTasks() }
function onPageSizeChange(s) { pageSize.value = s; page.value = 1; loadTasks() }

async function loadAssistants() {
  try {
    const { data, code } = await fetchAssistants()
    if (code === 0) {
      assistants.value = (data && data.assistants) || data || []
    }
  } catch { /* 助手列表加载失败不阻塞 */ }
}

function fmtTime(iso) {
  if (!iso) return '—'
  const d = new Date(iso)
  if (isNaN(d)) return iso
  const p = (n) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

function cronHuman(row) {
  return row._human || row.schedule_cron
}

function statusType(s) {
  return s === 0 ? 'success' : s === 2 ? 'warning' : 'danger'
}
function statusText(s) {
  return s === 0 ? '成功' : s === 2 ? '超时' : s === 1 ? '失败' : '—'
}

function openCreate() {
  editingId.value = null
  form.value = emptyForm()
  parsedHuman.value = ''
  parsedNext.value = []
  scheduleDirty.value = false
  dialogVisible.value = true
}

function openEdit(row) {
  editingId.value = row.id
  form.value = {
    name: row.name,
    assistant_id: row.assistant_id,
    instruction: row.instruction,
    schedule_nl: '',
    schedule_cron: row.schedule_cron,
    timezone: row.timezone || 'Asia/Shanghai',
  }
  parsedHuman.value = ''
  parsedNext.value = []
  scheduleDirty.value = false
  dialogVisible.value = true
}

async function handleParse() {
  if (!form.value.schedule_nl.trim()) {
    ElMessage.warning('请先用大白话描述调度，如「每天早上9点」')
    return
  }
  parsing.value = true
  try {
    const { data, code, message } = await parseSchedule(form.value.schedule_nl.trim(), form.value.timezone)
    if (code === 0 && data && data.cron) {
      form.value.schedule_cron = data.cron
      parsedHuman.value = data.human || ''
      parsedNext.value = data.next_runs || []
      scheduleDirty.value = false
      ElMessage.success('已解析为 cron 表达式')
    } else {
      ElMessage.error(message || '无法识别该调度描述')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    parsing.value = false
  }
}

async function handleSave() {
  if (!form.value.name.trim() || !form.value.assistant_id || !form.value.instruction.trim()) {
    ElMessage.warning('请填写任务名、助手与执行内容')
    return
  }
  if (!form.value.schedule_cron) {
    ElMessage.warning('请先点「解析」把调度描述转成 cron')
    return
  }
  saving.value = true
  try {
    const payload = {
      assistant_id: form.value.assistant_id,
      name: form.value.name.trim(),
      instruction: form.value.instruction.trim(),
      schedule_cron: form.value.schedule_cron,
      timezone: form.value.timezone || 'Asia/Shanghai',
    }
    const { code, message } = editingId.value
      ? await updateScheduledTask(editingId.value, payload)
      : await createScheduledTask(payload)
    if (code === 0) {
      ElMessage.success(editingId.value ? '已保存' : '已创建')
      dialogVisible.value = false
      await loadTasks()
    } else {
      ElMessage.error(message || '保存失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    saving.value = false
  }
}

async function handleToggle(row, val) {
  togglingIds.value = new Set(togglingIds.value).add(row.id)
  try {
    const { code, message } = await updateScheduledTask(row.id, { enabled: val })
    if (code === 0) {
      row.enabled = val
      ElMessage.success(val ? `已启用「${row.name}」` : `已停用「${row.name}」`)
      await loadTasks()
    } else {
      ElMessage.error(message || '操作失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    const s = new Set(togglingIds.value); s.delete(row.id); togglingIds.value = s
  }
}

async function handleRunNow(row) {
  runningIds.value = new Set(runningIds.value).add(row.id)
  try {
    const { code, message } = await runScheduledTaskNow(row.id)
    if (code === 0) {
      ElMessage.success(`已触发「${row.name}」立即运行，稍后点「运行记录」查看`)
      if (refreshTimer) clearTimeout(refreshTimer)
      refreshTimer = setTimeout(() => { refreshTimer = null; loadTasks() }, 1500)
    } else {
      ElMessage.error(message || '触发失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    const s = new Set(runningIds.value); s.delete(row.id); runningIds.value = s
  }
}

async function handleDelete(row) {
  try {
    await ElMessageBox.confirm(
      `确定删除定时任务「${row.name}」？历史运行会话会保留，但不再自动执行。`,
      '删除确认',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  deletingIds.value = new Set(deletingIds.value).add(row.id)
  try {
    const { code, message } = await deleteScheduledTask(row.id)
    if (code === 0) {
      ElMessage.success('已删除')
      await loadTasks()
    } else {
      ElMessage.error(message || '删除失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    const s = new Set(deletingIds.value); s.delete(row.id); deletingIds.value = s
  }
}

// 运行记录 → 跳独立详情页
function openDetail(row) {
  router.push({ path: `/scheduled-tasks/${row.id}` })
}
</script>

<style scoped>
.page-root { padding: 12px 16px; }
.task-name { font-weight: 600; }
.task-assistant { font-size: 12px; }
.cron-text { font-size: 12px; opacity: 0.75; }
.cron-preview { margin-top: 6px; display: flex; flex-direction: column; gap: 2px; }
.next-list { margin-top: 2px; }
.cell-muted { color: var(--el-text-color-secondary); }
.page-pagination { margin-top: 14px; display: flex; justify-content: flex-end; }
</style>
