use axum::Json;
use axum::http::StatusCode;
use axum::http::header::RETRY_AFTER;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Clone)]
pub enum AppError {
    BadRequest(String),
    Unauthorized,
    NotFound(String),
    RateLimited {
        retry_after_secs: u64,
        reason: String,
    },
    Internal(String),
    ServiceUnavailable(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            AppError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: msg.clone(),
                    code: None,
                    retry_after: None,
                },
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                ErrorBody {
                    error: "unauthorized".to_string(),
                    code: None,
                    retry_after: None,
                },
            ),
            AppError::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                ErrorBody {
                    error: msg.clone(),
                    code: None,
                    retry_after: None,
                },
            ),
            AppError::RateLimited { reason, .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorBody {
                    error: reason.clone(),
                    code: Some("rate_limited".to_string()),
                    retry_after: None,
                },
            ),
            AppError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody {
                    error: msg.clone(),
                    code: None,
                    retry_after: None,
                },
            ),
            AppError::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorBody {
                    error: msg.clone(),
                    code: None,
                    retry_after: None,
                },
            ),
        };

        let mut response = (status, Json(body)).into_response();
        if let AppError::RateLimited {
            retry_after_secs, ..
        } = &self
        {
            response
                .headers_mut()
                .insert(RETRY_AFTER, retry_after_secs.to_string().parse().unwrap());
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{HeaderValue, StatusCode};

    #[test]
    fn bad_request_returns_400() {
        let resp = AppError::BadRequest("invalid input".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unauthorized_returns_401() {
        let resp = AppError::Unauthorized.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn not_found_returns_404() {
        let resp = AppError::NotFound("no such id".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn rate_limited_returns_429_with_retry_after() {
        let resp = AppError::RateLimited {
            retry_after_secs: 60,
            reason: "too many requests".into(),
        }
        .into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(RETRY_AFTER),
            Some(&HeaderValue::from_static("60"))
        );
    }

    #[test]
    fn internal_returns_500() {
        let resp = AppError::Internal("db error".into()).into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn service_unavailable_returns_503() {
        let resp = AppError::ServiceUnavailable("worker backlog full".into()).into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn rate_limited_body_contains_code_and_error_keys() {
        let resp = AppError::RateLimited {
            retry_after_secs: 30,
            reason: "slow down".into(),
        }
        .into_response();

        let bytes = to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body fits in 1 MiB");
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["code"], "rate_limited");
        assert_eq!(body["error"], "slow down");
    }

    #[tokio::test]
    async fn bad_request_body_has_error_no_code() {
        let resp = AppError::BadRequest("bad".into()).into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body fits in 1 MiB");
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "bad");
        assert!(body.get("code").is_none());
    }
}
