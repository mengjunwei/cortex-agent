import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
  fetchAssistants,
  fetchAssistant,
  createAssistant,
  updateAssistant,
  deleteAssistant,
  duplicateAssistant,
  fetchExploreAssistants,
  enableShare,
  disableShare,
  forkAssistant,
  exportAssistant,
  importAssistant,
  fetchTools,
} from '../api'

export const useAssistantStore = defineStore('assistant', () => {
  // ── 列表态 ──
  const assistants = ref([])
  const loading = ref(false)

  // ── 广场态 ──
  const exploreList = ref([])
  const exploreLoading = ref(false)

  // ── 工具注册表（可勾选项） ──
  const tools = ref([])
  const toolsLoaded = ref(false)

  // ── 当前选中的助手（新建会话 / 对话页使用） ──
  const currentAssistantId = ref(null)

  const builtinAssistants = computed(() => assistants.value.filter((a) => a.kind === 0))
  const customAssistants = computed(() => assistants.value.filter((a) => a.kind === 1))
  const currentAssistant = computed(() =>
    assistants.value.find((a) => a.id === currentAssistantId.value) || null,
  )

  /** 加载全部助手（内置 + 自定义） */
  async function loadAssistants() {
    loading.value = true
    try {
      const { data, code } = await fetchAssistants()
      if (code === 0) {
        assistants.value = data.assistants || []
      }
    } catch (_) {
      // 静默：DB 未启用时后端返回 503，列表置空
      assistants.value = []
    } finally {
      loading.value = false
    }
  }

  /** 加载单个助手详情：不存在返回 null；网络/服务异常向上抛（调用方需区分
   *  「助手没了该退出编辑页」与「请求失败应留在页面重试」，二者不能混为一谈） */
  async function loadAssistant(id) {
    const { data, code, message } = await fetchAssistant(id)
    if (code === 0 && data.assistant) {
      return data.assistant
    }
    // 明确的「不存在」(NOT_FOUND=2002) 才返回 null；其余业务码（如 DB 故障 3001）
    // 属服务异常，抛给调用方走「加载失败」分支——误判成助手没了会把用户踢出编辑页
    if (code === 2002) return null
    throw new Error(message || '加载助手失败')
  }

  /** 加载可勾选工具列表（编辑页渲染工具开关） */
  async function loadTools() {
    if (toolsLoaded.value) return
    try {
      const { data, code } = await fetchTools()
      if (code === 0) {
        tools.value = data.tools || []
        toolsLoaded.value = true
      }
    } catch (_) {}
  }

  /** 创建自定义助手，返回新 id（失败返回 null） */
  async function create(payload) {
    const { data, code, message } = await createAssistant(payload)
    if (code === 0) {
      await loadAssistants()
      return data.id
    }
    throw new Error(message || '创建失败')
  }

  /** 更新自定义助手 */
  async function update(id, payload) {
    const { code, message } = await updateAssistant(id, payload)
    if (code === 0) {
      await loadAssistants()
      return true
    }
    throw new Error(message || '更新失败')
  }

  /** 删除自定义助手（直接真删；带预检确认的删除请用 confirmDeleteWithImpact） */
  async function remove(id) {
    const { code, message } = await deleteAssistant(id, true)
    if (code === 0) {
      await loadAssistants()
      return true
    }
    throw new Error(message || '删除失败')
  }

  /** 复制内置助手 → 自定义副本，返回新 id */
  async function duplicate(id) {
    const { data, code, message } = await duplicateAssistant(id)
    if (code === 0) {
      await loadAssistants()
      return data.id
    }
    throw new Error(message || '复制失败')
  }

  /** 生成 / 续用分享口令，返回 token */
  async function share(id) {
    const { data, code, message } = await enableShare(id)
    if (code === 0) {
      await loadAssistants()
      return data.share_token
    }
    throw new Error(message || '生成口令失败')
  }

  /** 关闭分享 */
  async function unshare(id) {
    const { code, message } = await disableShare(id)
    if (code === 0) {
      await loadAssistants()
      return true
    }
    throw new Error(message || '关闭分享失败')
  }

  /** 加载广场列表 */
  async function loadExplore() {
    exploreLoading.value = true
    try {
      const { data, code } = await fetchExploreAssistants()
      if (code === 0) {
        exploreList.value = data.assistants || []
      }
    } catch (_) {
      exploreList.value = []
    } finally {
      exploreLoading.value = false
    }
  }

  /** Fork 公开/分享助手到本地，返回新 id */
  async function fork(id) {
    const { data, code, message } = await forkAssistant(id)
    if (code === 0) {
      await loadAssistants()
      return data.id
    }
    throw new Error(message || 'Fork 失败')
  }

  /** 导出助手为 JSON 对象（前端可直接下载/复制） */
  async function exportOne(id) {
    const { data, code, message } = await exportAssistant(id)
    if (code === 0) return data
    throw new Error(message || '导出失败')
  }

  /** 导入助手 JSON，返回新 id */
  async function importOne(payload) {
    const { data, code, message } = await importAssistant(payload)
    if (code === 0) {
      await loadAssistants()
      return data.id
    }
    throw new Error(message || '导入失败')
  }

  /** 选中助手（供对话页 / 新建会话使用） */
  function selectAssistant(id) {
    currentAssistantId.value = id
  }

  return {
    // state
    assistants, loading, exploreList, exploreLoading, tools, toolsLoaded,
    currentAssistantId,
    // getters
    builtinAssistants, customAssistants, currentAssistant,
    // actions
    loadAssistants, loadAssistant, loadTools,
    create, update, remove, duplicate,
    share, unshare, loadExplore, fork,
    exportOne, importOne, selectAssistant,
  }
})
