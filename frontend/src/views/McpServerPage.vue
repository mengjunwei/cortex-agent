<template>
  <div class="page-root">
    <!-- 顶部操作栏 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <el-input
          v-model="searchKeyword"
          placeholder="搜索名称 / Slug / 端点"
          clearable
          size="small"
          style="width: 300px;"
          @keyup.enter="handleSearch"
          @clear="handleSearch"
        >
          <template #prefix>
            <el-icon><Search /></el-icon>
          </template>
        </el-input>
        <el-button size="small" @click="handleSearch">
          <el-icon><Search /></el-icon> 搜索
        </el-button>
        <el-button size="small" text @click="searchKeyword = ''; handleSearch()">
          <el-icon><Refresh /></el-icon> 重置
        </el-button>
      </div>
      <div class="page-toolbar-right">
        <el-button type="primary" size="small" @click="openServerDialog()">
          <el-icon><Plus /></el-icon> 新建 MCP 服务
        </el-button>
        <el-button size="small" @click="loadServers" :loading="loading">
          <el-icon><Refresh /></el-icon> 刷新
        </el-button>
      </div>
    </div>

    <!-- 说明条 -->
    <div class="info-banner">
      <el-icon class="info-icon"><InfoFilled /></el-icon>
      <span>MCP 服务作为<b>外部工具源</b>接入；stdio 启动子进程、http 直连远端。env / headers 仅可设置、不可查看。在<b>助手编辑页</b>勾选启用后，其工具以 <code>mcp__slug__tool</code> 命名空间注入。</span>
    </div>

    <!-- 批量操作工具栏（有选中项时显示） -->
    <transition name="el-zoom-in-top">
      <div v-if="hasSelection" class="batch-toolbar">
        <div class="batch-info">
          <template v-if="selectAllMode">
            <span class="batch-count">已选择全部 {{ total }} 项（跨页）</span>
            <el-button text size="small" type="primary" @click="clearSelection">清除选择</el-button>
            <span v-if="excludedIds.length" class="batch-exclude-hint">（已排除 {{ excludedIds.length }} 项）</span>
          </template>
          <template v-else>
            <span class="batch-count">已选中 {{ selectedIds.length }} 项</span>
            <el-button
              v-if="total > servers.length && total > selectedIds.length"
              text
              size="small"
              type="primary"
              @click="enterSelectAllMode"
            >
              选择全部 {{ total }} 项（跨页）
            </el-button>
            <el-button text size="small" @click="clearSelection">清除选择</el-button>
          </template>
        </div>
        <div class="batch-actions">
          <el-button
            size="small"
            :icon="Check"
            :disabled="!!batchLoading && batchLoading !== 'enable'"
            :loading="batchLoading === 'enable'"
            @click="handleBatchStatus(1)"
          >批量启用</el-button>
          <el-button
            size="small"
            :icon="CircleClose"
            :disabled="!!batchLoading && batchLoading !== 'disable'"
            :loading="batchLoading === 'disable'"
            @click="handleBatchStatus(0)"
          >批量禁用</el-button>
          <el-button
            size="small"
            type="primary"
            plain
            :icon="Connection"
            :disabled="selectAllMode || (!!batchLoading && batchLoading !== 'probe')"
            :loading="batchLoading === 'probe'"
            @click="handleBatchProbe"
          >
            {{ selectAllMode ? '探测不支持全选' : '批量探测' }}
          </el-button>
          <el-popconfirm
            :title="selectAllMode ? `确定删除全部 ${total} 项匹配的服务${excludedIds.length ? `（排除 ${excludedIds.length} 项）` : ''}？此操作不可撤销` : `确定删除选中的 ${selectedIds.length} 项服务？`"
            confirm-button-text="删除"
            cancel-button-text="取消"
            @confirm="handleBatchDelete"
          >
            <template #reference>
              <el-button
                size="small"
                type="danger"
                plain
                :icon="Delete"
                :disabled="!!batchLoading && batchLoading !== 'delete'"
                :loading="batchLoading === 'delete'"
              >批量删除</el-button>
            </template>
          </el-popconfirm>
        </div>
      </div>
    </transition>

    <!-- 服务表格 -->
    <div class="data-table-wrapper" v-loading="loading">
      <el-table
        ref="tableRef"
        class="data-table"
        :data="servers"
        row-key="id"
        height="100%"
        border
        @selection-change="handleSelectionChange"
        @select="handleSelect"
        @select-all="handleSelectAll"
      >
        <el-table-column type="selection" width="45" :selectable="() => true" reserve-selection />
        <el-table-column label="服务名称" min-width="200" show-overflow-tooltip>
          <template #default="{ row }">
            <div style="display:flex; align-items:center; gap:8px;">
              <span style="font-size:16px;">{{ row.transport === 1 ? '⚙️' : '🌐' }}</span>
              <div>
                <div class="cell-title">{{ row.name }}</div>
                <div class="cell-muted" style="font-size:11px; font-family: var(--font-mono);">{{ row.slug }}</div>
              </div>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="传输方式" width="120" align="center">
          <template #default="{ row }">
            <el-tag size="small" :type="row.transport === 1 ? 'info' : 'success'" effect="plain">
              {{ row.transport === 1 ? 'stdio' : 'streamable http' }}
            </el-tag>
          </template>
        </el-table-column>

        <el-table-column label="端点" min-width="260" show-overflow-tooltip>
          <template #default="{ row }">
            <code class="base-url-code">{{ row.endpoint }}</code>
          </template>
        </el-table-column>

        <el-table-column label="工具数" width="90" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.health && row.health.state === 'healthy'" size="small" type="info" effect="plain">
              {{ row.health.tools_count ?? 0 }}
            </el-tag>
            <span v-else class="cell-muted">—</span>
          </template>
        </el-table-column>

        <el-table-column label="健康状态" width="150" align="center">
          <template #default="{ row }">
            <el-tooltip
              :disabled="!row.health || row.health.state !== 'unhealthy' || !row.health.reason"
              :content="row.health.reason"
              placement="top"
              :show-after="200"
            >
              <div class="health-cell">
                <span class="health-dot" :class="healthClass(row.health)"></span>
                <span class="health-label" :class="healthClass(row.health)">{{ healthText(row.health) }}</span>
              </div>
            </el-tooltip>
          </template>
        </el-table-column>

        <el-table-column label="启用" width="80" align="center">
          <template #default="{ row }">
            <el-switch
              :model-value="row.status"
              :active-value="1"
              :inactive-value="0"
              @change="(v) => handleStatusChange(row, v)"
            />
          </template>
        </el-table-column>

        <el-table-column v-if="userStore.user?.is_admin" label="归属" width="120">
          <template #default="{ row }">
            <el-tag v-if="row.owner" size="small" type="warning" effect="plain">{{ row.owner }}</el-tag>
            <span v-else class="cell-muted">-</span>
          </template>
        </el-table-column>
        <el-table-column label="更新时间" width="150">
          <template #default="{ row }">
            <span class="cell-muted">{{ formatTime(row.updated_at) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="操作" width="250" align="center" fixed="right">
          <template #default="{ row }">
            <div class="row-actions" @click.stop>
              <el-button size="small" @click="openServerDialog(row)">编辑</el-button>
              <el-button size="small" type="primary" plain :loading="probingId === row.id" @click="handleProbe(row)">
                探测
              </el-button>
              <el-button size="small" @click="openToolsDialog(row)">工具</el-button>
              <el-button size="small" type="danger" plain @click="handleDelete(row)">删除</el-button>
            </div>
          </template>
        </el-table-column>

        <template #empty>
          <div class="table-empty">
            <div class="empty-icon">🔌</div>
            <div class="empty-title">暂无 MCP 服务</div>
            <div class="empty-hint">点击右上角「新建 MCP 服务」接入外部工具源</div>
          </div>
        </template>
      </el-table>
    </div>

    <!-- 分页 -->
    <div class="pagination-wrapper">
      <el-pagination
        v-model:current-page="currentPage"
        v-model:page-size="pageSize"
        :total="total"
        :page-sizes="[10, 20, 50]"
        layout="total, sizes, prev, pager, next, jumper"
        background
        @current-change="loadServers()"
        @size-change="handlePageSizeChange"
      />
    </div>

    <!-- 新建/编辑 对话框 -->
    <el-dialog
      v-model="serverDialogVisible"
      :title="serverForm.id ? '编辑 MCP 服务' : '新建 MCP 服务'"
      width="600px"
      :close-on-click-modal="false"
    >
      <el-form ref="serverFormRef" :model="serverForm" :rules="serverRules" label-width="100px">
        <el-form-item label="名称" prop="name">
          <el-input v-model="serverForm.name" placeholder="如「文件系统」" maxlength="64" show-word-limit />
        </el-form-item>
        <el-form-item label="传输方式" prop="transport">
          <el-radio-group v-model="serverForm.transport" @change="onTransportChange">
            <el-radio :value="1" border>stdio（本地子进程）</el-radio>
            <el-radio :value="2" border>Streamable HTTP</el-radio>
          </el-radio-group>
        </el-form-item>
        <el-form-item :label="serverForm.transport === 1 ? '命令路径' : '服务地址'" prop="endpoint">
          <el-input
            v-model="serverForm.endpoint"
            :placeholder="serverForm.transport === 1
              ? '可执行文件绝对路径，如 npx（禁止 shell 元字符）'
              : 'https://example.com/mcp'"
          />
        </el-form-item>

        <!-- stdio 独有：启动参数 -->
        <template v-if="serverForm.transport === 1">
          <el-form-item label="启动参数">
            <div class="kv-list">
              <div v-for="(arg, i) in serverForm.args" :key="i" class="arg-row">
                <el-input v-model="serverForm.args[i]" placeholder="如 @modelcontextprotocol/server-filesystem /tmp" size="small" />
                <el-button size="small" text type="danger" :icon="Delete" @click="serverForm.args.splice(i, 1)" />
              </div>
              <el-button size="small" text type="primary" :icon="Plus" @click="serverForm.args.push('')">添加参数</el-button>
            </div>
          </el-form-item>
        </template>

        <!-- 环境变量（两种传输均可用；编辑态按变量回显掩码，值留空=保留、输入=覆盖、删行=移除） -->
        <el-form-item label="环境变量">
          <div class="kv-list">
            <div class="kv-tip">
              <el-icon><InfoFilled /></el-icon>
              <span>{{ serverForm.id ? '已配置项值以密文保存、不可查看：留空=保留原值，输入新值=覆盖该变量，删除行=移除；改键名=删除原变量、需填新值。' : '将以密文存储，保存后不可查看。' }}</span>
            </div>
            <div v-for="(item, i) in serverForm.envPairs" :key="'env' + i" class="kv-row">
              <el-input v-model="item.key" placeholder="变量名" size="small" style="flex:1;" />
              <el-input v-model="item.value" :placeholder="item.masked && item.key === item.origKey ? `已设置 ${item.masked}，留空保持不变` : '变量值'" size="small" style="flex:1.4;" />
              <el-button size="small" text type="danger" :icon="Delete" @click="serverForm.envPairs.splice(i, 1)" />
            </div>
            <el-button size="small" text type="primary" :icon="Plus" @click="serverForm.envPairs.push({ key: '', value: '' })">添加变量</el-button>
          </div>
        </el-form-item>

        <!-- HTTP 独有：自定义请求头（编辑态语义同环境变量） -->
        <template v-if="serverForm.transport === 2">
          <el-form-item label="请求头">
            <div class="kv-list">
              <div class="kv-tip">
                <el-icon><InfoFilled /></el-icon>
                <span>{{ serverForm.id ? '已配置项值以密文保存、不可查看：留空=保留原值，输入新值=覆盖该请求头，删除行=移除；改键名=删除原请求头、需填新值。' : '将以密文存储，保存后不可查看。' }}</span>
              </div>
              <div v-for="(item, i) in serverForm.headerPairs" :key="'hdr' + i" class="kv-row">
                <el-input v-model="item.key" placeholder="头名，如 Authorization" size="small" style="flex:1;" />
                <el-input v-model="item.value" :placeholder="item.masked && item.key === item.origKey ? `已设置 ${item.masked}，留空保持不变` : '头值'" size="small" style="flex:1.4;" />
                <el-button size="small" text type="danger" :icon="Delete" @click="serverForm.headerPairs.splice(i, 1)" />
              </div>
              <el-button size="small" text type="primary" :icon="Plus" @click="serverForm.headerPairs.push({ key: '', value: '' })">添加请求头</el-button>
            </div>
          </el-form-item>
        </template>

        <el-form-item label="工具超时(秒)">
          <el-input-number
            v-model="serverForm.tool_timeout_secs"
            :min="1"
            :max="3600"
            :step="10"
            controls-position="right"
            style="width: 140px;"
          />
          <span class="form-hint">单次工具调用超时，默认 60（卡死时按此返回错误，不无限阻塞）</span>
        </el-form-item>
        <el-form-item label="状态" prop="status">
          <el-switch v-model="serverForm.status" :active-value="1" :inactive-value="0" />
          <span class="form-hint">{{ serverForm.status === 1 ? '启用' : '禁用' }}</span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="serverDialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="submitting" @click="submitServer">保存</el-button>
      </template>
    </el-dialog>

    <!-- 工具清单 对话框 -->
    <el-dialog
      v-model="toolsDialogVisible"
      :title="`${currentToolsServer?.name || ''} · 工具清单`"
      width="640px"
    >
      <div v-loading="toolsLoading">
        <el-alert v-if="toolsError" type="error" :title="toolsError" :closable="false" show-icon style="margin-bottom:12px;" />
        <div v-if="toolsList.length" class="tools-dialog-list">
          <div v-for="t in toolsList" :key="t.namespaced_name" class="tool-item">
            <div class="tool-item-head">
              <code class="tool-ns">{{ t.namespaced_name }}</code>
            </div>
            <div class="tool-item-desc">{{ t.description || '（无描述）' }}</div>
          </div>
        </div>
        <el-empty v-else-if="!toolsLoading && !toolsError" description="未发现工具，请先「探测」确认连接正常" :image-size="70" />
      </div>
      <template #footer>
        <el-button @click="toolsDialogVisible = false">关闭</el-button>
        <el-button type="primary" :loading="toolsLoading" :disabled="!currentToolsServer" @click="reloadTools">重新加载</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { ref, computed, onMounted, onUnmounted, nextTick, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { confirmDeleteWithImpact } from '../composables/useDeleteWithImpact'
import { Search, Refresh, Plus, InfoFilled, Delete, Check, CircleClose, Connection } from '@element-plus/icons-vue'
import {
  fetchMcpServers,
  createMcpServer,
  updateMcpServer,
  deleteMcpServer,
  probeMcpServer,
  fetchMcpTools,
  batchSetMcpStatus,
  batchDeleteMcpServers,
  batchProbeMcpServers,
} from '../api'
import { useUserStore } from '../stores/user'

const userStore = useUserStore()

const servers = ref([])
const loading = ref(false)
const submitting = ref(false)
const probingId = ref('')
const searchKeyword = ref('')
const appliedKeyword = ref('')
const tableRef = ref()

// 分页状态
const currentPage = ref(1)
const pageSize = ref(10)
const total = ref(0)

// 选择状态（支持跨页全选）
const selectedIds = ref([])
const selectAllMode = ref(false)
const excludedIds = ref([])
const batchLoading = ref('')

const hasSelection = computed(() => {
  return selectAllMode.value || selectedIds.value.length > 0
})

function handleSearch() {
  appliedKeyword.value = searchKeyword.value.trim()
  currentPage.value = 1
  clearSelection()
  loadServers()
}

function handlePageSizeChange() {
  currentPage.value = 1
  clearSelection()
  loadServers()
}

// ========== 选择管理（跨页全选） ==========
function handleSelectionChange(selection) {
  if (!selectAllMode.value) {
    const currentPageIds = servers.value.map(s => s.id)
    const newSet = new Set(selectedIds.value.filter(id => !currentPageIds.includes(id)))
    for (const row of selection) newSet.add(row.id)
    selectedIds.value = [...newSet]
  }
}

function handleSelect(selection, row) {
  if (selectAllMode.value) {
    const idx = excludedIds.value.indexOf(row.id)
    const isSelected = selection.some(r => r.id === row.id)
    if (!isSelected && idx === -1) {
      excludedIds.value.push(row.id)
    } else if (isSelected && idx >= 0) {
      excludedIds.value.splice(idx, 1)
    }
  }
}

function handleSelectAll(selection) {
  if (selectAllMode.value) {
    const currentPageIds = servers.value.map(s => s.id)
    const allSelected = selection.length === servers.value.length
    if (allSelected) {
      excludedIds.value = excludedIds.value.filter(id => !currentPageIds.includes(id))
    } else {
      for (const id of currentPageIds) {
        if (!excludedIds.value.includes(id)) excludedIds.value.push(id)
      }
    }
  }
}

function clearSelection() {
  selectedIds.value = []
  selectAllMode.value = false
  excludedIds.value = []
  tableRef.value?.clearSelection()
}

// 进入跨页全选模式：立即把表格勾选同步为「本页全选中（除已排除项）」，
// 否则表格仍停留在进入前的部分勾选态，与工具栏「已选择全部 N 项」文案不一致
function enterSelectAllMode() {
  selectAllMode.value = true
  syncTableSelection()
}

function syncTableSelection() {
  nextTick(() => {
    if (!tableRef.value) return
    if (selectAllMode.value) {
      for (const row of servers.value) {
        if (!excludedIds.value.includes(row.id)) {
          tableRef.value.toggleRowSelection(row, true)
        } else {
          tableRef.value.toggleRowSelection(row, false)
        }
      }
    } else {
      for (const row of servers.value) {
        tableRef.value.toggleRowSelection(row, selectedIds.value.includes(row.id))
      }
    }
  })
}

// ========== 批量操作 ==========
// 拉全当前关键字匹配的服务 id（跨页全选 + 有排除项时的精确操作集）。
// 任一页业务失败 / 超出兜底页数未拉全 → 返回 null：带着残缺集合上送，
// 「已排除」项之外的真实成员会被漏掉（删除场景即误删范围失真）
async function collectAllMatchedIds() {
  const ids = []
  let page = 1
  while (page <= 50) {
    const { data, code } = await fetchMcpServers(page, 100, appliedKeyword.value)
    if (code !== 0) return null
    const list = data.servers || []
    ids.push(...list.map(s => s.id))
    if (ids.length >= (data.total || 0) || list.length < 100) return ids
    page++
  }
  return null
}

// 跨页全选模式下构造批量操作入参：无排除 → keyword 整体匹配（后端原生语义）；
// 有排除 → 显式 id 集合（全量减排除）。后端接口没有排除参数，直接发 keyword
// 会把用户明确取消勾选的服务一并改掉/删掉
async function buildBatchInput() {
  if (!selectAllMode.value) return { ids: selectedIds.value }
  if (!excludedIds.value.length) return { ids: null, keyword: appliedKeyword.value }
  const all = await collectAllMatchedIds()
  if (!all) return { error: '获取全量服务列表失败，操作已取消' }
  const excluded = new Set(excludedIds.value)
  const ids = all.filter(id => !excluded.has(id))
  if (!ids.length) return { error: '排除后没有可操作的服务', skipped: true }
  return { ids }
}

async function handleBatchStatus(status) {
  if (batchLoading.value) return
  batchLoading.value = status === 1 ? 'enable' : 'disable'
  try {
    const built = await buildBatchInput()
    if (built.error) {
      if (built.skipped) ElMessage.info(built.error)
      else ElMessage.error(built.error)
      return
    }
    const input = { ...built, status }
    const { code, message, data } = await batchSetMcpStatus(input)
    if (code === 0) {
      ElMessage.success(`${status === 1 ? '启用' : '禁用'} ${data.affected} 项`)
      clearSelection()
      await loadServers()
    } else {
      ElMessage.error(message || '批量操作失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    batchLoading.value = ''
  }
}

async function handleBatchDelete() {
  if (batchLoading.value) return
  batchLoading.value = 'delete'
  try {
    const built = await buildBatchInput()
    if (built.error) {
      if (built.skipped) ElMessage.info(built.error)
      else ElMessage.error(built.error)
      return
    }
    const { code, message, data } = await batchDeleteMcpServers(built)
    if (code === 0) {
      ElMessage.success(`已删除 ${data.affected} 项`)
      clearSelection()
      clampPageAfterDelete(data.affected || 0)
      await loadServers()
    } else {
      ElMessage.error(message || '批量删除失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    batchLoading.value = ''
  }
}

// 删除后按剩余总量收敛页码：整页删空（跨页全选按关键字删、删末页最后一条）时
// 沿用旧页码会请求到空页（后端只 clamp 下限不回退页码，列表显示为空白）
function clampPageAfterDelete(affected) {
  const remain = Math.max((total.value || 0) - affected, 0)
  const maxPage = Math.max(1, Math.ceil(remain / (pageSize.value || 10)))
  if (currentPage.value > maxPage) currentPage.value = maxPage
}

async function handleBatchProbe() {
  if (batchLoading.value) return
  batchLoading.value = 'probe'
  try {
    const { code, message } = await batchProbeMcpServers(selectedIds.value)
    if (code === 0) {
      ElMessage.success('批量探测完成')
      clearSelection()
      await loadServers()
    } else {
      ElMessage.error(message || '批量探测失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    batchLoading.value = ''
  }
}

// 请求序号：翻页/改关键词时快速连续触发，先发的后到会把新数据覆盖成旧页
let loadServersSeq = 0

async function loadServers(silent = false) {
  const seq = ++loadServersSeq
  if (!silent) loading.value = true
  try {
    const { data, code, message } = await fetchMcpServers(
      currentPage.value,
      pageSize.value,
      appliedKeyword.value,
    )
    if (seq !== loadServersSeq) return // 已被更新的请求取代，丢弃过期回包
    if (code === 0) {
      servers.value = data.servers || []
      total.value = data.total || 0
      if (data.page) currentPage.value = data.page
      if (data.page_size) pageSize.value = data.page_size
      syncTableSelection()
    } else {
      if (!silent) ElMessage.error(message || '加载 MCP 服务失败')
    }
  } catch (e) {
    if (!silent) ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    if (!silent) loading.value = false
  }
}

// ========== 自动轮询：进入页面后对 Unknown 状态服务持续刷新 ==========
// 后端启动后异步探测所有已启用服务，但探测需要时间（连接 + list_tools）。
// 前端在探测完成前会看到 Unknown，因此每 3 秒静默刷新一次，直到状态确定或超时。
const AUTO_REFRESH_INTERVAL = 3000
const AUTO_REFRESH_MAX = 5
let autoRefreshTimer = null
let autoRefreshCount = 0

function hasUnknownEnabledServers() {
  return servers.value.some(
    s => s.status === 1 && (!s.health || s.health.state === 'unknown'),
  )
}

async function loadServersWithAutoRefresh() {
  await loadServers(true)
  if (hasUnknownEnabledServers() && autoRefreshCount < AUTO_REFRESH_MAX) {
    autoRefreshCount++
    autoRefreshTimer = setTimeout(loadServersWithAutoRefresh, AUTO_REFRESH_INTERVAL)
  }
}

// ========== 健康状态映射 ==========
function healthClass(h) {
  const s = h && h.state ? h.state : 'unknown'
  return {
    'dot-unknown': s === 'unknown',
    'dot-healthy': s === 'healthy',
    'dot-degraded': s === 'degraded',
    'dot-unhealthy': s === 'unhealthy',
  }
}
function healthText(h) {
  const s = h && h.state ? h.state : 'unknown'
  switch (s) {
    case 'healthy': return '在线'
    case 'degraded': return `降级 ${h.consecutive_failures || 1}`
    case 'unhealthy': return '离线'
    default: return '未知'
  }
}

// ========== 状态切换 ==========
// 仅切换启用状态：必须基于当前行的真实数据构造 payload（后端 name/transport/endpoint 必填），
// env/headers 传 null 表示保留原值。切勿用 serverForm（可能为空或残留）。
async function handleStatusChange(row, value) {
  try {
    const payload = {
      name: row.name,
      transport: row.transport,
      endpoint: row.endpoint,
      args: Array.isArray(row.args) ? row.args : [],
      env: null,
      headers: null,
      status: value,
      // 必须回传当前超时：后端 UPDATE 无条件写该列，缺省（serde 默认 60）会把
      // 用户配置的自定义超时在状态切换时静默重置
      tool_timeout_secs: row.tool_timeout_secs ?? 60,
    }
    const { code, message } = await updateMcpServer(row.id, payload)
    if (code === 0) {
      row.status = value
      ElMessage.success(value === 1 ? '已启用' : '已禁用')
      await loadServers()
    } else {
      ElMessage.error(message || '状态更新失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  }
}

async function handleProbe(row) {
  probingId.value = row.id
  try {
    const { code, message } = await probeMcpServer(row.id)
    if (code === 0) {
      ElMessage.success('探测完成')
      await loadServers()
    } else {
      ElMessage.error(message || '探测失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    probingId.value = ''
  }
}

async function handleDelete(row) {
  await confirmDeleteWithImpact({
    id: row.id,
    removeFn: deleteMcpServer,
    title: '删除 MCP 服务',
    targetLabel: row.name,
    onSuccess: () => {
      clampPageAfterDelete(1)
      loadServers()
    },
  })
}

// ========== 新建/编辑 表单 ==========
const serverDialogVisible = ref(false)
const serverFormRef = ref()
const serverForm = ref(makeEmptyForm())
const serverRules = {
  name: [{ required: true, message: '请输入名称', trigger: 'blur' }],
  transport: [{ required: true, message: '请选择传输方式', trigger: 'change' }],
  endpoint: [{ required: true, message: '请输入端点', trigger: 'blur' }],
}

function makeEmptyForm() {
  return {
    id: '',
    name: '',
    transport: 2,
    endpoint: '',
    args: [],
    envPairs: [],
    headerPairs: [],
    status: 1,
    tool_timeout_secs: 60,
  }
}

function onTransportChange() {
  // 切换传输方式时清空不相关字段，避免误传
}

// 把后端响应的脱敏 map 转为编辑行：key 回显、value 留空（placeholder 显示掩码提示「已设置」）。
// origKey 记录该行来自哪个已存键——键名被改动后 masked 提示失效（改名=删旧键的新变量）
function maskedPairs(map) {
  if (!map) return []
  return Object.entries(map).map(([k, v]) => ({ key: k, value: '', masked: v, origKey: k }))
}

function openServerDialog(row) {
  if (row) {
    serverForm.value = {
      id: row.id,
      name: row.name,
      transport: row.transport,
      endpoint: row.endpoint,
      args: Array.isArray(row.args) ? [...row.args] : [],
      envPairs: maskedPairs(row.env),
      headerPairs: maskedPairs(row.headers),
      status: row.status,
      tool_timeout_secs: row.tool_timeout_secs ?? 60,
    }
  } else {
    serverForm.value = makeEmptyForm()
  }
  // 清掉上一次打开残留的校验错误（换 model 不重置 el-form 的验证态，
  // 重开弹窗会显示已修复字段的红字提示）
  nextTick(() => serverFormRef.value?.clearValidate())
  serverDialogVisible.value = true
}

// 收集键值对为 map（新建用）：忽略空 key 与空 value
function collectPairs(pairs) {
  const out = {}
  for (const p of pairs) {
    const k = (p.key || '').trim()
    if (!k) continue
    const v = p.value || ''
    if (v === '') continue
    out[k] = v
  }
  return out
}

// 该行是否仍指向已存储的键：键名未改动时 masked 提示才有效（改回原名同样恢复保留语义）
function isExistingVar(p) {
  return !!p.masked && (p.key || '').trim() === p.origKey
}

// 收集编辑载荷（按键合并，与后端 merge_secret_map 精确对齐）：
// - 已有键（键名未改）值留空 → null（保留原密文）
// - 新键值留空 → 跳过（不产生条目）
// - 键缺席（用户删行/改键名离开原键）→ 不出现在 map 中 → 后端删除该键
function collectPairsForUpdate(pairs) {
  const out = {}
  for (const p of pairs) {
    const k = (p.key || '').trim()
    if (!k) continue
    const v = p.value || ''
    if (v === '' && !isExistingVar(p)) continue
    out[k] = v === '' ? null : v
  }
  return out
}

// 提交前校验键值对（新建/编辑共用），返回错误文案（空串=通过）：
// - 重复键名：对象构造后者覆盖前者，结果依赖行顺序 → 直接报错
// - 改名未填值：改名=删旧键的新变量，空值行会被忽略 → 原变量静默丢失，必须显式填值或删行
function validatePairs(pairs, label) {
  const seen = new Set()
  for (const p of pairs) {
    const k = (p.key || '').trim()
    if (!k) continue
    if (seen.has(k)) return `${label}存在重复键名：${k}`
    seen.add(k)
    if (p.origKey && k !== p.origKey && !(p.value || '').trim()) {
      return `${label}「${p.origKey}」已改名为「${k}」但未填值：原变量将被删除，请填新值或删除该行`
    }
  }
  return ''
}

// 构造编辑载荷（基于表单）：env 总是传完整键集（删行才生效）；
// headers 仅 http 传输传（stdio 不涉及，传 null 保留原值不动）
function buildFormUpdatePayload() {
  const f = serverForm.value
  return {
    name: f.name.trim(),
    transport: f.transport,
    endpoint: f.endpoint.trim(),
    args: f.transport === 1 ? f.args.filter(a => (a || '').trim() !== '') : [],
    env: collectPairsForUpdate(f.envPairs),
    headers: f.transport === 2 ? collectPairsForUpdate(f.headerPairs) : null,
    status: f.status,
    tool_timeout_secs: f.tool_timeout_secs,
  }
}

async function submitServer() {
  await serverFormRef.value?.validate(async (valid) => {
    if (!valid) return
    // 键值对边界校验：重复键名 / 改名未填值（防止静默丢变量或行顺序决定结果）
    const f = serverForm.value
    const envErr = validatePairs(f.envPairs, '环境变量')
    if (envErr) return ElMessage.warning(envErr)
    if (f.transport === 2) {
      const hdrErr = validatePairs(f.headerPairs, '请求头')
      if (hdrErr) return ElMessage.warning(hdrErr)
    }
    submitting.value = true
    try {
      if (f.id) {
        // 编辑：env 按键合并（留空=保留、输入=覆盖、删行=移除）
        const payload = buildFormUpdatePayload()
        const { code, message } = await updateMcpServer(f.id, payload)
        if (code === 0) {
          ElMessage.success('MCP 服务已更新')
          serverDialogVisible.value = false
          await loadServers()
        } else {
          ElMessage.error(message || '更新失败')
        }
      } else {
        // 新建：env/headers 均为完整 map（默认 {}）
        const payload = {
          name: f.name.trim(),
          transport: f.transport,
          endpoint: f.endpoint.trim(),
          args: f.transport === 1 ? f.args.filter(a => (a || '').trim() !== '') : [],
          env: collectPairs(f.envPairs),
          headers: f.transport === 2 ? collectPairs(f.headerPairs) : {},
          status: f.status,
          tool_timeout_secs: f.tool_timeout_secs,
        }
        const { code, message } = await createMcpServer(payload)
        if (code === 0) {
          ElMessage.success('MCP 服务已创建')
          serverDialogVisible.value = false
          await loadServers()
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

// ========== 工具清单 ==========
const toolsDialogVisible = ref(false)
const toolsLoading = ref(false)
const toolsList = ref([])
const toolsError = ref('')
const currentToolsServer = ref(null)

async function openToolsDialog(row) {
  currentToolsServer.value = row
  toolsDialogVisible.value = true
  await reloadTools()
}

async function reloadTools() {
  if (!currentToolsServer.value) return
  // 快照本次请求的 server id：await 期间用户可能已关闭弹窗或切到另一个服务的
  // 工具清单，回包若不核对会把 A 的结果/错误写进 B 的弹窗
  const sid = currentToolsServer.value.id
  toolsLoading.value = true
  toolsError.value = ''
  toolsList.value = []
  try {
    const { data, code, message } = await fetchMcpTools([sid])
    if (currentToolsServer.value?.id !== sid) return
    if (code === 0) {
      const map = (data && data.tools) || {}
      toolsList.value = map[sid] || []
      if (!toolsList.value.length) {
        toolsError.value = '该服务暂未返回工具，可能尚未连接，请先点击「探测」。'
      }
    } else {
      toolsError.value = message || '加载工具失败'
    }
  } catch (e) {
    if (currentToolsServer.value?.id === sid) {
      toolsError.value = '请求失败: ' + (e.message || '网络错误')
    }
  } finally {
    if (currentToolsServer.value?.id === sid) toolsLoading.value = false
  }
}

function formatTime(iso) {
  if (!iso) return ''
  const d = new Date(iso)
  return d.toLocaleString('zh-CN', { year: 'numeric', month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
}

onMounted(() => {
  autoRefreshCount = 0
  loadServersWithAutoRefresh()
})

onUnmounted(() => {
  if (autoRefreshTimer) clearTimeout(autoRefreshTimer)
})
</script>

<style scoped>
.batch-toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 8px 14px;
  margin-bottom: 12px;
  border-radius: var(--radius);
  background: var(--card-bg);
  border: 1px solid var(--border);
}
.batch-info {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
}
.batch-count { font-weight: 600; color: var(--text); }
.batch-exclude-hint { color: var(--muted); font-size: 12px; }
.batch-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}
.pagination-wrapper {
  display: flex;
  justify-content: flex-end;
  padding: 12px 0 4px;
}
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
.info-banner .info-icon { color: var(--accent); font-size: 15px; flex-shrink: 0; }
.info-banner b { color: var(--text); font-weight: 700; }
.info-banner code {
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--accent);
  background: rgba(0, 212, 255, 0.08);
  padding: 1px 5px;
  border-radius: 4px;
}

.base-url-code {
  font-family: var(--font-mono);
  font-size: 12px;
  color: var(--text);
  word-break: break-all;
}

/* 健康状态指示 */
.health-cell { display: flex; align-items: center; gap: 6px; justify-content: center; }
.health-dot {
  width: 8px; height: 8px; border-radius: 50%; display: inline-block;
}
.health-dot.dot-unknown { background: var(--muted); }
.health-dot.dot-healthy { background: #10b981; box-shadow: 0 0 6px rgba(16, 185, 129, 0.6); }
.health-dot.dot-degraded { background: #f59e0b; box-shadow: 0 0 6px rgba(245, 158, 11, 0.6); }
.health-dot.dot-unhealthy { background: #ef4444; box-shadow: 0 0 6px rgba(239, 68, 68, 0.6); }
.health-label { font-size: 12px; font-weight: 600; }
.health-label.dot-unknown { color: var(--muted); }
.health-label.dot-healthy { color: #10b981; }
.health-label.dot-degraded { color: #f59e0b; }
.health-label.dot-unhealthy { color: #ef4444; }
.health-cell:has(.dot-unhealthy) { cursor: help; }

.form-hint { margin-left: 10px; font-size: 12px; color: var(--muted); }

/* 键值对编辑列表 */
.kv-list { display: flex; flex-direction: column; gap: 8px; width: 100%; }
.kv-tip {
  display: flex; align-items: center; gap: 6px;
  font-size: 11px; color: var(--muted);
  padding: 6px 8px; background: var(--bg-elevated); border-radius: 6px;
  border: 1px dashed var(--border);
}
.kv-row, .arg-row { display: flex; align-items: center; gap: 8px; }

.tools-dialog-list { display: flex; flex-direction: column; gap: 10px; }
.tool-item {
  padding: 10px 12px; border-radius: 8px;
  background: var(--bg-elevated); border: 1px solid var(--border);
}
.tool-item-head { margin-bottom: 4px; }
.tool-ns {
  font-family: var(--font-mono); font-size: 12px; font-weight: 600;
  color: var(--accent); background: rgba(0, 212, 255, 0.08);
  padding: 1px 6px; border-radius: 4px;
}
.tool-item-desc { font-size: 12px; color: var(--muted); line-height: 1.5; }

:deep(.el-radio.is-bordered) { margin-right: 8px; }
</style>
