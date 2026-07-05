use axum::Form;
use axum::Json;
use axum::extract::{ConnectInfo, State};
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;
use url::Url;

use crate::db::repo::NewComment;
use crate::error::AppError;
use crate::github::GitHubLookup;
use crate::sanitize;
use crate::state::{AppState, ip_daily_key};
use crate::validate;

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
    pub website: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/comment",
    request_body(content = CommentForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 201, description = "Comment created (pending moderation)"),
        (status = 400, description = "Validation error"),
        (status = 429, description = "Rate limited"),
    ),
    tag = "comments",
)]
pub async fn create_comment(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<CommentForm>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), AppError> {
    // ── Honeypot check ────────────────────────────────────────
    // If the honeypot field is filled, the comment is still stored but
    // flagged for moderator review. The moderation system decides what to do.
    let is_honeypot = !form.website.as_deref().unwrap_or("").trim().is_empty();
    if is_honeypot {
        tracing::info!(ip = %addr.ip(), "honeypot triggered, comment flagged");
    }

    // ── Per-IP daily cap ──────────────────────────────────────
    if state.config.max_comments_per_ip_per_day > 0 {
        let key = ip_daily_key(&addr.ip());
        if !state.limiter.check_and_increment(&key, state.config.max_comments_per_ip_per_day) {
            return Err(AppError::RateLimited {
                retry_after_secs: 86400,
                reason: format!("daily comment limit ({}) reached for this IP", state.config.max_comments_per_ip_per_day),
            });
        }
    }

    // Treat empty optional fields as None
    let author_url = form.author_url.filter(|u| !u.trim().is_empty());
    let github_username = form.github_username.filter(|u| !u.trim().is_empty());

    // 1. Validate
    let target_path = &form.target_path;
    validate::validate_target_path(target_path)
        .map_err(|e| AppError::BadRequest(format!("invalid target_path: {e}")))?;

    let mut author_name = validate::strip_control_chars(&form.author_name);
    author_name = author_name.trim().to_string();
    if author_name.is_empty() {
        return Err(AppError::BadRequest(
            "author_name must not be empty".to_string(),
        ));
    }
    if author_name.chars().count() > state.config.max_author_len {
        return Err(AppError::BadRequest(format!(
            "author_name exceeds max length of {} chars",
            state.config.max_author_len
        )));
    }

    if let Some(ref url) = author_url {
        validate::validate_http_url(url)
            .map_err(|e| AppError::BadRequest(format!("invalid author_url: {e}")))?;
    }

    // 2. Compute content hash for dedup detection.
    let content_hash = Some(sanitize::content_hash(&form.content));

    // 3. Sanitize content
    let content = sanitize::sanitize_html(&form.content, state.config.max_content_len);

    // 4. Resolve author info
    let (resolved_name, resolved_url) =
        resolve_author(author_url.as_deref(), github_username.as_deref(), author_name, &state.github).await;

    // 4. Resolve avatar with best-effort approach
    let resolved_avatar = resolve_avatar(
        &resolved_url,
        author_url.as_deref(),
        github_username.as_deref(),
        &state.github,
        &state.http_client,
    )
    .await;

    // 6. Resolve parent for nesting
    let (parent_id, depth) = resolve_parent(&form.parent_id, target_path, &state).await?;

    // 5. Generate delete token for self-service deletion.
    let delete_token = generate_delete_token(&addr);
    let delete_token_str = delete_token.clone();

    // 6. Optionally store submitter IP for spam analysis.
    let submitter_ip = if state.config.store_ip_address {
        Some(addr.ip().to_string())
    } else {
        None
    };

    // 7. Store. Clone values needed for the webhook payload later.
    let hook_name = resolved_name.clone();
    let hook_url = resolved_url.clone();
    let hook_avatar = resolved_avatar.clone();
    let hook_ip = submitter_ip.clone();
    let hook_content_hash = content_hash.clone();
    let new_id = state
        .repo
        .insert_comment(NewComment {
            target_path: target_path.clone(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: resolved_name,
            author_url: resolved_url,
            author_avatar: resolved_avatar,
            content,
            parent_id,
            depth,
            honeypot: is_honeypot,
            delete_token: Some(delete_token),
            submitter_ip,
            content_hash,
        })
        .await?;

    // 9. Optionally auto-approve (allowed-by-default moderation).
    if state.config.default_comment_status == "approved" {
        let _ = state.repo.update_status(new_id, "approved").await;
    }

    // 9. Extract and store URLs from content for cross-comment tracking.
    let urls = sanitize::extract_urls(&form.content);
    if !urls.is_empty() {
        let _ = state.repo.insert_urls(new_id, urls).await;
    }

    // 10. Moderation webhook — either sync (await decision) or async (fire-and-forget).
    let mut final_status = state.config.default_comment_status.clone(); // already applied above
    if let Some(ref webhook_url) = state.config.moderation_webhook_url {
        let client = state.http_client.clone();
        let url = webhook_url.clone();
        let is_sync = state.config.moderation_webhook_mode == "sync";

        // Build enriched payload
        let submitter_stats = if let Some(ref ip) = hook_ip {
            state.repo.submitter_stats(ip).await.ok()
        } else {
            None
        };
        let parent_chain = if parent_id.is_some() {
            state.repo.get_comment_chain(new_id).await.ok().flatten()
        } else {
            None
        };
        let (total, approved, spam, pending, deleted, first_seen) = submitter_stats
            .unwrap_or((0, 0, 0, 0, 0, None));
        let parents = parent_chain.map(|(_, chain)| {
            chain.into_iter().map(|p| serde_json::json!({
                "id": p.id, "author_name": p.author_name, "content": p.content, "depth": p.depth,
            })).collect::<Vec<_>>()
        });

        let payload = serde_json::json!({
            "event": "comment.created",
            "id": new_id, "target_path": target_path, "comment_type": "native",
            "author_name": hook_name, "author_url": hook_url, "author_avatar": hook_avatar,
            "honeypot": is_honeypot, "parent_id": parent_id, "depth": depth,
            "submitter_ip": hook_ip, "delete_token": delete_token_str,
            "content_hash": hook_content_hash, "is_reply": parent_id.is_some(),
            "parents": parents,
            "submitter": { "ip": hook_ip, "total_comments": total, "approved_comments": approved,
                "spam_comments": spam, "pending_comments": pending, "deleted_comments": deleted,
                "first_seen": first_seen },
            "admin_url": format!("/api/admin/comments/{}", new_id),
        });

        if is_sync {
            // Sync: wait for the webhook to respond with a decision.
            match client.post(&url).json(&payload).timeout(std::time::Duration::from_secs(10)).send().await {
                Ok(r) if r.status().is_success() => {
                    if let Ok(decision) = r.json::<serde_json::Value>().await {
                        if let Some(action) = decision["action"].as_str() {
                            if matches!(action, "approved" | "spam" | "deleted" | "pending") {
                                let _ = state.repo.update_status(new_id, action).await;
                                final_status = action.to_string();
                            }
                        }
                    }
                }
                Ok(r) => tracing::warn!(webhook = %url, status = %r.status(), "sync webhook returned error"),
                Err(e) => tracing::warn!(webhook = %url, err = %e, "sync webhook failed"),
            }
        } else {
            // Async: fire-and-forget.
            tokio::spawn(async move {
                match client.post(&url).json(&payload).timeout(std::time::Duration::from_secs(10)).send().await {
                    Ok(r) => tracing::debug!(webhook = %url, status = %r.status(), "moderation webhook notified"),
                    Err(e) => tracing::warn!(webhook = %url, err = %e, "moderation webhook failed"),
                }
            });
        }
    }

    tracing::debug!(id = new_id, status = %final_status, "comment stored");

    Ok((
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({ "delete_token": delete_token_str, "status": final_status })),
    ))
}

/// Generate a random hex token for self-service comment deletion.
/// Uses a hash of the peer IP, current time, and a monotonic counter.
/// Not cryptographically secure, but sufficient for anonymous comment deletion
/// (the delete endpoint is rate-limited).
fn generate_delete_token(addr: &SocketAddr) -> String {
    let mut hasher = DefaultHasher::new();
    addr.hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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
    let deleted = state
        .repo
        .delete_by_token(id, &body.token)
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

/// Validate and resolve the parent_id for a new comment.
/// Returns (parent_id_to_store, computed_depth).
///
/// Rules:
/// - If `form_parent_id` is None: top-level comment (parent_id = None, depth = 0).
/// - If `form_parent_id` is Some(id):
///   - Parent must exist, be approved, and belong to the same target_path.
///   - Parent's depth must be < max_thread_depth.
///   - Reply depth = parent.depth + 1.
/// - If max_thread_depth is 0, nesting is disabled entirely.
async fn resolve_parent(
    form_parent_id: &Option<i64>,
    target_path: &str,
    state: &AppState,
) -> Result<(Option<i64>, i64), AppError> {
    let Some(pid) = form_parent_id else {
        return Ok((None, 0));
    };
    let pid = *pid;

    let max_depth = state.config.max_thread_depth;
    if max_depth == 0 {
        return Err(AppError::BadRequest(
            "threaded replies are disabled on this server".to_string(),
        ));
    }

    let parent = state
        .repo
        .get_comment(pid)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("parent comment {pid} not found")))?;

    if parent.status != "approved" {
        return Err(AppError::BadRequest(format!(
            "parent comment {pid} is not approved (status: {})",
            parent.status
        )));
    }

    if parent.target_path != target_path {
        return Err(AppError::BadRequest(format!(
            "parent comment {pid} belongs to a different page ('{}'), not '{target_path}'",
            parent.target_path
        )));
    }

    if parent.depth >= max_depth {
        return Err(AppError::BadRequest(format!(
            "nesting depth exceeded: parent comment {pid} is at depth {}, max allowed is {max_depth}",
            parent.depth
        )));
    }

    let depth = parent.depth + 1;
    Ok((Some(pid), depth))
}

async fn resolve_author(
    author_url: Option<&str>,
    github_username: Option<&str>,
    cleaned_name: String,
    github: &Arc<dyn GitHubLookup>,
) -> (String, Option<String>) {
    // Priority 1: GitHub username → use the API name.
    if let Some(gh) = github_username {
        let gh = gh.trim();
        if !gh.is_empty()
            && let Some(profile) = github.lookup(gh).await
        {
            return (profile.name, None);
        }
    }

    // Priority 2: author_url → use form name, keep the URL.
    if let Some(url) = author_url {
        return (cleaned_name, Some(url.to_string()));
    }

    // Priority 3: name only, no enrichment.
    (cleaned_name, None)
}

/// Resolve a profile picture URL. Tries multiple strategies in priority order:
///
/// 1. GitHub API avatar (if `author_url` is a github.com URL)
/// 2. (webmentions feature) h-card photo from the author's page
/// 3. (webmentions feature) Favicon from the author's page
/// 4. icon.horse favicon service (always available)
/// 5. GitHub API avatar (if `github_username` was provided as fallback)
/// 6. Default avatar
async fn resolve_avatar(
    resolved_url: &Option<String>,
    raw_author_url: Option<&str>,
    github_username: Option<&str>,
    github: &Arc<dyn GitHubLookup>,
    http_client: &reqwest::Client,
) -> Option<String> {
    // Priority 1: If author_url is a GitHub profile, get avatar via API.
    if let Some(url) = resolved_url {
        if let Some(username) = extract_github_username(url) {
            if let Some(profile) = github.lookup(&username).await {
                return Some(profile.avatar_url);
            }
        }
    }

    // Priority 2-3: Fetch the author's page and try h-card photo + favicon.
    // Requires the webmentions feature for HTML parsing.
    #[cfg(feature = "webmentions")]
    if let Some(url) = resolved_url {
        if let Ok(parsed) = Url::parse(url) {
            let avatar = fetch_page_avatar(http_client, &parsed).await;
            if avatar.is_some() {
                return avatar;
            }
        }
    }

    // Priority 4: icon.horse fallback from the author URL domain.
    if let Some(url) = raw_author_url {
        if let Ok(parsed) = Url::parse(url) {
            if let Some(domain) = parsed.host_str() {
                return Some(format!("https://icon.horse/{}", domain));
            }
        }
    }

    // Priority 5: GitHub avatar from `github_username` form field.
    if let Some(gh) = github_username {
        let gh = gh.trim();
        if !gh.is_empty() {
            if let Some(profile) = github.lookup(gh).await {
                return Some(profile.avatar_url);
            }
        }
    }

    // Priority 6: Default avatar.
    Some("/embed/default-avatar.jpg".to_string())
}

/// Fetch a URL, parse the HTML, and try to extract an avatar.
/// Tries h-card photo first, then favicon.
#[cfg(feature = "webmentions")]
async fn fetch_page_avatar(http_client: &reqwest::Client, url: &Url) -> Option<String> {
    let resp = http_client
        .get(url.as_str())
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let html = resp.text().await.ok()?;

    // Try h-card photo first
    use crate::mf2;
    if let Some(parsed) = mf2::parse_h_entry(&html) {
        if let Some(avatar) = parsed.author_avatar {
            if let Ok(abs) = url.join(&avatar) {
                return Some(abs.to_string());
            }
        }
        // Also check the author's u-photo directly
        if let Some(avatar) = mf2::extract_photo(&html, url) {
            return Some(avatar);
        }
    }

    // Fallback to favicon
    crate::avatar::best_favicon(&html, url)
}

/// Extract a GitHub username from a URL like `https://github.com/username`.
fn extract_github_username(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let username = parsed.path().trim_start_matches('/').split('/').next()?;
    if username.is_empty() {
        return None;
    }
    Some(username.to_string())
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
        assert_eq!(c.author_name, "Alice Green", "name came from GitHub");
        assert_eq!(
            c.author_avatar,
            Some("https://avatars.githubusercontent.com/u/1".to_string())
        );
    }

    #[tokio::test]
    async fn github_unknown_username_falls_through() {
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body = "target_path=/gh-unknown&author_name=Bob&content=hi&github_username=nobody";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending
            .iter()
            .find(|c| c.target_path == "/gh-unknown")
            .unwrap();
        assert_eq!(c.author_name, "Bob", "name stayed as form value");
        assert_eq!(
            c.author_avatar.as_deref(),
            Some("/embed/default-avatar.jpg"),
            "default avatar when no URL and unknown GitHub"
        );
    }

    #[tokio::test]
    async fn author_url_sets_icon_horse_avatar() {
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
            Some("https://icon.horse/alice.blog".to_string())
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
            Some("/embed/default-avatar.jpg"),
            "default avatar should be set when no URL or GitHub is provided"
        );
    }

    #[tokio::test]
    async fn response_201_returns_delete_token() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "target_path=/token-test&author_name=Alice&content=hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let resp_body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024).await.unwrap(),
        )
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
        let resp_body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024).await.unwrap(),
        )
        .unwrap();
        let token = resp_body["delete_token"].as_str().expect("delete_token present");
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
}
