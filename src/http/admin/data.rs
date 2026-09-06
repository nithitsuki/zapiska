//! Data export/import endpoints for backups and migration.
//!
//! - `GET /api/admin/export` - full dump of all five tables as JSON.
//! - `POST /api/admin/import` — restore a dump (idempotent; preserves ids,
//!   statuses, and timestamps; re-sanitizes comment content).

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::db::repo::{
    Comment, CommentReaction, CommentUrl, ExportSnapshot, GithubProfile, NewGithubProfile,
    NewWebmentionSeen, WebmentionSeen,
};
use crate::error::AppError;
use crate::sanitize;
use crate::state::AppState;
use crate::validate;

/// Format version of the export document. Bump on breaking shape changes;
/// imports reject any other version.
pub const EXPORT_VERSION: i64 = 1;
/// Max accepted import body. Comment exports can legitimately exceed the
/// 8 KB form-body limit, so this route gets its own ceiling.
pub const MAX_IMPORT_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize)]
pub struct ExportFile {
    pub version: i64,
    pub exported_at: String,
    /// Whether the exporting server had `IP_HASH_SECRET` set. Stored IP
    /// hashes are salted with that secret, so the secret itself is part of
    /// the backup: back up `.env` alongside this file. Never the secret.
    pub ip_hash_salted: Option<bool>,
    pub comments: Vec<Comment>,
    pub webmention_seen: Vec<WebmentionSeen>,
    pub comment_urls: Vec<CommentUrl>,
    pub github_profiles: Vec<GithubProfile>,
    pub comment_reactions: Vec<CommentReaction>,
}

#[derive(Deserialize, Default)]
pub struct ImportFile {
    pub version: Option<i64>,
    /// Salt status of the exporting server. `None` = pre-flag export,
    /// unverifiable — see the import warning logic.
    #[serde(default)]
    pub ip_hash_salted: Option<bool>,
    pub comments: Option<Vec<Comment>>,
    pub webmention_seen: Option<Vec<WebmentionSeen>>,
    pub comment_urls: Option<Vec<CommentUrl>>,
    pub github_profiles: Option<Vec<GithubProfile>>,
    pub comment_reactions: Option<Vec<CommentReaction>>,
}

#[derive(Serialize)]
pub struct ImportResponse {
    pub comments_imported: usize,
    pub comments_skipped: usize,
    pub webmention_seen_imported: usize,
    pub comment_urls_imported: usize,
    pub github_profiles_imported: usize,
    pub comment_reactions_imported: usize,
    /// Comment hashes re-derived from the raw IP with this server's secret.
    pub ip_hashes_recomputed: usize,
    /// Present when the export's salt status mismatches this server, meaning
    /// reaction identities and unrecomputable hashes may be orphaned.
    pub warning: Option<String>,
}

/// GET /api/admin/export — full JSON dump (backup/migration source).
/// The five tables come from ONE connection in one read transaction
/// (T15 unit of work), so the dump is a single WAL snapshot, not five.
pub async fn export(State(state): State<AppState>) -> Result<Json<ExportFile>, AppError> {
    let ExportSnapshot {
        comments,
        seen: webmention_seen,
        urls: comment_urls,
        profiles: github_profiles,
        reactions: comment_reactions,
    } = state.repo.export_snapshot().await?;

    Ok(Json(ExportFile {
        version: EXPORT_VERSION,
        exported_at: crate::timeutil::now_iso8601(),
        ip_hash_salted: Some(state.config.ip_hash_secret.is_some()),
        comments,
        webmention_seen,
        comment_urls,
        github_profiles,
        comment_reactions,
    }))
}

/// POST /api/admin/import — restore an export document. Idempotent: rows are
/// upserted by natural key, so re-importing the same document is a no-op.
pub async fn import(
    State(state): State<AppState>,
    Json(body): Json<ImportFile>,
) -> Result<Json<ImportResponse>, AppError> {
    if body.version != Some(EXPORT_VERSION) {
        return Err(AppError::BadRequest(format!(
            "unsupported export version {:?}, expected {EXPORT_VERSION}",
            body.version
        )));
    }

    let mut response = ImportResponse {
        comments_imported: 0,
        comments_skipped: 0,
        webmention_seen_imported: 0,
        comment_urls_imported: 0,
        github_profiles_imported: 0,
        comment_reactions_imported: 0,
        ip_hashes_recomputed: 0,
        warning: None,
    };
    // Whether any salted-looking identity material crossed the wire. Used
    // for the salt-mismatch warning below.
    let mut saw_salted_identities = false;
    let export_salted = body.ip_hash_salted;
    let current_salted = state.config.ip_hash_secret.is_some();

    // Comments first (URL rows reference them), sorted by id so the
    // parent_id foreign key always resolves (parents precede children).
    if let Some(mut comments) = body.comments {
        comments.sort_by_key(|c| c.id);
        for mut c in comments {
            let id = c.id;
            if let Err(e) = validate_imported_comment(&mut c, state.config.max_content_len) {
                tracing::warn!(id, err = %e, "import skipped invalid comment");
                response.comments_skipped += 1;
                continue;
            }
            if c.submitter_ip_hash.is_some() {
                saw_salted_identities = true;
            }
            // Self-heal IP hashes across secret rotation: when the raw IP is
            // present, the hash is re-derived with THIS server's secret
            // instead of trusting the exported value. Rows without a raw IP
            // keep their exported hash verbatim.
            if let Some(ref raw) = c.submitter_ip {
                if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
                    // Canonicalize through ClientIdentity so a stored
                    // IPv4-mapped address re-derives the same hash as its
                    // plain IPv4 form (see src/http/peer.rs).
                    let ip = crate::http::peer::ClientIdentity::normalize_ip(ip);
                    let fresh =
                        crate::ip_hash::hash_ip(&ip, state.config.ip_hash_secret.as_deref());
                    if c.submitter_ip_hash.as_deref() != Some(fresh.as_str()) {
                        c.submitter_ip_hash = Some(fresh);
                        response.ip_hashes_recomputed += 1;
                    }
                }
            }
            // DB-level failures (e.g. an orphaned parent_id whose parent row
            // was skipped) are skipped too — a single bad row must never
            // abort the whole import.
            if let Err(e) = state.repo.import_comment(c).await {
                tracing::warn!(id, err = %e, "import skipped failed comment");
                response.comments_skipped += 1;
                continue;
            }
            response.comments_imported += 1;
        }
    }

    if let Some(rows) = body.webmention_seen {
        for row in rows {
            if !matches!(row.last_status.as_str(), "alive" | "gone") {
                continue;
            }
            state
                .repo
                .upsert_webmention_seen(NewWebmentionSeen {
                    source: row.source,
                    target: row.target,
                    last_status: row.last_status,
                })
                .await?;
            response.webmention_seen_imported += 1;
        }
    }

    if let Some(urls) = body.comment_urls {
        // Group by comment so each comment is cleared once, then re-inserted
        // in one batch (idempotent re-import).
        let mut by_comment: HashMap<i64, Vec<(String, String, String)>> = HashMap::new();
        for u in urls {
            by_comment
                .entry(u.comment_id)
                .or_default()
                .push((u.url, u.domain, u.url_hash));
        }
        for (comment_id, rows) in by_comment {
            let count = rows.len();
            // URL rows referencing comments that don't exist (e.g. skipped
            // during import) are dropped without aborting the import.
            if state
                .repo
                .delete_urls_for_comment(comment_id)
                .await
                .is_err()
                || state.repo.insert_urls(comment_id, rows).await.is_err()
            {
                tracing::warn!(comment_id, "import skipped URL rows for missing comment");
                continue;
            }
            response.comment_urls_imported += count;
        }
    }

    if let Some(reactions) = body.comment_reactions {
        for r in reactions {
            if let Err(e) = validate_imported_reaction(&r) {
                tracing::warn!(id = r.id, err = %e, "import skipped invalid reaction");
                continue;
            }
            // Anyone-mode identifiers are salted IP hashes: they cannot be
            // re-derived on import (no raw IP is stored for reactions).
            if r.identifier.starts_with("h:") {
                saw_salted_identities = true;
            }
            state.repo.import_comment_reaction(r).await?;
            response.comment_reactions_imported += 1;
        }
    }

    if let Some(profiles) = body.github_profiles {
        for p in profiles {
            state
                .repo
                .upsert_github_profile(NewGithubProfile {
                    login: p.login,
                    name: p.name,
                    avatar_url: p.avatar_url,
                    valid: p.valid,
                })
                .await?;
            response.github_profiles_imported += 1;
        }
    }

    // Salt-mismatch warning: comment hashes were re-derived above when a raw
    // IP existed, but reaction identities cannot be healed. A changed or lost
    // secret orphans anyone-mode reactions (old voters look like strangers)
    // and any hash kept verbatim for lack of a raw IP.
    if let Some(exported) = export_salted {
        if exported != current_salted && saw_salted_identities {
            let msg = format!(
                "IP hash salt mismatch: export salted={exported}, this server salted={current_salted}. \
                 Comment hashes were re-derived where a raw IP existed (see ip_hashes_recomputed); \
                 reaction identities cannot be re-derived. Keep IP_HASH_SECRET stable and back up .env alongside exports."
            );
            tracing::warn!(msg = %msg, "import completed with salt mismatch");
            response.warning = Some(msg);
        }
    }

    Ok(Json(response))
}

/// Validate a reaction row from an untrusted import document. The reaction
/// set itself is config, so only structural bounds are enforced here.
fn validate_imported_reaction(r: &CommentReaction) -> Result<(), String> {
    if r.id <= 0 {
        return Err("id must be a positive integer".to_string());
    }
    if r.comment_id <= 0 {
        return Err("comment_id must be a positive integer".to_string());
    }
    if r.reaction.is_empty() || r.reaction.chars().count() > 16 {
        return Err("reaction must be 1-16 characters".to_string());
    }
    if !matches!(
        r.status.as_str(),
        "pending" | "approved" | "spam" | "deleted"
    ) {
        return Err(format!("invalid status '{}'", r.status));
    }
    if r.identifier.is_empty() || r.identifier.len() > 128 {
        return Err("identifier must be 1-128 characters".to_string());
    }
    Ok(())
}

/// Validate a comment from an untrusted import document. Defense in depth:
/// even though the endpoint is admin-only, imported content is re-sanitized
/// and every field is re-checked exactly as a native submission would be.
fn validate_imported_comment(c: &mut Comment, max_content_len: usize) -> Result<(), String> {
    if c.id <= 0 {
        return Err("id must be a positive integer".to_string());
    }
    validate::validate_target_path(&c.target_path).map_err(|e| e.to_string())?;
    if !matches!(c.comment_type.as_str(), "native" | "webmention") {
        return Err(format!("invalid comment_type '{}'", c.comment_type));
    }
    if !matches!(
        c.status.as_str(),
        "pending" | "approved" | "spam" | "deleted"
    ) {
        return Err(format!("invalid status '{}'", c.status));
    }
    c.author_name = validate::strip_control_chars(&c.author_name)
        .trim()
        .to_string();
    if c.author_name.is_empty() {
        return Err("author_name must not be empty".to_string());
    }
    if c.author_name.chars().count() > 100 {
        c.author_name = c.author_name.chars().take(100).collect();
    }
    if let Some(ref u) = c.author_url {
        validate::validate_http_url(u).map_err(|e| e.to_string())?;
    }
    if let Some(pid) = c.parent_id {
        if pid >= c.id {
            return Err(format!("parent_id {pid} must precede id {}", c.id));
        }
    }
    c.depth = c.depth.clamp(0, 10);
    c.content = sanitize::sanitize_html(&c.content, max_content_len);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Comment {
        Comment {
            id: 42,
            target_path: "/blog/hello".to_string(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: "Alice".to_string(),
            author_url: Some("https://alice.blog".to_string()),
            author_avatar: None,
            content: "<p>Great post!</p>".to_string(),
            status: "approved".to_string(),
            created_at: "2026-08-07 12:00:00".to_string(),
            updated_at: "2026-08-07 12:00:00".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
        }
    }

    #[test]
    fn valid_comment_passes() {
        let mut c = sample();
        assert!(validate_imported_comment(&mut c, 2000).is_ok());
    }

    #[test]
    fn invalid_status_and_type_rejected() {
        let mut c = sample();
        c.status = "evil".to_string();
        assert!(validate_imported_comment(&mut c, 2000).is_err());
        let mut c = sample();
        c.comment_type = "spam".to_string();
        assert!(validate_imported_comment(&mut c, 2000).is_err());
    }

    #[test]
    fn parent_id_must_precede_comment() {
        let mut c = sample();
        c.parent_id = Some(43);
        assert!(validate_imported_comment(&mut c, 2000).is_err());
        let mut c = sample();
        c.parent_id = Some(41);
        assert!(validate_imported_comment(&mut c, 2000).is_ok());
    }

    #[test]
    fn content_is_resanitized_and_truncated() {
        let mut c = sample();
        c.content = format!("<script>alert(1)</script>{}", "x".repeat(5000));
        assert!(validate_imported_comment(&mut c, 2000).is_ok());
        assert!(!c.content.contains("<script>"), "script stripped on import");
        assert!(c.content.chars().count() <= 2000, "truncated to max len");
    }

    #[test]
    fn control_chars_stripped_from_author_name() {
        let mut c = sample();
        c.author_name = "Bad\x00Guy".to_string();
        assert!(validate_imported_comment(&mut c, 2000).is_ok());
        assert_eq!(c.author_name, "BadGuy");
    }

    #[test]
    fn reaction_validation() {
        let valid = CommentReaction {
            id: 1,
            comment_id: 2,
            reaction: "👍".to_string(),
            identifier: "admin".to_string(),
            status: "approved".to_string(),
            created_at: "2026-08-01 10:00:00".to_string(),
            updated_at: "2026-08-01 10:00:00".to_string(),
        };
        assert!(validate_imported_reaction(&valid).is_ok());
        let mut bad = valid.clone();
        bad.id = 0;
        assert!(validate_imported_reaction(&bad).is_err());
        let mut bad = valid.clone();
        bad.reaction = "x".repeat(17);
        assert!(validate_imported_reaction(&bad).is_err());
        let mut bad = valid.clone();
        bad.status = "evil".to_string();
        assert!(validate_imported_reaction(&bad).is_err());
        let mut bad = valid.clone();
        bad.comment_id = 0;
        assert!(validate_imported_reaction(&bad).is_err());
    }

    #[test]
    fn invalid_path_and_url_rejected() {
        let mut c = sample();
        c.target_path = "no-slash".to_string();
        assert!(validate_imported_comment(&mut c, 2000).is_err());
        let mut c = sample();
        c.author_url = Some("javascript:alert(1)".to_string());
        assert!(validate_imported_comment(&mut c, 2000).is_err());
    }
}
