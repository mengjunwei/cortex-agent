/**
 * API 接入层
 *
 * ## 架构
 *
 * 业务接口统一走单一 GraphQL 入口 `POST /api/graphql`。例外（仍为 REST）：
 *   - SSE 流式：`runSse`（SSE 无法良好映射到 GraphQL）
 *   - 健康检查：`getMonitorHealth`（`/api/v1/monitor/health`）
 *
 * ## 统一响应信封
 *
 * 后端所有业务返回值（GraphQL resolver 的 JSON 标量值、REST 健康检查）均为：
 *
 *   { "code": 0, "message": "", "data": <payload> }
 *
 * - `code === 0` 成功；非 0 表示错误，每个码对应一类错误
 *   （1xxx 参数 / 2xxx 业务 / 3xxx 数据库 / 4xxx 外部 / 5xxx 系统）。
 * - `message` 成功为空，失败为可展示给人的错误描述。
 * - `data` 承载业务 payload；失败时为 `null`。
 *
 * `gql()` 解包 GraphQL `{ data, errors }` 后，取出根字段值（即上述信封），
 * 再拆成扁平的 `{ data, code, message }` 返回。调用方约定：
 *
 *   const res = await someApi()
 *   if (res.code !== 0) { 报错(res.message); return }
 *   // 使用 res.data.<payload 字段>
 */

// ── GraphQL 通用辅助 ──────────────────────────

/**
 * 发起一次 GraphQL 请求，返回扁平的 `{ data, code, message }`。
 *
 * 约定：每个 query/mutation 只选取「一个」根字段；该根字段的值即统一信封
 * `{ code, message, data }`。本函数拆信封后返回扁平结构，便于调用方判断。
 *
 * 注意：业务层错误（`code !== 0`）不会抛异常，由调用方检查 `code` 决定处理。
 * 仅 GraphQL 协议层错误（HTTP 非 2xx、`body.errors`）才会 throw。
 *
 * @param {string} query GraphQL 查询字符串（含 query/mutation 关键字）
 * @param {object} [variables] 变量字典
 * @returns {Promise<{ data: any, code: number, message: string }>}
 */
async function gql(query, variables) {
  const resp = await fetch('/api/graphql', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ query, variables }),
  })
  if (!resp.ok) {
    throw new Error(`GraphQL HTTP ${resp.status}`)
  }
  const body = await resp.json()
  if (body.errors && body.errors.length) {
    const msg = body.errors.map((e) => e.message).join('; ')
    throw new Error(msg)
  }
  const rootField = body.data && Object.keys(body.data)[0]
  const env = rootField ? body.data[rootField] : null
  if (!env) {
    return { data: null, code: 0, message: '' }
  }
  return {
    data: env.data === undefined ? null : env.data,
    code: typeof env.code === 'number' ? env.code : 0,
    message: typeof env.message === 'string' ? env.message : '',
  }
}

// ── 通用 ─────────────────────────────────────
export const fetchModels = () => gql('{ models }')
export const fetchCatalog = () => gql('{ catalog }')

// ── Session ──────────────────────────────────
export const fetchSessions = (page = 1, pageSize = 20, { keyword = '', kind = null, assistantId = null } = {}) =>
  gql(
    `query($page: Int, $pageSize: Int, $keyword: String, $kind: Int, $assistantId: String) {
      sessions(page: $page, pageSize: $pageSize, keyword: $keyword, kind: $kind, assistantId: $assistantId)
    }`,
    {
      page,
      pageSize,
      keyword: keyword || null,
      kind,
      assistantId: assistantId || null,
    },
  )
export const createSession = (data) =>
  gql(
    `mutation($input: JSON!) { createSession(input: $input) }`,
    { input: data },
  )
export const deleteSession = (id) =>
  gql(
    `mutation($id: String!) { deleteSession(id: $id) }`,
    { id },
  )
export const fetchHistory = (id) =>
  gql(
    `query($id: String!) { sessionHistory(id: $id) }`,
    { id },
  )
export const renameSession = (id, title) =>
  gql(
    `mutation($id: String!, $title: String!) { renameSession(id: $id, title: $title) }`,
    { id, title },
  )
export const updateSessionModel = (id, modelId) =>
  gql(
    `mutation($id: String!, $modelId: String) { updateSessionModel(id: $id, modelId: $modelId) }`,
    { id, modelId: modelId || null },
  )
// 会话级思考级别（与会话当前模型的协议绑定：anthropic 6 档 / openai 3 档，默认 high）
export const fetchSessionThinkingLevel = (id) =>
  gql(
    `query($id: String!) { sessionThinkingLevel(id: $id) }`,
    { id },
  )
export const updateSessionThinkingLevel = (id, level) =>
  gql(
    `mutation($id: String!, $level: String!) { updateSessionThinkingLevel(id: $id, level: $level) }`,
    { id, level },
  )
// 会话级审批方式（沙箱模式 + 审批策略；未设置 → 全局 [shell] 默认）
export const fetchSessionPermissionPolicy = (id) =>
  gql(
    `query($id: String!) { sessionPermissionPolicy(id: $id) }`,
    { id },
  )
export const updateSessionPermissionPolicy = (id, sandboxMode, approvalPolicy) =>
  gql(
    `mutation($id: String!, $sandboxMode: String!, $approvalPolicy: String!) { updateSessionPermissionPolicy(id: $id, sandboxMode: $sandboxMode, approvalPolicy: $approvalPolicy) }`,
    { id, sandboxMode, approvalPolicy },
  )

// ── Chat / SSE ───────────────────────────────
// SSE 流式接口保留为 REST（无法映射到 GraphQL）
export const runSse = (body, signal) =>
  fetch('/api/run_sse', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal,
  })

// 上传图片附件（multipart），返回 { url(data url), filename, mime_type, size }
export const uploadImage = async (file) => {
  const fd = new FormData()
  fd.append('file', file)
  const resp = await fetch('/api/uploads', { method: 'POST', body: fd })
  return resp.json()
}
export const cancelRun = (threadId) =>
  gql(
    `mutation($threadId: String!) { cancelRun(threadId: $threadId) }`,
    { threadId },
  )

// ── 自定义助手（REST，统一信封 { code, message, data }） ─────
// ── 助手（GraphQL） ──────────────────────────

export const fetchAssistants = () =>
  gql(`query { assistants }`)

export const fetchAssistant = (id) =>
  gql(
    `query($id: String!) { assistant(id: $id) }`,
    { id },
  )

export const createAssistant = (payload) =>
  gql(
    `mutation($input: JSON!) { createAssistant(input: $input) }`,
    { input: payload },
  )

export const generateAssistantDraft = (prompt, modelId) =>
  gql(
    `mutation($input: JSON!) { generateAssistant(input: $input) }`,
    { input: { prompt, model_id: modelId || null } },
  )

export const updateAssistant = (id, payload) =>
  gql(
    `mutation($id: String!, $input: JSON!) { updateAssistant(id: $id, input: $input) }`,
    { id, input: payload },
  )

export const deleteAssistant = (id, force = false) =>
  gql(
    `mutation($id: String!, $force: Boolean) { deleteAssistant(id: $id, force: $force) }`,
    { id, force },
  )

export const duplicateAssistant = (id) =>
  gql(
    `mutation($id: String!) { duplicateAssistant(id: $id) }`,
    { id },
  )

export const fetchExploreAssistants = () =>
  gql(`query { exploreAssistants }`)

export const fetchAssistantByToken = (token) =>
  gql(
    `query($token: String!) { assistantByToken(token: $token) }`,
    { token },
  )

export const enableShare = (id) =>
  gql(
    `mutation($id: String!) { shareAssistant(id: $id) }`,
    { id },
  )

export const disableShare = (id) =>
  gql(
    `mutation($id: String!) { unshareAssistant(id: $id) }`,
    { id },
  )

export const forkAssistant = (id) =>
  gql(
    `mutation($id: String!) { forkAssistant(id: $id) }`,
    { id },
  )

export const exportAssistant = (id) =>
  gql(
    `mutation($id: String!) { exportAssistant(id: $id) }`,
    { id },
  )

export const importAssistant = (payload) =>
  gql(
    `mutation($input: JSON!) { importAssistant(input: $input) }`,
    { input: payload },
  )

// 绑定/解绑助手的知识库实例（内置助手配置知识库用；kbInstanceId 传空串=解绑）
export const bindAssistantKbInstance = (assistantId, kbInstanceId) =>
  gql(
    `mutation($assistantId: String!, $kbInstanceId: String) {
      bindAssistantKbInstance(assistantId: $assistantId, kbInstanceId: $kbInstanceId)
    }`,
    { assistantId, kbInstanceId: kbInstanceId || '' },
  )

export const fetchTools = () =>
  gql(`query { tools }`)

// ── Knowledge（FAQ 学习：从会话萃取问答；实例/文档管理见下方 Instances 段）────
export const learnFromSession = (data) =>
  gql(
    `mutation($input: JSON!) { kbLearn(input: $input) }`,
    { input: data },
  )
export const regenerateLearn = (data) =>
  gql(
    `mutation($input: JSON!) { kbLearnRegenerate(input: $input) }`,
    { input: data },
  )
export const commitLearn = (data) =>
  gql(
    `mutation($input: JSON!) { kbLearnCommit(input: $input) }`,
    { input: data },
  )

// ── Knowledge Instances（多 provider 知识库实例：Dify 外挂 + 内置）──────
export const fetchKbInstances = () =>
  gql(`query { kbInstances }`)

export const fetchKbProviderSchema = () =>
  gql(`query { kbProviderSchema }`)

export const createKbInstance = (data) =>
  gql(
    `mutation($input: JSON!) { kbInstanceCreate(input: $input) }`,
    { input: data },
  )

export const updateKbInstance = (data) =>
  gql(
    `mutation($input: JSON!) { kbInstanceUpdate(input: $input) }`,
    { input: data },
  )

export const deleteKbInstance = (id, force = false) =>
  gql(
    `mutation($id: String!, $force: Boolean) { kbInstanceDelete(id: $id, force: $force) }`,
    { id, force },
  )

export const testKbInstance = (id) =>
  gql(
    `mutation($id: String!) { kbInstanceTest(id: $id) }`,
    { id },
  )

// 某实例的文档操作（路由到对应 provider：Dify 调 API，内置走 Qdrant）
export const fetchInstanceDocuments = (instanceId, page = 1, pageSize = 20, { keyword = '' } = {}) =>
  gql(
    `query($input: JSON!) { kbInstanceDocuments(input: $input) }`,
    {
      input: {
        instance_id: instanceId,
        page,
        page_size: pageSize,
        keyword: keyword || null,
      },
    },
  )

export const fetchInstanceSegments = (instanceId, docId) =>
  gql(
    `query($instanceId: String!, $docId: String!) { kbInstanceSegments(instanceId: $instanceId, docId: $docId) }`,
    { instanceId, docId },
  )

export const deleteInstanceDocument = (instanceId, docId) =>
  gql(
    `mutation($instanceId: String!, $docId: String!) { kbInstanceDeleteDocument(instanceId: $instanceId, docId: $docId) }`,
    { instanceId, docId },
  )

export const uploadInstanceDocument = (instanceId, data) =>
  gql(
    `mutation($input: JSON!) { kbInstanceUpload(input: $input) }`,
    { input: { ...data, instance_id: instanceId } },
  )

// ── Device ───────────────────────────────────
export const searchDevice = (query) =>
  gql(
    `query($input: JSON!) { deviceSearch(input: $input) }`,
    { input: { query } },
  )

// ── Monitor ──────────────────────────────────
export const fetchPlugins = () => gql('{ monitorPlugins }')
export const getPlugins = fetchPlugins
export const registerPlugin = (data) =>
  gql(
    `mutation($input: JSON!) { registerMonitorPlugin(input: $input) }`,
    { input: data },
  )
export const unregisterPlugin = (data) =>
  gql(
    `mutation($pluginId: String!) { unregisterMonitorPlugin(pluginId: $pluginId) }`,
    { pluginId: data.plugin_id },
  )
export const getMonitorOids = (pluginId) =>
  gql(
    `query($pluginId: String!) { monitorOids(pluginId: $pluginId) }`,
    { pluginId },
  )
export const calculateMonitor = (data) =>
  gql(
    `query($pluginId: String!, $oidValues: JSON!) {
      monitorCalculate(pluginId: $pluginId, oidValues: $oidValues)
    }`,
    { pluginId: data.plugin_id, oidValues: data.oid_values },
  )
// 健康检查接口保留为 REST（返回值同样遵循统一信封 { code, message, data }）
export const getMonitorHealth = () =>
  fetch('/api/v1/monitor/health')
    .then((r) => r.json())
    .then((env) => ({
      data: env && env.data !== undefined ? env.data : null,
      code: env && typeof env.code === 'number' ? env.code : 0,
      message: env && typeof env.message === 'string' ? env.message : '',
    }))
export const fetchPluginInfo = (pluginId) =>
  gql(
    `query($pluginId: String!) { monitorPlugin(pluginId: $pluginId) }`,
    { pluginId },
  )
export const fetchPluginVersions = (pluginId) =>
  gql(
    `query($pluginId: String!) { monitorPluginVersions(pluginId: $pluginId) }`,
    { pluginId },
  )
export const rollbackPlugin = (pluginId, version) =>
  gql(
    `mutation($pluginId: String!, $version: Int!) {
      rollbackMonitorPlugin(pluginId: $pluginId, version: $version)
    }`,
    { pluginId, version },
  )

// ── Model Provider（模型供应商/模型 管理） ───
// 说明：status 字段为数字枚举（0=禁用，1=启用），前后端统一数字传输。
//       API Key 仅在「新建/重置」时上行明文，列表接口只返回 key_suffix（末 4 位）。
export const fetchModelProviders = () => gql('{ modelProviders }')
export const createModelProvider = (data) =>
  gql(
    `mutation($input: JSON!) { createModelProvider(input: $input) }`,
    { input: data },
  )
export const updateModelProvider = (id, data) =>
  gql(
    `mutation($id: String!, $input: JSON!) { updateModelProvider(id: $id, input: $input) }`,
    { id, input: data },
  )
export const deleteModelProvider = (id, force = false) =>
  gql(
    `mutation($id: String!, $force: Boolean) { deleteModelProvider(id: $id, force: $force) }`,
    { id, force },
  )
export const resetModelProviderKey = (id, apiKey) =>
  gql(
    `mutation($id: String!, $input: JSON!) { resetModelProviderKey(id: $id, input: $input) }`,
    { id, input: { api_key: apiKey } },
  )
export const createModel = (providerId, data) =>
  gql(
    `mutation($providerId: String!, $input: JSON!) {
      createModel(providerId: $providerId, input: $input)
    }`,
    { providerId, input: data },
  )
export const updateModel = (id, data) =>
  gql(
    `mutation($id: String!, $input: JSON!) { updateModel(id: $id, input: $input) }`,
    { id, input: data },
  )
export const deleteModel = (id, force = false) =>
  gql(
    `mutation($id: String!, $force: Boolean) { deleteModel(id: $id, force: $force) }`,
    { id, force },
  )
export const setDefaultModel = (id) =>
  gql(
    `mutation($id: String!) { setDefaultModel(id: $id) }`,
    { id },
  )
export const setEmbeddingDefaultModel = (id) =>
  gql(
    `mutation($id: String!) { setEmbeddingDefaultModel(id: $id) }`,
    { id },
  )
// 批量探测模型可用性（返回 data.results：每项含 model_id/status/latency_ms/probe_kind/error/probed_at）
export const probeModels = (ids) =>
  gql(`mutation($input: JSON!) { probeModels(input: $input) }`, { input: { ids } })

// ── Auth（SSO 单点登录） ──────────────────────
// OAuth 回调跳转流程无法映射到 GraphQL，保留为 REST。
// 后端响应同样遵循统一信封 { code, message, data }。
async function unwrapEnv(resp) {
  const env = await resp.json()
  return {
    data: env && env.data !== undefined ? env.data : null,
    code: env && typeof env.code === 'number' ? env.code : 0,
    message: env && typeof env.message === 'string' ? env.message : '',
  }
}

/** 获取已配置的身份提供商列表（登录页渲染按钮用） */
export const fetchAuthProviders = () =>
  fetch('/api/auth/providers').then(unwrapEnv)

/** 获取当前登录用户（未登录返回 authenticated=false） */
export const fetchMe = () =>
  fetch('/api/auth/me').then(unwrapEnv)

/** 登出（清除会话 Cookie + Redis 黑名单） */
export const authLogout = () =>
  fetch('/api/auth/logout', { method: 'POST' }).then(unwrapEnv)

/** 本地账号注册（用户名密码，首用户自动管理员） */
export const authRegister = (username, password, name) =>
  fetch('/api/auth/register', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password, name }),
  }).then(unwrapEnv)

/** 本地账号登录（用户名密码） */
export const authLoginLocal = (username, password) =>
  fetch('/api/auth/login/local', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password }),
  }).then(unwrapEnv)

// ── 账户 API Token（访问令牌） ───────────────
// 外部系统以 Authorization: Bearer <令牌> 调接口，等价登录身份。
// 明文令牌仅在「新建」时由 POST 返回一次（res.data.token），列表只给脱敏前缀 prefix。
export const fetchApiTokens = () => fetch('/api/auth/tokens').then(unwrapEnv)
export const createApiToken = (data) =>
  fetch('/api/auth/tokens', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  }).then(unwrapEnv)
export const updateApiToken = (id, data) =>
  fetch(`/api/auth/tokens/${id}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  }).then(unwrapEnv)
export const deleteApiToken = (id) =>
  fetch(`/api/auth/tokens/${id}`, { method: 'DELETE' }).then(unwrapEnv)

/** shell_command 审批回传（用户允许/拒绝命令执行） */
export const approveShellCommand = (approvalId, approved) =>
  fetch('/api/shell-approve', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      approval_id: approvalId,
      decision: approved ? 'approved' : 'rejected',
    }),
  }).then(unwrapEnv)

// ── Shell 权限规则 ──────────────────────────────────
export const fetchShellRules = () =>
  gql(`query { shellRules }`, {})
export const createShellRule = (pattern, decision, priority = 0) =>
  gql(`mutation($input: JSON!) { createShellRule(input: $input) }`, {
    input: { pattern, decision, priority },
  })
export const deleteShellRule = (id) =>
  gql(`mutation($id: String!) { deleteShellRule(id: $id) }`, { id })

// ── 记忆（跨会话记忆管理） ────────────────────
// type: 0=习惯/偏好 1=坑；scope: 0=所有助手(用户级) 1=仅当前助手(助手级)
export const fetchMemories = () => gql('{ memories }')
export const fetchMemoryProposals = () => gql('{ memoryProposals }')
export const createMemory = (data) =>
  gql(`mutation($input: JSON!) { createMemory(input: $input) }`, { input: data })
export const updateMemory = (id, data) =>
  gql(
    `mutation($id: String!, $input: JSON!) { updateMemory(id: $id, input: $input) }`,
    { id, input: data },
  )
export const deleteMemory = (id) =>
  gql(`mutation($id: String!) { deleteMemory(id: $id) }`, { id })
export const acceptMemoryProposal = (id) =>
  gql(`mutation($id: String!) { acceptMemoryProposal(id: $id) }`, { id })
export const rejectMemoryProposal = (id) =>
  gql(`mutation($id: String!) { rejectMemoryProposal(id: $id) }`, { id })

// ── MCP Server（MCP 服务管理） ───────────────
// 说明：transport 字段为数字枚举（1=stdio，2=streamable_http），前后端统一数字传输。
//       env/headers 的明文仅在「新建/编辑」时上行；列表接口只返回脱敏值（value 形如 ****abcd）。
//       health 为运行时探测状态，tag=state：unknown / healthy / degraded / unhealthy。
export const fetchMcpServers = (page = 1, pageSize = 10, keyword = '') =>
  gql(
    `query($page: Int, $pageSize: Int, $keyword: String) { mcpServers(page: $page, pageSize: $pageSize, keyword: $keyword) }`,
    { page, pageSize, keyword: keyword || null },
  )
export const fetchMcpServer = (id) =>
  gql(
    `query($id: String!) { mcpServer(id: $id) }`,
    { id },
  )
export const fetchMcpTools = (serverIds) =>
  gql(
    `query($input: JSON!) { mcpTools(input: $input) }`,
    { input: { server_ids: serverIds } },
  )
export const createMcpServer = (data) =>
  gql(
    `mutation($input: JSON!) { createMcpServer(input: $input) }`,
    { input: data },
  )
export const updateMcpServer = (id, data) =>
  gql(
    `mutation($id: String!, $input: JSON!) { updateMcpServer(id: $id, input: $input) }`,
    { id, input: data },
  )
export const deleteMcpServer = (id, force = false) =>
  gql(
    `mutation($id: String!, $force: Boolean) { deleteMcpServer(id: $id, force: $force) }`,
    { id, force },
  )
export const probeMcpServer = (id) =>
  gql(
    `mutation($id: String!) { probeMcpServer(id: $id) }`,
    { id },
  )
export const batchSetMcpStatus = (input) =>
  gql(
    `mutation($input: JSON!) { batchSetMcpStatus(input: $input) }`,
    { input },
  )
export const batchDeleteMcpServers = (input) =>
  gql(
    `mutation($input: JSON!) { batchDeleteMcpServers(input: $input) }`,
    { input },
  )
export const batchProbeMcpServers = (ids) =>
  gql(
    `mutation($input: JSON!) { batchProbeMcpServers(input: $input) }`,
    { input: { ids } },
  )

// ── Skill（文件系统 Skill 目录管理） ──────────────────
export const fetchSkills = () => gql(`query { skills }`)
export const reloadSkills = () => gql(`mutation { reloadSkills }`)
// 安装 Skill（REST，沙箱只读需后端代写）。统一信封 { code, message, data }。
// install：从工作区绝对路径安装；upload：上传 tar.gz 安装。
export const installSkill = (path, overwrite = false) =>
  fetch('/api/skills/install', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ path, overwrite }),
  }).then(unwrapEnv)
export const uploadSkill = (file, overwrite = false) => {
  const fd = new FormData()
  fd.append('file', file)
  if (overwrite) fd.append('overwrite', 'true')
  return fetch('/api/skills/upload', { method: 'POST', body: fd }).then(unwrapEnv)
}
