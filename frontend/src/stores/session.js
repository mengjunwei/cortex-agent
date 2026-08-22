import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { fetchSessions, createSession, deleteSession, fetchHistory, renameSession } from '../api'
import { uuidv7 } from '../utils/uuid'

export const useSessionStore = defineStore('session', () => {
  const sessions = ref([])
  const currentSessionId = ref(null)
  // 标题兜底缓存：get_session_history 不返回 title，会话详情页只能依赖 sessions 列表里的
  // currentSession.title；直接进 /chat（列表未加载/会话不在当前页）时读不到。create/rename
  // 回包后把标题缓存到这里，displayTitle 优先取列表、其次取本缓存，保证详情页标题始终能显能改。
  const currentSessionTitle = ref(null)
  const currentAgentType = ref(null)
  const currentAssistantId = ref(null)
  const currentAssistantName = ref(null)
  const currentAssistantKind = ref(null)
  const currentPage = ref(1)
  const pageSize = ref(20)
  const totalCount = ref(0)
  const totalPages = ref(0)
  const filterKeyword = ref('')
  // 已提交的搜索词：filterKeyword 是输入框草稿（v-model 逐字符写入），翻页/刷新
  // 必须用已提交值——否则输入未回车的关键词会经分页静默生效，列表与总数突变
  const appliedKeyword = ref('')
  const filterKind = ref(null)
  const messages = ref([])
  const pendingConfirmation = ref(null)
  // 历史消息加载中标记：进入/切换会话拉取 fetchHistory 期间为 true，
  // ChatPage 据此渲染消息骨架屏，避免空白或误显「开始对话」空状态。
  const historyLoading = ref(false)

  const currentSession = computed(() =>
    sessions.value.find(s => s.id === currentSessionId.value),
  )

  // 详情页展示标题：优先取列表 currentSession.title（权威、随重命名刷新），
  // 列表缺失时回落到 currentSessionTitle 缓存（create/rename 回包写入）。
  const displayTitle = computed(() => {
    const s = currentSession.value
    return (s && s.title) || currentSessionTitle.value || ''
  })

  // 请求序号：快速翻页/切筛选时先发的请求可能后到，旧页数据会覆盖新页
  let loadSessionsSeq = 0

  async function loadSessions(page = 1, { keyword = appliedKeyword.value, kind = filterKind.value } = {}) {
    // 只归一化已提交值，不回写 filterKeyword 草稿：翻页时把输入框内容改回已提交值
    // 会打断正在输入的关键词
    appliedKeyword.value = keyword
    filterKind.value = kind
    const seq = ++loadSessionsSeq
    try {
      const { data, code } = await fetchSessions(page, pageSize.value, { keyword, kind })
      if (seq !== loadSessionsSeq) return // 已被更新的请求取代，丢弃过期回包
      if (code !== 0) return
      sessions.value = data.sessions || []
      totalCount.value = data.total || 0
      totalPages.value = data.total_pages || 0
      currentPage.value = data.page || page
      // 页码越界收敛：删除末页最后一条后重载原页会拿到空列表（后端只 clamp 下限
      // 不回退页码），自动退回最后一页，避免详情列表显示空白
      const cur = currentPage.value
      if (sessions.value.length === 0 && cur > 1 && (totalPages.value || 0) >= 1 && cur > totalPages.value) {
        await loadSessions(Math.max(totalPages.value, 1), { keyword, kind })
      }
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
    const { data, code, message } = await createSession(payload)
    // 后端成功返回 { id, title, agent_type, assistant_id, created_at, welcome_message }
    if (code === 0 && data.id) {
      currentSessionId.value = data.id || id
      // 缓存标题：新建后立即跳详情页时列表尚未刷新，详情页头部可凭此显示正确名称
      currentSessionTitle.value = data.title || payload.title || null
      currentAgentType.value = type
      currentAssistantId.value = assistantId || data.assistant_id || null
      messages.value = []
      // 清上一个会话的残留：pendingConfirmation 不清的话，新会话挂载时 ChatPage 的
      // immediate watch 会把旧会话的待确认工具弹成幽灵弹窗（批准还会把决策发进新会话）
      pendingConfirmation.value = null
      currentAssistantName.value = null
      currentAssistantKind.value = null
      // 清窗口用量显示：新建会话 ChatPage 走 restoreSessionFromUrl 早退分支（id 已同步、
      // 不重放 loadHistoryMessages），不清的话 footer 会一直显示上一个会话的用量
      try {
        const { useChatStore } = await import('./chat')
        useChatStore().restoreContextUsage(data.id || id, 0, 0)
      } catch (_) {}
      await loadSessions(1)
      return data.id || id
    }
    // 业务失败向上抛（网络异常本就会 throw）：调用方必须能感知失败并提示，
    // 旧逻辑静默返回 null 会让「新建会话」点了没反应也无任何报错
    throw new Error(message || '创建会话失败')
  }

  async function selectSession(id, agentType) {
    currentSessionId.value = id
    // 切换会话：清掉上一个会话残留的标题缓存，避免详情页短暂显示旧名称（列表刷新后由 currentSession 兜底）
    currentSessionTitle.value = null
    currentAgentType.value = agentType
    // 立即清窗口用量显示，拉取历史期间不残留上一会话的「XX% context left」
    //（loadHistoryMessages 回包后按持久化快照重填）
    try {
      const { useChatStore } = await import('./chat')
      useChatStore().restoreContextUsage(id, 0, 0)
    } catch (_) {}
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

  // 请求序号：快速连点两个会话时两次 fetchHistory 并发，慢的旧回包后到会把
  // messages/助手绑定/用量整组覆盖成旧会话数据（而 currentSessionId 已是新会话）
  let loadHistorySeq = 0

  // 返回后端传来的 model_id（供调用方设置 chatStore.currentModelId）
  async function loadHistoryMessages(id) {
    const seq = ++loadHistorySeq
    historyLoading.value = true
    // 动态引入避免与 chat store 循环依赖；用于恢复/清空 token 用量显示
    const { useChatStore } = await import('./chat')
    const chatStore = useChatStore()
    try {
      const { data, code } = await fetchHistory(id)
      if (seq !== loadHistorySeq) return null // 已被更新的会话切换取代，丢弃过期回包
      if (code !== 0) {
        messages.value = []
        pendingConfirmation.value = null
        currentAssistantId.value = null
        currentAssistantName.value = null
        currentAssistantKind.value = null
        chatStore.restoreContextUsage(id, 0, 0)
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
      // 恢复会话级 token 用量（持久化快照）：重进会话立即显示「已用 / 阈值」，无需等新消息。
      // 同步抬高单调 floor，避免本轮首条中间帧把显示压回历史峰值之下（对齐 codex token_info）。
      const tu = data.token_usage || {}
      chatStore.restoreContextUsage(id, tu.total_tokens || 0, tu.threshold || 0)
      return data.model_id || null
    } catch (_) {
      if (seq !== loadHistorySeq) return null
      messages.value = []
      pendingConfirmation.value = null
      currentAssistantId.value = null
      currentAssistantName.value = null
      currentAssistantKind.value = null
      chatStore.restoreContextUsage(id, 0, 0)
      return null
    } finally {
      if (seq === loadHistorySeq) historyLoading.value = false
    }
  }

  /** 切换当前会话绑定的助手（ChatPage 选择助手后调用） */
  function bindAssistant(id) {
    currentAssistantId.value = id || null
    currentAssistantName.value = null
    currentAssistantKind.value = null
  }

  /** 删除会话：业务失败/传输异常向上抛（旧版静默吞掉会让行不动且无任何提示） */
  async function deleteSessionById(id) {
    const { code, message } = await deleteSession(id)
    if (code !== 0) throw new Error(message || '删除失败')
    if (currentSessionId.value === id) {
      currentSessionId.value = null
      currentSessionTitle.value = null
      currentAgentType.value = null
      currentAssistantId.value = null
      currentAssistantName.value = null
      currentAssistantKind.value = null
      messages.value = []
    }
    await loadSessions(currentPage.value)
  }

  /** 重命名会话：失败抛错（成功才做乐观更新，失败时把标题改成本地值是数据错乱） */
  async function renameSessionById(id, title) {
    const { data, code, message } = await renameSession(id, title)
    if (code !== 0) throw new Error(message || '重命名失败')
    const newTitle = (data && data.title) || title
    // 缓存 + 内存列表即时更新：详情页/列表页无需等 loadSessions 回包即可看到新名称
    if (currentSessionId.value === id) currentSessionTitle.value = newTitle
    const s = sessions.value.find(x => x.id === id)
    if (s) s.title = newTitle
    await loadSessions(currentPage.value)
  }

  return {
    sessions, currentSessionId, currentSessionTitle, displayTitle, currentAgentType, currentAssistantId,
    currentAssistantName, currentAssistantKind,
    currentPage, pageSize, totalPages,
    totalCount, filterKeyword, appliedKeyword, filterKind, messages, currentSession, pendingConfirmation,
    historyLoading,
    loadSessions, createNewSession, selectSession,
    loadHistoryMessages, deleteSessionById, renameSessionById, bindAssistant,
  }
})
