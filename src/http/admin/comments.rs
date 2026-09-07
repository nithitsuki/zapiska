//! Comment listing endpoints: pending queue, filtered list, single comment
//! with its ancestor chain.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use crate::db::repo::Comment;
use crate::error::AppError;
use crate::state::AppState;

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
    /// SHA-256 hash of submitter IP (only available when STORE_IP_ADDRESS is enabled).
    pub submitter_ip_hash: Option<String>,
    /// Content hash for duplicate detection.
    pub content_hash: Option<String>,
    /// Approved reaction counts for this comment: `{emoji: count}`.
    /// Empty when none approved, or when the caller did not request them.
    #[serde(default)]
    pub reaction_counts: std::collections::HashMap<String, i64>,
    /// True when authored by the verified site owner (admin writer).
    pub verified: bool,
}

impl From<Comment> for PendingComment {
    fn from(c: Comment) -> Self {
        Self {
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
            submitter_ip_hash: c.submitter_ip_hash,
            content_hash: c.content_hash,
            created_at: c.created_at,
            reaction_counts: std::collections::HashMap::new(),
            verified: c.verified,
        }
    }
}

/// Attach approved reaction counts to a list of comments (one batched query).
async fn attach_counts(state: &AppState, comments: &mut [PendingComment]) -> Result<(), AppError> {
    let ids: Vec<i64> = comments.iter().map(|c| c.id).collect();
    let counts = state.repo.reaction_counts(&ids).await?;
    for c in comments.iter_mut() {
        if let Some(m) = counts.get(&c.id) {
            c.reaction_counts = m.clone();
        }
    }
    Ok(())
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
    let mut comments: Vec<PendingComment> =
        comments.into_iter().map(PendingComment::from).collect();
    attach_counts(&state, &mut comments).await?;

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
    /// Filter by content hash (for duplicate detection).
    pub content_hash: Option<String>,
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
    let content_hash = query.content_hash.as_deref();

    let comments = state
        .repo
        .list_comments(status, limit, before, path, ip, content_hash)
        .await?;
    let mut comments: Vec<PendingComment> =
        comments.into_iter().map(PendingComment::from).collect();
    attach_counts(&state, &mut comments).await?;

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

    Ok(Json(CommentDetail {
        parents: chain.into_iter().map(PendingComment::from).collect(),
        comment: PendingComment::from(comment),
    }))
}
