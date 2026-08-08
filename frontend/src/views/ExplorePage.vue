<template>
  <div class="page">
    <header class="page-header">
      <div class="header-left">
        <h1 class="page-title">助手广场</h1>
        <p class="page-subtitle">浏览社区分享的助手，一键 Fork 到本地使用</p>
      </div>
      <div class="header-actions">
        <el-input
          v-model="keyword"
          placeholder="搜索助手…"
          clearable
          :prefix-icon="Search"
          class="search-input"
        />
        <el-button :icon="Key" @click="forkDialog = true">口令导入</el-button>
        <el-button :icon="Back" @click="$router.push('/assistants')">我的助手</el-button>
      </div>
    </header>

    <div v-loading="assistantStore.exploreLoading" class="explore-grid">
      <article
        v-for="a in filtered"
        :key="a.id"
        class="explore-card"
      >
        <div class="ec-head">
          <div class="ec-avatar">{{ a.avatar || '🤖' }}</div>
          <div class="ec-title-wrap">
            <h3 class="ec-title">{{ a.name }}</h3>
            <div class="ec-tags">
              <el-tag size="small" effect="plain" round>
                {{ AGENT_TYPE_KEY_LABEL[a.agent_type_key] || '自定义' }}
              </el-tag>
              <el-tag v-if="a.fork_count > 0" size="small" type="warning" effect="plain" round>
                🍴 {{ a.fork_count }}
              </el-tag>
            </div>
          </div>
        </div>
        <p class="ec-desc">{{ a.description || '暂无描述' }}</p>
        <div class="ec-tools" v-if="a.enabled_tools && a.enabled_tools.length">
          <el-tag v-for="t in a.enabled_tools" :key="t" size="small" type="info" effect="dark">
            {{ toolLabel(t) }}
          </el-tag>
        </div>
        <div class="ec-greeting" v-if="a.greeting">
          <el-icon><ChatLineRound /></el-icon>
          <span>{{ a.greeting }}</span>
        </div>
        <div class="ec-actions">
          <el-button
            v-if="a.kind !== 0"
            type="primary"
            :icon="Download"
            :loading="forking === a.id"
            @click="fork(a)"
          >
            Fork 到我的助手
          </el-button>
          <el-tag v-else size="small" type="info" effect="plain" round>内置不可 Fork</el-tag>
        </div>
      </article>
      <el-empty
        v-if="!filtered.length && !assistantStore.exploreLoading"
        description="广场暂无公开助手"
      />
    </div>

    <ForkByTokenDialog v-model="forkDialog" :init-token="initToken" @update:model-value="onForkDialogClose" />
  </div>
</template>

<script setup>
import { ref, computed, onMounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Search, Key, Back, Download, ChatLineRound } from '@element-plus/icons-vue'
import { useAssistantStore } from '../stores/assistant'
import { AGENT_TYPE_KEY_LABEL } from '../utils/assistantEnums'
import ForkByTokenDialog from '../components/ForkByTokenDialog.vue'

const route = useRoute()
const router = useRouter()
const assistantStore = useAssistantStore()

const keyword = ref('')
const forkDialog = ref(false)
const forking = ref(null)
const initToken = ref('')

const filtered = computed(() => {
  const kw = keyword.value.trim().toLowerCase()
  if (!kw) return assistantStore.exploreList
  return assistantStore.exploreList.filter(
    (a) =>
      a.name.toLowerCase().includes(kw) ||
      (a.description || '').toLowerCase().includes(kw),
  )
})

onMounted(() => {
  assistantStore.loadExplore()
  assistantStore.loadTools()
  // 支持分享直链：/explore?token=xxx 自动弹出 Fork 对话框
  if (route.query.token) {
    initToken.value = String(route.query.token)
    forkDialog.value = true
  }
})

watch(
  () => route.query.token,
  (t) => {
    if (t) {
      initToken.value = String(t)
      forkDialog.value = true
    }
  },
)

function onForkDialogClose(closed) {
  if (!closed && route.query.token) {
    // 关闭后清理 URL 上的 token，避免刷新重复弹出
    router.replace({ path: '/explore' })
    initToken.value = ''
  }
}

function toolLabel(key) {
  const t = assistantStore.tools.find((x) => x.key === key)
  return t ? t.name : key
}

async function fork(a) {
  forking.value = a.id
  try {
    await assistantStore.fork(a.id)
    ElMessage.success(`已 Fork「${a.name}」到我的助手`)
  } catch (e) {
    ElMessage.error(e.message)
  } finally {
    forking.value = null
  }
}
</script>

<style scoped>
.page { padding: 24px 28px; max-width: 1280px; margin: 0 auto; }
.page-header {
  display: flex; align-items: flex-end; justify-content: space-between;
  margin-bottom: 24px; gap: 16px; flex-wrap: wrap;
}
.page-title { font-size: 24px; font-weight: 800; color: var(--text-h); margin: 0 0 6px; }
.page-subtitle { font-size: 13px; color: var(--muted-light); margin: 0; }
.header-actions { display: flex; gap: 10px; flex-wrap: wrap; align-items: center; }
.search-input { width: 240px; }

.explore-grid {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 16px;
}
.explore-card {
  background: var(--card); border: 1px solid var(--border); border-radius: 14px;
  padding: 18px; display: flex; flex-direction: column; gap: 12px;
  transition: all .2s;
}
.explore-card:hover { border-color: var(--accent); box-shadow: 0 0 24px var(--border-glow); }

.ec-head { display: flex; gap: 12px; align-items: flex-start; }
.ec-avatar {
  width: 48px; height: 48px; border-radius: 12px; flex-shrink: 0;
  display: flex; align-items: center; justify-content: center; font-size: 24px;
  background: var(--accent-dim); border: 1px solid var(--border);
}
.ec-title-wrap { flex: 1; min-width: 0; }
.ec-title { font-size: 15px; font-weight: 700; color: var(--text-h); margin: 0 0 6px; }
.ec-tags { display: flex; gap: 6px; flex-wrap: wrap; }
.ec-desc {
  font-size: 13px; color: var(--muted-light); line-height: 1.5; margin: 0; flex: 1;
  overflow: hidden; text-overflow: ellipsis; display: -webkit-box;
  -webkit-line-clamp: 3; -webkit-box-orient: vertical;
}
.ec-tools { display: flex; gap: 6px; flex-wrap: wrap; }
.ec-greeting {
  display: flex; gap: 6px; align-items: flex-start; font-size: 12px; color: var(--muted);
  background: var(--bg-elevated); padding: 8px 10px; border-radius: 8px; line-height: 1.5;
}
.ec-greeting .el-icon { flex-shrink: 0; margin-top: 2px; }
.ec-actions { padding-top: 4px; }
</style>
