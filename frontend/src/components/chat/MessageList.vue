<template>
  <template v-for="(group, idx) in grouped" :key="idx">
    <div class="message-row msg-enter" :style="{ animationDelay: Math.min(idx * 35, 350) + 'ms' }" :id="'msg-' + group.origIdx" :class="group.kind === 'single' ? group.msg.role : 'tool_call'">
      <ToolCallGroup v-if="group.kind === 'toolGroup'" :items="group.items" />
      <ExploringCell v-else-if="group.kind === 'exploring'" :items="group.items" />
      <UserBubble
        v-else-if="group.msg.role === 'user'"
        :content="group.msg.content"
        :attachments="group.msg.attachments"
        @preview-image="$emit('preview-image', $event)"
      />
      <AssistantMessage v-else-if="group.msg.role === 'assistant'" :content="group.msg.content" />
      <ToolCallCard v-else-if="group.msg.role === 'tool_call'" :msg="group.msg" />
      <SkillRow v-else-if="group.msg.role === 'skill'" :msg="group.msg" />
      <SpawnWaitRow v-else-if="group.msg.role === 'spawn_wait'" :msg="group.msg" />
      <ChildAgentCard v-else-if="group.msg.role === 'child_agent'" :msg="group.msg" />
      <ArtifactCard v-else-if="group.msg.role === 'artifact'" :artifact="group.msg.content" />
      <div v-else-if="group.msg.role === 'compacted'" class="compact-divider">
        <span class="compact-divider-line"></span>
        <span class="compact-divider-text">💡 对话较长，已自动精简早期内容<span v-if="group.msg.content && group.msg.content.compaction_count >= 2">（第 {{ group.msg.content.compaction_count }} 次，AI 可能记不清前面的细节，新话题建议开个新会话）</span></span>
        <span class="compact-divider-line"></span>
      </div>
    </div>
  </template>
</template>

<script setup>
import { computed } from 'vue'
import UserBubble from './UserBubble.vue'
import AssistantMessage from './AssistantMessage.vue'
import ToolCallCard from './ToolCallCard.vue'
import ToolCallGroup from './ToolCallGroup.vue'
import ArtifactCard from './ArtifactCard.vue'
import ChildAgentCard from './ChildAgentCard.vue'
import SpawnWaitRow from './SpawnWaitRow.vue'
import SkillRow from './SkillRow.vue'
import ExploringCell from './ExploringCell.vue'

const props = defineProps({
  messages: { type: Array, default: () => [] },
})
defineEmits(['preview-image'])

// 是否可归组：shell / 校验 / 注册 / 截图 等关键操作独立展示，
// 其余（检索 / 查询 / 采集 / 搜索等查询类）连续出现时折叠成一组，减少视觉噪音。
function isGroupableTool(m) {
  if (m.role !== 'tool_call') return false
  const n = m.toolName || ''
  if (n === 'shell_command') return false
  if (n === 'edit_file' || n === 'Edit' || n === '编辑文件') return false
  if (n === 'create_file' || n === 'Write' || n === '新建文件') return false
  if (n.includes('校验') || n.includes('注册')) return false
  if (n.toLowerCase().includes('screenshot')) return false
  return true
}

// 纯下载标记命令：shell_command 的 command 剥掉 echo "[[ARTIFACT:...]]" 及裸标记、连接符后为空。
// 这类命令对用户无信息量（只是触发文件卡片），整条不渲染（兜底历史会话残留）。
function isPureArtifactCall(m) {
  if (m.toolName !== 'shell_command') return false
  let cmd = ''
  try {
    const a = typeof m.args === 'string' ? JSON.parse(m.args) : m.args
    cmd = (a && a.command) || ''
  } catch { return false }
  cmd = cmd
    .replace(/echo\s+"?\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]"?/g, '')
    .replace(/\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]/g, '')
  return cmd.replace(/[&;\s]/g, '').length === 0
}

// spawn_agent / wait_agent：多智能体工具，独立扁平行（对齐 codex），不进折叠组
function isSpawnWaitTool(m) {
  const n = m.toolName || ''
  return m.role === 'tool_call' && (n === 'spawn_agent' || n === 'wait_agent')
}

// 探索类工具（对齐 codex exploring cell）：read_file / list_directory / grep，连续聚合成「Exploring」块。
// 工具名兼容 codex 短词(Read/List/Search)与原始英文名；旧历史的中文名(读取文件/列出目录/搜索内容)一并兼容。
const EXPLORE_NAMES = new Set([
  'read_file', 'list_directory', 'grep',
  'Read', 'List', 'Search',
  '读取文件', '列出目录', '搜索内容',
])
function isExploreTool(m) {
  return m.role === 'tool_call' && EXPLORE_NAMES.has(m.toolName || '')
}

// 连续的可归组 tool_call 合并成一个 toolGroup；其余逐条渲染
// origIdx 记录每组首条消息在原始 messages 数组中的下标，供侧边栏滚动定位（id="msg-{origIdx}"）
// spawn/wait：① 进行中不渲染（对齐 codex InProgress→None）② 完成/失败转 spawn_wait 独立行
const grouped = computed(() => {
  const out = []
  let buf = []
  let bufStart = -1
  let exBuf = []
  let exStart = -1
  const flushBuf = () => {
    if (!buf.length) return
    if (buf.length === 1) out.push({ kind: 'single', msg: buf[0], origIdx: bufStart })
    else out.push({ kind: 'toolGroup', items: buf.slice(), origIdx: bufStart })
    buf = []
    bufStart = -1
  }
  const flushEx = () => {
    if (!exBuf.length) return
    out.push({ kind: 'exploring', items: exBuf.slice(), origIdx: exStart })
    exBuf = []
    exStart = -1
  }
  props.messages.forEach((m, i) => {
    // 纯下载标记命令不渲染：shell_command 的命令剥掉 [[ARTIFACT:...]] 及其 echo 后为空
    // （即整条只是 echo 标记）。后端已对纯标记命令整条不发，这里兜底历史会话残留。
    if (m.role === 'tool_call' && isPureArtifactCall(m)) return
    // get_context_remaining：模型内部自查剩余 token 预算的工具，对用户无信息量。
    // codex 把它放状态栏 footer、不在聊天流渲染工具卡（cortex 的 token 用量 footer 已承担此职责），
    // 故聊天流中直接不渲染。
    if (m.role === 'tool_call' && m.toolName === 'get_context_remaining') return
    // read_skill：模型主动拉取 skill 正文注入上下文。cortex 的 skill 是真实工具调用
    // （区别于 codex 的 @提及隐式拉取），完全隐藏会看不到加载了哪个 skill / 是否失败。
    // 渲染成轻量单行 Skill(名字) + 状态（对齐 Claude Code 的 Skill 提示条），不进折叠组。
    if (m.role === 'tool_call' && m.toolName === 'read_skill') {
      flushBuf(); flushEx()
      out.push({ kind: 'single', msg: { ...m, role: 'skill' }, origIdx: i })
      return
    }
    // spawn/wait：进行中跳过不渲染；完成/失败转独立扁平行
    if (isSpawnWaitTool(m)) {
      if (m.status === 'running') return
      flushBuf(); flushEx()
      out.push({ kind: 'single', msg: { ...m, role: 'spawn_wait' }, origIdx: i })
      return
    }
    // 探索类：连续聚合成 exploring 块（对齐 codex exploring cell）
    if (isExploreTool(m)) {
      flushBuf()
      if (!exBuf.length) exStart = i
      exBuf.push(m)
      return
    }
    if (isGroupableTool(m)) {
      flushEx()
      if (!buf.length) bufStart = i
      buf.push(m)
    } else {
      flushEx()
      flushBuf()
      out.push({ kind: 'single', msg: m, origIdx: i })
    }
  })
  flushBuf(); flushEx()
  return out
})
</script>

<style scoped>
/* 消息进场：逐条淡入 + 轻微上浮（delay 由 :style 按序给定，封顶 350ms 防超长会话拖尾），
   历史加载完成后呈现、配合骨架屏收尾。 */
.msg-enter {
  opacity: 0;
  animation: msg-enter-in 0.4s cubic-bezier(0.22, 1, 0.36, 1) forwards;
}
@keyframes msg-enter-in {
  from { opacity: 0; transform: translateY(8px); }
  to { opacity: 1; transform: translateY(0); }
}
@media (prefers-reduced-motion: reduce) {
  .msg-enter { animation-duration: 0.01s; }
}

/* 上下文压缩分隔标记：居中横线 + 提示文字 */
.compact-divider {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  margin: 6px 0;
  user-select: none;
}
.compact-divider-line {
  flex: 1;
  height: 1px;
  background: var(--border, rgba(255,255,255,0.12));
}
.compact-divider-text {
  font-size: 12px;
  color: var(--text);
  white-space: nowrap;
}
</style>
