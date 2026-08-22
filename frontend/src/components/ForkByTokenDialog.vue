<template>
  <el-dialog
    :model-value="modelValue"
    @update:model-value="$emit('update:modelValue', $event)"
    title="按口令导入助手"
    width="520px"
    :close-on-click-modal="false"
    align-center
  >
    <div class="fork-body">
      <!-- 步骤 1：输入口令 -->
      <div v-if="step === 'input'" class="step-input">
        <p class="step-tip">输入同事分享的 8 位口令，预览后可 Fork 到我的助手：</p>
        <el-input
          v-model="tokenInput"
          placeholder="例如：Ab3xY9Km"
          clearable
          maxlength="8"
          :prefix-icon="Key"
          class="token-input"
          @keyup.enter="preview"
        />
      </div>

      <!-- 步骤 2：预览 -->
      <div v-else-if="step === 'preview'" class="step-preview" v-loading="previewing">
        <div v-if="previewData" class="preview-card">
          <div class="pv-avatar">{{ previewData.avatar || '🤖' }}</div>
          <div class="pv-info">
            <div class="pv-name">{{ previewData.name }}</div>
            <div class="pv-desc">{{ previewData.description || '暂无描述' }}</div>
            <div class="pv-meta">
              <el-tag size="small" effect="plain">Fork {{ previewData.fork_count }} 次</el-tag>
              <el-tag size="small" type="info" effect="plain">
                {{ AGENT_TYPE_KEY_LABEL[previewData.agent_type_key] || '自定义' }}
              </el-tag>
            </div>
          </div>
        </div>
        <el-alert v-else-if="previewError" type="error" :closable="false" show-icon>
          <template #title>加载失败：{{ previewError }}，请稍后重试。</template>
        </el-alert>
        <el-alert v-else type="warning" :closable="false" show-icon>
          <template #title>未找到对应助手，请检查口令是否正确。</template>
        </el-alert>
      </div>

      <!-- 步骤 3：Fork 中 -->
      <div v-else class="step-done">
        <el-result icon="success" title="Fork 成功" sub-title="已添加到我的助手，可在助手管理页查看。">
          <template #extra>
            <el-button type="primary" @click="goManage">去管理</el-button>
          </template>
        </el-result>
      </div>
    </div>

    <template #footer>
      <template v-if="step === 'input'">
        <el-button @click="$emit('update:modelValue', false)">取消</el-button>
        <el-button type="primary" :disabled="!tokenInput.trim()" :loading="previewing" :icon="Search" @click="preview">
          查询
        </el-button>
      </template>
      <template v-else-if="step === 'preview'">
        <el-button @click="resetInput">重新输入</el-button>
        <el-button
          type="primary"
          :disabled="!previewData || forking"
          :loading="forking"
          :icon="Download"
          @click="doFork"
        >
          Fork 到我的助手
        </el-button>
      </template>
      <template v-else>
        <el-button @click="closeAll">完成</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<script setup>
import { ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Key, Search, Download } from '@element-plus/icons-vue'
import { useAssistantStore } from '../stores/assistant'
import { useUserStore } from '../stores/user'
import { fetchAssistantByToken } from '../api'
import { AGENT_TYPE_KEY_LABEL } from '../utils/assistantEnums'

const props = defineProps({
  modelValue: Boolean,
  initToken: { type: String, default: '' },
})
const emit = defineEmits(['update:modelValue'])

const router = useRouter()
const assistantStore = useAssistantStore()
const userStore = useUserStore()

const step = ref('input') // input | preview | done
const tokenInput = ref('')
const previewing = ref(false)
const previewData = ref(null)
// 预览传输层错误（网络/5xx）：与「口令不存在」区分开，避免误导用户反复重输正确口令
const previewError = ref('')
// Fork 防重复提交：连点会 Fork 出两份副本
const forking = ref(false)

watch(
  () => props.modelValue,
  (v) => {
    if (v) {
      tokenInput.value = props.initToken || ''
      step.value = 'input'
      previewData.value = null
      previewError.value = ''
      // 若初始带口令（来自广场直链），直接预览
      if (tokenInput.value) {
        preview()
      }
    }
  },
)

async function preview() {
  const t = tokenInput.value.trim()
  if (!t || previewing.value) return
  previewing.value = true
  previewError.value = ''
  step.value = 'preview'
  try {
    const { data, code } = await fetchAssistantByToken(t)
    if (code === 0 && data.assistant) {
      previewData.value = data.assistant
    } else {
      previewData.value = null
    }
  } catch (e) {
    previewData.value = null
    previewError.value = e.message || '网络错误'
  } finally {
    previewing.value = false
  }
}

function resetInput() {
  step.value = 'input'
  tokenInput.value = ''
  previewData.value = null
  previewError.value = ''
}

async function doFork() {
  if (!previewData.value || forking.value) return
  // 匿名 Fork 的副本会挂到后端伪用户 "user" 名下（登录后消失）；后端 mutation
  // 对匿名不拒绝，前端必须自守。镜像路由守卫语义：未启用 auth 的部署放行
  const authOptional = !userStore.localEnabled && userStore.providers.length === 0
  if (!userStore.authenticated && !authOptional) {
    ElMessage.warning('请先登录后再 Fork')
    return
  }
  forking.value = true
  try {
    await assistantStore.fork(previewData.value.id)
    step.value = 'done'
    ElMessage.success('Fork 成功')
  } catch (e) {
    ElMessage.error(e.message)
  } finally {
    forking.value = false
  }
}

function goManage() {
  closeAll()
  router.push('/assistants')
}

function closeAll() {
  emit('update:modelValue', false)
}
</script>

<style scoped>
.fork-body { min-height: 140px; }
.step-tip { font-size: 13px; color: var(--muted); margin-bottom: 12px; line-height: 1.5; }
.token-input :deep(input) { letter-spacing: 2px; font-family: var(--font-mono); }
.preview-card {
  display: flex; gap: 14px; padding: 16px; border-radius: 12px;
  border: 1px solid var(--border); background: var(--card);
}
.pv-avatar {
  width: 56px; height: 56px; border-radius: 12px; flex-shrink: 0;
  display: flex; align-items: center; justify-content: center; font-size: 28px;
  background: var(--accent-dim); border: 1px solid var(--border);
}
.pv-info { flex: 1; min-width: 0; }
.pv-name { font-size: 16px; font-weight: 800; color: var(--text-h); margin-bottom: 6px; }
.pv-desc { font-size: 13px; color: var(--muted); line-height: 1.5; margin-bottom: 10px; }
.pv-meta { display: flex; gap: 8px; flex-wrap: wrap; }
</style>
