<template>
  <div class="edit-page">
    <!-- 顶部 -->
    <header class="edit-header">
      <div class="header-left">
        <el-button text :icon="Back" @click="goBack">返回</el-button>
        <h1 class="edit-title">{{ isEdit ? '编辑助手' : '创建助手' }}</h1>
      </div>
      <div class="header-actions">
        <el-button :icon="RefreshLeft" :disabled="loading || loadFailed" @click="resetForm">重置</el-button>
        <el-button type="primary" :icon="Check" :loading="saving" :disabled="loading || loadFailed" @click="save">
          {{ isEdit ? '保存修改' : '创建助手' }}
        </el-button>
      </div>
    </header>

    <div v-loading="loading" class="edit-body">
      <!-- 步骤条：可自由跳步（点击切换到任意步骤） -->
      <el-steps :active="currentStep" align-center finish-status="success" class="wizard-steps">
        <el-step
          v-for="(s, i) in STEPS"
          :key="i"
          :title="s.title"
          :description="s.desc"
          class="wizard-step"
          @click="goStep(i)"
        />
      </el-steps>

      <!-- ============ Step 1 基础与提示词 ============ -->
      <div v-show="currentStep === 0" class="wizard-panel">
        <!-- AI 智能生成入口（新建/编辑都可使用） -->
        <div class="ai-generate-banner">
          <div class="banner-icon">✨</div>
          <div class="banner-body">
            <div class="banner-title">AI 智能生成</div>
            <div class="banner-desc">用一句话描述你想要的助手，AI 会自动生成名字、简介、系统提示词和开场白</div>
          </div>
          <el-button type="primary" :icon="MagicStick" @click="openGenerateDialog">开始生成</el-button>
        </div>

        <section class="form-section">
          <h2 class="section-title">基础信息</h2>
          <el-form label-position="top" class="form">
            <div class="name-avatar-row">
              <el-form-item label="助手名称" required class="name-item">
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
            </div>
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

      <!-- ============ Step 2 能力挂载 ============ -->
      <div v-show="currentStep === 1" class="wizard-panel">
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

        <!-- 可用 Skill -->
        <section class="form-section">
          <h2 class="section-title">
            可用 Skill
            <el-tag size="small" type="info" effect="plain" class="section-tag">硬隔离</el-tag>
          </h2>
          <p class="section-hint">
            不勾选任何项表示<b>不限制</b>（全部 Skill 可见）。一旦勾选，仅列出的 Skill 对该助手可见——
            模型在目录、<code>read_skill</code> 工具和 <code>$提及</code> 中都无法触及未勾选的 Skill。
          </p>
          <div class="tools-list" v-loading="skillLoading">
            <div
              v-for="s in availableSkills"
              :key="s.name"
              class="tool-row"
            >
              <div class="tool-info">
                <span class="tool-name">
                  <code>{{ s.name }}</code>
                  <el-tag
                    :type="s.scope === 'builtin' ? 'info' : 'success'"
                    size="small"
                    class="scope-tag"
                  >{{ s.scope === 'builtin' ? '内置' : '用户' }}</el-tag>
                </span>
                <span class="tool-desc">{{ s.description || '（无描述）' }}</span>
              </div>
              <el-switch
                :model-value="form.enabled_skills.includes(s.name)"
                @update:model-value="(v) => toggleSkill(s.name, v)"
              />
            </div>
            <el-empty
              v-if="!skillLoading && !availableSkills.length"
              description="暂无可用 Skill"
              :image-size="60"
            >
              <el-button text type="primary" @click="goSkillManage">前往 Skill 目录</el-button>
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
      </div>

      <!-- ============ Step 3 模型与高级 ============ -->
      <div v-show="currentStep === 2" class="wizard-panel">
        <div class="two-col">
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

          <!-- 可见性 -->
          <section class="form-section">
            <h2 class="section-title">可见性</h2>
            <el-radio-group v-model="form.visibility">
              <el-radio :value="0" border>私有（仅自己可见）</el-radio>
              <el-radio :value="1" border>共享（生成口令后可分享）</el-radio>
            </el-radio-group>
          </section>
        </div>

        <!-- 环境变量 -->
        <section class="form-section">
          <h2 class="section-title">
            环境变量
            <el-tag v-if="isEdit && !envUnlocked" size="small" type="info" effect="plain">
              已加密 · 值已隐藏
            </el-tag>
          </h2>
          <p class="section-hint">
            会话执行命令/脚本时注入子进程环境，供 skill 脚本经 <code>os.environ['KEY']</code> 读取。
            加密存储；可能含密钥——不会出现在共享/导出/Fork 的副本中。
          </p>

          <!-- 编辑态且未解锁：脱敏只读 + 解锁按钮 -->
          <div v-if="isEdit && !envUnlocked" class="env-locked">
            <div v-for="item in form.env_vars" :key="item.id" class="env-row env-row-readonly">
              <span class="env-key-text">{{ item.key }}</span>
              <span class="env-val-text">{{ item.value }}</span>
            </div>
            <el-empty
              v-if="!form.env_vars.length"
              description="未配置环境变量"
              :image-size="50"
            />
            <el-button
              :icon="Unlock"
              type="primary"
              plain
              class="env-unlock-btn"
              @click="openEnvUnlock"
            >
              验证密码查看并编辑
            </el-button>
            <p class="env-locked-tip">为保护密钥，查看明文需再次输入你的登录密码。未解锁直接保存不会改动现有环境变量。</p>
          </div>

          <!-- 新建态 或 已解锁：完整编辑器 -->
          <div v-else class="env-list">
            <div v-for="(item, idx) in form.env_vars" :key="item.id" class="env-row">
              <el-input
                v-model="item.key"
                placeholder="KEY"
                class="env-key"
                :class="{ 'is-error': item.key && !isValidEnvKey(item.key) }"
              />
              <el-input
                v-model="item.value"
                placeholder="value"
                class="env-val"
              />
              <el-button :icon="Delete" text class="env-del" @click="removeEnvVar(idx)" />
            </div>
            <el-button :icon="Plus" text type="primary" class="env-add" @click="addEnvVar">
              添加环境变量
            </el-button>
            <el-empty
              v-if="!form.env_vars.length"
              description="未配置环境变量"
              :image-size="50"
            />
          </div>
        </section>
      </div>

      <!-- 底部向导操作 -->
      <div class="wizard-footer">
        <el-button v-if="currentStep > 0" :icon="ArrowLeft" @click="goStep(currentStep - 1)">上一步</el-button>
        <span v-else />
        <div class="wizard-footer-right">
          <el-button
            v-if="currentStep < STEPS.length - 1"
            type="primary"
            @click="nextStep"
          >下一步 <el-icon><ArrowRight /></el-icon></el-button>
          <el-button
            v-else
            type="primary"
            :icon="Check"
            :loading="saving"
            :disabled="loading || loadFailed"
            @click="save"
          >{{ isEdit ? '保存修改' : '创建助手' }}</el-button>
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

  <!-- 环境变量解锁对话框（验证登录密码后显示明文） -->
  <el-dialog
    v-model="envUnlockVisible"
    title="验证密码查看环境变量"
    width="420px"
    append-to-body
    :close-on-click-modal="!envRevealing"
    :close-on-press-escape="!envRevealing"
    :show-close="!envRevealing"
  >
    <div class="env-unlock-body">
      <p class="env-unlock-tip">为保护密钥，查看环境变量明文需再次输入你的登录密码。</p>
      <el-input
        v-model="envPassword"
        type="password"
        show-password
        placeholder="登录密码"
        :disabled="envRevealing"
        @keyup.enter="confirmEnvUnlock"
      />
    </div>
    <template #footer>
      <el-button :disabled="envRevealing" @click="envUnlockVisible = false">取消</el-button>
      <el-button
        type="primary"
        :loading="envRevealing"
        :disabled="!envPassword"
        @click="confirmEnvUnlock"
      >确认</el-button>
    </template>
  </el-dialog>
</template>

<script setup>
import { ref, reactive, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { ArrowLeft, ArrowRight, Back, Check, Delete, Loading, MagicStick, Plus, RefreshLeft, Unlock } from '@element-plus/icons-vue'
import { useAssistantStore } from '../stores/assistant'
import { useAppStore } from '../stores/app'
import { AVATAR_CHOICES } from '../utils/assistantEnums'
import { fetchMcpServers, fetchSkills, generateAssistantDraft, fetchKbInstances, revealAssistantEnvVars } from '../api'

const route = useRoute()
const router = useRouter()
const assistantStore = useAssistantStore()
const appStore = useAppStore()

const assistantId = computed(() => route.params.id)
const isEdit = computed(() => !!assistantId.value)

const loading = ref(false)
const saving = ref(false)
// 编辑态详情加载失败（网络异常）：留在页面但禁用保存/重置，
// 防止在空表单上误点保存把线上助手数据清掉
const loadFailed = ref(false)

// 分步向导：可自由跳步
const STEPS = [
  { title: '基础与提示词', desc: '名称 / 提示词 / 开场白' },
  { title: '能力挂载', desc: 'MCP / Skill / 知识库' },
  { title: '模型与高级', desc: '模型参数 / 可见性 / 环境变量' },
]
const currentStep = ref(0)

function goStep(i) {
  if (i >= 0 && i < STEPS.length) currentStep.value = i
}

function nextStep() {
  // 第一步名称必填，否则不放行到后续步骤
  if (currentStep.value === 0 && !form.name.trim()) {
    ElMessage.warning('请先填写助手名称')
    return
  }
  goStep(currentStep.value + 1)
}

// AI 智能生成对话框状态
const generateDialogVisible = ref(false)
const generatePrompt = ref('')
const generating = ref(false)
const mcpLoading = ref(false)
const availableMcpServers = ref([])
const skillLoading = ref(false)
const availableSkills = ref([])
const kbInstances = ref([])

// 环境变量解锁（编辑态默认锁定脱敏，验证密码后显示明文并允许编辑）
const envUnlocked = ref(false)
const envUnlockVisible = ref(false)
const envPassword = ref('')
const envRevealing = ref(false)

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

// 环境变量键名：首字符字母/下划线，其余字母/数字/下划线（与后端 is_valid_env_key 一致）
const ENV_KEY_RE = /^[A-Za-z_][A-Za-z0-9_]*$/
// 上限与后端一致（assistant.rs: MAX_ENV_VARS/MAX_ENV_KEY_LEN/MAX_ENV_VALUE_LEN）
const MAX_ENV_VARS = 64
const MAX_ENV_KEY_LEN = 128
const MAX_ENV_VALUE_LEN = 8192

// env 行自增 id：v-for 用稳定 key（数组索引会因删中间行导致校验样式/输入状态错位）
let envRowSeq = 0
function makeEnvRow(key = '', value = '') {
  return { id: ++envRowSeq, key, value }
}

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
  // 可用 Skill 白名单（存 skill name）；空数组=不限制=全部可见（硬隔离）
  enabled_skills: [],
  kb_instance_id: '',
  visibility: 0,
  // 环境变量：编辑态用 [{key,value}] 数组（便于增删行），保存/读取时与后端对象互转
  env_vars: [],
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
    enabled_skills: Array.isArray(a.enabled_skills) ? [...a.enabled_skills] : [],
    kb_instance_id: a.kb_instance_id || '',
    visibility: a.visibility ?? 0,
    // 后端对象 {"K":"V"} → 编辑数组 [{key,value}]（编辑态值已脱敏为掩码，解锁后才显示明文）
    env_vars: a.env_vars && typeof a.env_vars === 'object'
      ? Object.entries(a.env_vars).map(([k, v]) => makeEnvRow(k, String(v ?? '')))
      : [],
  })
  // 编辑态加载后重新锁定（脱敏），需验证密码才能查看/编辑明文
  envUnlocked.value = false
}

async function loadMcpServers() {
  mcpLoading.value = true
  try {
    // 拉全量 MCP：绑定校验需覆盖所有页（后端 page_size 上限 100），只拉默认首页
    //（10 条）时，第 11 条起已绑定的 server 不在返回里，会被当成无效绑定清掉，
    // 保存时静默解绑。翻页拉到 total 为止（上限 50 页兜底防死循环）
    const PAGE_SIZE = 100
    const all = []
    let page = 1
    // 是否拉到了全量：中途某页业务失败（code!==0）时列表是残缺的，拿它清绑定会把
    // 落在未拉到页里的合法绑定静默剔掉（保存即解绑），必须整体放弃本次结果
    let complete = false
    while (page <= 50) {
      const { data, code } = await fetchMcpServers(page, PAGE_SIZE)
      if (code !== 0) break
      const servers = data.servers || []
      all.push(...servers)
      if (all.length >= (data.total || 0) || servers.length < PAGE_SIZE) {
        complete = true
        break
      }
      page++
    }
    if (complete) {
      availableMcpServers.value = all.filter(s => s.status === 1)
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

async function loadSkills() {
  skillLoading.value = true
  try {
    const { data, code } = await fetchSkills()
    if (code === 0) {
      availableSkills.value = (data && data.skills) || []
      // 清理白名单中已不存在的 skill 名（仅编辑态；容错后续删除，不报错）
      if (isEdit.value) {
        const validNames = new Set(availableSkills.value.map(s => s.name))
        form.enabled_skills = form.enabled_skills.filter(n => validNames.has(n))
      }
    }
  } catch (_) {
    availableSkills.value = []
  } finally {
    skillLoading.value = false
  }
}

async function loadFormData() {
  if (isEdit.value) {
    // 网络失败（throw）与不存在（null）分开处理：失败时留在页面提示重试，
    // 不能把用户已填写的表单直接踢回列表（旧逻辑把两者混为一谈）
    let a = null
    try {
      a = await assistantStore.loadAssistant(assistantId.value)
    } catch (e) {
      loadFailed.value = true
      ElMessage.error('加载助手失败: ' + (e.message || '网络异常'))
      return
    }
    loadFailed.value = false
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
    await loadSkills()
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

function toggleSkill(name, enabled) {
  if (enabled) {
    if (!form.enabled_skills.includes(name)) form.enabled_skills.push(name)
  } else {
    form.enabled_skills = form.enabled_skills.filter((k) => k !== name)
  }
}

function goMcpManage() {
  router.push('/mcp-servers')
}

function goSkillManage() {
  router.push('/skills')
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
      enabled_tools: [...DEFAULT_ENABLED_TOOLS], enabled_mcps: [], enabled_skills: [],
      kb_instance_id: '', visibility: 0,
      env_vars: [],
    })
  }
}

function buildPayload() {
  // 环境变量：
  //   - 编辑态且未解锁 → null（保持原值，后端跳过该列；掩码绝不能当真实值回写）
  //   - 否则（新建态 / 已解锁）→ 对象，丢弃空键，保留空值（env 允许空值）
  let env_vars = null
  if (!isEdit.value || envUnlocked.value) {
    env_vars = {}
    for (const { key, value } of form.env_vars) {
      const k = (key || '').trim()
      if (k) env_vars[k] = value ?? ''
    }
  }
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
    enabled_skills: form.enabled_skills,
    kb_instance_id: form.kb_instance_id || null,
    visibility: form.visibility,
    env_vars,
  }
}

function isValidEnvKey(k) {
  return ENV_KEY_RE.test(k || '')
}

function addEnvVar() {
  form.env_vars.push(makeEnvRow())
}

function removeEnvVar(idx) {
  form.env_vars.splice(idx, 1)
}

function openEnvUnlock() {
  envPassword.value = ''
  envUnlockVisible.value = true
}

async function confirmEnvUnlock() {
  const pwd = envPassword.value
  if (!pwd) return
  envRevealing.value = true
  try {
    const { data, code, message } = await revealAssistantEnvVars(assistantId.value, pwd)
    if (code !== 0 || !data) {
      throw new Error(message || '密码错误')
    }
    // 用明文替换掩码值，保持键顺序；解锁后允许编辑
    const plain = data.env_vars && typeof data.env_vars === 'object' ? data.env_vars : {}
    form.env_vars = Object.entries(plain).map(([k, v]) => makeEnvRow(k, String(v ?? '')))
    envUnlocked.value = true
    envUnlockVisible.value = false
    ElMessage.success('已解锁，可查看并编辑环境变量')
  } catch (e) {
    ElMessage.error(e.message || '密码错误')
  } finally {
    envRevealing.value = false
    // 用后即清：登录密码不留在 Vue 响应式状态里（防 devtools/堆快照/XSS 扩大暴露）
    envPassword.value = ''
  }
}

async function save() {
  if (!form.name.trim()) {
    ElMessage.warning('请填写助手名称')
    currentStep.value = 0
    return
  }
  if ((form.system_prompt || '').length > 8000) {
    ElMessage.warning('系统提示词不能超过 8000 字')
    currentStep.value = 0
    return
  }
  // 环境变量校验（键名 / 长度 / 数量 / 去重——与后端一致，前端先拦避免无效往返）
  const envEditable = !isEdit.value || envUnlocked.value
  if (envEditable) {
    const rows = form.env_vars
      .map((r) => ({ ...r, key: (r.key || '').trim() }))
      .filter((r) => r.key)
    if (rows.length > MAX_ENV_VARS) {
      ElMessage.warning(`环境变量数量超过上限 ${MAX_ENV_VARS}`)
      currentStep.value = 2
      return
    }
    const seen = new Set()
    for (const { key, value } of rows) {
      if (!isValidEnvKey(key)) {
        ElMessage.warning(`非法的环境变量名: ${key}（仅允许字母/数字/下划线，首字符须字母或下划线）`)
        currentStep.value = 2
        return
      }
      if (key.length > MAX_ENV_KEY_LEN) {
        ElMessage.warning(`环境变量名 ${key} 过长（上限 ${MAX_ENV_KEY_LEN} 字符）`)
        currentStep.value = 2
        return
      }
      if ((value || '').length > MAX_ENV_VALUE_LEN) {
        ElMessage.warning(`环境变量 ${key} 的值过长（上限 ${MAX_ENV_VALUE_LEN} 字符）`)
        currentStep.value = 2
        return
      }
      if (seen.has(key)) {
        ElMessage.warning(`环境变量名重复: ${key}（同名只保留最后一个，请改成唯一键）`)
        currentStep.value = 2
        return
      }
      seen.add(key)
    }
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
.edit-page { padding: 20px 28px; max-width: 960px; margin: 0 auto; }
.edit-header {
  display: flex; align-items: center; justify-content: space-between;
  margin-bottom: 20px; gap: 16px;
}
.header-left { display: flex; align-items: center; gap: 12px; }
.edit-title { font-size: 20px; font-weight: 800; color: var(--text-h); margin: 0; }
.header-actions { display: flex; gap: 10px; }

.edit-body { min-height: 400px; }

/* 步骤条 */
.wizard-steps {
  margin-bottom: 24px;
  padding: 18px 12px;
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: 12px;
}
.wizard-step { cursor: pointer; }
.wizard-step :deep(.el-step__title) { font-size: 14px; font-weight: 600; }
.wizard-step :deep(.el-step__description) { font-size: 11px; }

.wizard-panel { animation: fadeIn .2s ease; }
@keyframes fadeIn { from { opacity: 0; transform: translateY(4px); } to { opacity: 1; transform: none; } }

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

.form-section {
  background: var(--card); border: 1px solid var(--border); border-radius: 12px;
  padding: 18px; margin-bottom: 16px;
}
.section-title { font-size: 14px; font-weight: 700; color: var(--text-h); margin: 0 0 12px; display: flex; align-items: center; gap: 8px; }
.section-tag { font-weight: 500; }
.section-hint { font-size: 12px; color: var(--muted); margin: -8px 0 12px; line-height: 1.6; }
.section-hint code { color: var(--accent); }
.form :deep(.el-form-item__label) { font-size: 13px; color: var(--muted-light); padding-bottom: 4px; }
.full-width { width: 100%; }

/* 名称+头像同行 */
.name-avatar-row { display: grid; grid-template-columns: 1fr auto; gap: 20px; align-items: start; }
@media (max-width: 720px) { .name-avatar-row { grid-template-columns: 1fr; } }

.avatar-picker { display: flex; flex-wrap: wrap; gap: 8px; max-width: 280px; }
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

/* Step3 模型参数 + 可见性 双栏 */
.two-col { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; align-items: start; }
@media (max-width: 860px) { .two-col { grid-template-columns: 1fr; } }

.tools-list { display: flex; flex-direction: column; gap: 10px; margin-bottom: 4px; }
.tool-row {
  display: flex; align-items: center; justify-content: space-between;
  padding: 10px 12px; border-radius: 8px; background: var(--bg-elevated);
  border: 1px solid var(--border); gap: 12px;
}
.tool-info { display: flex; flex-direction: column; gap: 3px; min-width: 0; flex: 1; }
.tool-name { font-size: 13px; font-weight: 600; color: var(--text-h); display: flex; align-items: center; gap: 8px; }
.tool-name code { font-family: var(--font-mono); }
.scope-tag { flex-shrink: 0; }
.tool-desc { font-size: 11px; color: var(--muted); line-height: 1.5; }
.mcp-endpoint {
  display: block;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: var(--font-mono);
}

:deep(.el-radio.is-bordered) { width: 100%; margin-right: 0; margin-bottom: 8px; }

/* 底部向导操作 */
.wizard-footer {
  display: flex; align-items: center; justify-content: space-between;
  padding: 14px 18px; margin-top: 4px;
  background: var(--card); border: 1px solid var(--border); border-radius: 12px;
}
.wizard-footer-right { display: flex; gap: 10px; }

/* 环境变量编辑器 */
.env-list { display: flex; flex-direction: column; gap: 8px; }
.env-row {
  display: flex; align-items: center; gap: 8px;
}
.env-row .env-key { flex: 0 0 38%; }
.env-row .env-val { flex: 1; min-width: 0; }
.env-row .env-del { flex: 0 0 auto; color: var(--muted); }
.env-add { align-self: flex-start; }
.env-key :deep(.el-input__wrapper).is-error,
.env-key.is-error :deep(.el-input__wrapper) {
  box-shadow: 0 0 0 1px var(--el-color-danger) inset;
}
.env-key :deep(input),
.env-val :deep(input) {
  font-family: var(--font-mono); font-size: 12px;
}

/* 环境变量锁定态（脱敏只读） */
.env-locked { display: flex; flex-direction: column; gap: 8px; }
.env-row-readonly {
  padding: 8px 10px; border-radius: 8px;
  background: var(--bg-elevated); border: 1px solid var(--border);
}
.env-key-text {
  font-family: var(--font-mono); font-size: 12px; font-weight: 600;
  color: var(--text-h); flex: 0 0 38%; overflow: hidden; text-overflow: ellipsis;
}
.env-val-text {
  font-family: var(--font-mono); font-size: 12px; color: var(--muted);
  flex: 1; min-width: 0; letter-spacing: 2px;
}
.env-unlock-btn { align-self: flex-start; margin-top: 4px; }
.env-locked-tip { font-size: 11px; color: var(--muted); margin: 4px 0 0; line-height: 1.5; }
.env-unlock-body { display: flex; flex-direction: column; gap: 12px; }
.env-unlock-tip { font-size: 13px; color: var(--muted); margin: 0; line-height: 1.5; }
</style>
