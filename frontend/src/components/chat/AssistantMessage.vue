<template>
  <div class="msg assistant">
    <div class="md-content" v-html="html" @click="onCopyClick"></div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { renderMd } from '../../utils/markdown'
import { useMarkdownCopy } from '../../composables/useMarkdownCopy'

const props = defineProps({
  content: { type: String, default: '' },
})

const html = computed(() => renderMd(props.content))
const { onCopyClick } = useMarkdownCopy()
</script>
