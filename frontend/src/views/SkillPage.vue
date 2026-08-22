<template>
  <div class="page-root">
    <!-- 顶部操作栏 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <span class="toolbar-title">Skill 目录</span>
      </div>
      <div class="page-toolbar-right">
        <input
          ref="fileInput"
          type="file"
          accept=".gz,.tgz,.tar.gz,application/gzip,.zip,application/zip,application/x-zip-compressed"
          style="display: none"
          @change="handleFileSelected"
        />
        <el-button type="success" size="small" @click="triggerUpload" :loading="uploading">
          <el-icon><Upload /></el-icon> 安装 Skill
        </el-button>
        <el-button type="primary" size="small" @click="handleReload" :loading="reloading">
          <el-icon><Refresh /></el-icon> 重新扫描
        </el-button>
      </div>
    </div>

    <!-- 说明条 -->
    <div class="info-banner">
      <el-icon class="info-icon"><InfoFilled /></el-icon>
      <span>
        Skill 是 <code>{data_dir}/skills/</code> 下的文件系统技能，每个目录一个 <code>SKILL.md</code>。
        新增或修改 Skill 后点击「重新扫描」即可让<b>新会话</b>生效，无需重启服务。
      </span>
    </div>

    <!-- Skill 列表 -->
    <el-table :data="skills" v-loading="loading" empty-text="暂无 Skill" stripe style="width: 100%;">
      <el-table-column label="名称" min-width="200">
        <template #default="{ row }">
          <code>{{ row.name }}</code>
        </template>
      </el-table-column>
      <el-table-column prop="description" label="描述" min-width="340" show-overflow-tooltip />
      <el-table-column label="来源" width="100" align="center">
        <template #default="{ row }">
          <el-tag :type="row.scope === 'builtin' ? 'info' : 'success'" size="small">
            {{ row.scope === 'builtin' ? '内置' : '用户' }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column v-if="canDelete" label="操作" width="90" align="center">
        <template #default="{ row }">
          <el-button
            v-if="row.scope === 'user'"
            size="small"
            type="danger"
            plain
            :loading="deletingNames.has(row.name)"
            @click="handleDelete(row)"
          >
            删除
          </el-button>
          <span v-else class="cell-muted">-</span>
        </template>
      </el-table-column>
    </el-table>
  </div>
</template>

<script setup>
import { ref, computed, onMounted } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Refresh, InfoFilled, Upload } from '@element-plus/icons-vue'
import { useUserStore } from '../stores/user'
import { fetchSkills, reloadSkills, uploadSkill, deleteSkill } from '../api'

const userStore = useUserStore()
const skills = ref([])
const loading = ref(false)
const reloading = ref(false)
const uploading = ref(false)
const fileInput = ref(null)
const deletingNames = ref(new Set())

/**
 * 删除入口可见性(与后端守卫语义对齐,后端鉴权为准):
 * - 管理员登录 → 可删
 * - 单机 no-auth 模式(未启用本地登录且无 SSO provider)→ 可删(后端放行)
 * - 多用户模式普通用户/未登录 → 不可见
 */
const canDelete = computed(
  () =>
    userStore.user?.is_admin ||
    (!userStore.authenticated && !userStore.localEnabled && userStore.providers.length === 0),
)

/** 拉取当前已加载的 Skill 列表 */
async function loadSkills() {
  loading.value = true
  try {
    const { data, code, message } = await fetchSkills()
    if (code === 0) {
      skills.value = (data && data.skills) || []
    } else {
      ElMessage.error(message || '加载 Skill 列表失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    loading.value = false
  }
}

/** 重新扫描磁盘（后端 reload catalog），成功后刷新列表 */
async function handleReload() {
  reloading.value = true
  try {
    const { code, message } = await reloadSkills()
    if (code === 0) {
      ElMessage.success('Skill 目录已重新扫描')
      await loadSkills()
    } else {
      ElMessage.error(message || '重新扫描失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    reloading.value = false
  }
}

/** 触发隐藏 file input 的文件选择对话框 */
function triggerUpload() {
  fileInput.value && fileInput.value.click()
}

/** 选择压缩包（.tar.gz/.tgz/.zip/.tar）上传安装（同名存在时覆盖），成功后刷新列表 */
async function handleFileSelected(e) {
  const file = e.target.files && e.target.files[0]
  // 重置 value 以便同一文件可再次选择
  e.target.value = ''
  if (!file) return
  uploading.value = true
  try {
    const { code, message, data } = await uploadSkill(file, true)
    if (code === 0) {
      ElMessage.success(`Skill 安装成功: ${(data && data.name) || ''}`)
      await loadSkills()
    } else {
      ElMessage.error(message || '安装失败')
    }
  } catch (err) {
    ElMessage.error('请求失败: ' + (err.message || '网络错误'))
  } finally {
    uploading.value = false
  }
}

/** 删除 user 级 Skill(仅管理员/no-auth 可见按钮;后端鉴权为准)。确认后调接口并刷新列表 */
async function handleDelete(row) {
  try {
    await ElMessageBox.confirm(
      `确定删除 Skill「${row.name}」吗?将删除其全部文件(含 scripts/references),删除后不可恢复。`,
      '删除 Skill',
      {
        type: 'warning',
        confirmButtonText: '删除',
        cancelButtonText: '取消',
        confirmButtonClass: 'el-button--danger',
      },
    )
  } catch {
    return // 用户取消
  }
  // Set 支持并发多行删除:各行 loading 互不覆盖(loadSkills 后重建)
  deletingNames.value = new Set(deletingNames.value).add(row.name)
  try {
    const { code, message } = await deleteSkill(row.name)
    if (code === 0) {
      ElMessage.success(`已删除 Skill: ${row.name}`)
      await loadSkills()
    } else {
      ElMessage.error(message || '删除失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    const next = new Set(deletingNames.value)
    next.delete(row.name)
    deletingNames.value = next
  }
}

onMounted(loadSkills)
</script>

<style scoped>
.toolbar-title {
  font-size: 16px;
  font-weight: 600;
}
</style>
