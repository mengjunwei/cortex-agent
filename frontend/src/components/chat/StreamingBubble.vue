<template>
  <div class="msg assistant">
    <div v-if="text" class="md-content" v-html="html" @click="onCopyClick"></div>
    <span class="typing"></span>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { renderMd } from '../../utils/markdown'
import { useMarkdownCopy } from '../../composables/useMarkdownCopy'

const props = defineProps({
  text: { type: String, default: '' },
})

const html = computed(() => renderMd(props.text))
const { onCopyClick } = useMarkdownCopy()
</script>
