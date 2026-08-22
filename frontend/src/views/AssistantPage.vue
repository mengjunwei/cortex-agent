<template>
  <div class="page">
    <!-- 顶部操作栏 -->
    <header class="page-header">
      <div class="header-left">
        <h1 class="page-title">助手管理</h1>
        <p class="page-subtitle">配置专属助手：自定义提示词、工具与生成参数</p>
      </div>
      <div class="header-actions">
        <el-button :icon="Key" @click="forkDialog = true">口令导入</el-button>
        <el-button :icon="Compass" @click="$router.push('/explore')">助手广场</el-button>
        <el-button type="primary" :icon="Plus" @click="createNew">创建助手</el-button>
      </div>
    </header>

    <!-- 我的助手（内置 + 自定义统一展示；内置助手仅管理员可见） -->
    <section class="block">
      <div class="block-head">
        <h2 class="block-title">我的助手</h2>
        <span class="block-count">{{ myAssistants.length }} 个</span>
      </div>
      <div v-loading="assistantStore.loading" class="card-grid">
        <article
          v-for="a in myAssistants"
          :key="a.id"
          class="asst-card"
          :class="a.kind === 0 ? 'builtin' : 'custom'"
        >
          <div class="ac-head">
            <div class="ac-avatar">{{ a.avatar || '🤖' }}</div>
            <div class="ac-title-wrap">
              <h3 class="ac-title">{{ a.name }}</h3>
              <div class="ac-tags">
                <el-tag size="small" :type="a.kind === 0 ? 'info' : 'success'" effect="plain" round>
                  {{ a.kind === 0 ? '内置' : '自定义' }}
                </el-tag>
                <el-tag
                  v-if="a.kind !== 0 && a.visibility === 1"
                  size="small"
                  type="warning"
                  effect="plain"
                  round
                >已分享</el-tag>
                <el-tag v-if="userStore.user?.is_admin && a.owner" size="small" type="warning" effect="plain" round>{{ a.owner }}</el-tag>
              </div>
            </div>
          </div>
          <p class="ac-desc">{{ a.description || a.greeting || '暂无描述' }}</p>
          <div class="ac-tools" v-if="a.enabled_tools && a.enabled_tools.length">
            <el-tag v-for="t in a.enabled_tools" :key="t" size="small" type="info" effect="dark">
              {{ toolLabel(t) }}
            </el-tag>
          </div>
          <!-- 设备命令类助手（内置 + Fork 副本）：知识库绑定行 -->
          <div class="ac-kb" v-if="['device_command'].includes(a.agent_type_key)">
            <span class="ac-kb-label">知识库</span>
            <el-select
              :model-value="a.kb_instance_id || ''"
              placeholder="未绑定"
              size="small"
              clearable
              filterable
              class="ac-kb-select"
              :loading="kbLoading"
              @change="(v) => onBuiltinKbChange(a, v)"
            >
              <el-option
                v-for="ins in kbInstances"
                :key="ins.id"
                :label="`${ins.name}（${ins.provider_kind === 1 ? 'Dify' : '内置'}）`"
                :value="ins.id"
              />
            </el-select>
          </div>
          <!-- 自定义助手：fork/模型元信息 -->
          <div class="ac-meta" v-else>
            <span class="ac-fork" v-if="a.fork_count > 0">🍴 {{ a.fork_count }}</span>
            <span class="ac-model">{{ a.model_id || '默认模型' }}</span>
          </div>
          <div class="ac-actions">
            <el-button size="small" type="primary" plain :icon="ChatDotRound" @click="startChat(a.id)">
              {{ a.kind === 0 ? '开始对话' : '对话' }}
            </el-button>
            <el-button size="small" :icon="Edit" @click="edit(a.id)">编辑</el-button>
            <el-button size="small" :icon="Share" @click="openShare(a.id)">分享</el-button>
            <el-dropdown trigger="click" @command="(cmd) => handleMore(cmd, a)">
              <el-button size="small" :icon="MoreFilled" circle />
              <template #dropdown>
                <el-dropdown-menu>
                  <el-dropdown-item command="export" :icon="Download">导出 JSON</el-dropdown-item>
                  <el-dropdown-item command="duplicate" :icon="CopyDocument">复制副本</el-dropdown-item>
                  <el-dropdown-item command="delete" :icon="Delete" divided class="danger-item">
                    删除助手
                  </el-dropdown-item>
                </el-dropdown-menu>
              </template>
            </el-dropdown>
          </div>
        </article>
        <el-empty
          v-if="!myAssistants.length && !assistantStore.loading"
          description="还没有助手，点击右上角「创建助手」开始"
        >
          <el-button type="primary" :icon="Plus" @click="createNew">创建第一个助手</el-button>
        </el-empty>
      </div>
    </section>

    <!-- 分享对话框 -->
    <ShareDialog v-model="shareDialog" :assistant-id="shareTargetId" @disabled="refresh" />
    <!-- 口令导入对话框 -->
    <ForkByTokenDialog v-model="forkDialog" />
    <!-- 选择已有会话对话框 -->
    <ChooseSessionDialog
      v-model="chooseSessionDialog"
      :assistant-id="chooseAssistantId"
      :assistant-name="chooseAssistantName"
      @choose="onChooseSession"
      @create-new="onCreateNewSession"
    />
  </div>
</template>

<script setup>
import { computed, ref, onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import {
  Plus, Edit, Share, Delete, MoreFilled, CopyDocument,
  Download, ChatDotRound, Key, Compass,
} from '@element-plus/icons-vue'
import { useAssistantStore } from '../stores/assistant'
import { useUserStore } from '../stores/user'
import { fetchSessions, fetchKbInstances, bindAssistantKbInstance, deleteAssistant } from '../api'
import { confirmDeleteWithImpact } from '../composables/useDeleteWithImpact'
import ShareDialog from '../components/ShareDialog.vue'
import ForkByTokenDialog from '../components/ForkByTokenDialog.vue'
import ChooseSessionDialog from '../components/ChooseSessionDialog.vue'

const router = useRouter()
const assistantStore = useAssistantStore()
const userStore = useUserStore()

const shareDialog = ref(false)
const shareTargetId = ref('')
const forkDialog = ref(false)
const chooseSessionDialog = ref(false)
const chooseAssistantId = ref('')
const chooseAssistantName = ref('')

// 知识库实例列表（内置助手「配置知识库」下拉用）
const kbInstances = ref([])
const kbLoading = ref(false)

const builtinAssistants = computed(() => assistantStore.builtinAssistants)
const customAssistants = computed(() => assistantStore.customAssistants)
// 统一展示：内置（仅管理员有）在前 + 自定义在后，与后端排序一致
const myAssistants = computed(() => [...builtinAssistants.value, ...customAssistants.value])

onMounted(() => {
  assistantStore.loadAssistants()
  assistantStore.loadTools()
  loadKbInstances()
})

async function loadKbInstances() {
  kbLoading.value = true
  try {
    const { data, code } = await fetchKbInstances()
    if (code === 0) kbInstances.value = data.instances || []
  } catch (_) {
    kbInstances.value = []
  } finally {
    kbLoading.value = false
  }
}

// 内置助手绑定/解绑知识库实例（其他字段仍只读，仅此一项可改）
async function onBuiltinKbChange(a, kbInstanceId) {
  const target = kbInstanceId || ''
  try {
    const { code, message } = await bindAssistantKbInstance(a.id, target)
    if (code === 0) {
      // 局部更新（store 中的助手对象是同一引用，直接改即可生效，避免整页刷新）
      a.kb_instance_id = target || null
      ElMessage.success(target ? '已绑定知识库' : '已解绑知识库')
    } else {
      ElMessage.error(message || '更新失败')
    }
  } catch (e) {
    ElMessage.error(e.message || '更新失败')
  }
}

function refresh() {
  assistantStore.loadAssistants()
}

function toolLabel(key) {
  const t = assistantStore.tools.find((x) => x.key === key)
  return t ? t.name : key
}

function createNew() {
  router.push('/assistants/new')
}

function edit(id) {
  router.push(`/assistants/${id}/edit`)
}

// startChat 请求序号：连点两个助手的「开始对话」时，先发请求的慢回包后到会把
// 选择会话弹窗带到错误的助手上
let startChatSeq = 0

async function startChat(id) {
  const seq = ++startChatSeq
  assistantStore.selectAssistant(id)
  const assistant = [...builtinAssistants.value, ...customAssistants.value].find(a => a.id === id)
  // 先查该助手是否已有会话：有则让用户选择，无则直接新建并跳转
  try {
    const { data, code } = await fetchSessions(1, 1, { assistantId: id })
    if (seq !== startChatSeq) return // 用户已点了别的助手，丢弃过期回包
    const exists = code === 0 && (data.total || 0) > 0
    if (exists) {
      chooseAssistantId.value = id
      chooseAssistantName.value = assistant?.name || ''
      chooseSessionDialog.value = true
      return
    }
  } catch (_) {
    // 查询失败时降级为直接新建
  }
  // 无已有会话 → 直接新建并跳转
  if (seq !== startChatSeq) return
  router.push({ path: '/chat', query: { assistant_id: id } })
}

// 选择已有会话继续
function onChooseSession(sessionId) {
  if (!sessionId) return
  const aid = chooseAssistantId.value
  router.push({ path: '/chat', query: { session: sessionId, assistant_id: aid } })
}

// 在弹框中选择新建会话
function onCreateNewSession() {
  const aid = chooseAssistantId.value
  if (!aid) return
  router.push({ path: '/chat', query: { assistant_id: aid } })
}

function openShare(id) {
  shareTargetId.value = id
  shareDialog.value = true
}

async function duplicate(a) {
  try {
    const newId = await assistantStore.duplicate(a.id)
    ElMessage.success('已复制为自定义助手')
    if (newId) edit(newId)
  } catch (e) {
    ElMessage.error(e.message)
  }
}

async function handleMore(cmd, a) {
  if (cmd === 'export') {
    try {
      const data = await assistantStore.exportOne(a.id)
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' })
      const url = URL.createObjectURL(blob)
      const link = document.createElement('a')
      link.href = url
      link.download = `${a.name || 'assistant'}.json`
      link.click()
      URL.revokeObjectURL(url)
      ElMessage.success('已导出')
    } catch (e) {
      ElMessage.error(e.message)
    }
  } else if (cmd === 'duplicate') {
    await duplicate(a)
  } else if (cmd === 'delete') {
    await confirmDeleteWithImpact({
      id: a.id,
      removeFn: deleteAssistant,
      title: '删除助手',
      targetLabel: a.name,
      onSuccess: () => assistantStore.loadAssistants(),
    })
  }
}
</script>

<style scoped>
.page { padding: 24px 28px; max-width: 1280px; margin: 0 auto; }
.page-header {
  display: flex; align-items: flex-end; justify-content: space-between;
  margin-bottom: 28px; gap: 16px; flex-wrap: wrap;
}
.page-title { font-size: 24px; font-weight: 800; color: var(--text-h); margin: 0 0 6px; }
.page-subtitle { font-size: 13px; color: var(--muted-light); margin: 0; }
.header-actions { display: flex; gap: 10px; flex-wrap: wrap; }

.block { margin-bottom: 36px; }
.block-head {
  display: flex; align-items: center; gap: 10px; margin-bottom: 16px;
  padding-bottom: 10px; border-bottom: 1px solid var(--border);
}
.block-title { font-size: 16px; font-weight: 700; color: var(--text-h); margin: 0; }
.block-count { font-size: 12px; color: var(--muted); }

.card-grid {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 16px;
}
.asst-card {
  background: var(--card); border: 1px solid var(--border); border-radius: 14px;
  padding: 18px; display: flex; flex-direction: column; gap: 12px;
  transition: all .2s;
}
.asst-card:hover { border-color: var(--border-hover); background: var(--card-hover); }
.asst-card.builtin { border-top: 3px solid var(--muted); }
.asst-card.custom { border-top: 3px solid var(--accent); }

.ac-head { display: flex; gap: 12px; align-items: flex-start; }
.ac-avatar {
  width: 48px; height: 48px; border-radius: 12px; flex-shrink: 0;
  display: flex; align-items: center; justify-content: center; font-size: 24px;
  background: var(--accent-dim); border: 1px solid var(--border);
}
.ac-title-wrap { flex: 1; min-width: 0; }
.ac-title {
  font-size: 15px; font-weight: 700; color: var(--text-h); margin: 0 0 6px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.ac-tags { display: flex; gap: 6px; flex-wrap: wrap; }
.ac-desc {
  font-size: 13px; color: var(--muted-light); line-height: 1.5; margin: 0;
  min-height: 20px; flex: 1;
  overflow: hidden; text-overflow: ellipsis; display: -webkit-box;
  -webkit-line-clamp: 2; -webkit-box-orient: vertical;
}
.ac-tools { display: flex; gap: 6px; flex-wrap: wrap; }
.ac-kb {
  display: flex; align-items: center; gap: 8px;
  padding: 8px 10px; border-radius: 8px;
  background: var(--bg-elevated); border: 1px solid var(--border);
}
.ac-kb-label { font-size: 12px; color: var(--muted); flex-shrink: 0; }
.ac-kb-select { flex: 1; min-width: 0; }
.ac-meta {
  display: flex; justify-content: space-between; font-size: 12px; color: var(--muted);
  padding-top: 8px; border-top: 1px dashed var(--border);
}
.ac-actions { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
.danger-item { color: var(--el-color-danger); }
</style>
