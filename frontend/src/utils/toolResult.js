/**
 * 工具结果解析工具集
 *
 * 把后端工具返回的结构化字段（require_confirmation / diagnostics 等）
 * 解析成前端可直接渲染的形态，保持 ChatPage 组件轻量。
 */

/**
 * 解析任意来源（字符串 JSON 或对象）为对象。
 * @param {string|object|null|undefined} v
 * @returns {object|null}
 */
export function parseAny(v) {
  if (v === null || v === undefined || v === '') return null
  if (typeof v === 'string') {
    try {
      const parsed = JSON.parse(v)
      // 双编码兜底：工具结果有时被多层 JSON.stringify 包成字符串（如
      // '"{"output":...}"'），解一层后仍是字符串。若该字符串本身又是合法 JSON，
      // 再解一层，避免下游对字符串用 `in` / 取属性时出错或拿不到真实结构。
      if (typeof parsed === 'string' && parsed.trim().startsWith('{')) {
        try {
          return JSON.parse(parsed)
        } catch {
          return parsed
        }
      }
      return parsed
    } catch {
      return null
    }
  }
  if (typeof v === 'object') return v
  return null
}

/**
 * 解析文件写入类工具（edit_file / create_file）结果里的 unified diff。
 *
 * 匹配后端返回结构：{ ok: true, diff: "--- a/x\n+++ b/x\n@@ ... @@\n-old\n+new" }
 *
 * @param {string|object|null} result 工具结果
 * @returns {{lines: Array<{type:'add'|'del'|'ctx'|'hunk'|'meta', text:string}>}|null}
 *   有 diff 时返回按行分类的渲染结构；否则 null
 */
export function getDiffResult(result) {
  const obj = parseAny(result)
  if (!obj || typeof obj !== 'object' || obj.ok !== true) return null
  if (typeof obj.diff !== 'string' || !obj.diff.trim()) return null
  // meta（--- a/x / +++ b/x）只出现在第一个 @@ hunk 头之前；hunk 体内以 --- / +++
  // 开头的行是被增删的内容（如删除一行 SQL 注释 '-- note' 生成 '--- note'），
  // 必须按首字符分类为 del/add，不能按前缀猜 meta。按行定位第一个 @@ 的行索引。
  const allLines = obj.diff.split('\n')
  const firstHunkLine = allLines.findIndex((l) => l.startsWith('@@'))
  // 无任何 hunk（如 append 空内容 old==new，diff 只有 ---/+++ 头两行）：没有可渲染的
  // 正文，返回 null 走通用 JSON 展示（bytes/lines/appended 摘要），避免空 diff 卡
  if (firstHunkLine === -1) return null
  const metaZone = firstHunkLine
  const lines = []
  allLines.forEach((line, i) => {
    if (i < metaZone && (line.startsWith('+++') || line.startsWith('---'))) {
      lines.push({ type: 'meta', text: line })
    } else if (line.startsWith('@@')) {
      lines.push({ type: 'hunk', text: line })
    } else if (line.startsWith('+')) {
      lines.push({ type: 'add', text: line.slice(1) })
    } else if (line.startsWith('-')) {
      lines.push({ type: 'del', text: line.slice(1) })
    } else {
      lines.push({ type: 'ctx', text: line.replace(/^ /, '') })
    }
  })
  return { lines }
}

/**
 * 检测 run_command 工具结果是否为「需要用户确认」的危险命令。
 *
 * 匹配后端 run_command 返回结构：
 *   { ok: false, require_confirmation: true, confirm_token, command, error }
 *
 * @param {string} toolName 工具名
 * @param {string|object|null} result 工具结果（字符串或对象）
 * @returns {{toolName:string, command:string, confirmToken:string, error:string}|null}
 *   需要确认时返回结构化对象；否则返回 null
 */
export function getToolResultConfirmation(toolName, result) {
  if (!toolName || toolName !== 'run_command') return null
  const obj = parseAny(result)
  if (!obj || obj.require_confirmation !== true) return null
  return {
    toolName,
    command: String(obj.command || ''),
    confirmToken: String(obj.confirm_token || ''),
    error: String(obj.error || ''),
  }
}

/**
 * 构造「用户已批准危险命令」的后续提示文案。
 *
 * 因为后端 run_command 的确认令牌机制是工具层（非 ADK 原生确认），
 * 用户批准后需要让 Agent 用 confirm_token 重新调用 run_command。
 * 这里生成一段喂给模型的消息，引导它带上 token 重发命令。
 *
 * @param {{command:string, confirmToken:string}} ctx
 * @returns {string}
 */
export function buildRunCommandConfirmationPrompt({ command, confirmToken }) {
  return [
    '用户已确认执行危险命令，请使用 run_command 工具重新执行，并在参数中携带 confirm_token 以放行：',
    '',
    `命令：${command}`,
    `confirm_token：${confirmToken}`,
    '',
    '注意：仅可执行这一条已确认命令，不得扩展为其他危险操作。',
  ].join('\n')
}

/**
 * 提取 run_command 结果中的编译诊断列表。
 *
 * @param {string|object|null} result
 * @returns {Array<{severity:string,file:string,line?:number,column?:number,message:string}>}
 */
export function getRunCommandDiagnostics(result) {
  const obj = parseAny(result)
  if (!obj || !Array.isArray(obj.diagnostics)) return []
  return obj.diagnostics
}

/**
 * 剥离 `[[ARTIFACT:path|title|mime]]` 下载标记。
 *
 * 该标记是脚本产物 → 前端文件卡片的内部信号（见后端 base_instruction.md），
 * 仅用于界面，对用户无意义。后端在文本出口/工具输出已剥，前端再兜底一道：
 * 命令文本（剥 echo 及标记）、结果输出（剥标记原文）。
 *
 * @param {string} text
 * @returns {string}
 */
export function stripArtifact(text) {
  if (!text || typeof text !== 'string') return text || ''
  return text
    .replace(/\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]/g, '')
    // 收敛剥除后残留的多余空行（标记常独占一行）
    .replace(/\n{3,}/g, '\n\n')
    .trim()
}

