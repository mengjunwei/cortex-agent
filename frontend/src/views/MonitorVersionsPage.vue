<template>
  <div class="monitor-detail-page">
    <div class="detail-header">
      <el-button size="small" @click="goBack">
        <el-icon><ArrowLeft /></el-icon> 返回插件列表
      </el-button>
      <div class="detail-title">
        <span class="title-icon">🗂️</span>
        <span>版本管理 - {{ pluginId }}</span>
      </div>
    </div>

    <div class="panel-card" v-loading="versionsLoading">
      <div class="version-info" v-if="activeVersion">
        当前生效版本: <strong>v{{ activeVersion }}</strong> | 历史版本: {{ versions.length }}
      </div>
      <div v-if="versions.length === 0 && !versionsLoading" class="empty-text">暂无版本记录</div>

      <div class="versions-items">
        <div
          v-for="v in versions"
          :key="v.version"
          class="version-card"
          :class="{ active: v.version === activeVersion }"
        >
          <div class="version-top">
            <div class="version-left">
              <span class="version-num" :class="{ 'is-active': v.version === activeVersion }">
                v{{ v.version }}
              </span>
              <el-tag v-if="v.version === activeVersion" size="small" type="success" effect="dark">当前</el-tag>
              <span class="version-time">{{ v.registered_at || '' }}</span>
            </div>
            <div class="version-actions">
              <el-button size="small" @click="handleViewSource(pluginId)">查看源码</el-button>
              <el-button
                v-if="v.version !== activeVersion"
                size="small"
                type="warning"
                plain
                :loading="rollbackLoading"
                @click="handleRollback(pluginId, v.version)"
              >
                回滚到此版本
              </el-button>
            </div>
          </div>
          <div v-if="v.change_description" class="version-change">
            <span class="change-label">变更说明：</span>
            <span class="change-text">{{ v.change_description }}</span>
          </div>
          <div v-else class="version-change empty">
            <span class="change-label">无变更说明</span>
          </div>
        </div>
      </div>
    </div>

    <!-- 源码查看弹窗 -->
    <el-dialog v-model="sourceDialogVisible" title="插件源码" width="700px" :close-on-click-modal="true">
      <div class="source-dialog-title">{{ pluginId }}</div>
      <pre class="source-code-block">{{ sourceCode }}</pre>
      <template #footer>
        <el-button @click="sourceDialogVisible = false">关闭</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { ref, computed, onMounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import { ArrowLeft } from '@element-plus/icons-vue'
import { fetchPluginVersions, fetchPluginInfo, rollbackPlugin } from '../api'

const route = useRoute()
const router = useRouter()
const pluginId = computed(() => route.params.pluginId || '')

const versions = ref([])
const activeVersion = ref(null)
const versionsLoading = ref(false)
const rollbackLoading = ref(false)

const sourceDialogVisible = ref(false)
const sourceCode = ref('')

function goBack() {
  const savedUrl = sessionStorage.getItem('monitor_list_url')
  if (savedUrl) {
    router.push(savedUrl)
  } else if (window.history.length > 1) {
    router.back()
  } else {
    router.push('/monitor')
  }
}

async function loadVersions() {
  if (!pluginId.value) return
  versionsLoading.value = true
  try {
    const { data, code, message } = await fetchPluginVersions(pluginId.value)
    if (code === 0) {
      versions.value = data.versions || []
      activeVersion.value = data.active_version || null
    } else {
      ElMessage.error('加载版本失败: ' + (message || '未知错误'))
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    versionsLoading.value = false
  }
}

async function handleViewSource(pid) {
  try {
    const { data, code, message } = await fetchPluginInfo(pid)
    if (code === 0 && data.plugin) {
      sourceCode.value = data.plugin.source_code || ''
      sourceDialogVisible.value = true
    } else {
      ElMessage.error('获取源码失败: ' + (message || '未知错误'))
    }
  } catch (e) {
    ElMessage.error('获取源码失败: ' + (e.message || '网络错误'))
  }
}

async function handleRollback(pid, version) {
  try {
    await ElMessageBox.confirm(
      `确定要回滚插件 "${pid}" 到版本 v${version} 吗？这将切换当前生效版本。`,
      '回滚确认',
      { confirmButtonText: '确定回滚', cancelButtonText: '取消', type: 'warning' },
    )
  } catch {
    return
  }
  rollbackLoading.value = true
  try {
    const { code, message } = await rollbackPlugin(pid, version)
    if (code === 0) {
      ElMessage.success(`已切换生效版本为 v${version}`)
      await loadVersions()
    } else {
      ElMessage.error('回滚失败: ' + (message || '未知错误'))
    }
  } catch (e) {
    ElMessage.error('回滚请求失败: ' + (e.message || '网络错误'))
  } finally {
    rollbackLoading.value = false
  }
}

watch(pluginId, () => { loadVersions() })
onMounted(() => { loadVersions() })
</script>

<style scoped>
.monitor-detail-page {
  height: 100%;
  display: flex;
  flex-direction: column;
  gap: 18px;
}

.detail-header {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-shrink: 0;
}

.detail-title {
  font-size: 17px;
  font-weight: 800;
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--text-h);
}
.detail-title .title-icon {
  font-size: 20px;
}

.panel-card {
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 24px 28px;
  position: relative;
  overflow: hidden;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.2);
}
.panel-card::before {
  content: ''; position: absolute; top: 0; left: 0; right: 0; height: 2px;
  background: linear-gradient(90deg, var(--accent) 0%, var(--accent-secondary) 50%, transparent 100%);
  opacity: 0.6;
}

.version-info {
  font-size: 13px;
  color: var(--muted);
  margin-bottom: 14px;
}

.version-info strong {
  color: var(--accent);
}

.empty-text {
  font-size: 14px;
  color: var(--muted);
  padding: 30px 0;
  text-align: center;
}

.versions-items {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.version-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 14px 18px;
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  transition: all 0.2s;
}
.version-card.active {
  border-color: var(--accent);
  border-left: 3px solid var(--accent);
  box-shadow: 0 0 16px rgba(0, 212, 255, 0.06);
}
.version-card:hover {
  border-color: var(--border-hover);
}

.version-top {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}

.version-left {
  display: flex;
  align-items: center;
  gap: 10px;
}

.version-num {
  font-size: 15px;
  font-weight: 700;
  color: var(--text-h);
}

.version-num.is-active {
  color: var(--accent);
  text-shadow: 0 0 8px rgba(0, 212, 255, 0.2);
}

.version-time {
  font-size: 12px;
  color: var(--muted);
  font-family: var(--font-mono);
}

.version-change {
  font-size: 13px;
  line-height: 1.5;
  padding: 10px 14px;
  background: rgba(0, 212, 255, 0.04);
  border-radius: var(--radius-sm);
  word-break: break-all;
  border: 1px solid var(--border);
}

.version-change .change-label {
  color: var(--muted);
  font-weight: 700;
  margin-right: 4px;
}

.version-change .change-text {
  color: var(--text);
}

.version-change.empty {
  background: rgba(255, 255, 255, 0.02);
  color: var(--muted);
}

.version-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}

.source-dialog-title {
  font-size: 14px;
  font-weight: 700;
  margin-bottom: 14px;
  color: var(--text-h);
  font-family: var(--font-mono);
}

.source-code-block {
  background: #0a0a12;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 16px 18px;
  font-size: 13px;
  font-family: var(--font-mono);
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-word;
  color: var(--text);
  margin: 0;
  max-height: 60vh;
  line-height: 1.6;
}
</style>
