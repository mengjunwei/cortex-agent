<template>
  <div class="page-root">
    <!-- 顶部操作栏 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <el-input
          v-model="searchKeyword"
          placeholder="搜索供应商 / 品牌 / 地址"
          clearable
          size="small"
          style="width: 300px;"
        >
          <template #prefix>
            <el-icon><Search /></el-icon>
          </template>
        </el-input>
        <el-button size="small" text @click="searchKeyword = ''">
          <el-icon><Refresh /></el-icon> 重置
        </el-button>
      </div>
      <div class="page-toolbar-right">
        <el-button type="primary" size="small" @click="openProviderDialog()">
          <el-icon><Plus /></el-icon> 新建供应商
        </el-button>
        <el-button
          size="small"
          type="success"
          :disabled="selectedCount === 0"
          :loading="probeLoading"
          @click="probeSelected"
        >
          <el-icon><Connection /></el-icon> 探测选中({{ selectedCount }})
        </el-button>
        <el-button size="small" @click="loadProviders" :loading="loading">
          <el-icon><Refresh /></el-icon> 刷新
        </el-button>
      </div>
    </div>

    <!-- 说明条：全局默认模型唯一 -->
    <div class="info-banner">
      <el-icon class="info-icon"><InfoFilled /></el-icon>
      <span>全局仅允许 <b>一个</b> 默认模型；同一供应商下 <b>模型 ID 不可重复</b>；API Key 仅可重置、不可查看。</span>
    </div>

    <!-- 供应商表格（可展开内嵌模型） -->
    <div class="data-table-wrapper" v-loading="loading">
      <el-table
        class="data-table"
        :data="filteredProviders"
        row-key="id"
        height="100%"
        border
        :expand-row-keys="expandedKeys"
        @expand-change="onExpandChange"
      >
        <el-table-column type="expand">
          <template #default="{ row }">
            <div class="model-nested">
              <div class="model-nested-head">
                <span class="model-nested-title">模型列表（{{ row.models.length }}）</span>
                <div class="model-nested-actions">
                  <el-button
                    size="small"
                    type="success"
                    :disabled="(row.models || []).length === 0 || probeLoading"
                    @click="probeProviderAll(row)"
                  >
                    <el-icon><Connection /></el-icon> 探测本供应商全部
                  </el-button>
                  <el-button size="small" type="primary" plain @click="openModelDialog(row.id)">
                    <el-icon><Plus /></el-icon> 添加模型
                  </el-button>
                </div>
              </div>
              <el-table
                :data="row.models"
                size="small"
                border
                :show-header="true"
                empty-text="该供应商下暂无模型，点击「添加模型」新建"
              >
                <el-table-column label="" width="45" align="center">
                  <template #default="{ row: m }">
                    <el-checkbox
                      :model-value="isSelected(m.id)"
                      @change="(v) => toggleSelect(m.id, v)"
                      @click.stop
                    />
                  </template>
                </el-table-column>
                <el-table-column label="默认" width="100" align="center">
                  <template #default="{ row: m }">
                    <el-tag v-if="m.is_default" type="success" size="small" effect="dark">对话默认</el-tag>
                    <el-button v-else size="small" text type="primary" @click="handleSetDefault(m.id)">设对话默认</el-button>
                    <div v-if="(m.tags || []).includes('embedding')" style="margin-top:4px;">
                      <el-tag v-if="m.embedding_default" type="warning" size="small" effect="dark">向量默认</el-tag>
                      <el-button v-else size="small" text type="warning" @click="handleSetEmbeddingDefault(m.id)">设向量默认</el-button>
                    </div>
                  </template>
                </el-table-column>
                <el-table-column label="显示名称" min-width="140" show-overflow-tooltip>
                  <template #default="{ row: m }">
                    <span class="cell-title">{{ m.name }}</span>
                  </template>
                </el-table-column>
                <el-table-column label="模型 ID（model）" min-width="180" show-overflow-tooltip>
                  <template #default="{ row: m }">
                    <code class="model-id-code">{{ m.model }}</code>
                  </template>
                </el-table-column>
                <el-table-column label="标签" min-width="150" align="center">
                  <template #default="{ row: m }">
                    <el-tag v-for="t in (m.tags || ['chat'])" :key="t" :type="t === 'embedding' ? 'warning' : 'info'" size="small" effect="plain" style="margin:1px;">{{ t }}</el-tag>
                    <div v-if="(m.tags || []).includes('embedding')" class="cell-muted" style="font-size:11px;">{{ m.embedding_dimensions || '?' }}维</div>
                  </template>
                </el-table-column>
                <el-table-column label="状态" width="100" align="center">
                  <template #default="{ row: m }">
                    <el-switch
                      :model-value="m.status"
                      :active-value="1"
                      :inactive-value="0"
                      @change="(v) => handleModelStatusChange(m, v)"
                    />
                  </template>
                </el-table-column>
                <el-table-column label="探测" width="120" align="center">
                  <template #default="{ row: m }">
                    <span v-if="!probeStatusMap.get(m.id)" class="cell-muted">—</span>
                    <span v-else-if="probeStatusMap.get(m.id).status === 'probing'" class="probe-probing">
                      <el-icon class="is-loading"><Loading /></el-icon> 探测中
                    </span>
                    <span v-else-if="probeStatusMap.get(m.id).status === 'ok'" class="probe-ok">
                      ✅ {{ probeStatusMap.get(m.id).latency }}ms
                    </span>
                    <span v-else class="probe-fail" :title="probeStatusMap.get(m.id).error">
                      ❌ 失败
                    </span>
                  </template>
                </el-table-column>
                <el-table-column label="更新时间" width="150">
                  <template #default="{ row: m }">
                    <span class="cell-muted">{{ formatTime(m.updated_at) }}</span>
                  </template>
                </el-table-column>
                <el-table-column label="操作" width="220" align="center" fixed="right">
                  <template #default="{ row: m }">
                    <div class="row-actions" @click.stop>
                      <el-button size="small" type="success" @click="probeOneModel(m)">探测</el-button>
                      <el-button size="small" @click="openModelDialog(row.id, m)">编辑</el-button>
                      <el-button size="small" type="danger" plain @click="handleDeleteModel(m)">删除</el-button>
                    </div>
                  </template>
                </el-table-column>
              </el-table>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="供应商" min-width="200" show-overflow-tooltip>
          <template #default="{ row }">
            <div style="display:flex; align-items:center; gap:8px;">
              <span style="font-size:16px;">🏢</span>
              <div>
                <div class="cell-title">{{ row.name }}</div>
                <div class="cell-muted" style="font-size:11px;">
                  {{ row.vendor_name }}
                  <el-tag size="small" :type="row.protocol === 'anthropic' ? 'warning' : 'info'" effect="plain" style="margin-left:6px;">{{ protocolLabel(row.protocol) }}</el-tag>
                </div>
              </div>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="Base URL" min-width="240" show-overflow-tooltip>
          <template #default="{ row }">
            <code class="base-url-code">{{ row.base_url }}</code>
          </template>
        </el-table-column>

        <el-table-column label="API Key" width="150" align="center">
          <template #default="{ row }">
            <div class="key-cell">
              <span class="key-mask">****</span>
              <span class="key-suffix" v-if="row.api_key_suffix">{{ row.api_key_suffix }}</span>
              <span class="cell-muted" v-else>未设置</span>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="模型数" width="80" align="center">
          <template #default="{ row }">
            <el-tag size="small" type="info" effect="plain">{{ row.models.length }}</el-tag>
          </template>
        </el-table-column>

        <el-table-column label="状态" width="90" align="center">
          <template #default="{ row }">
            <el-switch
              :model-value="row.status"
              :active-value="1"
              :inactive-value="0"
              @change="(v) => handleProviderStatusChange(row, v)"
            />
          </template>
        </el-table-column>

        <el-table-column label="操作" width="240" align="center" fixed="right">
          <template #default="{ row }">
            <div class="row-actions" @click.stop>
              <el-button size="small" @click="openProviderDialog(row)">编辑</el-button>
              <el-button size="small" type="warning" plain @click="openResetKeyDialog(row)">重置密钥</el-button>
              <el-button size="small" type="danger" plain @click="handleDeleteProvider(row)">删除</el-button>
            </div>
          </template>
        </el-table-column>

        <template #empty>
          <div class="table-empty">
            <div class="empty-icon">🔌</div>
            <div class="empty-title">暂无模型供应商</div>
            <div class="empty-hint">点击右上角「新建供应商」配置模型接入（支持 OpenAI 兼容 / Anthropic 协议）</div>
          </div>
        </template>
      </el-table>
    </div>

    <!-- 供应商 新建/编辑 对话框 -->
    <el-dialog
      v-model="providerDialogVisible"
      :title="providerForm.id ? '编辑供应商' : '新建供应商'"
      width="520px"
      :close-on-click-modal="false"
    >
      <el-form ref="providerFormRef" :model="providerForm" :rules="providerRules" label-width="100px">
        <el-form-item label="品牌" prop="vendor_name">
          <el-input v-model="providerForm.vendor_name" placeholder="如 OpenAI / DeepSeek / Azure" />
        </el-form-item>
        <el-form-item label="名称" prop="name">
          <el-input v-model="providerForm.name" placeholder="该分组的显示名称，如「DeepSeek-主账号」" />
        </el-form-item>
        <el-form-item label="协议" prop="protocol">
          <el-radio-group v-model="providerForm.protocol">
            <el-radio :value="'openai_compat'">OpenAI 兼容</el-radio>
            <el-radio :value="'anthropic'">Anthropic</el-radio>
          </el-radio-group>
          <span class="form-hint">{{ providerForm.protocol === 'anthropic' ? 'Claude Messages API' : '/chat/completions' }}</span>
        </el-form-item>
        <el-form-item label="Base URL" prop="base_url">
          <el-input v-model="providerForm.base_url" :placeholder="baseUrlPlaceholder" />
        </el-form-item>
        <el-form-item v-if="!providerForm.id" label="API Key" prop="api_key">
          <el-input v-model="providerForm.api_key" type="password" show-password placeholder="仅此一次填写，保存后不可查看" />
        </el-form-item>
        <el-form-item label="状态" prop="status">
          <el-switch v-model="providerForm.status" :active-value="1" :inactive-value="0" />
          <span class="form-hint">{{ providerForm.status === 1 ? '启用' : '禁用' }}</span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="providerDialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="submitting" @click="submitProvider">保存</el-button>
      </template>
    </el-dialog>

    <!-- 重置 API Key 对话框 -->
    <el-dialog
      v-model="resetKeyDialogVisible"
      title="重置 API Key"
      width="460px"
      :close-on-click-modal="false"
    >
      <el-alert type="warning" :closable="false" show-icon style="margin-bottom: 12px;">
        出于安全考虑，旧密钥不可查看。请输入新的 API Key，保存后立即生效。
      </el-alert>
      <el-form ref="resetKeyFormRef" :model="resetKeyForm" :rules="resetKeyRules" label-width="90px">
        <el-form-item label="供应商">
          <span class="cell-muted">{{ resetKeyForm.provider_name }}</span>
        </el-form-item>
        <el-form-item label="新 API Key" prop="api_key">
          <el-input v-model="resetKeyForm.api_key" type="password" show-password placeholder="输入新的 API Key" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="resetKeyDialogVisible = false">取消</el-button>
        <el-button type="warning" :loading="submitting" @click="submitResetKey">确认重置</el-button>
      </template>
    </el-dialog>

    <!-- 模型 新建/编辑 对话框 -->
    <el-dialog
      v-model="modelDialogVisible"
      :title="modelForm.id ? '编辑模型' : '新建模型'"
      width="480px"
      :close-on-click-modal="false"
    >
      <el-form ref="modelFormRef" :model="modelForm" :rules="modelRules" label-width="100px">
        <el-form-item label="显示名称" prop="name">
          <el-input v-model="modelForm.name" placeholder="会话下拉展示的名称，如「DeepSeek 对话」" />
        </el-form-item>
        <el-form-item label="模型 ID" prop="model">
          <el-input v-model="modelForm.model" placeholder="OpenAI 协议的 model 字段，如 deepseek-chat" />
        </el-form-item>
        <el-form-item label="能力标签">
          <el-select v-model="modelForm.tags" multiple filterable allow-create default-first-option placeholder="选择或输入标签" style="width: 100%;">
            <el-option v-for="t in ['chat','embedding','rerank','reasoning','vision','tool_use']" :key="t" :label="t" :value="t" />
          </el-select>
          <span class="form-hint">chat=对话 / embedding=向量化 / reasoning=推理 / rerank=重排，可多选</span>
        </el-form-item>
        <el-form-item v-if="modelForm.tags.includes('embedding')" label="向量维度">
          <el-input-number v-model="modelForm.embedding_dimensions" :min="1" :controls="false" placeholder="如 768 / 1024" style="width: 100%;" />
          <span class="form-hint">模型产出向量维度，如 nomic-embed-text=768、bge-m3=1024</span>
        </el-form-item>
        <el-form-item label="上下文窗口">
          <el-input-number v-model="modelForm.context_window" :min="0" :controls="false" placeholder="如 128000，留空走默认" style="width: 100%;" />
          <span class="form-hint">模型上下文 token 上限，用于动态压缩阈值；留空=默认 128000</span>
        </el-form-item>
        <el-form-item label="状态" prop="status">
          <el-switch v-model="modelForm.status" :active-value="1" :inactive-value="0" />
          <span class="form-hint">{{ modelForm.status === 1 ? '启用' : '禁用' }}</span>
        </el-form-item>
        <div class="form-tip">同一供应商下「模型 ID」不可重复；首个创建的模型将自动成为默认。</div>
      </el-form>
      <template #footer>
        <el-button @click="modelDialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="submitting" @click="submitModel">保存</el-button>
      </template>
    </el-dialog>

    <!-- 探测结果抽屉 -->
    <el-drawer
      v-model="probeDrawerVisible"
      title="探测结果"
      direction="rtl"
      size="560px"
    >
      <el-table :data="probeResults" border size="small">
        <el-table-column label="模型" min-width="160" show-overflow-tooltip>
          <template #default="{ row }">
            <div class="cell-title">{{ row.model || row.model_id }}</div>
            <div class="cell-muted" style="font-size:11px;">{{ row.provider_name }}</div>
          </template>
        </el-table-column>
        <el-table-column label="类型" width="90" align="center">
          <template #default="{ row }">
            <el-tag size="small" effect="plain">{{ row.probe_kind }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="90" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.status === 'ok'" type="success" size="small" effect="dark">✅ 存活</el-tag>
            <el-tag v-else type="danger" size="small" effect="dark">❌ 失败</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="耗时" width="80" align="center">
          <template #default="{ row }">
            <span class="cell-muted">{{ row.latency_ms }}ms</span>
          </template>
        </el-table-column>
        <el-table-column label="错误详情" min-width="200">
          <template #default="{ row }">
            <div v-if="row.error" class="probe-error-cell">
              <span class="probe-error-text">{{ row.error }}</span>
              <el-button size="small" text @click="copyError(row.error)">复制</el-button>
            </div>
            <span v-else class="cell-muted">—</span>
          </template>
        </el-table-column>
      </el-table>
    </el-drawer>
  </div>
</template>

<script setup>
import { ref, computed, onMounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { confirmDeleteWithImpact } from '../composables/useDeleteWithImpact'
import { Search, Refresh, Plus, InfoFilled, Loading, Connection } from '@element-plus/icons-vue'
import {
  fetchModelProviders,
  createModelProvider,
  updateModelProvider,
  deleteModelProvider,
  resetModelProviderKey,
  createModel,
  updateModel,
  deleteModel,
  setDefaultModel,
  setEmbeddingDefaultModel,
  probeModels,
} from '../api'
import { useAppStore } from '../stores/app'

const appStore = useAppStore()
const route = useRoute()
const router = useRouter()

const providers = ref([])
const loading = ref(false)
const submitting = ref(false)
const searchKeyword = ref('')
const expandedKeys = ref([])

// ========== 模型探测 ==========
const selectedIds = ref(new Set())          // 跨供应商勾选的模型 id
const probeStatusMap = ref(new Map())       // model id -> {status:'probing'|'ok'|'fail', latency, error, kind}
const probeResults = ref([])                 // 最近一次探测结果数组（结果抽屉用）
const probeDrawerVisible = ref(false)
const probeLoading = ref(false)

function restoreFromQuery() {
  const q = route.query
  if (q.kw) searchKeyword.value = String(q.kw)
  if (q.exp) {
    expandedKeys.value = String(q.exp).split(',').filter(Boolean)
  }
}

function syncToQuery() {
  const query = {}
  if (searchKeyword.value) query.kw = searchKeyword.value
  if (expandedKeys.value.length > 0) query.exp = expandedKeys.value.join(',')
  router.replace({ path: '/model-providers', query })
}

const filteredProviders = computed(() => {
  const kw = searchKeyword.value.trim().toLowerCase()
  if (!kw) return providers.value
  return providers.value.filter(p =>
    (p.name || '').toLowerCase().includes(kw) ||
    (p.vendor_name || '').toLowerCase().includes(kw) ||
    (p.base_url || '').toLowerCase().includes(kw),
  )
})

async function loadProviders() {
  loading.value = true
  try {
    const { data, code, message } = await fetchModelProviders()
    if (code === 0) {
      providers.value = data.providers || []
      // 展开第一个供应商，便于直接看到模型
      if (expandedKeys.value.length === 0 && providers.value.length > 0) {
        expandedKeys.value = [providers.value[0].id]
      }
    } else {
      ElMessage.error(message || '加载供应商失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    loading.value = false
  }
}

function onExpandChange(row, expandedList) {
  expandedKeys.value = expandedList.map(r => r.id)
}

// ========== 供应商 表单 ==========
const providerDialogVisible = ref(false)
const providerFormRef = ref()
const providerForm = ref({ id: '', vendor_name: '', name: '', base_url: '', protocol: 'openai_compat', api_key: '', status: 1 })
const providerRules = {
  vendor_name: [{ required: true, message: '请输入品牌', trigger: 'blur' }],
  name: [{ required: true, message: '请输入名称', trigger: 'blur' }],
  base_url: [{ required: true, message: '请输入 Base URL', trigger: 'blur' }],
  api_key: [{ required: true, message: '请输入 API Key', trigger: 'blur' }],
}

function protocolLabel(p) {
  return p === 'anthropic' ? 'Anthropic' : 'OpenAI 兼容'
}

const baseUrlPlaceholder = computed(() => {
  return providerForm.value.protocol === 'anthropic'
    ? '默认 https://api.anthropic.com，可填第三方 Anthropic 兼容网关'
    : 'OpenAI 协议兼容地址，如 https://api.deepseek.com/v1'
})

function openProviderDialog(row) {
  if (row) {
    providerForm.value = {
      id: row.id,
      vendor_name: row.vendor_name,
      name: row.name,
      base_url: row.base_url,
      protocol: row.protocol || 'openai_compat',
      api_key: '',
      status: row.status,
    }
  } else {
    providerForm.value = { id: '', vendor_name: '', name: '', base_url: '', protocol: 'openai_compat', api_key: '', status: 1 }
  }
  providerDialogVisible.value = true
}

async function submitProvider() {
  await providerFormRef.value?.validate(async (valid) => {
    if (!valid) return
    submitting.value = true
    try {
      const f = providerForm.value
      if (f.id) {
        const { data, code, message } = await updateModelProvider(f.id, {
          vendor_name: f.vendor_name,
          name: f.name,
          base_url: f.base_url,
          protocol: f.protocol,
          status: f.status,
        })
        if (code === 0) {
          ElMessage.success(data.notice ? `供应商已更新。${data.notice}` : '供应商已更新')
          providerDialogVisible.value = false
          await reload()
        } else {
          ElMessage.error(message || '更新失败')
        }
      } else {
        const { code, message } = await createModelProvider({
          vendor_name: f.vendor_name,
          name: f.name,
          base_url: f.base_url,
          protocol: f.protocol,
          api_key: f.api_key,
          status: f.status,
        })
        if (code === 0) {
          ElMessage.success('供应商已创建')
          providerDialogVisible.value = false
          await reload()
        } else {
          ElMessage.error(message || '创建失败')
        }
      }
    } catch (e) {
      ElMessage.error('请求失败: ' + (e.message || '网络错误'))
    } finally {
      submitting.value = false
    }
  })
}

async function handleDeleteProvider(row) {
  await confirmDeleteWithImpact({
    id: row.id,
    removeFn: deleteModelProvider,
    title: '删除供应商',
    targetLabel: row.name,
    onSuccess: () => reload(),
  })
}

async function handleProviderStatusChange(row, value) {
  // 切换启用状态：等待接口成功后再刷新列表，失败时由 reload 重新拉取真实状态
  try {
    const { data, code, message } = await updateModelProvider(row.id, {
      vendor_name: row.vendor_name,
      name: row.name,
      base_url: row.base_url,
      protocol: row.protocol || 'openai_compat',
      status: value,
    })
    if (code === 0) {
      row.status = value
      const base = value === 1 ? '已启用' : '已禁用'
      ElMessage.success(data.notice ? `${base}。${data.notice}` : base)
      await reload()
    } else {
      ElMessage.error(message || '状态更新失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  }
}

// ========== 重置 API Key ==========
const resetKeyDialogVisible = ref(false)
const resetKeyFormRef = ref()
const resetKeyForm = ref({ id: '', provider_name: '', api_key: '' })
const resetKeyRules = {
  api_key: [{ required: true, message: '请输入新的 API Key', trigger: 'blur' }],
}

function openResetKeyDialog(row) {
  resetKeyForm.value = { id: row.id, provider_name: `${row.vendor_name} / ${row.name}`, api_key: '' }
  resetKeyDialogVisible.value = true
}

async function submitResetKey() {
  await resetKeyFormRef.value?.validate(async (valid) => {
    if (!valid) return
    submitting.value = true
    try {
      const { code, message } = await resetModelProviderKey(resetKeyForm.value.id, resetKeyForm.value.api_key)
      if (code === 0) {
        ElMessage.success('API Key 已重置')
        resetKeyDialogVisible.value = false
        await reload()
      } else {
        ElMessage.error(message || '重置失败')
      }
    } catch (e) {
      ElMessage.error('请求失败: ' + (e.message || '网络错误'))
    } finally {
      submitting.value = false
    }
  })
}

// ========== 模型 表单 ==========
const modelDialogVisible = ref(false)
const modelFormRef = ref()
const modelForm = ref({ id: '', provider_id: '', name: '', model: '', status: 1, tags: ['chat'], embedding_dimensions: null, context_window: null })
const modelRules = {
  name: [{ required: true, message: '请输入显示名称', trigger: 'blur' }],
  model: [{ required: true, message: '请输入模型 ID', trigger: 'blur' }],
}

function openModelDialog(providerId, row) {
  if (row) {
    modelForm.value = {
      id: row.id,
      provider_id: providerId,
      name: row.name,
      model: row.model,
      status: row.status,
      tags: Array.isArray(row.tags) && row.tags.length ? row.tags : ['chat'],
      embedding_dimensions: row.embedding_dimensions ?? null,
      context_window: row.context_window ?? null,
    }
  } else {
    modelForm.value = { id: '', provider_id: providerId, name: '', model: '', status: 1, tags: ['chat'], embedding_dimensions: null, context_window: null }
  }
  modelDialogVisible.value = true
}

async function submitModel() {
  await modelFormRef.value?.validate(async (valid) => {
    if (!valid) return
    submitting.value = true
    try {
      const f = modelForm.value
      const payload = {
        name: f.name,
        model: f.model,
        status: f.status,
        tags: f.tags,
        embedding_dimensions: f.tags.includes('embedding') ? f.embedding_dimensions : null,
        context_window: f.context_window || null,
      }
      if (f.id) {
        const { data, code, message } = await updateModel(f.id, payload)
        if (code === 0) {
          ElMessage.success(data.notice ? `模型已更新。${data.notice}` : '模型已更新')
          modelDialogVisible.value = false
          await reload()
        } else {
          ElMessage.error(message || '更新失败')
        }
      } else {
        const { code, message } = await createModel(f.provider_id, payload)
        if (code === 0) {
          ElMessage.success('模型已创建')
          modelDialogVisible.value = false
          await reload()
        } else {
          ElMessage.error(message || '创建失败')
        }
      }
    } catch (e) {
      ElMessage.error('请求失败: ' + (e.message || '网络错误'))
    } finally {
      submitting.value = false
    }
  })
}

async function handleDeleteModel(m) {
  await confirmDeleteWithImpact({
    id: m.id,
    removeFn: deleteModel,
    title: '删除模型',
    targetLabel: m.name,
    onSuccess: () => reload(),
  })
}

async function handleModelStatusChange(row, value) {
  try {
    const { data, code, message } = await updateModel(row.id, {
      name: row.name,
      model: row.model,
      status: value,
      tags: row.tags || ['chat'],
      embedding_dimensions: row.embedding_dimensions ?? null,
      context_window: row.context_window ?? null,
    })
    if (code === 0) {
      row.status = value
      const base = value === 1 ? '已启用' : '已禁用'
      ElMessage.success(data.notice ? `${base}。${data.notice}` : base)
      await reload()
    } else {
      ElMessage.error(message || '状态更新失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  }
}

async function handleSetDefault(id) {
  try {
    const { code, message } = await setDefaultModel(id)
    if (code === 0) {
      ElMessage.success('已设为默认模型')
      await reload()
    } else {
      ElMessage.error(message || '设置失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  }
}

async function handleSetEmbeddingDefault(id) {
  try {
    const { code, message } = await setEmbeddingDefaultModel(id)
    if (code === 0) {
      ElMessage.success('已设为默认 embedding 模型')
      await reload()
    } else {
      ElMessage.error(message || '设置失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  }
}

// ===== 选中管理 =====
function toggleSelect(id, checked) {
  const s = new Set(selectedIds.value)
  if (checked) s.add(id)
  else s.delete(id)
  selectedIds.value = s
}
function isSelected(id) {
  return selectedIds.value.has(id)
}
const selectedCount = computed(() => selectedIds.value.size)

// 刷新列表后与当前可见模型取交集，剔除已删除模型的残留选中
function pruneSelectedByVisible() {
  const visible = new Set()
  for (const p of providers.value) {
    for (const m of p.models || []) visible.add(m.id)
  }
  const s = new Set()
  for (const id of selectedIds.value) if (visible.has(id)) s.add(id)
  selectedIds.value = s
}

// ===== 探测 =====
async function runProbe(ids) {
  if (!ids || ids.length === 0) return
  probeLoading.value = true
  // 立即把待探测项置为 probing，UI 转圈
  const m = new Map(probeStatusMap.value)
  for (const id of ids) m.set(id, { status: 'probing' })
  probeStatusMap.value = m
  try {
    const { data, code, message } = await probeModels(ids)
    if (code === 0) {
      const results = (data && data.results) || []
      probeResults.value = results
      // 回填徽标状态
      const m2 = new Map(probeStatusMap.value)
      for (const r of results) {
        m2.set(r.model_id, {
          status: r.status, // 'ok' | 'fail'
          latency: r.latency_ms,
          error: r.error,
          kind: r.probe_kind,
        })
      }
      probeStatusMap.value = m2
      probeDrawerVisible.value = true
      const failCount = results.filter((r) => r.status === 'fail').length
      if (failCount === 0) ElMessage.success(`探测完成：${results.length} 个全部存活`)
      else ElMessage.warning(`探测完成：${failCount} 个失败，详见结果面板`)
    } else {
      ElMessage.error(message || '探测失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    probeLoading.value = false
  }
}

async function probeSelected() {
  await runProbe(Array.from(selectedIds.value))
}
function probeProviderAll(row) {
  const ids = (row.models || []).map((m) => m.id)
  if (ids.length === 0) return
  runProbe(ids)
}
async function probeOneModel(m) {
  await runProbe([m.id])
}

// 重新加载列表 + 同步会话下拉
async function reload() {
  await loadProviders()
  pruneSelectedByVisible()
  // 模型列表可能变化，同步刷新会话顶部下拉与默认模型
  await appStore.loadModels()
}

function formatTime(iso) {
  if (!iso) return ''
  const d = new Date(iso)
  return d.toLocaleString('zh-CN', { year: 'numeric', month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
}

// 复制探测错误信息到剪贴板
async function copyError(text) {
  try {
    await navigator.clipboard.writeText(text || '')
    ElMessage.success('错误信息已复制')
  } catch {
    ElMessage.error('复制失败，请手动选择文本复制')
  }
}

onMounted(() => {
  restoreFromQuery()
  loadProviders()
})

watch([searchKeyword, expandedKeys], () => {
  syncToQuery()
}, { deep: true })
</script>

<style scoped>
.info-banner {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 14px;
  margin-bottom: 12px;
  border-radius: var(--radius);
  background: rgba(0, 212, 255, 0.06);
  border: 1px solid rgba(0, 212, 255, 0.15);
  color: var(--muted);
  font-size: 12px;
}
.info-banner .info-icon { color: var(--accent); font-size: 15px; }
.info-banner b { color: var(--text); font-weight: 700; }

.model-nested {
  padding: 12px 16px 16px;
  background: rgba(255, 255, 255, 0.02);
}
.model-nested-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 10px;
}
.model-nested-title {
  font-size: 12px;
  font-weight: 700;
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 1px;
}
.model-id-code {
  font-family: var(--font-mono);
  font-size: 12px;
  color: var(--accent);
  background: rgba(0, 212, 255, 0.08);
  padding: 1px 6px;
  border-radius: 4px;
}
.base-url-code {
  font-family: var(--font-mono);
  font-size: 12px;
  color: var(--text);
}
.key-cell {
  display: flex;
  align-items: center;
  gap: 2px;
  justify-content: center;
  font-family: var(--font-mono);
}
.key-mask { color: var(--muted); letter-spacing: 1px; }
.key-suffix { color: var(--accent); font-weight: 600; }

.form-hint {
  margin-left: 10px;
  font-size: 12px;
  color: var(--muted);
}
.form-tip {
  margin-top: -6px;
  padding-left: 100px;
  font-size: 11px;
  color: var(--muted);
  line-height: 1.5;
}

/* 嵌套表头右侧按钮组（探测本供应商全部 + 添加模型） */
.model-nested-actions {
  display: flex;
  gap: 8px;
}

/* 模型探测徽标 */
.probe-probing { color: var(--accent); font-size: 12px; display: inline-flex; align-items: center; gap: 3px; }
.probe-ok { color: #67c23a; font-size: 12px; }
.probe-fail { color: #f56c6c; font-size: 12px; cursor: help; }
.probe-error-cell { display: flex; flex-direction: column; gap: 4px; }
.probe-error-text {
  font-size: 12px;
  color: #f56c6c;
  white-space: pre-wrap;
  word-break: break-all;
  max-height: 120px;
  overflow-y: auto;
}
</style>
