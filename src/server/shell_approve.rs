use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::json;

use super::AppState;
use super::{auth, response, session, shell_approval};
use crate::domain::audit;

#[derive(serde::Deserialize)]
pub(super) struct ShellApproveRequest {
    approval_id: String,
    decision: String,
}

pub(super) async fn handle_shell_approve(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    Json(req): Json<ShellApproveRequest>,
) -> impl IntoResponse {
    let decision = match req.decision.to_lowercase().as_str() {
        "approved" | "approve" | "yes" | "true" => shell_approval::ApprovalDecision::Approved,
        _ => shell_approval::ApprovalDecision::Rejected,
    };
    let caller_user_id = opt_user
        .as_ref()
        .map(|u| u.user_id.clone())
        .unwrap_or_default();
    let is_admin = opt_user.as_ref().is_some_and(|u| u.is_admin);
    // 原子 remove：单次锁内消耗条目并取回 session_id + sender，
    // 消除 `session_of` + `resolve` 两步锁之间的 TOCTOU 竞态。
    let resolved = match state
        .shell_approval_registry
        .resolve_with_session(&req.approval_id)
        .await
    {
        Some((session_id, tx)) => {
            if state.auth.is_some() {
                if let Err(e) = session::check_session_access(
                    &state,
                    &caller_user_id,
                    is_admin,
                    &session_id,
                )
                .await
                {
                    // 条目已消耗；安全方向：拒绝而非放行
                    let _ = tx.send(shell_approval::ApprovalDecision::Rejected);
                    return Json(e);
                }
            }
            let _ = tx.send(decision);
            true
        }
        None => false,
    };
    audit::spawn_record(
        state.audit_store.as_ref(),
        audit::AuditEntry {
            user_id: opt_user
                .as_ref()
                .map(|u| u.user_id.clone())
                .unwrap_or_default(),
            actor: opt_user
                .as_ref()
                .map(|u| u.name.clone())
                .unwrap_or_default(),
            source: "web".to_string(),
            operation: "shell_approve".to_string(),
            target_id: req.approval_id.clone(),
            success: resolved,
            detail: json!({ "decision": req.decision }).to_string(),
            ip: crate::server::audit::client_ip(&headers),
        },
    );
    match resolved {
        true => Json(response::ok(json!({ "resolved": true }))),
        false => Json(response::err(
            response::code::NOT_FOUND,
            "审批请求不存在或已过期",
        )),
    }
}
