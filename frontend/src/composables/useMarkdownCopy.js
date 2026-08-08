/**
 * markdown 代码块「复制」按钮的事件委托。
 *
 * renderMd 通过 v-html 注入的 HTML 无法直接绑定 Vue 事件（也没有内联 onclick，
 * 以符合 CSP 最佳实践），因此在渲染容器上委托 click：命中 .copy-btn 时
 * 复制同一 <pre> 内的 <code> 文本，并短暂切换按钮文案。
 */
export function useMarkdownCopy() {
  function onCopyClick(e) {
    const btn = e.target?.closest?.('.copy-btn')
    if (!btn) return
    const code = btn.closest('pre')?.querySelector('code')?.textContent || ''
    navigator.clipboard
      .writeText(code)
      .then(() => {
        btn.textContent = '已复制'
        setTimeout(() => {
          btn.textContent = '复制'
        }, 1500)
      })
      .catch(() => {})
  }
  return { onCopyClick }
}
