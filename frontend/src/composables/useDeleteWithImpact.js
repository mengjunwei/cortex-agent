import { ElMessage, ElMessageBox } from 'element-plus'

/**
 * 通用「删除预检 → 带影响清单确认 → 真删」流程。
 *
 * 配合后端 deleteXxx(id, force) 两步合一接口：
 * - force=false：后端只做只读预检，返回 { deleted:false, impact:{...}, summary:"..." }
 * - force=true：后端在单个事务内级联清理引用并删除（保留引用方主体）
 *
 * 用法：
 *   await confirmDeleteWithImpact({
 *     id: a.id,
 *     removeFn: deleteAssistant,
 *     title: '删除助手',
 *     targetLabel: a.name,
 *     onSuccess: () => loadAssistants(),
 *   })
 *
 * @param {object} cfg
 * @param {string} cfg.id 待删对象 id
 * @param {(id:string, force:boolean)=>Promise<{data:any,code:number,message:string}>} cfg.removeFn
 *        支持 force 参数的删除 API（见 @/api 中 deleteAssistant / deleteModel ...）
 * @param {string} cfg.title 确认框标题，如「删除助手」
 * @param {string} cfg.targetLabel 被删对象显示名（助手名/模型名…），拼进标题
 * @param {function} [cfg.onSuccess] 真删成功后的回调（通常刷新列表）
 * @returns {Promise<boolean>} 是否真正删除了（用户取消 / 失败均返回 false）
 */
export async function confirmDeleteWithImpact({ id, removeFn, title, targetLabel, onSuccess }) {
  // 1. 预检：拿到影响清单（后端只读，不删）。
  //    removeFn 在 HTTP 非 2xx / 断网 / GraphQL 协议错时会 throw（业务码走 code 判定），
  //    不接住会一路冒泡成未处理拒绝——按钮看着完全失灵且无任何提示
  let pre
  try {
    pre = await removeFn(id, false)
  } catch (e) {
    ElMessage.error('预检请求失败: ' + (e.message || '网络错误'))
    return false
  }
  if (pre.code !== 0) {
    ElMessage.error(pre.message || '预检失败')
    return false
  }
  const summary = (pre.data && pre.data.summary) || '确定删除？此操作不可恢复。'

  // 2. 带影响清单的确认框（summary 已是后端拼好的人类可读文本）
  try {
    await ElMessageBox.confirm(summary, `${title}「${targetLabel}」`, {
      type: 'warning',
      confirmButtonText: '删除',
      cancelButtonText: '取消',
      confirmButtonClass: 'el-button--danger',
    })
  } catch {
    return false // 用户取消
  }

  // 3. 真删（force=true）
  let res
  try {
    res = await removeFn(id, true)
  } catch (e) {
    ElMessage.error('删除请求失败: ' + (e.message || '网络错误'))
    return false
  }
  if (res.code !== 0) {
    ElMessage.error(res.message || '删除失败')
    return false
  }
  ElMessage.success('已删除')
  if (typeof onSuccess === 'function') {
    try {
      await onSuccess()
    } catch (e) {
      // 刷新失败不阻塞主流程，仅记录
      console.warn('[confirmDeleteWithImpact] onSuccess 回调失败', e)
    }
  }
  return true
}
