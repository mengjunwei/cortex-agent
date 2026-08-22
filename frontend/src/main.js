import { createApp } from 'vue'
import { createPinia } from 'pinia'
import 'element-plus/theme-chalk/dark/css-vars.css'
import 'element-plus/es/components/message-box/style/css'
import 'element-plus/es/components/message/style/css'
import 'highlight.js/styles/github-dark.css'
import router from './router'
import App from './App.vue'
import './styles/global.css'

document.documentElement.classList.add('dark')

const app = createApp(App)
app.use(createPinia())
app.use(router)
app.mount('#app')
