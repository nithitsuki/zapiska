//! Admin API: token/cookie auth, moderation, comment listing, author/URL
//! lookup, and bulk context. Every endpoint is gated by `admin_auth`
//! (Bearer header or `__Host-admin_token` cookie, constant-time comparison).
//!
//! Layout:
//! - `auth.rs` — login/logout + the `admin_auth` middleware.
//! - `comments.rs` — pending/list/single comment endpoints.
//! - `moderate.rs` — single + batch moderation.
//! - `lookup.rs` — author, path, URL, and bulk-context endpoints.

pub(crate) mod auth;
pub(crate) mod comments;
pub(crate) mod data;
pub(crate) mod lookup;
pub(crate) mod moderate;
pub(crate) mod profile;
pub(crate) mod reactions;
pub(crate) mod status;

pub(crate) use auth::{admin_auth, login, logout, request_has_admin_token};
pub(crate) use comments::{get_comment, list_comments, list_pending};
pub(crate) use data::{MAX_IMPORT_BODY_BYTES, export, import};
pub(crate) use lookup::{author_lookup, bulk_context, comment_urls, list_paths, url_lookup};
pub(crate) use moderate::{moderate, moderate_batch};
pub(crate) use profile::{create_owner_comment, get_profile, set_profile};
pub(crate) use reactions::{list_reactions, moderate_reaction, moderate_reactions_batch};
pub(crate) use status::status;

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
                    .header(header::COOKIE, "__Host-admin_token=test")
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
                    .header(header::COOKIE, "__Host-admin_token=wrong")
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
            cookie.contains("__Host-admin_token=test"),
            "cookie has correct __Host- name and value: {cookie}"
        );
        assert!(cookie.contains("HttpOnly"), "cookie is HttpOnly");
        assert!(cookie.contains("Secure"), "cookie is Secure");
        assert!(
            cookie.contains("SameSite=Lax"),
            "cookie keeps SameSite=Lax: {cookie}"
        );
        assert!(cookie.contains("Path=/"), "cookie keeps Path=/: {cookie}");
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

    // ── Export / import (data backup & restore) ─────────────

    /// Seed a comment directly through the repo with a given status.
    async fn seed_status(
        state: &crate::state::AppState,
        path: &str,
        author: &str,
        status: &str,
    ) -> i64 {
        let id = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: path.to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: author.to_string(),
                author_url: None,
                author_avatar: None,
                content: format!("comment by {author}"),
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
        if status != "pending" {
            state.repo.update_status(id, status).await.unwrap();
        }
        id
    }

    #[tokio::test]
    async fn export_requires_auth() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(unauthorized_request(
                axum::http::Method::GET,
                "/api/admin/export",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn import_requires_auth() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(unauthorized_request(
                axum::http::Method::POST,
                "/api/admin/import",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn export_includes_all_statuses_and_threading() {
        let (state, _dir) = helpers::test_state();
        let parent = seed_status(&state, "/export", "Parent", "approved").await;
        let child = state
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/export".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Child".to_string(),
                author_url: None,
                author_avatar: None,
                content: "reply".to_string(),
                parent_id: Some(parent),
                depth: 1,
                honeypot: false,
                delete_token: None,
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap();
        seed_status(&state, "/export", "Spammy", "spam").await;
        seed_status(&state, "/export", "Deleted", "deleted").await;

        let app = build_app(state);
        let resp = app
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/export",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["version"], 1);
        assert!(body["exported_at"].as_str().unwrap().ends_with('Z'));
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 4, "all statuses exported");
        let statuses: Vec<&str> = comments
            .iter()
            .map(|c| c["status"].as_str().unwrap())
            .collect();
        assert!(statuses.contains(&"approved"));
        assert!(statuses.contains(&"spam"));
        assert!(statuses.contains(&"deleted"));
        let child_row = comments
            .iter()
            .find(|c| c["id"].as_i64() == Some(child))
            .unwrap();
        assert_eq!(
            child_row["parent_id"].as_i64(),
            Some(parent),
            "threading preserved"
        );
        assert_eq!(child_row["depth"].as_i64(), Some(1));
    }

    #[tokio::test]
    async fn import_restores_full_backup_into_fresh_db() {
        // Source: seeded state with an approved comment (incl. a reply).
        let (state_a, _dir_a) = helpers::test_state();
        let parent = seed_status(&state_a, "/migrate", "Alice", "approved").await;
        state_a
            .repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: "/migrate".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Bob".to_string(),
                author_url: None,
                author_avatar: None,
                content: "reply".to_string(),
                parent_id: Some(parent),
                depth: 1,
                honeypot: false,
                delete_token: None,
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap();
        let app_a = build_app(state_a.clone());
        let resp = app_a
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/export",
            ))
            .await
            .unwrap();
        let export = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();

        // Target: a fresh, empty DB.
        let (state_b, _dir_b) = helpers::test_state();
        assert_eq!(
            state_b.repo.list_all_comments().await.unwrap().len(),
            0,
            "target starts empty"
        );
        let app_b = build_app(state_b.clone());
        let resp = app_b
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                core::str::from_utf8(&export).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "import accepted");
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["comments_imported"], 2);

        // Full fidelity: ids, threading, and status survive.
        let restored = state_b.repo.list_all_comments().await.unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].id, parent, "id preserved");
        assert_eq!(restored[0].status, "approved");
        assert_eq!(restored[1].parent_id, Some(parent), "threading restored");
    }

    #[tokio::test]
    async fn import_is_idempotent() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state.clone());
        let export_body = serde_json::json!({
            "version": 1,
            "comments": [{
                "id": 7,
                "target_path": "/idem",
                "comment_type": "native",
                "source_url": null,
                "author_name": "Alice",
                "author_url": null,
                "author_avatar": null,
                "content": "<p>hi</p>",
                "status": "approved",
                "created_at": "2026-08-01 10:00:00",
                "updated_at": "2026-08-01 10:00:00",
                "parent_id": null,
                "depth": 0,
                "honeypot": false,
                "delete_token": null,
                "submitter_ip": null,
                "submitter_ip_hash": null,
                "content_hash": null
            }]
        })
        .to_string();

        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(json_request(
                    axum::http::Method::POST,
                    "/api/admin/import",
                    &export_body,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }
        assert_eq!(
            state.repo.list_all_comments().await.unwrap().len(),
            1,
            "re-import must not duplicate rows"
        );
    }

    #[tokio::test]
    async fn import_rejects_unknown_version() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                r#"{"version": 99, "comments": []}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn import_resanitizes_malicious_content() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state.clone());
        let export_body = serde_json::json!({
            "version": 1,
            "comments": [{
                "id": 1,
                "target_path": "/evil-import",
                "comment_type": "native",
                "source_url": null,
                "author_name": "Hacker",
                "author_url": "javascript:alert(1)",
                "author_avatar": null,
                "content": "<script>alert(1)</script><p>ok</p>",
                "status": "approved",
                "created_at": "2026-08-01 10:00:00",
                "updated_at": "2026-08-01 10:00:00",
                "parent_id": null,
                "depth": 0,
                "honeypot": false,
                "delete_token": null,
                "submitter_ip": null,
                "submitter_ip_hash": null,
                "content_hash": null
            }]
        })
        .to_string();
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &export_body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // The comment as a whole is rejected: author_url with a non-http(s)
        // scheme fails the same validation native submissions get.
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["comments_imported"], 0);
        assert_eq!(
            result["comments_skipped"], 1,
            "bad author_url rejects the comment"
        );
        assert!(state.repo.get_comment(1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn import_sanitizes_content_but_keeps_valid_urls() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state.clone());
        let export_body = serde_json::json!({
            "version": 1,
            "comments": [{
                "id": 2,
                "target_path": "/sanitize-import",
                "comment_type": "native",
                "source_url": null,
                "author_name": "Alice",
                "author_url": "https://alice.blog",
                "author_avatar": null,
                "content": "<script>alert(1)</script><p>ok</p>",
                "status": "approved",
                "created_at": "2026-08-01 10:00:00",
                "updated_at": "2026-08-01 10:00:00",
                "parent_id": null,
                "depth": 0,
                "honeypot": false,
                "delete_token": null,
                "submitter_ip": null,
                "submitter_ip_hash": null,
                "content_hash": null
            }]
        })
        .to_string();
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &export_body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let stored = state.repo.get_comment(2).await.unwrap().unwrap();
        assert!(
            !stored.content.contains("<script>"),
            "content re-sanitized on import"
        );
        assert!(stored.content.contains("<p>ok</p>"));
        assert_eq!(stored.author_url.as_deref(), Some("https://alice.blog"));
        assert_eq!(stored.status, "approved");
    }

    #[tokio::test]
    async fn import_accepts_large_payloads_beyond_form_limit() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state.clone());
        // 50 comments x ~200 bytes ≈ 10 KB — well over the 8 KB form body
        // limit that public routes enforce.
        let mut comments = Vec::new();
        for i in 0..50 {
            comments.push(serde_json::json!({
                "id": i + 1,
                "target_path": "/bulk",
                "comment_type": "native",
                "source_url": null,
                "author_name": format!("User{i}"),
                "author_url": null,
                "author_avatar": null,
                "content": format!("<p>bulk comment {i} with some padding</p>"),
                "status": "pending",
                "created_at": "2026-08-01 10:00:00",
                "updated_at": "2026-08-01 10:00:00",
                "parent_id": null,
                "depth": 0,
                "honeypot": false,
                "delete_token": null,
                "submitter_ip": null,
                "submitter_ip_hash": null,
                "content_hash": null
            }));
        }
        let export_body = serde_json::json!({ "version": 1, "comments": comments }).to_string();
        assert!(
            export_body.len() > 8192,
            "payload must exceed the form limit"
        );

        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &export_body,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "large import must not hit the 8 KB form cap"
        );
        assert_eq!(state.repo.list_all_comments().await.unwrap().len(), 50);
    }

    #[tokio::test]
    async fn import_skips_orphaned_rows_without_aborting() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state.clone());
        // Child references a parent that fails validation (bad status) and
        // gets skipped; a URL row references a comment that was never in the
        // import at all. The import must complete, not 500.
        let export_body = serde_json::json!({
            "version": 1,
            "comments": [
                {
                    "id": 1, "target_path": "/orphan", "comment_type": "native",
                    "source_url": null, "author_name": "Bad", "author_url": null,
                    "author_avatar": null, "content": "x", "status": "evil",
                    "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00",
                    "parent_id": null, "depth": 0, "honeypot": false,
                    "delete_token": null, "submitter_ip": null,
                    "submitter_ip_hash": null, "content_hash": null
                },
                {
                    "id": 2, "target_path": "/orphan", "comment_type": "native",
                    "source_url": null, "author_name": "Child", "author_url": null,
                    "author_avatar": null, "content": "reply to skipped parent",
                    "status": "pending", "created_at": "2026-08-01 10:00:00",
                    "updated_at": "2026-08-01 10:00:00", "parent_id": 1,
                    "depth": 1, "honeypot": false, "delete_token": null,
                    "submitter_ip": null, "submitter_ip_hash": null, "content_hash": null
                }
            ],
            "comment_urls": [
                { "id": 1, "comment_id": 999, "url": "https://evil.com/x",
                  "domain": "evil.com", "url_hash": "h:deadbeef" }
            ]
        })
        .to_string();
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &export_body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "import must complete despite bad rows");
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["comments_imported"], 0);
        assert_eq!(
            result["comments_skipped"], 2,
            "parent + orphaned child skipped"
        );
        assert_eq!(
            result["comment_urls_imported"], 0,
            "URL for missing comment skipped"
        );
        assert!(state.repo.get_comment(1).await.unwrap().is_none());
        assert!(state.repo.get_comment(2).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn export_records_ip_salt_flag() {
        // Default test state has no IP_HASH_SECRET.
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/export",
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["ip_hash_salted"], false);

        // With a secret configured, the flag flips — the secret itself must
        // never appear in the document.
        let (mut salted_state, _dir) = helpers::test_state();
        salted_state.config.ip_hash_secret = Some("s3cr3t".to_string());
        let app = build_app(salted_state);
        let resp = app
            .oneshot(authorized_request(
                axum::http::Method::GET,
                "/api/admin/export",
            ))
            .await
            .unwrap();
        let raw = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(body["ip_hash_salted"], true);
        assert!(
            !String::from_utf8_lossy(&raw).contains("s3cr3t"),
            "secret must never leak into the export"
        );
    }

    fn salted_comment_json(id: i64, ip: &str, hash: &str, salted_flag: Option<bool>) -> String {
        let flag = match salted_flag {
            Some(true) => "\"ip_hash_salted\": true,",
            Some(false) => "\"ip_hash_salted\": false,",
            None => "",
        };
        format!(
            r#"{{"version": 1, {flag} "comments": [{{
                "id": {id}, "target_path": "/salt", "comment_type": "native",
                "source_url": null, "author_name": "Ada", "author_url": null,
                "author_avatar": null, "content": "hi", "status": "approved",
                "created_at": "2026-08-01 10:00:00", "updated_at": "2026-08-01 10:00:00",
                "parent_id": null, "depth": 0, "honeypot": false,
                "delete_token": null, "submitter_ip": "{ip}",
                "submitter_ip_hash": "{hash}", "content_hash": null
            }}]}}"#
        )
    }

    #[tokio::test]
    async fn import_recomputes_stale_hash_and_warns_on_salt_mismatch() {
        // Export claims a salted server, but this server has no secret and the
        // shipped hash is stale. The raw IP heals the row; the response warns.
        let (state, _dir) = helpers::test_state();
        assert!(state.config.ip_hash_secret.is_none());
        let app = build_app(state.clone());
        let body = salted_comment_json(11, "9.9.9.9", "h:stale", Some(true));
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["comments_imported"], 1);
        assert_eq!(result["ip_hashes_recomputed"], 1);
        assert!(
            result["warning"].as_str().unwrap().contains("mismatch"),
            "salt mismatch must warn: {result}"
        );

        let stored = state.repo.get_comment(11).await.unwrap().unwrap();
        let expected =
            crate::ip_hash::hash_ip(&"9.9.9.9".parse::<std::net::IpAddr>().unwrap(), None);
        assert_eq!(stored.submitter_ip_hash.as_deref(), Some(expected.as_str()));
        assert_eq!(stored.submitter_ip.as_deref(), Some("9.9.9.9"));
    }

    #[tokio::test]
    async fn import_matching_salt_has_no_warning() {
        // Flag and server agree (both unsalted) with a consistent hash.
        let (state, _dir) = helpers::test_state();
        let fresh = crate::ip_hash::hash_ip(&"9.9.9.9".parse::<std::net::IpAddr>().unwrap(), None);
        let app = build_app(state.clone());
        let body = salted_comment_json(12, "9.9.9.9", &fresh, Some(false));
        let resp = app
            .oneshot(json_request(
                axum::http::Method::POST,
                "/api/admin/import",
                &body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["comments_imported"], 1);
        assert_eq!(result["ip_hashes_recomputed"], 0);
        assert!(result["warning"].is_null(), "no warning: {result}");
    }

    #[tokio::test]
    async fn import_pre_flag_export_without_hashes_has_no_warning() {
        // Old exports have no flag at all; with no salted identities there is
        // nothing to warn about.
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let plain = serde_json::json!({
            "version": 1,
            "comments": [{
                "id": 13, "target_path": "/salt", "comment_type": "native",
                "source_url": null, "author_name": "Ada", "author_url": null,
                "author_avatar": null, "content": "hi", "status": "approved",
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
                &plain,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(result["warning"].is_null(), "no warning: {result}");
    }
}
