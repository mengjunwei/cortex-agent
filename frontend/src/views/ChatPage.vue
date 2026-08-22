<template>
  <div class="chat-page">
    <!-- 左侧轮次导航：列出所有用户提问，点击跳转+高亮 -->
    <aside class="turn-sidebar">
      <div class="turn-list" ref="turnListEl">
        <div
          v-for="turn in userTurns"
          :key="turn.idx"
          :data-turn-idx="turn.idx"
          class="turn-item"
          :class="{ active: activeTurnIdx === turn.idx }"
          @click="jumpToTurn(turn.idx)"
        >
          <span class="turn-no">{{ turn.number }}</span>
          <span class="turn-meta">
            <span class="turn-time">{{ formatTurnTime(turn.msg.timestamp) }}</span>
            <span class="turn-text">{{ turn.preview }}</span>
          </span>
        </div>
        <div v-if="userTurns.length === 0" class="turn-empty">暂无提问</div>
      </div>
      <!-- 上下文 token 用量：从输入框下方挪到轮次导航底部，复用侧边栏内列表下方的留白 -->
      <div v-if="sessionStore.currentSessionId && usageText" class="turn-usage" :class="{ warn: usageNearLimit }" title="上下文剩余百分比">
        <el-icon class="turn-usage-icon"><Gauge /></el-icon>
        <span class="usage-collapsed">{{ usageShort }}</span>
        <span class="usage-expanded">{{ usageText }}</span>
      </div>
    </aside>

    <!-- 主内容区：顶部栏 + 消息 + 输入 -->
    <div class="chat-content">
    <!-- 顶部栏 -->
    <div class="chat-header">
      <div class="header-left">
        <el-button size="small" class="back-btn" @click="goBackToList">
          <el-icon><ArrowLeft /></el-icon> 返回列表
        </el-button>
        <span class="agent-label">{{ agentLabel }}</span>
      </div>
      <div class="header-center">
        <el-tooltip
          v-if="sessionStore.currentSessionId && !isScheduledSession"
          content="点击修改会话名称"
          placement="bottom"
          :show-after="400"
        >
          <span class="session-hint clickable" @click="onRenameSession">
            {{ sessionTitle }}
            <el-icon class="title-edit-icon"><EditPen /></el-icon>
          </span>
        </el-tooltip>
        <span v-else-if="sessionStore.currentSessionId" class="session-hint">
          {{ sessionTitle }}
        </span>
        <span v-else class="session-hint muted">请选择或新建会话</span>
      </div>
      <div class="header-right">
        <el-select
          v-model="chatStore.currentModelId"
          size="small"
          style="width: 240px"
          :class="{ 'current-model-unavailable': modelUnavailable }"
          :disabled="isScheduledSession"
          @change="onModelChange"
        >
          <el-option
            v-for="m in appStore.models"
            :key="m.id"
            :label="m.status === 1 ? modelLabel(m) : `${modelLabel(m)}（已禁用）`"
            :value="m.id"
            :disabled="m.status !== 1"
          >
            <div style="display: flex; flex-direction: column; line-height: 1.3;" :style="{ opacity: m.status === 1 ? 1 : 0.45 }">
              <span>{{ m.name }}
                <el-tag size="small" :type="m.protocol === 'anthropic' ? 'warning' : 'info'" effect="plain" style="margin-left:4px;">{{ protocolLabel(m.protocol) }}</el-tag>
                <span v-if="m.status !== 1" style="font-size: 11px; margin-left: 4px;">（已禁用）</span>
              </span>
              <span style="font-size: 11px; color: var(--muted);">
                {{ m.vendor_name || m.provider_name }} · {{ m.model }}
              </span>
            </div>
          </el-option>
        </el-select>
        <el-button
          v-if="sessionStore.currentAgentType === 'device_command'"
          type="success"
          size="small"
          @click="extractKnowledge"
          :loading="knowledgeLoading"
        >
          知识萃取
        </el-button>
      </div>
    </div>

    <!-- 模型不可用警告横幅（持久显示，直到用户切换模型） -->
    <div v-if="modelUnavailable" class="model-warning-banner">
      <span class="warning-icon"><AlertTriangle :size="16" /></span>
      <span class="warning-text">
        当前会话绑定的模型{{ modelUnavailableReason }}，请在上方下拉框选择其他模型后继续
      </span>
    </div>

    <!-- 消息区域 -->
    <div class="messages-area" ref="messagesContainer">
      <!-- 定时任务会话只读提示 -->
      <div v-if="isScheduledSession" class="scheduled-readonly-banner">
        <el-icon><InfoFilled /></el-icon>
        这是定时任务自动运行的会话回放，仅供查看，不可发送消息或修改审批配置。
      </div>
      <!-- 历史加载中：消息骨架屏（贴合主题的扫光/淡入动效） -->
      <SessionSkeleton v-if="sessionStore.historyLoading" />

      <template v-else>
        <!-- 空状态 -->
        <div v-if="messages.length === 0 && !isCurrentStreaming" class="empty-state">
          <div class="empty-icon"><MessageCircle :size="48" /></div>
          <div class="empty-title">开始对话</div>
          <div class="empty-desc">输入消息开始与 {{ agentLabel }} 交流</div>
        </div>

        <!-- 消息列表 -->
        <MessageList :messages="messages" @preview-image="previewUserImage" />

        <!-- 正在流式输出的助手消息 -->
        <div v-if="isCurrentStreaming && chatStore.streamingText" class="message-row assistant">
          <StreamingBubble :text="chatStore.streamingText" />
        </div>

        <!-- 思考气泡 -->
        <div v-if="isCurrentStreaming && chatStore.thinkingText" class="message-row assistant">
          <ThinkingCard :text="chatStore.thinkingText" />
        </div>

        <!-- 等待中的流式占位 -->
        <div v-if="isCurrentStreaming && !chatStore.streamingText && !chatStore.thinkingText" class="message-row assistant">
          <StreamingBubble text="" />
        </div>
      </template>
    </div>

    <!-- 工具确认弹窗 -->
    <el-dialog
      v-model="showToolConfirm"
      title="工具调用确认"
      width="480px"
      :close-on-click-modal="false"
      :close-on-press-escape="false"
      :show-close="false"
      append-to-body
    >
      <div class="confirm-body">
        <p>助手请求调用以下工具：</p>
        <div class="confirm-tool-info">
          <div class="confirm-tool-name">
            <span class="icon"><Wrench :size="16" /></span>
            <strong>{{ chatStore.pendingToolConfirm?.name }}</strong>
          </div>
          <pre v-if="chatStore.pendingToolConfirm?.args" class="tool-code">{{ formatJson(chatStore.pendingToolConfirm.args) }}</pre>
        </div>
      </div>
      <template #footer>
        <el-button @click="onToolDeny">拒绝</el-button>
        <el-button type="primary" @click="onToolApprove">批准</el-button>
      </template>
    </el-dialog>

    <!-- 危险命令确认弹窗（run_command 返回 require_confirmation） -->
    <el-dialog
      v-model="showDangerousCommandConfirm"
      title="危险命令确认"
      width="560px"
      :close-on-click-modal="false"
      :close-on-press-escape="false"
      :show-close="false"
      append-to-body
    >
      <div class="confirm-body">
        <el-alert
          type="warning"
          :closable="false"
          show-icon
          title="助手尝试执行一条危险命令"
          description="该命令可能造成不可逆的破坏。请仔细核对后决定是否允许执行。"
          style="margin-bottom: 12px;"
        />
        <div class="confirm-tool-info">
          <div class="confirm-tool-name">
            <span class="icon"><Zap :size="16" /></span>
            <strong>run_command</strong>
            <el-tag v-if="chatStore.pendingToolResultConfirm.length > 1" size="small" type="warning" style="margin-left: 8px;">
              还有 {{ chatStore.pendingToolResultConfirm.length - 1 }} 条待确认
            </el-tag>
          </div>
          <pre v-if="currentDangerousConfirm?.command" class="tool-code danger-code">{{ currentDangerousConfirm.command }}</pre>
          <p v-if="currentDangerousConfirm?.error" class="confirm-hint">{{ currentDangerousConfirm.error }}</p>
        </div>
      </div>
      <template #footer>
        <el-button @click="onDangerousDeny">拒绝执行</el-button>
        <el-button type="danger" @click="onDangerousApprove">确认执行</el-button>
      </template>
    </el-dialog>

    <!-- shell_command 审批已内嵌到命令卡底部（见 ToolCallCard），不再用全局弹窗 -->

    <!-- 知识萃取对话框 -->
    <el-dialog
      v-model="showLearnDialog"
      title="知识萃取 - FAQ 审阅"
      width="680px"
      append-to-body
      :close-on-click-modal="!learnExtracting"
      :close-on-press-escape="!learnExtracting"
      :show-close="!learnExtracting"
    >
      <!-- 厂商/设备类型选择器（始终显示） -->
      <div class="learn-meta-bar">
        <div class="learn-meta-item">
          <label>厂商</label>
          <el-select v-model="learnBrand" placeholder="选择厂商" clearable filterable size="small" style="width: 100%;" :disabled="learnExtracting || learnCommitting">
            <el-option v-for="v in appStore.catalog.brands" :key="v.id" :label="v.name_ch" :value="v.name_ch" />
          </el-select>
        </div>
        <div class="learn-meta-item">
          <label>设备类型</label>
          <el-select v-model="learnDevType" placeholder="选择设备类型" clearable filterable size="small" style="width: 100%;" :disabled="learnExtracting || learnCommitting">
            <el-option v-for="d in appStore.catalog.dev_types" :key="d.id" :label="d.name_ch" :value="d.name_ch" />
          </el-select>
        </div>
        <div class="learn-meta-item">
          <label>设备型号</label>
          <el-input v-model="learnModel" placeholder="如 S5300（可选）" size="small" clearable :disabled="learnExtracting || learnCommitting" />
        </div>
      </div>

      <div v-if="learnExtracting" class="learn-loading-overlay">
        <el-icon class="is-loading" :size="28"><Loading /></el-icon>
        <span>正在从会话中萃取知识，请稍候…</span>
      </div>
      <div v-else-if="!learnStarted" class="learn-start-section">
        <p class="learn-hint">选择厂商和设备类型后，点击「开始萃取」从当前会话提取 FAQ 知识。</p>
      </div>
      <div v-else-if="learnItems.length === 0" class="learn-empty">
        <p>暂无可萃取的知识。</p>
      </div>
      <div v-else class="learn-list">
        <div class="learn-select-bar">
          <el-checkbox
            :model-value="learnSelected.size === learnItems.length && learnItems.length > 0"
            :indeterminate="learnSelected.size > 0 && learnSelected.size < learnItems.length"
            @change="toggleSelectAllLearn"
          >
            全选 ({{ learnSelected.size }}/{{ learnItems.length }})
          </el-checkbox>
        </div>
        <div v-for="item in learnItems" :key="item._id" class="learn-item" :class="{ 'is-selected': learnSelected.has(item._id), 'is-duplicate': item.duplicate }">
          <div class="learn-item-header">
            <el-checkbox
              :model-value="learnSelected.has(item._id)"
              @change="toggleLearnSelect(item._id)"
            />
            <el-tag v-if="item.duplicate" size="small" type="warning" effect="plain" class="dup-tag">知识库已存在</el-tag>
            <el-button text size="small" type="primary" @click="onRegenerate(item)" :loading="item._loading" :disabled="learnCommitting">
              重新生成
            </el-button>
            <el-button text size="small" type="danger" @click="removeLearnItem(item._id)" :disabled="learnCommitting">
              删除
            </el-button>
          </div>
          <div class="learn-q">
            <strong>Q:</strong>
            <el-input v-model="item.question" type="textarea" :rows="2" size="small" />
          </div>
          <div class="learn-a">
            <strong>A:</strong>
            <el-input v-model="item.answer" type="textarea" :rows="3" size="small" />
          </div>
        </div>
      </div>
      <template #footer>
        <el-button @click="showLearnDialog = false" :disabled="learnExtracting || learnCommitting">取消</el-button>
        <el-button
          v-if="!learnStarted"
          type="primary"
          @click="startExtract"
          :disabled="!learnBrand || !learnDevType"
          :loading="learnExtracting"
        >
          开始萃取
        </el-button>
        <el-button
          v-else
          type="primary"
          @click="onCommitLearn"
          :disabled="learnSelected.size === 0"
          :loading="learnCommitting"
        >
          提交到知识库 ({{ learnSelected.size }})
        </el-button>
      </template>
    </el-dialog>

    <!-- 输入区域（定时任务会话只读：上传/输入/发送已禁用，配置下拉只读展示） -->
    <div class="input-area">
      <!-- 待发送图片预览条 -->
      <div v-if="pendingImages.length" class="pending-images">
        <div
          v-for="(img, i) in pendingImages"
          :key="i"
          class="pending-img-chip"
        >
          <img :src="img.url" :alt="img.filename" />
          <el-icon class="pending-img-remove" @click="removePendingImage(i)"><Close /></el-icon>
        </div>
      </div>

      <!-- 待发送文档预览条（文件名胶囊，发送时随消息一起提交，后端走 markitdown 解析） -->
      <div v-if="pendingDocs.length" class="pending-docs">
        <div v-for="(doc, i) in pendingDocs" :key="i" class="pending-doc-chip">
          <el-icon class="pending-doc-icon"><Document /></el-icon>
          <span class="pending-doc-name" :title="doc.filename">{{ doc.filename }}</span>
          <el-icon class="pending-doc-remove" @click="removePendingDoc(i)"><Close /></el-icon>
        </div>
      </div>

      <div class="input-row">
        <div class="input-wrapper">
          <!-- 图片附件按钮 -->
          <el-upload
            v-if="enableImageAttach"
            class="attach-btn"
            :show-file-list="false"
            :before-upload="handleImagePick"
            accept="image/png,image/jpeg,image/webp,image/gif"
            :disabled="!sessionStore.currentSessionId || isCurrentStreaming || isScheduledSession"
          >
            <el-icon :class="{ uploading: uploadingImage }">
              <Loading v-if="uploadingImage" />
              <Picture v-else />
            </el-icon>
          </el-upload>

          <!-- 文档附件按钮：上传 Word/Excel/PPT/PDF 等，由后端 markitdown 解析后供模型分析 -->
          <el-upload
            v-if="enableDocAttach"
            class="attach-btn"
            :show-file-list="false"
            :before-upload="handleDocPick"
            accept=".pdf,.doc,.docx,.xls,.xlsx,.ppt,.pptx,.csv,.txt,.md,.rtf"
            :disabled="!sessionStore.currentSessionId || isCurrentStreaming || isScheduledSession"
          >
            <el-icon :class="{ uploading: uploadingDoc }">
              <Loading v-if="uploadingDoc" />
              <Document v-else />
            </el-icon>
          </el-upload>

          <el-input
            v-model="inputText"
            type="textarea"
            :autosize="{ minRows: 1, maxRows: 6 }"
            :placeholder="isScheduledSession ? '定时任务会话为只读回放，不可发送消息' : '输入消息… (Enter 发送，Shift+Enter 换行)'"
            resize="none"
            @keydown="onKeydown"
            :disabled="!sessionStore.currentSessionId || isScheduledSession"
          />
        </div>
        <div class="input-actions">
        <el-tooltip
          v-if="sessionStore.currentSessionId"
          content="当前会话的思考级别（随会话模型协议变化，默认 high）"
          placement="top"
          :show-after="400"
        >
          <el-select
            v-model="chatStore.sessionThinkingLevel"
            class="thinking-select"
            size="default"
            :disabled="!sessionStore.currentSessionId || isScheduledSession"
            @change="onThinkingLevelChange"
          >
            <template #prefix>
              <span class="thinking-prefix">思考</span>
            </template>
            <el-option
              v-for="o in thinkingOptions"
              :key="o.value"
              :value="o.value"
              :label="o.label"
            />
          </el-select>
        </el-tooltip>
        <el-tooltip
          v-if="sessionStore.currentSessionId"
          content="沙箱模式：限制 AI 命令的文件读写范围（默认工作区写）"
          placement="top"
          :show-after="400"
        >
          <el-select
            v-model="chatStore.sessionPermissionPolicy.sandbox_mode"
            class="perm-select"
            size="default"
            :disabled="!sessionStore.currentSessionId || isScheduledSession"
            @change="onSandboxModeChange"
          >
            <template #prefix>
              <span class="perm-prefix">沙箱</span>
            </template>
            <el-option
              v-for="o in sandboxModeOptions"
              :key="o.value"
              :value="o.value"
              :label="o.label"
            />
          </el-select>
        </el-tooltip>
        <el-tooltip
          v-if="sessionStore.currentSessionId"
          content="审批策略：AI 执行命令前是否征求确认（默认除只读命令外都确认）"
          placement="top"
          :show-after="400"
        >
          <el-select
            v-model="chatStore.sessionPermissionPolicy.approval_policy"
            class="perm-select"
            size="default"
            :disabled="!sessionStore.currentSessionId || isScheduledSession"
            @change="onApprovalPolicyChange"
          >
            <template #prefix>
              <span class="perm-prefix">审批</span>
            </template>
            <el-option
              v-for="o in approvalPolicyOptions"
              :key="o.value"
              :value="o.value"
              :label="o.label"
            />
          </el-select>
        </el-tooltip>
        <el-button
          type="primary"
          @click="sendMessage"
          :disabled="(!inputText.trim() && pendingImages.length === 0 && pendingDocs.length === 0) || !sessionStore.currentSessionId || uploadingImage || uploadingDoc || isScheduledSession"
        >
          发送
        </el-button>
        <el-button
          v-if="isCurrentStreaming"
          type="danger"
          @click="cancelRun"
        >
          停止
        </el-button>
        </div>
      </div>

    </div>

    <!-- 图片预览（点击用户已发图片放大） -->
    <el-image-viewer
      v-if="previewVisible"
      :url-list="previewUrlList"
      @close="previewVisible = false"
    />
    </div>
  </div>
</template>

<script setup>
import { ref, watch, nextTick, computed, onMounted, onBeforeUnmount } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useSessionStore } from '../stores/session'
import { useChatStore } from '../stores/chat'
import { useAppStore } from '../stores/app'
import { useAssistantStore } from '../stores/assistant'
import { useUserStore } from '../stores/user'
import { getRunCommandDiagnostics, parseAny } from '../utils/toolResult'
import { uuidv7 } from '../utils/uuid'
import { setKbImageContext } from '../utils/markdown'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Loading, ArrowLeft, Picture, Close, Document, EditPen, InfoFilled } from '@element-plus/icons-vue'
import { MessageCircle, AlertTriangle, Wrench, Zap, Gauge } from 'lucide-vue-next'
import { learnFromSession, regenerateLearn, commitLearn, uploadImage, uploadDocument } from '../api'
import MessageList from '../components/chat/MessageList.vue'
import SessionSkeleton from '../components/chat/SessionSkeleton.vue'
import StreamingBubble from '../components/chat/StreamingBubble.vue'
import ThinkingCard from '../components/chat/ThinkingCard.vue'

const route = useRoute()
const router = useRouter()
const sessionStore = useSessionStore()
const chatStore = useChatStore()
const appStore = useAppStore()
const assistantStore = useAssistantStore()
const userStore = useUserStore()

const inputText = ref('')
// 对话输入框的附件按钮开关
// 图片按钮暂关:当前接入的模型无 VL 能力,发了也读不了图;接入 VL 模型后置回 true 即恢复
const enableImageAttach = false
const enableDocAttach = true
const pendingImages = ref([])  // 待发送的图片附件 [{ url, filename, mime_type, size }]
const uploadingImage = ref(false)
const pendingDocs = ref([])    // 待发送的文档附件 [{ url, filename, mime_type, size }]（后端走 markitdown 解析）
const uploadingDoc = ref(false)
const messagesContainer = ref(null)
const knowledgeLoading = ref(false)
const showLearnDialog = ref(false)
const learnItems = ref([])
const learnSelected = ref(new Set())
const learnExtracting = ref(false)
const learnCommitting = ref(false)
const learnBrand = ref('')
const learnDevType = ref('')
const learnModel = ref('')
const learnStarted = ref(false)

// 当前会话绑定的助手对象（从 assistantStore.assistants 中查找）
const currentAssistant = computed(() => {
  const id = sessionStore.currentAssistantId
  if (!id) return null
  return assistantStore.assistants.find((a) => a.id === id) || null
})

// 把当前会话绑定的知识库实例 id 注入 markdown 渲染上下文，
// 用于把 Dify 图片 URL 改写为带认证的代理 URL（每个实例 api_key 不同）。
watch(
  () => currentAssistant.value?.kb_instance_id,
  (id) => setKbImageContext(id),
  { immediate: true },
)

// ── 会话级思考级别切换 ──
// 思考级别跟会话绑定（不再跟助手）；进入/切换会话时由下方 watch 加载
async function onThinkingLevelChange(val) {
  await chatStore.saveSessionThinkingLevel(sessionStore.currentSessionId, val)
}

// 审批方式切换：沙箱模式 / 审批策略各自触发整体持久化（带另一半的当前值）
async function onSandboxModeChange(val) {
  await chatStore.saveSessionPermissionPolicy(
    sessionStore.currentSessionId,
    val,
    chatStore.sessionPermissionPolicy.approval_policy,
  )
}
async function onApprovalPolicyChange(val) {
  await chatStore.saveSessionPermissionPolicy(
    sessionStore.currentSessionId,
    chatStore.sessionPermissionPolicy.sandbox_mode,
    val,
  )
}

// 监听 prefillValue 变化
watch(
  () => chatStore.prefillValue,
  (newVal) => {
    if (newVal && newVal !== inputText.value) {
      inputText.value = newVal
      // 使用后清空
      chatStore.prefillMessage('')
    }
  }
)

// 本地消息列表（响应式）
const messages = ref([])

// ── 左侧轮次导航 ──
// 序号统一为纯数字（圆圈字符 ①~⑳ 与 21. 之后纯文本样式断裂，且字符宽度不一致）。
// 三位数以内（999）均整齐展示，由 CSS 控制统一徽标样式与宽度。
function turnNumber(n) {
  return String(n)
}
// 提取所有用户提问作为轮次条目（idx 为在 messages 数组中的下标，与 MessageList 的 id="msg-{idx}" 对齐）
const userTurns = computed(() => {
  const out = []
  let n = 0
  messages.value.forEach((m, i) => {
    if (m.role === 'user') {
      n += 1
      const raw = (m.content || '').replace(/\s+/g, ' ').trim()
      out.push({
        idx: i,
        number: turnNumber(n),
        msg: m,
        preview: raw.slice(0, 14),
      })
    }
  })
  return out
})
const activeTurnIdx = ref(null)
const turnListEl = ref(null)
// active 变化时，把对应轮次项滚到侧栏中部，避免「侧栏一直从 1 显示、active 被挤出可视区」。
// 用容器内手动定位（scrollTop = 项中心 - 容器半高），只滚侧栏、不影响主消息区。
function scrollActiveTurnIntoView() {
  const list = turnListEl.value
  if (!list || activeTurnIdx.value == null) return
  const el = list.querySelector(`[data-turn-idx="${activeTurnIdx.value}"]`)
  if (!el) return
  const target = el.offsetTop - list.clientHeight / 2 + el.offsetHeight / 2
  list.scrollTo({ top: Math.max(0, target), behavior: 'smooth' })
}
// 轮次侧栏时间：统一 HH:MM
function formatTurnTime(ts) {
  if (!ts) return ''
  const d = new Date(ts)
  if (isNaN(d.getTime())) return ''
  const pad = (x) => String(x).padStart(2, '0')
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`
}
// 点击轮次：滚动定位到顶部（符合从上往下阅读）+ 短暂高亮
function jumpToTurn(idx) {
  const el = document.getElementById('msg-' + idx)
  if (!el) return
  // block:'start' 让目标轮次顶到可视区上沿，上下文自然顺下来；
  // scroll-margin-top 留出顶栏高度，避免被 sticky 头部遮挡
  el.scrollIntoView({ behavior: 'smooth', block: 'start' })
  el.classList.remove('turn-flash')
  // 强制重排以重新触发动画
  void el.offsetWidth
  el.classList.add('turn-flash')
  setTimeout(() => el.classList.remove('turn-flash'), 2500)
}
// 追踪 active 轮次：以视口顶部往下 1/3 处为「阅读线」，active = 顶部已越过该线的最后一轮。
// 用 scroll 事件 + 几何计算（非 IntersectionObserver 观察带），保证滚到底部时最后一轮
// 必然命中（其 top 早已越过阅读线），修掉「22 轮滚到底仍高亮 19」的边界 bug。
function updateActiveTurn() {
  const root = messagesContainer.value
  if (!root || !userTurns.value.length) return
  // 已滚到底：内容末尾不足一屏时，末尾几轮的用户消息会停在阅读线下方，
  // 边界判定会漏掉最后一轮（22 轮滚到底卡 19 的 bug）。滚到底即高亮最后一轮，无歧义。
  const atBottom = root.scrollTop + root.clientHeight >= root.scrollHeight - 4
  if (atBottom) {
    activeTurnIdx.value = userTurns.value[userTurns.value.length - 1].idx
    return
  }
  const rootRect = root.getBoundingClientRect()
  const readLine = rootRect.top + rootRect.height / 3
  let active = null
  for (const turn of userTurns.value) {
    const el = document.getElementById('msg-' + turn.idx)
    if (!el) continue
    if (el.getBoundingClientRect().top <= readLine) active = turn.idx
    else break // 行按文档序排列，越过后即可停
  }
  // 一轮都没越过阅读线（顶部），退化为第一轮；滚到底则落在最后一轮
  activeTurnIdx.value = active !== null ? active : userTurns.value[0].idx
}
let turnScrollRaf = 0
function onTurnScroll() {
  if (turnScrollRaf) return
  turnScrollRaf = requestAnimationFrame(() => {
    turnScrollRaf = 0
    updateActiveTurn()
  })
}
function setupTurnObserver() {
  nextTick(updateActiveTurn)
}
onMounted(() => {
  nextTick(() => messagesContainer.value?.addEventListener('scroll', onTurnScroll, { passive: true }))
})
watch(() => userTurns.value.length, () => nextTick(updateActiveTurn))
// active 轮次变化 → 侧栏把该项滚到中部（用户在主区滚动时侧栏同步跟随）
watch(activeTurnIdx, () => nextTick(scrollActiveTurnIntoView))
onBeforeUnmount(() => {
  if (turnScrollRaf) cancelAnimationFrame(turnScrollRaf)
  messagesContainer.value?.removeEventListener('scroll', onTurnScroll)
})

const agentLabel = computed(() => {
  if (currentAssistant.value) {
    return `${currentAssistant.value.avatar || '🤖'} ${currentAssistant.value.name}`
  }
  // 助手列表尚未加载时，用 session store 中 history 返回的 name fallback
  if (sessionStore.currentAssistantName) {
    const kind = sessionStore.currentAssistantKind
    const prefix = kind === 1 ? '自定义' : (kind === 0 ? '内置' : '')
    return `${prefix} - ${sessionStore.currentAssistantName}`
  }
  return ''
})

// 工具确认弹窗可见性（历史加载期间不弹：pendingConfirmation 还是上一个会话的旧值，
// 等本会话 fetchHistory 回包后再按新值同步）
const showToolConfirm = computed(() => !!chatStore.pendingToolConfirm && !sessionStore.historyLoading)

// 危险命令确认弹窗可见性（队列非空时显示）
const showDangerousCommandConfirm = computed(() => chatStore.pendingToolResultConfirm.length > 0)
// 当前展示的待确认命令（队列头部）
const currentDangerousConfirm = computed(() => chatStore.pendingToolResultConfirm[0] || null)

// 本页会话的流是否正在输出：后台别的会话的流（streamSessionId 不同）不在本页渲染
//（流式气泡/思考卡/停止按钮/空状态占位均按此判定，避免后台流的内容串到正在查看的会话）
const isCurrentStreaming = computed(
  () => chatStore.isStreaming && (!chatStore.streamSessionId || chatStore.streamSessionId === sessionStore.currentSessionId),
)

// shell_command 审批内嵌：审批请求入队时逐条挂到 running 的终端命令卡（每张卡至多一条，
// 支持并发审批），由 ToolCallCard 在卡片底部内嵌渲染审批按钮（替代全局弹窗）。
// 抽成函数供两处触发：审批入队时 + 历史 messages 重建后——历史落地会整体替换 messages
// 数组，旧对象上的 _pendingApproval 随之丢失，不在重建后重挂会导致重进会话审批条消失。
// 卡片条件除 running 外也接受 aborted：后端把恢复出的未决命令卡标为 aborted，
// 只认 running 会让重进会话时（SPA 内切换、store 队列仍持有待决项）挂不上目标卡
function attachPendingApprovals() {
  for (const ap of chatStore.pendingShellApprovals || []) {
    // 只挂本会话的审批（队列里可能残留切走会话的待决项，session_id 为聊天会话 id）
    if (ap.session_id && ap.session_id !== sessionStore.currentSessionId) continue
    const target = [...messages.value]
      .reverse()
      .find(
        (m) => m.role === 'tool_call' && m.toolName === 'shell_command' && !m.result
          && (m.status === 'running' || m.status === 'aborted') && !m._pendingApproval,
      )
    if (target) target._pendingApproval = { ...ap }
  }
}
watch(() => chatStore.pendingShellApprovals, attachPendingApprovals, { immediate: true, deep: true })

// ── 监听 sessionStore.messages 变化，同步到本地（合并成对展示） ──
watch(
  () => sessionStore.messages,
  (newMsgs) => {
    const result = []
    for (const m of (newMsgs || [])) {
      if (m.role === 'tool' || m.role === 'tool_call') {
        const tn = m.toolName || m.name || ''
        result.push({
          ...m,
          role: 'tool_call',
          toolName: tn || '工具',
          args: m.args || null,
          result: m.result || null,
          status: m.status === 'calling' ? 'running' : (m.status || 'done'),
          // 历史卡带的是 tool_call_id（下划线），实时流的 appendTool 写的是 toolCallId（驼峰）。
          // 补驼峰别名对齐后者——updateToolResult/updateToolArgs 只按 toolCallId 匹配，
          // 缺别名的恢复卡收不到续跑/重挂后推来的 TOOL_CALL_RESULT，永远停在 running/aborted
          toolCallId: m.tool_call_id || m.toolCallId || null,
          // 历史恢复统一收起卡片；spawn/wait 等长参数卡尤其不能摊开（避免整段 JSON 刷屏）
          _expanded: false,
        })
      } else if (m.role === 'tool_result') {
        // 成对合并：优先按 tool_call_id 精确配对（并发工具调用的结果按完成序落库，
        // 位置配对会把 A 调用的结果填到 B 的卡片上）；双方 id 缺失时回落到位置配对
        const rid = m.tool_call_id || ''
        const lastTool = [...result]
          .reverse()
          .find((r) => r.role === 'tool_call' && !r.result && (!rid || !r.tool_call_id || r.tool_call_id === rid))
        if (lastTool) {
          lastTool.result = m.content || m.result
          lastTool.status = 'done'
        } else {
          result.push({
            role: 'tool_call',
            toolName: m.toolName || m.name || '工具',
            tool_call_id: rid || null,
            // 同上：补驼峰别名，与实时卡字段名对齐
            toolCallId: rid || null,
            status: 'done',
            args: null,
            result: m.content || m.result,
            _expanded: false,
          })
        }
      } else {
        result.push({ ...m, _expanded: undefined })
      }
    }
    // 预计算工具结果解析字段，避免模板重复调用解析函数（性能优化）
    for (const msg of result) {
      if (msg.role !== 'tool_call' || !msg.result) continue
      msg._diagnostics = getRunCommandDiagnostics(msg.result)
    }
    messages.value = result
    // 历史重建会整体替换 messages：待决审批要重挂到新对象上（M-1）；
    // 流仍在跑时把 store 的 SSE 回调换成本实例的闭包（H-1，重进流式会话重挂）
    attachPendingApprovals()
    reattachSseIfStreaming()
    scrollToBottom()
  },
  { immediate: true, deep: true }
)

// H-1 重挂：store 的流循环持有发起时组件实例的闭包；本组件若是重进流式会话的新实例
//（旧实例已随路由销毁），不重挂则流式输出/工具卡全部追加进已销毁实例的数组——页面
// 冻住直到流结束。必须在历史快照落地之后再重挂：早于快照重挂，随后落地的历史会整体
// 覆盖掉重挂后已追加的实时项（messages 赋值是整组替换）
function reattachSseIfStreaming() {
  if (
    chatStore.isStreaming &&
    chatStore.streamSessionId &&
    chatStore.streamSessionId === sessionStore.currentSessionId &&
    !sessionStore.historyLoading
  ) {
    chatStore.reattachSseCallbacks(...makeSseCallbacks())
  }
}

// ── 监听流式文本，实时滚动 ──
watch(
  () => chatStore.streamingText,
  () => { scrollToBottom() }
)

// ── 监听待确认工具，恢复确认弹窗 ──
// 双向同步：本会话有持久化 pending → 恢复弹窗；没有 → 清掉弹窗。
// 否则 A 会话弹出的确认框会残留到 B 会话（批准操作会把决策发进当前查看的会话）
watch(
  () => sessionStore.pendingConfirmation,
  (pc) => {
    if (pc && pc.tool_name) {
      chatStore.pendingToolConfirm = {
        name: pc.tool_name,
        args: pc.args,
        function_call_id: pc.function_call_id,
      }
    } else {
      chatStore.pendingToolConfirm = null
    }
  },
  { immediate: true }
)

// ── 监听思考文本，实时滚动 ──
watch(
  () => chatStore.thinkingText,
  () => { scrollToBottom() }
)

// ── 进入会话时检测当前模型是否可用 ──
// 用 computed 驱动持久化横幅（比 ElMessage 更可靠可见）
const modelUnavailableReason = ref('')

const modelUnavailable = computed(() => {
  const modelId = chatStore.currentModelId
  if (!modelId || appStore.models.length === 0) return false
  if (modelId === 'default') return false
  const target = appStore.models.find(m => m.id === modelId)
  if (!target) {
    modelUnavailableReason.value = '已不存在'
    return true
  }
  if (target.status !== 1) {
    modelUnavailableReason.value = '已被禁用'
    return true
  }
  return false
})

// 监听会话切换：弹出一次 ElMessage 提醒（横幅会持久显示）
watch(
  () => sessionStore.currentSessionId,
  () => {
    nextTick(() => {
      if (modelUnavailable.value) {
        ElMessage.warning(
          `当前会话绑定的模型${modelUnavailableReason.value}，请在上方下拉框选择其他模型后继续`,
          { duration: 6000 }
        )
      }
    })
  }
)

// 页面挂载时也检测一次（覆盖刷新页面后直接进入 /chat 的场景）
// 页面挂载：从 URL 恢复会话（解决刷新丢失问题）
// 三种情况：
//   1. URL 有 session 且与 store 不一致 → 重新加载该会话
//   2. URL 无 session 但 store 有 → 把 store 的 id 补到 URL（保持可分享/可刷新）
//   3. 两者都无 → 不处理（等待用户从历史列表选择）
// 从 URL 恢复会话（route.query 解析驱动，比 onMounted 更可靠：多标签并发时 query 一定就绪后再恢复）
// 恢复请求序号：恢复期间用户又切换（后退/前进连点）时旧的恢复不得改写模型下拉。
// 旧版用布尔标志直接丢弃第二次恢复——被丢弃的恰是用户「前进」到达的会话，
// 随后 URL 同步 watch 会把地址劫持回旧会话。消息数据本身由 loadHistoryMessages
// 内部的序号守卫保证最后写入胜出
let restoreSessionSeq = 0
async function restoreSessionFromUrl(urlSessionId, storeSessionId) {
  if (!urlSessionId) return
  if (urlSessionId === storeSessionId && sessionStore.sessions.length) return // 已恢复
  const seq = ++restoreSessionSeq
  try {
    const boundModelId = await sessionStore.selectSession(urlSessionId, null)
    if (seq !== restoreSessionSeq) return // 期间有更新的切换，模型跟随最新
    chatStore.loadModelForSession(boundModelId || null)
  } catch (e) {
    console.warn('[ChatPage] 从 URL 恢复会话失败', e)
  }
}
// watch route.query.session：query 解析完成（含并发多标签时）即恢复；URL 后续变化也恢复
watch(
  () => route.query.session,
  (sid) => {
    if (sid && typeof sid === 'string') restoreSessionFromUrl(sid, sessionStore.currentSessionId)
    // assistant_id 单独出现（助手页「对话」/弹框「新建会话」入口）时不回写旧会话：
    // onMounted 会新建会话并跳转，此刻把 URL 钉到残留的 currentSessionId 会在建会话
    // 窗口期闪旧会话内容，建会话失败时更是直接卡死在旧会话
    else if (!sid && !route.query.assistant_id && sessionStore.currentSessionId)
      router.replace({ path: '/chat', query: { session: sessionStore.currentSessionId } })
  },
  { immediate: true },
)

onMounted(async () => {
  // 确保助手列表已加载，currentAssistant 计算属性才能正确解析
  assistantStore.loadAssistants()

  const urlSessionId = route.query.session
  const storeSessionId = sessionStore.currentSessionId

  restoreSessionFromUrl(urlSessionId, storeSessionId)

  // 从助手页面进入（/chat?assistant_id=xxx）：绑定助手到当前会话
  const urlAssistantId = route.query.assistant_id
  if (urlAssistantId && typeof urlAssistantId === 'string') {
    assistantStore.selectAssistant(urlAssistantId)
    if (!urlSessionId) {
      // 只带 assistant_id（「开始对话」无历史会话 / 弹框「新建会话」入口）→ 一律新建会话并绑定
      // 该助手。分流看 URL 而非 currentSessionId：后者是 Pinia 单例残留，看过任何会话后就
      // 非空，用它会把这些入口错误地落回旧会话（失败抛错需接住提示，不能冒泡成未处理拒绝）
      try {
        const newId = await sessionStore.createNewSession(null, null, urlAssistantId)
        if (newId) {
          router.replace({ path: '/chat', query: { session: newId } })
        }
      } catch (e) {
        ElMessage.error(e.message || '创建会话失败')
      }
    } else {
      // URL 显式带了 session（弹框「继续该会话」）→ 仅更新绑定
      sessionStore.bindAssistant(urlAssistantId)
    }
  }

  // 同步检测模型可用性
  if (modelUnavailable.value) {
    ElMessage.warning(
      `当前会话绑定的模型${modelUnavailableReason.value}，请在上方下拉框选择其他模型后继续`,
      { duration: 6000 }
    )
  }

  // 初始化轮次导航的 active 计算（历史消息已加载时立即定位）
  nextTick(setupTurnObserver)
})

// 监听会话切换：同步到 URL（保证刷新不丢失）
watch(
  () => sessionStore.currentSessionId,
  (newId, oldId) => {
    // 会话切换/进页时过滤待确认危险命令队列（条目带 session_id，只留本会话的），
    // 避免跨会话污染（immediate 覆盖从列表进详情页的路径：selectSession 在路由跳转前
    // 已写入新 id，非 immediate 时首次挂载不触发，上一个会话的残留弹窗会带进本会话）
    if (newId !== oldId) {
      chatStore.clearToolResultConfirm()
    }
    // 首次 immediate 同步（oldId === undefined）只清残留弹窗，不做 URL 改写：
    // 硬刷新 /chat?session=xxx 时 store 尚空，此刻按「无会话」strip 掉 URL 参数
    // 会制造一次多余的导航并让恢复失败时丢失重试入口
    if (oldId === undefined) return
    const urlSession = route.query.session
    if (newId && urlSession !== newId) {
      router.replace({ path: '/chat', query: { session: newId } })
    } else if (!newId && urlSession) {
      // 会话被删除等场景：清掉 URL 的 session 参数
      router.replace({ path: '/chat' })
    }
  },
  { immediate: true }
)

// 进入/切换会话时加载会话级思考级别（immediate 覆盖刷新直接进 /chat 的场景）
watch(
  () => sessionStore.currentSessionId,
  (newId) => {
    chatStore.loadSessionThinkingLevel(newId)
    chatStore.loadSessionPermissionPolicy(newId)
  },
  { immediate: true },
)

// ── 格式化 JSON ──
function formatJson(value) {
  if (!value) return ''
  if (typeof value === 'string') {
    try {
      return JSON.stringify(JSON.parse(value), null, 2)
    } catch {
      return value
    }
  }
  try {
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
}

// parseAny 已复用 toolResult.js 的实现，避免重复定义

// 工具结果解析与渲染（含 Rhai 高亮 / 复制 / 图标）已下沉到 components/chat/ToolCallCard.vue

// ── 滚动到底部 ──
function scrollToBottom() {
  nextTick(() => {
    const el = messagesContainer.value
    if (el) el.scrollTop = el.scrollHeight
  })
}

// ── 死循环去重：连续两条 assistant 消息若归一化后高度相似，视为重复退化 ──
// 后端检测到死循环会注入重导向 prompt 让模型换方向；在模型真正跳出循环前，
// 重复段落照常流式推过来，这里在前端落库时丢弃，避免刷屏（保留首条）。
function normalizeForCompare(s) {
  return (s || '').replace(/[\s\p{P}]/gu, '').toLowerCase()
}
// 归一化后长度足够、且前 60 字符相同 → 判定为重复退化（更敏感：死循环段开头几乎一致）
function isDupAssistant(prev, curr) {
  const a = normalizeForCompare(prev)
  const b = normalizeForCompare(curr)
  if (a.length < 30 || b.length < 30) return false
  return a.slice(0, 60) === b.slice(0, 60)
}

// ── SSE 回调工厂：统一构造 appendMsg/appendTool/updateToolResult/onDone/updateToolArgs ──
// 消除 sendMessage / onToolApprove / onToolDeny / onDangerousApprove 中的 7 参数重复
function makeSseCallbacks() {
  return [
    (role, content, attachments) => {
      // 死循环去重：连续两条 assistant 消息若高度相似则丢弃这条（保留首条），
      // 避免重复退化段刷屏。后端已注入重导向，等模型换方向后的新内容正常落库。
      if (role === 'assistant') {
        const prevAssistant = [...messages.value].reverse().find((m) => m.role === 'assistant')
        if (prevAssistant && isDupAssistant(prevAssistant.content, content)) {
          return
        }
      }
      messages.value.push({ role, content, attachments, _expanded: undefined, timestamp: new Date().toISOString() })
      scrollToBottom()
    },
    null, // appendThinking（暂未使用）
    (toolName, status, toolCallId, serverName) => {
      messages.value.push({
        role: 'tool_call',
        toolName,
        serverName: serverName || null,
        toolCallId: toolCallId || null,
        status,
        args: null,
        result: null,
        _expanded: false,
        timestamp: new Date().toISOString(),
      })
      scrollToBottom()
    },
    (result, toolCallId) => {
      // 按 tool_call_id 精确匹配卡片；无 id 时回落到最后一条 tool_call。
      // 修复并发工具调用结果错配：模型一次发多个 shell_command 时，旧逻辑“找最后一条”
      // 会把前一个命令的结果填进后一个卡片，导致前者永远 running（刷新才恢复）。
      const target = toolCallId
        ? messages.value.find((m) => m.role === 'tool_call' && m.toolCallId === toolCallId)
        : [...messages.value].reverse().find((m) => m.role === 'tool_call')
      if (target) {
        target.result = result
        // 取消补发的终态 RESULT（含 {"cancelled":true}）→ 标记 aborted，否则 done
        let cancelled = false
        try { cancelled = JSON.parse(result)?.cancelled === true } catch (_) {}
        target.status = cancelled ? 'aborted' : 'done'
      }
    },
    (endedSessionId) => {
      // 只清尾本组件当前展示会话的流（endedSessionId 是刚结束流自己的归属会话）；
      // 查看别的会话时后台流结束，不应碰当前页面的卡片状态
      if (endedSessionId && endedSessionId !== sessionStore.currentSessionId) {
        scrollToBottom()
        return
      }
      // 流结束清尾：剥掉消息上仍挂着的 shell 审批条（store 队列已随流结束清空，
      // 后端注册表同样注销，残留按钮点了只会打 NOT_FOUND）；没收到终态 RESULT 的
      // running 卡标 aborted（与 cancelRun 的本地兜底一致，避免卡片永远转圈）
      messages.value.forEach((m) => {
        if (m._pendingApproval) m._pendingApproval = null
        if (m.role === 'tool_call' && m.status === 'running') m.status = 'aborted'
      })
      scrollToBottom()
    },
    (argsDelta, toolCallId) => {
      const target = toolCallId
        ? messages.value.find((m) => m.role === 'tool_call' && m.toolCallId === toolCallId)
        : [...messages.value].reverse().find((m) => m.role === 'tool_call')
      if (target) {
        target.args = (target.args || '') + argsDelta
      }
    },
    // 子 agent（spawn_agent）活动：按 task_name upsert 一条「子任务」消息
    (evt) => {
      let target = messages.value.find((m) => m.role === 'child_agent' && m.taskName === evt.task_name)
      if (!target) {
        target = {
          role: 'child_agent',
          taskName: evt.task_name,
          status: 'running',
          text: '',
          toolCalls: [],
          _expanded: false,
          timestamp: new Date().toISOString(),
        }
        messages.value.push(target)
        scrollToBottom()
      }
      if (evt.kind === 'started') {
        target.status = 'running'
      } else if (evt.kind === 'text') {
        target.text += evt.delta || ''
      } else if (evt.kind === 'tool_call') {
        target.toolCalls.push({
          toolCallId: evt.tool_call_id,
          name: evt.name || '',
          args: evt.args || '',
          result: null,
          status: 'running',
        })
      } else if (evt.kind === 'tool_result') {
        const tc = target.toolCalls.find((t) => t.toolCallId === evt.tool_call_id)
        if (tc) { tc.result = evt.content; tc.status = 'done' }
      } else if (evt.kind === 'finished') {
        target.status = evt.ok ? 'completed' : 'failed'
        if (evt.result && !target.text.trim()) target.text = evt.result
      }
    },
  ]
}

// ── 发送消息 ──
function sendMessage() {
  const text = inputText.value.trim()
  if (!text && pendingImages.value.length === 0 && pendingDocs.value.length === 0) return
  // 会话未就绪（URL 恢复中/未选会话）时拦截，避免消息以空/错会话 id 发出而丢失。
  // 按钮已按此禁用，此处兜底 Enter 快捷键路径（onKeydown 不经按钮 disabled）。
  if (!sessionStore.currentSessionId) {
    ElMessage.warning('会话加载中，请稍候…')
    return
  }
  // 定时任务会话只读：兜底 Enter 快捷键路径（textarea 虽 disabled，双保险）。
  if (isScheduledSession.value) {
    ElMessage.info('定时任务会话为只读回放，不可发送消息')
    return
  }
  inputText.value = ''
  // 图片 + 文档合并为 attachments；文档随消息提交，由后端 markitdown 解析后注入对话
  const attachments = [...pendingImages.value.splice(0), ...pendingDocs.value.splice(0)]
  chatStore.sendMessage(text, ...makeSseCallbacks(), attachments)
}

// ── 图片附件：上传 + 预览 + 移除 ──
async function handleImagePick(file) {
  if (!file.type.startsWith('image/')) {
    ElMessage.error('仅支持图片格式')
    return false
  }
  if (file.size > 20 * 1024 * 1024) {
    ElMessage.error('图片不能超过 20MB')
    return false
  }
  uploadingImage.value = true
  try {
    const env = await uploadImage(file)
    if (env.code !== 0 || !env.data) {
      ElMessage.error(env.message || '上传失败')
    } else {
      pendingImages.value.push(env.data)
    }
  } catch (e) {
    ElMessage.error('上传失败：' + (e.message || e))
  } finally {
    uploadingImage.value = false
  }
  return false  // 阻止 el-upload 默认上传行为
}

function removePendingImage(i) {
  pendingImages.value.splice(i, 1)
}

// ── 文档附件：上传 + 预览 + 移除（Word/Excel/PPT/PDF 等，后端走 markitdown 解析） ──
async function handleDocPick(file) {
  const okExt = /\.(pdf|docx?|xlsx?|pptx?|csv|txt|md|markdown|rtf)$/i
  if (!okExt.test(file.name)) {
    ElMessage.error('仅支持 PDF/Word/Excel/PPT/CSV/TXT/MD/RTF')
    return false
  }
  if (file.size > 20 * 1024 * 1024) {
    ElMessage.error('文档不能超过 20MB')
    return false
  }
  uploadingDoc.value = true
  try {
    const env = await uploadDocument(file)
    if (env.code !== 0 || !env.data) {
      ElMessage.error(env.message || '上传失败')
    } else {
      pendingDocs.value.push(env.data)
    }
  } catch (e) {
    ElMessage.error('上传失败：' + (e.message || e))
  } finally {
    uploadingDoc.value = false
  }
  return false
}

function removePendingDoc(i) {
  pendingDocs.value.splice(i, 1)
}

// 用户已发图片预览（点击放大）
const previewVisible = ref(false)
const previewUrlList = ref([])
function previewUserImage(url) {
  previewUrlList.value = [url]
  previewVisible.value = true
}

// ── 取消运行 ──
function cancelRun() {
  chatStore.cancel()
  // 清除已挂到命令卡的 shell 审批条 + tool_confirmation 的 session 标记
  // （store 的三态 pending 已在 chatStore.cancel 内清除）
  messages.value.forEach((m) => {
    if (m._pendingApproval) m._pendingApproval = null
    // 仍在 running 的工具调用立即标为 aborted：后端补发的终态 RESULT 可能因前端 abort 竞态丢失，
    // 这里本地兜底，避免取消后卡片卡在「沙箱执行中」直到刷新
    if (m.role === 'tool_call' && m.status === 'running') m.status = 'aborted'
  })
  sessionStore.pendingConfirmation = null
}

// ── 键盘事件 ──
function onKeydown(e) {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault()
    sendMessage()
  }
}

// ── 模型切换 ──
// 下拉框收起时显示的文本：模型名 (供应商标识)，避免同模型不同供应商无法区分
function modelLabel(m) {
  const vendor = m.vendor_name || m.provider_name || ''
  return vendor ? `${m.name} (${vendor})` : m.name
}

function protocolLabel(p) {
  return p === 'anthropic' ? 'Anthropic' : 'OpenAI'
}

// 当前会话模型的协议（anthropic / openai_compat），决定思考级别可选档位
const currentProtocol = computed(
  () => appStore.models.find((m) => m.id === chatStore.currentModelId)?.protocol,
)
// 思考级别选项按协议动态：anthropic 5 档（含 max），openai_compat 4 档（无 max）
const thinkingOptions = computed(() => {
  const all = [
    { value: 'low', label: '低' },
    { value: 'medium', label: '中' },
    { value: 'high', label: '高' },
    { value: 'xhigh', label: '极高' },
    { value: 'max', label: '最高' },
  ]
  return currentProtocol.value === 'openai_compat' ? all.filter((o) => o.value !== 'max') : all
})

// 审批方式选项（静态，对齐 codex 双轴；不随模型协议变化）
// 沙箱模式：read-only 只读 / workspace-write 工作区写 / danger-full-access 完全访问
// 完全访问（danger-full-access）仅管理员可见可选——后端 update 接口 + 执行入口双重强制，
// 前端这里只做 UX 隐藏（非管理员看不到该选项，避免误设后被服务端拒绝）。
const sandboxModeOptions = computed(() => {
  const all = [
    { value: 'read-only', label: '只读' },
    { value: 'workspace-write', label: '工作区写' },
    { value: 'danger-full-access', label: '完全访问' },
  ]
  if (userStore.user?.is_admin) return all
  return all.filter((o) => o.value !== 'danger-full-access')
})
// 定时任务会话：只读回放——禁发消息、禁改沙箱/审批。source_type=1 由后端按会话标记返回。
// （置于审批选项前，供 options 的 disabled 判定引用。）
const isScheduledSession = computed(() => chatStore.sessionSourceType === 1)
// 审批策略：unless-trusted 除只读外都确认 / on-request 模型决定 / never 从不确认
// （隐藏 on-request-rule-request-permission：依赖规则系统，普通用户用不到）
// auto（自动批准）仅定时任务无人值守会话由后端写入——普通会话直接不显示该项
// （后端 update 接口拒绝 auto，置灰仍可见会误导；定时会话保留以正确只读展示其策略）。
const approvalPolicyOptions = computed(() => {
  const all = [
    { value: 'unless-trusted', label: '都确认' },
    { value: 'on-request', label: '模型决定' },
    { value: 'never', label: '从不确认' },
  ]
  if (isScheduledSession.value) {
    all.push({ value: 'auto', label: '自动批准(定时任务)' })
  }
  return all
})

// 会话标题：定时任务会话不在普通会话列表（取不到 displayTitle），用 permission_policy 接口兜底带回的标题。
const sessionTitle = computed(() =>
  isScheduledSession.value
    ? (chatStore.scheduledSessionTitle || sessionStore.displayTitle || '定时任务会话')
    : (sessionStore.displayTitle || '当前会话'),
)

// ── footer 上下文 token 用量 ──
// token 紧凑格式化（对齐 codex format_tokens_compact：1234 → 1.2k）
function formatTokens(n) {
  if (!n) return '0'
  if (n < 1000) return String(n)
  if (n < 10000) return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'k'
  return Math.round(n / 1000) + 'k'
}
// footer 用量文本：对齐 codex「XX% context left」百分比显示，clamp(0,100) 天然不超限。
// 无百分比字段时 fallback 到旧「已用 / 窗口总量」绝对值。
// 子 agent 并行任务的花费（child_tokens）追加展示，不并入已用（语义不同：那是独立上下文的花费）
const usageText = computed(() => {
  const u = chatStore.contextUsage
  if (!u || !u.total_tokens) return ''
  const child = u.child_tokens > 0 ? ` (+子任务 ${formatTokens(u.child_tokens)})` : ''
  if (u.context_remaining_percent != null) {
    return `${u.context_remaining_percent}% context left${child}`
  }
  const denominator = u.window_size || u.threshold
  if (denominator > 0) return `${formatTokens(u.total_tokens)} / ${formatTokens(denominator)}${child}`
  return `${formatTokens(u.total_tokens)} tokens${child}`
})
// 收起态紧凑用量：侧边栏收起 56px 宽放不下完整文本，只显图标 + 百分比。
const usageShort = computed(() => {
  const u = chatStore.contextUsage
  if (!u || !u.total_tokens) return ''
  if (u.context_remaining_percent != null) return `${u.context_remaining_percent}%`
  return formatTokens(u.total_tokens)
})
// 用量警告：对齐 codex ≤15% context left 标黄（≈ 85% 已用）
const usageNearLimit = computed(() => {
  const u = chatStore.contextUsage
  if (!u || !u.total_tokens) return false
  if (u.context_remaining_percent != null) return u.context_remaining_percent <= 15
  if (!u.threshold) return false
  return u.total_tokens / u.threshold >= 0.85
})

// 记录上一次的模型 id，用于 onModelChange 比较新旧模型的 protocol
// 通过 watch 与 currentModelId 保持同步；由于 watch 回调在 flush 时才执行，
// 而 @change 处理器同步读取本值，因此用户切换模型时读到的是切换前的旧 id
const prevModelId = ref(chatStore.currentModelId)
watch(
  () => chatStore.currentModelId,
  (v) => { prevModelId.value = v },
)

async function onModelChange(val) {
  const sid = sessionStore.currentSessionId
  // 比较新旧模型的 protocol（anthropic / openai_compat）
  const oldProto = appStore.models.find((m) => m.id === prevModelId.value)?.protocol
  const newProto = appStore.models.find((m) => m.id === val)?.protocol
  const sw = await chatStore.saveModelForSession(sid, val)
  // 模型已切换：时间线插入分隔条（仅会话已有对话时；后端已落 system 事件，重进会话从历史恢复）
  if (sw && messages.value.some((m) => m.role === 'user' || m.role === 'assistant')) {
    messages.value.push({ role: 'model_switched', from: sw.from, to: sw.to, timestamp: new Date().toISOString() })
    scrollToBottom()
  }
  // 协议变化：当前级别新协议不支持（OpenAI 无 max）→ 降到 xhigh；其余两协议都支持，保留
  if (oldProto && newProto && oldProto !== newProto) {
    const cur = chatStore.sessionThinkingLevel
    if (newProto !== 'anthropic' && cur === 'max') {
      await chatStore.saveSessionThinkingLevel(sid, 'xhigh')
    }
  }
}

// 返回会话列表：优先用 sessionStorage 保存的列表 URL（含筛选状态）
function goBackToList() {
  // 定时任务会话：返回所属任务的运行记录详情页，而非普通会话列表
  if (isScheduledSession.value && chatStore.sessionScheduleTaskId) {
    router.push(`/scheduled-tasks/${chatStore.sessionScheduleTaskId}`)
    return
  }
  const savedUrl = sessionStorage.getItem('sessions_list_url')
  if (savedUrl) {
    router.push(savedUrl)
  } else if (window.history.length > 1) {
    router.back()
  } else {
    router.push('/sessions')
  }
}

// 会话详情页就地重命名：与列表页 onRename 同款 prompt，沿用一致交互
async function onRenameSession() {
  const id = sessionStore.currentSessionId
  if (!id) return
  try {
    const { value } = await ElMessageBox.prompt('输入新名称', '重命名会话', {
      inputValue: sessionStore.displayTitle || '',
      confirmButtonText: '保存',
      cancelButtonText: '取消',
    })
    if (value?.trim()) await sessionStore.renameSessionById(id, value.trim())
  } catch (_) {}
}

// ── 工具确认 ──
function onToolApprove() {
  const result = chatStore.resolveToolConfirm(true)
  sessionStore.pendingConfirmation = null
  if (result?.name) {
    chatStore.sendToolDecision({ [result.name]: 'approve' }, ...makeSseCallbacks())
  }
}

function onToolDeny() {
  const result = chatStore.resolveToolConfirm(false)
  sessionStore.pendingConfirmation = null
  ElMessage.info('已拒绝工具调用')
  if (result?.name) {
    chatStore.sendToolDecision({ [result.name]: 'deny' }, ...makeSseCallbacks())
  }
}

// ── 危险命令确认（run_command 工具结果层） ──
// 批准：把"用户已确认 + confirm_token"作为新消息发给模型，引导它带 token 重发命令
function onDangerousApprove() {
  const { approved, prompt } = chatStore.resolveToolResultConfirm(true)
  if (!approved || !prompt) return
  ElMessage.success('已批准执行，正在重新调用…')
  chatStore.sendMessage(prompt, ...makeSseCallbacks())
}

// 拒绝：告知模型用户拒绝了该命令，让模型基于此决定替代方案
function onDangerousDeny() {
  const confirm = chatStore.pendingToolResultConfirm[0]
  chatStore.resolveToolResultConfirm(false)
  ElMessage.info('已拒绝执行该危险命令')
  // 发送拒绝消息引导模型选择替代方案（而非卡住等待）
  const command = confirm?.command || ''
  const prompt = `用户已拒绝执行该危险命令${command ? `（${command}）` : ''}，请改用其他安全方案完成目标。`
  chatStore.sendMessage(prompt, ...makeSseCallbacks())
}

// ── 知识萃取（列表项用唯一 _id 键控：数组索引在删除/并发重新生成时会上移，
// 旧索引键控会把生成结果写错行、勾选错行） ──
function toggleLearnSelect(id) {
  const set = new Set(learnSelected.value)
  if (set.has(id)) set.delete(id)
  else set.add(id)
  learnSelected.value = set
}

function toggleSelectAllLearn() {
  if (learnSelected.value.size === learnItems.value.length) {
    learnSelected.value = new Set()
  } else {
    learnSelected.value = new Set(learnItems.value.map((i) => i._id))
  }
}

function removeLearnItem(id) {
  const idx = learnItems.value.findIndex((i) => i._id === id)
  if (idx !== -1) learnItems.value.splice(idx, 1)
  const set = new Set(learnSelected.value)
  set.delete(id)
  learnSelected.value = set
}

function extractKnowledge() {
  if (!sessionStore.currentSessionId) return
  learnStarted.value = false
  learnItems.value = []
  learnSelected.value = new Set()

  // 自动从会话历史中提取 brand/dev_type/model
  let extractedBrand = ''
  let extractedDevType = ''
  let extractedModel = ''
  // 从后往前找最后一次 search_kb 调用
  const searchMsgs = [...messages.value].reverse().filter((m) =>
    m.role === 'tool_call' && m.toolName === '检索知识库'
  )
  for (const msg of searchMsgs) {
    const args = parseAny(msg.args)
    if (args && typeof args === 'object') {
      const brand = args.brand
      const devType = args.dev_type || args.devType
      const model = args.model
      if (brand && typeof brand === 'string') extractedBrand = brand
      if (devType && typeof devType === 'string') extractedDevType = devType
      // model 可选：从命中的那次调用里附带读取（不作为 break 条件，型号可能为空）
      if (model && typeof model === 'string') extractedModel = model
      if (extractedBrand && extractedDevType) break
    }
  }
  learnBrand.value = extractedBrand
  learnDevType.value = extractedDevType
  learnModel.value = extractedModel

  showLearnDialog.value = true
}

async function startExtract() {
  if (!sessionStore.currentSessionId) return
  if (!currentAssistant.value?.kb_instance_id) {
    ElMessage.warning('当前助手未绑定知识库，请先在助手设置中绑定')
    return
  }
  if (!learnBrand.value || !learnDevType.value) {
    ElMessage.warning('请先选择厂商和设备类型')
    return
  }
  knowledgeLoading.value = true
  learnExtracting.value = true
  learnItems.value = []
  learnSelected.value = new Set()
  try {
    const { data, code, message } = await learnFromSession({
      session_id: sessionStore.currentSessionId,
      brand: learnBrand.value,
      dev_type: learnDevType.value,
      model: learnModel.value,
      instance_id: currentAssistant.value?.kb_instance_id || undefined,
    })
    if (code === 0) {
      learnItems.value = (data.candidates || []).map((item) => ({
        _id: uuidv7(),
        question: item.title || '',
        answer: item.content || '',
        duplicate: item.duplicate || false,
        _loading: false,
      }))
      learnSelected.value = new Set(learnItems.value.map((i) => i._id))
    } else {
      ElMessage.warning(message || '暂无可萃取的知识')
    }
  } catch (e) {
    ElMessage.error('知识萃取失败: ' + (e.message || '未知错误'))
  } finally {
    knowledgeLoading.value = false
    learnExtracting.value = false
    learnStarted.value = true
  }
}

async function onRegenerate(item) {
  if (!item) return
  item._loading = true
  try {
    const { data, code, message } = await regenerateLearn({
      session_id: sessionStore.currentSessionId,
      brand: learnBrand.value,
      dev_type: learnDevType.value,
      model: learnModel.value,
      target_title: item.question,
      feedback: '',
      instance_id: currentAssistant.value?.kb_instance_id || undefined,
    })
    if (code === 0 && data.candidates && data.candidates.length > 0) {
      const first = data.candidates[0]
      // 回包时按 _id 重新定位行（请求期间列表可能被删改，索引会上移错行）
      const idx = learnItems.value.findIndex((x) => x._id === item._id)
      if (idx !== -1) {
        learnItems.value[idx] = {
          _id: item._id,
          question: first.title || item.question,
          answer: first.content || '',
          duplicate: first.duplicate || false,
          _loading: false,
        }
      }
    } else {
      ElMessage.warning(message || '重新生成失败')
    }
  } catch (e) {
    ElMessage.error('重新生成失败: ' + (e.message || '未知错误'))
  } finally {
    item._loading = false
  }
}

async function onCommitLearn() {
  const selectedItems = [...learnSelected.value]
    .map((id) => learnItems.value.find((i) => i._id === id))
    .filter(Boolean)
  if (selectedItems.length === 0) return
  if (!currentAssistant.value?.kb_instance_id) {
    ElMessage.warning('当前助手未绑定知识库，请先在助手设置中绑定')
    return
  }
  learnCommitting.value = true
  try {
    const { data, code, message } = await commitLearn({
      brand: learnBrand.value,
      dev_type: learnDevType.value,
      model: learnModel.value,
      items: selectedItems.map((i) => ({
        title: i.question,
        content: i.answer,
      })),
      instance_id: currentAssistant.value?.kb_instance_id || undefined,
    })
    if (code === 0) {
      ElMessage.success(data.message || `已提交 ${selectedItems.length} 条知识到知识库`)
      showLearnDialog.value = false
      learnItems.value = []
      learnSelected.value = new Set()
    } else {
      ElMessage.error(message || '提交失败')
    }
  } catch (e) {
    ElMessage.error('提交失败: ' + (e.message || '未知错误'))
  } finally {
    learnCommitting.value = false
  }
}
</script>

<style scoped>
.chat-page {
  display: flex;
  flex-direction: row;
  height: 100%;
  background: var(--bg);
  overflow: hidden;
}

/* ── 左侧轮次导航 ── */
.turn-sidebar {
  flex-shrink: 0;
  /* 收起宽度：容下三位数序号徽标 + 底部 token 用量行，留出呼吸空间不挤 */
  width: 76px;
  /* 高度取中部 2/3、竖向居中，不与会话整高等平齐，更轻盈 */
  height: 66%;
  align-self: center;
  display: flex;
  flex-direction: column;
  border: 1px solid var(--border);
  border-radius: var(--radius-md, 8px);
  margin-left: 6px;
  background: rgba(6, 6, 10, 0.6);
  overflow: hidden;
  transition: width 0.2s ease;
}
.turn-sidebar:hover {
  width: 160px;
}
.turn-list {
  flex: 1;
  overflow-y: auto;
  padding: 8px 6px;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.turn-item {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 7px 8px;
  border-radius: var(--radius-sm);
  cursor: pointer;
  transition: background 0.15s ease, border-color 0.15s ease;
  border: 1px solid transparent;
  white-space: nowrap;
  overflow: hidden;
  /* 关键：禁止 flex 压缩。轮次多/侧栏矮时各项保持固有高度，
     超出由 .turn-list 滚动承载，避免被挤压导致序号与文字重叠 */
  flex-shrink: 0;
}
.turn-item:hover {
  background: rgba(0, 212, 255, 0.06);
}
.turn-item.active {
  background: rgba(0, 212, 255, 0.12);
  border-color: rgba(0, 212, 255, 0.35);
}
.turn-no {
  flex-shrink: 0;
  /* 统一样式的序号徽标：纯数字（1/22/105 通用），等宽数字、最小宽度容三位数，
     居中展示，替代原先 ①~⑳ 圆圈字符与 21. 纯文本的不一致 */
  min-width: 30px;
  height: 20px;
  padding: 0 4px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  line-height: 1;
  color: var(--accent);
  background: rgba(0, 212, 255, 0.08);
  border-radius: 5px;
}
.turn-item.active .turn-no {
  font-weight: 700;
  background: rgba(0, 212, 255, 0.2);
}
.turn-meta {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 1px;
  overflow: hidden;
  opacity: 0;
  transition: opacity 0.15s ease;
}
.turn-sidebar:hover .turn-meta {
  opacity: 1;
}
.turn-time {
  font-size: 10px;
  color: var(--muted);
  line-height: 1.2;
}
.turn-text {
  font-size: 12px;
  color: var(--text);
  line-height: 1.35;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.turn-empty {
  padding: 16px 8px;
  text-align: center;
  font-size: 12px;
  color: var(--muted);
}

/* 主内容区：占满剩余宽度，内部纵向排列 顶栏/消息/输入 */
.chat-content {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
}

/* 跳转高亮：滚动定位后目标消息行停留呼吸高亮再缓慢渐隐（比一闪而过更有定位感） */
.messages-area :deep(.turn-flash) {
  border-radius: var(--radius-sm);
  animation: turn-flash 2.5s ease;
}
@keyframes turn-flash {
  0% { background: rgba(0, 212, 255, 0.22); box-shadow: 0 0 0 1px rgba(0, 212, 255, 0.35); }
  30% { background: rgba(0, 212, 255, 0.16); box-shadow: 0 0 0 1px rgba(0, 212, 255, 0.25); }
  100% { background: transparent; box-shadow: 0 0 0 1px transparent; }
}

/* 窄屏隐藏轮次导航，主内容占满 */
@media (max-width: 768px) {
  .turn-sidebar { display: none; }
}

/* ── 顶部栏 ── */
.chat-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 20px;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
  min-height: 48px;
  gap: 12px;
  background: linear-gradient(180deg, rgba(6, 6, 10, 0.8) 0%, transparent 100%);
}

.header-left {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}

.back-btn {
  font-size: 12px;
}

.agent-label {
  font-size: 12px;
  font-weight: 700;
  padding: 3px 12px;
  border-radius: 20px;
  background: linear-gradient(135deg, rgba(0, 212, 255, 0.15) 0%, rgba(14, 165, 233, 0.1) 100%);
  color: var(--accent);
  border: 1px solid rgba(0, 212, 255, 0.2);
  box-shadow: 0 0 10px rgba(0, 212, 255, 0.08);
  display: inline-flex;
  align-items: center;
  gap: 4px;
  max-width: 240px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.agent-label.clickable {
  cursor: pointer;
  transition: all 0.2s ease;
}
.agent-label.clickable:hover {
  background: linear-gradient(135deg, rgba(0, 212, 255, 0.25) 0%, rgba(14, 165, 233, 0.18) 100%);
  border-color: rgba(0, 212, 255, 0.4);
  box-shadow: 0 0 14px rgba(0, 212, 255, 0.15);
}
.agent-caret {
  font-size: 10px;
  opacity: 0.7;
  flex-shrink: 0;
}

.header-center {
  flex: 1;
  text-align: center;
}

.session-hint {
  font-size: 13px;
  color: var(--text);
  font-weight: 500;
}

.session-hint.muted {
  color: var(--muted);
}

/* 可点击重命名的会话标题：hover 高亮 + 展示编辑小图标提示可改 */
.session-hint.clickable {
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 5px;
  max-width: 100%;
  padding: 3px 12px;
  border-radius: 8px;
  border: 1px solid transparent;
  transition: all 0.2s ease;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.session-hint.clickable:hover {
  background: rgba(0, 212, 255, 0.08);
  border-color: rgba(0, 212, 255, 0.25);
  color: var(--accent);
}
.title-edit-icon {
  flex-shrink: 0;
  font-size: 13px;
  opacity: 0.45;
  transition: opacity 0.2s ease;
}
.session-hint.clickable:hover .title-edit-icon {
  opacity: 0.9;
}

.header-right {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-shrink: 0;
}

/* ── 模型不可用警告横幅 ── */
.model-warning-banner {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 16px;
  background: rgba(245, 158, 11, 0.14);
  border-bottom: 1px solid rgba(245, 158, 11, 0.35);
  color: #fcd34d;
  font-size: 13px;
  flex-shrink: 0;
}
.model-warning-banner .warning-icon {
  font-size: 16px;
}
.model-warning-banner .warning-text {
  line-height: 1.4;
  color: #fde68a;
}

/* 当前选中模型不可用时，把下拉框关闭状态的显示文字灰掉 */
.current-model-unavailable :deep(.el-select__selected-item),
.current-model-unavailable :deep(.el-select__placeholder) {
  color: var(--el-text-color-disabled, #a8abb2) !important;
  opacity: 0.6;
}

/* ── 消息区域 ── */
.scheduled-readonly-banner {
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 0 auto 12px;
  max-width: 760px;
  padding: 8px 14px;
  font-size: 13px;
  color: var(--el-color-warning);
  background: var(--el-color-warning-light-9, rgba(230, 162, 60, 0.08));
  border: 1px solid var(--el-color-warning-light-7, rgba(230, 162, 60, 0.3));
  border-radius: 8px;
}
.messages-area {
  flex: 1;
  overflow-y: auto;
  padding: 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

/* 空状态 */
.empty-state {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  color: var(--muted);
}

.empty-icon {
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--accent);
  opacity: 0.4;
  filter: drop-shadow(0 0 20px rgba(0, 212, 255, 0.15));
}

.empty-title {
  font-size: 20px;
  font-weight: 700;
  color: var(--text-h);
  letter-spacing: -0.3px;
}

.empty-desc {
  font-size: 14px;
  color: var(--muted);
}

/* 消息行 */
.message-row {
  display: flex;
  flex-direction: column;
  max-width: 100%;
  animation: fadeIn 0.3s ease;
  /* 轮次导航 jumpToTurn 用 block:'start' 定位时，留出顶栏+呼吸间距，
     避免目标消息贴到滚动容器上沿被视觉裁切 */
  scroll-margin-top: 12px;
}

.message-row.user {
  align-items: flex-end;
}

.message-row.assistant {
  align-items: flex-start;
}

/* 用户消息 / 思考过程样式已下沉到 UserBubble.vue / ThinkingCard.vue */

/* 工具调用卡片样式已下沉到 ToolCallCard.vue;.tool-code 用全局样式(global.css) */

/* ── 工具确认弹窗 ── */
.confirm-body p {
  font-size: 14px;
  color: var(--text);
  margin-bottom: 12px;
}

.confirm-tool-info {
  background: #0a0a12;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 14px;
}

.confirm-tool-name {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 14px;
  margin-bottom: 8px;
  color: var(--text);
  font-weight: 600;
}

.confirm-tool-name .icon {
  display: flex;
  align-items: center;
  color: var(--accent);
}

/* ── 知识萃取对话框 ── */
.learn-meta-bar {
  display: flex;
  gap: 12px;
  margin-bottom: 16px;
}

.learn-meta-item {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.learn-meta-item label {
  font-size: 12px;
  color: var(--muted);
  font-weight: 600;
}

.learn-loading-overlay {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  padding: 48px 20px;
  color: var(--muted);
  font-size: 14px;
}

.learn-loading-overlay .el-icon {
  color: var(--accent);
}

.learn-empty {
  text-align: center;
  padding: 24px;
  color: var(--muted);
}

.learn-start-section {
  text-align: center;
  padding: 32px 16px;
}

.learn-hint {
  color: var(--muted);
  font-size: 13px;
  margin: 0;
}

.learn-select-bar {
  display: flex;
  align-items: center;
  padding: 10px 14px;
  background: rgba(0, 212, 255, 0.05);
  border-radius: var(--radius-sm);
  margin-bottom: 8px;
  border: 1px solid var(--border);
}

.learn-list {
  display: flex;
  flex-direction: column;
  gap: 12px;
  max-height: 420px;
  overflow-y: auto;
}

.learn-item {
  background: #0a0a12;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 14px;
  transition: all 0.2s;
}

.learn-item.is-selected {
  border-color: var(--accent);
  background: rgba(0, 212, 255, 0.04);
  box-shadow: 0 0 16px rgba(0, 212, 255, 0.06);
}

.learn-item.is-duplicate {
  border-color: #d97706;
  background: rgba(217, 119, 6, 0.04);
}

.learn-item.is-duplicate.is-selected {
  border-color: #d97706;
  background: rgba(217, 119, 6, 0.06);
}

.learn-item-header .dup-tag {
  margin-right: auto;
  margin-left: 8px;
}

.learn-item-header {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 4px;
  margin-bottom: 10px;
}

.learn-item-header .el-checkbox {
  margin-right: auto;
}

.learn-q,
.learn-a {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin-bottom: 8px;
}

.learn-q strong,
.learn-a strong {
  flex-shrink: 0;
  font-size: 13px;
  color: var(--accent);
  margin-top: 6px;
  font-weight: 700;
}

.learn-q .el-textarea,
.learn-a .el-textarea {
  flex: 1;
}

/* ── 输入区域 ── */
.input-area {
  flex-shrink: 0;
  border-top: 1px solid var(--border);
  padding: 14px 20px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  background: linear-gradient(0deg, rgba(6, 6, 10, 0.95) 0%, rgba(6, 6, 10, 0.8) 100%);
  backdrop-filter: blur(12px);
}

/* 待发送图片预览条 */
.pending-images {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.pending-img-chip {
  position: relative;
  width: 64px;
  height: 64px;
  border-radius: 8px;
  overflow: hidden;
  border: 1px solid var(--border);
}
.pending-img-chip img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.pending-img-remove {
  position: absolute;
  top: 2px;
  right: 2px;
  width: 18px;
  height: 18px;
  border-radius: 50%;
  background: rgba(0, 0, 0, 0.6);
  color: #fff;
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 12px;
}
.pending-img-remove:hover {
  background: var(--danger, #e44);
}

/* 待发送文档胶囊条 */
.pending-docs {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.pending-doc-chip {
  display: flex;
  align-items: center;
  gap: 6px;
  max-width: 240px;
  padding: 4px 8px;
  border-radius: 8px;
  border: 1px solid var(--border);
  background: var(--bg-soft, rgba(0, 0, 0, 0.04));
  font-size: 13px;
}
.pending-doc-icon {
  flex-shrink: 0;
  color: var(--primary, #409eff);
}
.pending-doc-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pending-doc-remove {
  flex-shrink: 0;
  cursor: pointer;
  color: var(--text-secondary, #999);
  font-size: 14px;
}
.pending-doc-remove:hover {
  color: var(--danger, #e44);
}

/* 输入框行（附件按钮 + textarea + 发送按钮） */
.input-row {
  display: flex;
  align-items: flex-end;
  gap: 12px;
}
.input-wrapper {
  flex: 1;
  display: flex;
  align-items: flex-end;
  gap: 8px;
}
.attach-btn {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 36px;
  height: 36px;
  border-radius: 8px;
  cursor: pointer;
  color: var(--text);
  background: var(--card);
  border: 1px solid var(--border);
}
.attach-btn:hover {
  color: var(--accent);
  border-color: var(--accent);
}
.attach-btn .el-icon.uploading {
  animation: spin 1s linear infinite;
}
@keyframes spin { to { transform: rotate(360deg); } }

.input-wrapper :deep(.el-textarea__inner) {
  background: var(--card) !important;
  border: 1px solid var(--border) !important;
  box-shadow: none !important;
  color: var(--text) !important;
  border-radius: var(--radius) !important;
  padding: 10px 14px;
  font-size: 14px;
  line-height: 1.6;
  transition: all 0.2s ease;
}
.input-wrapper :deep(.el-textarea__inner):hover {
  border-color: var(--border-hover) !important;
}
.input-wrapper :deep(.el-textarea__inner):focus {
  border-color: var(--accent) !important;
  box-shadow: 0 0 16px rgba(0, 212, 255, 0.1) !important;
}

.input-wrapper :deep(.el-textarea__inner)::placeholder {
  color: var(--muted);
}

.input-actions {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 8px;
}

/* 会话内思考级别紧凑下拉 */
.thinking-select,
.perm-select {
  width: 120px;
}
.thinking-select :deep(.el-select__prefix),
.perm-select :deep(.el-select__prefix) {
  font-size: 12px;
  color: var(--muted);
  margin-right: 4px;
}

.input-actions .el-button {
  min-width: 72px;
  height: 40px;
  font-weight: 700;
}

/* ── 轮次导航底部：上下文 token 用量 ── */
/* 从输入框下方挪来，复用侧边栏内列表下方的留白。
   收起态(56px)只显图标+紧凑数字，hover 展开显完整「已用/阈值」。 */
.turn-usage {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  padding: 8px 6px;
  border-top: 1px solid var(--border);
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  user-select: none;
}
.turn-usage-icon {
  font-size: 13px;
  opacity: 0.8;
  flex-shrink: 0;
}
.usage-collapsed,
.usage-expanded {
  font-variant-numeric: tabular-nums;
}
/* 收起/展开切换：与 turn-meta 同套机制 */
.usage-expanded { display: none; }
.turn-sidebar:hover .usage-collapsed { display: none; }
.turn-sidebar:hover .usage-expanded { display: inline; }
.turn-usage.warn {
  color: var(--warn);
  font-weight: 600;
}
.turn-usage.warn .turn-usage-icon { opacity: 1; }

/* 打字光标 .typing 使用全局样式(global.css) */

/* ── 危险命令确认弹窗 ── */
.danger-code {
  color: #f56c6c;
  background: rgba(245, 108, 108, 0.08);
  border: 1px solid rgba(245, 108, 108, 0.3);
}
.confirm-hint {
  margin-top: 6px;
  font-size: 12px;
  color: var(--muted);
}

/* 编译诊断 / grep 摘要 / 折叠目录样式已下沉到 ToolCallCard.vue */

</style>
