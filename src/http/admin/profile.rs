//! Verified owner profile endpoints: view and edit the site owner's public
//! identity, and author a verified comment (rendered with a checkmark).

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::db::repo::AdminProfile;
use crate::error::AppError;
use crate::state::AppState;
use crate::validate;

#[derive(Deserialize)]
pub struct AdminProfileInput {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub github_username: String,
    #[serde(default)]
    pub website_url: String,
    #[serde(default)]
    pub avatar_url: String,
}

#[derive(Serialize)]
pub struct AdminProfileResponse {
    pub display_name: String,
    pub github_username: String,
    pub website_url: String,
    pub avatar_url: String,
    /// Derived flags for display: whether the profile is set at all.
    pub set: bool,
}

impl From<AdminProfile> for AdminProfileResponse {
    fn from(p: AdminProfile) -> Self {
        let set = !(p.display_name.is_empty()
            && p.github_username.is_empty()
            && p.website_url.is_empty()
            && p.avatar_url.is_empty());
        Self {
            display_name: p.display_name,
            github_username: p.github_username,
            website_url: p.website_url,
            avatar_url: p.avatar_url,
            set,
        }
    }
}

/// GET /api/admin/profile — the current owner profile.
pub async fn get_profile(
    State(state): State<AppState>,
) -> Result<Json<AdminProfileResponse>, AppError> {
    let profile = state.repo.get_admin_profile().await?;
    Ok(Json(profile.into()))
}

/// PUT /api/admin/profile — validate and save the owner profile.
pub async fn set_profile(
    State(state): State<AppState>,
    Json(body): Json<AdminProfileInput>,
) -> Result<Json<AdminProfileResponse>, AppError> {
    let display_name = validate::strip_control_chars(&body.display_name);
    let display_name = validate::clamp_to_max_len(&display_name, state.config.max_author_len);

    let github_username = validate::strip_control_chars(&body.github_username)
        .trim()
        .to_string();
    if !github_username.is_empty() {
        validate::validate_github_username(&github_username)
            .map_err(|e| AppError::BadRequest(format!("invalid github username: {e}")))?;
    }

    let website_url = body.website_url.trim().to_string();
    if !website_url.is_empty() {
        validate::validate_http_url(&website_url)
            .map_err(|e| AppError::BadRequest(format!("invalid website url: {e}")))?;
    }
    let avatar_url = body.avatar_url.trim().to_string();
    if !avatar_url.is_empty() {
        validate::validate_http_url(&avatar_url)
            .map_err(|e| AppError::BadRequest(format!("invalid avatar url: {e}")))?;
    }

    let profile = AdminProfile {
        display_name,
        github_username,
        website_url,
        avatar_url,
    };
    state.repo.set_admin_profile(profile.clone()).await?;
    Ok(Json(AdminProfileResponse::from(profile)))
}

/// POST /api/admin/comments — author a comment as the verified site owner.
///
/// The comment is created `approved` and `verified`, so it appears publicly
/// with the checkmark immediately. `parent_id` optionally makes it a reply
/// (parent must be approved and on the same path). Author name / URL default
/// to the profile.
#[derive(Deserialize)]
pub struct OwnerCommentRequest {
    pub target_path: String,
    pub content: String,
    pub parent_id: Option<i64>,
}

pub async fn create_owner_comment(
    State(state): State<AppState>,
    Json(body): Json<OwnerCommentRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate::validate_target_path(&body.target_path)
        .map_err(|e| AppError::BadRequest(format!("invalid path: {e}")))?;

    let profile = state.repo.get_admin_profile().await?;
    let author_name = if profile.display_name.trim().is_empty() {
        "Owner".to_string()
    } else {
        profile.display_name.clone()
    };
    let author_url = if profile.website_url.trim().is_empty() {
        None
    } else {
        Some(profile.website_url.clone())
    };
    let author_avatar = if profile.avatar_url.trim().is_empty() {
        None
    } else {
        Some(profile.avatar_url.clone())
    };

    let content = crate::sanitize::sanitize_html(&body.content, state.config.max_content_len);
    if content.trim().is_empty() {
        return Err(AppError::BadRequest("content cannot be empty".to_string()));
    }
    let content_hash = Some(crate::sanitize::content_hash(&body.content));

    // Resolve parent + depth following the reply rules.
    let (parent_id, depth) = match body.parent_id {
        Some(pid) => {
            let parent = state
                .repo
                .get_comment(pid)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("parent comment {pid} not found")))?;
            if parent.status != "approved" {
                return Err(AppError::BadRequest(format!(
                    "parent comment {pid} is not approved (status: {})",
                    parent.status
                )));
            }
            if parent.target_path != body.target_path {
                return Err(AppError::BadRequest(
                    "parent comment is on a different path".to_string(),
                ));
            }
            if state.config.max_thread_depth > 0 && parent.depth >= state.config.max_thread_depth {
                return Err(AppError::BadRequest(
                    "maximum thread depth reached".to_string(),
                ));
            }
            (Some(pid), parent.depth + 1)
        }
        None => (None, 0),
    };

    let input = crate::db::repo::NewComment {
        target_path: body.target_path,
        comment_type: "native".to_string(),
        source_url: None,
        author_name,
        author_url,
        author_avatar,
        content,
        parent_id,
        depth,
        honeypot: false,
        delete_token: None,
        submitter_ip: None,
        submitter_ip_hash: None,
        content_hash,
    };
    let id = state.repo.insert_owner_comment(input).await?;
    // Created verified; ensure approved immediately (owner comments are
    // public). A plain write, not a moderation transition — no event needed.
    state.repo.update_status(id, "approved").await?;

    Ok(Json(
        serde_json::json!({ "id": id, "status": "approved", "verified": true }),
    ))
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use crate::http::build_app;
    use crate::http::test_support::helpers;

    fn authorized(
        method: axum::http::Method,
        uri: &str,
        body: Option<&str>,
    ) -> axum::http::Request<axum::body::Body> {
        let mut req = helpers::request(method, uri);
        if let Some(b) = body {
            *req.body_mut() = axum::body::Body::from(b.to_owned());
            req.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            req.headers_mut()
                .insert(axum::http::header::CONTENT_LENGTH, b.len().into());
        }
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test"),
        );
        req
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn profile_defaults_empty_then_round_trips() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let d = body_json(
            app.clone()
                .oneshot(authorized(
                    axum::http::Method::GET,
                    "/api/admin/profile",
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(d["set"], false);
        let d = body_json(
            app.clone()
                .oneshot(authorized(
                    axum::http::Method::PUT,
                    "/api/admin/profile",
                    Some(r#"{"display_name":"Nithi","github_username":"nithitsuki","website_url":"https://nithitsuki.com","avatar_url":""}"#),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(d["set"], true);
        assert_eq!(d["display_name"], "Nithi");
        let d = body_json(
            app.clone()
                .oneshot(authorized(
                    axum::http::Method::GET,
                    "/api/admin/profile",
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(d["github_username"], "nithitsuki");
    }

    #[tokio::test]
    async fn profile_rejects_bad_github_and_url() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        for body in [
            r#"{"github_username":"not a name!!!"}"#,
            r#"{"website_url":"javascript:alert(1)"}"#,
            r#"{"avatar_url":"notaurl"}"#,
        ] {
            let resp = app
                .clone()
                .oneshot(authorized(
                    axum::http::Method::PUT,
                    "/api/admin/profile",
                    Some(body),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "body accepted: {body}");
        }
    }

    #[tokio::test]
    async fn owner_comment_posts_verified_approved_with_profile_identity() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let put = authorized(
            axum::http::Method::PUT,
            "/api/admin/profile",
            Some(r#"{"display_name":"Nithi","website_url":"https://nithitsuki.com"}"#),
        );
        assert!(
            app.clone()
                .oneshot(put)
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let d = body_json(
            app.clone()
                .oneshot(authorized(
                    axum::http::Method::POST,
                    "/api/admin/comments",
                    Some(r#"{"target_path":"/blog/hello","content":"Official reply"}"#),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(d["status"], "approved");
        assert_eq!(d["verified"], true);
        // Visible through the admin list with the owner identity attached.
        let list = body_json(
            app.clone()
                .oneshot(authorized(
                    axum::http::Method::GET,
                    "/api/admin/comments?status=approved&limit=5",
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        let arr = list["comments"].as_array().unwrap();
        let max_id = arr.iter().map(|c| c["id"].as_i64().unwrap()).max().unwrap();
        let mine = arr
            .iter()
            .find(|c| c["id"].as_i64() == Some(max_id))
            .unwrap();
        assert_eq!(mine["author_name"], "Nithi");
        assert_eq!(mine["verified"], true);
    }

    #[tokio::test]
    async fn owner_comment_rejects_empty_content_and_bad_parent() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let r = app
            .clone()
            .oneshot(authorized(
                axum::http::Method::POST,
                "/api/admin/comments",
                Some(r#"{"target_path":"/blog/hello","content":"   "}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let r = app
            .clone()
            .oneshot(authorized(
                axum::http::Method::POST,
                "/api/admin/comments",
                Some(r#"{"target_path":"/x","content":"hi","parent_id":424242}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn profile_endpoints_require_auth() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        for (method, uri, body) in [
            (axum::http::Method::GET, "/api/admin/profile", None),
            (axum::http::Method::PUT, "/api/admin/profile", Some("{}")),
            (
                axum::http::Method::POST,
                "/api/admin/comments",
                Some(r#"{"target_path":"/x","content":"hi"}"#),
            ),
        ] {
            let mut req = helpers::request(method.clone(), uri);
            if let Some(b) = body {
                *req.body_mut() = axum::body::Body::from(b.to_owned());
                req.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
            }
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), 401, "unauthenticated {uri} must 401");
        }
    }
}
