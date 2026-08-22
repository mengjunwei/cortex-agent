<template>
  <div class="memory-page">
    <div class="page-header">
      <div class="page-title-row">
        <h2>我的记忆</h2>
        <el-button type="primary" @click="openCreate">+ 添加记忆</el-button>
      </div>
      <p class="page-desc">
        跨会话积累的习惯与避坑记录，每次对话自动带上（注入助手 system prompt）。助手级记忆仅在对应助手生效。
      </p>
    </div>

    <!-- 待确认建议 -->
    <el-collapse v-if="proposals.length" v-model="proposalCollapse" class="proposal-section">
      <el-collapse-item title="待确认的记忆建议（来自对话中助手的提议）" :name="1">
        <div v-for="p in proposals" :key="p.id" class="proposal-card">
          <div class="mp-top">
            <span class="mp-badge" :class="p.type === 1 ? 'pitfall' : 'preference'">
              {{ p.type === 1 ? '⚠️ 避坑' : '💡 习惯' }}
            </span>
            <span class="mp-scope">{{ scopeLabel(p.scope, p.assistant_id) }}</span>
            <el-tag v-if="userStore.user?.is_admin && p.owner" size="small" type="warning" effect="plain">{{ p.owner }}</el-tag>
            <span class="mp-time">{{ p.created_at }}</span>
          </div>
          <div class="mp-content">{{ p.content }}</div>
          <div v-if="p.reason" class="mp-reason"><span class="mp-reason-label">理由：</span>{{ p.reason }}</div>
          <div class="mp-actions">
            <el-button size="small" @click="rejectProposal(p)">忽略</el-button>
            <el-button size="small" type="primary" @click="acceptProposal(p)">加入记忆</el-button>
          </div>
        </div>
      </el-collapse-item>
    </el-collapse>

    <!-- 记忆列表 -->
    <div v-loading="loading" class="memory-list">
      <div v-for="m in memories" :key="m.id" class="memory-card">
        <div class="mp-top">
          <span class="mp-badge" :class="m.type === 1 ? 'pitfall' : 'preference'">
            {{ m.type === 1 ? '⚠️ 避坑' : '💡 习惯' }}
          </span>
          <span class="mp-scope">{{ scopeLabel(m.scope, m.assistant_id) }}</span>
          <el-tag v-if="userStore.user?.is_admin && m.owner" size="small" type="warning" effect="plain">{{ m.owner }}</el-tag>
          <span class="mp-time">{{ m.updated_at }}</span>
        </div>
        <div class="mp-content">{{ m.content }}</div>
        <div class="mp-actions">
          <el-button size="small" text @click="openEdit(m)">编辑</el-button>
          <el-button size="small" text type="danger" @click="removeMemory(m)">删除</el-button>
        </div>
      </div>
      <el-empty
        v-if="!loading && !memories.length"
        description="还没有记忆。对话中助手识别出值得长期记住的习惯/坑时会主动建议，你确认后就会出现在这里。"
      />
    </div>

    <!-- 新建/编辑弹窗 -->
    <el-dialog v-model="dialogVisible" :title="editing ? '编辑记忆' : '添加记忆'" width="520px">
      <el-form label-width="80px">
        <el-form-item label="类型">
          <el-radio-group v-model="form.type">
            <el-radio :value="0">💡 习惯</el-radio>
            <el-radio :value="1">⚠️ 坑</el-radio>
          </el-radio-group>
        </el-form-item>
        <el-form-item label="作用域">
          <!-- 编辑态锁定：后端更新接口只支持改内容/类型，作用域改动不会生效
               （改作用域请删除后重建），开放编辑只会误导用户以为改成功了 -->
          <el-radio-group v-model="form.scope" :disabled="!!editing">
            <el-radio :value="0">所有助手</el-radio>
            <el-radio :value="1">仅指定助手</el-radio>
          </el-radio-group>
        </el-form-item>
        <el-form-item v-if="form.scope === 1" label="助手">
          <el-select v-model="form.assistant_id" placeholder="选择该记忆绑定的助手" filterable style="width: 100%" :disabled="!!editing">
            <el-option v-for="a in customAssistants" :key="a.id" :label="a.name" :value="a.id" />
          </el-select>
        </el-form-item>
        <el-form-item label="内容">
          <el-input
            v-model="form.content"
            type="textarea"
            :rows="3"
            placeholder="一句陈述句，例如：用简体中文回复"
          />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="save">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { ref, computed, onMounted } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { useUserStore } from '../stores/user'
import {
  fetchMemories,
  fetchMemoryProposals,
  fetchAssistants,
  createMemory,
  updateMemory,
  deleteMemory,
  acceptMemoryProposal,
  rejectMemoryProposal,
} from '../api'

const memories = ref([])
const proposals = ref([])
const assistants = ref([])
const loading = ref(false)
const proposalCollapse = ref([1])
const userStore = useUserStore()

const dialogVisible = ref(false)
const editing = ref(null)
const saving = ref(false)
const form = ref({ type: 0, scope: 0, content: '', assistant_id: '' })

// 助手 id → name 映射
const assistantsMap = computed(() => {
  const m = {}
  for (const a of assistants.value) m[a.id] = a.name
  return m
})
// 只有自定义助手(kind=1)走 build_custom_agent 才注入记忆；助手级记忆只对自定义助手有意义
const customAssistants = computed(() => assistants.value.filter((a) => a.kind === 1))

// scope 标签：助手级显示绑定的助手名
function scopeLabel(scope, aid) {
  if (scope === 1) {
    const name = aid ? assistantsMap.value[aid] : ''
    return name ? `仅助手：${name}` : aid ? `仅助手：${aid.slice(0, 8)}` : '仅当前助手'
  }
  return '所有助手'
}

async function load() {
  loading.value = true
  try {
    const [mr, pr, ar] = await Promise.all([fetchMemories(), fetchMemoryProposals(), fetchAssistants()])
    if (mr.code === 0) memories.value = mr.data?.items || []
    if (pr.code === 0) proposals.value = pr.data?.items || []
    if (ar.code === 0) assistants.value = ar.data?.assistants || []
  } catch (e) {
    // gql 网络异常会 throw：不接住的话 loading 永远不复位（页面卡转圈）
    ElMessage.error('加载失败: ' + (e.message || '网络错误'))
  } finally {
    loading.value = false
  }
}

function openCreate() {
  editing.value = null
  form.value = { type: 0, scope: 0, content: '', assistant_id: '' }
  dialogVisible.value = true
}
function openEdit(m) {
  editing.value = m
  form.value = { type: m.type, scope: m.scope, content: m.content, assistant_id: m.assistant_id || '' }
  dialogVisible.value = true
}

async function save() {
  const f = form.value
  if (!f.content.trim()) {
    ElMessage.warning('内容不能为空')
    return
  }
  // 新建时助手级记忆必须选助手；编辑态后端不收 scope/assistant_id（控件已锁定），
  // 存量「scope=1 且未绑助手」的旧数据若同样拦截，会陷入既改不了绑定也存不了内容的死锁
  if (!editing.value && f.scope === 1 && !f.assistant_id) {
    ElMessage.warning('助手级记忆需要选择助手')
    return
  }
  saving.value = true
  try {
    // 编辑态后端只更新 content/type（scope/assistant_id 不生效，表单已锁定）
    const payload = editing.value
      ? { type: f.type, content: f.content.trim() }
      : { type: f.type, scope: f.scope, content: f.content.trim(), ...(f.scope === 1 ? { assistant_id: f.assistant_id } : {}) }
    const res = editing.value ? await updateMemory(editing.value.id, payload) : await createMemory(payload)
    if (res.code !== 0) {
      ElMessage.error(res.message || '保存失败')
      return
    }
    ElMessage.success('已保存')
    dialogVisible.value = false
    load()
  } catch (e) {
    ElMessage.error('保存失败: ' + (e.message || '网络错误'))
  } finally {
    saving.value = false
  }
}

async function removeMemory(m) {
  try {
    await ElMessageBox.confirm('确定删除这条记忆？', '提示', { type: 'warning' })
  } catch {
    return
  }
  try {
    const res = await deleteMemory(m.id)
    if (res.code !== 0) {
      ElMessage.error(res.message || '删除失败')
      return
    }
    ElMessage.success('已删除')
    load()
  } catch (e) {
    ElMessage.error('删除失败: ' + (e.message || '网络错误'))
  }
}

async function acceptProposal(p) {
  try {
    const res = await acceptMemoryProposal(p.id)
    if (res.code !== 0) {
      ElMessage.error(res.message || '操作失败')
      return
    }
    ElMessage.success('已加入记忆')
    load()
  } catch (e) {
    ElMessage.error('操作失败: ' + (e.message || '网络错误'))
  }
}
async function rejectProposal(p) {
  try {
    const res = await rejectMemoryProposal(p.id)
    if (res.code !== 0) {
      ElMessage.error(res.message || '操作失败')
      return
    }
    ElMessage.success('已忽略')
    load()
  } catch (e) {
    ElMessage.error('操作失败: ' + (e.message || '网络错误'))
  }
}

onMounted(load)
</script>

<style scoped>
.memory-page {
  padding: 20px;
  max-width: 820px;
  margin: 0 auto;
}
.page-header {
  margin-bottom: 16px;
}
.page-title-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.page-header h2 {
  margin: 0;
}
.page-desc {
  margin: 6px 0 0;
  color: var(--el-text-color-secondary);
  font-size: 13px;
}
.proposal-section {
  margin-bottom: 20px;
  border: 1px solid var(--el-color-warning-light-7);
  border-radius: 8px;
  padding: 0 12px;
  background: var(--el-color-warning-light-9);
}
.proposal-card,
.memory-card {
  border: 1px solid var(--el-border-color-light);
  border-radius: 8px;
  padding: 10px 12px;
  margin-bottom: 10px;
  background: var(--el-fill-color-light);
}
.mp-top {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
}
.mp-badge {
  font-size: 12px;
  font-weight: 600;
  padding: 2px 8px;
  border-radius: 10px;
}
.mp-badge.preference {
  background: #ecf5ff;
  color: #409eff;
}
.mp-badge.pitfall {
  background: #fdf6ec;
  color: #e6a23c;
}
.mp-scope {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.mp-time {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
  margin-left: auto;
}
.mp-content {
  font-size: 14px;
  line-height: 1.6;
}
.mp-reason {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin: 4px 0;
}
.mp-reason-label {
  font-weight: 600;
}
.mp-actions {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
  margin-top: 8px;
}
</style>
