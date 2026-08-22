<template>
  <!-- 用户消息：靠右蓝色渐变气泡（经典聊天布局） -->
  <div class="msg user">
    <div v-if="attachments?.length" class="user-attachments">
      <template v-for="(a, i) in attachments" :key="i">
        <!-- 图片：缩略图，点击预览 -->
        <img
          v-if="isImage(a)"
          :src="a.url"
          :alt="a.filename || '图片'"
          class="user-attachment-img"
          @click="$emit('preview-image', a.url)"
        />
        <!-- 文档：文件名胶囊，点击新窗口打开（下载/预览） -->
        <a
          v-else
          :href="a.url"
          target="_blank"
          rel="noopener"
          class="user-attachment-doc"
          :title="a.filename || '文档'"
        >
          <el-icon class="doc-icon"><Document /></el-icon>
          <span class="doc-name">{{ a.filename || '文档' }}</span>
        </a>
      </template>
    </div>
    <div v-if="content" class="md-content md-user" v-html="html" @click="onCopyClick"></div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { Document } from '@element-plus/icons-vue'
import { renderMd } from '../../utils/markdown'
import { useMarkdownCopy } from '../../composables/useMarkdownCopy'

const props = defineProps({
  content: { type: String, default: '' },
  attachments: { type: Array, default: () => [] },
})
defineEmits(['preview-image'])

const html = computed(() => renderMd(props.content))
const { onCopyClick } = useMarkdownCopy()

// 文档附件：MIME 非 image/* 即视为文档（后端已按类型分流解析）
const isImage = (a) => (a.mime_type || '').startsWith('image/')
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
/* 文档附件胶囊 */
.user-attachment-doc {
  display: flex;
  align-items: center;
  gap: 6px;
  max-width: 220px;
  height: 120px;
  box-sizing: border-box;
  padding: 0 14px;
  border-radius: var(--radius-sm);
  border: 1px solid rgba(255, 255, 255, 0.35);
  background: rgba(255, 255, 255, 0.12);
  color: #fff;
  text-decoration: none;
  font-size: 13px;
  transition: background 0.15s;
}
.user-attachment-doc:hover {
  background: rgba(255, 255, 255, 0.22);
}
.user-attachment-doc .doc-icon {
  flex-shrink: 0;
  font-size: 18px;
}
.user-attachment-doc .doc-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* 用户气泡为蓝色渐变底，行内 code / 引用用浅色保证可读 */
.md-user :not(pre) > code {
  background: rgba(255, 255, 255, 0.16);
  color: #e0f2fe;
  border-color: rgba(255, 255, 255, 0.2);
}
</style>
