/**
 * 工具结果解析工具集
 *
 * 把后端工具返回的结构化字段（require_confirmation / diagnostics / grep summary /
 * list_directory collapsed）解析成前端可直接渲染的形态，保持 ChatPage 组件轻量。
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
      return JSON.parse(v)
    } catch {
      return null
    }
  }
  if (typeof v === 'object') return v
  return null
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
 * 提取 grep 工具结果的摘要信息（命中数超阈值时后端返回）。
 *
 * @param {string|object|null} result
 * @returns {{summary:object, files:Array, totalMatches:number}|null}
 */
export function getGrepSummary(result) {
  const obj = parseAny(result)
  if (!obj || !obj.summary || obj.summary.summary_enabled !== true) return null
  return {
    summary: obj.summary,
    files: Array.isArray(obj.matches) ? obj.matches : [],
    totalMatches: Number(obj.total_matches || 0),
  }
}

/**
 * 提取 list_directory 结果中被折叠的深层目录条目。
 *
 * @param {string|object|null} result
 * @returns {Array<{name:string, kind:'collapsed', collapsed_count:number}>}
 */
export function getCollapsedDirectoryEntries(result) {
  const obj = parseAny(result)
  if (!obj || !Array.isArray(obj.entries)) return []
  return obj.entries.filter(
    (e) => e && e.kind === 'collapsed' && typeof e.collapsed_count === 'number',
  )
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

