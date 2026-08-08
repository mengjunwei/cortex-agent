<template>
  <div class="page-root" v-if="!notFound">
    <div class="page-toolbar">
      <div class="page-toolbar-left kb-header">
        <el-button @click="goBack"><el-icon><ArrowLeft /></el-icon> 返回</el-button>
        <span class="kb-title">{{ currentInstance ? currentInstance.name : '加载中…' }}</span>
        <el-tag v-if="currentInstance" size="small" type="info">{{ providerLabel(currentInstance.provider_kind) }}</el-tag>
        <el-tag v-if="currentInstance && currentInstance.status !== 1" size="small" type="warning">已禁用</el-tag>
      </div>
      <div class="page-toolbar-right">
        <el-input
          v-model="searchKeyword"
          placeholder="搜索文档标题"
          clearable
          size="small"
          style="width: 220px;"
          @keyup.enter="onSearch"
          @clear="onSearch"
        >
          <template #prefix><el-icon><Search /></el-icon></template>
        </el-input>
        <el-button size="small" @click="onSearch"><el-icon><Search /></el-icon> 搜索</el-button>
        <el-button size="small" text @click="resetFilter"><el-icon><Refresh /></el-icon> 重置</el-button>
        <el-button
          v-if="selectedIds.size > 0"
          size="small"
          type="danger"
          plain
          :loading="batchDeleting"
          @click="showBatchDeleteConfirm"
        >批量删除 ({{ selectedIds.size }})</el-button>
        <el-button type="primary" size="small" @click="openUpload"><el-icon><Plus /></el-icon> 上传文档</el-button>
      </div>
    </div>

    <div class="data-table-wrapper" v-loading="docsLoading">
      <el-table class="data-table" :data="documents" row-key="id" height="100%" stripe border @expand-change="onExpandChange">
        <template #empty>
          <div class="table-empty">
            <div class="empty-icon">📭</div>
            <div class="empty-title">暂无文档</div>
            <div class="empty-hint">点击「上传文档」添加内容</div>
          </div>
        </template>
        <el-table-column type="expand" width="40">
          <template #default="{ row }">
            <div class="doc-segments">
              <div v-if="segmentsLoading.has(row.id)" class="segments-loading">加载中...</div>
              <div v-else-if="(docSegments[row.id] || []).length === 0" class="segments-empty">暂无片段</div>
              <div v-else class="segments-list">
                <div v-for="(seg, si) in docSegments[row.id]" :key="si" class="segment-item">
                  <div class="segment-header"><span class="segment-idx">片段 #{{ si + 1 }}</span></div>
                  <pre class="segment-content">{{ seg.content || seg.text || '' }}</pre>
                </div>
              </div>
            </div>
          </template>
        </el-table-column>
        <el-table-column width="48" align="center">
          <template #header>
            <el-checkbox :model-value="isCurrentPageAllSelected" :indeterminate="isCurrentPageIndeterminate" :disabled="documents.length === 0" @change="toggleSelectCurrentPage" />
          </template>
          <template #default="{ row }">
            <el-checkbox :model-value="selectedIds.has(row.id)" @change="toggleSelect(row.id)" @click.stop />
          </template>
        </el-table-column>
        <el-table-column prop="title" label="标题" min-width="280" sortable show-overflow-tooltip>
          <template #default="{ row }"><span class="cell-title">{{ row.title || '未命名' }}</span></template>
        </el-table-column>
        <el-table-column label="字数" width="100" align="right" sortable prop="word_count">
          <template #default="{ row }"><span class="cell-muted">{{ row.word_count || 0 }}</span></template>
        </el-table-column>
        <el-table-column label="操作" width="100" align="center" fixed="right">
          <template #default="{ row }">
            <div class="row-actions" @click.stop>
              <el-popconfirm title="确定删除此文档？" confirm-button-text="删除" cancel-button-text="取消" @confirm="handleDelete(row)">
                <template #reference><el-button size="small" type="danger" plain>删除</el-button></template>
              </el-popconfirm>
            </div>
          </template>
        </el-table-column>
      </el-table>
    </div>

    <div class="page-pagination" v-if="total > 0">
      <span class="page-total">共 {{ total }} 条</span>
      <el-pagination background layout="sizes, prev, pager, next, jumper" :total="total" :page-size="pageSize" :page-sizes="[10, 20, 50, 100]" :current-page="currentPage" @current-change="onPageChange" @size-change="onPageSizeChange" />
    </div>

    <!-- 上传弹窗 -->
    <el-dialog v-model="uploadOpen" title="上传文档" width="640px" :close-on-click-modal="false">
      <div class="upload-form">
        <div class="form-item">
          <label class="form-label">标题</label>
          <el-input v-model="uploadForm.title" placeholder="文档标题" clearable />
        </div>
        <div class="form-row">
          <div class="form-item">
            <label class="form-label">厂商（可选）</label>
            <el-input v-model="uploadForm.brand" placeholder="如 H3C，留空则不限定" clearable />
          </div>
          <div class="form-item">
            <label class="form-label">设备类型（可选）</label>
            <el-input v-model="uploadForm.dev_type" placeholder="如 路由器，留空则不限定" clearable />
          </div>
          <div class="form-item">
            <label class="form-label">设备型号（可选）</label>
            <el-input v-model="uploadForm.model" placeholder="如 S5300，留空则不限定" clearable />
          </div>
        </div>
        <div class="form-item">
          <label class="form-label">内容</label>
          <el-input v-model="uploadForm.content" type="textarea" :rows="12" placeholder="粘贴文档内容..." />
        </div>
      </div>
      <template #footer>
        <el-button @click="uploadOpen = false">取消</el-button>
        <el-button type="primary" :loading="uploading" @click="handleUpload">上传</el-button>
      </template>
    </el-dialog>
  </div>
  <el-empty v-else description="实例不存在或已删除">
    <el-button type="primary" @click="goBack">返回知识库列表</el-button>
  </el-empty>
</template>

<script setup>
import { ref, reactive, computed, onMounted, watch } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { ArrowLeft, Search, Refresh, Plus } from '@element-plus/icons-vue'
import { useRoute, useRouter } from 'vue-router'
import {
  fetchKbInstances, fetchInstanceDocuments, fetchInstanceSegments,
  deleteInstanceDocument, uploadInstanceDocument,
} from '../api'

const route = useRoute()
const router = useRouter()

const instanceId = computed(() => route.params.id)
const instances = ref([])
const currentInstance = computed(() => instances.value.find((i) => i.id === instanceId.value))
const notFound = computed(() => instances.value.length > 0 && !currentInstance.value)

function providerLabel(kind) {
  return kind === 1 ? 'Dify' : kind === 2 ? '内置' : '未知'
}

async function loadInstances() {
  const { data, code } = await fetchKbInstances()
  if (code === 0) instances.value = data.instances || []
}

// ===== 文档列表 =====
const documents = ref([])
const docsLoading = ref(false)
const docSegments = ref({})
const segmentsLoading = ref(new Set())
const searchKeyword = ref('')
const currentPage = ref(1)
const pageSize = ref(20)
const total = ref(0)
const selectedIds = ref(new Set())
const batchDeleting = ref(false)

async function loadDocuments() {
  if (!instanceId.value) return
  docsLoading.value = true
  try {
    const { data, code, message } = await fetchInstanceDocuments(
      instanceId.value, currentPage.value, pageSize.value,
      { keyword: searchKeyword.value },
    )
    if (code === 0) {
      documents.value = data.documents || []
      total.value = data.total || 0
    } else {
      ElMessage.error(message || '加载文档失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || ''))
  } finally {
    docsLoading.value = false
  }
}

function onSearch() { currentPage.value = 1; loadDocuments() }
function resetFilter() { searchKeyword.value = ''; currentPage.value = 1; loadDocuments() }
function onPageChange(p) { currentPage.value = p; loadDocuments() }
function onPageSizeChange(s) { pageSize.value = s; currentPage.value = 1; loadDocuments() }

async function loadSegments(docId) {
  const set = new Set(segmentsLoading.value); set.add(docId); segmentsLoading.value = set
  try {
    const { data, code } = await fetchInstanceSegments(instanceId.value, docId)
    if (code === 0) docSegments.value[docId] = data.segments || []
    else docSegments.value[docId] = []
  } catch { docSegments.value[docId] = [] }
  finally { const s = new Set(segmentsLoading.value); s.delete(docId); segmentsLoading.value = s }
}
function onExpandChange(row, expanded) {
  if (expanded.some((r) => r.id === row.id) && !docSegments.value[row.id]) loadSegments(row.id)
}

const isCurrentPageAllSelected = computed(() => documents.value.length > 0 && documents.value.every((d) => selectedIds.value.has(d.id)))
const isCurrentPageIndeterminate = computed(() => {
  const n = documents.value.filter((d) => selectedIds.value.has(d.id)).length
  return n > 0 && n < documents.value.length
})
function toggleSelect(id) {
  const s = new Set(selectedIds.value)
  if (s.has(id)) s.delete(id); else s.add(id)
  selectedIds.value = s
}
function toggleSelectCurrentPage(checked) {
  const s = new Set(selectedIds.value)
  if (checked) documents.value.forEach((d) => s.add(d.id))
  else documents.value.forEach((d) => s.delete(d.id))
  selectedIds.value = s
}

async function handleDelete(doc) {
  try {
    const { code, message } = await deleteInstanceDocument(instanceId.value, doc.id)
    if (code === 0) {
      ElMessage.success('已删除')
      const s = new Set(selectedIds.value); s.delete(doc.id); selectedIds.value = s
      await reloadAfterDelete(1)
    } else ElMessage.error(message || '删除失败')
  } catch (e) { ElMessage.error('删除失败: ' + (e.message || '')) }
}
async function showBatchDeleteConfirm() {
  try {
    await ElMessageBox.confirm(`确定删除选中的 ${selectedIds.value.size} 个文档？`, '批量删除', { confirmButtonText: '确定', cancelButtonText: '取消', type: 'warning' })
    await handleBatchDelete()
  } catch {}
}
async function handleBatchDelete() {
  const ids = [...selectedIds.value]
  batchDeleting.value = true
  const failed = []
  try {
    for (const id of ids) {
      try { const { code } = await deleteInstanceDocument(instanceId.value, id); if (code !== 0) failed.push(id) } catch { failed.push(id) }
    }
    if (ids.length - failed.length > 0) ElMessage.success(`已删除 ${ids.length - failed.length} 个`)
    if (failed.length > 0) ElMessage.error(`${failed.length} 个失败`)
    selectedIds.value = new Set(failed)
    await reloadAfterDelete(ids.length - failed.length)
  } finally { batchDeleting.value = false }
}
async function reloadAfterDelete(n) {
  const remain = total.value - n
  const maxPage = Math.max(1, Math.ceil(remain / pageSize.value))
  if (currentPage.value > maxPage) currentPage.value = maxPage
  await loadDocuments()
}

// ===== 上传 =====
const uploadOpen = ref(false)
const uploading = ref(false)
const uploadForm = reactive({ title: '', content: '', brand: '', dev_type: '', model: '' })
function resetUploadForm() { uploadForm.title = ''; uploadForm.content = ''; uploadForm.brand = ''; uploadForm.dev_type = ''; uploadForm.model = '' }
function openUpload() { resetUploadForm(); uploadOpen.value = true }
async function handleUpload() {
  if (!uploadForm.content.trim()) { ElMessage.warning('请输入内容'); return }
  uploading.value = true
  try {
    const { code, message } = await uploadInstanceDocument(instanceId.value, {
      title: uploadForm.title || '未命名文档',
      content: uploadForm.content,
      brand: uploadForm.brand.trim(),
      dev_type: uploadForm.dev_type.trim(),
      model: uploadForm.model.trim(),
    })
    if (code === 0) { ElMessage.success('上传成功'); uploadOpen.value = false; resetUploadForm(); await loadDocuments() }
    else ElMessage.error(message || '上传失败')
  } catch (e) { ElMessage.error('上传失败: ' + (e.message || '')) }
  finally { uploading.value = false }
}

function goBack() { router.push('/knowledge') }

onMounted(async () => {
  await Promise.all([loadInstances(), loadDocuments()])
})

watch(instanceId, (v) => {
  if (!v) return
  docSegments.value = {}
  currentPage.value = 1
  selectedIds.value = new Set()
  loadDocuments()
})
</script>

<style scoped>
.kb-header { align-items: center; }
.kb-title { font-size: 16px; font-weight: 800; color: var(--text-h); }
.doc-segments { padding: 14px 18px; }
.segments-loading, .segments-empty { font-size: 13px; color: var(--muted); padding: 12px 0; }
.segments-list { display: flex; flex-direction: column; gap: 10px; }
.segment-item { background: #0a0a12; border: 1px solid var(--border); border-radius: var(--radius-sm); overflow: hidden; }
.segment-header { display: flex; align-items: center; gap: 10px; padding: 8px 12px; background: rgba(0,212,255,0.04); border-bottom: 1px solid var(--border); font-size: 12px; }
.segment-idx { font-weight: 700; color: var(--accent); }
.segment-content { padding: 12px 14px; font-size: 13px; line-height: 1.6; font-family: var(--font-mono); white-space: pre-wrap; word-break: break-word; color: var(--text); margin: 0; overflow-x: auto; }
.page-total { font-size: 13px; color: var(--muted); font-weight: 500; }
.cell-muted { color: var(--muted); }
.cell-title { font-weight: 600; }
.upload-form { display: flex; flex-direction: column; gap: 16px; }
.form-row { display: flex; gap: 16px; }
.form-row .form-item { flex: 1; min-width: 0; }
.form-item { display: flex; flex-direction: column; gap: 6px; }
.form-label { font-size: 12px; font-weight: 700; color: var(--muted); text-transform: uppercase; letter-spacing: 0.5px; }
</style>
