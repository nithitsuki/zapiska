//! Moderation endpoints: single + batch status changes, with a status-change
//! webhook notification to the configured moderation service.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::state::AppState;

// ── POST /api/admin/moderate/batch ──────────────────────────

#[derive(Deserialize)]
pub struct BatchModerateRequest {
    pub actions: Vec<ModerateAction>,
}

#[derive(Deserialize)]
pub struct ModerateAction {
    pub id: i64,
    pub action: String,
}

#[derive(Serialize)]
pub struct BatchModerateResponse {
    pub results: Vec<ModerateResult>,
}

#[derive(Serialize)]
pub struct ModerateResult {
    pub id: i64,
    pub status: String,
    pub error: Option<String>,
}

pub async fn moderate_batch(
    State(state): State<AppState>,
    Json(body): Json<BatchModerateRequest>,
) -> Result<Json<BatchModerateResponse>, AppError> {
    let mut results = Vec::with_capacity(body.actions.len());

    for action in body.actions {
        let result = match moderate_single(&state, action.id, &action.action).await {
            Ok(status) => ModerateResult {
                id: action.id,
                status,
                error: None,
            },
            Err(e) => ModerateResult {
                id: action.id,
                status: String::new(),
                error: Some(e.to_string()),
            },
        };
        results.push(result);
    }

    Ok(Json(BatchModerateResponse { results }))
}

/// Moderate a single comment. Returns the new status on success.
async fn moderate_single(state: &AppState, id: i64, action: &str) -> Result<String, AppError> {
    if !is_valid_action(action) {
        return Err(AppError::BadRequest(format!(
            "invalid action '{action}', must be one of: approved, spam, deleted, pending"
        )));
    }

    let _comment = state
        .repo
        .get_comment(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("comment {id} not found")))?;

    state.repo.update_status(id, action).await?;
    Ok(action.to_string())
}

// ── POST /api/admin/moderate ────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ModerateRequest {
    pub id: i64,
    #[schema(example = "approved")]
    pub action: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ModerateResponse {
    pub id: i64,
    #[schema(example = "approved")]
    pub status: String,
}

/// The four moderation statuses a comment can transition to.
fn is_valid_action(action: &str) -> bool {
    matches!(action, "approved" | "spam" | "deleted" | "pending")
}

/// Fire a webhook notification when a comment's status changes.
/// Used by the moderation system to keep its cache in sync.
fn fire_status_webhook(
    state: &AppState,
    id: i64,
    old_status: &str,
    new_status: &str,
    changed_by: &str,
) {
    if let Some(ref url) = state.config.moderation_webhook_url {
        let client = state.http_client.clone();
        let url = url.clone();
        let old = old_status.to_string();
        let new = new_status.to_string();
        let by = changed_by.to_string();
        tokio::spawn(async move {
            let payload = serde_json::json!({
                "event": "comment.status_changed",
                "id": id,
                "old_status": old,
                "new_status": new,
                "changed_by": by,
            });
            let resp = client
                .post(&url)
                .json(&payload)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    tracing::debug!(id, new_status = %new, webhook_status = %r.status(), "status change webhook sent")
                }
                Err(e) => tracing::warn!(id, err = %e, "status change webhook failed"),
            }
        });
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/moderate",
    request_body = ModerateRequest,
    responses(
        (status = 200, description = "Comment moderated", body = ModerateResponse),
        (status = 400, description = "Invalid action"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Comment not found"),
    ),
    tag = "admin",
)]
pub async fn moderate(
    State(state): State<AppState>,
    Json(body): Json<ModerateRequest>,
) -> Result<Json<ModerateResponse>, AppError> {
    // Validate action first (before any DB calls) for consistent error messages.
    if !is_valid_action(&body.action) {
        return Err(AppError::BadRequest(format!(
            "invalid action '{}', must be one of: approved, spam, deleted, pending",
            body.action
        )));
    }
    // Fetch the current status for the webhook notification.
    let old_status = state
        .repo
        .get_comment(body.id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("comment {} not found", body.id)))?
        .status;

    state.repo.update_status(body.id, &body.action).await?;
    fire_status_webhook(&state, body.id, &old_status, &body.action, "admin");
    Ok(Json(ModerateResponse {
        id: body.id,
        status: body.action.clone(),
    }))
}
