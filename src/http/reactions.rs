//! Public reaction endpoints.
//!
//! - `POST /api/comment/{id}/reaction` — add or change a reaction
//!   (`{ "reaction": "👍" }`). Reactions are moderation-pending like
//!   comments: the row starts `pending` and only `approved` reactions count
//!   in read responses, so the moderation engine can take the right action.
//! - `DELETE /api/comment/{id}/reaction` — remove one's own reaction.
//!
//! Identity: in `REACTIONS_ALLOWED=admin` mode (default) only requests with
//! the admin token are accepted. In `anyone` mode (highly discouraged) any
//! visitor may react; identity is a SHA-256 hash of their IP (never stored
//! raw), giving one reaction per person per comment.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use serde::Deserialize;

use crate::config::ReactionsMode;
use crate::error::AppError;
use crate::http::admin::request_has_admin_token;
use crate::http::peer::ClientIdentity;
use crate::moderation::ModerationSink as _;
use crate::state::AppState;

/// Identifier used for admin reactions (in either mode).
const ADMIN_IDENTIFIER: &str = "admin";

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReactionBody {
    #[schema(example = "👍")]
    pub reaction: String,
}

#[utoipa::path(
    post,
    path = "/api/comment/{id}/reaction",
    responses(
        (status = 201, description = "Reaction stored (pending moderation)"),
        (status = 200, description = "Reaction unchanged (already active)"),
        (status = 400, description = "Invalid reaction or comment not approved"),
        (status = 401, description = "Admin token required (REACTIONS_ALLOWED=admin)"),
        (status = 404, description = "Comment not found"),
    ),
    tag = "comments",
)]
pub async fn add_reaction(
    State(state): State<AppState>,
    peer: ClientIdentity,
    Path(comment_id): Path<i64>,
    headers: HeaderMap,
    Json(body): Json<ReactionBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let is_admin = request_has_admin_token(&state, &headers);

    if matches!(state.config.reactions_allowed, ReactionsMode::Admin) && !is_admin {
        return Err(AppError::Unauthorized);
    }
    if !state.config.reactions_set.contains(&body.reaction) {
        return Err(AppError::BadRequest(format!(
            "invalid reaction '{}', allowed reactions: {}",
            body.reaction,
            state.config.reactions_set.join(", ")
        )));
    }

    // Only approved comments can be reacted to (mirrors reply rules).
    let comment = state
        .repo
        .get_comment(comment_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("comment {comment_id} not found")))?;
    if comment.status != "approved" {
        return Err(AppError::BadRequest(format!(
            "comment {comment_id} is not approved (status: {})",
            comment.status
        )));
    }

    let identifier = if is_admin {
        ADMIN_IDENTIFIER.to_string()
    } else {
        crate::ip_hash::hash_ip(&peer.ip(), state.config.ip_hash_secret.as_deref())
    };

    let (reaction_id, changed) = state
        .repo
        .upsert_reaction(comment_id, &body.reaction, &identifier)
        .await?;

    // Notify the moderation engine through the ONE shared T16 adapter
    // (T19 dedup point with native comments): sync awaits the decision,
    // async emits. Signed when a secret is configured (S3).
    let mut final_status = "pending".to_string();
    if let Some(ref webhook_url) = state.config.moderation_webhook_url {
        let payload = crate::moderation::reaction_created_payload(
            reaction_id,
            comment_id,
            &body.reaction,
            &comment.target_path,
            is_admin,
        );
        let sink = crate::moderation::WebhookSink::created_sink_signed(
            &state.http_client,
            webhook_url,
            state.config.webhook_signing_secret.clone(),
        );
        if let Some(decision) = sink
            .deliver(&payload, state.config.moderation_webhook_mode.is_sync())
            .await
        {
            let _ = state
                .repo
                .update_reaction_status(reaction_id, decision.as_str())
                .await;
            final_status = decision.to_string();
        }
    }

    let code = if changed {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        code,
        Json(serde_json::json!({
            "id": reaction_id,
            "reaction": body.reaction,
            "status": final_status,
            "changed": changed,
        })),
    ))
}

/// DELETE /api/comment/{id}/reaction — remove one's own active reaction.
pub async fn remove_reaction(
    State(state): State<AppState>,
    peer: ClientIdentity,
    Path(comment_id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    let is_admin = request_has_admin_token(&state, &headers);

    if matches!(state.config.reactions_allowed, ReactionsMode::Admin) && !is_admin {
        return Err(AppError::Unauthorized);
    }

    let identifier = if is_admin {
        ADMIN_IDENTIFIER.to_string()
    } else {
        crate::ip_hash::hash_ip(&peer.ip(), state.config.ip_hash_secret.as_deref())
    };

    let removed = state.repo.delete_reaction(comment_id, &identifier).await?;
    Ok(Json(serde_json::json!({ "success": removed })))
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, header};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    use crate::http::build_app;
    use crate::http::test_support::helpers;
    use crate::state::AppState;

    fn reaction_request(
        method: axum::http::Method,
        uri: &str,
        body: Option<&str>,
        admin: bool,
        ip: [u8; 4],
    ) -> axum::http::Request<axum::body::Body> {
        let mut req = helpers::request(method, uri);
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if admin {
            req.headers_mut().insert(
                header::AUTHORIZATION,
                HeaderValue::from_static("Bearer test"),
            );
        }
        if let Some(b) = body {
            *req.body_mut() = axum::body::Body::from(b.to_owned());
            req.headers_mut()
                .insert(header::CONTENT_LENGTH, b.len().into());
        }
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(SocketAddr::from((ip, 54321))));
        req
    }

    /// Seed an approved comment; returns its id.
    async fn seed_approved(state: &AppState) -> i64 {
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/react".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Alice".to_string(),
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
        state.repo.update_status(id, "approved").await.unwrap();
        id
    }

    #[tokio::test]
    async fn admin_mode_requires_token() {
        let (state, _dir) = helpers::test_state();
        let id = seed_approved(&state).await;
        let app = build_app(state);
        let resp = app
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                false,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "admin mode: no token -> 401");
    }

    #[tokio::test]
    async fn admin_reaction_flow_to_read_api() {
        let (state, _dir) = helpers::test_state();
        let id = seed_approved(&state).await;
        let app = build_app(state.clone());

        // Admin reacts → 201 pending.
        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                true,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["status"], "pending");
        let reaction_id = body["id"].as_i64().unwrap();

        // Same reaction again → 200, unchanged.
        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                true,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["changed"], false);

        // Pending reaction is NOT visible in the read API yet.
        let resp = app
            .clone()
            .oneshot(helpers::request(
                axum::http::Method::GET,
                "/api/comments?path=/react",
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["comments"][0]["reactions"], serde_json::json!({}));

        // Approve via the admin moderation endpoint → now visible.
        let resp = app
            .clone()
            .oneshot(crate::http::test_support::helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate",
                &format!(r#"{{"id":{reaction_id},"action":"approved"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = app
            .oneshot(helpers::request(
                axum::http::Method::GET,
                "/api/comments?path=/react",
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["comments"][0]["reactions"],
            serde_json::json!({"👍": 1}),
            "approved reaction counted in read API"
        );
    }

    #[tokio::test]
    async fn invalid_reaction_rejected() {
        let (state, _dir) = helpers::test_state();
        let id = seed_approved(&state).await;
        let app = build_app(state);
        for payload in [
            r#"{"reaction":"😈"}"#,
            r#"{"reaction":"<script>alert(1)</script>"}"#,
            r#"{"reaction":""}"#,
        ] {
            let resp = app
                .clone()
                .oneshot(reaction_request(
                    axum::http::Method::POST,
                    &format!("/api/comment/{id}/reaction"),
                    Some(payload),
                    true,
                    [127, 0, 0, 1],
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "payload {payload} must be rejected");
        }
    }

    #[tokio::test]
    async fn reaction_on_missing_or_unapproved_comment_rejected() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state.clone());
        // Nonexistent comment → 404.
        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::POST,
                "/api/comment/999/reaction",
                Some(r#"{"reaction":"👍"}"#),
                true,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // Pending comment → 400.
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/react".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Pend".to_string(),
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
        let resp = app
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                true,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "unapproved comment cannot be reacted to"
        );
    }

    #[tokio::test]
    async fn anyone_mode_uses_ip_identity_and_uniqueness() {
        let (mut state, _dir) = helpers::test_state();
        state.config.reactions_allowed = crate::config::ReactionsMode::Anyone;
        let id = seed_approved(&state).await;
        let app = build_app(state.clone());

        // No token required in anyone mode.
        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                false,
                [10, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);

        // Same IP re-reacting is a no-op (unique per IP).
        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                false,
                [10, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &axum::body::to_bytes(resp.into_body(), 1024).await.unwrap()
            )
            .unwrap()["changed"],
            false
        );

        // A different IP counts separately.
        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                false,
                [10, 0, 0, 2],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);

        // DELETE removes only that IP's reaction.
        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::DELETE,
                &format!("/api/comment/{id}/reaction"),
                None,
                false,
                [10, 0, 0, 1],
            ))
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["success"], true);

        let pending = state.repo.list_reactions(None, 10, None).await.unwrap();
        assert_eq!(pending.len(), 2, "one deleted, one still pending");
    }

    #[tokio::test]
    async fn sync_webhook_can_auto_approve_reaction() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"action": "approved"})),
            )
            .mount(&server)
            .await;

        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        state.config.moderation_webhook_mode = crate::config::WebhookMode::Sync;
        let id = seed_approved(&state).await;
        let app = build_app(state.clone());

        let resp = app
            .clone()
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                true,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["status"], "approved", "sync webhook decision applied");

        // And the approved reaction is immediately visible.
        let resp = app
            .clone()
            .oneshot(helpers::request(
                axum::http::Method::GET,
                "/api/comments?path=/react",
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["comments"][0]["reactions"],
            serde_json::json!({"👍": 1})
        );
    }

    #[tokio::test]
    async fn sync_webhook_invalid_action_keeps_reaction_pending() {
        // M7: an invalid sync decision is ignored at the HTTP layer (not just
        // in the unit-tested parser) — the row stays pending, no event.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"action": "publish"})),
            )
            .mount(&server)
            .await;

        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        state.config.moderation_webhook_mode = crate::config::WebhookMode::Sync;
        let id = seed_approved(&state).await;
        let app = build_app(state.clone());

        let resp = app
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                true,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["status"], "pending", "invalid action ignored");
        assert_eq!(
            state
                .repo
                .list_reactions(Some("pending"), 10, None)
                .await
                .unwrap()
                .len(),
            1,
            "row stays pending"
        );
    }

    #[tokio::test]
    async fn admin_reactions_list_and_batch() {
        let (state, _dir) = helpers::test_state();
        let id = seed_approved(&state).await;
        let app = build_app(state.clone());

        // Two reactions from different identifiers (direct repo upserts).
        let (r1, _) = state.repo.upsert_reaction(id, "👍", "h:one").await.unwrap();
        let (r2, _) = state.repo.upsert_reaction(id, "❤️", "h:two").await.unwrap();

        // List pending.
        let resp = app
            .clone()
            .oneshot(crate::http::test_support::helpers::json_request(
                axum::http::Method::GET,
                "/api/admin/reactions?status=pending",
                "",
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
        let reactions = body["reactions"].as_array().unwrap();
        assert_eq!(reactions.len(), 2);

        // Batch moderate: approve one, spam the other.
        let resp = app
            .clone()
            .oneshot(crate::http::test_support::helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate/batch",
                &format!(
                    r#"{{"actions":[{{"id":{r1},"action":"approved"}},{{"id":{r2},"action":"spam"}}]}}"#
                ),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.repo.get_reaction(r1).await.unwrap().unwrap().status,
            "approved"
        );
        assert_eq!(
            state.repo.get_reaction(r2).await.unwrap().unwrap().status,
            "spam"
        );

        // Moderate nonexistent → 404.
        let resp = app
            .oneshot(crate::http::test_support::helpers::json_request(
                axum::http::Method::POST,
                "/api/admin/reactions/moderate",
                r#"{"id":999,"action":"approved"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn reaction_created_emission_is_signed_through_shared_sink() {
        // T19(e) behavioral: the reaction path emits through the same signed
        // T16 sink as native comments — one implementation serves both.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        state.config.webhook_signing_secret = Some("s3cr3t".to_string());
        let id = seed_approved(&state).await;
        let app = build_app(state);
        let resp = app
            .oneshot(reaction_request(
                axum::http::Method::POST,
                &format!("/api/comment/{id}/reaction"),
                Some(r#"{"reaction":"👍"}"#),
                true,
                [127, 0, 0, 1],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let mut reqs = Vec::new();
        for _ in 0..100 {
            reqs = server.received_requests().await.unwrap_or_default();
            if !reqs.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(reqs.len(), 1, "reaction.created emitted exactly once");
        let ts = reqs[0]
            .headers
            .get(crate::http::webhook::TIMESTAMP_HEADER)
            .expect("timestamp header via shared sink");
        let sig = reqs[0]
            .headers
            .get(crate::http::webhook::SIGNATURE_HEADER)
            .expect("signature header via shared sink");
        assert!(crate::http::webhook::verify_body(
            "s3cr3t",
            Some(ts.to_str().unwrap()),
            Some(sig.to_str().unwrap()),
            &reqs[0].body,
            crate::http::webhook::timestamp_now()
        ));
    }
}
