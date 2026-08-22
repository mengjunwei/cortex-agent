<template>
  <div class="page-root">
    <!-- 账号安全：修改密码（仅有本地密码的账号显示） -->
    <div class="security-card" v-if="hasPassword">
      <div class="security-head">
        <span class="security-title">🔒 登录密码</span>
        <el-button type="primary" size="small" @click="openPwdDialog">
          <el-icon><Key /></el-icon>&nbsp;修改密码
        </el-button>
      </div>
      <div class="security-desc">修改密码后，该账号在所有设备上的登录将立即失效，需要重新登录。</div>
    </div>

    <!-- 顶部操作栏 -->
    <div class="page-toolbar">
      <div class="page-toolbar-left">
        <span class="toolbar-title">🔑 API 访问令牌</span>
      </div>
      <div class="page-toolbar-right">
        <el-button type="primary" size="small" @click="openCreateDialog">
          <el-icon><Plus /></el-icon> 新建令牌
        </el-button>
        <el-button size="small" @click="loadTokens" :loading="loading">
          <el-icon><Refresh /></el-icon> 刷新
        </el-button>
      </div>
    </div>

    <!-- 说明条 -->
    <div class="info-banner">
      <el-icon class="info-icon"><InfoFilled /></el-icon>
      <span>
        令牌供外部系统以 <code>Authorization: Bearer &lt;令牌&gt;</code> 调用本系统接口，等价登录身份。
        <b>明文仅在创建时显示一次</b>，请立即复制保存；丢失只能删除后重建。
      </span>
    </div>

    <!-- 令牌表格 -->
    <div class="data-table-wrapper" v-loading="loading">
      <el-table class="data-table" :data="tokens" border height="100%">
        <el-table-column label="名称 / 备注" min-width="170" show-overflow-tooltip>
          <template #default="{ row }">
            <div class="cell-title">{{ row.name }}</div>
            <div v-if="row.remark" class="cell-muted" style="font-size:11px; white-space:normal;">{{ row.remark }}</div>
          </template>
        </el-table-column>

        <el-table-column label="令牌" width="150">
          <template #default="{ row }">
            <code class="token-prefix">{{ row.prefix }}…</code>
          </template>
        </el-table-column>

        <el-table-column label="启用" width="80" align="center">
          <template #default="{ row }">
            <el-switch :model-value="row.enabled" @change="(v) => toggleEnabled(row, v)" />
          </template>
        </el-table-column>

        <el-table-column label="生效时间段" min-width="240">
          <template #default="{ row }">
            <span class="cell-muted">{{ formatWindow(row) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="最后使用" width="160">
          <template #default="{ row }">
            <span class="cell-muted">{{ row.last_used_at ? formatTime(row.last_used_at) : '从未使用' }}</span>
          </template>
        </el-table-column>

        <el-table-column label="创建时间" width="160">
          <template #default="{ row }">
            <span class="cell-muted">{{ formatTime(row.created_at) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="操作" width="170" align="center" fixed="right">
          <template #default="{ row }">
            <div class="row-actions">
              <el-button size="small" @click="openEditDialog(row)">编辑</el-button>
              <el-popconfirm
                title="确定删除该令牌吗？删除后使用该令牌的调用将立即失败。"
                confirm-button-text="删除"
                cancel-button-text="取消"
                @confirm="handleDelete(row.id)"
              >
                <template #reference>
                  <el-button size="small" type="danger" plain>删除</el-button>
                </template>
              </el-popconfirm>
            </div>
          </template>
        </el-table-column>

        <template #empty>
          <div class="table-empty">
            <div class="empty-icon">🔑</div>
            <div class="empty-title">暂无 API 令牌</div>
            <div class="empty-hint">点击右上角「新建令牌」，为外部系统生成一把访问钥匙</div>
          </div>
        </template>
      </el-table>
    </div>

    <!-- 新建 / 编辑 弹窗 -->
    <el-dialog
      v-model="formDialogVisible"
      :title="form.id ? '编辑令牌' : '新建令牌'"
      width="480px"
      :close-on-click-modal="false"
    >
      <el-form ref="formRef" :model="form" :rules="formRules" label-width="100px">
        <el-form-item label="名称" prop="name">
          <el-input v-model="form.name" placeholder="标识用途，如「数据看板接入」" maxlength="64" show-word-limit />
        </el-form-item>
        <el-form-item label="备注">
          <el-input
            v-model="form.remark"
            type="textarea"
            :rows="2"
            placeholder="可选，记录用途 / 责任人等"
            maxlength="200"
            show-word-limit
          />
        </el-form-item>
        <el-form-item label="生效开始">
          <el-date-picker
            v-model="form.valid_from"
            type="datetime"
            placeholder="留空 = 创建即生效"
            style="width: 100%;"
          />
          <div class="form-hint">留空表示创建后立即生效</div>
        </el-form-item>
        <el-form-item label="过期时间">
          <el-date-picker
            v-model="form.expires_at"
            type="datetime"
            placeholder="留空 = 永不过期"
            style="width: 100%;"
          />
          <div class="form-hint">留空表示永久有效</div>
        </el-form-item>
        <el-form-item v-if="form.id" label="启用">
          <el-switch v-model="form.enabled" />
          <span class="form-hint" style="margin-left:8px;">{{ form.enabled ? '启用中' : '已禁用' }}</span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="formDialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="submitting" @click="submitForm">
          {{ form.id ? '保存' : '创建' }}
        </el-button>
      </template>
    </el-dialog>

    <!-- 创建成功：一次性明文展示 -->
    <el-dialog
      v-model="tokenResultVisible"
      title="令牌已创建"
      width="580px"
      :close-on-click-modal="false"
      :show-close="false"
    >
      <el-alert type="warning" :closable="false" show-icon style="margin-bottom: 12px;">
        这是该令牌的<b>唯一一次明文展示</b>，关闭后将无法再次查看。请立即复制并妥善保存！
      </el-alert>
      <el-input :model-value="createdToken" readonly type="textarea" :rows="2" resize="none" />
      <template #footer>
        <el-button type="primary" @click="copyToken">
          <el-icon><DocumentCopy /></el-icon> 复制令牌
        </el-button>
        <el-button @click="tokenResultVisible = false">我已保存，关闭</el-button>
      </template>
    </el-dialog>

    <!-- 修改密码弹窗 -->
    <el-dialog
      v-model="pwdDialogVisible"
      title="修改密码"
      width="440px"
      :close-on-click-modal="false"
    >
      <el-form ref="pwdFormRef" :model="pwdForm" :rules="pwdRules" label-width="90px">
        <el-form-item label="原密码" prop="old_password">
          <el-input v-model="pwdForm.old_password" type="password" show-password placeholder="输入当前密码" />
        </el-form-item>
        <el-form-item label="新密码" prop="new_password">
          <el-input v-model="pwdForm.new_password" type="password" show-password placeholder="至少 8 位" />
        </el-form-item>
        <el-form-item label="确认密码" prop="confirm">
          <el-input v-model="pwdForm.confirm" type="password" show-password placeholder="再次输入新密码" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="pwdDialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="pwdSubmitting" @click="submitPwd">确认修改</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup>
import { ref, reactive, computed, onMounted, nextTick } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Plus, Refresh, DocumentCopy, InfoFilled, Key } from '@element-plus/icons-vue'
import { fetchApiTokens, createApiToken, updateApiToken, deleteApiToken, authChangePassword } from '../api'
import { useUserStore } from '../stores/user'

const router = useRouter()
const userStore = useUserStore()
// 仅有本地密码的账号才显示「修改密码」入口（纯 SSO 账号无密码）
const hasPassword = computed(() => !!userStore.user?.has_password)

const loading = ref(false)
const submitting = ref(false)
const tokens = ref([])
const formDialogVisible = ref(false)
const tokenResultVisible = ref(false)
const createdToken = ref('')
const formRef = ref(null)

// 修改密码
const pwdDialogVisible = ref(false)
const pwdSubmitting = ref(false)
const pwdFormRef = ref(null)
const pwdForm = reactive({ old_password: '', new_password: '', confirm: '' })
const pwdRules = {
  old_password: [{ required: true, message: '请输入原密码', trigger: 'blur' }],
  new_password: [
    { required: true, message: '请输入新密码', trigger: 'blur' },
    { min: 8, max: 128, message: '密码长度 8-128 位', trigger: 'blur' },
  ],
  confirm: [
    { required: true, message: '请再次输入新密码', trigger: 'blur' },
    {
      validator: (rule, value, cb) =>
        value === pwdForm.new_password ? cb() : cb(new Error('两次输入的密码不一致')),
      trigger: 'blur',
    },
  ],
}

const form = reactive({
  id: null,
  name: '',
  remark: '',
  valid_from: null,
  expires_at: null,
  enabled: true,
})

const formRules = {
  name: [{ required: true, message: '请输入令牌名称', trigger: 'blur' }],
}

function resetForm() {
  form.id = null
  form.name = ''
  form.remark = ''
  form.valid_from = null
  form.expires_at = null
  form.enabled = true
}

async function loadTokens() {
  loading.value = true
  try {
    const res = await fetchApiTokens()
    if (res.code === 0) {
      tokens.value = res.data?.tokens || []
    } else {
      ElMessage.error(res.message || '加载令牌失败')
    }
  } catch (e) {
    ElMessage.error('加载令牌失败：' + (e?.message || e))
  } finally {
    loading.value = false
  }
}

function openCreateDialog() {
  resetForm()
  // 清掉上次打开残留的校验红字（重置值不重置 el-form 验证态）
  nextTick(() => formRef.value?.clearValidate())
  formDialogVisible.value = true
}

function openEditDialog(row) {
  form.id = row.id
  form.name = row.name
  form.remark = row.remark
  form.valid_from = row.valid_from ? new Date(row.valid_from) : null
  form.expires_at = row.expires_at ? new Date(row.expires_at) : null
  form.enabled = !!row.enabled
  nextTick(() => formRef.value?.clearValidate())
  formDialogVisible.value = true
}

// Date → ISO 字符串（RFC3339 带 Z），null 时返回 null（后端按"不设置"处理）
function toIso(d) {
  return d ? d.toISOString() : null
}

async function submitForm() {
  if (!formRef.value) return
  try {
    await formRef.value.validate()
  } catch (_) {
    return
  }
  // 时间段合理性前端兜底校验
  if (form.valid_from && form.expires_at && form.valid_from > form.expires_at) {
    ElMessage.error('生效起始时间不能晚于过期时间')
    return
  }
  submitting.value = true
  try {
    const payload = {
      name: form.name.trim(),
      remark: form.remark.trim(),
      valid_from: toIso(form.valid_from),
      expires_at: toIso(form.expires_at),
    }
    if (form.id) {
      payload.enabled = form.enabled
      const res = await updateApiToken(form.id, payload)
      if (res.code === 0) {
        ElMessage.success('已保存')
        formDialogVisible.value = false
        await loadTokens()
      } else {
        ElMessage.error(res.message || '保存失败')
      }
    } else {
      const res = await createApiToken(payload)
      if (res.code === 0) {
        formDialogVisible.value = false
        createdToken.value = res.data?.token || ''
        tokenResultVisible.value = true
        await loadTokens()
      } else {
        ElMessage.error(res.message || '创建失败')
      }
    }
  } catch (e) {
    ElMessage.error('操作失败：' + (e?.message || e))
  } finally {
    submitting.value = false
  }
}

// 启用开关：携带当前完整字段 + 新 enabled 调 PATCH
async function toggleEnabled(row, v) {
  try {
    const res = await updateApiToken(row.id, {
      name: row.name,
      remark: row.remark,
      valid_from: row.valid_from || null,
      expires_at: row.expires_at || null,
      enabled: v,
    })
    if (res.code === 0) {
      row.enabled = v
      ElMessage.success(v ? '已启用' : '已禁用')
    } else {
      ElMessage.error(res.message || '操作失败')
    }
  } catch (e) {
    // 失败不改 row.enabled（开关回弹），提示网络错误
    ElMessage.error('操作失败：' + (e?.message || e))
  }
}

async function handleDelete(id) {
  try {
    const res = await deleteApiToken(id)
    if (res.code === 0) {
      ElMessage.success('已删除')
      await loadTokens()
    } else {
      ElMessage.error(res.message || '删除失败')
    }
  } catch (e) {
    ElMessage.error('删除失败：' + (e?.message || e))
  }
}

async function copyToken() {
  try {
    await navigator.clipboard.writeText(createdToken.value)
    ElMessage.success('已复制到剪贴板')
  } catch (_) {
    ElMessage.warning('复制失败，请手动选择文本复制')
  }
}

function openPwdDialog() {
  pwdForm.old_password = ''
  pwdForm.new_password = ''
  pwdForm.confirm = ''
  nextTick(() => pwdFormRef.value?.clearValidate())
  pwdDialogVisible.value = true
}

async function submitPwd() {
  if (!pwdFormRef.value) return
  try {
    await pwdFormRef.value.validate()
  } catch (_) {
    return
  }
  pwdSubmitting.value = true
  try {
    const res = await authChangePassword(pwdForm.old_password, pwdForm.new_password)
    if (res.code !== 0) {
      ElMessage.error(res.message || '修改失败')
      return
    }
    pwdDialogVisible.value = false
    ElMessage.success('密码已修改，请重新登录')
    // 改密成功 → 后端已使该账号全部旧会话失效；前端作废当前会话并回登录页
    await userStore.doLogout()
    userStore.checked = false
    router.push('/login')
  } catch (e) {
    ElMessage.error('修改失败：' + (e?.message || e))
  } finally {
    pwdSubmitting.value = false
  }
}

function formatTime(iso) {
  if (!iso) return '—'
  return new Date(iso).toLocaleString('zh-CN', { hour12: false })
}

function formatWindow(row) {
  const from = row.valid_from ? formatTime(row.valid_from) : '立即'
  const to = row.expires_at ? formatTime(row.expires_at) : '永久'
  return `${from}  ~  ${to}`
}

onMounted(loadTokens)
</script>

<style scoped>
.toolbar-title {
  font-size: 14px;
  font-weight: 700;
  color: var(--text-h);
}

.info-banner {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 14px;
  background: rgba(14, 165, 233, 0.08);
  border: 1px solid rgba(14, 165, 233, 0.25);
  border-radius: var(--radius);
  margin-bottom: 14px;
  font-size: 13px;
  color: var(--text);
  flex-shrink: 0;
}
.info-banner .info-icon {
  color: var(--accent);
  font-size: 16px;
  flex-shrink: 0;
}
.info-banner code {
  color: var(--accent);
  font-family: var(--font-mono);
  background: rgba(0, 212, 255, 0.1);
  padding: 1px 6px;
  border-radius: 4px;
}

.token-prefix {
  font-family: var(--font-mono);
  color: var(--accent);
  font-size: 12px;
}

.form-hint {
  font-size: 11px;
  color: var(--muted);
  margin-top: 4px;
}

.security-card {
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 14px 16px;
  margin-bottom: 14px;
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
.security-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.security-title {
  font-size: 14px;
  font-weight: 700;
  color: var(--text-h);
}
.security-desc {
  font-size: 12px;
  color: var(--muted);
  line-height: 1.5;
}
</style>
