<template>
  <div class="session-skeleton" aria-busy="true" aria-label="会话加载中">
    <!-- 顶部细进度条：霓虹青流光，提示正在加载 -->
    <div class="sk-progress"><span class="sk-progress-bar"></span></div>

  <!-- 骨架消息：codex transcript 风格（全宽，用户浅灰块 / 助手纯文本条） -->
  <div class="sk-body">
    <div
      v-for="row in rows"
      :key="row.key"
      class="sk-row"
      :class="row.side"
      :style="{ animationDelay: row.delay + 'ms' }"
    >
      <div class="sk-bubble" :style="{ width: row.width }">
        <span class="sk-line" style="width: 100%"></span>
        <span class="sk-line" :style="{ width: row.tail }"></span>
      </div>
    </div>
  </div>

    <div class="sk-hint">
      <span class="sk-dot"></span><span class="sk-dot"></span><span class="sk-dot"></span>
      正在恢复会话记录…
    </div>
  </div>
</template>

<script setup>
// 会话详情历史消息加载骨架屏。
// 交替渲染用户(右侧蓝色)/助手(左侧深色)气泡占位条，配合霓虹扫光与淡入上浮，
// 让加载过程贴合现有深色科技风，而非单调转圈。
const rows = [
  { key: 1, side: 'user', width: '46%', tail: '60%', delay: 0 },
  { key: 2, side: 'assistant', width: '72%', tail: '78%', delay: 120 },
  { key: 3, side: 'user', width: '38%', tail: '55%', delay: 240 },
  { key: 4, side: 'assistant', width: '66%', tail: '70%', delay: 360 },
  { key: 5, side: 'assistant', width: '52%', tail: '48%', delay: 480 },
]
</script>

<style scoped>
.session-skeleton {
  position: relative;
  display: flex;
  flex-direction: column;
  height: 100%;
  width: 100%;
  overflow: hidden;
}

/* === 顶部细进度条：自左向右循环的霓虹流光 === */
.sk-progress {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  height: 2px;
  background: rgba(0, 212, 255, 0.06);
  overflow: hidden;
}
.sk-progress-bar {
  position: absolute;
  top: 0;
  left: 0;
  height: 100%;
  width: 40%;
  border-radius: 2px;
  background: linear-gradient(90deg, transparent, var(--accent), var(--accent-hover), transparent);
  box-shadow: 0 0 12px var(--accent-glow);
  animation: sk-progress-slide 1.4s ease-in-out infinite;
}
@keyframes sk-progress-slide {
  0% { transform: translateX(-110%); }
  100% { transform: translateX(280%); }
}

.sk-body {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 18px;
  padding: 28px 4px 0;
}

/* 每条骨架消息：淡入 + 轻微上浮进场，错落延迟营造层次 */
.sk-row {
  display: flex;
  opacity: 0;
  animation: sk-row-in 0.5s cubic-bezier(0.22, 1, 0.36, 1) forwards;
}
.sk-row.user { justify-content: flex-end; }
.sk-row.assistant { justify-content: flex-start; }
@keyframes sk-row-in {
  from { opacity: 0; transform: translateY(10px); }
  to { opacity: 1; transform: translateY(0); }
}

/* 气泡容器：用户=蓝渐变，助手=深色卡；内部叠加呼吸扫光 */
.sk-bubble {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 14px 16px;
  border-radius: 12px;
  position: relative;
  overflow: hidden;
}
.sk-row.user .sk-bubble {
  background: var(--user-bg);
  border: 1px solid var(--user-border);
  border-bottom-right-radius: 4px;
  opacity: 0.55;
}
.sk-row.assistant .sk-bubble {
  background: var(--assistant-bg);
  border: 1px solid var(--assistant-border);
  border-bottom-left-radius: 4px;
}

/* 文本占位条 */
.sk-line {
  display: block;
  height: 11px;
  border-radius: 6px;
  background: rgba(255, 255, 255, 0.08);
}

/* 扫光：一条斜向霓虹高光在气泡上从左扫到右，循环呼吸 */
.sk-bubble::after {
  content: '';
  position: absolute;
  top: 0;
  left: 0;
  height: 100%;
  width: 45%;
  background: linear-gradient(
    100deg,
    transparent 0%,
    rgba(0, 212, 255, 0.10) 45%,
    rgba(56, 230, 255, 0.18) 50%,
    rgba(0, 212, 255, 0.10) 55%,
    transparent 100%
  );
  transform: skewX(-18deg) translateX(-160%);
  animation: sk-shimmer 1.9s ease-in-out infinite;
}
@keyframes sk-shimmer {
  0% { transform: skewX(-18deg) translateX(-160%); }
  100% { transform: skewX(-18deg) translateX(360%); }
}

/* 底部提示：三点跳动 + 文案 */
.sk-hint {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 22px 0 6px;
  font-size: 12.5px;
  color: var(--muted);
  letter-spacing: 0.3px;
}
.sk-dot {
  width: 5px;
  height: 5px;
  border-radius: 50%;
  background: var(--accent);
  box-shadow: 0 0 8px var(--accent-glow);
  animation: sk-dot-bounce 1.2s ease-in-out infinite;
}
.sk-dot:nth-child(2) { animation-delay: 0.15s; }
.sk-dot:nth-child(3) { animation-delay: 0.3s; }
@keyframes sk-dot-bounce {
  0%, 60%, 100% { transform: translateY(0); opacity: 0.5; }
  30% { transform: translateY(-4px); opacity: 1; }
}

/* 尊重系统减弱动效偏好：关闭循环动画，仅保留静态骨架 */
@media (prefers-reduced-motion: reduce) {
  .sk-progress-bar, .sk-bubble::after, .sk-dot { animation: none; }
  .sk-row { animation-duration: 0.01s; }
}
</style>
