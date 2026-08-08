/**
 * 助手枚举映射（前后端数字编码 ↔ 可读标签）
 *
 * 与后端 src/domain/assistant/enums.rs 的 i16 编码一一对应。
 * 设计文档 §4.4 规定：存储用 SMALLINT 数字，前后端各自维护映射表翻译为可读值。
 */

export const KIND = { BUILTIN: 0, CUSTOM: 1 }
export const KIND_LABEL = { 0: '内置', 1: '自定义' }

export const AGENT_TYPE = {
  DEVICE_COMMAND: 2,
  MONITOR_PLUGIN: 4,
  CUSTOM: 9,
}

export const AGENT_TYPE_LABEL = {
  2: '设备命令',
  4: '监控插件',
  9: '自定义',
}

export const AGENT_TYPE_KEY_LABEL = {
  device_command: '设备命令',
  monitor_plugin: '监控插件',
  custom: '自定义',
}

export const VISIBILITY = { PRIVATE: 0, SHARED: 1, BUILTIN: 2 }
export const VISIBILITY_LABEL = { 0: '私有', 1: '共享', 2: '内置' }

/** 内置助手的固定 ID（与后端 seed_builtin 一致，用于直链开聊） */
export const BUILTIN_IDS = {
  DEVICE_COMMAND: '01950000-0000-7000-8000-000000000003',
  MONITOR_PLUGIN: '01950000-0000-7000-8000-000000000005',
}

/** 常用 emoji 头像候选（编辑页头像选择器） */
export const AVATAR_CHOICES = [
  '🤖', '🧭', '💬', '🛠️', '💡', '📈', '🌐', '🔧', '🚀', '⚡',
  '🧠', '📚', '🔍', '🎯', '✨', '🔮', '🛡️', '📡', '🗄️', '💻',
]
