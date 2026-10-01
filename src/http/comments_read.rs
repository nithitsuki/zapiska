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

/// Comment-origin filter for `GET /api/comments`. `All` maps to no SQL
/// predicate, matching the behaviour before the `type` parameter existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentTypeFilter {
    All,
    Native,
    Webmention,
}

impl CommentTypeFilter {
    /// Parse the `type` query parameter. An absent parameter means `All`.
    /// Unknown values are rejected before any SQL runs, so a user-supplied
    /// value can never reach the query text.
    fn parse(raw: Option<&str>) -> Result<Self, AppError> {
        match raw {
            None | Some("all") => Ok(Self::All),
            Some("native") => Ok(Self::Native),
            Some("webmention") => Ok(Self::Webmention),
            Some(other) => Err(AppError::BadRequest(format!(
                "invalid type '{other}', must be 'native', 'webmention' or 'all'"
            ))),
        }
    }

    /// Value bound to the `comment_type` SQL parameter, or `None` for `All`
    /// (the predicate reads `?N IS NULL OR comment_type = ?N`). A fixed
    /// literal, never a user string, so the query stays parameterized.
    fn sql_value(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Native => Some("native"),
            Self::Webmention => Some("webmention"),
        }
    }
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
    /// Filter by comment origin: `native`, `webmention`, or `all` (default)
    #[serde(rename = "type")]
    #[param(example = "all")]
    pub comment_type: Option<String>,
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
    /// Source page URL for webmention comments. Null for native comments.
    pub source_url: Option<String>,
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
    /// True when this comment is authored by the verified site owner.
    /// Widgets render a checkmark badge for these.
    pub verified: bool,
}

#[utoipa::path(
    get,
    path = "/api/comments",
    params(CommentsQuery),
    responses(
        (status = 200, description = "List of approved comments", body = CommentsResponse),
        (status = 400, description = "Invalid path or type parameter"),
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

    let comment_type = CommentTypeFilter::parse(query.comment_type.as_deref())?;

    let order = match query.sort.as_deref() {
        None | Some("newest") => SortOrder::Newest,
        Some("oldest") => SortOrder::Oldest,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "invalid sort '{other}', must be 'newest' or 'oldest'"
            )));
        }
    };

    // List + total + approved reaction counts share ONE connection (T15
    // read batch): one acquire per request instead of three.
    let (comments, total, reaction_counts) = match order {
        SortOrder::Newest => {
            state
                .repo
                .list_approved_page(&target_path, limit, query.before, comment_type.sql_value())
                .await?
        }
        SortOrder::Oldest => {
            state
                .repo
                .list_approved_oldest_page(
                    &target_path,
                    limit,
                    query.after,
                    comment_type.sql_value(),
                )
                .await?
        }
    };

    let comments: Vec<CommentJson> = comments
        .into_iter()
        .map(|c| CommentJson {
            reactions: reaction_counts.get(&c.id).cloned().unwrap_or_default(),
            id: c.id,
            comment_type: c.comment_type,
            // The public `source_url` is the WHATWG-canonical form, while
            // STORAGE keeps the sender's raw string (that raw string is the
            // idempotency key — see `validate::normalize_http_url`). For a
            // non-canonical source the stored and returned values therefore
            // differ, by design.
            //
            // FALLBACK: a stored value that no longer parses, is not
            // http/https, or has no host (a row written before this rule, or
            // an imported row) is returned UNCHANGED rather than dropped —
            // dropping it would silently change the public contract and hide
            // the row's source.
            source_url: c
                .source_url
                .as_deref()
                .map(|raw| validate::normalize_http_url(raw).unwrap_or_else(|| raw.to_string())),
            author_name: c.author_name,
            author_url: c.author_url,
            author_avatar: c.author_avatar,
            content: c.content,
            created_at: c.created_at,
            parent_id: c.parent_id,
            depth: c.depth,
            verified: c.verified,
        })
        .collect();

    Ok(Json(CommentsResponse { total, comments }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_filter_absent_or_all_is_unfiltered() {
        assert_eq!(
            CommentTypeFilter::parse(None).unwrap(),
            CommentTypeFilter::All
        );
        assert_eq!(
            CommentTypeFilter::parse(Some("all")).unwrap(),
            CommentTypeFilter::All
        );
        assert_eq!(CommentTypeFilter::All.sql_value(), None);
    }

    #[test]
    fn type_filter_maps_known_values_to_bound_literals() {
        assert_eq!(
            CommentTypeFilter::parse(Some("native"))
                .unwrap()
                .sql_value(),
            Some("native")
        );
        assert_eq!(
            CommentTypeFilter::parse(Some("webmention"))
                .unwrap()
                .sql_value(),
            Some("webmention")
        );
    }

    #[test]
    fn type_filter_rejects_injection_and_unknown_values() {
        for raw in [
            "bogus",
            "'; DROP TABLE comments;--",
            "' OR '1'='1",
            "native' OR 1=1--",
        ] {
            assert!(
                CommentTypeFilter::parse(Some(raw)).is_err(),
                "type '{raw}' must be rejected"
            );
        }
    }
}
