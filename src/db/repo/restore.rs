//! Restore service: [`Repo::restore`] replays an export document into the
//! database. Moved out of the HTTP handler (`src/http/admin/data.rs`, F4) so
//! the restore concept — ordering, per-row skip semantics, orphan tolerance,
//! salt re-derivation — lives where the SQL lives and is unit-testable
//! without HTTP plumbing.
//!
//! HISTORICAL DATA, NOT TRANSITIONS: every status in the export is recorded
//! state, replayed with a direct SQL write through the `import_*`
//! primitives. Restore never calls `Moderation::transition`, never touches
//! `approve_reaction_cas` (the live-moderation compare-and-swap), and never
//! emits to any `ModerationSink`: an import replaying thousands of approved
//! rows must not fire thousands of `status_changed` events at the moderation
//! engine (that would DDoS it). Only live paths — admin moderation,
//! self-delete, reaction upserts — transition and emit.
//!
//! UNIFORM SKIP POLICY (B-19): every section validates each row and writes it
//! with a per-row commit. A row that is invalid (failed validation) or
//! violates a storage constraint ([`RepoError::Constraint`]: FK orphan,
//! UNIQUE, CHECK — matched structurally on the T14 variant via
//! [`should_skip_row`], never on message substrings) is skipped and counted;
//! the restore continues and the counts always reach the caller in the
//! [`RestoreReport`]. A storage-health failure (`Busy`/`Io`/`Other`: disk
//! full, lock contention, corruption) aborts the whole restore with
//! [`RestoreError::Aborted`] — which embeds the counts-so-far, so the 500
//! carries what landed and what skipped before the failure — so the operator
//! retries instead of trusting a half-restore; re-import is idempotent and
//! heals partial state.
//!
//! ORDERING: comments sort by id so parents precede children (`parent_id <
//! id` is enforced by validation; a skipped parent cascades — its children
//! fail the FK and are skipped too, B-23). Comments land before URLs and
//! reactions because both reference them; orphaned URL/reaction rows for
//! missing comments are skipped, never aborting.
//!
//! OVERLAP (B-21, refuse-by-default): before the first write, every exported
//! comment/reaction id is compared against the live row with that id — after
//! normalizing the export row through the same validate-and-rederive
//! pipeline the write path uses, so idempotent re-imports (clamped depths,
//! re-sanitized content, re-derived hashes) never refuse themselves. Only a
//! *differing* normalized row collides — identical rows are the idempotent
//! re-import after a crash (B-24) and always pass, as are invalid rows the
//! write path would skip (they can never collide).
//! Any differing collision refuses the
//! whole restore ([`RestoreError::Refused`], HTTP 400) with the remedy in the
//! message, unless the import body sets `"force": true`, which is the
//! explicit operator decision to overwrite live rows (e.g. re-running a
//! migration after review). Ledger/profile rows upsert by natural key and
//! cannot clobber by id, so they are exempt. Rationale for refuse-by-default
//! over warn-and-continue: silently reverting live moderation decisions is
//! the highest-impact restore failure, and a warning buried in a 200 response
//! is exactly what an operator piping `curl` into a log file never reads.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{
    Comment, CommentReaction, CommentUrl, GithubProfile, NewGithubProfile, NewWebmentionSeen, Repo,
    RepoError, WebmentionSeen,
};
use crate::db::RestoreError;

/// One restore request: the export document plus the importing server's
/// policy inputs. The HTTP handler maps the import body + `Config` into this;
/// the storage layer never reads `Config` itself.
#[derive(Debug, Clone, Default)]
pub struct RestoreInput {
    /// Exported comment rows (any order; restore sorts parents first).
    pub comments: Vec<Comment>,
    /// Exported webmention ledger rows.
    pub seen: Vec<WebmentionSeen>,
    /// Exported extracted-URL rows.
    pub urls: Vec<CommentUrl>,
    /// Exported GitHub profile cache rows.
    pub profiles: Vec<GithubProfile>,
    /// Exported reaction rows.
    pub reactions: Vec<CommentReaction>,
    /// Salt flag recorded by the exporting server (`None` = pre-flag export).
    pub export_salted: Option<bool>,
    /// THIS server's `IP_HASH_SECRET` (for hash re-derivation, never exported).
    pub ip_hash_secret: Option<String>,
    /// THIS server's `MAX_CONTENT_LEN` (for content re-sanitization).
    /// Must be set: `0` would truncate every restored comment to empty.
    pub max_content_len: usize,
    /// Explicit overwrite policy for id collisions (B-21): `false` refuses
    /// when an exported id holds different live data, `true` overwrites.
    /// Maps from the import body's `"force"` field.
    pub force: bool,
}

/// Per-section outcome of a restore. Shared with the HTTP layer as the import
/// response body (`ImportResponse` is this type), so storage counts and HTTP
/// counts can never drift: every skipped row is counted in its section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub comments_imported: usize,
    pub comments_skipped: usize,
    pub webmention_seen_imported: usize,
    pub webmention_seen_skipped: usize,
    pub comment_urls_imported: usize,
    pub comment_urls_skipped: usize,
    pub github_profiles_imported: usize,
    pub github_profiles_skipped: usize,
    pub comment_reactions_imported: usize,
    pub comment_reactions_skipped: usize,
    /// Comment hashes re-derived from the raw IP with this server's secret.
    pub ip_hashes_recomputed: usize,
    /// Present when the export's salt status mismatches this server, meaning
    /// reaction identities and unrecomputable hashes may be orphaned.
    pub warning: Option<String>,
}

/// Classify a per-row write failure structurally (T14, no substrings):
/// `Constraint` means the ROW is invalid — skip and count it.
/// `Busy`/`Io`/`Other`/`NotFound` mean storage health or programmer error —
/// abort the whole restore for retry/investigation.
fn should_skip_row(err: &RepoError) -> bool {
    matches!(err, RepoError::Constraint(_))
}

/// Canonicalize one address: IPv4-mapped IPv6 collapses to IPv4. Must match
/// `ClientIdentity::normalize_ip` (`src/http/peer.rs`) — restore re-derives
/// hashes with this server's secret through the same canonical form, so a
/// stored IPv4-mapped address re-derives the same hash as its plain IPv4
/// form. Duplicated (3 lines) instead of depending on the HTTP layer from
/// storage; parity is pinned by `normalize_ip_matches_client_identity`.
fn normalize_ip(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

impl Repo {
    /// Replay an export document. See the module docs for the contract:
    /// uniform skip-and-count, refuse-by-default overlap, historical statuses
    /// without webhooks or CAS.
    pub async fn restore(&self, input: RestoreInput) -> Result<RestoreReport, RestoreError> {
        // G4.2: `max_content_len: 0` (e.g. via `..Default::default()`) would
        // sanitize every restored comment to empty. Debug-only on purpose:
        // production keeps serving rather than panicking on config drift.
        debug_assert!(
            input.max_content_len > 0,
            "RestoreInput::max_content_len must be > 0"
        );
        self.check_restore_overlap(&input).await?;
        let mut report = RestoreReport::default();
        let mut saw_salted_identities = false;
        self.restore_comments(&input, &mut report, &mut saw_salted_identities)
            .await?;
        self.restore_seen(&input, &mut report).await?;
        self.restore_urls(&input, &mut report).await?;
        self.restore_reactions(&input, &mut report, &mut saw_salted_identities)
            .await?;
        self.restore_profiles(&input, &mut report).await?;
        // Salt-mismatch warning: comment hashes were re-derived above when a
        // raw IP existed, but reaction identities cannot be healed. A changed
        // or lost secret orphans anyone-mode reactions (old voters look like
        // strangers) and any hash kept verbatim for lack of a raw IP.
        if let Some(exported) = input.export_salted {
            let current_salted = input.ip_hash_secret.is_some();
            if exported != current_salted && saw_salted_identities {
                let msg = format!(
                    "IP hash salt mismatch: export salted={exported}, this server salted={current_salted}. \
                     Comment hashes were re-derived where a raw IP existed (see ip_hashes_recomputed); \
                     reaction identities cannot be re-derived. Keep IP_HASH_SECRET stable and back up .env alongside exports."
                );
                tracing::warn!(msg = %msg, "import completed with salt mismatch");
                report.warning = Some(msg);
            }
        }
        Ok(report)
    }

    /// B-21 pre-flight: compare every exported id against the live row with
    /// that id. Runs before the first write, so a refusal leaves the database
    /// untouched. Identical rows (idempotent re-import) never collide; only
    /// differing rows refuse, unless `force` explicitly allows the overwrite.
    async fn check_restore_overlap(&self, input: &RestoreInput) -> Result<(), RestoreError> {
        if input.force {
            if !input.comments.is_empty() || !input.reactions.is_empty() {
                // G4.6: audit trail for the explicit overwrite decision.
                tracing::warn!(
                    comments = input.comments.len(),
                    reactions = input.reactions.len(),
                    "restore force=true: live rows with colliding ids will be overwritten"
                );
            }
            return Ok(());
        }
        // G4.3: compare NORMALIZED rows — the same validate-and-rederive
        // pipeline the write path applies — so a re-import never refuses
        // itself over a clamped depth, re-sanitized content, or re-derived
        // hash. Invalid rows normalize to `None` (the write path would skip
        // them, so they can never collide) and are ignored here.
        let wanted_comments: HashMap<i64, Comment> = input
            .comments
            .iter()
            .filter_map(|c| {
                normalize_comment_for_compare(
                    c,
                    input.max_content_len,
                    input.ip_hash_secret.as_deref(),
                )
                .map(|normalized| (c.id, normalized))
            })
            .collect();
        let mut colliding_comments: Vec<i64> = Vec::new();
        let ids: Vec<i64> = wanted_comments.keys().copied().collect();
        for chunk in ids.chunks(500) {
            let owned = chunk.to_vec();
            let existing = self
                .spawn(move |conn| select_comments_by_ids(conn, &owned))
                .await?;
            for row in existing {
                if wanted_comments.get(&row.id).is_some_and(|w| w != &row) {
                    colliding_comments.push(row.id);
                }
            }
        }
        let wanted_reactions: HashMap<i64, &CommentReaction> =
            input.reactions.iter().map(|r| (r.id, r)).collect();
        let mut colliding_reactions: Vec<i64> = Vec::new();
        let ids: Vec<i64> = wanted_reactions.keys().copied().collect();
        for chunk in ids.chunks(500) {
            let owned = chunk.to_vec();
            let existing = self
                .spawn(move |conn| select_reactions_by_ids(conn, &owned))
                .await?;
            for row in existing {
                if wanted_reactions.get(&row.id).is_some_and(|w| *w != &row) {
                    colliding_reactions.push(row.id);
                }
            }
        }
        if colliding_comments.is_empty() && colliding_reactions.is_empty() {
            return Ok(());
        }
        colliding_comments.sort_unstable();
        colliding_reactions.sort_unstable();
        let sample_c: Vec<i64> = colliding_comments.iter().copied().take(5).collect();
        let sample_r: Vec<i64> = colliding_reactions.iter().copied().take(5).collect();
        Err(RestoreError::Refused(format!(
            "import refused: export would overwrite {} live comment(s) (e.g. ids {sample_c:?}) \
             and {} live reaction(s) (e.g. ids {sample_r:?}) holding different data; \
             restore targets an empty database — re-run with `\"force\": true` in the import body \
             to overwrite live rows explicitly. Re-importing the same document is always allowed \
             because identical rows never collide",
            colliding_comments.len(),
            colliding_reactions.len(),
        )))
    }

    /// Comments first (URL/reaction rows reference them), sorted by id so the
    /// `parent_id` foreign key always resolves (parents precede children).
    /// Historical statuses write directly — never a transition, never an event.
    async fn restore_comments(
        &self,
        input: &RestoreInput,
        report: &mut RestoreReport,
        saw_salted_identities: &mut bool,
    ) -> Result<(), RestoreError> {
        let mut comments = input.comments.clone();
        comments.sort_by_key(|c| c.id);
        for mut c in comments {
            let id = c.id;
            if let Err(e) = validate_imported_comment(&mut c, input.max_content_len) {
                tracing::warn!(id, err = %e, "import skipped invalid comment");
                report.comments_skipped += 1;
                continue;
            }
            if c.submitter_ip_hash.is_some() {
                *saw_salted_identities = true;
            }
            // Self-heal IP hashes across secret rotation: when the raw IP is
            // present, the hash is re-derived with THIS server's secret
            // (beside the `hash_ip` logic it references) instead of trusting
            // the exported value. Rows without a raw IP keep their exported
            // hash verbatim.
            if let Some(ref raw) = c.submitter_ip {
                if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
                    let ip = normalize_ip(ip);
                    let fresh = crate::ip_hash::hash_ip(&ip, input.ip_hash_secret.as_deref());
                    if c.submitter_ip_hash.as_deref() != Some(fresh.as_str()) {
                        c.submitter_ip_hash = Some(fresh);
                        report.ip_hashes_recomputed += 1;
                    }
                }
            }
            // DB-level failures (e.g. an orphaned parent_id whose parent row
            // was skipped) skip too — a single bad row never aborts.
            match self.import_comment(c).await {
                Ok(()) => report.comments_imported += 1,
                Err(e) if should_skip_row(&e) => {
                    tracing::warn!(id, err = %e, "import skipped failed comment");
                    report.comments_skipped += 1;
                }
                Err(e) => return Err(RestoreError::abort(e, report)),
            }
        }
        Ok(())
    }

    async fn restore_seen(
        &self,
        input: &RestoreInput,
        report: &mut RestoreReport,
    ) -> Result<(), RestoreError> {
        for row in &input.seen {
            if let Err(e) = validate_imported_seen(row) {
                tracing::warn!(
                    source = %row.source,
                    err = %e,
                    "import skipped invalid webmention-seen row"
                );
                report.webmention_seen_skipped += 1;
                continue;
            }
            let out = self
                .upsert_webmention_seen(NewWebmentionSeen {
                    source: row.source.clone(),
                    target: row.target.clone(),
                    last_status: row.last_status.clone(),
                })
                .await;
            match out {
                Ok(()) => report.webmention_seen_imported += 1,
                Err(e) if should_skip_row(&e) => {
                    tracing::warn!(source = %row.source, err = %e, "import skipped failed seen row");
                    report.webmention_seen_skipped += 1;
                }
                Err(e) => return Err(RestoreError::abort(e, report)),
            }
        }
        Ok(())
    }

    /// URLs group by comment so each comment is cleared once, then re-inserted
    /// in one atomic batch (idempotent re-import, B-24). URL rows referencing
    /// comments that don't exist (e.g. skipped during import) are dropped
    /// without aborting the import.
    async fn restore_urls(
        &self,
        input: &RestoreInput,
        report: &mut RestoreReport,
    ) -> Result<(), RestoreError> {
        let mut by_comment: HashMap<i64, Vec<(String, String, String)>> = HashMap::new();
        for u in &input.urls {
            match validate_imported_url(u) {
                Ok(()) => by_comment.entry(u.comment_id).or_default().push((
                    u.url.clone(),
                    u.domain.clone(),
                    u.url_hash.clone(),
                )),
                Err(e) => {
                    tracing::warn!(id = u.id, err = %e, "import skipped invalid URL row");
                    report.comment_urls_skipped += 1;
                }
            }
        }
        for (comment_id, rows) in by_comment {
            let wanted = rows.len();
            let exists = match self.get_comment(comment_id).await {
                Ok(opt) => opt.is_some(),
                Err(e) => return Err(RestoreError::abort(e, report)),
            };
            if !exists {
                tracing::warn!(comment_id, "import skipped URL rows for missing comment");
                report.comment_urls_skipped += wanted;
                continue;
            }
            match self.replace_urls_for_comment(comment_id, rows).await {
                Ok((ok, skipped)) => {
                    report.comment_urls_imported += ok;
                    report.comment_urls_skipped += skipped;
                }
                Err(e) if should_skip_row(&e) => {
                    tracing::warn!(comment_id, err = %e, "import skipped URL rows for comment");
                    report.comment_urls_skipped += wanted;
                }
                Err(e) => return Err(RestoreError::abort(e, report)),
            }
        }
        Ok(())
    }

    /// Reactions write statuses directly (historical replay): no CAS —
    /// `approve_reaction_cas` is for live moderation races, not restore — and
    /// no webhook emission. Orphaned rows (comment skipped above) fail the FK
    /// and skip with a count instead of aborting (B-19).
    async fn restore_reactions(
        &self,
        input: &RestoreInput,
        report: &mut RestoreReport,
        saw_salted_identities: &mut bool,
    ) -> Result<(), RestoreError> {
        for r in &input.reactions {
            if let Err(e) = validate_imported_reaction(r) {
                tracing::warn!(id = r.id, err = %e, "import skipped invalid reaction");
                report.comment_reactions_skipped += 1;
                continue;
            }
            // Anyone-mode identifiers are salted IP hashes: they cannot be
            // re-derived on import (no raw IP is stored for reactions).
            if r.identifier.starts_with("h:") {
                *saw_salted_identities = true;
            }
            match self.import_comment_reaction(r.clone()).await {
                Ok(()) => report.comment_reactions_imported += 1,
                Err(e) if should_skip_row(&e) => {
                    tracing::warn!(id = r.id, err = %e, "import skipped failed reaction");
                    report.comment_reactions_skipped += 1;
                }
                Err(e) => return Err(RestoreError::abort(e, report)),
            }
        }
        Ok(())
    }

    async fn restore_profiles(
        &self,
        input: &RestoreInput,
        report: &mut RestoreReport,
    ) -> Result<(), RestoreError> {
        for p in &input.profiles {
            if let Err(e) = validate_imported_profile(p) {
                tracing::warn!(login = %p.login, err = %e, "import skipped invalid profile");
                report.github_profiles_skipped += 1;
                continue;
            }
            let out = self
                .upsert_github_profile(NewGithubProfile {
                    login: p.login.clone(),
                    name: p.name.clone(),
                    avatar_url: p.avatar_url.clone(),
                    valid: p.valid,
                })
                .await;
            match out {
                Ok(()) => report.github_profiles_imported += 1,
                Err(e) if should_skip_row(&e) => {
                    tracing::warn!(login = %p.login, err = %e, "import skipped failed profile");
                    report.github_profiles_skipped += 1;
                }
                Err(e) => return Err(RestoreError::abort(e, report)),
            }
        }
        Ok(())
    }
}

/// Comments by id on the caller's connection (overlap pre-flight).
fn select_comments_by_ids(
    conn: &rusqlite::Connection,
    ids: &[i64],
) -> Result<Vec<Comment>, RepoError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT {} FROM comments WHERE id IN ({placeholders}) ORDER BY id",
        super::COMMENT_COLUMNS
    );
    let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids), super::row_to_comment)
        .map_err(RepoError::from)?;
    let mut out = Vec::with_capacity(ids.len());
    for row in rows {
        out.push(row.map_err(RepoError::from)?);
    }
    Ok(out)
}

/// Reactions by id on the caller's connection (overlap pre-flight).
fn select_reactions_by_ids(
    conn: &rusqlite::Connection,
    ids: &[i64],
) -> Result<Vec<CommentReaction>, RepoError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, comment_id, reaction, identifier, status, created_at, updated_at \
         FROM comment_reactions WHERE id IN ({placeholders}) ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids), |row| {
            Ok(CommentReaction {
                id: row.get(0)?,
                comment_id: row.get(1)?,
                reaction: row.get(2)?,
                identifier: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })
        .map_err(RepoError::from)?;
    let mut out = Vec::with_capacity(ids.len());
    for row in rows {
        out.push(row.map_err(RepoError::from)?);
    }
    Ok(out)
}

/// Normalize one exported comment exactly as the write path does
/// (validate-mutate + IP-hash re-derive), for the overlap pre-flight.
/// Returns `None` for invalid rows: the write path would skip them, so they
/// can never collide with live data. MUST stay in lockstep with
/// `restore_comments`, which is deliberately not refactored to share it —
/// the write path additionally counts recomputes and salt sightings, and the
/// pre-flight must stay side-effect free.
fn normalize_comment_for_compare(
    c: &Comment,
    max_content_len: usize,
    ip_hash_secret: Option<&str>,
) -> Option<Comment> {
    let mut out = c.clone();
    if validate_imported_comment(&mut out, max_content_len).is_err() {
        return None;
    }
    if let Some(ref raw) = out.submitter_ip {
        if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
            let fresh = crate::ip_hash::hash_ip(&normalize_ip(ip), ip_hash_secret);
            if out.submitter_ip_hash.as_deref() != Some(fresh.as_str()) {
                out.submitter_ip_hash = Some(fresh);
            }
        }
    }
    Some(out)
}

/// Validate a comment from an untrusted import document. Defense in depth:
/// even though the endpoint is admin-only, imported content is re-sanitized
/// and every field is re-checked exactly as a native submission would be.
fn validate_imported_comment(c: &mut Comment, max_content_len: usize) -> Result<(), String> {
    if c.id <= 0 {
        return Err("id must be a positive integer".to_string());
    }
    crate::validate::validate_target_path(&c.target_path).map_err(|e| e.to_string())?;
    if !matches!(c.comment_type.as_str(), "native" | "webmention") {
        return Err(format!("invalid comment_type '{}'", c.comment_type));
    }
    if !matches!(
        c.status.as_str(),
        "pending" | "approved" | "spam" | "deleted"
    ) {
        return Err(format!("invalid status '{}'", c.status));
    }
    c.author_name = crate::validate::strip_control_chars(&c.author_name)
        .trim()
        .to_string();
    if c.author_name.is_empty() {
        return Err("author_name must not be empty".to_string());
    }
    if c.author_name.chars().count() > 100 {
        c.author_name = c.author_name.chars().take(100).collect();
    }
    if let Some(ref u) = c.author_url {
        crate::validate::validate_http_url(u).map_err(|e| e.to_string())?;
    }
    if let Some(pid) = c.parent_id {
        if pid >= c.id {
            return Err(format!("parent_id {pid} must precede id {}", c.id));
        }
    }
    c.depth = c.depth.clamp(0, 10);
    c.content = crate::sanitize::sanitize_html(&c.content, max_content_len);
    Ok(())
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

/// Validate a webmention ledger row (B-27): the ledger only ever holds
/// absolute source/target URLs with an alive/gone state, so anything else is
/// skipped. Byte-length caps mirror the URL-row caps below.
fn validate_imported_seen(row: &WebmentionSeen) -> Result<(), String> {
    if row.source.is_empty() || row.source.len() > 2048 {
        return Err("source must be 1-2048 bytes".to_string());
    }
    if row.target.is_empty() || row.target.len() > 2048 {
        return Err("target must be 1-2048 bytes".to_string());
    }
    crate::validate::validate_http_url(&row.source).map_err(|e| e.to_string())?;
    crate::validate::validate_http_url(&row.target).map_err(|e| e.to_string())?;
    if !matches!(row.last_status.as_str(), "alive" | "gone") {
        return Err(format!("invalid last_status '{}'", row.last_status));
    }
    Ok(())
}

/// Validate an extracted-URL row (B-27): rows are produced by URL extraction,
/// so they are absolute http(s) URLs with a DNS-sized domain and a short
/// hash. Anything else is skipped. Byte-length caps.
fn validate_imported_url(u: &CommentUrl) -> Result<(), String> {
    if u.comment_id <= 0 {
        return Err("comment_id must be a positive integer".to_string());
    }
    if u.url.is_empty() || u.url.len() > 2048 {
        return Err("url must be 1-2048 bytes".to_string());
    }
    crate::validate::validate_http_url(&u.url).map_err(|e| e.to_string())?;
    if u.domain.is_empty() || u.domain.len() > 253 {
        return Err("domain must be 1-253 bytes".to_string());
    }
    if u.domain.chars().any(|c| c.is_control()) {
        return Err("domain contains control characters".to_string());
    }
    if u.url_hash.is_empty() || u.url_hash.len() > 128 {
        return Err("url_hash must be 1-128 bytes".to_string());
    }
    Ok(())
}

/// Validate a GitHub profile cache row (B-27): logins are GitHub's shape
/// (non-empty, 39 chars max), negative-cache rows carry an empty avatar URL
/// while positive rows carry an absolute http(s) avatar URL.
fn validate_imported_profile(p: &GithubProfile) -> Result<(), String> {
    if p.login.is_empty() || p.login.len() > 39 {
        return Err("login must be 1-39 bytes".to_string());
    }
    if p.login.chars().any(|c| c.is_control()) {
        return Err("login contains control characters".to_string());
    }
    if let Some(ref name) = p.name {
        if name.len() > 255 {
            return Err("name must be at most 255 bytes".to_string());
        }
    }
    if !p.avatar_url.is_empty() {
        if p.avatar_url.len() > 2048 {
            return Err("avatar_url must be at most 2048 bytes".to_string());
        }
        crate::validate::validate_http_url(&p.avatar_url).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod t17_restore_tests {
    use super::*;
    use crate::db::pool::{create_pool, run_migrations};

    fn setup_repo() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t17_restore.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    fn comment(id: i64, status: &str) -> Comment {
        Comment {
            id,
            target_path: "/t17".to_string(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: "T17".to_string(),
            author_url: None,
            author_avatar: None,
            content: format!("comment {id}"),
            status: status.to_string(),
            created_at: "2026-08-01 10:00:00".to_string(),
            updated_at: "2026-08-01 10:00:00".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
        }
    }

    fn reaction(id: i64, comment_id: i64, status: &str) -> CommentReaction {
        CommentReaction {
            id,
            comment_id,
            reaction: "👍".to_string(),
            identifier: "admin".to_string(),
            status: status.to_string(),
            created_at: "2026-08-01 10:00:00".to_string(),
            updated_at: "2026-08-01 10:00:00".to_string(),
        }
    }

    fn seen(source: &str, target: &str, status: &str) -> WebmentionSeen {
        WebmentionSeen {
            source: source.to_string(),
            target: target.to_string(),
            last_seen_at: "2026-08-01 10:00:00".to_string(),
            last_status: status.to_string(),
        }
    }

    fn url(id: i64, comment_id: i64) -> CommentUrl {
        CommentUrl {
            id,
            comment_id,
            url: format!("https://u17.example/p{id}"),
            domain: "u17.example".to_string(),
            url_hash: format!("h:u17-{id}"),
        }
    }

    fn profile(login: &str) -> GithubProfile {
        GithubProfile {
            login: login.to_string(),
            name: None,
            avatar_url: "https://avatars.example/u/17".to_string(),
            cached_at: "2026-08-01 10:00:00".to_string(),
            valid: true,
        }
    }

    fn input() -> RestoreInput {
        RestoreInput {
            max_content_len: 2000,
            ..RestoreInput::default()
        }
    }

    #[test]
    fn skip_policy_pins_t14_variants_structurally() {
        // (c): Constraint skips; everything else aborts. New variants added
        // to RepoError later must make a deliberate choice here (the match is
        // exhaustive over the policy, not a catch-all).
        assert!(should_skip_row(&RepoError::Constraint("fk".to_string())));
        assert!(!should_skip_row(&RepoError::Busy("locked".to_string())));
        assert!(!should_skip_row(&RepoError::Io("disk full".to_string())));
        assert!(!should_skip_row(&RepoError::Other("corrupt".to_string())));
        assert!(!should_skip_row(&RepoError::NotFound("gone".to_string())));
    }

    #[test]
    fn normalize_ip_matches_client_identity() {
        // Pins parity with `ClientIdentity::normalize_ip` without depending
        // on the HTTP layer from storage.
        let mapped: std::net::IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        let v4: std::net::IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(normalize_ip(mapped), v4);
        assert_eq!(
            normalize_ip(mapped),
            crate::http::peer::ClientIdentity::normalize_ip(mapped)
        );
        let v6: std::net::IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(normalize_ip(v6), v6);
        assert_eq!(normalize_ip(v4), v4);
    }

    #[tokio::test]
    async fn fk_broken_reaction_skips_with_counts() {
        let (repo, _dir) = setup_repo();
        let mut doc = input();
        doc.comments = vec![comment(1, "approved")];
        doc.reactions = vec![reaction(1, 999, "approved")];
        let report = repo.restore(doc).await.unwrap();
        assert_eq!(report.comments_imported, 1);
        assert_eq!(report.comments_skipped, 0);
        assert_eq!(report.comment_reactions_imported, 0);
        assert_eq!(report.comment_reactions_skipped, 1);
        assert!(repo.get_comment(1).await.unwrap().is_some());
        assert!(repo.get_reaction(1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn live_db_colliding_comment_refuses_without_touching_db() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .insert_comment(crate::db::NewComment {
                target_path: "/live".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Live".to_string(),
                author_url: None,
                author_avatar: None,
                content: "live decision".to_string(),
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
        repo.update_status(id, "approved").await.unwrap();
        let mut clash = comment(id, "pending");
        clash.author_name = "Backup".to_string();
        clash.content = "stale backup".to_string();
        let mut doc = input();
        doc.comments = vec![clash];
        let err = repo.restore(doc).await.unwrap_err();
        assert!(
            matches!(err, RestoreError::Refused(_)),
            "colliding live id must refuse, got {err:?}"
        );
        assert!(err.to_string().contains("\"force\": true"));
        let live = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(live.content, "live decision");
        assert_eq!(live.status, "approved");
    }

    #[tokio::test]
    async fn reaction_id_collision_refuses() {
        let (repo, _dir) = setup_repo();
        let cid = repo
            .insert_comment(crate::db::NewComment {
                target_path: "/t17-r".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "T17".to_string(),
                author_url: None,
                author_avatar: None,
                content: "hi".to_string(),
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
        repo.update_status(cid, "approved").await.unwrap();
        let (rid, _) = repo.upsert_reaction(cid, "👍", "admin").await.unwrap();
        repo.update_reaction_status(rid, "approved").await.unwrap();
        // Same comment id must also match for the overlap pre-flight to even
        // run: reuse the live comment row verbatim so only the reaction
        // collides.
        let live_comment = repo.get_comment(cid).await.unwrap().unwrap();
        let mut clash = reaction(rid, cid, "spam");
        clash.reaction = "❤️".to_string();
        let mut doc = input();
        doc.comments = vec![live_comment];
        doc.reactions = vec![clash];
        let err = repo.restore(doc).await.unwrap_err();
        assert!(matches!(err, RestoreError::Refused(_)), "got {err:?}");
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().reaction,
            "👍"
        );
    }

    #[tokio::test]
    async fn identical_reimport_is_allowed_and_idempotent() {
        // Crash recovery (B-24): re-importing the same document after a
        // partial restore never refuses — identical rows don't collide.
        let (repo, _dir) = setup_repo();
        let mut doc = input();
        doc.comments = vec![comment(7, "approved")];
        doc.reactions = vec![reaction(9, 7, "approved")];
        for _ in 0..2 {
            let report = repo.restore(doc.clone()).await.unwrap();
            assert_eq!(report.comments_imported, 1);
            assert_eq!(report.comment_reactions_imported, 1);
        }
        assert_eq!(repo.list_all_comments().await.unwrap().len(), 1);
        assert_eq!(repo.list_all_comment_reactions().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn force_overwrites_colliding_live_row_explicitly() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .insert_comment(crate::db::NewComment {
                target_path: "/live".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Live".to_string(),
                author_url: None,
                author_avatar: None,
                content: "live".to_string(),
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
        let mut overwrite = comment(id, "approved");
        overwrite.content = "operator reviewed backup".to_string();
        let mut doc = input();
        doc.comments = vec![overwrite];
        doc.force = true;
        let report = repo.restore(doc).await.unwrap();
        assert_eq!(report.comments_imported, 1);
        assert_eq!(
            repo.get_comment(id).await.unwrap().unwrap().content,
            "operator reviewed backup"
        );
    }

    #[tokio::test]
    async fn mixed_sections_report_exact_counts() {
        let (repo, _dir) = setup_repo();
        let bad_comment = comment(1, "evil-status");
        let mut child_of_skipped = comment(2, "pending");
        child_of_skipped.parent_id = Some(1);
        let mut doc = input();
        doc.comments = vec![comment(3, "approved"), bad_comment, child_of_skipped];
        doc.seen = vec![
            seen("https://s.example/a", "https://site.example/a", "alive"),
            seen("https://s.example/b", "https://site.example/b", "bogus"),
        ];
        let mut bad_url = url(7, 3);
        bad_url.url = "ftp://u17.example/nope".to_string();
        doc.urls = vec![url(8, 3), bad_url, url(9, 999)];
        let empty_login = profile("");
        doc.profiles = vec![profile("t17user"), empty_login];
        let bad_reaction = reaction(11, 3, "evil");
        doc.reactions = vec![
            reaction(12, 3, "approved"),
            bad_reaction,
            reaction(13, 999, "approved"),
        ];
        let report = repo.restore(doc).await.unwrap();
        // Comments: 1 valid + invalid parent + orphaned child.
        assert_eq!(report.comments_imported, 1);
        assert_eq!(report.comments_skipped, 2);
        // Seen: 1 valid + 1 bad status.
        assert_eq!(report.webmention_seen_imported, 1);
        assert_eq!(report.webmention_seen_skipped, 1);
        // URLs: 1 valid + 1 bad scheme + 1 orphaned (comment 999 missing).
        assert_eq!(report.comment_urls_imported, 1);
        assert_eq!(report.comment_urls_skipped, 2);
        // Profiles: 1 valid + 1 empty login.
        assert_eq!(report.github_profiles_imported, 1);
        assert_eq!(report.github_profiles_skipped, 1);
        // Reactions: 1 valid + 1 bad status + 1 orphaned.
        assert_eq!(report.comment_reactions_imported, 1);
        assert_eq!(report.comment_reactions_skipped, 2);
        assert!(report.warning.is_none());
    }

    #[tokio::test]
    async fn restored_statuses_are_historical_with_zero_sink_events() {
        // (d): statuses replay verbatim — including `deleted`, which no live
        // transition from nothing could produce — with no webhook emission
        // and no CAS. A spam→approved re-import is a replay, not an override.
        // (No-emission is pinned by the wiremock import test in
        // `http::admin::data`, which observes the real webhook path; restore
        // takes no sink, so a local counting sink here would assert nothing.)
        let (repo, _dir) = setup_repo();
        let mut doc = input();
        doc.comments = vec![
            comment(1, "approved"),
            comment(2, "spam"),
            comment(3, "deleted"),
        ];
        doc.reactions = vec![reaction(4, 1, "approved")];
        repo.restore(doc).await.unwrap();
        assert_eq!(
            repo.get_comment(1).await.unwrap().unwrap().status,
            "approved"
        );
        assert_eq!(repo.get_comment(2).await.unwrap().unwrap().status, "spam");
        assert_eq!(
            repo.get_comment(3).await.unwrap().unwrap().status,
            "deleted"
        );
        assert_eq!(
            repo.get_reaction(4).await.unwrap().unwrap().status,
            "approved"
        );
        // Replay moves spam→approved without any transition validity gate.
        // (The statuses differ from the stored rows, so this second replay
        // of an *updated* export carries the explicit force flag — the same
        // guard that protects live decisions.)
        let mut doc2 = input();
        doc2.force = true;
        doc2.comments = vec![
            repo.get_comment(1).await.unwrap().unwrap(),
            {
                let mut c = repo.get_comment(2).await.unwrap().unwrap();
                c.status = "approved".to_string();
                c
            },
            repo.get_comment(3).await.unwrap().unwrap(),
        ];
        repo.restore(doc2).await.unwrap();
        assert_eq!(
            repo.get_comment(2).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn reimport_heals_lost_url_rows() {
        // B-24: a crash between delete and insert (simulated by deleting a
        // row) is healed by re-importing the same document.
        let (repo, _dir) = setup_repo();
        let mut doc = input();
        doc.comments = vec![comment(1, "approved")];
        doc.urls = vec![url(1, 1), url(2, 1)];
        let report = repo.restore(doc.clone()).await.unwrap();
        assert_eq!(report.comment_urls_imported, 2);
        repo.delete_urls_for_comment(1).await.unwrap();
        assert!(repo.get_comment_urls(1).await.unwrap().is_empty());
        let report = repo.restore(doc).await.unwrap();
        assert_eq!(report.comment_urls_imported, 2);
        assert_eq!(repo.get_comment_urls(1).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn export_snapshot_consistent_under_concurrent_insert() {
        // B-22: every snapshot is one WAL read transaction — a comment
        // created mid-export cannot appear in only some arrays.
        let (repo, _dir) = setup_repo();
        let mut seed = input();
        seed.comments = vec![comment(1, "approved")];
        seed.urls = vec![url(1, 1)];
        seed.reactions = vec![reaction(1, 1, "approved")];
        repo.restore(seed).await.unwrap();
        let writer = repo.clone();
        let writer_done = tokio::spawn(async move {
            for i in 100..120 {
                let mut doc = RestoreInput {
                    max_content_len: 2000,
                    ..RestoreInput::default()
                };
                let mut c = comment(i, "approved");
                c.target_path = "/t17-traffic".to_string();
                doc.comments = vec![c];
                doc.urls = vec![url(i, i)];
                doc.reactions = vec![reaction(i, i, "approved")];
                // Best-effort traffic: a Busy abort is fine, the reader's
                // snapshots must still be self-consistent.
                let _ = writer.restore(doc).await;
            }
        });
        for _ in 0..20 {
            let snap = repo.export_snapshot().await.unwrap();
            let comment_ids: std::collections::HashSet<i64> =
                snap.comments.iter().map(|c| c.id).collect();
            for u in &snap.urls {
                assert!(
                    comment_ids.contains(&u.comment_id),
                    "torn snapshot: URL for missing comment {}",
                    u.comment_id
                );
            }
            for r in &snap.reactions {
                assert!(
                    comment_ids.contains(&r.comment_id),
                    "torn snapshot: reaction for missing comment {}",
                    r.comment_id
                );
            }
        }
        writer_done.await.unwrap();
        assert!(
            repo.get_comment(1).await.unwrap().is_some(),
            "seed row survives concurrent traffic"
        );
    }

    #[tokio::test]
    async fn salt_mismatch_recomputes_hash_and_warns() {
        let (repo, _dir) = setup_repo();
        // Export claims salted; this server has no secret. Raw IP heals the
        // row (re-derived unsalted beside `hash_ip`); the warning fires.
        let mut c = comment(11, "approved");
        c.submitter_ip = Some("9.9.9.9".to_string());
        c.submitter_ip_hash = Some("h:stale".to_string());
        let mut doc = input();
        doc.comments = vec![c];
        doc.export_salted = Some(true);
        let report = repo.restore(doc).await.unwrap();
        assert_eq!(report.comments_imported, 1);
        assert_eq!(report.ip_hashes_recomputed, 1);
        assert!(report.warning.as_deref().unwrap().contains("mismatch"));
        let stored = repo.get_comment(11).await.unwrap().unwrap();
        let expected =
            crate::ip_hash::hash_ip(&"9.9.9.9".parse::<std::net::IpAddr>().unwrap(), None);
        assert_eq!(stored.submitter_ip_hash.as_deref(), Some(expected.as_str()));
    }

    #[tokio::test]
    async fn matching_salt_has_no_warning() {
        let (repo, _dir) = setup_repo();
        let fresh = crate::ip_hash::hash_ip(&"9.9.9.9".parse::<std::net::IpAddr>().unwrap(), None);
        let mut c = comment(12, "approved");
        c.submitter_ip = Some("9.9.9.9".to_string());
        c.submitter_ip_hash = Some(fresh);
        let mut doc = input();
        doc.comments = vec![c];
        doc.export_salted = Some(false);
        let report = repo.restore(doc).await.unwrap();
        assert_eq!(report.ip_hashes_recomputed, 0);
        assert!(report.warning.is_none());
    }

    #[tokio::test]
    async fn parent_before_child_ordering_survives_shuffled_export() {
        // Children sort after parents even when the export lists them first;
        // depth clamps instead of rejecting.
        let (repo, _dir) = setup_repo();
        let mut child = comment(6, "approved");
        child.parent_id = Some(5);
        child.depth = 999;
        let mut doc = input();
        doc.comments = vec![child, comment(5, "approved")];
        let report = repo.restore(doc).await.unwrap();
        assert_eq!(report.comments_imported, 2);
        assert_eq!(report.comments_skipped, 0);
        let stored = repo.get_comment(6).await.unwrap().unwrap();
        assert_eq!(stored.parent_id, Some(5));
        assert_eq!(stored.depth, 10, "depth clamps, not rejects");
    }

    // ── Moved import-validation unit tests (were data.rs handler tests) ──

    fn sample() -> Comment {
        comment(42, "approved")
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
        let valid = reaction(1, 2, "approved");
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

    #[test]
    fn seen_url_and_profile_validation() {
        assert!(
            validate_imported_seen(&seen(
                "https://s.example/a",
                "https://site.example/a",
                "alive"
            ))
            .is_ok()
        );
        assert!(
            validate_imported_seen(&seen(
                "https://s.example/a",
                "https://site.example/a",
                "bogus"
            ))
            .is_err()
        );
        assert!(
            validate_imported_seen(&seen("not-a-url", "https://site.example/a", "alive")).is_err()
        );
        assert!(validate_imported_url(&url(1, 1)).is_ok());
        let mut bad = url(1, 1);
        bad.url = "ftp://u17.example/nope".to_string();
        assert!(validate_imported_url(&bad).is_err());
        let bad = url(1, 0);
        assert!(validate_imported_url(&bad).is_err());
        assert!(validate_imported_profile(&profile("t17user")).is_ok());
        assert!(validate_imported_profile(&profile("")).is_err());
        let mut bad = profile("t17user");
        bad.avatar_url = "javascript:alert(1)".to_string();
        assert!(validate_imported_profile(&bad).is_err());
        // Negative-cache rows carry an empty avatar URL.
        let mut neg = profile("ghost999");
        neg.avatar_url = String::new();
        neg.valid = false;
        assert!(validate_imported_profile(&neg).is_ok());
    }

    // ── G4 minors (red-first) ──

    #[tokio::test]
    async fn mid_restore_busy_abort_carries_counts_so_far() {
        // G4.1: force SQLITE_BUSY mid-restore by holding an uncommitted
        // IMMEDIATE write on a second pool connection. Pre-flight reads
        // still pass (SHARED vs RESERVED); the first comment write blocks
        // until busy_timeout (~5 s) then aborts carrying counts-so-far.
        // Slow by construction.
        let dir = tempfile::tempdir().unwrap();
        let pool = create_pool(&dir.path().join("t17_busy.db").to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        let repo = Repo::new(pool.clone());
        let guard = pool.get().unwrap();
        guard.execute_batch("BEGIN IMMEDIATE").unwrap();
        guard
            .execute(
                "INSERT INTO comments (target_path, comment_type, author_name, content)
                 VALUES ('/t17-lock', 'native', 'Lock', 'holder')",
                [],
            )
            .unwrap();
        let mut doc = input();
        doc.comments = vec![comment(1, "evil-status"), comment(2, "approved")];
        let err = repo.restore(doc).await.unwrap_err();
        guard.execute_batch("ROLLBACK").unwrap();
        match err {
            RestoreError::Aborted { source, partial } => {
                assert!(
                    matches!(source, RepoError::Busy(_)),
                    "expected Busy, got {source:?}"
                );
                assert_eq!(partial.comments_imported, 0);
                assert_eq!(partial.comments_skipped, 1);
            }
            other => panic!("expected an abort carrying counts, got {other:?}"),
        }
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "max_content_len")]
    async fn restore_zero_max_content_len_panics_in_debug() {
        // G4.2: `..Default::default()` with an unset max_content_len would
        // sanitize every restored comment to empty — fail loud in debug.
        let (repo, _dir) = setup_repo();
        let _ = repo.restore(RestoreInput::default()).await;
    }

    #[tokio::test]
    async fn preflight_compares_normalized_rows_allows_reimport_after_clamp() {
        // G4.3: depth 999 clamps to 10 on write; the pre-flight must compare
        // the normalized row, not the raw export — otherwise re-importing the
        // same document refuses itself.
        let (repo, _dir) = setup_repo();
        let mut c = comment(5, "approved");
        c.depth = 999;
        let mut doc = input();
        doc.comments = vec![c];
        let first = repo.restore(doc.clone()).await.unwrap();
        assert_eq!(first.comments_imported, 1);
        assert_eq!(repo.get_comment(5).await.unwrap().unwrap().depth, 10);
        let second = repo.restore(doc).await.unwrap();
        assert_eq!(second.comments_imported, 1);
        assert_eq!(second.comments_skipped, 0);
    }

    #[tokio::test]
    async fn preflight_ignores_invalid_rows_that_write_would_skip() {
        // An invalid export row can never collide: the write path skips it,
        // so the pre-flight must not refuse on it either.
        let (repo, _dir) = setup_repo();
        let id = repo
            .insert_comment(crate::db::NewComment {
                target_path: "/live".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Live".to_string(),
                author_url: None,
                author_avatar: None,
                content: "live".to_string(),
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
        let mut clash = comment(id, "evil-status");
        clash.content = "whatever".to_string();
        let mut doc = input();
        doc.comments = vec![clash];
        let report = repo.restore(doc).await.unwrap();
        assert_eq!(report.comments_imported, 0);
        assert_eq!(report.comments_skipped, 1);
        assert_eq!(repo.get_comment(id).await.unwrap().unwrap().content, "live");
    }
}
