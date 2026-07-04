use axum::Form;
use axum::extract::State;
use serde::Deserialize;
use std::sync::Arc;
use url::Url;

use crate::db::repo::NewComment;
use crate::error::AppError;
use crate::github::GitHubLookup;
use crate::sanitize;
use crate::state::AppState;
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
    Form(form): Form<CommentForm>,
) -> Result<(axum::http::StatusCode, ()), AppError> {
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

    // 2. Sanitize content
    let content = sanitize::sanitize_html(&form.content, state.config.max_content_len);

    // 3. Resolve author info
    let (resolved_name, resolved_url, resolved_avatar) =
        resolve_author(author_url.as_deref(), github_username.as_deref(), author_name, &state.github).await;

    // 4. Store
    state
        .repo
        .insert_comment(NewComment {
            target_path: target_path.clone(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: resolved_name,
            author_url: resolved_url,
            author_avatar: resolved_avatar,
            content,
        })
        .await?;

    Ok((axum::http::StatusCode::CREATED, ()))
}

async fn resolve_author(
    author_url: Option<&str>,
    github_username: Option<&str>,
    cleaned_name: String,
    github: &Arc<dyn GitHubLookup>,
) -> (String, Option<String>, Option<String>) {
    // Priority 1: GitHub username.
    if let Some(gh) = github_username {
        let gh = gh.trim();
        if !gh.is_empty()
            && let Some(profile) = github.lookup(gh).await
        {
            return (profile.name, None, Some(profile.avatar_url));
        }
    }

    // Priority 2: author_url → icon.horse favicon.
    if let Some(url) = author_url {
        let parsed = Url::parse(url).ok();
        let domain = parsed.and_then(|u| u.host_str().map(|h| h.to_string()));
        let avatar = domain.map(|d| format!("https://icon.horse/{}", d));
        return (cleaned_name, Some(url.to_string()), avatar);
    }

    // Priority 3: name only, no enrichment.
    (cleaned_name, None, None)
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
        assert!(c.author_avatar.is_none());
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
        assert!(c.author_avatar.is_none());
    }

    #[tokio::test]
    async fn response_body_empty_on_201() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "target_path=/empty-body&author_name=Alice&content=hi";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        let resp_body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(resp_body.is_empty(), "201 response must have empty body");
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
        // Submit payload with script + control char; verify 201 body empty
        // and stored content sanitized.
        let (state, _dir) = test_state();
        let app = build_app(state.clone());
        let body =
            "target_path=/leak&author_name=Bad%00Guy&content=<script>alert(1)</script><p>text</p>";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 201);

        // Response body is empty (no echo).
        let resp_body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(resp_body.is_empty(), "no reflection of input in response");

        // Content was sanitized in DB.
        let pending = state.repo.list_pending(10, None, None).await.unwrap();
        let c = pending.iter().find(|c| c.target_path == "/leak").unwrap();
        assert!(!c.content.contains("<script>"), "script content sanitized");
        assert!(c.content.contains("<p>text</p>"), "safe text preserved");
        // author_name had a control char stripped
        assert_eq!(c.author_name, "BadGuy", "control char stripped from name");
    }
}
