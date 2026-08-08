import { defineStore } from 'pinia'
import { ref } from 'vue'
import {
  fetchAuthProviders,
  fetchMe,
  authLogout,
  authRegister,
  authLoginLocal,
} from '../api'

/**
 * 当前登录用户与身份提供商状态
 *
 * - `authenticated`：是否已登录（由 /api/auth/me 决定）
 * - `checked`：是否已完成首次 me 检测（路由守卫依赖此标志避免重复请求）
 * - `user`：{ user_id, name, avatar, is_admin }，未登录为 null
 * - `providers`：后端配置的 SSO 身份提供商列表（飞书/微信/OIDC）
 * - `localEnabled`：本地用户名密码登录是否可用（Auth 服务启用即为 true）
 */
export const useUserStore = defineStore('user', () => {
  const authenticated = ref(false)
  const checked = ref(false)
  const user = ref(null)
  const providers = ref([])
  const localEnabled = ref(false)

  async function loadMe() {
    try {
      const { data } = await fetchMe()
      const authed = !!(data && data.authenticated)
      authenticated.value = authed
      user.value = authed && data.user ? data.user : null
      // 未登录时探测后端登录可用性（SSO provider + 本地登录）
      if (!authed) {
        await loadProviders()
      }
    } catch (_) {
      authenticated.value = false
      user.value = null
    } finally {
      checked.value = true
    }
  }

  async function loadProviders() {
    try {
      const { data } = await fetchAuthProviders()
      providers.value = data && Array.isArray(data.providers) ? data.providers : []
      localEnabled.value = !!(data && data.local_enabled)
    } catch (_) {
      providers.value = []
      localEnabled.value = false
    }
  }

  async function doLogout() {
    try {
      await authLogout()
    } catch (_) {
      // 登出接口失败也前端清状态（fail-open）
    }
    reset()
  }

  // 本地账号注册成功后更新本地状态
  function setUser(u) {
    user.value = u
    authenticated.value = true
    checked.value = true
  }

  // 本地账号登录
  async function doLoginLocal(username, password) {
    const { code, message, data } = await authLoginLocal(username, password)
    if (code !== 0) {
      throw new Error(message || '登录失败')
    }
    user.value = data && data.user ? data.user : null
    authenticated.value = true
    checked.value = true
    return data && data.user
  }

  // 本地账号注册
  async function doRegister(username, password, name) {
    const { code, message, data } = await authRegister(username, password, name)
    if (code !== 0) {
      throw new Error(message || '注册失败')
    }
    user.value = data && data.user ? data.user : null
    authenticated.value = true
    checked.value = true
    return data && data.user
  }

  function reset() {
    authenticated.value = false
    user.value = null
  }

  return {
    authenticated,
    checked,
    user,
    providers,
    localEnabled,
    loadMe,
    loadProviders,
    doLogout,
    setUser,
    doLoginLocal,
    doRegister,
    reset,
  }
})
