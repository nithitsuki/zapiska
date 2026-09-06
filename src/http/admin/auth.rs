//! Admin authentication: Bearer-header or cookie token check, login/logout.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::validate_token;
use crate::error::AppError;
use crate::state::AppState;

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

/// Session cookie name. The `__Host-` prefix tells browsers to require
/// `Secure` + `Path=/` + no `Domain` (all hold here), so the token cookie can
/// never be set or sent over plain HTTP.
const SESSION_COOKIE_NAME: &str = "__Host-admin_token";

fn set_cookie_value(token: &str, max_age_secs: i64) -> String {
    format!(
        "{SESSION_COOKIE_NAME}={token}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_secs}"
    )
}

// ── Auth middleware ──────────────────────────────────────────

/// True when the request carries a valid admin token (Bearer header or
/// [`SESSION_COOKIE_NAME`] cookie). Shared by the middleware and endpoints
/// that must honor admin identity without being fully gated (e.g. reactions
/// in admin-only mode).
pub(crate) fn request_has_admin_token(state: &AppState, headers: &HeaderMap) -> bool {
    let expected = state.config.admin_token.as_bytes();

    let header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let t = header.strip_prefix("Bearer ").unwrap_or("");
    if !t.is_empty() && validate_token(t.as_bytes(), expected) {
        return true;
    }

    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    extract_cookie(cookie_header, SESSION_COOKIE_NAME)
        .map(|t| validate_token(t.as_bytes(), expected))
        .unwrap_or(false)
}

pub async fn admin_auth(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    if !request_has_admin_token(&state, req.headers()) {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(req).await)
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
