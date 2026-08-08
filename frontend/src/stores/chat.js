import { defineStore } from 'pinia'
import { ref } from 'vue'
import { runSse, cancelRun, updateSessionModel, updateSessionThinkingLevel, fetchSessionThinkingLevel, fetchSessionPermissionPolicy, updateSessionPermissionPolicy, approveShellCommand } from '../api'
import { useSessionStore } from './session'
import { useAppStore } from './app'
import { uuidv7 } from '../utils/uuid'
import { getToolResultConfirmation, buildRunCommandConfirmationPrompt } from '../utils/toolResult'

export const useChatStore = defineStore('chat', () => {
  const isStreaming = ref(false)
  const currentRunId = ref(null)
  const currentModelId = ref('default')
  // 会话级思考级别（与会话当前模型协议绑定；默认 high）
  // 取值：low / medium / high / xhigh / max
  const sessionThinkingLevel = ref('high')
  // 会话级审批方式（沙箱模式 + 审批策略；默认对齐全局 [shell]：workspace-write + unless-trusted）
  // 结构：{ sandbox_mode, approval_policy }
  const sessionPermissionPolicy = ref({ sandbox_mode: 'workspace-write', approval_policy: 'unless-trusted' })
  const streamingText = ref('')
  const thinkingText = ref('')
  const pendingToolConfirm = ref(null)
  // run_command 危险命令确认队列（工具结果层 require_confirmation）
  // 支持多个待确认命令，不互相覆盖；元素结构：
  //   { toolName, command, confirmToken, error }
  const pendingToolResultConfirm = ref([])
  // shell_command 审批请求（SHELL_APPROVAL_REQUEST 事件）
  //   { approval_id, command, session_id }
  const pendingShellApproval = ref(null)
  const prefillValue = ref('')
  // MVP 排队式：运行中继续输入的消息进队列，当前 run 结束后自动发送（FIFO）
  // 元素结构：{ text, attachments }
  const pendingQueue = ref([])
  // 上下文 token 用量（后端 CONTEXT_USAGE 事件，每轮 LLM 响应后推送）
  // 结构：{ prompt_tokens, completion_tokens, total_tokens, threshold }；null=暂无数据
  const contextUsage = ref(null)

  let abortCtrl = null
  // 最近一次 sendMessage 的回调，供队列自动重发复用
  let currentCallbacks = null

  // 获取全局默认模型 id；优先从 appStore 读取，兜底 'default'（后端会自动解析）
  function resolveDefaultModel() {
    try {
      return useAppStore().defaultModelId || 'default'
    } catch (_) {
      return 'default'
    }
  }

  // 从后端获取的会话绑定模型 id 加载到 currentModelId
  // 参数 boundModelId：后端返回的 model_id（null 表示未绑定具体模型）
  function loadModelForSession(boundModelId) {
    const defaultId = resolveDefaultModel()
    // 后端有具体绑定 → 使用绑定值；否则用全局默认
    currentModelId.value = boundModelId || defaultId
  }

  // 切换会话模型：更新前端状态 + 持久化到后端
  async function saveModelForSession(sessionId, modelId) {
    currentModelId.value = modelId
    if (!sessionId) return
    try {
      await updateSessionModel(sessionId, modelId)
    } catch (e) {
      console.warn('[chat] 保存会话模型绑定失败', e)
    }
  }

  // 加载会话思考级别：从后端读取并写入 state；未设/无会话时回落 high
  async function loadSessionThinkingLevel(sessionId) {
    if (!sessionId) {
      sessionThinkingLevel.value = 'high'
      return
    }
    try {
      const { data, code } = await fetchSessionThinkingLevel(sessionId)
      if (code === 0 && data && data.thinking_level) {
        sessionThinkingLevel.value = data.thinking_level
      } else {
        sessionThinkingLevel.value = 'high'
      }
    } catch (_) {
      sessionThinkingLevel.value = 'high'
    }
  }

  // 保存会话思考级别：乐观更新 state + 持久化到后端
  async function saveSessionThinkingLevel(sessionId, level) {
    sessionThinkingLevel.value = level
    if (!sessionId) return
    try {
      await updateSessionThinkingLevel(sessionId, level)
    } catch (e) {
      console.warn('[chat] 保存会话思考级别失败', e)
    }
  }

  // 加载会话审批方式：从后端读取（未设/无会话 → 全局默认 workspace-write + unless-trusted）
  async function loadSessionPermissionPolicy(sessionId) {
    const fallback = { sandbox_mode: 'workspace-write', approval_policy: 'unless-trusted' }
    if (!sessionId) {
      sessionPermissionPolicy.value = fallback
      return
    }
    try {
      const { data, code } = await fetchSessionPermissionPolicy(sessionId)
      if (code === 0 && data && data.sandbox_mode && data.approval_policy) {
        sessionPermissionPolicy.value = { sandbox_mode: data.sandbox_mode, approval_policy: data.approval_policy }
      } else {
        sessionPermissionPolicy.value = fallback
      }
    } catch (_) {
      sessionPermissionPolicy.value = fallback
    }
  }

  // 保存会话审批方式（沙箱模式 + 审批策略任一变化均整体持久化）
  async function saveSessionPermissionPolicy(sessionId, sandboxMode, approvalPolicy) {
    sessionPermissionPolicy.value = { sandbox_mode: sandboxMode, approval_policy: approvalPolicy }
    if (!sessionId) return
    try {
      await updateSessionPermissionPolicy(sessionId, sandboxMode, approvalPolicy)
    } catch (e) {
      console.warn('[chat] 保存会话审批方式失败', e)
    }
  }

  function prefillMessage(text) {
    prefillValue.value = text
  }

  async function sendMessage(text, appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent, attachments = []) {
    const sess = useSessionStore()
    const trimmed = text.trim()
    if (!trimmed && attachments.length === 0) return

    currentCallbacks = { appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent }

    // 运行中：入队，等当前 run 结束后自动发送（MVP 排队式，不中断当前任务）
    if (isStreaming.value) {
      pendingQueue.value.push({ text: trimmed, attachments, sessionId: sess.currentSessionId })
      return
    }

    appendMsg('user', text, attachments)
    isStreaming.value = true
    currentRunId.value = uuidv7()
    streamingText.value = ''
    thinkingText.value = ''

    const body = {
      thread_id: sess.currentSessionId,
      run_id: currentRunId.value,
      assistant_id: sess.currentAssistantId,
      model_id: currentModelId.value,
      messages: [{
        id: uuidv7(),
        role: 'user',
        content: text,
        attachments: attachments.map(a => ({ url: a.url, mime_type: a.mime_type })),
      }],
    }

    await doSse(body, appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs)
  }

  async function sendToolDecision(decisions, appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent) {
    const sess = useSessionStore()
    if (isStreaming.value) return

    isStreaming.value = true
    currentCallbacks = { appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent }
    currentRunId.value = uuidv7()
    streamingText.value = ''
    thinkingText.value = ''

    const body = {
      thread_id: sess.currentSessionId,
      run_id: currentRunId.value,
      assistant_id: sess.currentAssistantId,
      model_id: currentModelId.value,
      messages: [],
      tool_decisions: decisions,
    }

    await doSse(body, appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs)
  }

  async function doSse(body, appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs) {
    try {
      abortCtrl = new AbortController()
      const resp = await runSse(body, abortCtrl.signal)
      const reader = resp.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''

      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        buffer += decoder.decode(value, { stream: true })
        const lines = buffer.split('\n')
        buffer = lines.pop()

        for (const line of lines) {
          if (line.startsWith('data: ')) {
            try {
              const evt = JSON.parse(line.slice(6))
              handleEvent(evt, appendMsg, appendThinking, appendTool, updateToolResult, updateToolArgs)
            } catch (_) {}
          }
        }
      }
    } catch (e) {
      if (e.name !== 'AbortError') console.error('SSE error:', e)
    } finally {
      isStreaming.value = false
      currentRunId.value = null
      abortCtrl = null
      onDone?.()
      // MVP 排队式：当前 run 结束后自动发送队列下一条
      drainPendingQueue()
    }
  }

  // 取出队列首条并用上次的回调重新发送（isStreaming 已复位，走立即发路径）
  function drainPendingQueue() {
    if (pendingQueue.value.length === 0 || !currentCallbacks) return
    // 有未处理的审批弹窗时不自动发送，避免打断审批流程（审批回调的 sendMessage 会再触发 drain）
    if (pendingToolConfirm.value || pendingShellApproval.value || pendingToolResultConfirm.value.length > 0) return
    const sess = useSessionStore()
    const next = pendingQueue.value.shift()
    // 会话切换保护：旧会话的排队消息不带到新会话
    if (next.sessionId !== sess.currentSessionId) return
    const cb = currentCallbacks
    sendMessage(
      next.text,
      cb.appendMsg, cb.appendThinking, cb.appendTool, cb.updateToolResult, cb.onDone, cb.updateToolArgs, cb.upsertChildAgent,
      next.attachments,
    )
  }

  function removeQueued(idx) {
    pendingQueue.value.splice(idx, 1)
  }

  function clearQueued() {
    pendingQueue.value = []
  }

  function handleEvent(evt, appendMsg, appendThinking, appendTool, updateToolResult, updateToolArgs) {
    switch (evt.type) {
      case 'TEXT_MESSAGE_START':
        break
      case 'TEXT_MESSAGE_CONTENT':
        streamingText.value += evt.delta || ''
        break
      case 'THINKING_MESSAGE_START':
        break
      case 'THINKING_MESSAGE_CONTENT':
        thinkingText.value += evt.delta || ''
        break
      case 'THINKING_MESSAGE_END':
        break
      case 'TOOL_CALL_START':
        appendTool?.(evt.tool_call_name || 'tool', 'running', evt.tool_call_id, evt.server_name)
        break
      case 'TOOL_CALL_ARGS':
        updateToolArgs?.(evt.delta || '', evt.tool_call_id)
        break
      case 'TOOL_CALL_END':
        break
      case 'TOOL_CALL_RESULT': {
        updateToolResult?.(evt.content || '', evt.tool_call_id)
        // 检测 run_command 返回的危险命令确认信号（工具结果层 require_confirmation）
        // 入队（不覆盖），支持多个待确认命令
        const toolName = evt.tool_name || evt.tool_call_name || ''
        const confirm = getToolResultConfirmation(toolName, evt.content)
        if (confirm) {
          pendingToolResultConfirm.value.push(confirm)
          // 中断 SSE 流：阻止 AI 基于未确认的危险命令结果继续生成回复，
          // 必须等用户在弹框中批准/拒绝后才能继续（批准→重新发消息带 token）
          abortCtrl?.abort()
        }
        break
      }
      case 'TOOL_CONFIRMATION':
        pendingToolConfirm.value = { name: evt.tool_name, args: evt.args }
        break
      case 'SHELL_APPROVAL_REQUEST':
        pendingShellApproval.value = {
          approval_id: evt.approval_id,
          command: evt.command,
          session_id: evt.session_id,
        }
        break
      case 'TEXT_MESSAGE_END':
        if (streamingText.value) {
          appendMsg?.('assistant', streamingText.value)
          streamingText.value = ''
        }
        thinkingText.value = ''
        break
      case 'RUN_STARTED':
        break
      case 'RUN_FINISHED':
        break
      case 'RUN_ERROR':
        appendMsg?.('assistant', `[错误] ${evt.message || '运行失败'}`)
        break
      case 'FILE_ARTIFACT':
        appendMsg?.('artifact', {
          path: evt.path,
          filename: evt.filename,
          title: evt.title,
          mime: evt.mime,
          size: evt.size,
        })
        break
      case 'CONTEXT_COMPACTED':
        // 上下文已自动压缩：显示分隔标记（compaction_count≥2 提示新建会话）
        appendMsg?.('compacted', { compaction_count: evt.compaction_count || 1 })
        break
      case 'CONTEXT_USAGE':
        // 上下文 token 用量（每轮 LLM 响应后推送），供 footer 状态栏展示
        contextUsage.value = {
          prompt_tokens: evt.prompt_tokens || 0,
          completion_tokens: evt.completion_tokens || 0,
          total_tokens: evt.total_tokens || 0,
          threshold: evt.threshold || 0,
        }
        break
      case 'CHILD_AGENT_ACTIVITY':
        // 子 agent（spawn_agent）活动：按 task_name 聚合，渲染成「子任务」面板
        currentCallbacks?.upsertChildAgent?.(evt)
        break
    }
  }

  async function cancel() {
    const sess = useSessionStore()
    if (!sess.currentSessionId) return
    try { await cancelRun(sess.currentSessionId) } catch (_) {}
    abortCtrl?.abort()
    isStreaming.value = false
    clearQueued()  // 停止时清空排队消息
    // 清除残留的审批状态：停止后弹窗/审批条不应残留（否则用户误点会向后端已取消的 run 重发）
    pendingToolConfirm.value = null
    pendingShellApproval.value = null
    pendingToolResultConfirm.value = []
  }

  function resolveToolConfirm(approved) {
    const confirm = pendingToolConfirm.value
    pendingToolConfirm.value = null
    return { name: confirm?.name, approved }
  }

  // run_command 危险命令确认（队列）：
  //   批准时从队列头部取出一条，返回引导模型带 token 重发的提示文案
  //   拒绝时移除队列头部一条，不重发命令
  function resolveToolResultConfirm(approved) {
    const confirm = pendingToolResultConfirm.value.shift() || null
    if (!approved || !confirm) return { approved: false, prompt: '' }
    return {
      approved: true,
      prompt: buildRunCommandConfirmationPrompt({
        command: confirm.command,
        confirmToken: confirm.confirmToken,
      }),
    }
  }

  // 切换会话时清空所有待确认状态，避免跨会话污染
  function clearToolResultConfirm() {
    pendingToolResultConfirm.value = []
    pendingShellApproval.value = null
  }

  // shell_command 审批：用户点击允许/拒绝后调用
  async function resolveShellApproval(approved) {
    const ap = pendingShellApproval.value
    pendingShellApproval.value = null
    if (!ap) return
    try {
      await approveShellCommand(ap.approval_id, approved)
    } catch (e) {
      console.error('[chat] shell 审批回传失败', e)
    }
  }

  return {
    isStreaming, currentRunId, currentModelId, sessionThinkingLevel, sessionPermissionPolicy, streamingText,
    thinkingText, pendingToolConfirm, pendingToolResultConfirm,
    pendingShellApproval, prefillValue, pendingQueue, contextUsage,
    loadModelForSession, saveModelForSession,
    loadSessionThinkingLevel, saveSessionThinkingLevel,
    loadSessionPermissionPolicy, saveSessionPermissionPolicy,
    sendMessage, sendToolDecision, cancel,
    resolveToolConfirm, resolveToolResultConfirm,
    resolveShellApproval,
    clearToolResultConfirm, doSse, prefillMessage,
    removeQueued, clearQueued,
  }
})
