//! Admin API: token/cookie auth, moderation, comment listing, author/URL
//! lookup, and bulk context. Every endpoint is gated by `admin_auth`
//! (Bearer header or `admin_token` cookie, constant-time comparison).
//!
//! Layout:
//! - `auth.rs` — login/logout + the `admin_auth` middleware.
//! - `comments.rs` — pending/list/single comment endpoints.
//! - `moderate.rs` — single + batch moderation.
//! - `lookup.rs` — author, path, URL, and bulk-context endpoints.

pub(crate) mod auth;
pub(crate) mod comments;
pub(crate) mod lookup;
pub(crate) mod moderate;

pub(crate) use auth::{admin_auth, login, logout};
pub(crate) use comments::{get_comment, list_comments, list_pending};
pub(crate) use lookup::{author_lookup, bulk_context, comment_urls, list_paths, url_lookup};
pub(crate) use moderate::{moderate, moderate_batch};

/// Constant-time token comparison, length-independent (both sides padded).
fn validate_token(actual: &[u8], expected: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    let max_len = expected.len().max(actual.len());
    let mut expected_padded = vec![0u8; max_len];
    let mut actual_padded = vec![0u8; max_len];
    expected_padded[..expected.len()].copy_from_slice(expected);
    actual_padded[..actual.len()].copy_from_slice(actual);
    expected_padded.ct_eq(&actual_padded).unwrap_u8() == 1
}

#[cfg(test)]
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
                submitter_ip_hash: None,
                content_hash: None,
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
                submitter_ip_hash: None,
                content_hash: None,
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
                submitter_ip_hash: None,
                content_hash: None,
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
                submitter_ip_hash: None,
                content_hash: None,
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
                submitter_ip_hash: None,
                content_hash: None,
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
                submitter_ip_hash: None,
                content_hash: None,
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
        assert!(
            cookie.contains("admin_token=test"),
            "cookie has correct value"
        );
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

        let req = json_request(axum::http::Method::POST, "/api/admin/logout", r#"{}"#);
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
                submitter_ip_hash: None,
                content_hash: None,
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
                submitter_ip_hash: None,
                content_hash: None,
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
