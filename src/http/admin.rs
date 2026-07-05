use axum::Json;
use axum::extract::{Query, State};
use axum::http::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::error::AppError;
use crate::state::AppState;

// ── Helpers ─────────────────────────────────────────────────

fn validate_token(actual: &[u8], expected: &[u8]) -> bool {
    let max_len = expected.len().max(actual.len());
    let mut expected_padded = vec![0u8; max_len];
    let mut actual_padded = vec![0u8; max_len];
    expected_padded[..expected.len()].copy_from_slice(expected);
    actual_padded[..actual.len()].copy_from_slice(actual);
    expected_padded.ct_eq(&actual_padded).unwrap_u8() == 1
}

fn extract_cookie<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    for pair in cookie_header.split(';') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next()?.trim();
        let val = parts.next()?;
        if key.eq_ignore_ascii_case(name) {
            return Some(val.trim());
        }
    }
    None
}

fn set_cookie_value(token: &str, max_age_secs: i64) -> String {
    format!(
        "admin_token={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        token, max_age_secs
    )
}

// ── Auth middleware ──────────────────────────────────────────

pub async fn admin_auth(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    let expected = state.config.admin_token.as_bytes();

    let token = {
        let header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let t = header.strip_prefix("Bearer ").unwrap_or("");
        if !t.is_empty() && validate_token(t.as_bytes(), expected) {
            return Ok(next.run(req).await);
        }

        let cookie_header = req
            .headers()
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        extract_cookie(cookie_header, "admin_token")
    };

    match token {
        Some(t) if validate_token(t.as_bytes(), expected) => Ok(next.run(req).await),
        _ => Err(AppError::Unauthorized),
    }
}

// ── POST /api/admin/login ───────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    pub token: String,
}

pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, AppError> {
    let expected = state.config.admin_token.as_bytes();
    let actual = body.token.as_bytes();

    if !validate_token(actual, expected) {
        return Err(AppError::Unauthorized);
    }

    let cookie = set_cookie_value(&body.token, 2592000);

    let mut resp = Json(serde_json::json!({"success": true})).into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, cookie.parse().unwrap());
    Ok(resp)
}

// ── POST /api/admin/logout ──────────────────────────────────

pub async fn logout() -> Response {
    let cookie = set_cookie_value("", 0);
    let mut resp = Json(serde_json::json!({"success": true})).into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

// ── GET /api/admin/pending ──────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
pub struct PendingQuery {
    #[param(maximum = 100, default = 50)]
    pub limit: Option<i64>,
    pub before: Option<i64>,
    #[param(example = "/blog/hello")]
    pub path: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct PendingResponse {
    pub comments: Vec<PendingComment>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct PendingComment {
    pub id: i64,
    pub target_path: String,
    #[schema(example = "native")]
    pub comment_type: String,
    pub source_url: Option<String>,
    pub author_name: String,
    pub author_url: Option<String>,
    pub author_avatar: Option<String>,
    pub content: String,
    #[schema(example = "pending")]
    pub status: String,
    pub created_at: String,
    /// ID of the parent comment, if a reply. Null for top-level.
    pub parent_id: Option<i64>,
    /// Nesting depth (0 = top-level).
    pub depth: i64,
    /// True if this comment was caught by the honeypot anti-spam field.
    pub honeypot: bool,
    /// Self-deletion token (if one was generated for this comment).
    pub delete_token: Option<String>,
    /// Submitter IP address (only available when STORE_IP_ADDRESS is enabled).
    pub submitter_ip: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/pending",
    params(PendingQuery),
    responses(
        (status = 200, description = "List of pending comments", body = PendingResponse),
        (status = 401, description = "Unauthorized (missing or invalid admin token)"),
    ),
    tag = "admin",
)]
pub async fn list_pending(
    State(state): State<AppState>,
    Query(query): Query<PendingQuery>,
) -> Result<Json<PendingResponse>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let before = query.before;
    let path = query.path.as_deref();

    let comments = state.repo.list_pending(limit, before, path).await?;

    let comments: Vec<PendingComment> = comments
        .into_iter()
        .map(|c| PendingComment {
            id: c.id,
            target_path: c.target_path,
            comment_type: c.comment_type,
            source_url: c.source_url,
            author_name: c.author_name,
            author_url: c.author_url,
            author_avatar: c.author_avatar,
            content: c.content,
            status: c.status,
            parent_id: c.parent_id,
            depth: c.depth,
            honeypot: c.honeypot,
            delete_token: c.delete_token,
            submitter_ip: c.submitter_ip,
            created_at: c.created_at,
        })
        .collect();

    Ok(Json(PendingResponse { comments }))
}

// ── GET /api/admin/comments ─────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
pub struct AdminCommentsQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub before: Option<i64>,
    pub path: Option<String>,
    /// Filter by submitter IP address (requires STORE_IP_ADDRESS=true).
    pub ip: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/comments",
    params(AdminCommentsQuery),
    responses(
        (status = 200, description = "List of comments with optional status filter", body = PendingResponse),
        (status = 401, description = "Unauthorized"),
    ),
    tag = "admin",
)]
pub async fn list_comments(
    State(state): State<AppState>,
    Query(query): Query<AdminCommentsQuery>,
) -> Result<Json<PendingResponse>, AppError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let status = query.status.as_deref();
    let before = query.before;
    let path = query.path.as_deref();
    let ip = query.ip.as_deref();

    let comments = state
        .repo
        .list_comments(status, limit, before, path, ip)
        .await?;

    let comments: Vec<PendingComment> = comments
        .into_iter()
        .map(|c| PendingComment {
            id: c.id,
            target_path: c.target_path,
            comment_type: c.comment_type,
            source_url: c.source_url,
            author_name: c.author_name,
            author_url: c.author_url,
            author_avatar: c.author_avatar,
            content: c.content,
            status: c.status,
            parent_id: c.parent_id,
            depth: c.depth,
            honeypot: c.honeypot,
            delete_token: c.delete_token,
            submitter_ip: c.submitter_ip,
            created_at: c.created_at,
        })
        .collect();

    Ok(Json(PendingResponse { comments }))
}

// ── GET /api/admin/comments/:id ─────────────────────────────

#[derive(Serialize)]
pub struct CommentDetail {
    pub comment: PendingComment,
    /// Ancestor chain from immediate parent up to the root comment.
    /// Empty for top-level comments.
    pub parents: Vec<PendingComment>,
}

pub async fn get_comment(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<CommentDetail>, AppError> {
    let (comment, chain) = state
        .repo
        .get_comment_chain(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("comment {id} not found")))?;

    let map = |c: crate::db::repo::Comment| PendingComment {
        id: c.id,
        target_path: c.target_path,
        comment_type: c.comment_type,
        source_url: c.source_url,
        author_name: c.author_name,
        author_url: c.author_url,
        author_avatar: c.author_avatar,
        content: c.content,
        status: c.status,
        parent_id: c.parent_id,
        depth: c.depth,
        honeypot: c.honeypot,
        delete_token: c.delete_token,
        submitter_ip: c.submitter_ip,
        created_at: c.created_at,
    };

    Ok(Json(CommentDetail {
        parents: chain.into_iter().map(map).collect(),
        comment: map(comment),
    }))
}

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
            Ok(status) => ModerateResult { id: action.id, status, error: None },
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
async fn moderate_single(
    state: &AppState,
    id: i64,
    action: &str,
) -> Result<String, AppError> {
    if action != "approved" && action != "spam" && action != "deleted" && action != "pending" {
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
    let status = moderate_single(&state, body.id, &body.action).await?;
    Ok(Json(ModerateResponse {
        id: body.id,
        status,
    }))
}

#[cfg(test)]
mod tests {

    use axum::body::Body;
    use axum::http::{HeaderValue, Request, header};

    use tower::ServiceExt;

    use crate::http::build_app;
    use crate::http::test_support::helpers;

    fn authorized_request(method: axum::http::Method, uri: &str) -> Request<Body> {
        let mut req = helpers::request(method, uri);
        req.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test"),
        );
        req
    }

    fn json_request(method: axum::http::Method, uri: &str, body: &str) -> Request<Body> {
        let mut req = helpers::request(method, uri);
        *req.body_mut() = Body::from(body.to_owned());
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        req.headers_mut()
            .insert(header::CONTENT_LENGTH, body.len().into());
        req.headers_mut()
            .insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer test"));
        req
    }

    fn unauthorized_request(method: axum::http::Method, uri: &str) -> Request<Body> {
        helpers::request(method, uri)
    }

    #[tokio::test]
    async fn missing_auth_returns_401() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(unauthorized_request(
                axum::http::Method::GET,
                "/api/admin/pending",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let mut req = helpers::request(axum::http::Method::GET, "/api/admin/pending");
        req.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn correct_token_allows_pending_list() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/pending",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn pending_list_shows_only_pending() {
        let (state, _dir) = helpers::test_state();
        state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/admin-test".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "AdminPending".to_string(),
                author_url: None,
                author_avatar: None,
                content: "moderate me".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();

        let app = build_app(state.clone());
        let resp = app
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/pending",
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
        let comments = body["comments"].as_array().unwrap();
        assert!(comments.iter().all(|c| c["status"] == "pending"));
    }

    #[tokio::test]
    async fn pending_list_filters_by_path() {
        let (state, _dir) = helpers::test_state();
        state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/path-a".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "A".to_string(),
                author_url: None,
                author_avatar: None,
                content: "a".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();
        state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/path-b".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "B".to_string(),
                author_url: None,
                author_avatar: None,
                content: "b".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();

        let app = build_app(state.clone());
        let resp = app
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/pending?path=/path-a",
            ))
            .await
            .unwrap();

        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["target_path"], "/path-a");
    }

    #[tokio::test]
    async fn moderate_approve_works() {
        let (state, _dir) = helpers::test_state();
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/approve-me".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Approver".to_string(),
                author_url: None,
                author_avatar: None,
                content: "approve".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();

        let app = build_app(state.clone());
        let body = format!(r#"{{"id":{},"action":"approved"}}"#, id);
        let req = json_request(axum::http::Method::POST, "/api/admin/moderate", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let comment = state.repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(comment.status, "approved");
    }

    #[tokio::test]
    async fn moderate_spam_and_deleted() {
        let (state, _dir) = helpers::test_state();
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/mod-all".to_string(),
                comment_type: "webmention".to_string(),
                source_url: Some("https://src.example".to_string()),
                author_name: "Spammy".to_string(),
                author_url: None,
                author_avatar: None,
                content: "buy now".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();

        let app = build_app(state.clone());

        let body = format!(r#"{{"id":{},"action":"spam"}}"#, id);
        let req = json_request(axum::http::Method::POST, "/api/admin/moderate", &body);
        app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            state.repo.get_comment(id).await.unwrap().unwrap().status,
            "spam"
        );

        let body = format!(r#"{{"id":{},"action":"deleted"}}"#, id);
        let req = json_request(axum::http::Method::POST, "/api/admin/moderate", &body);
        app.oneshot(req).await.unwrap();
        assert_eq!(
            state.repo.get_comment(id).await.unwrap().unwrap().status,
            "deleted"
        );
    }

    #[tokio::test]
    async fn moderate_invalid_action_returns_400() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let req = json_request(
            axum::http::Method::POST,
            "/api/admin/moderate",
            r#"{"id":1,"action":"publish"}"#,
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn moderate_unknown_id_returns_404() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let req = json_request(
            axum::http::Method::POST,
            "/api/admin/moderate",
            r#"{"id":9999,"action":"approved"}"#,
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn moderate_already_approved_still_allows_transition() {
        let (state, _dir) = helpers::test_state();
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/already".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "X".to_string(),
                author_url: None,
                author_avatar: None,
                content: "x".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();
        state.repo.update_status(id, "approved").await.unwrap();

        let app = build_app(state.clone());
        let body = format!(r#"{{"id":{},"action":"spam"}}"#, id);
        let req = json_request(axum::http::Method::POST, "/api/admin/moderate", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.repo.get_comment(id).await.unwrap().unwrap().status,
            "spam"
        );
    }

    #[tokio::test]
    async fn admin_token_not_in_response_body() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);

        let mut req = helpers::request(axum::http::Method::GET, "/api/admin/pending");
        req.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            !body_str.contains("test"),
            "token must not appear in response body"
        );
        assert!(!body_str.contains("Bearer"), "header format not echoed");
    }

    #[tokio::test]
    async fn cookie_auth_works() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::GET)
                    .uri("/api/admin/pending")
                    .header(header::COOKIE, "admin_token=test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn cookie_auth_wrong_token_returns_401() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::GET)
                    .uri("/api/admin/pending")
                    .header(header::COOKIE, "admin_token=wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn login_sets_cookie() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);

        let req = json_request(
            axum::http::Method::POST,
            "/api/admin/login",
            r#"{"token":"test"}"#,
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let set_cookie = resp.headers().get(header::SET_COOKIE);
        assert!(set_cookie.is_some(), "login must set Set-Cookie header");
        let cookie = set_cookie.unwrap().to_str().unwrap();
        assert!(cookie.contains("admin_token=test"), "cookie has correct value");
        assert!(cookie.contains("HttpOnly"), "cookie is HttpOnly");
        assert!(cookie.contains("Max-Age="), "cookie has max age");
    }

    #[tokio::test]
    async fn login_wrong_token_returns_401() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);

        let req = json_request(
            axum::http::Method::POST,
            "/api/admin/login",
            r#"{"token":"wrong"}"#,
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn logout_clears_cookie() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);

        let req = json_request(
            axum::http::Method::POST,
            "/api/admin/logout",
            r#"{}"#,
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let set_cookie = resp.headers().get(header::SET_COOKIE);
        assert!(set_cookie.is_some());
        let cookie = set_cookie.unwrap().to_str().unwrap();
        assert!(cookie.contains("Max-Age=0"), "logout clears cookie");
    }

    #[tokio::test]
    async fn list_comments_filters_by_status() {
        let (state, _dir) = helpers::test_state();

        state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/filters".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "A".to_string(),
                author_url: None,
                author_avatar: None,
                content: "approved one".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();
        let id2 = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/filters".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "B".to_string(),
                author_url: None,
                author_avatar: None,
                content: "spam one".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            })
            .await
            .unwrap();
        state.repo.update_status(id2, "spam").await.unwrap();

        let app = build_app(state.clone());

        let resp = app
            .clone()
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/comments?status=spam",
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["author_name"], "B");

        let resp = app
            .clone()
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/comments?status=all",
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 2);
    }
}
