<template>
  <div class="tool-card" :class="cardClass">
    <!-- ───────────── 头部栏（图标 + 标题 + 右侧状态） ───────────── -->
    <div class="tool-head" :class="{ clickable: canToggle }" @click="onHeadClick">
      <span class="tool-icon"><component :is="iconComp" :size="14" /></span>
      <span class="tool-name">{{ titleFor(msg) }}</span>
      <span v-if="msg.serverName" class="tool-source">{{ msg.serverName }}</span>

      <span class="tool-status">
        <!-- 终端：exit code -->
        <span
          v-if="isShellCall(msg) && getShellResult(msg.result)"
          class="pf-badge"
          :class="getShellResult(msg.result).ok ? 'ok' : 'fail'"
        >exit {{ getShellResult(msg.result).exitCode }}</span>
        <!-- 校验：通过/失败 -->
        <span
          v-else-if="getValidateResult(msg.result)"
          class="pf-badge"
          :class="getValidateResult(msg.result).ok ? 'ok' : 'fail'"
        >{{ getValidateResult(msg.result).ok ? '✓ 通过' : '✗ 失败' }}</span>
        <!-- 注册：成功/失败 -->
        <span
          v-else-if="getRegisterResult(msg.result)"
          class="pf-badge"
          :class="getRegisterResult(msg.result).ok ? 'ok' : 'fail'"
        >{{ getRegisterResult(msg.result).ok ? '✓ 成功' : '✗ 失败' }}</span>
        <!-- 通用：运行中 / 完成 -->
        <span v-else-if="msg.status === 'running'" class="running-pill">
          <el-icon class="is-loading"><Loading /></el-icon>运行中
        </span>
        <span v-else-if="msg.status === 'aborted'" class="abort-pill">已中止</span>
        <span v-else class="done-pill">完成</span>

        <span v-if="canToggle" class="expand-arrow" :class="{ expanded: msg._expanded }">▸</span>
      </span>
    </div>

    <!-- ═══════════════ 终端命令卡（始终展开） ═══════════════ -->
    <template v-if="isShellCall(msg)">
      <div v-if="getShellCommand(msg.args)" class="shell-cmd">
        <span class="shell-prompt">$</span>
        <code class="shell-cmd-text">{{ getShellCommand(msg.args) }}</code>
        <button type="button" class="copy-btn" @click="copyCode(getShellCommand(msg.args))">复制</button>
      </div>
      <div v-if="getShellResult(msg.result)" class="shell-output-wrap">
        <button type="button" class="copy-btn shell-copy" @click="copyCode(shellOutput)">复制</button>
        <pre class="tool-code shell-output">{{ shellOutput }}</pre>
      </div>
      <div v-else-if="msg.status === 'running'" class="tool-running">
        <el-icon class="is-loading"><Loading /></el-icon>
        <span>正在沙箱中执行…</span>
      </div>
      <div v-else-if="msg.status === 'aborted'" class="tool-aborted">
        <span>已中止</span>
      </div>

      <!-- 审批内嵌条（chatStore.pendingShellApprovals 队列挂载到本卡时显示） -->
      <div v-if="msg._pendingApproval" class="shell-approval">
        <div class="shell-approval-hint"><AlertTriangle :size="13" /> 该命令不在安全白名单，需审批后执行</div>
        <div class="shell-approval-actions">
          <button type="button" class="apv-btn deny" @click="decideApproval(false)">拒绝</button>
          <button type="button" class="apv-btn allow" @click="decideApproval(true)">允许执行</button>
        </div>
      </div>
    </template>

    <!-- ═══════════════ 其他工具（特化结果默认展开；兜底卡可折叠） ═══════════════ -->
    <div v-else class="tool-body">
      <template v-if="!canToggle || msg._expanded">
        <!-- 文件写入类工具（edit_file/create_file）的红绿 diff 视图（对齐 codex/Claude Code 编辑卡） -->
        <div v-if="diffView" class="tool-section diff-section">
          <div
            v-for="(l, li) in diffView.lines"
            :key="li"
            class="diff-line"
            :class="l.type"
          ><span class="diff-gutter">{{ l.type === 'add' ? '+' : l.type === 'del' ? '−' : '' }}</span><span class="diff-text">{{ l.text }}</span></div>
        </div>

        <!-- Rhai 脚本代码块 -->
        <div v-if="extractScript(msg.args)" class="tool-section">
          <div class="section-label">插件代码 (Rhai)</div>
          <div class="code-block-wrapper">
            <div class="code-header">
              <span class="code-lang">rhai</span>
              <button type="button" class="copy-btn" @click="copyCode(extractScript(msg.args))">复制</button>
            </div>
            <pre class="tool-code hljs" v-html="highlightCode(extractScript(msg.args))"></pre>
          </div>
        </div>

        <!-- 其他参数（非 script 部分；记忆建议卡单独渲染，不重复显示参数 JSON） -->
        <div v-if="getOtherArgs(msg.args) && !isProposeMemory(msg)" class="tool-section">
          <div class="section-label">参数</div>
          <pre class="tool-code">{{ formatJson(getOtherArgs(msg.args)) }}</pre>
        </div>

        <!-- 记忆建议卡片（propose_memory 工具）：用户在此确认是否记入长期记忆 -->
        <div v-if="isProposeMemory(msg)" class="tool-section memory-proposal" :class="msg._memoryDecision">
          <div class="mp-top">
            <span class="mp-badge" :class="proposeArgs(msg).type === 'pitfall' ? 'pitfall' : 'preference'">
              {{ proposeArgs(msg).type === 'pitfall' ? '⚠️ 避坑' : '💡 习惯' }}
            </span>
            <span class="mp-scope">{{ proposeArgs(msg).scope === 'assistant' ? '仅当前助手' : '所有助手' }}</span>
          </div>
          <div class="mp-content">{{ proposeArgs(msg).content }}</div>
          <div v-if="proposeArgs(msg).reason" class="mp-reason">
            <span class="mp-reason-label">理由：</span>{{ proposeArgs(msg).reason }}
          </div>
          <div v-if="msg._memoryDecision" class="mp-decided" :class="msg._memoryDecision">
            {{ msg._memoryDecision === 'accepted' ? '✓ 已加入长期记忆' : '✗ 已忽略' }}
          </div>
          <div v-else class="mp-actions">
            <button type="button" class="mp-btn ignore" :disabled="msg._memoryBusy" @click="decideMemory(false)">忽略</button>
            <button type="button" class="mp-btn accept" :disabled="msg._memoryBusy" @click="decideMemory(true)">加入记忆</button>
          </div>
        </div>

        <!-- 结构化校验结果 -->
        <div v-if="getValidateResult(msg.result)" class="tool-section">
          <div class="validate-overview" :class="getValidateResult(msg.result).ok ? 'pass' : 'fail'">
            <span class="overview-icon">{{ getValidateResult(msg.result).ok ? '✓' : '✗' }}</span>
            <span>{{ getValidateResult(msg.result).summary }}</span>
          </div>
          <div class="case-list">
            <div
              v-for="(c, ci) in getValidateResult(msg.result).cases"
              :key="ci"
              class="case-item"
              :class="c.passed ? 'pass' : 'fail'"
            >
              <div class="case-header">
                <span class="case-icon">{{ c.passed ? '✓' : '✗' }}</span>
                <span class="case-name">{{ c.name }}</span>
                <span v-if="c.duration_ms" class="case-duration">{{ c.duration_ms }}ms</span>
              </div>
              <div v-if="c.layers" class="case-layers">
                <span class="layer-tag" :class="c.layers.l1 ? 'pass' : 'fail'">L1 语法</span>
                <span v-if="c.layers.l2 !== null" class="layer-tag" :class="c.layers.l2 ? 'pass' : 'fail'">L2 沙箱</span>
                <span v-if="c.layers.l3 !== null" class="layer-tag" :class="c.layers.l3 ? 'pass' : 'fail'">L3 编译</span>
              </div>
              <div v-if="c.error" class="case-error">{{ c.error }}</div>
            </div>
          </div>
        </div>

        <!-- 注册结果 -->
        <div v-else-if="getRegisterResult(msg.result)" class="tool-section">
          <div class="register-info" :class="getRegisterResult(msg.result).ok ? 'pass' : 'fail'">
            <span class="overview-icon">{{ getRegisterResult(msg.result).ok ? '✓' : '✗' }}</span>
            <span>{{ getRegisterResult(msg.result).message }}</span>
          </div>
          <div v-if="getRegisterResult(msg.result).ok" class="register-detail">
            <span class="detail-label">插件 ID：</span>
            <code class="detail-value">{{ getRegisterResult(msg.result).plugin_id }}</code>
            <span class="detail-label">版本：</span>
            <code class="detail-value">v{{ getRegisterResult(msg.result).version }}</code>
          </div>
        </div>

        <!-- 截图结果 -->
        <div v-else-if="msg.toolName && msg.toolName.toLowerCase().includes('screenshot') && getScreenshotImageUrl(msg.result)" class="tool-section">
          <img :src="getScreenshotImageUrl(msg.result)" alt="截图" class="screenshot-img" />
        </div>

        <!-- 编译诊断 -->
        <div v-else-if="msg._diagnostics && msg._diagnostics.length" class="tool-section">
          <div class="diag-list">
            <div
              v-for="(d, di) in msg._diagnostics"
              :key="di"
              class="diag-item"
              :class="d.severity"
            >
              <span class="diag-sev" :class="d.severity">{{ d.severity === 'error' ? '✗' : '⚠' }}</span>
              <code v-if="d.file" class="diag-loc">{{ d.file }}<template v-if="d.line">:{{ d.line }}<template v-if="d.column">:{{ d.column }}</template></template></code>
              <span class="diag-msg">{{ d.message }}</span>
            </div>
          </div>
        </div>

        <!-- 通用 JSON 结果（diff 视图已渲染的写入类工具跳过：同一 diff 不显示两遍，
             但失败（无 diff）时仍走这里展示错误详情） -->
        <div v-else-if="msg.result && !diffView" class="tool-section">
          <div class="section-label">结果</div>
          <pre class="tool-code">{{ formatJson(msg.result) }}</pre>
        </div>

        <!-- 运行中提示（无结果时） -->
        <div v-else-if="msg.status === 'running'" class="tool-running">
          <el-icon class="is-loading"><Loading /></el-icon>
          <span>正在沙箱中执行…</span>
        </div>
        <div v-else-if="msg.status === 'aborted'" class="tool-aborted">
          <span>已中止</span>
        </div>
      </template>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { ElMessage } from 'element-plus'
import { Loading } from '@element-plus/icons-vue'
import {
  Wrench, Search, Globe, Activity, Package, Folder,
  Terminal, Image, AlertTriangle, ShieldCheck,
  FileText, FileEdit, FilePlus,
} from 'lucide-vue-next'
import hljs from 'highlight.js/lib/core'
import { parseAny, stripArtifact, getDiffResult } from '../../utils/toolResult'
import { useChatStore } from '../../stores/chat'
import { acceptMemoryProposal, rejectMemoryProposal } from '../../api'

const props = defineProps({
  msg: { type: Object, required: true },
})

const chatStore = useChatStore()

// 工具图标：按名称语义映射到 lucide 线性图标
const iconComp = computed(() => {
  const n = props.msg.toolName || ''
  if (n === 'shell_command' || n === '终端') return Terminal
  if (n.includes('截图') || n.toLowerCase().includes('screenshot')) return Image
  if (n.includes('诊断') || n.includes('编译')) return AlertTriangle
  if (n.includes('校验')) return ShieldCheck
  if (n.includes('注册')) return Package
  if (n.includes('SNMP') || n.includes('采集')) return Activity
  // 内置代码/文件工具：各自区分图标（对齐 codex Read/Search/List/Write/Edit 的差异化）
  // 兼容 codex 短词(Read/List/Search/Edit/Write)、原始英文名、旧中文名
  if (n === 'read_file' || n === 'Read' || n.includes('读取文件')) return FileText
  if (n === 'glob' || n === 'Glob') return Folder
  if (n === 'edit_file' || n === 'Edit' || n.includes('编辑文件')) return FileEdit
  if (n === 'create_file' || n === 'Write' || n.includes('新建文件')) return FilePlus
  if (n === 'grep' || n === 'Search' || n.includes('搜索内容') || n.includes('查询') || n.includes('检索') || n.includes('搜索')) return Search
  if (n.includes('浏览器') || n.includes('打开') || n.includes('抓取')) return Globe
  return Wrench
})

// 卡片标题：shell_command 显示「终端」，Edit/Write 带路径（对齐 codex 标签带关键参数）
function titleFor(msg) {
  const n = msg.toolName || '工具'
  if (n === 'shell_command') return '终端'
  if (n === 'propose_memory') return '建议记忆'
  if (n === 'edit_file' || n === 'Edit' || n === '编辑文件') {
    const p = String((parseAny(msg.args) || {}).path || '').trim()
    return p ? `Edit · ${p}` : 'Edit'
  }
  if (n === 'create_file' || n === 'Write' || n === '新建文件') {
    const p = String((parseAny(msg.args) || {}).path || '').trim()
    return p ? `Write · ${p}` : 'Write'
  }
  return n
}

// 卡片左侧状态色边框：成功绿 / 失败红 / 运行黄
const cardClass = computed(() => {
  const m = props.msg
  const cls = []
  if (m.toolName === 'shell_command') cls.push('kind-shell')
  if (m.toolName === 'propose_memory') cls.push('kind-memory')
  const sr = getShellResult(m.result)
  const vr = getValidateResult(m.result)
  const rr = getRegisterResult(m.result)
  if (sr) cls.push(sr.ok ? 'st-ok' : 'st-fail')
  else if (vr) cls.push(vr.ok ? 'st-ok' : 'st-fail')
  else if (rr) cls.push(rr.ok ? 'st-ok' : 'st-fail')
  if (m.status === 'running' && !sr) cls.push('st-running')
  return cls
})

// 是否可折叠：仅「无特化渲染」的兜底工具卡可折叠；特化卡（终端/校验/注册/截图/
// diff 视图…）默认展开
const canToggle = computed(() => {
  const m = props.msg
  if (isShellCall(m)) return false
  if (isProposeMemory(m)) return false
  if (getValidateResult(m.result)) return false
  if (getRegisterResult(m.result)) return false
  if (m.toolName && m.toolName.toLowerCase().includes('screenshot') && getScreenshotImageUrl(m.result)) return false
  if (m._diagnostics && m._diagnostics.length) return false
  // 写入类工具的 diff 是核心展示：默认展开（对齐「特化结果默认展开」约定）
  if (diffView.value) return false
  return true
})

// shell 输出：剥掉 [[ARTIFACT:...]] 标记原文（后端已剥工具 output，这里兜底历史/异常残留）
const shellOutput = computed(() => {
  const r = getShellResult(props.msg.result)
  return r && r.output ? stripArtifact(r.output) : (r && r.output) || ''
})

function onHeadClick() {
  if (canToggle.value) props.msg._expanded = !props.msg._expanded
}

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

// 文件写入类工具（edit_file / create_file 及 codex 短名/旧中文名）：走 diff 特化渲染
function isFileWriteTool(msg) {
  const n = msg.toolName || ''
  return n === 'edit_file' || n === 'Edit' || n === '编辑文件'
    || n === 'create_file' || n === 'Write' || n === '新建文件'
}

// diff 视图（computed 缓存：parseAny+split 只在 result 变化时算一次，
// 避免流式会话重渲染时每卡每轮重复 JSON.parse 大 diff 两遍）
const diffView = computed(() =>
  isFileWriteTool(props.msg) ? getDiffResult(props.msg.result) : null
)

function extractScript(args) {
  const obj = parseAny(args)
  if (obj && typeof obj.script === 'string' && obj.script.trim()) return obj.script
  return null
}

function getOtherArgs(args) {
  const obj = parseAny(args)
  if (!obj || typeof obj !== 'object') return null
  const rest = { ...obj }
  delete rest.script
  // 文件写入类工具且 diff 视图已命中：大块文本（content/old_text/new_text）已由
  // diff 视图呈现，参数区只留 path/overwrite/append/occurrence 等小字段，避免同一
  // 内容显示两遍。失败（无 diff）时保留全部参数——old_text 是定位匹配失败原因的
  // 唯一线索，不能剔。
  if (diffView.value) {
    delete rest.content
    delete rest.old_text
    delete rest.new_text
  }
  return Object.keys(rest).length > 0 ? rest : null
}

// —— 记忆建议（propose_memory 工具）——
function isProposeMemory(msg) {
  return msg.toolName === 'propose_memory'
}
function proposeArgs(msg) {
  const a = parseAny(msg.args) || {}
  return {
    type: String(a.type || 'preference').toLowerCase(),
    content: a.content || '',
    reason: a.reason || '',
    scope: String(a.scope || 'user').toLowerCase(),
  }
}
function proposalId(msg) {
  const r = parseAny(msg.result) || {}
  return r.proposal_id || ''
}
async function decideMemory(accept) {
  const m = props.msg
  if (m._memoryBusy) return
  const id = proposalId(m)
  if (!id) {
    ElMessage.warning('建议尚未生成，请稍候再试')
    return
  }
  m._memoryBusy = true
  const fn = accept ? acceptMemoryProposal : rejectMemoryProposal
  try {
    const res = await fn(id)
    if (res.code !== 0) {
      ElMessage.error(res.message || '操作失败')
      return
    }
    m._memoryDecision = accept ? 'accepted' : 'rejected'
    ElMessage.success(accept ? '已加入长期记忆' : '已忽略')
  } catch (e) {
    // 传输层异常（HTTP 非 2xx/断网 gql 会 throw）：不接住会成未处理拒绝且无任何提示
    ElMessage.error('操作失败: ' + (e.message || '网络错误'))
  } finally {
    m._memoryBusy = false
  }
}

function getValidateResult(result) {
  const obj = parseAny(result)
  if (!obj || typeof obj !== 'object' || !Array.isArray(obj.cases) || !('passed_cases' in obj)) return null
  return {
    ok: obj.ok === true,
    summary: obj.summary || `${obj.passed_cases}/${obj.total_cases} 用例通过`,
    cases: (obj.cases || []).map((c) => ({
      name: c.name || '未命名',
      passed: c.passed === true,
      duration_ms: c.layer2_sandbox?.duration_ms || c.layer3_code?.duration_ms || null,
      layers: {
        l1: c.layer1_syntax === true,
        l2: c.layer2_sandbox ? c.layer2_sandbox.ok === true : null,
        l3: c.layer3_code ? c.layer3_code.ok === true : null,
      },
      error:
        c.layer1_error
        || (c.layer2_sandbox && !c.layer2_sandbox.ok ? c.layer2_sandbox.error || c.layer2_sandbox.stderr : null)
        || (c.layer3_code && !c.layer3_code.ok ? c.layer3_code.error || c.layer3_code.stderr : null)
        || null,
    })),
  }
}

function getRegisterResult(result) {
  const obj = parseAny(result)
  // parseAny 可能返回字符串（双编码 JSON 只解了一层）；`in` 对原始类型会抛 TypeError，
  // 中断渲染 effect，进而破坏流式渲染。这里强制要求对象后再用 `in`。
  if (!obj || typeof obj !== 'object') return null
  if (!('plugin_id' in obj)) return null
  return {
    ok: obj.ok === true,
    plugin_id: obj.plugin_id || '',
    version: obj.version || '',
    message: obj.message || (obj.ok ? '注册成功' : '注册失败'),
  }
}

// 已知的截图 base64 字段名（小写匹配，兼容 base64Data / data / image 等各种命名）
const SCREENSHOT_IMG_KEYS = new Set([
  'base64data', 'base64_data', 'data', 'image', 'base64', 'screenshot', 'png', 'imagedata',
])

// 判定字符串是否疑似 base64 图片数据（仅含 base64 字符集 + 足够长）
function isLikelyBase64(s) {
  return typeof s === 'string' && s.length > 100 && /^[A-Za-z0-9+/=]+$/.test(s)
}

// 按 base64 数据头（magic bytes）推断 MIME，避免 JPEG 被当成 PNG 导致显示异常
function guessImageMime(b64) {
  if (b64.startsWith('/9j/')) return 'image/jpeg'
  if (b64.startsWith('iVBOR')) return 'image/png'
  if (b64.startsWith('R0lGOD')) return 'image/gif'
  if (b64.startsWith('UklGR')) return 'image/webp'
  return 'image/png'
}

// 从任意结构中递归挖掘 base64 图片串。
// 兼容三种形态：已知字段直接持有、纯 base64 串、被 JSON 化后塞进字符串字段
// （如 output: "{\"base64Data\":\"...\"}"，需先 JSON.parse 再递归）。
function findImageBase64(v) {
  if (v == null) return null
  if (typeof v === 'string') {
    const trimmed = v.trim()
    if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
      try {
        const inner = JSON.parse(trimmed)
        const found = findImageBase64(inner)
        if (found) return found
      } catch {}
    }
    return isLikelyBase64(v) ? v : null
  }
  if (Array.isArray(v)) {
    for (const item of v) {
      const found = findImageBase64(item)
      if (found) return found
    }
    return null
  }
  if (typeof v === 'object') {
    for (const [k, val] of Object.entries(v)) {
      if (SCREENSHOT_IMG_KEYS.has(String(k).toLowerCase())) {
        if (typeof val === 'string' && isLikelyBase64(val)) return val
        const found = findImageBase64(val)
        if (found) return found
      }
    }
    for (const val of Object.values(v)) {
      const found = findImageBase64(val)
      if (found) return found
    }
  }
  return null
}

function getScreenshotImageUrl(result) {
  const obj = parseAny(result)
  if (!obj) return null
  // 1) 优先 image_url：后端存盘注入的标准字段（/api/screenshots/{file}）
  if (typeof obj.image_url === 'string' && obj.image_url) return obj.image_url
  if (obj.mime_type && typeof obj.data === 'string' && obj.data.length > 100) {
    return `data:${obj.mime_type};base64,${obj.data}`
  }
  // 2) 兜底：递归挖掘 base64Data / 嵌套 JSON 串中的图片数据
  const b64 = findImageBase64(obj)
  if (b64) return `data:${guessImageMime(b64)};base64,${b64}`
  return null
}

// Rhai 脚本高亮（近似用 rust 语法）
function highlightCode(code) {
  if (!code) return ''
  try {
    if (hljs.getLanguage('rust')) {
      return hljs.highlight(code, { language: 'rust' }).value
    }
  } catch (_) {}
  return code.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}

function copyCode(code) {
  navigator.clipboard
    .writeText(code)
    .then(() => ElMessage.success('已复制'))
    .catch(() => ElMessage.error('复制失败'))
}

// 内嵌审批：用户点击允许/拒绝后回传后端，并清除本卡的待审批标记
//（审批队列支持并发，按 approval_id 定向出队）
function decideApproval(approved) {
  const ap = props.msg._pendingApproval
  if (!ap) return
  props.msg._pendingApproval = null
  chatStore.resolveShellApproval(ap.approval_id, approved)
}

// shell_command 执行结果:{ ok, exit_code, output, duration_ms }
// output 为后端预格式化文本 "Exit code: X\nWall time: Y.Ys\nOutput:\n<实际输出>"，
// 这里剥掉前缀，只保留实际输出正文（退出码/耗时单独用结构化字段展示，避免重复）。
function getShellResult(result) {
  const obj = parseAny(result)
  if (!obj || typeof obj !== 'object' || !('exit_code' in obj) || typeof obj.output !== 'string') {
    return null
  }
  let body = obj.output
  const marker = 'Output:\n'
  const idx = body.indexOf(marker)
  if (idx >= 0) body = body.slice(idx + marker.length)
  return {
    ok: obj.ok === true,
    exitCode: obj.exit_code,
    durationMs: typeof obj.duration_ms === 'number' ? obj.duration_ms : null,
    output: body,
  }
}

// 从参数中提取命令文本
function getShellCommand(args) {
  const obj = parseAny(args)
  if (!obj || typeof obj.command !== 'string') return null
  // 剥掉命令里的 [[ARTIFACT:...]] 下载标记及其 echo（内部信号，见 base_instruction.md）：
  // 先剥「echo "[[ARTIFACT:...]]"」整体（含可选引号），再剥裸标记，最后清理尾部连接符。
  const cmd = obj.command
    .replace(/echo\s+"?\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]"?/g, '')
    .replace(/\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]/g, '')
    .replace(/\s*&&\s*$/g, '')
    .replace(/^\s*&&\s*/g, '')
    .replace(/\s*;\s*$/g, '')
    .trim()
  return cmd || null
}

// 判断是否为 shell_command 调用：结果到达前（running）也走“始终展开”渲染，
// 避免先显示折叠的 “shell_command” 标题、结果到了再展开的视觉跳变。
// 后端 tool_display_name 对 shell_command 原样返回，故 toolName 即 'shell_command'。
function isShellCall(msg) {
  return msg.toolName === 'shell_command' || !!getShellResult(msg.result)
}
</script>

<style scoped>
/* —— 记忆建议卡片（propose_memory）—— */
.memory-proposal {
  border: 1px solid var(--el-border-color-light, #e4e7ed);
  border-radius: 8px;
  padding: 10px 12px;
  background: var(--el-fill-color-light, #fafafa);
}
.memory-proposal.accepted {
  border-color: var(--el-color-success-light-5, #95d475);
  background: var(--el-color-success-light-9, #f0f9eb);
}
.memory-proposal.rejected { opacity: 0.6; }
.mp-top { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
.mp-badge { font-size: 12px; font-weight: 600; padding: 2px 8px; border-radius: 10px; }
.mp-badge.preference { background: #ecf5ff; color: #409eff; }
.mp-badge.pitfall { background: #fdf6ec; color: #e6a23c; }
.mp-scope { font-size: 12px; color: var(--el-text-color-secondary, #909399); }
.mp-content { font-size: 14px; line-height: 1.6; color: var(--el-text-color-primary, #303133); margin-bottom: 4px; }
.mp-reason { font-size: 12px; color: var(--el-text-color-secondary, #909399); margin-bottom: 8px; }
.mp-reason-label { font-weight: 600; }
.mp-actions { display: flex; gap: 8px; justify-content: flex-end; margin-top: 8px; }
.mp-btn { padding: 4px 14px; border-radius: 6px; font-size: 13px; cursor: pointer; border: 1px solid var(--el-border-color, #dcdfe6); background: #fff; }
.mp-btn:disabled { cursor: not-allowed; opacity: 0.6; }
.mp-btn.accept { background: var(--el-color-primary, #409eff); color: #fff; border-color: var(--el-color-primary, #409eff); }
.mp-btn.ignore { color: var(--el-text-color-secondary, #909399); }
.mp-decided { font-size: 13px; margin-top: 8px; font-weight: 600; }
.mp-decided.accepted { color: var(--el-color-success, #67c23a); }
.mp-decided.rejected { color: var(--el-text-color-secondary, #909399); }
/* ───────────── 卡片容器（统一设计语言：圆角 + 状态色左边框） ───────────── */
.tool-card {
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: rgba(14, 14, 22, 0.6);
  max-width: 88%;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.2);
  overflow: hidden;
  transition: border-color 0.2s, box-shadow 0.2s;
}
.tool-card:hover {
  border-color: var(--border-hover);
  box-shadow: 0 4px 18px rgba(0, 0, 0, 0.28);
}
.tool-card.st-ok { border-left: 3px solid var(--done); }
.tool-card.st-fail { border-left: 3px solid var(--error); }
.tool-card.st-running { border-left: 3px solid var(--doing); }

/* ───────────── 头部栏 ───────────── */
.tool-head {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 9px 14px;
  font-size: 12px;
  user-select: none;
}
.tool-head.clickable { cursor: pointer; }
.tool-head.clickable:hover { background: rgba(255, 255, 255, 0.025); }
.tool-icon {
  width: 22px;
  height: 22px;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--accent-dim);
  border-radius: 6px;
  color: var(--accent);
  flex-shrink: 0;
}
.tool-name {
  font-weight: 700;
  color: var(--text-h);
  flex-shrink: 0;
}
/* MCP 工具来源（server 名）：工具名后的小字徽标 */
.tool-source {
  font-size: 11px;
  font-weight: 500;
  color: var(--muted);
  padding: 1px 7px;
  border-radius: 8px;
  background: var(--accent-dim);
  flex-shrink: 0;
}
.tool-status {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: 8px;
}
/* 通过/失败/exit 徽章 */
.pf-badge {
  font-family: var(--font-mono);
  font-size: 11px;
  font-weight: 700;
  padding: 2px 10px;
  border-radius: 10px;
}
.pf-badge.ok {
  background: rgba(16, 185, 129, 0.14);
  color: var(--done);
  border: 1px solid rgba(16, 185, 129, 0.3);
}
.pf-badge.fail {
  background: rgba(239, 68, 68, 0.14);
  color: var(--error);
  border: 1px solid rgba(239, 68, 68, 0.3);
}
/* 运行中 / 完成 小药丸 */
.running-pill {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  font-weight: 700;
  padding: 2px 10px;
  border-radius: 10px;
  background: var(--doing-dim);
  color: var(--doing);
  border: 1px solid rgba(245, 158, 11, 0.3);
}
.running-pill .el-icon { font-size: 12px; }
.abort-pill {
  font-size: 11px;
  font-weight: 700;
  padding: 2px 10px;
  border-radius: 10px;
  background: var(--muted-dim, rgba(148, 163, 184, 0.12));
  color: var(--muted, #94a3b8);
  border: 1px solid rgba(148, 163, 184, 0.3);
}
.done-pill {
  font-size: 11px;
  font-weight: 700;
  padding: 2px 10px;
  border-radius: 10px;
  background: var(--done-dim);
  color: var(--done);
  border: 1px solid rgba(16, 185, 129, 0.3);
}
.expand-arrow {
  font-size: 11px;
  color: var(--muted);
  transition: transform 0.2s;
}
.expand-arrow.expanded { transform: rotate(90deg); }

/* ───────────── body 容器 ───────────── */
.tool-body {
  padding: 4px 14px 12px;
  border-top: 1px solid var(--border);
}
.tool-section { margin-top: 10px; }

/* ── 文件写入 diff 视图（红绿行，对齐 codex/Claude Code 编辑卡） ── */
.diff-section {
  border: 1px solid var(--border, #2a2a35);
  border-radius: var(--radius-sm, 8px);
  overflow: hidden;
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  line-height: 1.55;
  max-height: 320px;
  overflow-y: auto;
}
.diff-line {
  display: flex;
  align-items: baseline;
  white-space: pre-wrap;
  word-break: break-all;
}
.diff-gutter {
  flex: 0 0 26px;
  text-align: center;
  color: var(--muted, #8e8e93);
  user-select: none;
  background: rgba(128, 128, 128, 0.06);
}
.diff-text { flex: 1; padding-right: 8px; }
.diff-line.add { background: rgba(46, 160, 67, 0.13); }
.diff-line.add .diff-gutter { color: #3fb950; }
.diff-line.del { background: rgba(248, 81, 73, 0.13); }
.diff-line.del .diff-gutter { color: #f85149; }
.diff-line.hunk {
  color: #7aa2f7;
  background: rgba(122, 162, 247, 0.08);
  padding: 1px 8px 1px 0;
}
.diff-line.hunk .diff-gutter { visibility: hidden; }
.diff-line.meta {
  color: var(--muted, #8e8e93);
  padding: 1px 8px 1px 0;
}
.diff-line.meta .diff-gutter { visibility: hidden; }
.diff-line.ctx { color: var(--text, #c9c9cf); }
.tool-section:first-child { margin-top: 8px; }
.section-label {
  font-size: 10px;
  color: var(--muted);
  font-weight: 700;
  text-transform: uppercase;
  letter-spacing: 0.8px;
  margin-bottom: 6px;
}

/* ───────────── Rhai 代码块 ───────────── */
.code-block-wrapper {
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  overflow: hidden;
  margin-top: 4px;
}
.code-block-wrapper .tool-code {
  border: none;
  border-radius: 0;
  max-height: 400px;
  overflow-y: auto;
  white-space: pre;
  word-break: normal;
}

/* ───────────── 校验结果 ───────────── */
.validate-overview {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 14px;
  border-radius: var(--radius-sm);
  font-size: 13px;
  font-weight: 700;
}
.validate-overview.pass {
  background: rgba(16, 185, 129, 0.1);
  color: var(--done);
  border: 1px solid rgba(16, 185, 129, 0.25);
}
.validate-overview.fail {
  background: rgba(239, 68, 68, 0.1);
  color: var(--error);
  border: 1px solid rgba(239, 68, 68, 0.25);
}
.overview-icon { font-size: 16px; }

.case-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-top: 8px;
}
.case-item {
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 10px 14px;
  background: rgba(10, 10, 18, 0.5);
  transition: all 0.2s;
}
.case-item.pass { border-left: 3px solid var(--done); }
.case-item.fail { border-left: 3px solid var(--error); }
.case-header {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
}
.case-icon { font-weight: 700; }
.case-item.pass .case-icon { color: var(--done); }
.case-item.fail .case-icon { color: var(--error); }
.case-name {
  color: var(--text);
  flex: 1;
  font-weight: 600;
}
.case-duration {
  color: var(--muted);
  font-size: 11px;
  font-family: var(--font-mono);
}
.case-layers {
  display: flex;
  gap: 6px;
  margin-top: 6px;
  flex-wrap: wrap;
}
.layer-tag {
  font-size: 10px;
  padding: 2px 10px;
  border-radius: 10px;
  font-weight: 700;
}
.layer-tag.pass {
  background: rgba(16, 185, 129, 0.15);
  color: var(--done);
}
.layer-tag.fail {
  background: rgba(239, 68, 68, 0.15);
  color: var(--error);
}
.case-error {
  margin-top: 6px;
  padding: 8px 12px;
  background: rgba(239, 68, 68, 0.06);
  border-radius: var(--radius-sm);
  font-size: 11px;
  color: #f87171;
  font-family: var(--font-mono);
  white-space: pre-wrap;
  word-break: break-all;
}

/* ───────────── 注册结果 ───────────── */
.register-info {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 14px;
  border-radius: var(--radius-sm);
  font-size: 13px;
  font-weight: 700;
}
.register-info.pass {
  background: rgba(16, 185, 129, 0.1);
  color: var(--done);
  border: 1px solid rgba(16, 185, 129, 0.25);
}
.register-info.fail {
  background: rgba(239, 68, 68, 0.1);
  color: var(--error);
  border: 1px solid rgba(239, 68, 68, 0.25);
}
.register-detail {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 8px;
  padding: 10px 14px;
  background: rgba(10, 10, 18, 0.5);
  border-radius: var(--radius-sm);
  font-size: 12px;
  flex-wrap: wrap;
}
.detail-label { color: var(--muted); }
.detail-value {
  background: rgba(0, 212, 255, 0.12);
  color: var(--accent);
  padding: 2px 10px;
  border-radius: 4px;
  font-family: var(--font-mono);
  font-weight: 500;
  border: 1px solid rgba(0, 212, 255, 0.15);
}

/* ───────────── 截图 ───────────── */
.screenshot-img {
  max-width: 100%;
  border-radius: var(--radius-sm);
  border: 1px solid var(--border);
}

/* ───────────── 终端命令（始终展开） ───────────── */
.tool-card.kind-shell .tool-head { border-bottom: 1px solid var(--border); }
.shell-cmd {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 14px;
  background: rgba(255, 255, 255, 0.02);
  border-bottom: 1px solid var(--border);
  font-size: 12px;
}
.shell-prompt {
  color: var(--done);
  font-weight: 700;
  flex-shrink: 0;
}
.shell-cmd-text {
  flex: 1;
  font-family: var(--font-mono);
  white-space: pre-wrap;
  word-break: break-all;
  color: var(--text);
}
.shell-output-wrap {
  position: relative;
}
.shell-output {
  max-height: 400px;
  overflow: auto;
  border: none;
  border-radius: 0;
}
.shell-copy {
  position: absolute;
  top: 8px;
  right: 8px;
  opacity: 0;
  transition: opacity 0.15s;
  z-index: 1;
}
.shell-output-wrap:hover .shell-copy,
.shell-copy:focus-visible {
  opacity: 1;
}

/* ───────────── 运行中提示 ───────────── */
.tool-running {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 10px;
  padding: 10px 14px;
  background: rgba(245, 158, 11, 0.08);
  border: 1px solid rgba(245, 158, 11, 0.2);
  border-radius: var(--radius-sm);
  font-size: 12px;
  color: var(--doing);
  font-weight: 600;
}
.tool-running .el-icon { font-size: 14px; }

/* ───────────── 已中止提示（取消/异常未返回结果） ───────────── */
.tool-aborted {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 10px;
  padding: 10px 14px;
  background: rgba(148, 163, 184, 0.08);
  border: 1px solid rgba(148, 163, 184, 0.2);
  border-radius: var(--radius-sm);
  font-size: 12px;
  color: var(--muted, #94a3b8);
  font-weight: 600;
}

/* ───────────── 审批内嵌条 ───────────── */
.shell-approval {
  padding: 10px 14px;
  border-top: 1px solid var(--border);
  background: rgba(245, 158, 11, 0.06);
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.shell-approval-hint {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  font-weight: 600;
  color: var(--doing);
}
.shell-approval-actions {
  margin-left: auto;
  display: flex;
  gap: 8px;
}
.apv-btn {
  padding: 4px 14px;
  border-radius: var(--radius-sm);
  font-size: 12px;
  font-weight: 700;
  cursor: pointer;
  transition: all 0.15s;
  font-family: var(--font-sans);
}
.apv-btn.deny {
  background: rgba(239, 68, 68, 0.1);
  border: 1px solid rgba(239, 68, 68, 0.4);
  color: #f87171;
}
.apv-btn.deny:hover { background: rgba(239, 68, 68, 0.2); }
.apv-btn.allow {
  background: rgba(16, 185, 129, 0.12);
  border: 1px solid rgba(16, 185, 129, 0.4);
  color: var(--done);
}
.apv-btn.allow:hover { background: rgba(16, 185, 129, 0.22); }

/* ───────────── 编译诊断 ───────────── */
.diag-list { display: flex; flex-direction: column; gap: 6px; }
.diag-item {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 6px 10px;
  border-radius: 6px;
  font-size: 12px;
  line-height: 1.5;
}
.diag-item.error { background: rgba(245, 108, 108, 0.08); }
.diag-item.warning { background: rgba(230, 162, 60, 0.08); }
.diag-sev { font-weight: 700; flex-shrink: 0; }
.diag-sev.error { color: #f56c6c; }
.diag-sev.warning { color: #e6a23c; }
.diag-loc {
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--text-h);
  background: rgba(10, 10, 18, 0.6);
  padding: 1px 5px;
  border-radius: 3px;
  flex-shrink: 0;
}
.diag-msg { color: var(--text); flex: 1; word-break: break-word; }

/* ───────────── grep 摘要 ───────────── */
.grep-alert { margin-bottom: 8px; }
.grep-summary-list { display: flex; flex-direction: column; gap: 4px; }
.grep-summary-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 4px 8px;
  border-radius: 4px;
  background: rgba(10, 10, 18, 0.6);
}
.grep-file {
  font-family: var(--font-mono);
  font-size: 12px;
  color: var(--text-h);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* ───────────── 折叠目录提示 ───────────── */
.collapsed-hint-list { display: flex; flex-direction: column; gap: 4px; }
.collapsed-hint-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 4px 8px;
  border-radius: 4px;
  background: rgba(10, 10, 18, 0.6);
  font-size: 12px;
}
.collapsed-name {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-family: var(--font-mono);
  color: var(--text-h);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
