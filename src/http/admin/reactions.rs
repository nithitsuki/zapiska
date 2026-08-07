//! Reaction moderation endpoints: list reactions by status, moderate single
//! or in batch, with a status-change webhook to the moderation engine.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::error::AppError;
use crate::http::webhook;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ReactionsQuery {
    /// Filter by status: pending, approved, spam, deleted, or all.
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub before: Option<i64>,
}

/// GET /api/admin/reactions — list reactions with comment context.
pub async fn list_reactions(
    State(state): State<AppState>,
    Query(query): Query<ReactionsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let reactions = state
        .repo
        .list_reactions(query.status.as_deref(), limit, query.before)
        .await?;
    Ok(Json(serde_json::json!({ "reactions": reactions })))
}

#[derive(Deserialize)]
pub struct ModerateReactionRequest {
    pub id: i64,
    pub action: String,
}

#[derive(Deserialize)]
pub struct BatchReactionRequest {
    pub actions: Vec<ModerateReactionRequest>,
}

/// POST /api/admin/reactions/moderate — approve/spam/delete a single reaction.
pub async fn moderate_reaction(
    State(state): State<AppState>,
    Json(body): Json<ModerateReactionRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !is_valid_reaction_action(&body.action) {
        return Err(AppError::BadRequest(format!(
            "invalid action '{}', must be one of: approved, spam, deleted, pending",
            body.action
        )));
    }
    let reaction = state
        .repo
        .get_reaction(body.id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("reaction {} not found", body.id)))?;

    state
        .repo
        .update_reaction_status(body.id, &body.action)
        .await?;
    fire_reaction_status_webhook(&state, &reaction, &body.action);
    Ok(Json(serde_json::json!({
        "id": body.id,
        "status": body.action,
    })))
}

/// POST /api/admin/reactions/moderate/batch — moderate many reactions at once.
pub async fn moderate_reactions_batch(
    State(state): State<AppState>,
    Json(body): Json<BatchReactionRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut results = Vec::with_capacity(body.actions.len());
    for action in body.actions {
        let result = match moderate_reaction_single(&state, action.id, &action.action).await {
            Ok(status) => serde_json::json!({ "id": action.id, "status": status, "error": null }),
            Err(e) => {
                serde_json::json!({ "id": action.id, "status": null, "error": e.to_string() })
            }
        };
        results.push(result);
    }
    Ok(Json(serde_json::json!({ "results": results })))
}

async fn moderate_reaction_single(
    state: &AppState,
    id: i64,
    action: &str,
) -> Result<String, AppError> {
    if !is_valid_reaction_action(action) {
        return Err(AppError::BadRequest(format!(
            "invalid action '{action}', must be one of: approved, spam, deleted, pending"
        )));
    }
    let reaction = state
        .repo
        .get_reaction(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("reaction {id} not found")))?;
    state.repo.update_reaction_status(id, action).await?;
    fire_reaction_status_webhook(state, &reaction, action);
    Ok(action.to_string())
}

fn is_valid_reaction_action(action: &str) -> bool {
    matches!(action, "approved" | "spam" | "deleted" | "pending")
}

/// Notify the moderation engine that a reaction's status changed.
fn fire_reaction_status_webhook(
    state: &AppState,
    reaction: &crate::db::repo::CommentReaction,
    new_status: &str,
) {
    if let Some(ref url) = state.config.moderation_webhook_url {
        webhook::fire(
            &state.http_client,
            url,
            serde_json::json!({
                "event": "reaction.status_changed",
                "id": reaction.id,
                "comment_id": reaction.comment_id,
                "reaction": reaction.reaction,
                "old_status": reaction.status,
                "new_status": new_status,
                "changed_by": "admin",
            }),
            5,
        );
    }
}
