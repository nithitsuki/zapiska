use axum::Json;
use axum::extract::{Query, State};
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::error::AppError;
use crate::state::AppState;

// ── Auth middleware ──────────────────────────────────────────

/// Axum middleware that guards `/api/admin/*` routes.
/// Compares the `Authorization: Bearer <token>` header against
/// the configured admin token in constant time.
pub async fn admin_auth(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Even a missing header goes through the same code path.
    let token = header.strip_prefix("Bearer ").unwrap_or_default();

    let expected = state.config.admin_token.as_bytes();
    let actual = token.as_bytes();

    // Pad both to the same length to avoid leaking the expected length.
    let max_len = expected.len().max(actual.len());
    let mut expected_padded = vec![0u8; max_len];
    let mut actual_padded = vec![0u8; max_len];
    expected_padded[..expected.len()].copy_from_slice(expected);
    actual_padded[..actual.len()].copy_from_slice(actual);

    if expected_padded.ct_eq(&actual_padded).unwrap_u8() == 1 {
        Ok(next.run(req).await)
    } else {
        Err(AppError::Unauthorized)
    }
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
            created_at: c.created_at,
        })
        .collect();

    Ok(Json(PendingResponse { comments }))
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
    if body.action != "approved" && body.action != "spam" && body.action != "deleted" {
        return Err(AppError::BadRequest(format!(
            "invalid action '{}', must be one of: approved, spam, deleted",
            body.action
        )));
    }

    // Check the comment exists first (for a better error message).
    let _comment = state
        .repo
        .get_comment(body.id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("comment {} not found", body.id)))?;

    state.repo.update_status(body.id, &body.action).await?;

    Ok(Json(ModerateResponse {
        id: body.id,
        status: body.action,
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
        // Override the empty body + Content-Length: 0 set by helpers::request.
        *req.body_mut() = Body::from(body.to_owned());
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        req.headers_mut()
            .insert(header::CONTENT_LENGTH, body.len().into());
        req.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test"),
        );
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
        // Insert a pending comment.
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
            })
            .await
            .unwrap();

        let app = build_app(state.clone());

        // Mark as spam
        let body = format!(r#"{{"id":{},"action":"spam"}}"#, id);
        let req = json_request(axum::http::Method::POST, "/api/admin/moderate", &body);
        app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            state.repo.get_comment(id).await.unwrap().unwrap().status,
            "spam"
        );

        // Then delete
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

        // Wrong token — response body should not echo the token.
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
}
