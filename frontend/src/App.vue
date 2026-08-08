<template>
  <!-- 公共路由（登录页等）：纯净全屏，无侧边栏/顶栏 -->
  <div v-if="isPublicRoute" class="bare-layout">
    <router-view />
    <div class="toast-container" id="toast-container"></div>
  </div>

  <!-- 应用主框架：侧边栏 + 顶栏 -->
  <div v-else class="app-layout">
    <!-- 侧边栏 -->
    <aside class="sidebar" :class="{ collapsed: appStore.sidebarCollapsed }">
      <div class="sidebar-header">
        <div class="logo-area" v-show="!appStore.sidebarCollapsed">
          <div class="logo-icon">
            <svg width="28" height="28" viewBox="0 0 28 28" fill="none">
              <rect x="2" y="2" width="10" height="10" rx="2" fill="#00d4ff" opacity="0.9"/>
              <rect x="16" y="2" width="10" height="10" rx="2" fill="#0ea5e9" opacity="0.7"/>
              <rect x="2" y="16" width="10" height="10" rx="2" fill="#0ea5e9" opacity="0.7"/>
              <rect x="16" y="16" width="10" height="10" rx="2" fill="#00d4ff" opacity="0.5"/>
            </svg>
          </div>
          <h1>cortex-agent</h1>
        </div>
        <button class="sidebar-toggle" @click="appStore.toggleSidebar">
          <el-icon><DArrowLeft v-if="!appStore.sidebarCollapsed" /><DArrowRight v-else /></el-icon>
        </button>
      </div>

      <div class="nav-section" v-show="!appStore.sidebarCollapsed">
        <div
          v-for="(group, gi) in navGroups"
          :key="group.name"
          class="nav-group"
          :class="['group-tint-' + (gi + 1)]"
        >
          <div class="nav-group-header" @click="toggleGroup(gi)">
            <span class="nav-group-name">{{ group.name }}</span>
            <el-icon class="nav-group-arrow" :class="{ collapsed: collapsedGroups.includes(gi) }">
              <ArrowDown />
            </el-icon>
          </div>
          <div class="nav-group-body" v-show="!collapsedGroups.includes(gi)">
            <el-tooltip
              v-for="item in group.items"
              :key="item.path"
              :content="item.tooltip"
              placement="right"
              :show-after="400"
              :hide-after="0"
            >
              <router-link
                :to="navTarget(item.path)"
                class="nav-item"
                :class="{ active: isItemActive(item) }"
              >
                <span class="nav-icon-chip"><span class="nav-icon"><component :is="item.icon" :size="16" /></span></span>
                <span class="nav-text">{{ item.label }}</span>
                <div class="nav-glow" v-if="route.path === item.path"></div>
              </router-link>
            </el-tooltip>
          </div>
        </div>
      </div>

      <div class="sidebar-footer" v-show="!appStore.sidebarCollapsed">
        <div class="footer-line"></div>
        <!-- 当前登录用户（点击登出） -->
        <el-tooltip v-if="userStore.user" content="退出登录" placement="right" :show-after="400" :hide-after="0">
          <div class="settings-item" @click="confirmLogout">
            <img
              v-if="userStore.user.avatar"
              :src="userStore.user.avatar"
              class="footer-avatar"
              alt="avatar"
              referrerpolicy="no-referrer"
            />
            <span v-else class="footer-avatar placeholder">{{ avatarLetter }}</span>
            <span class="nav-text">{{ userStore.user.name || '用户' }}</span>
          </div>
        </el-tooltip>
        <el-tooltip v-else content="打开个人设置" placement="right" :show-after="400" :hide-after="0">
          <div class="settings-item" @click="openSettings">
            <span class="nav-icon-chip"><span class="nav-icon"><Settings :size="16" /></span></span>
            <span class="nav-text">个人设置</span>
          </div>
        </el-tooltip>
        <div class="footer-text">v2.0.0</div>
      </div>
    </aside>

    <!-- 主内容 -->
    <main class="main-content" :class="{ 'sb-collapsed': appStore.sidebarCollapsed }">
      <!-- 侧边栏收起时的浮动展开按钮（不依赖 header，所有页面可用） -->
      <button
        v-if="appStore.sidebarCollapsed"
        class="sidebar-float-btn"
        @click="appStore.toggleSidebar"
        title="展开侧边栏"
      >
        <el-icon><DArrowRight /></el-icon>
      </button>

      <header class="main-header" v-if="!isSessionsPage">
        <div class="header-title">
          <div class="title-accent"></div>
          <h2>{{ route.meta.title }}</h2>
        </div>
        <div class="header-right">
          <div class="status-dot"></div>
          <span class="status-text">系统就绪</span>
          <el-dropdown v-if="userStore.user" trigger="click" @command="onUserCommand">
            <div class="user-chip">
              <img
                v-if="userStore.user.avatar"
                :src="userStore.user.avatar"
                class="user-avatar"
                alt="avatar"
                referrerpolicy="no-referrer"
              />
              <span v-else class="user-avatar placeholder">{{ avatarLetter }}</span>
              <span class="user-name">{{ userStore.user.name || '用户' }}</span>
              <el-icon class="user-caret"><ArrowDown /></el-icon>
            </div>
            <template #dropdown>
              <el-dropdown-menu>
                <el-dropdown-item disabled>
                  <span class="dd-uid">{{ userStore.user.name || '未命名' }}</span>
                </el-dropdown-item>
                <el-dropdown-item command="account">
                  <el-icon><Settings /></el-icon> 账户设置
                </el-dropdown-item>
                <el-dropdown-item divided command="logout">
                  <el-icon><SwitchButton /></el-icon> 退出登录
                </el-dropdown-item>
              </el-dropdown-menu>
            </template>
          </el-dropdown>
        </div>
      </header>
      <div class="page-content" :class="{ 'chat-mode': route.path === '/chat', 'bare-mode': isSessionsPage, 'sidebar-collapsed': appStore.sidebarCollapsed }">
        <router-view />
      </div>
    </main>

    <!-- Toast 容器 -->
    <div class="toast-container" id="toast-container"></div>
  </div>
</template>

<script setup>
import { ref, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import { DArrowLeft, DArrowRight, ArrowDown, SwitchButton } from '@element-plus/icons-vue'
import { MessageCircle, Bot, Compass, Library, Cpu, Plug, Settings, Brain, Sparkles } from 'lucide-vue-next'
import { useAppStore } from './stores/app'
import { useChatStore } from './stores/chat'
import { useUserStore } from './stores/user'

const route = useRoute()
const router = useRouter()
const appStore = useAppStore()
const chatStore = useChatStore()
const userStore = useUserStore()

const isSessionsPage = computed(() => route.path === '/sessions')
// 公共路由（登录页等）使用纯净全屏布局，不渲染侧边栏/顶栏
const isPublicRoute = computed(() => route.meta.public === true)

// 用户头像占位字母（取昵称首字符，无昵称时用「U」）
const avatarLetter = computed(() => {
  const name = userStore.user?.name
  return name && name.length ? name.charAt(0).toUpperCase() : 'U'
})

// 顶栏下拉命令处理
async function onUserCommand(cmd) {
  if (cmd === 'logout') {
    await confirmLogout()
  } else if (cmd === 'account') {
    router.push('/account')
  }
}

// 退出登录确认
async function confirmLogout() {
  try {
    await ElMessageBox.confirm('确定要退出登录吗？', '退出确认', {
      confirmButtonText: '退出',
      cancelButtonText: '取消',
      type: 'warning',
    })
  } catch (_) {
    return
  }
  await userStore.doLogout()
  ElMessage.success('已退出登录')
  // 重置检测标志，使守卫重新走 loadMe 流程并拦截到登录页
  userStore.checked = false
  router.push('/login')
}

// 计算 router-link 的目标：/chat 页面保留当前 session_id，避免点击菜单丢会话
function navTarget(path) {
  if (path === '/chat') {
    const sessionId = route.query.session
    return sessionId ? { path: '/chat', query: { session: sessionId } } : '/chat'
  }
  return path
}

// 计算菜单激活态：处理子路由（/assistants/new、/assistants/:id/edit）也激活父菜单
function isItemActive(item) {
  if (route.path === item.path) return true
  if (item.path === '/assistants' && route.path.startsWith('/assistants/')) return true
  if (item.path === '/knowledge' && route.path.startsWith('/knowledge/')) return true
  return false
}

const navGroups = [
  {
    name: '首页',
    items: [
      { path: '/sessions', icon: MessageCircle, label: '会话历史', tooltip: '查看和管理所有对话' },
    ],
  },
  {
    name: '智能助手',
    items: [
      { path: '/assistants', icon: Bot, label: '助手管理', tooltip: '配置专属助手：提示词、工具与参数' },
      { path: '/explore', icon: Compass, label: '助手广场', tooltip: '浏览社区分享的助手，凭分享码导入' },
      { path: '/memories', icon: Brain, label: '我的记忆', tooltip: '跨会话的习惯与避坑记录，对话自动带上' },
    ],
  },
  // 「运维工具」菜单组暂隐藏（监控插件助手已下线）；/monitor 路由保留。如需恢复：取消下方注释，并在上方 lucide 导入里补回 Activity 图标：
  // {
  //   name: '运维工具',
  //   items: [
  //     { path: '/monitor', icon: Activity, label: '监控插件管理', tooltip: '查看已注册插件、SNMP 测试与版本管理' },
  //   ],
  // },
  {
    name: '系统配置',
    items: [
      { path: '/knowledge', icon: Library, label: '知识库管理', tooltip: '管理知识库文档与配置' },
      { path: '/model-providers', icon: Cpu, label: '模型供应商管理', tooltip: '配置 OpenAI 协议模型接入、API Key 与默认模型' },
      { path: '/mcp-servers', icon: Plug, label: 'MCP 服务管理', tooltip: '接入外部 MCP 工具源、健康探测与工具清单' },
      { path: '/skills', icon: Sparkles, label: 'Skill 管理', tooltip: '查看已加载的 Skill 目录；新增 Skill 后点重新扫描生效' },
    ],
  },
]

const collapsedGroups = ref([])
function toggleGroup(i) {
  const idx = collapsedGroups.value.indexOf(i)
  if (idx >= 0) collapsedGroups.value.splice(idx, 1)
  else collapsedGroups.value.push(i)
}

function openSettings() {
  router.push('/account')
}

onMounted(async () => {
  await Promise.all([
    appStore.loadModels(),
    appStore.loadCatalog(),
  ])
})
</script>

<style scoped>
.app-layout { display: flex; height: 100vh; overflow: hidden; background: var(--bg); }

/* 公共路由（登录页）纯净全屏布局 */
.bare-layout { height: 100vh; background: var(--bg); overflow: hidden; }

/* === 侧边栏 === */
.sidebar {
  width: 18%; min-width: 220px; max-width: 300px;
  background: var(--sidebar);
  border-right: 1px solid var(--border);
  display: flex; flex-direction: column; transition: width .3s ease, min-width .3s ease;
  overflow: hidden; position: relative;
}
.sidebar::before {
  content: ''; position: absolute; inset: 0;
  background:
    radial-gradient(ellipse 80% 50% at 20% 40%, rgba(0, 212, 255, 0.03) 0%, transparent 50%),
    radial-gradient(ellipse 60% 40% at 80% 80%, rgba(14, 165, 233, 0.02) 0%, transparent 50%);
  pointer-events: none;
}
.sidebar.collapsed { width: 0; min-width: 0; border-right: none; }

.sidebar-header {
  display: flex; align-items: center; justify-content: space-between;
  padding: 20px 16px 16px; border-bottom: 1px solid var(--border); position: relative; z-index: 1;
}
.logo-area { display: flex; align-items: center; gap: 10px; }
.logo-icon {
  width: 32px; height: 32px; border-radius: 8px;
  background: linear-gradient(135deg, rgba(0, 212, 255, 0.15) 0%, rgba(14, 165, 233, 0.1) 100%);
  border: 1px solid rgba(0, 212, 255, 0.2);
  display: flex; align-items: center; justify-content: center;
  box-shadow: 0 0 12px rgba(0, 212, 255, 0.1);
}
.sidebar-header h1 { font-size: 15px; font-weight: 800; white-space: nowrap; color: var(--text-h); letter-spacing: -0.3px; }
.sidebar-toggle {
  background: none; border: none; color: var(--muted); cursor: pointer; font-size: 16px;
  display: flex; align-items: center; justify-content: center; width: 28px; height: 28px; border-radius: 6px;
  transition: all 0.2s;
}
.sidebar-toggle:hover { background: var(--accent-dim); color: var(--accent); }

/* 导航 */
.nav-section {
  padding: 14px 10px; display: flex; flex-direction: column; gap: 10px;
  flex: 1; position: relative; z-index: 1; overflow-y: auto;
}
.nav-section::-webkit-scrollbar { width: 4px; }
.nav-section::-webkit-scrollbar-thumb { background: var(--border); border-radius: 2px; }

.nav-group { display: flex; flex-direction: column; border-radius: var(--radius); padding: 6px; }
.nav-group.group-tint-1 { background: rgba(0, 212, 255, 0.035); border: 1px solid rgba(0, 212, 255, 0.08); }
.nav-group.group-tint-2 { background: rgba(16, 185, 129, 0.03); border: 1px solid rgba(16, 185, 129, 0.07); }
.nav-group.group-tint-3 { background: rgba(14, 165, 233, 0.03); border: 1px solid rgba(14, 165, 233, 0.07); }
.nav-group.group-tint-4 { background: rgba(139, 92, 246, 0.03); border: 1px solid rgba(139, 92, 246, 0.07); }

.nav-group-header {
  display: flex; align-items: center; justify-content: space-between;
  padding: 6px 10px; cursor: pointer; user-select: none; border-radius: var(--radius-sm);
}
.nav-group-header:hover { background: rgba(255, 255, 255, 0.03); }
.nav-group-name {
  font-size: 10px; font-weight: 800; color: var(--muted);
  text-transform: uppercase; letter-spacing: 1px;
}
.nav-group-arrow { font-size: 12px; color: var(--muted); transition: transform .2s ease; }
.nav-group-arrow.collapsed { transform: rotate(-90deg); }
.nav-group-body { display: flex; flex-direction: column; gap: 2px; margin-top: 4px; }

.nav-item {
  display: flex; align-items: center; gap: 10px; padding: 9px 10px; border-radius: var(--radius-sm);
  color: var(--muted); text-decoration: none; font-size: 13px; transition: all .2s ease;
  position: relative; overflow: hidden; font-weight: 600;
}
.nav-item:hover { background: rgba(0, 212, 255, 0.06); color: var(--text); }
.nav-item.active {
  background: linear-gradient(90deg, rgba(0, 212, 255, 0.14) 0%, rgba(0, 212, 255, 0.03) 100%);
  color: var(--accent); font-weight: 700;
}
.nav-item.active::before {
  content: ''; position: absolute; left: 0; top: 50%; transform: translateY(-50%);
  width: 3px; height: 18px; background: var(--accent);
  border-radius: 0 2px 2px 0; box-shadow: 0 0 8px var(--accent-glow);
}
.nav-glow {
  position: absolute; right: -20px; top: 50%; transform: translateY(-50%);
  width: 40px; height: 40px; border-radius: 50%;
  background: radial-gradient(circle, rgba(0, 212, 255, 0.15) 0%, transparent 70%);
  pointer-events: none;
}
.nav-icon-chip {
  width: 26px; height: 26px; border-radius: 7px; flex-shrink: 0;
  display: flex; align-items: center; justify-content: center;
  background: rgba(255, 255, 255, 0.04); border: 1px solid var(--border);
}
.nav-item.active .nav-icon-chip {
  background: rgba(0, 212, 255, 0.12); border-color: rgba(0, 212, 255, 0.3);
}
.nav-icon { display: flex; align-items: center; justify-content: center; color: var(--muted-light); transition: color 0.2s; }
.nav-item.active .nav-icon { color: var(--accent); }
.nav-text { white-space: nowrap; }

.sidebar-footer { padding: 12px 10px; position: relative; z-index: 1; }
.footer-line { height: 1px; background: linear-gradient(90deg, transparent, var(--border), transparent); margin-bottom: 10px; }
.footer-text { font-size: 11px; color: var(--muted); text-align: center; font-family: var(--font-mono); margin-top: 10px; }
.settings-item {
  display: flex; align-items: center; gap: 10px; padding: 9px 10px; border-radius: var(--radius-sm);
  color: var(--muted); cursor: pointer; font-size: 13px; font-weight: 600; transition: all .2s ease;
}
.settings-item:hover { background: rgba(0, 212, 255, 0.06); color: var(--text); }

/* === 主内容 === */
.main-content { flex: 1; display: flex; flex-direction: column; overflow: hidden; min-width: 0; position: relative; }
.main-content::before {
  content: ''; position: absolute; inset: 0; pointer-events: none;
  background:
    radial-gradient(ellipse 60% 40% at 50% 0%, rgba(0, 212, 255, 0.02) 0%, transparent 50%);
}
.main-header {
  display: flex; align-items: center; gap: 12px; padding: 12px 20px;
  border-bottom: 1px solid var(--border); min-height: 56px; flex-shrink: 0;
  position: relative; z-index: 1;
  background: linear-gradient(180deg, rgba(6, 6, 10, 0.9) 0%, rgba(6, 6, 10, 0.6) 100%);
  backdrop-filter: blur(12px);
}
.header-title { display: flex; align-items: center; gap: 10px; flex: 1; }
.title-accent {
  width: 4px; height: 20px; border-radius: 2px;
  background: linear-gradient(180deg, var(--accent) 0%, var(--accent-secondary) 100%);
  box-shadow: 0 0 8px var(--accent-glow);
}
.main-header h2 { font-size: 16px; font-weight: 700; color: var(--text-h); letter-spacing: -0.2px; }
/* 侧边栏收起时，非会话页的 header 需要给浮动按钮让出空间 */
.main-content.sb-collapsed .main-header { padding-left: 60px; }
.sidebar-float-btn {
  position: absolute;
  top: 14px;
  left: 14px;
  z-index: 50;
  background: var(--card);
  border: 1px solid var(--border);
  color: var(--muted);
  cursor: pointer;
  display: flex; align-items: center; justify-content: center;
  width: 34px; height: 34px; border-radius: 8px;
  transition: all 0.2s;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
}
.sidebar-float-btn:hover {
  background: var(--accent-dim);
  color: var(--accent);
  border-color: var(--accent);
  box-shadow: 0 0 12px rgba(0, 212, 255, 0.2);
}
.header-right { display: flex; align-items: center; gap: 8px; }
.status-dot {
  width: 7px; height: 7px; border-radius: 50%; background: var(--done);
  box-shadow: 0 0 6px rgba(16, 185, 129, 0.5);
  animation: pulse 2s infinite;
}
@keyframes pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.5; }
}
.status-text { font-size: 12px; color: var(--muted); font-weight: 500; }

/* === 顶栏用户区 === */
.user-chip {
  display: flex; align-items: center; gap: 8px;
  padding: 4px 10px 4px 4px; border-radius: 20px;
  background: rgba(255, 255, 255, 0.03); border: 1px solid var(--border);
  cursor: pointer; transition: all 0.2s ease; outline: none;
}
.user-chip:hover { background: var(--accent-dim); border-color: rgba(0, 212, 255, 0.3); }
.user-avatar {
  width: 26px; height: 26px; border-radius: 50%; object-fit: cover; flex-shrink: 0;
  border: 1px solid var(--border);
}
.user-avatar.placeholder {
  display: flex; align-items: center; justify-content: center;
  background: linear-gradient(135deg, var(--accent-secondary), var(--accent));
  color: #06060a; font-weight: 800; font-size: 13px; border: none;
}
.user-name {
  font-size: 13px; font-weight: 600; color: var(--text);
  max-width: 120px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.user-caret { font-size: 11px; color: var(--muted); }
.dd-uid { font-weight: 600; color: var(--text); }

/* === 侧边栏页脚头像 === */
.footer-avatar {
  width: 26px; height: 26px; border-radius: 7px; object-fit: cover; flex-shrink: 0;
  border: 1px solid var(--border);
}
.footer-avatar.placeholder {
  display: flex; align-items: center; justify-content: center;
  background: linear-gradient(135deg, var(--accent-secondary), var(--accent));
  color: #06060a; font-weight: 800; font-size: 13px; border: none;
}

.page-content { flex: 1; overflow-y: auto; padding: 20px; position: relative; z-index: 1; }
.page-content.chat-mode { padding: 0; overflow: hidden; }
.page-content.bare-mode { padding: 20px 24px; }
/* 侧边栏收起时，会话列表页顶部留出浮动展开按钮的空间 */
.page-content.bare-mode.sidebar-collapsed { padding-left: 58px; }
</style>
