import { marked } from 'marked'
import { markedHighlight } from 'marked-highlight'
import hljs from 'highlight.js/lib/core'
import bash from 'highlight.js/lib/languages/bash'
import python from 'highlight.js/lib/languages/python'
import javascript from 'highlight.js/lib/languages/javascript'
import json from 'highlight.js/lib/languages/json'
import xml from 'highlight.js/lib/languages/xml'
import css from 'highlight.js/lib/languages/css'
import rust from 'highlight.js/lib/languages/rust'
import sql from 'highlight.js/lib/languages/sql'
import yaml from 'highlight.js/lib/languages/yaml'
import go from 'highlight.js/lib/languages/go'
import cpp from 'highlight.js/lib/languages/cpp'
import java from 'highlight.js/lib/languages/java'
import ini from 'highlight.js/lib/languages/ini'
import markdown from 'highlight.js/lib/languages/markdown'

hljs.registerLanguage('bash', bash)
hljs.registerLanguage('shell', bash)
hljs.registerLanguage('python', python)
hljs.registerLanguage('javascript', javascript)
hljs.registerLanguage('json', json)
hljs.registerLanguage('xml', xml)
hljs.registerLanguage('html', xml)
hljs.registerLanguage('css', css)
hljs.registerLanguage('rust', rust)
hljs.registerLanguage('sql', sql)
hljs.registerLanguage('yaml', yaml)
hljs.registerLanguage('go', go)
hljs.registerLanguage('cpp', cpp)
hljs.registerLanguage('c', cpp)
hljs.registerLanguage('java', java)
hljs.registerLanguage('ini', ini)
hljs.registerLanguage('markdown', markdown)

// marked v5 起废弃、v12 起彻底移除了 setOptions 的 highlight 选项；
// 改用官方 marked-highlight 扩展恢复 fenced code block 的 hljs 语法高亮。
function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}

marked.use(
  markedHighlight({
    emptyLangClass: 'hljs',
    langPrefix: 'hljs language-',
    highlight(code, lang) {
      if (lang && hljs.getLanguage(lang)) {
        try {
          return hljs.highlight(code, { language: lang }).value
        } catch (_) {}
      }
      // 未注册语言：手动转义，避免原始 < > & 破坏 HTML 结构
      return escapeHtml(code)
    },
  }),
)
marked.setOptions({ breaks: false, gfm: true })

// 判定一个字符是否为 CJK 字符（含中文标点、全角符号）
function isCjkChar(ch) {
  if (!ch) return false
  const c = ch.codePointAt(0)
  return (
    (c >= 0x4e00 && c <= 0x9fff) || // CJK 统一汉字
    (c >= 0x3400 && c <= 0x4dbf) || // CJK 扩展 A
    (c >= 0x3000 && c <= 0x303f) || // CJK 标点符号
    (c >= 0xff00 && c <= 0xffef)    // 全角字符
  )
}

// 判定某行是否为 Markdown 结构行（标题 / 引用 / 列表 / 表格 / 代码栅栏 / 分隔线）
function isMarkdownStructuralLine(line) {
  const t = line.replace(/^\s+/, '')
  if (t.startsWith('```') || t.startsWith('~~~')) return true
  if (/^#{1,6}\s/.test(t)) return true
  if (/^>\s?/.test(t)) return true
  if (/^[-*+]\s/.test(t)) return true
  if (/^\d+[.)]\s/.test(t)) return true
  if (t.includes('|')) return true
  if (/^(-{3,}|\*{3,}|_{3,})\s*$/.test(t)) return true
  return false
}

// 规整流式文本里多余的换行：模型在叙述阶段常在 token 之间夹带 \n，
// 导致「值\n已\n设置」被逐字断行。这里把连续的「普通正文行」合并为一行，
// 同时保留 Markdown 结构（空行=段落、标题/列表/表格/代码块）不被破坏。
function normalizeStreamingNewlines(text) {
  const lines = String(text || '').split('\n')
  const out = []
  let inCode = false

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]
    const trimmedStart = line.replace(/^\s+/, '')

    // 代码栅栏开关
    if (trimmedStart.startsWith('```') || trimmedStart.startsWith('~~~')) {
      inCode = !inCode
      out.push(line)
      continue
    }
    // 代码块内部原样保留
    if (inCode) {
      out.push(line)
      continue
    }
    // 空行 = 段落边界，保留
    if (line.trim() === '') {
      out.push(line)
      continue
    }
    // Markdown 结构行保留
    if (isMarkdownStructuralLine(line)) {
      out.push(line)
      continue
    }

    // 普通正文行：与后续连续的正文行合并
    let merged = line
    while (i + 1 < lines.length) {
      const next = lines[i + 1]
      const nextTrimStart = next.replace(/^\s+/, '')
      if (next.trim() === '') break
      if (nextTrimStart.startsWith('```') || nextTrimStart.startsWith('~~~')) break
      if (isMarkdownStructuralLine(next)) break
      const lastChar = merged.slice(-1)
      const firstChar = nextTrimStart[0] || ''
      if (isCjkChar(lastChar) && isCjkChar(firstChar)) {
        merged += nextTrimStart
      } else {
        merged += ' ' + nextTrimStart
      }
      i++
    }
    out.push(merged)
  }

  return out.join('\n')
}

export function renderMd(text) {
  let html = marked.parse(normalizeStreamingNewlines(text || ''))
  // 带语言标签的代码块：注入头部（语言标签 + 复制按钮）。
  // 复制按钮不再使用内联 onclick，改由 markdown 容器的事件委托统一处理。
  html = html.replace(
    /<pre><code class="hljs language-(\w+)">/g,
    (_, lang) =>
      `<pre class="code-block"><div class="code-header"><span class="code-lang">${lang}</span><button type="button" class="copy-btn">复制</button></div><code class="hljs language-${lang}">`,
  )
  // 无语言标签的代码块：仅注入复制按钮头部
  html = html.replace(
    /<pre><code class="hljs">/g,
    '<pre class="code-block"><div class="code-header"><button type="button" class="copy-btn">复制</button></div><code class="hljs">',
  )
  return html
}
