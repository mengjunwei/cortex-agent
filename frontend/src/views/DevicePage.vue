<template>
  <div class="device-page">
    <!-- 搜索区域 -->
    <div class="search-section">
      <div class="search-card">
        <div class="search-title">
          <span class="title-icon">🔍</span>
          <span>设备命令搜索</span>
        </div>
        <el-input
          v-model="query"
          type="textarea"
          :rows="3"
          placeholder="输入设备型号、厂商或命令关键词，例如：华为交换机查看接口状态"
          class="search-input"
          @keydown.ctrl.enter="doSearch"
        />
        <div class="search-actions">
          <span class="search-hint">Ctrl + Enter 搜索</span>
          <el-button type="primary" :icon="Search" :loading="loading" @click="doSearch">
            搜索
          </el-button>
        </div>
      </div>
    </div>

    <!-- 结果列表 -->
    <div v-if="results.length" class="results-section">
      <div class="results-header">
        <span class="results-count">找到 <strong>{{ results.length }}</strong> 条结果</span>
      </div>
      <div class="results-list">
        <div
          v-for="(item, idx) in results"
          :key="idx"
          class="result-card"
          :class="{ expanded: expandedIdx === idx }"
        >
          <div class="result-header" @click="toggleExpand(idx)">
            <div class="result-info">
              <div class="result-title">{{ item.title || '未知文档' }}</div>
              <div class="result-meta">
                <el-tag v-if="item.brand" size="small" effect="plain">{{ item.brand }}</el-tag>
                <el-tag v-if="item.dev_type" size="small" type="info" effect="plain">{{ item.dev_type }}</el-tag>
                <span v-if="item.access_count != null" class="meta-text">访问 {{ item.access_count }} 次</span>
              </div>
            </div>
            <el-icon class="expand-icon">
              <ArrowDown v-if="expandedIdx !== idx" />
              <ArrowUp v-else />
            </el-icon>
          </div>
          <div v-show="expandedIdx === idx" class="result-content">
            <pre class="content-block">{{ item.content || '暂无内容' }}</pre>
          </div>
        </div>
      </div>
    </div>

    <!-- 空状态 -->
    <div v-else-if="searched && !loading" class="empty-state">
      <div class="empty-icon">🔍</div>
      <div class="empty-text">未找到相关结果，请尝试其他关键词</div>
    </div>

    <!-- 初始状态 -->
    <div v-if="!searched && !loading" class="welcome-state">
      <div class="welcome-icon">📡</div>
      <div class="welcome-title">设备命令助手</div>
      <div class="welcome-desc">输入设备型号或命令关键词，快速检索设备配置命令与操作指南</div>
    </div>
  </div>
</template>

<script setup>
import { ref, onMounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { Search, ArrowDown, ArrowUp } from '@element-plus/icons-vue'
import { ElMessage } from 'element-plus'
import { searchDevice } from '../api'

const route = useRoute()
const router = useRouter()

const query = ref('')
const results = ref([])
const loading = ref(false)
const searched = ref(false)
const expandedIdx = ref(-1)

// ── 搜索词同步 URL ──
function restoreFromQuery() {
  const q = route.query
  if (q.kw) query.value = String(q.kw)
}

function syncToQuery() {
  const queryObj = {}
  if (query.value) queryObj.kw = query.value
  router.replace({ path: '/device', query: queryObj })
}

function toggleExpand(idx) {
  expandedIdx.value = expandedIdx.value === idx ? -1 : idx
}

async function doSearch() {
  const q = query.value.trim()
  if (!q) {
    ElMessage.warning('请输入搜索关键词')
    return
  }
  loading.value = true
  searched.value = true
  expandedIdx.value = -1
  try {
    const { data, code, message } = await searchDevice(q)
    if (code === 0) {
      results.value = data.results || []
      if (results.value.length === 0) {
        ElMessage.info('未找到相关结果')
      }
    } else {
      ElMessage.error(message || '搜索失败')
      results.value = []
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
    results.value = []
  } finally {
    loading.value = false
  }
}

// 刷新恢复：从 URL 读回搜索词并自动重搜
onMounted(() => {
  restoreFromQuery()
  if (query.value) doSearch()
})

// 搜索词变化时同步到 URL（用户手动清空输入框时 URL 也应清掉）
watch(query, () => syncToQuery())
</script>

<style scoped>
.device-page {
  max-width: 960px;
  margin: 0 auto;
  padding: 8px 0 20px;
}

.search-section {
  margin-bottom: 24px;
}
.search-card {
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 24px 28px;
  position: relative;
  overflow: hidden;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.2);
}
.search-card::before {
  content: ''; position: absolute; top: 0; left: 0; right: 0; height: 2px;
  background: linear-gradient(90deg, var(--accent) 0%, var(--accent-secondary) 50%, transparent 100%);
  opacity: 0.6;
}
.search-title {
  font-size: 17px;
  font-weight: 800;
  margin-bottom: 16px;
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--text-h);
}
.title-icon {
  font-size: 20px;
}
.search-input :deep(.el-textarea__inner) {
  font-size: 14px;
  line-height: 1.6;
  resize: vertical;
}

.search-actions {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-top: 12px;
}

.search-hint {
  font-size: 12px;
  color: var(--muted);
  font-family: var(--font-mono);
}

.results-section {
  margin-top: 8px;
}

.results-header {
  font-size: 13px;
  color: var(--muted);
  margin-bottom: 14px;
  font-weight: 700;
}
.results-count strong {
  color: var(--accent);
}

.results-list {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.result-card {
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  overflow: hidden;
  transition: all 0.2s;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15);
}
.result-card:hover {
  border-color: var(--border-hover);
}
.result-card.expanded {
  border-color: var(--accent);
  box-shadow: 0 0 20px rgba(0, 212, 255, 0.06);
}

.result-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 18px;
  cursor: pointer;
  transition: background 0.15s;
}
.result-header:hover {
  background: rgba(0, 212, 255, 0.04);
}

.result-info {
  flex: 1;
  min-width: 0;
}

.result-title {
  font-size: 14px;
  font-weight: 700;
  margin-bottom: 6px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text-h);
}

.result-meta {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.meta-text {
  font-size: 12px;
  color: var(--muted);
}

.expand-icon {
  font-size: 16px;
  color: var(--muted);
  flex-shrink: 0;
  margin-left: 12px;
  transition: transform 0.2s;
}

.result-content {
  border-top: 1px solid var(--border);
  padding: 16px 18px;
}

.content-block {
  background: #0a0a12;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 14px 16px;
  font-size: 13px;
  line-height: 1.6;
  font-family: var(--font-mono);
  overflow-x: auto;
  white-space: pre-wrap;
  word-break: break-word;
  color: var(--text);
  margin: 0;
}

.empty-state,
.welcome-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  padding: 80px 20px;
  color: var(--muted);
}

.empty-icon,
.welcome-icon {
  font-size: 56px;
  margin-bottom: 16px;
  opacity: 0.4;
}

.empty-text {
  font-size: 14px;
}

.welcome-title {
  font-size: 22px;
  font-weight: 800;
  color: var(--text-h);
  margin-bottom: 8px;
  letter-spacing: -0.3px;
}

.welcome-desc {
  font-size: 14px;
  text-align: center;
  max-width: 400px;
  line-height: 1.6;
}
</style>
