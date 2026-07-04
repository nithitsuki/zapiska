use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::state::AppState;
use crate::validate;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct CommentsQuery {
    /// Path on the main site (e.g. /blog/hello)
    #[param(example = "/blog/hello")]
    pub path: Option<String>,
    /// Maximum number of comments to return (max 100)
    #[param(maximum = 100, default = 50)]
    pub limit: Option<i64>,
    /// Cursor: return comments with id < before
    pub before: Option<i64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CommentsResponse {
    pub total: i64,
    pub comments: Vec<CommentJson>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CommentJson {
    pub id: i64,
    #[schema(example = "native")]
    pub comment_type: String,
    pub author_name: String,
    pub author_url: Option<String>,
    pub author_avatar: Option<String>,
    pub content: String,
    pub created_at: String,
}

#[utoipa::path(
    get,
    path = "/api/comments",
    params(CommentsQuery),
    responses(
        (status = 200, description = "List of approved comments", body = CommentsResponse),
        (status = 400, description = "Invalid path parameter"),
    ),
    tag = "comments",
)]
pub async fn list_comments(
    State(state): State<AppState>,
    Query(query): Query<CommentsQuery>,
) -> Result<Json<CommentsResponse>, AppError> {
    let target_path = query
        .path
        .ok_or_else(|| AppError::BadRequest("query parameter 'path' is required".to_string()))?;

    validate::validate_target_path(&target_path)
        .map_err(|e| AppError::BadRequest(format!("invalid path: {e}")))?;

    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let before = query.before;

    let comments = state
        .repo
        .list_approved(&target_path, limit, before)
        .await?;
    let total = state.repo.count_approved(&target_path).await?;

    let comments: Vec<CommentJson> = comments
        .into_iter()
        .map(|c| CommentJson {
            id: c.id,
            comment_type: c.comment_type,
            author_name: c.author_name,
            author_url: c.author_url,
            author_avatar: c.author_avatar,
            content: c.content,
            created_at: c.created_at,
        })
        .collect();

    Ok(Json(CommentsResponse { total, comments }))
}
