<template>
  <div class="edit-page">
    <!-- 顶部 -->
    <header class="edit-header">
      <div class="header-left">
        <el-button text :icon="Back" @click="goBack">返回</el-button>
        <h1 class="edit-title">{{ isEdit ? '编辑助手' : '创建助手' }}</h1>
      </div>
      <div class="header-actions">
        <el-button :icon="RefreshLeft" :disabled="loading" @click="resetForm">重置</el-button>
        <el-button type="primary" :icon="Check" :loading="saving" :disabled="loading" @click="save">
          {{ isEdit ? '保存修改' : '创建助手' }}
        </el-button>
      </div>
    </header>

    <div v-loading="loading" class="edit-body">
      <!-- AI 智能生成入口（新建/编辑都可使用） -->
      <div class="ai-generate-banner">
        <div class="banner-icon">✨</div>
        <div class="banner-body">
          <div class="banner-title">AI 智能生成</div>
          <div class="banner-desc">用一句话描述你想要的助手，AI 会自动生成名字、简介、系统提示词和开场白</div>
        </div>
        <el-button type="primary" :icon="MagicStick" @click="openGenerateDialog">开始生成</el-button>
      </div>

      <div class="form-grid">
        <!-- 左列：核心配置 -->
        <div class="col-main">
          <!-- 基础信息 -->
          <section class="form-section">
            <h2 class="section-title">基础信息</h2>
            <el-form label-position="top" class="form">
              <el-form-item label="助手名称" required>
                <el-input
                  v-model="form.name"
                  placeholder="如：网络配置专家"
                  maxlength="32"
                  show-word-limit
                />
              </el-form-item>
              <el-form-item label="头像">
                <div class="avatar-picker">
                  <div
                    v-for="emoji in AVATAR_CHOICES"
                    :key="emoji"
                    class="avatar-chip"
                    :class="{ active: form.avatar === emoji }"
                    @click="form.avatar = emoji"
                  >{{ emoji }}</div>
                </div>
              </el-form-item>
              <el-form-item label="简介">
                <el-input
                  v-model="form.description"
                  type="textarea"
                  :rows="2"
                  placeholder="一句话描述助手能力"
                  maxlength="200"
                  show-word-limit
                />
              </el-form-item>
            </el-form>
          </section>

          <!-- 系统提示词 -->
          <section class="form-section">
            <h2 class="section-title">系统提示词</h2>
            <p class="section-hint">定义助手的角色、能力与回复风格（支持 Markdown）</p>
            <el-input
              v-model="form.system_prompt"
              type="textarea"
              :rows="12"
              placeholder="你是一位资深的网络工程师，擅长华为/思科设备的配置与故障排查…"
              resize="vertical"
              class="prompt-area"
            />
            <div class="prompt-stats">{{ (form.system_prompt || '').length }} / 8000</div>
          </section>

          <!-- 开场白 -->
          <section class="form-section">
            <h2 class="section-title">开场白</h2>
            <p class="section-hint">新会话首次显示的问候语</p>
            <el-input
              v-model="form.greeting"
              type="textarea"
              :rows="2"
              placeholder="你好！我是网络配置专家，请告诉我你的设备型号和需求。"
              maxlength="500"
              show-word-limit
            />
          </section>
        </div>

        <!-- 右列：能力配置 -->
        <div class="col-side">
          <!-- 模型与参数 -->
          <section class="form-section">
            <h2 class="section-title">模型与参数</h2>
            <el-form label-position="top" class="form">
              <el-form-item label="模型">
                <el-select
                  v-model="form.model_id"
                  placeholder="留空使用默认模型"
                  clearable
                  filterable
                  class="full-width"
                >
                  <el-option
                    v-for="m in modelOptions"
                    :key="m.value"
                    :label="m.label"
                    :value="m.value"
                  />
                </el-select>
              </el-form-item>
              <el-form-item label="温度 (Temperature)">
                <div class="slider-row">
                  <el-slider
                    v-model="form.temperature"
                    :min="0" :max="1" :step="0.1"
                    class="slider"
                  />
                  <span class="slider-val">{{ form.temperature != null ? form.temperature.toFixed(1) : '默认' }}</span>
                </div>
              </el-form-item>
              <el-form-item label="Top-P">
                <div class="slider-row">
                  <el-slider
                    v-model="form.top_p"
                    :min="0" :max="1" :step="0.1"
                    class="slider"
                  />
                  <span class="slider-val">{{ form.top_p != null ? form.top_p.toFixed(1) : '默认' }}</span>
                </div>
              </el-form-item>
              <el-form-item label="最大输出 Tokens">
                <el-input-number
                  v-model="form.max_tokens"
                  :min="16384" :max="32768" :step="256"
                  placeholder="默认 16384"
                  class="full-width"
                  controls-position="right"
                />
              </el-form-item>
            </el-form>
          </section>

          <!-- MCP 服务 -->
          <section class="form-section">
            <h2 class="section-title">MCP 服务</h2>
            <p class="section-hint">勾选要挂载到该助手的 MCP 服务，其工具将以 <code>mcp__slug__tool</code> 命名注入</p>
            <div class="tools-list" v-loading="mcpLoading">
              <div
                v-for="s in availableMcpServers"
                :key="s.id"
                class="tool-row"
              >
                <div class="tool-info">
                  <span class="tool-name">{{ s.name }}</span>
                  <span class="tool-desc mcp-endpoint">
                    {{ s.transport === 1 ? 'stdio' : 'http' }} · {{ s.endpoint }}
                  </span>
                </div>
                <el-switch
                  :model-value="form.enabled_mcps.includes(s.id)"
                  @update:model-value="(v) => toggleMcp(s.id, v)"
                />
              </div>
              <el-empty
                v-if="!mcpLoading && !availableMcpServers.length"
                description="暂无可用 MCP 服务"
                :image-size="60"
              >
                <el-button text type="primary" @click="goMcpManage">前往 MCP 服务管理</el-button>
              </el-empty>
            </div>
          </section>

          <!-- 知识库 -->
          <section class="form-section">
            <h2 class="section-title">知识库</h2>
            <p class="section-hint">选择该助手检索时使用的知识库实例（在「知识库管理」页创建 Dify/内置实例）</p>
            <el-select
              v-model="form.kb_instance_id"
              placeholder="不绑定知识库"
              clearable
              filterable
              class="full-width"
            >
              <el-option
                v-for="ins in kbInstances"
                :key="ins.id"
                :label="`${ins.name}（${ins.provider_kind === 1 ? 'Dify' : '内置'}）`"
                :value="ins.id"
              />
            </el-select>
          </section>

          <!-- 可见性 -->
          <section class="form-section">
            <h2 class="section-title">可见性</h2>
            <el-radio-group v-model="form.visibility">
              <el-radio :value="0" border>私有（仅自己可见）</el-radio>
              <el-radio :value="1" border>共享（生成口令后可分享）</el-radio>
            </el-radio-group>
          </section>
        </div>
      </div>
    </div>
  </div>

  <!-- AI 智能生成对话框（template 根级，脱离 edit-page，避免任何祖先容器/loading 干扰输入） -->
  <el-dialog
    v-model="generateDialogVisible"
    title="AI 智能生成助手"
    width="560px"
    append-to-body
    :close-on-click-modal="!generating"
    :close-on-press-escape="!generating"
    :show-close="!generating"
  >
    <div class="gen-dialog-body">
      <p class="gen-tip">
        用自然语言描述你想要的助手用途，比如：
        <span class="gen-example" @click="generatePrompt = '帮我做一个能把 SQL 慢查询翻译成优化建议的助手'">
          "把 SQL 慢查询翻译成优化建议"
        </span>
      </p>
      <el-input
        v-model="generatePrompt"
        type="textarea"
        :rows="4"
        maxlength="500"
        show-word-limit
        placeholder="例如：帮我做一个能审阅 Python 代码风格并给出改进建议的助手"
        :disabled="generating"
      />
      <p v-if="generating" class="gen-loading">
        <el-icon class="is-loading"><Loading /></el-icon>
        正在生成，通常需要 5-15 秒...
      </p>
    </div>
    <template #footer>
      <el-button :disabled="generating" @click="generateDialogVisible = false">取消</el-button>
      <el-button
        type="primary"
        :icon="MagicStick"
        :loading="generating"
        :disabled="!generatePrompt.trim()"
        @click="doGenerate"
      >{{ generating ? '生成中...' : '开始生成' }}</el-button>
    </template>
  </el-dialog>
</template>

<script setup>
import { ref, reactive, computed, onMounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Back, Check, Loading, MagicStick, RefreshLeft } from '@element-plus/icons-vue'
import { useAssistantStore } from '../stores/assistant'
import { useAppStore } from '../stores/app'
import { AVATAR_CHOICES } from '../utils/assistantEnums'
import { fetchMcpServers, generateAssistantDraft, fetchKbInstances } from '../api'

const route = useRoute()
const router = useRouter()
const assistantStore = useAssistantStore()
const appStore = useAppStore()

const assistantId = computed(() => route.params.id)
const isEdit = computed(() => !!assistantId.value)

const loading = ref(false)
const saving = ref(false)

// AI 智能生成对话框状态
const generateDialogVisible = ref(false)
const generatePrompt = ref('')
const generating = ref(false)
const mcpLoading = ref(false)
const availableMcpServers = ref([])
const kbInstances = ref([])

async function loadKbInstances() {
  try {
    const { data, code } = await fetchKbInstances()
    if (code === 0) kbInstances.value = data.instances || []
  } catch (_) {
    kbInstances.value = []
  }
}

// 自定义助手默认固定启用「命令执行 shell_command」工具（不再开放工具选择）
const DEFAULT_ENABLED_TOOLS = ['shell_command']
// 温度 / top_p 默认值（业界通用推荐：平衡确定性与创造性；范围 0~1 对绝大多数模型安全）
const DEFAULT_TEMPERATURE = 0.7
const DEFAULT_TOP_P = 0.9

const form = reactive({
  name: '',
  avatar: '🤖',
  description: '',
  system_prompt: '',
  greeting: '',
  model_id: '',
  temperature: DEFAULT_TEMPERATURE,
  top_p: DEFAULT_TOP_P,
  max_tokens: null,
  enabled_tools: [...DEFAULT_ENABLED_TOOLS],
  enabled_mcps: [],
  kb_instance_id: '',
  visibility: 0,
})

const modelOptions = computed(() => {
  const list = appStore.models || []
  return list.map((m) => {
    if (typeof m === 'string') return { value: m, label: m }
    return { value: m.id || m.model_id || m.name, label: m.name || m.id || '未命名模型' }
  })
})

function applyAssistant(a) {
  Object.assign(form, {
    name: a.name || '',
    avatar: a.avatar || '🤖',
    description: a.description || '',
    system_prompt: a.system_prompt || '',
    greeting: a.greeting || '',
    model_id: a.model_id || '',
    temperature: a.temperature ?? DEFAULT_TEMPERATURE,
    top_p: a.top_p ?? DEFAULT_TOP_P,
    max_tokens: a.max_tokens ?? null,
    // 不沿用历史 enabled_tools：自定义助手工具能力固定为 shell_command
    enabled_tools: [...DEFAULT_ENABLED_TOOLS],
    enabled_mcps: Array.isArray(a.enabled_mcps) ? [...a.enabled_mcps] : [],
    kb_instance_id: a.kb_instance_id || '',
    visibility: a.visibility ?? 0,
  })
}

async function loadMcpServers() {
  mcpLoading.value = true
  try {
    const { data, code } = await fetchMcpServers()
    if (code === 0) {
      availableMcpServers.value = (data.servers || []).filter(s => s.status === 1)
      // 清理已绑定但不存在的 / 已禁用的 MCP（仅编辑态）
      if (isEdit.value) {
        const validIds = new Set(availableMcpServers.value.map(s => s.id))
        form.enabled_mcps = form.enabled_mcps.filter(id => validIds.has(id))
      }
    }
  } catch (_) {
    availableMcpServers.value = []
  } finally {
    mcpLoading.value = false
  }
}

async function loadFormData() {
  if (isEdit.value) {
    const a = await assistantStore.loadAssistant(assistantId.value)
    if (a) {
      applyAssistant(a)
    } else {
      ElMessage.error('助手不存在')
      router.replace('/assistants')
    }
  }
}

onMounted(async () => {
  loading.value = true
  if (!appStore.models.length) appStore.loadModels()
  try {
    await loadFormData()
    await loadMcpServers()
    await loadKbInstances()
  } finally {
    loading.value = false
  }
})

function toggleMcp(id, enabled) {
  if (enabled) {
    if (!form.enabled_mcps.includes(id)) form.enabled_mcps.push(id)
  } else {
    form.enabled_mcps = form.enabled_mcps.filter((k) => k !== id)
  }
}

function goMcpManage() {
  router.push('/mcp-servers')
}

async function resetForm() {
  if (isEdit.value) {
    loading.value = true
    try {
      await loadFormData()
    } finally {
      loading.value = false
    }
  } else {
    Object.assign(form, {
      name: '', avatar: '🤖', description: '', system_prompt: '', greeting: '',
      model_id: '', temperature: DEFAULT_TEMPERATURE, top_p: DEFAULT_TOP_P, max_tokens: null,
      enabled_tools: [...DEFAULT_ENABLED_TOOLS], enabled_mcps: [], kb_instance_id: '', visibility: 0,
    })
  }
}

function buildPayload() {
  return {
    name: form.name.trim(),
    description: form.description.trim(),
    avatar: form.avatar,
    system_prompt: form.system_prompt,
    greeting: form.greeting,
    model_id: form.model_id || '',
    temperature: form.temperature != null ? Number(form.temperature.toFixed(1)) : null,
    top_p: form.top_p != null ? Number(form.top_p.toFixed(1)) : null,
    max_tokens: form.max_tokens,
    // 工具能力固定为 shell_command（忽略表单残留，确保覆盖历史数据）
    enabled_tools: [...DEFAULT_ENABLED_TOOLS],
    enabled_mcps: form.enabled_mcps,
    kb_instance_id: form.kb_instance_id || null,
    visibility: form.visibility,
  }
}

async function save() {
  if (!form.name.trim()) {
    ElMessage.warning('请填写助手名称')
    return
  }
  if ((form.system_prompt || '').length > 8000) {
    ElMessage.warning('系统提示词不能超过 8000 字')
    return
  }
  saving.value = true
  try {
    const payload = buildPayload()
    if (isEdit.value) {
      await assistantStore.update(assistantId.value, payload)
      ElMessage.success('已保存')
    } else {
      await assistantStore.create(payload)
      ElMessage.success('创建成功')
    }
    // 保存成功统一跳回助手列表页
    router.push('/assistants')
  } catch (e) {
    ElMessage.error(e.message)
  } finally {
    saving.value = false
  }
}

function goBack() {
  router.push('/assistants')
}

function openGenerateDialog() {
  generatePrompt.value = ''
  generateDialogVisible.value = true
}

async function doGenerate() {
  const prompt = generatePrompt.value.trim()
  if (!prompt) {
    ElMessage.warning('请描述你想要的助手')
    return
  }
  generating.value = true
  try {
    // 传入当前选中的模型（若已选），未选则用系统默认模型
    const { data, code, message } = await generateAssistantDraft(prompt, form.model_id || null)
    if (code !== 0 || !data) {
      throw new Error(message || '生成失败，请稍后重试')
    }
    // 填充表单（覆盖已有内容，用户可再编辑）
    form.name = data.name || form.name
    form.description = data.description || form.description
    form.system_prompt = data.system_prompt || form.system_prompt
    form.greeting = data.greeting || form.greeting
    ElMessage.success('已生成，可继续编辑后保存')
    generateDialogVisible.value = false
  } catch (e) {
    ElMessage.error(e.message || '生成失败')
  } finally {
    generating.value = false
  }
}
</script>

<style scoped>
.edit-page { padding: 20px 28px; max-width: 1280px; margin: 0 auto; }
.edit-header {
  display: flex; align-items: center; justify-content: space-between;
  margin-bottom: 20px; gap: 16px;
}
.header-left { display: flex; align-items: center; gap: 12px; }
.edit-title { font-size: 20px; font-weight: 800; color: var(--text-h); margin: 0; }
.header-actions { display: flex; gap: 10px; }

.edit-body { min-height: 400px; }

/* AI 智能生成 banner */
.ai-generate-banner {
  display: flex; align-items: center; gap: 14px;
  padding: 14px 18px; margin-bottom: 16px;
  border-radius: 10px;
  background: linear-gradient(135deg, rgba(99, 102, 241, 0.08), rgba(168, 85, 247, 0.08));
  border: 1px solid var(--accent);
}
.banner-icon { font-size: 28px; flex-shrink: 0; }
.banner-body { flex: 1; min-width: 0; }
.banner-title { font-size: 14px; font-weight: 600; color: var(--text-h); margin-bottom: 2px; }
.banner-desc { font-size: 12px; color: var(--muted); }

/* 生成对话框 */
.gen-dialog-body { display: flex; flex-direction: column; gap: 12px; }
.gen-tip { font-size: 13px; color: var(--muted); margin: 0; line-height: 1.6; }
.gen-example {
  color: var(--accent); cursor: pointer;
  border-bottom: 1px dashed var(--accent);
}
.gen-loading {
  display: flex; align-items: center; gap: 6px;
  font-size: 13px; color: var(--accent); margin: 4px 0 0;
}
.form-grid { display: grid; grid-template-columns: 1fr 380px; gap: 20px; align-items: start; }
@media (max-width: 980px) { .form-grid { grid-template-columns: 1fr; } }

.form-section {
  background: var(--card); border: 1px solid var(--border); border-radius: 12px;
  padding: 18px; margin-bottom: 16px;
}
.section-title { font-size: 14px; font-weight: 700; color: var(--text-h); margin: 0 0 12px; display: flex; align-items: center; gap: 8px; }
.section-optional {
  font-size: 10px; font-weight: 500; color: var(--muted);
  padding: 1px 6px; border-radius: 4px; background: var(--bg-elevated); border: 1px solid var(--border);
}
.section-hint { font-size: 12px; color: var(--muted); margin: -8px 0 12px; }
.form :deep(.el-form-item__label) { font-size: 13px; color: var(--muted-light); padding-bottom: 4px; }
.full-width { width: 100%; }

.avatar-picker { display: flex; flex-wrap: wrap; gap: 8px; }
.avatar-chip {
  width: 38px; height: 38px; border-radius: 10px; cursor: pointer; font-size: 20px;
  display: flex; align-items: center; justify-content: center;
  border: 1px solid var(--border); background: var(--bg-elevated); transition: all .15s;
}
.avatar-chip:hover { border-color: var(--accent); }
.avatar-chip.active {
  border-color: var(--accent); background: var(--accent-dim);
  box-shadow: 0 0 0 2px var(--border-glow);
}

.prompt-area :deep(textarea) { font-family: var(--font-mono); font-size: 13px; line-height: 1.6; }
.prompt-stats { text-align: right; font-size: 12px; color: var(--muted); margin-top: 6px; }

.slider-row { display: flex; align-items: center; gap: 14px; width: 100%; }
.slider { flex: 1; }
.slider-val {
  font-size: 12px; color: var(--accent); font-family: var(--font-mono);
  min-width: 40px; text-align: right;
}

.tools-list { display: flex; flex-direction: column; gap: 10px; margin-bottom: 16px; }
.tool-row {
  display: flex; align-items: center; justify-content: space-between;
  padding: 10px 12px; border-radius: 8px; background: var(--bg-elevated);
  border: 1px solid var(--border);
}
.tool-info { display: flex; flex-direction: column; gap: 2px; }
.tool-name { font-size: 13px; font-weight: 600; color: var(--text-h); }
.tool-desc { font-size: 11px; color: var(--muted); }
.mcp-endpoint {
  display: block;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: var(--font-mono);
}

:deep(.el-radio.is-bordered) { width: 100%; margin-right: 0; margin-bottom: 8px; }
</style>
