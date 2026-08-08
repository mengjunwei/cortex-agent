import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { fetchSessions, createSession, deleteSession, fetchHistory, renameSession } from '../api'
import { uuidv7 } from '../utils/uuid'

export const useSessionStore = defineStore('session', () => {
  const sessions = ref([])
  const currentSessionId = ref(null)
  const currentAgentType = ref(null)
  const currentAssistantId = ref(null)
  const currentAssistantName = ref(null)
  const currentAssistantKind = ref(null)
  const currentPage = ref(1)
  const pageSize = ref(20)
  const totalCount = ref(0)
  const totalPages = ref(0)
  const filterKeyword = ref('')
  const filterKind = ref(null)
  const messages = ref([])
  const pendingConfirmation = ref(null)
  // 历史消息加载中标记：进入/切换会话拉取 fetchHistory 期间为 true，
  // ChatPage 据此渲染消息骨架屏，避免空白或误显「开始对话」空状态。
  const historyLoading = ref(false)

  const currentSession = computed(() =>
    sessions.value.find(s => s.id === currentSessionId.value),
  )

  async function loadSessions(page = 1, { keyword = filterKeyword.value, kind = filterKind.value } = {}) {
    filterKeyword.value = keyword
    filterKind.value = kind
    try {
      const { data, code } = await fetchSessions(page, 20, { keyword, kind })
      if (code !== 0) return
      sessions.value = data.sessions || []
      totalCount.value = data.total || 0
      totalPages.value = data.total_pages || 0
      currentPage.value = data.page || page
    } catch (_) {}
  }

  async function createNewSession(type, title, assistantId) {
    const id = uuidv7()
    // 新建会话时携带当前选中的模型（若有），让后端立即持久化绑定
    let initModelId = 'default'
    try {
      const { useChatStore } = await import('./chat')
      const chatStore = useChatStore()
      initModelId = chatStore.currentModelId || 'default'
    } catch (_) {}
    const payload = {
      session_id: id,
      agent_type: type,
      model_id: initModelId,
      title: title || `新会话 ${new Date().toLocaleString('zh-CN', { year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric' })}`,
    }
    // 携带助手绑定（内置/自定义助手 ID），让后端持久化到 session state
    if (assistantId) payload.assistant_id = assistantId
    const { data, code } = await createSession(payload)
    // 后端成功返回 { id, title, agent_type, assistant_id, created_at, welcome_message }
    if (code === 0 && data.id) {
      currentSessionId.value = data.id || id
      currentAgentType.value = type
      currentAssistantId.value = assistantId || data.assistant_id || null
      messages.value = []
      await loadSessions(1)
      return data.id || id
    }
    return null
  }

  async function selectSession(id, agentType) {
    currentSessionId.value = id
    currentAgentType.value = agentType
    // 兜底：目标会话不在 sessions 列表（如直接开 URL/刷新/新标签页，列表尚未加载）时，
    // 先拉一次列表，保证 currentSession（标题、agent_type 等）能正常解析。
    // 已存在则跳过，不产生额外请求。
    if (id && !sessions.value.some(s => s.id === id)) {
      await loadSessions(1)
    }
    // loadHistoryMessages 内部会设置 currentAssistantId（从后端 session state 恢复）
    // 返回后端绑定的 model_id（null 表示未绑定）
    return await loadHistoryMessages(id)
  }

  // 返回后端传来的 model_id（供调用方设置 chatStore.currentModelId）
  async function loadHistoryMessages(id) {
    historyLoading.value = true
    try {
      const { data, code } = await fetchHistory(id)
      if (code !== 0) {
        messages.value = []
        pendingConfirmation.value = null
        currentAssistantId.value = null
        currentAssistantName.value = null
        currentAssistantKind.value = null
        return null
      }
      messages.value = data.messages || []
      pendingConfirmation.value = data.pending_confirmation || null
      // 从后端 session state 恢复助手绑定
      currentAssistantId.value = data.assistant_id || null
      currentAssistantName.value = data.assistant_name || null
      currentAssistantKind.value = data.assistant_kind ?? null
      // 同步恢复 agent_type（供 currentAgentType 使用；调用方无需再单独 fetchHistory 推断）
      if (data.agent_type) currentAgentType.value = data.agent_type
      return data.model_id || null
    } catch (_) {
      messages.value = []
      pendingConfirmation.value = null
      currentAssistantId.value = null
      currentAssistantName.value = null
      currentAssistantKind.value = null
      return null
    } finally {
      historyLoading.value = false
    }
  }

  /** 切换当前会话绑定的助手（ChatPage 选择助手后调用） */
  function bindAssistant(id) {
    currentAssistantId.value = id || null
    currentAssistantName.value = null
    currentAssistantKind.value = null
  }

  async function deleteSessionById(id) {
    try {
      await deleteSession(id)
      if (currentSessionId.value === id) {
        currentSessionId.value = null
        currentAgentType.value = null
        currentAssistantId.value = null
        currentAssistantName.value = null
        currentAssistantKind.value = null
        messages.value = []
      }
      await loadSessions(currentPage.value)
    } catch (_) {}
  }

  async function renameSessionById(id, title) {
    try {
      await renameSession(id, title)
      await loadSessions(currentPage.value)
    } catch (_) {}
  }

  return {
    sessions, currentSessionId, currentAgentType, currentAssistantId,
    currentAssistantName, currentAssistantKind,
    currentPage, pageSize, totalPages,
    totalCount, filterKeyword, filterKind, messages, currentSession, pendingConfirmation,
    historyLoading,
    loadSessions, createNewSession, selectSession,
    loadHistoryMessages, deleteSessionById, renameSessionById, bindAssistant,
  }
})
