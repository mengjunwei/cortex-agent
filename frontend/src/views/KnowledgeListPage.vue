<template>
  <div class="page-root">
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <span class="kb-hint">支持 Dify（外挂）与内置（Qdrant 向量库）两种类型，可创建多个。点击「文档管理」进入该知识库的文档列表。</span>
      </div>
      <div class="page-toolbar-right">
        <el-button type="primary" @click="startCreate"><el-icon><Plus /></el-icon> 新建知识库</el-button>
      </div>
    </div>

    <div class="data-table-wrapper" v-loading="loading">
      <el-table class="data-table" :data="instances" stripe border>
        <template #empty>
          <div class="table-empty">
            <div class="empty-icon">📚</div>
            <div class="empty-title">暂无知识库</div>
            <div class="empty-hint">点击右上角「新建知识库」创建</div>
          </div>
        </template>
        <el-table-column label="名称" min-width="200" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="cell-link" @click="goDetail(row)">{{ row.name }}</span>
          </template>
        </el-table-column>
        <el-table-column label="类型" width="110">
          <template #default="{ row }">{{ providerLabel(row.provider_kind) }}</template>
        </el-table-column>
        <el-table-column label="状态" width="90">
          <template #default="{ row }">
            <el-tag v-if="row.status === 1" size="small" type="success">启用</el-tag>
            <el-tag v-else size="small" type="info">禁用</el-tag>
          </template>
        </el-table-column>
        <el-table-column v-if="userStore.user?.is_admin" label="归属" width="120">
          <template #default="{ row }">
            <el-tag v-if="row.owner" size="small" type="warning" effect="plain">{{ row.owner }}</el-tag>
            <span v-else class="cell-muted">-</span>
          </template>
        </el-table-column>
        <el-table-column label="创建时间" width="170">
          <template #default="{ row }"><span class="cell-muted">{{ formatTime(row.created_at) }}</span></template>
        </el-table-column>
        <el-table-column label="操作" width="270" align="center" fixed="right">
          <template #default="{ row }">
            <div class="row-actions">
              <el-button size="small" type="primary" @click="goDetail(row)">文档管理</el-button>
              <el-button size="small" :loading="testingId === row.id" @click="onTest(row)">测试</el-button>
              <el-button size="small" @click="startEdit(row)">编辑</el-button>
              <el-button size="small" type="danger" plain @click="onDelete(row)">删除</el-button>
            </div>
          </template>
        </el-table-column>
      </el-table>
    </div>

    <!-- 新建/编辑表单 -->
    <el-dialog v-model="editOpen" :title="editingId ? '编辑知识库' : '新建知识库'" width="560px" :close-on-click-modal="false">
      <el-form label-width="120px" size="small">
        <el-form-item label="名称" required>
          <el-input v-model="editForm.name" placeholder="如：产品手册知识库" />
        </el-form-item>
        <el-form-item label="类型" required>
          <el-select v-model="editForm.provider_kind" :disabled="!!editingId" style="width: 100%;" @change="onKindChange">
            <el-option v-for="p in schemaProviders" :key="p.kind" :label="p.name" :value="p.kind" />
          </el-select>
        </el-form-item>
        <template v-for="f in currentSchemaFields" :key="f.key">
          <el-form-item :label="f.label" :required="f.required">
            <el-input
              v-if="f.field_type === 'secret'"
              v-model="editForm.config[f.key]"
              type="password"
              show-password
              :placeholder="editingId ? '留空不修改' : (f.placeholder || '')"
            />
            <el-input-number
              v-else-if="f.field_type === 'number'"
              v-model="editForm.config[f.key]"
              :controls="false"
              style="width: 100%;"
              :placeholder="f.placeholder || ''"
            />
            <el-select
              v-else-if="f.field_type === 'select'"
              v-model="editForm.config[f.key]"
              filterable
              placeholder="选择模型"
              style="width: 100%;"
            >
              <el-option v-for="opt in fieldOptions(f)" :key="opt.value" :label="opt.label" :value="opt.value" />
            </el-select>
            <el-input v-else v-model="editForm.config[f.key]" :placeholder="f.placeholder || ''" />
            <div v-if="f.help" class="field-help">{{ f.help }}</div>
          </el-form-item>
        </template>
        <el-form-item label="启用">
          <el-switch v-model="editForm.enabled" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="editOpen = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="onSave">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { ref, reactive, computed, onMounted } from 'vue'
import { ElMessage } from 'element-plus'
import { confirmDeleteWithImpact } from '../composables/useDeleteWithImpact'
import { Plus } from '@element-plus/icons-vue'
import { useRouter } from 'vue-router'
import {
  fetchKbInstances, fetchKbProviderSchema, createKbInstance, updateKbInstance,
  deleteKbInstance, testKbInstance,
} from '../api'
import { useAppStore } from '../stores/app'
import { useUserStore } from '../stores/user'

const router = useRouter()
const appStore = useAppStore()
const userStore = useUserStore()

const loading = ref(false)
const instances = ref([])
const schemaProviders = ref([])

function providerLabel(kind) {
  return kind === 1 ? 'Dify' : kind === 2 ? '内置' : '未知'
}

function formatTime(s) {
  if (!s) return ''
  const d = new Date(s)
  if (isNaN(d.getTime())) return String(s).slice(0, 19).replace('T', ' ')
  const p = (n) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

/// tags 含 embedding 的模型（内置实例创建下拉用）
const embeddingModels = computed(() => {
  const list = appStore.models || []
  return list.filter((m) => Array.isArray(m.tags) ? m.tags.includes('embedding') : m.purpose === 1)
})

/// schema 字段下拉选项（select 类型）：预定义用 schema，embedding_model_id 动态拉模型
function fieldOptions(f) {
  if (f.options && f.options.length) return f.options.map((o) => ({ value: o[0], label: o[1] }))
  if (f.key === 'embedding_model_id') {
    return embeddingModels.value.map((m) => ({
      value: m.id,
      label: `${m.vendor_name || m.provider_name || '未知'} · ${m.name}（${m.model}）${m.embedding_default ? ' ★默认' : ''}`,
    }))
  }
  return []
}

async function loadInstances() {
  loading.value = true
  try {
    const { data, code, message } = await fetchKbInstances()
    if (code === 0) instances.value = data.instances || []
    else ElMessage.error(message || '加载知识库实例失败')
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally { loading.value = false }
}

async function loadSchema() {
  try {
    const { data, code } = await fetchKbProviderSchema()
    if (code === 0) schemaProviders.value = data.providers || []
  } catch (_) {
    // schema 拉不到仅影响新建弹窗的下拉选项，不打断列表页
  }
}

// ===== 新建/编辑 =====
const editOpen = ref(false)
const editingId = ref('')
const saving = ref(false)
const testingId = ref('')
const editForm = reactive({ name: '', provider_kind: 1, config: {}, enabled: true })

const currentSchemaFields = computed(() => {
  const p = schemaProviders.value.find((x) => x.kind === editForm.provider_kind)
  return p ? p.fields : []
})

function startCreate() {
  editingId.value = ''
  editForm.name = ''
  editForm.provider_kind = schemaProviders.value[0]?.kind || 1
  editForm.config = {}
  editForm.enabled = true
  onKindChange()
  editOpen.value = true
}

function startEdit(row) {
  editingId.value = row.id
  editForm.name = row.name
  editForm.provider_kind = row.provider_kind
  editForm.config = { ...(row.config || {}) }
  currentSchemaFields.value.forEach((f) => {
    if (f.field_type === 'number' && editForm.config[f.key] != null) {
      editForm.config[f.key] = Number(editForm.config[f.key])
    }
    if (f.field_type === 'secret') editForm.config[f.key] = ''
  })
  editForm.enabled = row.status === 1
  editOpen.value = true
}

function onKindChange() {
  currentSchemaFields.value.forEach((f) => {
    if (editForm.config[f.key] == null || editForm.config[f.key] === '') {
      if (f.default != null && f.field_type === 'number') editForm.config[f.key] = Number(f.default)
      else if (f.default != null) editForm.config[f.key] = f.default
    }
  })
}

async function onSave() {
  if (!editForm.name.trim()) { ElMessage.warning('请填写名称'); return }
  saving.value = true
  try {
    const payload = {
      name: editForm.name.trim(),
      provider_kind: editForm.provider_kind,
      config: editForm.config,
      status: editForm.enabled ? 1 : 0,
    }
    const fn = editingId.value ? updateKbInstance({ ...payload, id: editingId.value }) : createKbInstance(payload)
    const { code, message } = await fn
    if (code === 0) {
      ElMessage.success(editingId.value ? '已更新' : '已创建')
      editOpen.value = false
      await loadInstances()
    } else ElMessage.error(message || '保存失败')
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally { saving.value = false }
}

async function onDelete(row) {
  await confirmDeleteWithImpact({
    id: row.id,
    removeFn: deleteKbInstance,
    title: '删除知识库',
    targetLabel: row.name,
    onSuccess: () => loadInstances(),
  })
}

async function onTest(row) {
  testingId.value = row.id
  try {
    const { data, code, message } = await testKbInstance(row.id)
    if (code === 0) {
      if (data.ok) ElMessage.success(data.message || '连通正常')
      else ElMessage.error(data.message || '连通失败')
    } else ElMessage.error(message || '测试失败')
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally { testingId.value = '' }
}

function goDetail(row) {
  router.push(`/knowledge/${row.id}`)
}

onMounted(async () => {
  await Promise.all([loadInstances(), loadSchema()])
  if (!appStore.models || !appStore.models.length) appStore.loadModels?.()
})
</script>

<style scoped>
.kb-hint { font-size: 12px; color: var(--muted); }
.cell-link { color: var(--accent); cursor: pointer; font-weight: 600; }
.cell-link:hover { text-decoration: underline; }
.cell-muted { color: var(--muted); }
.field-help { font-size: 11px; color: var(--muted); line-height: 1.4; margin-top: 2px; }
</style>
