import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ElMessage } from 'element-plus'
import { runSse, cancelRun, steerRun, updateSessionModel, updateSessionThinkingLevel, fetchSessionThinkingLevel, fetchSessionPermissionPolicy, updateSessionPermissionPolicy, approveShellCommand } from '../api'
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
  // 会话来源：0=普通会话 / 1=定时任务会话（只读回放，禁发消息/改审批）。schedule_task_id 供返回定位。
  const sessionSourceType = ref(0)
  const sessionScheduleTaskId = ref(null)
  // 定时任务会话标题（不在普通会话列表，列表取不到 → 由 permission_policy 接口兜底带回）
  const scheduledSessionTitle = ref(null)
  const streamingText = ref('')
  const thinkingText = ref('')
  const pendingToolConfirm = ref(null)
  // run_command 危险命令确认队列（工具结果层 require_confirmation）
  // 支持多个待确认命令，不互相覆盖；元素结构：
  //   { toolName, command, confirmToken, error }
  const pendingToolResultConfirm = ref([])
  // shell_command 审批请求队列（SHELL_APPROVAL_REQUEST 事件）：后端注册表按 approval_id
  // 支持并发审批，单槽会互相覆盖丢请求；元素结构：
  //   { approval_id, command, session_id }
  const pendingShellApprovals = ref([])
  // 当前活跃 SSE 流归属的会话（body.thread_id）：同一时刻只有一条流，但用户可能已
  // 切到别的会话查看（本流在后台收尾）。流式气泡/停止按钮/确认弹窗等"当前页面"状态
  // 必须以它与会话归属判定，否则后台流的内容会渲染到正在查看的会话页面上。
  const streamSessionId = ref(null)
  const prefillValue = ref('')
  // steer 竞态兜底队列（极小概率命中）：本地 isStreaming 仍为 true 但服务端 run 已结束
  //（结束帧在路上），steerRun 返回 steered:false → 暂存于此，流收尾后按新轮补发。
  // 元素结构：{ text, attachments, sessionId }；纯内存数组，无 UI（气泡已在 steer 时渲染）
  const steerRetryQueue = []
  // 上下文 token 用量（后端 CONTEXT_USAGE 事件，每轮 LLM 响应后推送）
  // 结构：{ prompt_tokens, completion_tokens, total_tokens, threshold }；null=暂无数据
  const contextUsage = ref(null)
  // token 用量单调闸门：按 sessionId 记录已展示过的最大 total_tokens。
  // 后端 usage_metadata（末帧真实值）与 budget 估算（中间帧）不在同一量纲，跨轮切换会让
  // 显示值回退；这里只接受 >= floor 的值，保证会话内「只增不减」。CONTEXT_COMPACTED 时清零，
  // 使压缩后更小的真实值能被接受；按 sessionId 隔离，切会话天然互不影响。
  const contextUsageFloor = ref({})

  let abortCtrl = null
  // 当前活跃 SSE 流的回调集（ChatPage 传入的闭包）。存放在 store 层而非固定为 doSse 的
  // 参数：ChatPage 重挂载后可通过 reattachSseCallbacks 整体替换——否则流循环一直引用已
  // 销毁组件的 messages 数组，重进流式会话的页面收不到任何后续事件（流"冻住"）
  let liveCallbacks = null

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
  // 返回 { from, to }（模型发生实际切换时；未切换/失败返回 null），
  // 供调用方在会话时间线插入「模型已切换 A → B」分隔条（持久化在后端 session 事件，重进会话可恢复）
  async function saveModelForSession(sessionId, modelId) {
    currentModelId.value = modelId
    if (!sessionId) return null
    try {
      const { data } = await updateSessionModel(sessionId, modelId)
      if (data && data.from && data.to && data.from !== data.to) {
        return { from: data.from, to: data.to }
      }
    } catch (e) {
      console.warn('[chat] 保存会话模型绑定失败', e)
    }
    return null
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
      sessionSourceType.value = 0
      sessionScheduleTaskId.value = null
      scheduledSessionTitle.value = null
      return
    }
    try {
      const { data, code } = await fetchSessionPermissionPolicy(sessionId)
      if (code === 0 && data && data.sandbox_mode && data.approval_policy) {
        sessionPermissionPolicy.value = { sandbox_mode: data.sandbox_mode, approval_policy: data.approval_policy }
        sessionSourceType.value = data.source_type ?? 0
        sessionScheduleTaskId.value = data.schedule_task_id ?? null
        scheduledSessionTitle.value = data.title ?? null
      } else {
        sessionPermissionPolicy.value = fallback
        sessionSourceType.value = 0
        sessionScheduleTaskId.value = null
        scheduledSessionTitle.value = null
      }
    } catch (_) {
      sessionPermissionPolicy.value = fallback
      sessionSourceType.value = 0
      sessionScheduleTaskId.value = null
      scheduledSessionTitle.value = null
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

  // 统一构造后端 InputMessage 载荷（run_sse 与 steer 共用同一结构）
  function buildMessagePayload(text, attachments) {
    return {
      id: uuidv7(),
      role: 'user',
      content: text,
      attachments: attachments.map(a => ({ url: a.url, mime_type: a.mime_type, filename: a.filename })),
    }
  }

  async function sendMessage(text, appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent, attachments = [], { skipAppend = false } = {}) {
    const sess = useSessionStore()
    const trimmed = text.trim()
    if (!trimmed && attachments.length === 0) return

    liveCallbacks = { appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent }

    // 运行中：codex 式 steer —— 注入当前 run 的服务端待处理输入，下轮模型请求前生效
    //（不中断当前任务；服务端队列由后端 run_registry 持有，刷新页面也不丢）
    if (isStreaming.value) {
      const sessionId = sess.currentSessionId
      appendMsg('user', text, attachments)
      steerRun(sessionId, [buildMessagePayload(trimmed, attachments)], currentRunId.value)
        .then(({ code, data }) => {
          if (code !== 0 || !data || data.steered === false) {
            // 竞态：服务端 run 已结束（结束帧还在路上）→ 暂存，流收尾后按新轮补发
            steerRetryQueue.push({ text: trimmed, attachments, sessionId })
            // 本响应可能在流收尾（drainSteerRetry 已跑过）之后才到达：流已空闲时这里
            // 补一次触发，否则消息会滞留队列直到下一次 run 结束才被补发
            if (!isStreaming.value) drainSteerRetry()
          }
        })
        .catch((e) => {
          console.warn('[chat] steer 注入失败，转为结束后补发', e)
          steerRetryQueue.push({ text: trimmed, attachments, sessionId })
          if (!isStreaming.value) drainSteerRetry()
        })
      return
    }

    if (!skipAppend) appendMsg('user', text, attachments)
    isStreaming.value = true
    currentRunId.value = uuidv7()
    streamingText.value = ''
    thinkingText.value = ''

    const body = {
      thread_id: sess.currentSessionId,
      run_id: currentRunId.value,
      assistant_id: sess.currentAssistantId,
      model_id: currentModelId.value,
      messages: [buildMessagePayload(text, attachments)],
    }

    await doSse(body)
  }

  async function sendToolDecision(decisions, appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent) {
    const sess = useSessionStore()
    if (isStreaming.value) return

    isStreaming.value = true
    liveCallbacks = { appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent }
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

    await doSse(body)
  }

  // 重进流式会话时由 ChatPage 调用：把回调集换成新组件实例的闭包（旧组件已销毁，其
  // messages 数组再也渲染不了）。仅在流仍在跑时替换；流已结束则没有可重挂的对象
  function reattachSseCallbacks(appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent) {
    if (!isStreaming.value) return
    liveCallbacks = { appendMsg, appendThinking, appendTool, updateToolResult, onDone, updateToolArgs, upsertChildAgent }
  }

  async function doSse(body) {
    try {
      abortCtrl = new AbortController()
      // 记录本条流归属的会话：切走会话后流在后台收尾期间，页面据此区分
      // 「自己会话的流」与「后台别的会话的流」（渲染/停止/确认弹窗均按此判定）
      streamSessionId.value = body.thread_id || null
      const resp = await runSse(body, abortCtrl.signal)
      // 启动失败（422 参数缺失 / 401 / 5xx）：SSE 端点直接回 JSON 错误体，不含 data: 帧。
      // 旧版不检查 resp.ok，会把错误体当流读完然后"安静结束"——用户发消息毫无反应也无提示
      if (!resp.ok) {
        let detail = ''
        try { detail = (await resp.text()).slice(0, 200) } catch (_) {}
        throw new Error(`消息发送失败（HTTP ${resp.status}）${detail ? `：${detail}` : ''}`)
      }
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
              handleEvent(evt, body.thread_id)
            } catch (_) {}
          }
        }
      }
    } catch (e) {
      if (e.name !== 'AbortError') {
        console.error('SSE error:', e)
        // 流启动/中途异常必须可见：全局提示 + 正在查看本流会话时补一条错误气泡
        //（本次失败后端没落历史，不补气泡则毫无痕迹）
        ElMessage.error(e.message || '消息发送失败')
        if (streamSessionId.value && streamSessionId.value === useSessionStore().currentSessionId) {
          liveCallbacks?.appendMsg?.('assistant', `[错误] ${e.message || '运行失败'}`)
        }
      }
    } finally {
      const endedSessionId = streamSessionId.value
      isStreaming.value = false
      currentRunId.value = null
      streamSessionId.value = null
      abortCtrl = null
      // run 结束（RUN_FINISHED / abort / 异常断流，abort 路径未必有结束帧）：清掉本流
      // 会话的待决 shell 审批——后端注册表随 run 结束注销，残留条目点「允许」只会 NOT_FOUND
      pendingShellApprovals.value = pendingShellApprovals.value.filter(
        (a) => !a.session_id || a.session_id !== endedSessionId,
      )
      liveCallbacks?.onDone?.(endedSessionId)
      // steer 竞态兜底：run 结束后补发未注入成功的消息（skipAppend，气泡已在 steer 时渲染）
      drainSteerRetry()
    }
  }

  // 补发 steer 竞态中未入队的消息（isStreaming 已复位，走正常发送路径）
  function drainSteerRetry() {
    if (steerRetryQueue.length === 0 || !liveCallbacks) return
    const sess = useSessionStore()
    // 有未处理的审批弹窗时不自动发送，避免打断审批流程。只看当前会话的待决项：
    // 审批队列会保留切走会话的条目（供重进恢复），别会话的残留不该卡住本会话的补发
    const blocking =
      pendingToolConfirm.value ||
      pendingShellApprovals.value.some((a) => !a.session_id || a.session_id === sess.currentSessionId) ||
      pendingToolResultConfirm.value.length > 0
    if (blocking) return
    // 会话切换保护：丢弃已切走的旧会话消息，不带到新会话
    for (let i = steerRetryQueue.length - 1; i >= 0; i--) {
      if (steerRetryQueue[i].sessionId !== sess.currentSessionId) steerRetryQueue.splice(i, 1)
    }
    if (steerRetryQueue.length === 0) return
    const next = steerRetryQueue.shift()
    const cb = liveCallbacks
    sendMessage(
      next.text,
      cb.appendMsg, cb.appendThinking, cb.appendTool, cb.updateToolResult, cb.onDone, cb.updateToolArgs, cb.upsertChildAgent,
      next.attachments,
      { skipAppend: true },
    )
  }

  // streamSessionId：本条 SSE 流归属的会话（body.thread_id）。会话级事件（CONTEXT_USAGE /
  // CONTEXT_COMPACTED）必须按它记账——切走会话后后台仍在收尾的 run 推来的事件，若按
  // 「当前查看会话」记账会把别的会话用量写到当前会话的 floor 与显示上（进入会话详情
  // 窗口用量"没重置"的根因）。
  // 回调统一从 liveCallbacks 读取（事件时刻取值，支持重进会话后 reattach 换新闭包）；
  // 可见内容的追加/更新再叠加「正在查看本流会话」门禁：切走后的事件不渲染到别的会话
  // 页面（内容后端已持久化，重进会话从历史恢复 + reattach 后续实时事件）。
  function handleEvent(evt, streamSessionId) {
    const sess = useSessionStore()
    const viewingOwnStream = streamSessionId === sess.currentSessionId
    const cb = liveCallbacks
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
        if (viewingOwnStream) cb?.appendTool?.(evt.tool_call_name || 'tool', 'running', evt.tool_call_id, evt.server_name)
        break
      case 'TOOL_CALL_ARGS':
        if (viewingOwnStream) cb?.updateToolArgs?.(evt.delta || '', evt.tool_call_id)
        break
      case 'TOOL_CALL_END':
        break
      case 'TOOL_CALL_RESULT': {
        if (viewingOwnStream) cb?.updateToolResult?.(evt.content || '', evt.tool_call_id)
        // 检测 run_command 返回的危险命令确认信号（工具结果层 require_confirmation）
        // 入队（不覆盖），支持多个待确认命令
        const toolName = evt.tool_name || evt.tool_call_name || ''
        const confirm = getToolResultConfirmation(toolName, evt.content)
        if (confirm) {
          // 仅在查看本流所属会话时弹确认并中断流：确认弹窗是全局的，且批准后的
          // 补发消息会发进当前查看的会话，跨会话弹框会把 A 的命令决策发到 B。
          // 切走时命令本身并未执行（后端只回了 require_confirmation 错误），
          // 让模型在后台自行基于该错误调整即可，重进会话可从历史卡片看到原因。
          if (viewingOwnStream) {
            // 盖会话戳：切换会话时 clearToolResultConfirm 据此过滤（L-4）
            pendingToolResultConfirm.value.push({ ...confirm, session_id: streamSessionId })
            // 中断 SSE 流：阻止 AI 基于未确认的危险命令结果继续生成回复，
            // 必须等用户在弹框中批准/拒绝后才能继续（批准→重新发消息带 token）
            abortCtrl?.abort()
          }
        }
        break
      }
      case 'TOOL_CONFIRMATION':
        // 仅在查看本流所属会话时弹确认框：后端发完该事件即结束 run 并持久化 pending
        // 状态，切走场景重进会话会从历史恢复弹窗；跨会话弹框会把决策发进当前查看的会话
        if (viewingOwnStream) {
          pendingToolConfirm.value = { name: evt.tool_name, args: evt.args }
        }
        break
      case 'SHELL_APPROVAL_REQUEST':
        // 并发审批入队（单槽会互相覆盖丢请求）；同样仅在查看本流所属会话时入队，
        // 切走时审批条无处挂载，留给服务端超时兜底（不在别的会话页面上挂审批条）
        if (viewingOwnStream) {
          pendingShellApprovals.value.push({
            approval_id: evt.approval_id,
            command: evt.command,
            session_id: evt.session_id,
          })
        }
        break
      case 'TEXT_MESSAGE_END':
        if (streamingText.value) {
          if (viewingOwnStream) cb?.appendMsg?.('assistant', streamingText.value)
          streamingText.value = ''
        }
        thinkingText.value = ''
        break
      case 'RUN_STARTED':
        break
      case 'RUN_FINISHED':
        break
      case 'RUN_ERROR':
        if (viewingOwnStream) cb?.appendMsg?.('assistant', `[错误] ${evt.message || '运行失败'}`)
        break
      case 'FILE_ARTIFACT':
        if (viewingOwnStream) {
          cb?.appendMsg?.('artifact', {
            path: evt.path,
            filename: evt.filename,
            title: evt.title,
            mime: evt.mime,
            size: evt.size,
          })
        }
        break
      case 'CONTEXT_COMPACTED':
        // 上下文已自动压缩：显示分隔标记（compaction_count≥2 提示新建会话）
        if (viewingOwnStream) cb?.appendMsg?.('compacted', { compaction_count: evt.compaction_count || 1 })
        // 压缩后上下文真实变小，清零本会话的单调闸门，使后续更小的真实值能被接受
        //（按流自己的会话清，不得清到当前查看会话的闸门上）
        {
          const sid = streamSessionId || sess.currentSessionId
          if (sid) {
            contextUsageFloor.value = { ...contextUsageFloor.value, [sid]: 0 }
          }
        }
        break
      case 'CONTEXT_USAGE': {
        // 上下文 token 用量（每轮 LLM 响应后推送），供 footer 状态栏展示
        // 单调闸：会话内 total_tokens 只增不减，杜绝后端来源切换导致的显示回退
        const sid = streamSessionId || sess.currentSessionId
        const rawTotal = evt.total_tokens || 0
        const floor = (sid && contextUsageFloor.value[sid]) || 0
        const totalTokens = Math.max(rawTotal, floor)
        if (sid) {
          contextUsageFloor.value = { ...contextUsageFloor.value, [sid]: totalTokens }
        }
        // 已切走（查看别的会话）：只记账到流自己的会话，不覆盖当前显示
        if (sid !== sess.currentSessionId) break
        contextUsage.value = {
          prompt_tokens: evt.prompt_tokens || 0,
          completion_tokens: evt.completion_tokens || 0,
          total_tokens: totalTokens,
          // 子 agent（spawn_agent 并行任务）本轮 token 花费：独立展示，不并入 total
          child_tokens: evt.child_tokens || 0,
          threshold: evt.threshold || 0,
          // 模型上下文窗口总量：进度条分母，前端显示「已用 / 窗口总量」
          window_size: evt.window_size || 0,
          // 对齐 codex：上下文剩余百分比（0-100，减 BASELINE_TOKENS 后计算，clamp 不超限）
          context_remaining_percent: evt.context_remaining_percent ?? null,
        }
        break
      }
      case 'CHILD_AGENT_ACTIVITY':
        // 子 agent（spawn_agent）活动：按 task_name 聚合，渲染成「子任务」面板
        if (viewingOwnStream) cb?.upsertChildAgent?.(evt)
        break
      case 'ASYNC_USER_MESSAGE':
        // 模型经 send_user_message_async 工具发的中途消息（进度更新/阻塞提问）：
        // 整条一次性推送（非流式），作为独立 assistant 气泡插入当前时间线——
        // 后端不持久化（刷新即丢，与子 agent 活动一致），仅实时渲染
        if (viewingOwnStream) cb?.appendMsg?.('assistant', evt.message || '')
        break
    }
  }

  async function cancel() {
    const sess = useSessionStore()
    // 定向取消流自己的会话（而非当前查看的会话）：查看 B 时 A 的后台流仍在跑，
    // 旧逻辑按 currentSessionId 取消会误杀/误留
    const sid = streamSessionId.value || sess.currentSessionId
    if (!sid) return
    try { await cancelRun(sid) } catch (_) {}
    abortCtrl?.abort()
    isStreaming.value = false
    // 停止时丢弃未注入成功的兜底消息（服务端 steer 队列由后端 cancel 一并清空）
    steerRetryQueue.length = 0
    // 清除残留的审批状态：停止后弹窗/审批条不应残留（否则用户误点会向后端已取消的 run 重发）
    pendingToolConfirm.value = null
    pendingShellApprovals.value = []
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

  // 切换会话时按归属过滤危险命令确认队列（条目入队时已盖 session_id 戳）：只保留属于
  // 新会话的条目——旧会话的确认弹窗不带进新会话（批准会把命令决策发进当前查看的会话）。
  // shell 审批队列不在此清：它是内嵌卡片渲染且挂载时按 session_id 过滤，不会污染别的
  // 会话页面；保留可让重进会话时把仍待决的审批重新挂回命令卡（清了会导致点击审批条
  // 时按 id 出队找不到条目而静默失效）。工具确认弹窗也不在此清：由 ChatPage 依据各
  // 会话持久化的 pendingConfirmation 同步恢复
  function clearToolResultConfirm() {
    const sid = useSessionStore().currentSessionId
    pendingToolResultConfirm.value = pendingToolResultConfirm.value.filter(
      (i) => i.session_id && i.session_id === sid,
    )
  }

  // shell_command 审批：用户点击允许/拒绝后按 approval_id 定向出队并回传
  //（并发审批互不影响，旧的"取单槽"会在两条审批时把第一条静默丢弃）
  async function resolveShellApproval(approvalId, approved) {
    const idx = pendingShellApprovals.value.findIndex((a) => a.approval_id === approvalId)
    if (idx === -1) return
    const [ap] = pendingShellApprovals.value.splice(idx, 1)
    let failed = false
    try {
      const { code, message } = await approveShellCommand(ap.approval_id, approved)
      // 业务失败（run 已结束/审批超时后端注销了 id 等）：不能静默吞掉——条目已出队，
      // 用户点「允许」后毫无反应。放回原位供重试，并提示原因
      if (code !== 0) {
        failed = true
        ElMessage.error(`审批回传失败：${message || '请重试'}`)
      }
    } catch (e) {
      failed = true
      console.error('[chat] shell 审批回传失败', e)
      ElMessage.error('审批回传失败：' + (e.message || '网络错误'))
    }
    if (failed) {
      pendingShellApprovals.value.splice(Math.min(idx, pendingShellApprovals.value.length), 0, ap)
    }
  }

  // 重进会话：从持久化快照恢复「已用 / 阈值」显示（对齐 codex 会话级 token_info 持久化）。
  // 同步抬高会话内单调 floor，使本轮首条 CONTEXT_USAGE（中间帧 / 重启后累计从低爬起）
  // 不会把显示压回历史峰值之下。total=0（无用量）→ 清空，回退到无数据态。
  function restoreContextUsage(sessionId, total, threshold) {
    if (!sessionId || !total) {
      contextUsage.value = null
      return
    }
    contextUsageFloor.value = { ...contextUsageFloor.value, [sessionId]: total }
    // threshold = 后端软闸 = context_window × 0.95（压缩触发线），按 0.95 反推真实窗口。
    // 旧代码 /0.9 会反推出偏大 ~5.6% 的假窗口，重进会话后剩余百分比虚高。
    const windowSize = threshold ? Math.round(threshold / 0.95) : 0
    // 推导剩余百分比（对齐 codex）：减 BASELINE_TOKENS(12K) 后计算，clamp(0,100)
    let remainingPct = null
    if (windowSize > 12000) {
      const effectiveWindow = windowSize - 12000
      const used = Math.max(total - 12000, 0)
      const remaining = Math.max(effectiveWindow - used, 0)
      remainingPct = Math.round(Math.min(Math.max((remaining / effectiveWindow) * 100, 0), 100))
    }
    contextUsage.value = {
      prompt_tokens: 0,
      completion_tokens: 0,
      total_tokens: total,
      threshold: threshold || 0,
      window_size: windowSize,
      context_remaining_percent: remainingPct,
    }
  }

  return {
    isStreaming, currentRunId, currentModelId, sessionThinkingLevel, sessionPermissionPolicy, streamingText,
    sessionSourceType, sessionScheduleTaskId, scheduledSessionTitle,
    thinkingText, pendingToolConfirm, pendingToolResultConfirm,
    pendingShellApprovals, streamSessionId, prefillValue, contextUsage,
    loadModelForSession, saveModelForSession,
    loadSessionThinkingLevel, saveSessionThinkingLevel,
    loadSessionPermissionPolicy, saveSessionPermissionPolicy,
    sendMessage, sendToolDecision, cancel,
    resolveToolConfirm, resolveToolResultConfirm,
    resolveShellApproval, reattachSseCallbacks,
    restoreContextUsage,
    clearToolResultConfirm, doSse, prefillMessage,
  }
})
