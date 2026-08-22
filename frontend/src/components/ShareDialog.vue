<template>
  <el-dialog
    :model-value="modelValue"
    @update:model-value="$emit('update:modelValue', $event)"
    title="分享助手"
    width="480px"
    :close-on-click-modal="false"
    align-center
  >
    <div class="share-body" v-loading="loading">
      <template v-if="token">
        <p class="share-tip">将以下口令发给同事，对方输入口令即可 Fork 此助手到本地：</p>
        <div class="token-box">
          <span class="token-text">{{ token }}</span>
          <el-button text :icon="CopyDocument" @click="copyToken">复制</el-button>
        </div>
        <div class="share-link">
          <span class="link-label">或分享直链：</span>
          <code class="link-url">{{ shareUrl }}</code>
          <el-button text :icon="CopyDocument" @click="copyLink">复制</el-button>
        </div>
        <el-alert type="info" :closable="false" show-icon class="share-alert">
          <template #title>
            口令分享不要求登录；关闭分享后口令立即失效。
          </template>
        </el-alert>
      </template>
      <el-empty v-else-if="!loading" description="暂无口令" />
    </div>
    <template #footer>
      <el-button @click="$emit('update:modelValue', false)">关闭</el-button>
      <el-button v-if="token" type="danger" plain :icon="CircleClose" @click="disable">
        关闭分享
      </el-button>
    </template>
  </el-dialog>
</template>

<script setup>
import { ref, computed, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { CopyDocument, CircleClose } from '@element-plus/icons-vue'
import { useAssistantStore } from '../stores/assistant'

const props = defineProps({
  modelValue: Boolean,
  assistantId: { type: String, default: '' },
})
const emit = defineEmits(['update:modelValue', 'disabled'])

const assistantStore = useAssistantStore()
const loading = ref(false)
const token = ref('')

const shareUrl = computed(() =>
  token.value ? `${window.location.origin}/explore?token=${token.value}` : '',
)

// 请求序号：快速切换分享对象（A 打开后关掉再开 B）时，A 的慢回包可能后到，
// 不核对会把 A 的口令显示在 B 的弹窗里
let shareSeq = 0

watch(
  () => props.modelValue,
  async (v) => {
    if (v && props.assistantId) {
      const seq = ++shareSeq
      loading.value = true
      token.value = ''
      try {
        const t = await assistantStore.share(props.assistantId)
        if (seq !== shareSeq) return // 弹窗已切换到别的助手，丢弃过期回包
        token.value = t
      } catch (e) {
        if (seq === shareSeq) ElMessage.error(e.message)
      } finally {
        if (seq === shareSeq) loading.value = false
      }
    }
  },
)

async function copyToken() {
  try {
    await navigator.clipboard.writeText(token.value)
    ElMessage.success('口令已复制')
  } catch (_) {
    ElMessage.warning('复制失败，请手动选择')
  }
}

async function copyLink() {
  try {
    await navigator.clipboard.writeText(shareUrl.value)
    ElMessage.success('链接已复制')
  } catch (_) {
    ElMessage.warning('复制失败，请手动选择')
  }
}

async function disable() {
  try {
    await assistantStore.unshare(props.assistantId)
    ElMessage.success('已关闭分享')
    token.value = ''
    emit('disabled')
    emit('update:modelValue', false)
  } catch (e) {
    ElMessage.error(e.message)
  }
}
</script>

<style scoped>
.share-body { min-height: 120px; }
.share-tip { font-size: 13px; color: var(--muted); margin-bottom: 12px; line-height: 1.5; }
.token-box {
  display: flex; align-items: center; justify-content: space-between;
  padding: 14px 16px; border-radius: 10px; border: 1px dashed var(--accent);
  background: var(--accent-dim); margin-bottom: 14px;
}
.token-text {
  font-size: 24px; font-weight: 800; letter-spacing: 4px;
  font-family: var(--font-mono); color: var(--accent);
}
.share-link {
  display: flex; align-items: center; gap: 8px; margin-bottom: 14px;
  padding: 8px 12px; border-radius: 8px; background: var(--card);
  border: 1px solid var(--border); font-size: 12px;
}
.link-label { color: var(--muted); white-space: nowrap; }
.link-url {
  flex: 1; color: var(--text); overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap; font-size: 12px;
}
.share-alert { margin-top: 4px; }
</style>
