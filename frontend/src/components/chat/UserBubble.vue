<template>
  <!-- 用户消息：靠右蓝色渐变气泡（经典聊天布局） -->
  <div class="msg user">
    <div v-if="attachments?.length" class="user-attachments">
      <img
        v-for="(a, i) in attachments"
        :key="i"
        :src="a.url"
        :alt="a.filename || '图片'"
        class="user-attachment-img"
        @click="$emit('preview-image', a.url)"
      />
    </div>
    <div v-if="content" class="md-content md-user" v-html="html" @click="onCopyClick"></div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { renderMd } from '../../utils/markdown'
import { useMarkdownCopy } from '../../composables/useMarkdownCopy'

const props = defineProps({
  content: { type: String, default: '' },
  attachments: { type: Array, default: () => [] },
})
defineEmits(['preview-image'])

const html = computed(() => renderMd(props.content))
const { onCopyClick } = useMarkdownCopy()
</script>

<style scoped>
.user-attachments {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
  margin-bottom: 6px;
}
.user-attachment-img {
  width: 120px;
  height: 120px;
  object-fit: cover;
  border-radius: var(--radius-sm);
  cursor: zoom-in;
  border: 1px solid var(--border);
  transition: transform 0.15s;
}
.user-attachment-img:hover {
  transform: scale(1.03);
}
/* 用户气泡为蓝色渐变底，行内 code / 引用用浅色保证可读 */
.md-user :not(pre) > code {
  background: rgba(255, 255, 255, 0.16);
  color: #e0f2fe;
  border-color: rgba(255, 255, 255, 0.2);
}
</style>
