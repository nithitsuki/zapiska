//! Thin HTTP adapter for native comment submission (T19): form →
//! [`SubmitRequest`] → [`Ingress::submit`] → response. All pipeline ordering
//! lives in [`crate::ingress`]; this file owns only HTTP mapping (form
//! deserialization incl. the flattened honeypot map) and the self-delete
//! route (which goes through the T16 machine).

use axum::Form;
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use std::collections::HashMap;

use crate::error::AppError;
use crate::http::peer::ClientIdentity;
use crate::ingress::{BatcherNotify, Ingress, RealUrlStore, SubmitCtx, SubmitRequest};
use crate::moderation::WebhookSink;
use crate::state::AppState;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CommentForm {
    #[schema(example = "/blog/hello")]
    pub target_path: String,
    #[schema(example = "Alice")]
    pub author_name: String,
    #[schema(example = "https://alice.blog")]
    pub author_url: Option<String>,
    #[schema(example = "alice")]
    pub github_username: Option<String>,
    #[schema(example = "Great post!")]
    pub content: String,
    /// ID of the parent comment for threaded replies. Omit for top-level comments.
    pub parent_id: Option<i64>,
    /// Honeypot field — bots auto-fill this, humans don't see it.
    /// When non-empty, the submission is stored with `honeypot = 1`.
    /// The moderation system decides what to do with flagged comments.
    /// This is the DEFAULT name: when `HONEYPOT_FIELD` renames the trap,
    /// this field is inert and the configured name arrives via `extra`.
    pub website: Option<String>,
    /// Any other form fields — notably the CONFIGURED honeypot value when
    /// `HONEYPOT_FIELD != website`. serde cannot name a dynamic field, so
    /// the map carries it and [`crate::ingress::honeypot_filled`] resolves
    /// it against config at submission time (S1).
    #[serde(default, flatten)]
    #[schema(value_type = Object)]
    pub extra: HashMap<String, String>,
    /// Cloudflare Turnstile token rendered by the widget in the browser.
    /// Required when `TURNSTILE_ENABLED=true`; ignored otherwise.
    /// Field name matches the widget's automatic hidden input.
    #[serde(rename = "cf-turnstile-response")]
    pub cf_turnstile_response: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/comment",
    request_body(content = CommentForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 201, description = "Comment created (pending moderation)"),
        (status = 400, description = "Validation error, or Turnstile verification failed when enabled"),
        (status = 429, description = "Rate limited"),
        (status = 503, description = "Turnstile siteverify endpoint unreachable"),
    ),
    tag = "comments",
)]
pub async fn create_comment(
    State(state): State<AppState>,
    peer: ClientIdentity,
    Form(form): Form<CommentForm>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), AppError> {
    // Signed T16 sink (S3) when a webhook URL is configured; None skips the
    // moderation half of step 13. Owned here so the borrow lives long enough
    // for the submit call below.
    let sink_owned = state.config.moderation_webhook_url.as_ref().map(|url| {
        WebhookSink::created_sink_signed(
            &state.http_client,
            url,
            state.config.webhook_signing_secret.clone(),
        )
    });
    let sink_ref = sink_owned
        .as_ref()
        .map(|s| s as &dyn crate::moderation::ModerationSink);
    let notify = BatcherNotify {
        client: &state.http_client,
        batcher: &state.notifier,
    };
    let urls = RealUrlStore;
    let ctx = SubmitCtx {
        config: &state.config,
        repo: &state.repo,
        github: &state.github,
        language: &state.language,
        limiter: &state.limiter,
        http_client: &state.http_client,
        peer_ip: peer.ip(),
        peer_limiter_key: peer.limiter_key(),
        notify: &notify,
        urls: &urls,
        moderation_sink: sink_ref,
        moderation_is_sync: state.config.moderation_webhook_mode == "sync",
    };
    let req = SubmitRequest {
        target_path: form.target_path,
        author_name: form.author_name,
        author_url: form.author_url,
        github_username: form.github_username,
        content: form.content,
        parent_id: form.parent_id,
        website: form.website,
        extra_fields: form.extra,
        turnstile_token: form.cf_turnstile_response,
    };
    let stored = Ingress::submit(req, &ctx).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({ "delete_token": stored.delete_token, "status": stored.status })),
    ))
}

// ── POST /api/comment/{id}/delete ───────────────────────────

#[derive(Deserialize)]
pub struct DeleteRequest {
    pub token: String,
}

pub async fn delete_comment(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(body): Json<DeleteRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Self-delete goes through the status machine so the engine sees exactly
    // one `comment.status_changed` event (previously silent). Signed like
    // every other emission when a secret is configured (S3).
    let sink = state.config.moderation_webhook_url.as_ref().map(|url| {
        WebhookSink::status_sink_signed(
            &state.http_client,
            url,
            state.config.webhook_signing_secret.clone(),
        )
    });
    let sink_ref = sink
        .as_ref()
        .map(|s| s as &dyn crate::moderation::ModerationSink);
    let deleted =
        crate::moderation::Moderation::self_delete_comment(&state.repo, sink_ref, id, &body.token)
            .await?;
    if deleted {
        tracing::info!(id, "comment deleted via self-service token");
        Ok(Json(serde_json::json!({"success": true})))
    } else {
        Err(AppError::NotFound(
            "comment not found or token doesn't match".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::github::GitHubLookup;
    use crate::http::build_app;
    use crate::http::test_support::helpers;

    struct TestGitHub;
    #[async_trait::async_trait]
    impl GitHubLookup for TestGitHub {
        async fn lookup(&self, username: &str) -> Option<crate::github::Profile> {
            match username {
                "alice" => Some(crate::github::Profile {
                    name: "Alice Green".to_string(),
                    avatar_url: "https://avatars.githubusercontent.com/u/1".to_string(),
                }),
                _ => None,
            }
        }
    }

    fn test_state() -> (crate::state::AppState, tempfile::TempDir) {
        helpers::test_state_with_github(Arc::new(TestGitHub))
    }

    fn form_request(body: &str) -> axum::http::Request<axum::body::Body> {
        helpers::form_request("/api/comment", body)
    }

    #[tokio::test]
    async fn happy_path_returns_201_and_stores_pending() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/blog/hello&author_name=Alice&content=Great+post!";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201, "expected 201 Created");

        // Verify stored in DB with pending status.
        let comments = state.repo.list_pending(10, None, None).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author_name, "Alice");
        assert_eq!(comments[0].status, "pending");
    }

    #[tokio::test]
    async fn content_truncated_when_too_long() {
        // The spec says content is truncated to MAX_CONTENT_LEN (2000).
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let long_content = "a".repeat(2500);
        let body = format!(
            "target_path=/trunc&author_name=Alice&content={}",
            long_content
        );

        // URL-encoded, the actual content length in the form might differ from
        // the value length. We verify truncation by reading from the DB.
        let resp = app.oneshot(form_request(&body)).await.unwrap();
        assert_eq!(
            resp.status(),
            201,
            "long content is truncated, not rejected"
        );

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending.iter().find(|c| c.target_path == "/trunc").unwrap();
        assert_eq!(
            c.content.chars().count(),
            2000,
            "content truncated to 2000 chars"
        );
    }

    #[tokio::test]
    async fn author_name_too_long_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let long_name = "a".repeat(101);
        let body = format!("target_path=/x&author_name={}&content=hi", long_name);
        let resp = app.oneshot(form_request(&body)).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn target_path_invalid_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);

        // No leading slash.
        let resp = app
            .clone()
            .oneshot(form_request(
                "target_path=no-slash&author_name=A&content=hi",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn author_url_invalid_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);

        // Non-http URL.
        let resp = app
            .clone()
            .oneshot(form_request(
                "target_path=/x&author_name=A&content=hi&author_url=ftp://bad.com",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // Relative URL.
        let resp = app
            .clone()
            .oneshot(form_request(
                "target_path=/x&author_name=A&content=hi&author_url=/relative",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn github_username_enriches_author_info() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/gh&author_name=Alice&content=hi&github_username=alice";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending.iter().find(|c| c.target_path == "/gh").unwrap();
        assert_eq!(
            c.author_name, "Alice",
            "form name preserved, github is pfp-only"
        );
        assert_eq!(
            c.author_url,
            Some("https://github.com/alice".to_string()),
            "author_url derived from github_username"
        );
        assert_eq!(
            c.author_avatar,
            Some("https://avatars.githubusercontent.com/u/1".to_string())
        );
    }

    #[tokio::test]
    async fn github_unknown_username_falls_through() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/gh-unknown&author_name=Bob&content=hi&github_username=zzz-nonexistent-user-000";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending
            .iter()
            .find(|c| c.target_path == "/gh-unknown")
            .unwrap();
        assert_eq!(c.author_name, "Bob", "name stayed as form value");
        assert_eq!(
            c.author_url,
            Some("https://github.com/zzz-nonexistent-user-000".to_string()),
            "author_url derived from github_username even when lookup fails"
        );
        assert_eq!(
            c.author_avatar.as_deref(),
            Some("https://api.dicebear.com/7.x/notionists/svg?seed=zzz-nonexistent-user-000"),
            "DiceBear avatar from github_username when GitHub lookup fails"
        );
    }

    #[tokio::test]
    async fn github_fills_name_when_form_name_empty() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/gh-name&author_name=&content=hi&github_username=alice";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            201,
            "empty name with github_username must succeed"
        );

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending
            .iter()
            .find(|c| c.target_path == "/gh-name")
            .unwrap();
        assert_eq!(
            c.author_name, "Alice Green",
            "name should come from GitHub when form name is empty"
        );
        assert_eq!(
            c.author_url,
            Some("https://github.com/alice".to_string()),
            "author_url derived from github_username"
        );
        assert_eq!(
            c.author_avatar,
            Some("https://avatars.githubusercontent.com/u/1".to_string())
        );
    }

    #[tokio::test]
    async fn github_unknown_fills_name_with_username_when_name_empty() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/gh-unknown-empty&author_name=&content=hi&github_username=zzz-nonexistent-user-000";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            201,
            "empty name with github_username must not 400"
        );

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending
            .iter()
            .find(|c| c.target_path == "/gh-unknown-empty")
            .unwrap();
        assert_eq!(
            c.author_name, "zzz-nonexistent-user-000",
            "fallback to github_username when name empty and GitHub lookup fails"
        );
        assert_eq!(
            c.author_url,
            Some("https://github.com/zzz-nonexistent-user-000".to_string()),
            "author_url derived from github_username"
        );
        assert_eq!(
            c.author_avatar.as_deref(),
            Some("https://api.dicebear.com/7.x/notionists/svg?seed=zzz-nonexistent-user-000"),
        );
    }

    #[tokio::test]
    async fn author_url_sets_dicebear_avatar() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body =
            "target_path=/url-test&author_name=Alice&content=hi&author_url=https://alice.blog";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending
            .iter()
            .find(|c| c.target_path == "/url-test")
            .unwrap();
        assert_eq!(c.author_url, Some("https://alice.blog".to_string()));
        assert_eq!(
            c.author_avatar,
            Some("https://api.dicebear.com/7.x/notionists/svg?seed=alice.blog".to_string())
        );
    }

    #[tokio::test]
    async fn blocked_author_url_falls_back_to_dicebear_without_panic() {
        // T05: the unauthenticated SSRF hole — a loopback author_url must be
        // refused before connecting and fall back to dicebear, not fail the
        // submission.
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body =
            "target_path=/ssrf-avatar&author_name=Alice&content=hi&author_url=http://127.0.0.1:9/";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending
            .iter()
            .find(|c| c.target_path == "/ssrf-avatar")
            .unwrap();
        assert_eq!(
            c.author_avatar,
            Some("https://api.dicebear.com/7.x/notionists/svg?seed=127.0.0.1".to_string())
        );
    }

    #[tokio::test]
    async fn no_github_no_url_uses_form_name_only() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/bare&author_name=Bob&content=hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending.iter().find(|c| c.target_path == "/bare").unwrap();
        assert_eq!(c.author_name, "Bob");
        assert!(c.author_url.is_none());
        assert_eq!(
            c.author_avatar.as_deref(),
            Some("https://api.dicebear.com/7.x/notionists/svg?seed=anonymous"),
            "DiceBear avatar should be set as fallback"
        );
    }

    #[tokio::test]
    async fn response_201_returns_delete_token() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "target_path=/token-test&author_name=Alice&content=hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let resp_body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert!(
            resp_body["delete_token"].as_str().unwrap().len() >= 16,
            "delete_token must be present and at least 16 hex chars"
        );
    }

    #[tokio::test]
    async fn stored_content_is_sanitized() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body =
            "target_path=/sanitize&author_name=Alice&content=<script>alert(1)</script><p>safe</p>";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending
            .iter()
            .find(|c| c.target_path == "/sanitize")
            .unwrap();
        assert!(!c.content.contains("<script>"), "script tag stripped");
        assert!(c.content.contains("<p>safe</p>"), "safe html preserved");
    }

    #[tokio::test]
    async fn echo_leakage_prevention() {
        // Submit payload with script + control char; verify no user input is
        // reflected in the response (only the delete_token is returned).
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body =
            "target_path=/leak&author_name=Bad%00Guy&content=<script>alert(1)</script><p>text</p>";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        // Response body must NOT echo user input — only delete_token.
        let resp_body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        let token = resp_body["delete_token"]
            .as_str()
            .expect("delete_token present");
        assert!(!token.contains("alert"), "no script reflection in response");
        assert!(!token.contains("Bad"), "no author_name reflection");

        // Content was sanitized in DB.
        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending.iter().find(|c| c.target_path == "/leak").unwrap();
        assert!(!c.content.contains("<script>"), "script content sanitized");
        assert!(c.content.contains("<p>text</p>"), "safe text preserved");
        // author_name had a control char stripped
        assert_eq!(c.author_name, "BadGuy", "control char stripped from name");
    }

    // ── Language filter tests ────────────────────────────────

    async fn post_content(app: &axum::Router, content: &str) -> axum::http::StatusCode {
        let body = format!(
            "target_path=/lang&author_name=Alice&content={}",
            urlencode(content)
        );
        let resp = app.clone().oneshot(form_request(&body)).await.unwrap();
        resp.status()
    }

    fn urlencode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    #[tokio::test]
    async fn language_filter_off_by_default() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = post_content(&app, "これは日本語のコメントです。").await;
        assert_eq!(
            resp,
            axum::http::StatusCode::CREATED,
            "filtering off by default"
        );
    }

    #[tokio::test]
    async fn whitelist_rejects_other_languages() {
        let (mut state, _dir) = test_state();
        state.config.comment_lang_allowed = vec!["en".to_string()];
        state.language = crate::language::LanguageGate::new(&state.config);
        let app = build_app(state.clone());

        let resp = post_content(
            &app,
            "This is a perfectly normal English comment that should pass.",
        )
        .await;
        assert_eq!(
            resp,
            axum::http::StatusCode::CREATED,
            "whitelisted language accepted"
        );

        let resp = post_content(&app, "これは日本語のコメントです。").await;
        assert_eq!(
            resp,
            axum::http::StatusCode::BAD_REQUEST,
            "non-whitelisted language rejected"
        );

        // Rejected comments are NOT stored.
        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        assert_eq!(pending.len(), 1, "only the English comment stored");
    }

    #[tokio::test]
    async fn blacklist_rejects_listed_languages() {
        let (mut state, _dir) = test_state();
        state.config.comment_lang_blocked = vec!["ja".to_string()];
        state.language = crate::language::LanguageGate::new(&state.config);
        let app = build_app(state);

        let resp = post_content(&app, "This is an English comment.").await;
        assert_eq!(resp, axum::http::StatusCode::CREATED);
        let resp = post_content(&app, "これは日本語のコメントです。").await;
        assert_eq!(resp, axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn emoji_policy_never_rejects_emoji_only() {
        let (mut state, _dir) = test_state();
        state.config.comment_lang_allowed = vec!["en".to_string()];
        state.config.comment_lang_allow_emoji = "never".to_string();
        state.language = crate::language::LanguageGate::new(&state.config);
        let app = build_app(state);

        let resp = post_content(&app, "👍👍👍👍").await;
        assert_eq!(
            resp,
            axum::http::StatusCode::BAD_REQUEST,
            "emoji-only rejected under never"
        );
        let resp = post_content(&app, "Nice comment 👍").await;
        assert_eq!(resp, axum::http::StatusCode::CREATED, "mixed text accepted");
    }

    #[tokio::test]
    async fn emoji_policy_if_unknown_rejects_gibberish() {
        let (mut state, _dir) = test_state();
        state.config.comment_lang_allowed = vec!["en".to_string()];
        state.config.comment_lang_allow_emoji = "if_unknown".to_string();
        state.language = crate::language::LanguageGate::new(&state.config);
        let app = build_app(state);

        let resp = post_content(&app, "👍👍👍👍").await;
        assert_eq!(
            resp,
            axum::http::StatusCode::CREATED,
            "emoji-only accepted under if_unknown"
        );
        let resp = post_content(&app, "qzx qzx qzx qzx qzx qzx").await;
        assert_eq!(
            resp,
            axum::http::StatusCode::BAD_REQUEST,
            "undetectable text rejected"
        );
    }

    #[tokio::test]
    async fn rejected_comment_error_is_clear() {
        let (mut state, _dir) = test_state();
        state.config.comment_lang_allowed = vec!["en".to_string()];
        state.language = crate::language::LanguageGate::new(&state.config);
        let app = build_app(state);
        let body = "target_path=/lang&author_name=Alice&content=これは日本語のコメントです。";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        let resp_body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert!(
            resp_body["error"]
                .as_str()
                .unwrap()
                .contains("not in the allowed list"),
            "error explains why: {}",
            resp_body["error"]
        );
    }

    // ── Turnstile tests ──────────────────────────────────────

    #[tokio::test]
    async fn turnstile_disabled_ignores_missing_token() {
        // Default test_state has turnstile disabled: no cf-turnstile-response
        // field, comment goes straight through as 201.
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/ts&author_name=Bob&content=Hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201, "disabled turnstile should not block");
    }

    #[tokio::test]
    async fn turnstile_enabled_rejects_missing_token() {
        let server = wiremock::MockServer::start().await;
        let (state, _dir) =
            helpers::test_state_with_turnstile(format!("{}/siteverify", server.uri()));
        let app = build_app(state);
        let body = "target_path=/ts&author_name=Bob&content=Hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 400, "missing token must be rejected");
    }

    #[tokio::test]
    async fn turnstile_enabled_rejects_failed_verification() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"success": false, "error-codes": ["invalid-input-response"]}),
            ))
            .mount(&server)
            .await;

        let (state, _dir) =
            helpers::test_state_with_turnstile(format!("{}/siteverify", server.uri()));
        let app = build_app(state.clone());
        let body = "target_path=/ts&author_name=Bob&content=Hi&cf-turnstile-response=stale";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 400, "failed verification must be 400");

        // No comment should have been stored.
        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        assert!(pending.is_empty(), "rejected comment must not be persisted");
    }

    #[tokio::test]
    async fn turnstile_enabled_accepts_successful_verification() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"success": true, "error-codes": []})),
            )
            .mount(&server)
            .await;

        let (state, _dir) =
            helpers::test_state_with_turnstile(format!("{}/siteverify", server.uri()));
        let app = build_app(state.clone());
        let body = "target_path=/ts&author_name=Bob&content=Hi&cf-turnstile-response=good-token";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            201,
            "passing verification should store comment"
        );

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_path, "/ts");
    }

    #[tokio::test]
    async fn turnstile_enabled_returns_503_when_siteverify_unreachable() {
        // Point at a port that nobody listens on — connection refused.
        let (state, _dir) =
            helpers::test_state_with_turnstile("https://127.0.0.1:1/siteverify".to_string());
        let app = build_app(state);
        let body = "target_path=/ts&author_name=Bob&content=Hi&cf-turnstile-response=whatever";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            503,
            "fail closed when siteverify is unreachable"
        );
    }

    // ── Notification tests (Telegram / Slack) ────────────────

    /// Poll the mock server until `n` requests arrive (notifications are
    /// fire-and-forget tasks, so we can't assert synchronously).
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

    #[tokio::test]
    async fn telegram_notified_on_new_comment() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(&server)
            .await;

        let (state, _dir) = helpers::test_state_with_notifications(server.uri(), None);
        let app = build_app(state.clone());
        let body = "target_path=/blog/hello&author_name=Alice&content=Great+post!";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            201,
            "comment stored regardless of notification"
        );

        let reqs = wait_for_requests(&server, 1).await;
        assert!(!reqs.is_empty(), "telegram must receive a request");
        let req = &reqs[0];
        assert_eq!(req.method, "POST");
        assert!(
            req.url
                .path()
                .starts_with("/botTESTTOKEN123:test-secret/sendMessage"),
            "telegram sendMessage endpoint with bot token, got {}",
            req.url.path()
        );
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["chat_id"], "@test_alerts");
        assert_eq!(body["parse_mode"], "HTML");
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("/blog/hello"), "path in message: {text}");
        assert!(text.contains("Alice"), "author in message: {text}");
        assert!(text.contains("Great post!"), "content in message: {text}");
        assert!(
            text.contains("/api/admin/comments/"),
            "admin path in message: {text}"
        );
    }

    #[tokio::test]
    async fn slack_notified_on_new_comment() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (state, _dir) = helpers::test_state_with_notifications(
            "https://unused.invalid".to_string(),
            Some(server.uri()),
        );
        let app = build_app(state.clone());
        let body = "target_path=/blog/hello&author_name=Alice&content=Hello+Slack!";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let reqs = wait_for_requests(&server, 1).await;
        assert!(!reqs.is_empty(), "slack must receive a request");
        let req = &reqs[0];
        assert_eq!(req.method, "POST");
        let payload: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        let text = payload["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(text.contains("New comment on `/blog/hello`"), "{text}");
        assert!(text.contains("Alice"), "{text}");
        assert!(text.contains("Hello Slack!"), "{text}");
    }

    #[tokio::test]
    async fn notification_failure_does_not_affect_comment() {
        // Both channels fail (500 + unreachable); the comment must still be 201.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (state, _dir) =
            helpers::test_state_with_notifications(server.uri(), Some(server.uri()));
        let app = build_app(state.clone());
        let body = "target_path=/resilient&author_name=Alice&content=hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            201,
            "notifications must never fail the request"
        );

        let stored = state.repo.list_pending(10, None, None).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].target_path, "/resilient");
    }

    #[tokio::test]
    async fn honeypot_comment_still_notifies_with_flag() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(&server)
            .await;

        let (state, _dir) = helpers::test_state_with_notifications(server.uri(), None);
        let app = build_app(state.clone());
        let body = "target_path=/honey&author_name=Bot&content=spam&website=spammer.example";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let reqs = wait_for_requests(&server, 1).await;
        assert!(!reqs.is_empty());
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body["text"].as_str().unwrap().contains("honeypot"),
            "honeypot flag surfaced in notification"
        );
    }

    // ── Batching tests (windowed digests) ────────────────────

    /// Mount a telegram-ok mock and post `comments` to `path`, returning the
    /// requests the mock server received.
    async fn post_and_collect(
        server: &wiremock::MockServer,
        path: &str,
        comments: &[(&str, &str)],
    ) -> Vec<wiremock::Request> {
        let (state, _dir) = helpers::test_state_with_batcher(2, 0, "page", server.uri(), None);
        let app = build_app(state.clone());

        for (name, content) in comments {
            let body = format!("target_path={path}&author_name={name}&content={content}");
            let resp = app.clone().oneshot(form_request(&body)).await.unwrap();
            assert_eq!(resp.status(), 201, "comment {name} stored");
        }

        wait_for_requests(server, 1).await
    }

    #[tokio::test]
    async fn batched_comments_produce_single_digest() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(&server)
            .await;

        let reqs = post_and_collect(
            &server,
            "/blog/hello",
            &[("Alice", "first"), ("Bob", "second"), ("Carol", "third")],
        )
        .await;
        assert_eq!(reqs.len(), 1, "three comments must produce ONE message");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("3 new comments on /blog/hello"), "{text}");
        assert!(
            text.contains("Alice, Bob, Carol"),
            "commenters listed: {text}"
        );
        assert!(text.contains("first"), "preview present: {text}");
    }

    #[tokio::test]
    async fn threshold_flush_mid_window() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(&server)
            .await;

        // 10s window, but flush as soon as 2 comments accumulate.
        let (state, _dir) = helpers::test_state_with_batcher(10, 2, "page", server.uri(), None);
        let app = build_app(state.clone());

        for (name, content) in [("Alice", "one"), ("Bob", "two"), ("Carol", "three")] {
            let body = format!("target_path=/blog/hello&author_name={name}&content={content}");
            let resp = app.clone().oneshot(form_request(&body)).await.unwrap();
            assert_eq!(resp.status(), 201);
        }

        // The first two hit the threshold → immediate digest of 2. The third
        // opens a fresh window (10s); we assert only on the immediate flush.
        let reqs = wait_for_requests(&server, 1).await;
        assert_eq!(reqs.len(), 1, "threshold flush must fire immediately");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("2 new comments on /blog/hello"), "{text}");
    }

    #[tokio::test]
    async fn global_granularity_batches_across_pages() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(&server)
            .await;

        let (state, _dir) = helpers::test_state_with_batcher(2, 0, "global", server.uri(), None);
        let app = build_app(state.clone());

        for (path, name) in [("/blog/a", "Alice"), ("/blog/b", "Bob")] {
            let body = format!("target_path={path}&author_name={name}&content=hello");
            let resp = app.clone().oneshot(form_request(&body)).await.unwrap();
            assert_eq!(resp.status(), 201);
        }

        let reqs = wait_for_requests(&server, 1).await;
        assert_eq!(reqs.len(), 1, "global window batches both pages");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("2 new comments on the site"), "{text}");
        assert!(text.contains("/blog/a"), "both pages previewed: {text}");
        assert!(text.contains("/blog/b"), "both pages previewed: {text}");
    }

    #[tokio::test]
    async fn discord_receives_batched_digest() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (state, _dir) = helpers::test_state_with_batcher(
            2,
            0,
            "page",
            "https://unused.invalid".to_string(),
            Some(server.uri()),
        );
        let app = build_app(state.clone());

        let body = "target_path=/blog/hello&author_name=Alice&content=hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let reqs = wait_for_requests(&server, 1).await;
        assert!(!reqs.is_empty(), "discord must receive the digest");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["username"], "zapiska");
        let content = body["content"].as_str().unwrap();
        assert!(
            content.contains("1 new comment on /blog/hello"),
            "{content}"
        );
        assert!(
            content.contains("/api/admin/pending?path=/blog/hello"),
            "{content}"
        );
    }

    // ── T16 self-delete status machine ───────────────────────

    async fn wait_for_moderation(
        server: &wiremock::MockServer,
        n: usize,
    ) -> Vec<wiremock::Request> {
        for _ in 0..100 {
            let reqs = server.received_requests().await.unwrap_or_default();
            if reqs.len() >= n {
                return reqs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        server.received_requests().await.unwrap_or_default()
    }

    fn delete_request(id: i64, token: &str) -> axum::http::Request<axum::body::Body> {
        helpers::json_request(
            axum::http::Method::POST,
            &format!("/api/comment/{id}/delete"),
            &format!(r#"{{"token":"{token}"}}"#),
        )
    }

    #[tokio::test]
    async fn self_delete_fires_status_changed() {
        // Previously silent: the engine never saw owner deletes.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/t16-del".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "T16".to_string(),
                author_url: None,
                author_avatar: None,
                content: "hi".to_string(),
                parent_id: None,
                depth: 0,
                honeypot: false,
                delete_token: Some("tok-del".to_string()),
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap();
        // json_request adds an admin Bearer header; the delete route ignores
        // it (public route), so reuse is safe.
        let app = build_app(state.clone());
        let resp = app.oneshot(delete_request(id, "tok-del")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.repo.get_comment(id).await.unwrap().unwrap().status,
            "deleted"
        );
        let reqs = wait_for_moderation(&server, 1).await;
        assert_eq!(reqs.len(), 1, "self-delete must fire status_changed");
        let p: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(p["event"], "comment.status_changed");
        assert_eq!(p["id"], id);
        assert_eq!(p["old_status"], "pending");
        assert_eq!(p["new_status"], "deleted");
        assert_eq!(p["changed_by"], "self");
    }

    #[tokio::test]
    async fn self_delete_wrong_token_is_404_without_event() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/t16-delbad".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "T16".to_string(),
                author_url: None,
                author_avatar: None,
                content: "hi".to_string(),
                parent_id: None,
                depth: 0,
                honeypot: false,
                delete_token: Some("tok-ok".to_string()),
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap();
        let app = build_app(state);
        let resp = app.oneshot(delete_request(id, "tok-wrong")).await.unwrap();
        assert_eq!(resp.status(), 404);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "failed delete must not emit"
        );
    }

    #[tokio::test]
    async fn double_self_delete_is_404_without_second_event() {
        // M7: deleting an already-deleted row 404s like a wrong token and
        // emits nothing further — exactly one event for the whole lifecycle.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/t16-deldbl".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "T16".to_string(),
                author_url: None,
                author_avatar: None,
                content: "hi".to_string(),
                parent_id: None,
                depth: 0,
                honeypot: false,
                delete_token: Some("tok-dbl".to_string()),
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap();
        let app = build_app(state.clone());
        let resp = app
            .clone()
            .oneshot(delete_request(id, "tok-dbl"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = app.oneshot(delete_request(id, "tok-dbl")).await.unwrap();
        assert_eq!(resp.status(), 404, "second delete of a deleted row 404s");
        let reqs = wait_for_moderation(&server, 1).await;
        assert_eq!(reqs.len(), 1, "exactly one event across both deletes");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            1,
            "no second event"
        );
    }

    #[tokio::test]
    async fn comment_created_emission_is_signed_through_shared_sink() {
        // T19(e) behavioral, comment half: the native path emits through the
        // same signed T16 sink as reactions — one implementation serves both.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (mut state, _dir) = helpers::test_state();
        state.config.moderation_webhook_url = Some(format!("{}/hook", server.uri()));
        state.config.webhook_signing_secret = Some("s3cr3t".to_string());
        let app = build_app(state);
        let resp = app
            .oneshot(form_request(
                "target_path=/signed&author_name=Ada&content=hi",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let reqs = wait_for_moderation(&server, 1).await;
        assert_eq!(reqs.len(), 1, "comment.created emitted exactly once");
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

    #[tokio::test]
    async fn honeypot_configured_name_flags_and_legacy_inert() {
        // S1 end-to-end: HONEYPOT_FIELD=company → filling company flags,
        // filling website alone does not.
        let (mut state, _dir) = helpers::test_state();
        state.config.honeypot_field = "company".to_string();
        let app = build_app(state.clone());
        let resp = app
            .clone()
            .oneshot(form_request(
                "target_path=/hp-a&author_name=Bot&content=spam&company=spam+co",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending.iter().find(|c| c.target_path == "/hp-a").unwrap();
        assert!(c.honeypot, "configured field must flag");

        let resp = app
            .oneshot(form_request(
                "target_path=/hp-b&author_name=Bot&content=spam&website=spammer.example",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending.iter().find(|c| c.target_path == "/hp-b").unwrap();
        assert!(!c.honeypot, "legacy field inert under a custom name");
    }

    #[tokio::test]
    async fn hostile_github_username_rejected_at_handler() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(form_request(
                "target_path=/gh-hostile&author_name=Ada&content=hi&github_username=a%3Cb%3E",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "hostile github_username must be a 400");
    }
}
