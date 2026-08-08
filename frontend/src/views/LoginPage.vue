<template>
  <div class="login-page">
    <!-- 背景光晕 -->
    <div class="bg-orbs">
      <div class="orb orb-1"></div>
      <div class="orb orb-2"></div>
      <div class="orb orb-3"></div>
    </div>

    <!-- 登录卡片 -->
    <div class="login-card">
      <!-- Logo + 标题 -->
      <div class="brand">
        <div class="logo-icon">
          <svg width="40" height="40" viewBox="0 0 28 28" fill="none">
            <rect x="2" y="2" width="10" height="10" rx="2" fill="#00d4ff" opacity="0.9" />
            <rect x="16" y="2" width="10" height="10" rx="2" fill="#0ea5e9" opacity="0.7" />
            <rect x="2" y="16" width="10" height="10" rx="2" fill="#0ea5e9" opacity="0.7" />
            <rect x="16" y="16" width="10" height="10" rx="2" fill="#00d4ff" opacity="0.5" />
          </svg>
        </div>
        <h1>{{ isRegisterMode ? '创建账号' : 'cortex-agent' }}</h1>
        <p class="subtitle">
          <span v-if="isRegisterMode">填写信息完成注册，首个账号将成为管理员</span>
          <span v-else>选择登录方式进入系统</span>
        </p>
      </div>

      <!-- 加载中 -->
      <div v-if="loading" class="state-block">
        <div class="spinner"></div>
        <span>正在加载…</span>
      </div>

      <!-- 无任何登录方式 -->
      <div
        v-else-if="!localEnabled && providers.length === 0"
        class="state-block error"
      >
        <span class="state-icon">⚠️</span>
        <span>登录服务未启用，请联系管理员</span>
      </div>

      <!-- 主体内容 -->
      <template v-else>
        <!-- 本地账号表单（登录 / 注册） -->
        <form v-if="localEnabled" class="local-form" @submit.prevent="submitLocal">
          <div class="form-field">
            <input
              v-model.trim="form.username"
              class="form-input"
              type="text"
              placeholder="用户名"
              autocomplete="username"
              :disabled="submitting"
            />
          </div>
          <div class="form-field">
            <input
              v-model.trim="form.name"
              v-if="isRegisterMode"
              class="form-input"
              type="text"
              placeholder="显示名称（可选）"
              autocomplete="name"
              :disabled="submitting"
            />
          </div>
          <div class="form-field">
            <input
              v-model="form.password"
              class="form-input"
              :type="showPassword ? 'text' : 'password'"
              placeholder="密码"
              :autocomplete="isRegisterMode ? 'new-password' : 'current-password'"
              :disabled="submitting"
            />
            <span class="pwd-toggle" @click="showPassword = !showPassword">
              {{ showPassword ? '🙈' : '👁️' }}
            </span>
          </div>
          <div v-if="isRegisterMode" class="form-field">
            <input
              v-model="form.confirmPassword"
              class="form-input"
              :type="showPassword ? 'text' : 'password'"
              placeholder="确认密码"
              autocomplete="new-password"
              :disabled="submitting"
            />
          </div>

          <p v-if="formError" class="form-error">{{ formError }}</p>

          <button type="submit" class="submit-btn" :disabled="submitting">
            <span v-if="!submitting">{{ isRegisterMode ? '注册并登录' : '登录' }}</span>
            <span v-else class="btn-loading">
              <span class="spinner small"></span> 处理中…
            </span>
          </button>

          <p class="switch-mode">
            <span v-if="isRegisterMode">已有账号？</span>
            <span v-else>还没有账号？</span>
            <a href="#" @click.prevent="toggleMode">
              {{ isRegisterMode ? '返回登录' : '注册新账号' }}
            </a>
          </p>
        </form>

        <!-- SSO 分隔线（本地 + SSO 同时存在时显示） -->
        <div v-if="localEnabled && providers.length > 0" class="divider">
          <span>或使用以下方式登录</span>
        </div>

        <!-- SSO 提供商按钮列表 -->
        <div v-if="providers.length > 0" class="provider-list">
          <button
            v-for="p in providers"
            :key="p.key"
            class="provider-btn"
            :class="'kind-' + p.kind"
            @click="loginWith(p.key)"
          >
            <span class="provider-icon" v-html="iconFor(p.kind)"></span>
            <span class="provider-name">{{ labelFor(p.kind) || p.name }}</span>
            <span class="provider-arrow">→</span>
          </button>
        </div>
      </template>

      <!-- 页脚 -->
      <div class="login-footer">
        <span>安全访问 · 单点登录</span>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, reactive, onMounted, computed } from 'vue'
import { useRouter } from 'vue-router'
import { useUserStore } from '../stores/user'

const router = useRouter()
const userStore = useUserStore()

// 从 store 暴露给模板使用（模板直接引用 localEnabled / providers）
const localEnabled = computed(() => userStore.localEnabled)
const providers = computed(() => userStore.providers)

const loading = ref(true)
const submitting = ref(false)
const isRegisterMode = ref(false)
const showPassword = ref(false)
const formError = ref('')

const form = reactive({
  username: '',
  name: '',
  password: '',
  confirmPassword: '',
})

onMounted(async () => {
  // 守卫可能已检测过登录状态；未检测时补一次（兼容直接访问 /login）
  if (!userStore.checked) {
    await userStore.loadMe()
  }
  // 已登录则直接进入系统
  if (userStore.authenticated) {
    redirectAfterAuth()
    return
  }
  if (!userStore.providers.length && !userStore.localEnabled) {
    await userStore.loadProviders()
  }
  loading.value = false
})

function redirectAfterAuth() {
  const redirect = router.currentRoute.value.query.redirect
  router.replace(
    typeof redirect === 'string' && redirect.startsWith('/') ? redirect : '/sessions',
  )
}

function toggleMode() {
  isRegisterMode.value = !isRegisterMode.value
  formError.value = ''
  form.password = ''
  form.confirmPassword = ''
}

function validateForm() {
  if (!form.username) {
    return '请输入用户名'
  }
  if (form.username.length < 3) {
    return '用户名至少 3 个字符'
  }
  if (!form.password) {
    return '请输入密码'
  }
  if (isRegisterMode.value) {
    if (form.password.length < 8) {
      return '密码至少 8 个字符'
    }
    if (form.password !== form.confirmPassword) {
      return '两次输入的密码不一致'
    }
  }
  return null
}

async function submitLocal() {
  formError.value = ''
  const err = validateForm()
  if (err) {
    formError.value = err
    return
  }
  submitting.value = true
  try {
    if (isRegisterMode.value) {
      await userStore.doRegister(form.username, form.password, form.name)
    } else {
      await userStore.doLoginLocal(form.username, form.password)
    }
    redirectAfterAuth()
  } catch (e) {
    formError.value = e.message || '操作失败，请重试'
  } finally {
    submitting.value = false
  }
}

function loginWith(key) {
  // 跳转后端 OAuth 入口，由后端 302 到 IdP
  window.location.href = '/api/auth/login/' + encodeURIComponent(key)
}

function labelFor(kind) {
  const map = { feishu: '飞书登录', wechat: '微信登录', oidc: '企业账号登录' }
  return map[kind] || '登录'
}

function iconFor(kind) {
  const map = {
    feishu: '<svg viewBox="0 0 24 24" width="22" height="22" fill="currentColor"><path d="M4.5 3.5h6.2c2.6 0 4.3 1.5 4.3 4 0 1.7-.9 2.9-2.3 3.4l3.6 5.6h-3.2l-3.2-5.1H7.4v5.1H4.5V3.5zm2.9 2.4v3.4h3.1c1.4 0 2.2-.6 2.2-1.7s-.8-1.7-2.2-1.7H7.4z"/><path d="M15 16.5c0-2.4 2-4 5-4 .9 0 1.7.1 2.4.4v-.3c0-1.2-.8-1.9-2.3-1.9-1.1 0-2.1.3-3.1.9l-.9-1.9c1.3-.8 2.8-1.2 4.4-1.2 2.9 0 4.5 1.4 4.5 4v5.8H22v-.8c-.7.6-1.7.9-2.8.9-2.3 0-4.2-1.4-4.2-3.9zm5-.6c-.8 0-1.5.2-2 .6v1.6c.5.4 1.2.6 2 .6 1 0 1.7-.5 1.7-1.4 0-.9-.7-1.4-1.7-1.4z"/></svg>',
    wechat: '<svg viewBox="0 0 24 24" width="22" height="22" fill="currentColor"><path d="M9.5 4C5.4 4 2 6.8 2 10.2c0 1.9 1 3.6 2.7 4.8L4 17l2.4-1.2c.7.2 1.5.3 2.3.3h.6c-.1-.5-.2-1-.2-1.5 0-3.2 3-5.7 6.7-5.7h.6C16 6.2 13.1 4 9.5 4zM7.3 8.6c-.6 0-1-.5-1-1s.5-1 1-1 1 .5 1 1-.4 1-1 1zm4.4 0c-.6 0-1-.5-1-1s.5-1 1-1 1 .5 1 1-.4 1-1 1z"/><path d="M22 14.6c0-2.8-2.7-5-6-5s-6 2.3-6 5 2.7 5 6 5c.7 0 1.4-.1 2-.3l1.9 1-.5-1.6c1.6-1 2.6-2.5 2.6-4.1zm-8-.8c-.5 0-.8-.4-.8-.8s.4-.8.8-.8.8.4.8.8-.3.8-.8.8zm4 0c-.5 0-.8-.4-.8-.8s.4-.8.8-.8.8.4.8.8-.3.8-.8.8z"/></svg>',
    oidc: '<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2l9 4v6c0 5-3.8 9.5-9 10-5.2-.5-9-5-9-10V6l9-4z"/><path d="M9 12l2 2 4-4"/></svg>',
  }
  return map[kind] || '<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="8" r="4"/><path d="M4 21c0-4.4 3.6-8 8-8s8 3.6 8 8"/></svg>'
}
</script>

<style scoped>
.login-page {
  height: 100vh;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--bg);
  position: relative;
  overflow: hidden;
}

/* === 背景光晕 === */
.bg-orbs { position: absolute; inset: 0; pointer-events: none; }
.orb { position: absolute; border-radius: 50%; filter: blur(80px); opacity: 0.4; }
.orb-1 {
  width: 400px; height: 400px;
  background: radial-gradient(circle, rgba(0, 212, 255, 0.25) 0%, transparent 70%);
  top: -100px; left: -100px; animation: float1 12s ease-in-out infinite;
}
.orb-2 {
  width: 350px; height: 350px;
  background: radial-gradient(circle, rgba(14, 165, 233, 0.2) 0%, transparent 70%);
  bottom: -80px; right: -80px; animation: float2 15s ease-in-out infinite;
}
.orb-3 {
  width: 300px; height: 300px;
  background: radial-gradient(circle, rgba(139, 92, 246, 0.12) 0%, transparent 70%);
  top: 50%; left: 60%; animation: float3 18s ease-in-out infinite;
}
@keyframes float1 { 0%, 100% { transform: translate(0, 0); } 50% { transform: translate(40px, 60px); } }
@keyframes float2 { 0%, 100% { transform: translate(0, 0); } 50% { transform: translate(-50px, -40px); } }
@keyframes float3 { 0%, 100% { transform: translate(0, 0); } 50% { transform: translate(-30px, 50px); } }

/* === 登录卡片 === */
.login-card {
  position: relative; z-index: 1;
  width: 400px; max-width: calc(100vw - 40px);
  background: linear-gradient(180deg, rgba(14, 14, 22, 0.9) 0%, rgba(8, 8, 15, 0.95) 100%);
  border: 1px solid var(--border); border-radius: var(--radius-lg);
  padding: 40px 36px 28px;
  box-shadow: var(--shadow-lg), 0 0 60px rgba(0, 212, 255, 0.08);
  backdrop-filter: blur(20px);
  animation: cardIn 0.5s ease;
}
@keyframes cardIn { from { opacity: 0; transform: translateY(20px); } to { opacity: 1; transform: translateY(0); } }

/* === 品牌区 === */
.brand { text-align: center; margin-bottom: 28px; }
.logo-icon {
  width: 64px; height: 64px; border-radius: 16px;
  background: linear-gradient(135deg, rgba(0, 212, 255, 0.15) 0%, rgba(14, 165, 233, 0.08) 100%);
  border: 1px solid rgba(0, 212, 255, 0.25);
  display: flex; align-items: center; justify-content: center;
  margin: 0 auto 16px; box-shadow: 0 0 24px rgba(0, 212, 255, 0.15);
}
.brand h1 { font-size: 22px; font-weight: 800; color: var(--text-h); letter-spacing: -0.5px; margin-bottom: 6px; }
.subtitle { font-size: 13px; color: var(--muted); font-weight: 500; }

/* === 状态块 === */
.state-block {
  display: flex; flex-direction: column; align-items: center; gap: 14px;
  padding: 40px 0; color: var(--muted); font-size: 14px;
}
.state-block.error { color: var(--error); }
.state-icon { font-size: 32px; }
.spinner {
  width: 32px; height: 32px;
  border: 3px solid var(--border); border-top-color: var(--accent);
  border-radius: 50%; animation: spin 0.8s linear infinite;
}
.spinner.small { width: 14px; height: 14px; border-width: 2px; }
@keyframes spin { to { transform: rotate(360deg); } }

/* === 本地表单 === */
.local-form { display: flex; flex-direction: column; gap: 12px; }
.form-field { position: relative; }
.form-input {
  width: 100%; padding: 12px 14px;
  background: var(--card); border: 1px solid var(--border); border-radius: var(--radius);
  color: var(--text); font-size: 14px; font-family: var(--font-sans);
  transition: all 0.2s ease; outline: none;
  box-sizing: border-box;
}
.form-input:focus {
  border-color: rgba(0, 212, 255, 0.5);
  background: var(--card-hover);
  box-shadow: 0 0 0 3px rgba(0, 212, 255, 0.08);
}
.form-input:disabled { opacity: 0.6; cursor: not-allowed; }
.form-input::placeholder { color: var(--muted); }
.pwd-toggle {
  position: absolute; right: 12px; top: 50%; transform: translateY(-50%);
  cursor: pointer; font-size: 16px; user-select: none; opacity: 0.7;
}
.form-error {
  color: var(--error); font-size: 12px; margin: 0; padding: 4px 2px;
}
.submit-btn {
  width: 100%; padding: 13px;
  background: linear-gradient(135deg, var(--accent) 0%, var(--accent-secondary) 100%);
  border: none; border-radius: var(--radius);
  color: #06060a; font-size: 14px; font-weight: 700; cursor: pointer;
  transition: all 0.2s ease; font-family: var(--font-sans);
  box-shadow: 0 4px 16px rgba(0, 212, 255, 0.2);
}
.submit-btn:hover:not(:disabled) {
  transform: translateY(-1px);
  box-shadow: 0 6px 20px rgba(0, 212, 255, 0.3);
}
.submit-btn:disabled { opacity: 0.7; cursor: not-allowed; }
.btn-loading { display: inline-flex; align-items: center; gap: 8px; }
.switch-mode {
  text-align: center; font-size: 13px; color: var(--muted); margin: 4px 0 0;
}
.switch-mode a { color: var(--accent); text-decoration: none; margin-left: 4px; }
.switch-mode a:hover { text-decoration: underline; }

/* === 分隔线 === */
.divider {
  display: flex; align-items: center; gap: 12px;
  margin: 20px 0; color: var(--muted); font-size: 12px;
}
.divider::before, .divider::after {
  content: ''; flex: 1; height: 1px; background: var(--border);
}

/* === SSO 提供商按钮 === */
.provider-list { display: flex; flex-direction: column; gap: 10px; }
.provider-btn {
  display: flex; align-items: center; gap: 12px;
  width: 100%; padding: 13px 16px;
  border-radius: var(--radius); border: 1px solid var(--border);
  background: var(--card); color: var(--text);
  font-size: 14px; font-weight: 600; cursor: pointer;
  transition: all 0.2s ease; font-family: var(--font-sans);
}
.provider-btn:hover {
  border-color: rgba(0, 212, 255, 0.4);
  background: var(--card-hover); transform: translateY(-1px);
  box-shadow: 0 4px 16px rgba(0, 212, 255, 0.1);
}
.provider-icon {
  width: 32px; height: 32px; border-radius: 8px;
  display: flex; align-items: center; justify-content: center; flex-shrink: 0;
  background: rgba(255, 255, 255, 0.05); color: var(--muted-light);
  transition: all 0.2s ease;
}
.provider-btn:hover .provider-icon { color: var(--accent); background: rgba(0, 212, 255, 0.1); }
.kind-feishu .provider-icon { color: #33d0ea; }
.kind-wechat .provider-icon { color: #07c160; }
.kind-oidc .provider-icon { color: var(--accent); }
.provider-name { flex: 1; text-align: left; }
.provider-arrow { color: var(--muted); font-size: 16px; transition: all 0.2s ease; }
.provider-btn:hover .provider-arrow { color: var(--accent); transform: translateX(3px); }

/* === 页脚 === */
.login-footer {
  margin-top: 24px; text-align: center;
  font-size: 12px; color: var(--muted); font-weight: 500;
}
</style>
