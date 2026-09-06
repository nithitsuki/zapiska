//! Reaction moderation endpoints: list reactions by status, moderate single
//! or in batch, with a status-change webhook to the moderation engine.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::error::AppError;
use crate::moderation::{Actor, InvalidStatus, Moderation, ModerationSink, Status, WebhookSink};
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
    /// The emoji the moderator reviewed. Optional and backward-compatible:
    /// absent means "approve whatever is currently stored" (fresh-read);
    /// present-but-stale rejects with 400 so the moderator re-reads.
    /// Consulted only for pending→approved; overrides from spam/deleted
    /// ignore it. Shared by the batch items, so each item carries its own.
    pub expected_emoji: Option<String>,
}

#[derive(Deserialize)]
pub struct BatchReactionRequest {
    pub actions: Vec<ModerateReactionRequest>,
}

fn status_sink(state: &AppState) -> Option<WebhookSink> {
    state.config.moderation_webhook_url.as_ref().map(|url| {
        WebhookSink::status_sink_signed(
            &state.http_client,
            url,
            state.config.webhook_signing_secret.clone(),
        )
    })
}

fn parse_action(action: &str) -> Result<Status, AppError> {
    action
        .parse()
        .map_err(|e: InvalidStatus| AppError::BadRequest(e.to_string()))
}

/// POST /api/admin/reactions/moderate — approve/spam/delete a single reaction.
pub async fn moderate_reaction(
    State(state): State<AppState>,
    Json(body): Json<ModerateReactionRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let to = parse_action(&body.action)?;
    let sink = status_sink(&state);
    let sink_ref = sink.as_ref().map(|s| s as &dyn ModerationSink);
    // The pending→approved path goes through T15's CAS inside the machine:
    // `expected_emoji` carries the moderator's seen value (absent =
    // fresh-read); other paths use plain status writes. `None` = approve the
    // currently stored emoji.
    let outcome = Moderation::transition_reaction(
        &state.repo,
        sink_ref,
        body.id,
        to,
        Actor::Admin,
        body.expected_emoji.as_deref(),
    )
    .await?;
    Ok(Json(serde_json::json!({
        "id": body.id,
        "status": outcome.new.as_str(),
    })))
}

/// POST /api/admin/reactions/moderate/batch — moderate many reactions at once.
pub async fn moderate_reactions_batch(
    State(state): State<AppState>,
    Json(body): Json<BatchReactionRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sink = status_sink(&state);
    let sink_ref = sink.as_ref().map(|s| s as &dyn ModerationSink);
    let mut results = Vec::with_capacity(body.actions.len());
    for action in body.actions {
        let result = match moderate_reaction_single(
            &state,
            sink_ref,
            action.id,
            &action.action,
            action.expected_emoji.as_deref(),
        )
        .await
        {
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
    sink: Option<&dyn ModerationSink>,
    id: i64,
    action: &str,
    expected_emoji: Option<&str>,
) -> Result<String, AppError> {
    let to = parse_action(action)?;
    let outcome =
        Moderation::transition_reaction(&state.repo, sink, id, to, Actor::Admin, expected_emoji)
            .await?;
    Ok(outcome.new.to_string())
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

    async fn seed_reaction(state: &crate::state::AppState) -> i64 {
        let cid = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/t16-rreact".to_string(),
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
            .unwrap();
        state.repo.update_status(cid, "approved").await.unwrap();
        let (rid, _) = state
            .repo
            .upsert_reaction(cid, "👍", "h:t16r")
            .await
            .unwrap();
        rid
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
    async fn reaction_single_and_batch_each_fire_exactly_once() {
        let (state, _dir, server) = webhook_state().await;
        let r1 = seed_reaction(&state).await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate",
                &format!(r#"{{"id":{r1},"action":"approved"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.repo.get_reaction(r1).await.unwrap().unwrap().status,
            "approved"
        );
        let reqs = wait_for_requests(&server, 1).await;
        assert_eq!(reqs.len(), 1, "single reaction emits exactly once");
        let p: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(p["event"], "reaction.status_changed");
        assert_eq!(p["old_status"], "pending");
        assert_eq!(p["new_status"], "approved");
        assert_eq!(p["changed_by"], "admin");

        // Batch: one more approve + one spam on two fresh rows.
        let r2 = seed_reaction(&state).await;
        let r3 = seed_reaction(&state).await;
        let before = server.received_requests().await.unwrap_or_default().len();
        let app = build_app(state.clone());
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate/batch",
                &format!(
                    r#"{{"actions":[{{"id":{r2},"action":"approved"}},{{"id":{r3},"action":"spam"}}]}}"#
                ),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let reqs = wait_for_requests(&server, before + 2).await;
        assert_eq!(reqs.len(), before + 2, "one event per batch item");
    }

    // M2: the moderator's seen emoji guards the approve. Present-but-stale
    // `expected_emoji` rejects (re-read); correct or absent approves.
    #[tokio::test]
    async fn reaction_approve_with_stale_expected_emoji_rejected() {
        let (state, _dir, _server) = webhook_state().await;
        let cid = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/t16-stale".to_string(),
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
            .unwrap();
        state.repo.update_status(cid, "approved").await.unwrap();
        let (rid, _) = state
            .repo
            .upsert_reaction(cid, "👍", "h:t16stale")
            .await
            .unwrap();
        // Owner swaps the emoji after the moderator's read.
        let (_, changed) = state
            .repo
            .upsert_reaction(cid, "❤️", "h:t16stale")
            .await
            .unwrap();
        assert!(changed);
        let app = build_app(state.clone());
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate",
                &format!(r#"{{"id":{rid},"action":"approved","expected_emoji":"👍"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "stale expected_emoji must be rejected");
        let row = state.repo.get_reaction(rid).await.unwrap().unwrap();
        assert_eq!(row.reaction, "❤️");
        assert_eq!(row.status, "pending", "unmoderated emoji stays pending");
    }

    #[tokio::test]
    async fn reaction_approve_with_correct_expected_emoji_succeeds() {
        let (state, _dir, _server) = webhook_state().await;
        let rid = seed_reaction(&state).await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate",
                &format!(r#"{{"id":{rid},"action":"approved","expected_emoji":"👍"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.repo.get_reaction(rid).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn reaction_batch_threads_expected_emoji_per_item() {
        let (state, _dir, _server) = webhook_state().await;
        // r1: swapped after the moderator's read (stale); r2: untouched.
        let cid = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/t16-batchexp".to_string(),
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
            .unwrap();
        state.repo.update_status(cid, "approved").await.unwrap();
        let (r1, _) = state
            .repo
            .upsert_reaction(cid, "👍", "h:t16be1")
            .await
            .unwrap();
        let (r2, _) = state
            .repo
            .upsert_reaction(cid, "👍", "h:t16be2")
            .await
            .unwrap();
        state
            .repo
            .upsert_reaction(cid, "❤️", "h:t16be1")
            .await
            .unwrap();
        let app = build_app(state.clone());
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate/batch",
                &format!(
                    r#"{{"actions":[{{"id":{r1},"action":"approved","expected_emoji":"👍"}},{{"id":{r2},"action":"approved","expected_emoji":"👍"}}]}}"#
                ),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let results = body["results"].as_array().unwrap();
        assert!(
            results[0]["error"].is_string(),
            "stale item errors: {results:?}"
        );
        assert!(
            results[1]["error"].is_null(),
            "fresh item succeeds: {results:?}"
        );
        assert_eq!(
            state.repo.get_reaction(r1).await.unwrap().unwrap().status,
            "pending"
        );
        assert_eq!(
            state.repo.get_reaction(r2).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn reaction_invalid_action_rejected() {
        let (state, _dir, server) = webhook_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate",
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
            "rejected reaction action must not emit"
        );
    }
}
