//! Admin lookup endpoints: author identity stats, path list, URL cross-
//! references, and bulk context for moderation engines.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::error::AppError;
use crate::state::AppState;

// ── GET /api/admin/authors/lookup ───────────────────────────

/// GET /api/admin/authors/lookup — resolve author identity and return stats.
/// Query params: ip, author_name, author_url, combine (bool).
pub async fn author_lookup(
    State(state): State<AppState>,
    Query(query): Query<AuthorLookupQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let combine = query.combine.unwrap_or(false);
    let result = state
        .repo
        .lookup_author(
            query.ip.as_deref(),
            query.author_name.as_deref(),
            query.author_url.as_deref(),
            combine,
        )
        .await?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct AuthorLookupQuery {
    pub ip: Option<String>,
    pub author_name: Option<String>,
    pub author_url: Option<String>,
    pub combine: Option<bool>,
}

// ── GET /api/admin/paths ────────────────────────────────────

/// List all paths that have comments, with counts per status.
pub async fn list_paths(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let paths = state.repo.list_paths().await?;
    let result: Vec<serde_json::Value> = paths
        .into_iter()
        .map(|(path, total, approved, spam, pending, deleted)| {
            serde_json::json!({
                "path": path,
                "total": total,
                "approved": approved,
                "spam": spam,
                "pending": pending,
                "deleted": deleted,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({"paths": result})))
}

// ── URL query endpoints ─────────────────────────────────────

/// GET /api/admin/urls/lookup?url_hash=<hash> — find all comments with a given URL.
pub async fn url_lookup(
    State(state): State<AppState>,
    Query(query): Query<UrlLookupQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    if let Some(hash) = &query.url_hash {
        let stats = state.repo.lookup_url(hash).await?;
        return Ok(Json(serde_json::to_value(&stats).unwrap()));
    }
    if let Some(domain) = &query.domain {
        let hashes = state.repo.lookup_domain(domain).await?;
        let mut results = Vec::new();
        for h in hashes {
            if let Ok(stats) = state.repo.lookup_url(&h).await {
                results.push(serde_json::to_value(&stats).unwrap());
            }
        }
        return Ok(Json(serde_json::json!({"urls": results, "domain": domain})));
    }
    Err(AppError::BadRequest(
        "provide url_hash or domain".to_string(),
    ))
}

#[derive(Deserialize)]
pub struct UrlLookupQuery {
    pub url_hash: Option<String>,
    pub domain: Option<String>,
}

/// GET /api/admin/comments/{id}/urls — list extracted URLs for a comment.
pub async fn comment_urls(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let urls = state.repo.get_comment_urls(id).await?;
    Ok(Json(serde_json::json!({"comment_id": id, "urls": urls})))
}

// ── POST /api/admin/comments/context ────────────────────────

/// Fetch context for multiple comments in one call: parent chains, author stats, URLs.
#[derive(Deserialize)]
pub struct BulkContextRequest {
    pub comment_ids: Vec<i64>,
    pub include_parents: Option<bool>,
    pub include_author_stats: Option<bool>,
    pub include_urls: Option<bool>,
}

pub async fn bulk_context(
    State(state): State<AppState>,
    Json(body): Json<BulkContextRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut results = Vec::with_capacity(body.comment_ids.len());

    for cid in body.comment_ids {
        let comment = state.repo.get_comment(cid).await?;
        let Some(c) = comment else { continue };

        let mut entry = serde_json::json!({
            "id": c.id,
            "target_path": c.target_path,
            "comment_type": c.comment_type,
            "author_name": c.author_name,
            "content": c.content,
            "status": c.status,
            "parent_id": c.parent_id,
            "depth": c.depth,
            "created_at": c.created_at,
        });

        if body.include_parents.unwrap_or(false) {
            if let Ok(Some((_, chain))) = state.repo.get_comment_chain(cid).await {
                entry["parents"] = serde_json::json!(
                    chain
                        .iter()
                        .map(|p| {
                            serde_json::json!({
                                "id": p.id,
                                "author_name": p.author_name,
                                "depth": p.depth,
                                "created_at": p.created_at,
                            })
                        })
                        .collect::<Vec<_>>()
                );
            }
        }

        if body.include_author_stats.unwrap_or(false) {
            if let Some(ref ip) = c.submitter_ip {
                if let Ok(stats) = state.repo.submitter_stats(ip).await {
                    entry["author_stats"] = serde_json::json!({
                        "total_comments": stats.0,
                        "approved": stats.1,
                        "spam": stats.2,
                        "pending": stats.3,
                        "deleted": stats.4,
                        "first_seen": stats.5,
                    });
                }
            }
        }

        if body.include_urls.unwrap_or(false) {
            if let Ok(urls) = state.repo.get_comment_urls(cid).await {
                entry["urls"] = serde_json::json!(urls);
            }
        }

        results.push(entry);
    }

    Ok(Json(serde_json::json!({"comments": results})))
}
