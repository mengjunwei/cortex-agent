import { defineStore } from 'pinia'
import { ref } from 'vue'
import { fetchModels, fetchCatalog } from '../api'

export const useAppStore = defineStore('app', () => {
  const models = ref([])
  const defaultModelId = ref('')
  const catalog = ref({ brands: [], dev_types: [] })
  const sidebarCollapsed = ref(false)

  async function loadModels() {
    try {
      const { data, code } = await fetchModels()
      if (code === 0) {
        models.value = data.models || []
        defaultModelId.value = data.default_model_id || ''
      }
    } catch (_) {}
  }

  async function loadCatalog() {
    try {
      const { data, code } = await fetchCatalog()
      if (code === 0) catalog.value = data
    } catch (_) {}
  }

  function toggleSidebar() { sidebarCollapsed.value = !sidebarCollapsed.value }

  return { models, defaultModelId, catalog, sidebarCollapsed, loadModels, loadCatalog, toggleSidebar }
})
