<template>
  <div class="page-root">
    <!-- 顶部操作栏 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <el-input
          v-model="searchKeyword"
          placeholder="搜索插件 ID 或描述"
          clearable
          size="small"
          style="width: 280px;"
          @keyup.enter="onSearch"
          @clear="onSearch"
        >
          <template #prefix>
            <el-icon><Search /></el-icon>
          </template>
        </el-input>
        <el-button size="small" @click="onSearch">
          <el-icon><Search /></el-icon> 搜索
        </el-button>
        <el-button size="small" text @click="resetFilter">
          <el-icon><Refresh /></el-icon> 重置
        </el-button>
      </div>

      <div class="page-toolbar-right">
        <el-select
          v-model="filterStatus"
          placeholder="状态筛选"
          clearable
          size="small"
          style="width: 120px;"
          @change="onSearch"
        >
          <el-option label="全部" value="" />
          <el-option label="已启用" value="enabled" />
          <el-option label="已禁用" value="disabled" />
        </el-select>
      </div>
    </div>

    <!-- 表格 -->
    <div class="data-table-wrapper" v-loading="pluginsLoading">
      <el-table
        class="data-table"
        :data="filteredPlugins"
        row-key="plugin_id"
        height="100%"
        stripe
        border
      >
        <template #empty>
          <div class="table-empty">
            <div class="empty-icon">📭</div>
            <div class="empty-title">暂无已注册的监控插件</div>
            <div class="empty-hint">请在「智能对话」中由 AI 助手生成并注册插件</div>
          </div>
        </template>

        <el-table-column label="插件 ID" min-width="240" sortable prop="plugin_id" show-overflow-tooltip>
          <template #default="{ row }">
            <div style="display: flex; align-items: center; gap: 8px;">
              <span style="font-size: 16px;">🔌</span>
              <span class="cell-title">{{ row.plugin_id }}</span>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="描述" min-width="280" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="cell-muted">{{ row.description || '—' }}</span>
          </template>
        </el-table-column>

        <el-table-column label="版本" width="110" align="center" sortable prop="version">
          <template #default="{ row }">
            <el-tag size="small" type="info" effect="plain">v{{ row.version }}</el-tag>
          </template>
        </el-table-column>

        <el-table-column label="注册时间" width="170" sortable>
          <template #default="{ row }">
            <span class="cell-muted">{{ formatTime(row.registered_at) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="操作" width="240" align="center" fixed="right">
          <template #default="{ row }">
            <div class="row-actions" @click.stop>
              <el-button size="small" @click="goTest(row.plugin_id)">测试</el-button>
              <el-button size="small" @click="goVersions(row.plugin_id)">版本</el-button>
              <el-popconfirm
                title="确定要注销此插件吗？此操作不可撤销。"
                confirm-button-text="注销"
                cancel-button-text="取消"
                @confirm="handleUnregister(row.plugin_id)"
              >
                <template #reference>
                  <el-button size="small" type="danger" plain>注销</el-button>
                </template>
              </el-popconfirm>
            </div>
          </template>
        </el-table-column>
      </el-table>
    </div>

    <!-- 分页 -->
    <div class="page-pagination" v-if="allPlugins.length > 0">
      <span class="page-total">共 {{ allPlugins.length }} 条</span>
      <el-pagination
        background
        layout="sizes, prev, pager, next, jumper"
        :total="allPlugins.length"
        :page-size="pageSize"
        :page-sizes="[10, 20, 50]"
        :current-page="currentPage"
        @current-change="onPageChange"
        @size-change="onSizeChange"
      />
    </div>
  </div>
</template>

<script setup>
import { ref, computed, onMounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Refresh, Search } from '@element-plus/icons-vue'
import { fetchPlugins, unregisterPlugin } from '../api'

const route = useRoute()
const router = useRouter()

const plugins = ref([])
const pluginsLoading = ref(false)
const searchKeyword = ref('')
const filterStatus = ref('')
const currentPage = ref(1)
const pageSize = ref(20)

// ── URL query 与本地状态双向同步 ──
// onMounted 时从 URL 恢复（刷新 / 从详情页返回都能保持筛选状态）
function restoreFromQuery() {
  const q = route.query
  if (q.kw) searchKeyword.value = String(q.kw)
  if (q.status) filterStatus.value = String(q.status)
  if (q.page) currentPage.value = parseInt(q.page, 10) || 1
  if (q.size) pageSize.value = parseInt(q.size, 10) || 20
}

// 把当前筛选状态写回 URL（replace，不污染历史栈）
function syncToQuery() {
  const query = {}
  if (searchKeyword.value) query.kw = searchKeyword.value
  if (filterStatus.value) query.status = filterStatus.value
  if (currentPage.value !== 1) query.page = currentPage.value
  if (pageSize.value !== 20) query.size = pageSize.value
  router.replace({ path: '/monitor', query })
}

// 状态变化时同步到 URL（watch 自动追踪）
watch([searchKeyword, filterStatus, currentPage, pageSize], () => {
  syncToQuery()
})

const allPlugins = computed(() => {
  let result = plugins.value
  if (searchKeyword.value.trim()) {
    const kw = searchKeyword.value.trim().toLowerCase()
    result = result.filter(p =>
      (p.plugin_id || '').toLowerCase().includes(kw) ||
      (p.description || '').toLowerCase().includes(kw),
    )
  }
  if (filterStatus.value) {
    const wantEnabled = filterStatus.value === 'enabled'
    result = result.filter(p => p.enabled === wantEnabled)
  }
  return result
})

const filteredPlugins = computed(() => {
  const start = (currentPage.value - 1) * pageSize.value
  return allPlugins.value.slice(start, start + pageSize.value)
})

async function loadPlugins() {
  pluginsLoading.value = true
  try {
    const { data, code, message } = await fetchPlugins()
    if (code === 0) {
      plugins.value = data.plugins || []
    } else {
      ElMessage.error(message || '加载插件失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    pluginsLoading.value = false
  }
}

async function handleUnregister(pluginId) {
  try {
    const { code, message } = await unregisterPlugin({ plugin_id: pluginId })
    if (code === 0) {
      ElMessage.success(`插件 "${pluginId}" 已注销`)
      await loadPlugins()
    } else {
      ElMessage.error('注销失败: ' + (message || '未知错误'))
    }
  } catch (e) {
    ElMessage.error('注销请求失败: ' + (e.message || '网络错误'))
  }
}

function onSearch() {
  currentPage.value = 1
}

function resetFilter() {
  searchKeyword.value = ''
  filterStatus.value = ''
  currentPage.value = 1
  loadPlugins()
}

function onPageChange(page) {
  currentPage.value = page
}

function onSizeChange(size) {
  pageSize.value = size
  currentPage.value = 1
}

function goTest(pluginId) {
  // 记住列表页完整 URL（含筛选 query），供详情页返回时恢复
  sessionStorage.setItem('monitor_list_url', route.fullPath)
  router.push(`/monitor/test/${pluginId}`)
}

function goVersions(pluginId) {
  sessionStorage.setItem('monitor_list_url', route.fullPath)
  router.push(`/monitor/versions/${pluginId}`)
}

function formatTime(iso) {
  if (!iso) return ''
  const d = new Date(iso)
  return d.toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
}

onMounted(() => {
  restoreFromQuery()
  loadPlugins()
})
</script>

<style scoped>
.page-total {
  font-size: 13px;
  color: var(--muted);
  font-weight: 500;
}
</style>
