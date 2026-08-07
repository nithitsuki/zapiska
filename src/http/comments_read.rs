use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::state::AppState;
use crate::validate;

/// Supported comment orderings for `GET /api/comments`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// Newest first (default). Cursor: `before` (id < before).
    Newest,
    /// Oldest first. Cursor: `after` (id > after).
    Oldest,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct CommentsQuery {
    /// Path on the main site (e.g. /blog/hello)
    #[param(example = "/blog/hello")]
    pub path: Option<String>,
    /// Maximum number of comments to return (max 100)
    #[param(maximum = 100, default = 50)]
    pub limit: Option<i64>,
    /// Cursor for `sort=newest`: return comments with id < before
    pub before: Option<i64>,
    /// Cursor for `sort=oldest`: return comments with id > after
    pub after: Option<i64>,
    /// Ordering: `newest` (default) or `oldest`
    #[param(example = "newest")]
    pub sort: Option<String>,
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
    /// ID of the parent comment, if a reply. Null for top-level comments.
    pub parent_id: Option<i64>,
    /// Nesting depth (0 = top-level).
    pub depth: i64,
    /// Approved reaction counts, e.g. `{"👍": 5, "❤️": 2}`. Empty object
    /// when there are no approved reactions.
    pub reactions: std::collections::HashMap<String, i64>,
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

    let order = match query.sort.as_deref() {
        None | Some("newest") => SortOrder::Newest,
        Some("oldest") => SortOrder::Oldest,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "invalid sort '{other}', must be 'newest' or 'oldest'"
            )));
        }
    };

    let comments = match order {
        SortOrder::Newest => {
            state
                .repo
                .list_approved(&target_path, limit, query.before)
                .await?
        }
        SortOrder::Oldest => {
            state
                .repo
                .list_approved_oldest(&target_path, limit, query.after)
                .await?
        }
    };
    let total = state.repo.count_approved(&target_path).await?;

    // Approved reaction counts for the returned comments, one query.
    let comment_ids: Vec<i64> = comments.iter().map(|c| c.id).collect();
    let reaction_counts = state.repo.reaction_counts(&comment_ids).await?;

    let comments: Vec<CommentJson> = comments
        .into_iter()
        .map(|c| CommentJson {
            reactions: reaction_counts.get(&c.id).cloned().unwrap_or_default(),
            id: c.id,
            comment_type: c.comment_type,
            author_name: c.author_name,
            author_url: c.author_url,
            author_avatar: c.author_avatar,
            content: c.content,
            created_at: c.created_at,
            parent_id: c.parent_id,
            depth: c.depth,
        })
        .collect();

    Ok(Json(CommentsResponse { total, comments }))
}
