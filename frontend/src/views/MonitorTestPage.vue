<template>
  <div class="monitor-detail-page">
    <div class="detail-header">
      <el-button size="small" @click="goBack">
        <el-icon><ArrowLeft /></el-icon> 返回插件列表
      </el-button>
      <div class="detail-title">
        <span class="title-icon">📡</span>
        <span>SNMP 测试 - {{ pluginId }}</span>
      </div>
    </div>

    <div class="panel-card">
      <div class="form-item">
        <label class="form-label">当前插件</label>
        <div class="plugin-tag">
          <span class="plugin-icon">🔌</span>
          <span class="plugin-id">{{ pluginId }}</span>
        </div>
      </div>
      <div style="margin-top: 14px;">
        <el-button type="primary" :loading="oidsLoading" @click="handlePrepareOids">
          <el-icon><Connection /></el-icon> 获取 OID 列表
        </el-button>
      </div>

      <!-- OID 列表 -->
      <div v-if="oids.length" class="oids-section">
        <div class="section-label">OID 列表 ({{ oids.length }})</div>
        <div class="oids-box">
          <div v-for="(o, i) in oids" :key="i" class="oid-row">
            <span class="oid-method">{{ o.method || 'get' }}</span>
            <span class="oid-value">{{ o.oid || '' }}</span>
          </div>
        </div>
      </div>

      <!-- 解析输入 -->
      <div class="form-item" style="margin-top: 16px;">
        <label class="form-label">SNMP 响应数据 (JSON)</label>
        <el-input
          v-model="parseInput"
          type="textarea"
          :rows="8"
          placeholder='粘贴 SNMP 响应 JSON，例如: {".1.3.6.1.2.1.1.3.0":{"oid_value_type":2,"value_str":"","value_num":42.0}}'
          class="script-textarea"
        />
      </div>
      <div style="margin-top: 12px;">
        <el-button type="primary" :loading="parsing" @click="handleParse">
          <el-icon><Search /></el-icon> 执行解析
        </el-button>
      </div>

      <!-- 解析结果 -->
      <div v-if="parseResult !== null" class="parse-result-section">
        <div class="section-label">解析结果</div>
        <pre class="parse-result-box">{{ JSON.stringify(parseResult, null, 2) }}</pre>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { ArrowLeft, Connection, Search } from '@element-plus/icons-vue'
import { getMonitorOids, calculateMonitor } from '../api'

const route = useRoute()
const router = useRouter()
const pluginId = computed(() => route.params.pluginId || '')

const oids = ref([])
const oidsLoading = ref(false)
const parseInput = ref('')
const parseResult = ref(null)
const parsing = ref(false)

function goBack() {
  // 优先用 sessionStorage 保存的列表 URL（刷新后仍可恢复筛选状态）
  const savedUrl = sessionStorage.getItem('monitor_list_url')
  if (savedUrl) {
    router.push(savedUrl)
  } else if (window.history.length > 1) {
    router.back()
  } else {
    router.push('/monitor')
  }
}

async function handlePrepareOids() {
  if (!pluginId.value) return
  oidsLoading.value = true
  oids.value = []
  try {
    const { data, code, message } = await getMonitorOids(pluginId.value)
    if (code === 0) {
      oids.value = data.oids || []
      if (oids.value.length === 0) {
        ElMessage.info('该插件未返回任何 OID')
      }
    } else {
      ElMessage.error('获取 OID 失败: ' + (message || '未知错误'))
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    oidsLoading.value = false
  }
}

async function handleParse() {
  if (!pluginId.value) return
  const input = parseInput.value.trim()
  if (!input) {
    ElMessage.warning('请输入 SNMP 响应 JSON 数据')
    return
  }
  let parsed
  try {
    parsed = JSON.parse(input)
  } catch (e) {
    ElMessage.error('JSON 格式错误: ' + e.message)
    return
  }
  parsing.value = true
  parseResult.value = null
  try {
    const { data, code, message } = await calculateMonitor({ plugin_id: pluginId.value, oid_values: parsed })
    if (code === 0) {
      parseResult.value = data.results || []
    } else {
      ElMessage.error('解析失败: ' + (message || '未知错误'))
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    parsing.value = false
  }
}
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

.form-item {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.form-label {
  font-size: 12px;
  font-weight: 700;
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.plugin-tag {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  padding: 8px 14px;
  background: rgba(0, 212, 255, 0.06);
  border: 1px solid rgba(0, 212, 255, 0.2);
  border-radius: var(--radius-sm);
  align-self: flex-start;
}
.plugin-icon {
  font-size: 16px;
}
.plugin-id {
  font-family: var(--font-mono);
  font-size: 14px;
  font-weight: 700;
  color: var(--accent);
}

.script-textarea :deep(.el-textarea__inner) {
  font-family: var(--font-mono);
  font-size: 13px;
  resize: vertical;
}

.oids-section {
  margin-top: 16px;
}

.section-label {
  font-size: 12px;
  font-weight: 700;
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
  margin-bottom: 8px;
}

.oids-box {
  background: #0a0a12;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 12px 16px;
  max-height: 240px;
  overflow-y: auto;
}

.oid-row {
  display: flex;
  align-items: center;
  gap: 14px;
  padding: 5px 0;
  font-size: 13px;
  font-family: var(--font-mono);
}

.oid-method {
  color: var(--accent);
  min-width: 40px;
  font-weight: 600;
}

.oid-value {
  color: var(--text);
}

.parse-result-section {
  margin-top: 16px;
}

.parse-result-box {
  background: #0a0a12;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 16px 18px;
  font-size: 13px;
  font-family: var(--font-mono);
  overflow-x: auto;
  white-space: pre-wrap;
  word-break: break-word;
  color: var(--text);
  margin: 0;
  max-height: 400px;
  overflow-y: auto;
  line-height: 1.7;
}
</style>
