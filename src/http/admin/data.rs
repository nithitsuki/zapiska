//! Data export/import endpoints for backups and migration.
//!
//! - `GET /api/admin/export` - full dump of all five tables as JSON.
//! - `POST /api/admin/import` — restore a dump via [`Repo::restore`]
//!   (idempotent; preserves ids, statuses, and timestamps; re-sanitizes
//!   comment content).
//!
//! The handler keeps auth + JSON only: version check, policy mapping
//! (`ImportFile` + `Config` → [`RestoreInput`]), one
//! [`Repo::restore`](crate::db::repo::Repo::restore) call, response mapping.
//! All restore intelligence — ordering, per-row skip semantics, orphan
//! tolerance, salt re-derivation, overlap refusal — lives in
//! `src/db/repo/restore.rs`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::db::RestoreError;
use crate::db::repo::{
    Comment, CommentReaction, CommentUrl, ExportSnapshot, GithubProfile, RestoreInput,
    RestoreReport, WebmentionSeen,
};
use crate::error::AppError;
use crate::state::AppState;

/// Format version of the export document. Bump on breaking shape changes;
/// imports reject any other version.
pub const EXPORT_VERSION: i64 = 1;
/// Max accepted import body. Comment exports can legitimately exceed the
/// 8 KB form-body limit, so this route gets its own ceiling.
pub const MAX_IMPORT_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize)]
pub struct ExportFile {
    pub version: i64,
    pub exported_at: String,
    /// Whether the exporting server had `IP_HASH_SECRET` set. Stored IP
    /// hashes are salted with that secret, so the secret itself is part of
    /// the backup: back up `.env` alongside this file. Never the secret.
    pub ip_hash_salted: Option<bool>,
    pub comments: Vec<Comment>,
    pub webmention_seen: Vec<WebmentionSeen>,
    pub comment_urls: Vec<CommentUrl>,
    pub github_profiles: Vec<GithubProfile>,
    pub comment_reactions: Vec<CommentReaction>,
}

#[derive(Deserialize, Default)]
pub struct ImportFile {
    pub version: Option<i64>,
    /// Salt status of the exporting server. `None` = pre-flag export,
    /// unverifiable — see the import warning logic.
    #[serde(default)]
    pub ip_hash_salted: Option<bool>,
    pub comments: Option<Vec<Comment>>,
    pub webmention_seen: Option<Vec<WebmentionSeen>>,
    pub comment_urls: Option<Vec<CommentUrl>>,
    pub github_profiles: Option<Vec<GithubProfile>>,
    pub comment_reactions: Option<Vec<CommentReaction>>,
    /// Explicit overwrite policy for id collisions (B-21): `false` (default)
    /// refuses when an exported id holds different live data, `true`
    /// overwrites live rows. Restore targets an empty database; set this
    /// only to deliberately re-run a reviewed migration.
    #[serde(default)]
    pub force: bool,
}

/// Import response body shape: on success the handler answers the
/// storage-layer [`RestoreReport`] as JSON (one shared type, so handler
/// counts and storage counts can never drift), including the per-section
/// skip counts and the salt-mismatch `warning`. A mid-restore abort answers
/// 500 with the partial report plus an `error` key (see [`abort_response`]).
pub type ImportResponse = RestoreReport;

/// GET /api/admin/export — full JSON dump (backup/migration source).
/// The five tables come from ONE connection in one read transaction
/// (T15 unit of work), so the dump is a single WAL snapshot, not five.
pub async fn export(State(state): State<AppState>) -> Result<Json<ExportFile>, AppError> {
    let ExportSnapshot {
        comments,
        seen: webmention_seen,
        urls: comment_urls,
        profiles: github_profiles,
        reactions: comment_reactions,
    } = state.repo.export_snapshot().await?;

    Ok(Json(ExportFile {
        version: EXPORT_VERSION,
        exported_at: crate::timeutil::now_iso8601(),
        ip_hash_salted: Some(state.config.ip_hash_secret.is_some()),
        comments,
        webmention_seen,
        comment_urls,
        github_profiles,
        comment_reactions,
    }))
}

/// POST /api/admin/import — restore an export document. Idempotent: rows are
/// upserted by natural key or id, so re-importing the same document is a
/// no-op; refusals (unknown version, live-id collision without `force`)
/// happen before the first write. A mid-restore storage abort answers 500
/// carrying the counts-so-far (see [`abort_response`]), never a bare message.
pub async fn import(
    State(state): State<AppState>,
    Json(body): Json<ImportFile>,
) -> Result<Response, AppError> {
    if body.version != Some(EXPORT_VERSION) {
        return Err(AppError::BadRequest(format!(
            "unsupported export version {:?}, expected {EXPORT_VERSION}",
            body.version
        )));
    }
    let input = RestoreInput {
        comments: body.comments.unwrap_or_default(),
        seen: body.webmention_seen.unwrap_or_default(),
        urls: body.comment_urls.unwrap_or_default(),
        profiles: body.github_profiles.unwrap_or_default(),
        reactions: body.comment_reactions.unwrap_or_default(),
        export_salted: body.ip_hash_salted,
        ip_hash_secret: state.config.ip_hash_secret.clone(),
        max_content_len: state.config.max_content_len,
        max_author_len: state.config.max_author_len,
        force: body.force,
    };
    match state.repo.restore(input).await {
        Ok(report) => Ok(Json::<ImportResponse>(report).into_response()),
        Err(RestoreError::Refused(msg)) => Err(AppError::BadRequest(msg)),
        Err(RestoreError::Aborted { source, partial }) => Ok(abort_response(&source, &partial)),
    }
}

/// 500 carrying counts-so-far for a mid-restore abort (G4.1): the partial
/// report plus the storage error, so the operator sees what landed and what
/// skipped before the failure instead of a bare message.
fn abort_response(source: &crate::db::RepoError, partial: &RestoreReport) -> Response {
    tracing::warn!(err = %source, "import aborted mid-restore; returning counts-so-far");
    let mut body = serde_json::to_value(partial).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "error".to_string(),
            serde_json::Value::String(source.to_string()),
        );
    }
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use crate::http::test_support::helpers;

    fn json_request(
        method: axum::http::Method,
        uri: &str,
        body: &str,
    ) -> axum::http::Request<axum::body::Body> {
        helpers::json_request(method, uri, body)
    }

    #[tokio::test]
    async fn import_fk_broken_reaction_skips_with_counts() {
        // Uniform skip-and-count (B-19): one valid comment plus a reaction
        // pointing at a comment that was never imported completes with 200
        // and reports the skip — the old handler `?`-aborted with a 500 on a
        // half-done restore and the counts never reached the operator.
        let (state, _dir) = helpers::test_state();
        let app = crate::http::build_app(state.clone());
        let body = serde_json::json!({
            "version": 1,
            "comments": [{
                "id": 1, "target_path": "/t17-red", "comment_type": "native",
                "source_url": null, "author_name": "Ada", "author_url": null,
                "author_avatar": null, "content": "hello", "status": "approved",
                "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00",
                "parent_id": null, "depth": 0, "honeypot": false,
                "delete_token": null, "submitter_ip": null,
                "submitter_ip_hash": null, "content_hash": null
            }],
            "comment_reactions": [{
                "id": 1, "comment_id": 999, "reaction": "👍",
                "identifier": "admin", "status": "approved",
                "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00"
            }]
        })
        .to_string();
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &body,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "FK-broken reaction must skip, not abort the restore"
        );
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["comments_imported"], 1);
        assert_eq!(result["comments_skipped"], 0);
        assert_eq!(result["comment_reactions_imported"], 0);
        assert_eq!(result["comment_reactions_skipped"], 1);
        assert!(state.repo.get_comment(1).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn import_live_db_colliding_ids_refuse() {
        // B-21 refuse-by-default: a live moderated comment whose id collides
        // with a differing export row refuses with 400 and stays untouched.
        let (state, _dir) = helpers::test_state();
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/live".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Live".to_string(),
                author_url: None,
                author_avatar: None,
                content: "live decision".to_string(),
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
        let app = crate::http::build_app(state.clone());
        let body = serde_json::json!({
            "version": 1,
            "comments": [{
                "id": id, "target_path": "/live", "comment_type": "native",
                "source_url": null, "author_name": "Backup", "author_url": null,
                "author_avatar": null, "content": "stale backup",
                "status": "pending",
                "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00",
                "parent_id": null, "depth": 0, "honeypot": false,
                "delete_token": null, "submitter_ip": null,
                "submitter_ip_hash": null, "content_hash": null
            }]
        })
        .to_string();
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &body,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "colliding live id must refuse, not clobber"
        );
        let live = state.repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(live.content, "live decision");
        assert_eq!(live.status, "approved");
    }

    #[tokio::test]
    async fn import_force_overwrites_colliding_live_row() {
        // The explicit `"force": true` policy overwrites after review.
        let (state, _dir) = helpers::test_state();
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/live".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Live".to_string(),
                author_url: None,
                author_avatar: None,
                content: "live".to_string(),
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
        let app = crate::http::build_app(state.clone());
        let body = serde_json::json!({
            "version": 1,
            "force": true,
            "comments": [{
                "id": id, "target_path": "/live", "comment_type": "native",
                "source_url": null, "author_name": "Live", "author_url": null,
                "author_avatar": null, "content": "operator reviewed backup",
                "status": "approved",
                "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00",
                "parent_id": null, "depth": 0, "honeypot": false,
                "delete_token": null, "submitter_ip": null,
                "submitter_ip_hash": null, "content_hash": null
            }]
        })
        .to_string();
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.repo.get_comment(id).await.unwrap().unwrap().content,
            "operator reviewed backup"
        );
    }

    #[tokio::test]
    async fn import_emits_zero_moderation_webhooks() {
        // (d): restored statuses are historical data, not transitions — even
        // with a moderation webhook configured, an import firing approved and
        // spam rows emits nothing (an import firing thousands of
        // status_changed would DDoS the engine).
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"action": "approved"})),
            )
            .mount(&server)
            .await;
        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(server.uri());
        let app = crate::http::build_app(state.clone());
        let body = serde_json::json!({
            "version": 1,
            "comments": [
                {
                    "id": 1, "target_path": "/t17-hook", "comment_type": "native",
                    "source_url": null, "author_name": "Ada", "author_url": null,
                    "author_avatar": null, "content": "approved row",
                    "status": "approved",
                    "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00",
                    "parent_id": null, "depth": 0, "honeypot": false,
                    "delete_token": null, "submitter_ip": null,
                    "submitter_ip_hash": null, "content_hash": null
                },
                {
                    "id": 2, "target_path": "/t17-hook", "comment_type": "native",
                    "source_url": null, "author_name": "Bob", "author_url": null,
                    "author_avatar": null, "content": "spam row",
                    "status": "spam",
                    "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00",
                    "parent_id": null, "depth": 0, "honeypot": false,
                    "delete_token": null, "submitter_ip": null,
                    "submitter_ip_hash": null, "content_hash": null
                }
            ],
            "comment_reactions": [{
                "id": 1, "comment_id": 1, "reaction": "👍",
                "identifier": "admin", "status": "approved",
                "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00"
            }]
        })
        .to_string();
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // Fire-and-forget delivery spawns a task: poll up to ~2 s so a late
        // regressed emission fails instead of passing silently after a nap.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let received = server.received_requests().await.unwrap();
            assert!(
                received.is_empty(),
                "restore must emit zero moderation webhooks, got {}",
                received.len()
            );
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            state.repo.get_comment(1).await.unwrap().unwrap().status,
            "approved"
        );
        assert_eq!(
            state.repo.get_comment(2).await.unwrap().unwrap().status,
            "spam"
        );
    }

    #[tokio::test]
    async fn abort_response_carries_counts_so_far_with_500() {
        // G4.1 handler half: a mid-restore abort maps to a 500 whose body
        // carries the partial report plus the storage error.
        use axum::http::StatusCode;
        let partial = super::RestoreReport {
            comments_imported: 2,
            comments_skipped: 1,
            ..super::RestoreReport::default()
        };
        let resp =
            super::abort_response(&crate::db::RepoError::Io("disk full".to_string()), &partial);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["comments_imported"], 2);
        assert_eq!(body["comments_skipped"], 1);
        assert!(
            body["error"].as_str().unwrap().contains("disk full"),
            "unexpected body: {body}"
        );
    }
}
