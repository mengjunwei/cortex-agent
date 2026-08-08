//! 截图生命周期管理 —— 会话删除同步清理(对象存储版)
//!
//! 截图按会话隔离存储于对象存储:`screenshots/{session_id}/{filename}`,image_url 为
//! `/api/screenshots/{session_id}/{filename}`(serve 时校验会话归属,后端从对象存储代理读)。
//! 本模块只负责会话删除时同步删除该会话的所有截图对象。
//!
//! 孤儿回收交给对象存储(RustFS)侧的生命周期规则(N 天后自动过期),不再在应用内
//! 维护后台扫描任务——对象存储原生支持过期,本地文件系统才需要那个任务。

use std::sync::Arc;

use adk_rust::session::SessionService;

use crate::infra::object_store::ObjectStore;

/// 校验文件名/会话 ID 是否安全(防止路径穿越:拒空、拒 / \ ..)
fn is_safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 256
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
}

/// 删除指定会话关联的所有截图对象(`screenshots/{session_id}/` 前缀)。
///
/// **安全**:`session_id` 来自外部输入(GraphQL `deleteSession`),必须先经
/// [`is_safe_filename`] 净化(防异常前缀),并校验 `user_id` 拥有该会话(adk get 按
/// user_id 过滤,防跨用户删别人截图)。归属/净化任一不通过则整体跳过。
pub async fn delete_session_screenshots(
    session_service: &Arc<dyn SessionService>,
    user_id: &str,
    session_id: &str,
    object_store: &ObjectStore,
) {
    // 防穿越:session_id 必须安全
    if !is_safe_filename(session_id) {
        tracing::warn!(
            "[screenshot-cleanup] 拒绝不安全的 session_id（防路径穿越）: {}",
            session_id
        );
        return;
    }

    // 归属校验:调用方 user_id 必须拥有该 session(adk get 按 user_id 过滤,不归属返回 Err),
    // 否则整体跳过(防跨用户删别人截图)。num_recent_events=1 轻量探测。
    let belongs = session_service
        .get(adk_rust::session::GetRequest {
            app_name: "cortex-agent".to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            num_recent_events: Some(1),
            after: None,
        })
        .await
        .is_ok();
    if !belongs {
        tracing::warn!(
            "[screenshot-cleanup] session {} 不属于 user {} 或不存在，跳过截图清理",
            session_id,
            user_id
        );
        return;
    }

    // 删该会话所有截图对象(按前缀)
    let prefix = format!("screenshots/{session_id}/");
    match object_store.delete_prefix(&prefix).await {
        Ok(_) => tracing::info!(
            "[screenshot-cleanup] 已删除会话 {} 截图对象(prefix={})",
            session_id,
            prefix
        ),
        Err(e) => tracing::warn!(
            "[screenshot-cleanup] 删除会话 {} 截图对象失败(可忽略): {}",
            session_id,
            e
        ),
    }
}
