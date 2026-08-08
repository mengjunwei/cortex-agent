import { createRouter, createWebHistory } from 'vue-router'
import { useUserStore } from '../stores/user'

const routes = [
  { path: '/', redirect: '/sessions' },
  { path: '/login', name: 'Login', component: () => import('../views/LoginPage.vue'), meta: { title: '登录', public: true } },
  { path: '/sessions', name: 'Sessions', component: () => import('../views/SessionHistoryPage.vue'), meta: { title: '会话历史' } },
  { path: '/chat', name: 'Chat', component: () => import('../views/ChatPage.vue'), meta: { title: '智能对话' } },
  { path: '/memories', name: 'Memories', component: () => import('../views/MemoryPage.vue'), meta: { title: '我的记忆' } },
  { path: '/device', name: 'Device', component: () => import('../views/DevicePage.vue'), meta: { title: '设备命令助手' } },
  { path: '/knowledge', name: 'KnowledgeList', component: () => import('../views/KnowledgeListPage.vue'), meta: { title: '知识库管理' } },
  { path: '/knowledge/:id', name: 'KnowledgeDetail', component: () => import('../views/KnowledgeDetailPage.vue'), meta: { title: '知识库详情' } },
  { path: '/monitor', name: 'Monitor', component: () => import('../views/MonitorPage.vue'), meta: { title: '监控插件管理' } },
  { path: '/model-providers', name: 'ModelProviders', component: () => import('../views/ModelProviderPage.vue'), meta: { title: '模型供应商管理' } },
  { path: '/mcp-servers', name: 'McpServers', component: () => import('../views/McpServerPage.vue'), meta: { title: 'MCP 服务管理' } },
  { path: '/skills', name: 'Skills', component: () => import('../views/SkillPage.vue'), meta: { title: 'Skill 管理' } },
  { path: '/assistants', name: 'Assistants', component: () => import('../views/AssistantPage.vue'), meta: { title: '助手管理' } },
  { path: '/assistants/new', name: 'AssistantNew', component: () => import('../views/AssistantEditPage.vue'), meta: { title: '创建助手' } },
  { path: '/assistants/:id/edit', name: 'AssistantEdit', component: () => import('../views/AssistantEditPage.vue'), meta: { title: '编辑助手' } },
  { path: '/explore', name: 'Explore', component: () => import('../views/ExplorePage.vue'), meta: { title: '助手广场', public: true } },
  { path: '/monitor/test/:pluginId', name: 'MonitorTest', component: () => import('../views/MonitorTestPage.vue'), meta: { title: 'SNMP 测试' } },
  { path: '/monitor/versions/:pluginId', name: 'MonitorVersions', component: () => import('../views/MonitorVersionsPage.vue'), meta: { title: '插件版本管理' } },
  { path: '/account', name: 'Account', component: () => import('../views/AccountPage.vue'), meta: { title: '账户设置' } },
]

const router = createRouter({
  history: createWebHistory(),
  routes,
})

/**
 * 全局前置守卫（强制鉴权）
 *
 * - 首次导航时调用 /api/auth/me 检测登录状态（仅一次）
 * - 未登录访问受保护路由 → 重定向到 /login，并记录 redirect 参数
 * - 已登录访问 /login → 重定向到 /sessions
 *
 * 注意：useUserStore() 在守卫内部调用，确保 Pinia 已激活。
 */
router.beforeEach(async (to) => {
  const userStore = useUserStore()

  if (!userStore.checked) {
    await userStore.loadMe()
  }

  const isPublic = to.meta.public === true

  // 未登录访问受保护路由：
  // - 后端启用了任意登录方式（本地登录 or SSO provider）→ 重定向到登录页（强制鉴权）
  // - 后端完全未启用 auth（localEnabled=false 且无 provider）→ 放行（开发/演示场景）
  if (!userStore.authenticated && !isPublic) {
    if (!userStore.localEnabled && userStore.providers.length === 0) {
      return true
    }
    return { name: 'Login', query: to.fullPath !== '/' ? { redirect: to.fullPath } : undefined }
  }

  if (userStore.authenticated && to.name === 'Login') {
    return { path: '/sessions' }
  }

  return true
})

export default router
