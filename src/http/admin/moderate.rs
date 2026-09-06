//! Moderation endpoints: single + batch status changes, with a status-change
//! webhook notification to the configured moderation service.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::moderation::{Actor, Moderation, ModerationSink, Status, WebhookSink};
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

/// The `*.status_changed` sink for this request, if a webhook is configured.
/// One sink per request (not per item): every batch item emits through it.
fn status_sink(state: &AppState) -> Option<WebhookSink> {
    state.config.moderation_webhook_url.as_ref().map(|url| {
        WebhookSink::status_sink_signed(
            &state.http_client,
            url,
            state.config.webhook_signing_secret.clone(),
        )
    })
}

pub async fn moderate_batch(
    State(state): State<AppState>,
    Json(body): Json<BatchModerateRequest>,
) -> Result<Json<BatchModerateResponse>, AppError> {
    let sink = status_sink(&state);
    let sink_ref = sink.as_ref().map(|s| s as &dyn ModerationSink);
    let mut results = Vec::with_capacity(body.actions.len());

    for action in body.actions {
        let result = match moderate_single(&state, sink_ref, action.id, &action.action).await {
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
/// Every item goes through the status machine, so the documented batch
/// polling path emits exactly one `comment.status_changed` event per change,
/// like the single route always did.
async fn moderate_single(
    state: &AppState,
    sink: Option<&dyn ModerationSink>,
    id: i64,
    action: &str,
) -> Result<String, AppError> {
    let to: Status = action
        .parse()
        .map_err(|e: crate::moderation::InvalidStatus| AppError::BadRequest(e.to_string()))?;
    let outcome = Moderation::transition_comment(&state.repo, sink, id, to, Actor::Admin).await?;
    Ok(outcome.new.to_string())
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
    let to: Status = body
        .action
        .parse()
        .map_err(|e: crate::moderation::InvalidStatus| AppError::BadRequest(e.to_string()))?;
    let sink = status_sink(&state);
    let sink_ref = sink.as_ref().map(|s| s as &dyn ModerationSink);
    let outcome =
        Moderation::transition_comment(&state.repo, sink_ref, body.id, to, Actor::Admin).await?;
    Ok(Json(ModerateResponse {
        id: body.id,
        status: outcome.new.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use crate::http::build_app;
    use crate::http::test_support::helpers;

    async fn wait_for_requests(server: &wiremock::MockServer, n: usize) -> Vec<wiremock::Request> {
        for _ in 0..100 {
            let reqs = server.received_requests().await.unwrap_or_default();
            if reqs.len() >= n {
                return reqs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        server.received_requests().await.unwrap_or_default()
    }

    async fn seed_pending(state: &crate::state::AppState, target: &str) -> i64 {
        state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: target.to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "T16".to_string(),
                author_url: None,
                author_avatar: None,
                content: "hi".to_string(),
                parent_id: None,
                depth: 0,
                honeypot: false,
                delete_token: None,
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap()
    }

    async fn webhook_state() -> (
        crate::state::AppState,
        tempfile::TempDir,
        wiremock::MockServer,
    ) {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (mut state, dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        (state, dir, server)
    }

    #[tokio::test]
    async fn batch_moderate_fires_status_changed_per_item() {
        // Headline: the documented engine polling path emits one event per
        // change (previously fired nothing).
        let (state, _dir, server) = webhook_state().await;
        let a = seed_pending(&state, "/t16-batch-a").await;
        let b = seed_pending(&state, "/t16-batch-b").await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/moderate/batch",
                &format!(r#"{{"actions":[{{"id":{a},"action":"approved"}},{{"id":{b},"action":"spam"}}]}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.repo.get_comment(a).await.unwrap().unwrap().status,
            "approved"
        );
        assert_eq!(
            state.repo.get_comment(b).await.unwrap().unwrap().status,
            "spam"
        );
        let reqs = wait_for_requests(&server, 2).await;
        assert_eq!(reqs.len(), 2, "one event per batch item");
        let mut events: Vec<serde_json::Value> = reqs
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        events.sort_by_key(|p| p["id"].as_i64().unwrap());
        assert_eq!(events[0]["event"], "comment.status_changed");
        assert_eq!(events[0]["id"], a);
        assert_eq!(events[0]["old_status"], "pending");
        assert_eq!(events[0]["new_status"], "approved");
        assert_eq!(events[0]["changed_by"], "admin");
        assert_eq!(events[1]["id"], b);
        assert_eq!(events[1]["new_status"], "spam");
    }

    #[tokio::test]
    async fn single_moderate_fires_exactly_once() {
        let (state, _dir, server) = webhook_state().await;
        let id = seed_pending(&state, "/t16-single").await;
        let app = build_app(state);
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/moderate",
                &format!(r#"{{"id":{id},"action":"approved"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let reqs = wait_for_requests(&server, 1).await;
        assert_eq!(reqs.len(), 1, "single moderate emits exactly one event");
        let p: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(p["event"], "comment.status_changed");
        assert_eq!(p["old_status"], "pending");
        assert_eq!(p["new_status"], "approved");
    }

    #[tokio::test]
    async fn same_status_moderate_emits_nothing() {
        let (state, _dir, server) = webhook_state().await;
        let id = seed_pending(&state, "/t16-noop").await;
        // Direct repo write bypasses the machine (no event by construction).
        state.repo.update_status(id, "approved").await.unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/moderate",
                &format!(r#"{{"id":{id},"action":"approved"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "same-status re-approve must not emit"
        );
    }

    #[tokio::test]
    async fn invalid_action_rejected_without_event() {
        let (state, _dir, server) = webhook_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/moderate",
                r#"{"id":1,"action":"publish"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "rejected action must not emit"
        );
    }

    #[tokio::test]
    async fn batch_revive_of_self_deleted_clears_token_and_fires() {
        let (state, _dir, server) = webhook_state().await;
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/t16-revive".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "T16".to_string(),
                author_url: None,
                author_avatar: None,
                content: "hi".to_string(),
                parent_id: None,
                depth: 0,
                honeypot: false,
                delete_token: Some("tok-revive".to_string()),
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap();
        assert!(state.repo.delete_by_token(id, "tok-revive").await.unwrap());
        let app = build_app(state.clone());
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/moderate/batch",
                &format!(r#"{{"actions":[{{"id":{id},"action":"approved"}}]}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let row = state.repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(row.status, "approved");
        assert!(
            row.delete_token.is_none(),
            "B10: token cleared on re-approve"
        );
        let reqs = wait_for_requests(&server, 1).await;
        assert_eq!(reqs.len(), 1);
        let p: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(p["old_status"], "deleted");
        assert_eq!(p["new_status"], "approved");
    }
}
