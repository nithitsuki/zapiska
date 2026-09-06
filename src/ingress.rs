//! CommentIngress — the native submission pipeline as one deep module.
//!
//! Placement: crate root (`crate::ingress`), NOT inside `http::comment_post`.
//! The pipeline is a domain concept (ordering of trust checks, the 13-step
//! native flow) with HTTP as just one adapter: the handler maps form →
//! [`SubmitRequest`], builds a [`SubmitCtx`], and awaits
//! [`Ingress::submit`]. Storage (`Repo`), identity rules, and the T16 sink
//! all live at this level too, so depending on them never inverts toward
//! HTTP. Same precedent as `crate::moderation` (T16) and `crate::identity`.
//!
//! Ordering (owns exactly this; step comments here are the contract, SPEC
//! mirrors them truthfully):
//! 1. Honeypot flag (reads `config.honeypot_field`, fallback `website`).
//! 2. Turnstile verify when enabled (fail-closed).
//! 3. Per-IP daily cap (honeypot-flagged submissions consume quota too).
//! 4. Validate target path, author identity (shared [`crate::identity`]
//!    rules), author URL, `github_username` shape.
//! 5. Content hash on the RAW input (moderation lookup key).
//! 6. Sanitize + truncate (ammonia).
//! 7. Language gate on the SANITIZED text.
//! 8. Author resolve (GitHub enrichment).
//! 9. Avatar resolve (best-effort, SafeFetcher door for page fetches).
//! 10. Parent check (threading rules).
//! 11. Delete token (128-bit CSPRNG) + peer IP/hash.
//! 12. Store via the T15 unit (`create_native_comment`: row + status + URL
//!     rows in ONE commit). URL extraction reads the SANITIZED content, so
//!     URLs inside tags ammonia strips never become rows (B12 fix).
//! 13. Effects: [`Notify`] then the T16 [`ModerationSink`] through its ONE
//!     shared `deliver` adapter (sync awaits the decision, async emits).
//!
//! NO IDEMPOTENCY KEYS (B9, deliberate): two concurrent identical POSTs
//! store two rows. `content_hash` is a lookup key for the moderation engine
//! to dedup post-hoc, never a uniqueness constraint — clients that retry
//! (double-click, flaky networks) accept duplicates rather than the server
//! holding client-supplied request IDs and their GC semantics.
//!
//! Follow-ups (NOT fixed here): language-gate quarantine tier (B6 — the gate
//! stays a hard block) and Unicode body-limit parity (B7 — the ~680-emoji
//! math stays documented, not fixed).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use url::Url;

use crate::config::Config;
use crate::db::repo::{NewComment, Repo};
use crate::error::AppError;
use crate::github::GitHubLookup;
use crate::identity;
use crate::language::LanguageGate;
use crate::moderation::{CommentCreated, ModerationSink, Status, comment_created_payload};
use crate::notify::{NewCommentInfo, NotificationBatcher};
use crate::state::Limiter;
use crate::validate::ValidationError;

// ── Effect seams ──────────────────────────────────────────────

/// Notification effect. The real adapter wraps the T18 batcher; tests use
/// an in-memory fake. Real + fake behind one trait = the seam is real.
pub trait Notify: Send + Sync {
    fn notify(&self, info: NewCommentInfo);
}

/// Real adapter: the T18 `NotificationBatcher::push` (stable signature —
/// consumed, not changed).
pub struct BatcherNotify<'a> {
    pub client: &'a reqwest::Client,
    pub batcher: &'a Arc<NotificationBatcher>,
}

impl Notify for BatcherNotify<'_> {
    fn notify(&self, info: NewCommentInfo) {
        self.batcher.push(self.client, info);
    }
}

/// URL-extraction effect. Persistence rides the T15 unit (extraction output
/// is an argument to `create_native_comment`, committed atomically), so the
/// separable effect is WHAT text gets scanned: always the sanitized content.
/// Tests use a recording fake to pin that invariant.
pub trait UrlStore: Send + Sync {
    fn extract(&self, sanitized_content: &str) -> Vec<(String, String, String)>;
}

/// Real adapter: the shared extractor over sanitized HTML.
pub struct RealUrlStore;

impl UrlStore for RealUrlStore {
    fn extract(&self, sanitized_content: &str) -> Vec<(String, String, String)> {
        crate::sanitize::extract_urls(sanitized_content)
    }
}

// ── Request / context ─────────────────────────────────────────

/// One native submission, already mapped out of HTTP types. `extra_fields`
/// carries the flattened form map so the CONFIGURED honeypot name resolves
/// even though serde cannot name a dynamic field.
pub struct SubmitRequest {
    pub target_path: String,
    pub author_name: String,
    pub author_url: Option<String>,
    pub github_username: Option<String>,
    pub content: String,
    pub parent_id: Option<i64>,
    /// Legacy `website` field value (also the default honeypot name).
    pub website: Option<String>,
    /// All other form fields (holds the configured honeypot value when the
    /// operator renames it away from `website`).
    pub extra_fields: HashMap<String, String>,
    /// Cloudflare Turnstile token (`cf-turnstile-response`).
    pub turnstile_token: Option<String>,
}

/// Everything `submit` needs, borrowed. Effect fakes slot in through
/// `notify` / `urls` / `moderation_sink`; everything else is the real seam
/// (repo unit, GitHub trait object, compiled gate, limiter).
pub struct SubmitCtx<'a> {
    pub config: &'a Config,
    pub repo: &'a Repo,
    pub github: &'a Arc<dyn GitHubLookup>,
    pub language: &'a LanguageGate,
    pub limiter: &'a Limiter,
    pub http_client: &'a reqwest::Client,
    pub peer_ip: IpAddr,
    pub peer_limiter_key: String,
    pub notify: &'a dyn Notify,
    pub urls: &'a dyn UrlStore,
    /// `Some` exactly when a moderation webhook URL is configured (the
    /// handler builds the signed T16 sink); `None` skips step 13b.
    pub moderation_sink: Option<&'a dyn ModerationSink>,
    pub moderation_is_sync: bool,
}

/// Stored outcome: the row id, its self-delete secret, and the status after
/// any sync moderation decision.
#[derive(Debug)]
pub struct StoredComment {
    pub id: i64,
    pub delete_token: String,
    pub status: String,
}

// ── Honeypot (S1) ─────────────────────────────────────────────

/// Resolve the configured honeypot field (`config.honeypot_field`, fallback
/// `website` when blank) against the submission. When the operator renames
/// the field, the legacy `website` value is INERT — filling it does not
/// flag. The widget emits the configured name via server-side substitution
/// into the served JS (see `comments_js`), so honest browsers always send
/// the right field; only bots guessing the old name slip through unflagged,
/// which is the documented cost of renaming.
#[must_use]
pub fn honeypot_filled(
    website: Option<&str>,
    extra: &HashMap<String, String>,
    honeypot_field: &str,
) -> bool {
    let field = {
        let f = honeypot_field.trim();
        if f.is_empty() { "website" } else { f }
    };
    let value = if field == "website" {
        website.unwrap_or("")
    } else {
        extra.get(field).map(String::as_str).unwrap_or("")
    };
    !value.trim().is_empty()
}

// ── Delete tokens (S3) ────────────────────────────────────────

/// Generate a 128-bit self-delete secret (32 lowercase hex chars) from the
/// OS CSPRNG via [`getrandom`] (portable: backed by BCrypt on Windows,
/// SecRandomCopyBytes on macOS, getrandom(2) on Linux — the old
/// `/dev/urandom` read fired its weak-token fallback on EVERY non-Linux
/// release target). Takes NO inputs — the token is not derived from the
/// peer address, time, or any counter, so it cannot be cracked offline by
/// narrowing an (ip, time) window (B3 fix). Uniqueness is statistical
/// (2^128); the delete route stays rate-limited with same-404 semantics as
/// defense in depth.
///
/// Fail-closed: an OS RNG failure aborts the submission with a 500 — the
/// server NEVER mints a weak token. There is no secondary entropy source by
/// design.
pub fn generate_delete_token() -> Result<String, AppError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| {
        AppError::Internal(format!(
            "OS random source unavailable, refusing to mint token: {e}"
        ))
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// ── Ingress ───────────────────────────────────────────────────

/// The submission pipeline. See the module docs for the owned ordering.
pub struct Ingress;

impl Ingress {
    #[allow(clippy::too_many_lines)]
    pub async fn submit(
        req: SubmitRequest,
        ctx: &SubmitCtx<'_>,
    ) -> Result<StoredComment, AppError> {
        // 1. Honeypot flag.
        let is_honeypot = honeypot_filled(
            req.website.as_deref(),
            &req.extra_fields,
            &ctx.config.honeypot_field,
        );
        if is_honeypot {
            tracing::info!(ip = %ctx.peer_ip, "honeypot triggered, comment flagged");
        }

        // 2. Turnstile verification (optional, fail-closed).
        if ctx.config.turnstile_enabled {
            let token = req.turnstile_token.as_deref().unwrap_or("").trim();
            if token.is_empty() {
                return Err(AppError::TurnstileFailed(
                    "turnstile token missing".to_string(),
                ));
            }
            let secret = ctx.config.turnstile_secret_key.as_deref().expect(
                "turnstile_enabled implies turnstile_secret_key is Some (enforced in Config::from_env)",
            );
            match crate::turnstile::verify(
                ctx.http_client,
                &ctx.config.turnstile_verify_url,
                secret,
                token,
                Some(&ctx.peer_ip),
            )
            .await
            {
                Ok(result) if result.success => {
                    tracing::debug!(ip = %ctx.peer_ip, "turnstile verification passed");
                }
                Ok(result) => {
                    tracing::info!(
                        ip = %ctx.peer_ip,
                        codes = ?result.error_codes,
                        "turnstile verification rejected token"
                    );
                    return Err(AppError::TurnstileFailed(
                        "turnstile verification failed".to_string(),
                    ));
                }
                Err(e) => {
                    tracing::warn!(err = %e, "turnstile siteverify request failed");
                    return Err(AppError::ServiceUnavailable(
                        "turnstile verification unavailable".to_string(),
                    ));
                }
            }
        }

        // 3. Per-IP daily cap (before validation: garbage consumes budget —
        // B15 documented cost; Turnstile failures above do NOT consume it).
        if ctx.config.max_comments_per_ip_per_day > 0
            && !ctx.limiter.check_and_increment(
                &ctx.peer_limiter_key,
                ctx.config.max_comments_per_ip_per_day,
            )
        {
            return Err(AppError::RateLimited {
                retry_after_secs: 86400,
                reason: format!(
                    "daily comment limit ({}) reached for this IP",
                    ctx.config.max_comments_per_ip_per_day
                ),
            });
        }

        // Treat empty optional fields as None.
        let author_url = req.author_url.filter(|u| !u.trim().is_empty());
        let github_username = req.github_username.filter(|u| !u.trim().is_empty());

        // 4. Validate (shared identity rules — native + import parity).
        crate::validate::validate_target_path(&req.target_path)
            .map_err(|e| AppError::BadRequest(format!("invalid target_path: {e}")))?;
        let author_name = identity::native_author_name(
            &req.author_name,
            github_username.as_deref(),
            ctx.config.max_author_len,
        )
        .map_err(|e| match &e {
            // Historical message spelling for the length reject.
            ValidationError::TooLong { max, .. } => {
                AppError::BadRequest(format!("author_name exceeds max length of {max} chars"))
            }
            _ => AppError::BadRequest(e.to_string()),
        })?;
        if let Some(ref url) = author_url {
            crate::validate::validate_http_url(url)
                .map_err(|e| AppError::BadRequest(format!("invalid author_url: {e}")))?;
        }
        if let Some(ref gh) = github_username {
            // S2: shape-check BEFORE interpolating into github.com URLs.
            identity::check_github_username(gh)
                .map_err(|e| AppError::BadRequest(format!("invalid github_username: {e}")))?;
        }

        // 5. Content hash on the RAW input (moderation lookup key; never a
        // uniqueness constraint — see the no-idempotency note above).
        let content_hash = Some(crate::sanitize::content_hash(&req.content));

        // 6. Sanitize + truncate.
        let content = crate::sanitize::sanitize_html(&req.content, ctx.config.max_content_len);

        // 7. Language gate on the SANITIZED text (hard block; B6 quarantine
        // tier stays a follow-up).
        if ctx.language.is_enabled() {
            if let Err(e) = ctx.language.check(&content) {
                return Err(AppError::BadRequest(format!("comment rejected: {e}")));
            }
        }

        // 8. Resolve author info.
        let (resolved_name, resolved_url) = resolve_author(
            author_url.as_deref(),
            github_username.as_deref(),
            author_name,
            ctx.github,
        )
        .await;

        // 9. Resolve avatar (best-effort).
        let resolved_avatar = resolve_avatar(
            &resolved_url,
            author_url.as_deref(),
            github_username.as_deref(),
            ctx.github,
            #[cfg(feature = "webmentions")]
            ctx.http_client,
        )
        .await;

        // 10. Resolve parent for nesting.
        let (parent_id, depth) = resolve_parent(
            &req.parent_id,
            &req.target_path,
            ctx.repo,
            ctx.config.max_thread_depth,
        )
        .await?;

        // 11. Delete token (CSPRNG, fail-closed) + peer IP/hash.
        let delete_token = generate_delete_token()?;
        let delete_token_str = delete_token.clone();
        let (submitter_ip, submitter_ip_hash) = if ctx.config.store_ip_address {
            let raw = ctx.peer_ip.to_string();
            let hash = crate::ip_hash::hash_ip(&ctx.peer_ip, ctx.config.ip_hash_secret.as_deref());
            (Some(raw), Some(hash))
        } else {
            (None, None)
        };

        // 12. Store row + status + URL rows in ONE T15 commit. URLs extract
        // from the SANITIZED content (B12 fix): tags ammonia strips
        // contribute no rows.
        let hook_content = content.clone();
        let hook_name = resolved_name.clone();
        let hook_url = resolved_url.clone();
        let hook_avatar = resolved_avatar.clone();
        let hook_ip = submitter_ip.clone();
        let hook_content_hash = content_hash.clone();
        let extracted_urls = ctx.urls.extract(&content);
        let auto_approve = ctx.config.default_comment_status == "approved";
        let new_id = ctx
            .repo
            .create_native_comment(
                NewComment {
                    target_path: req.target_path.clone(),
                    comment_type: "native".to_string(),
                    source_url: None,
                    author_name: resolved_name,
                    author_url: resolved_url,
                    author_avatar: resolved_avatar,
                    content,
                    parent_id,
                    depth,
                    honeypot: is_honeypot,
                    delete_token: Some(delete_token),
                    submitter_ip,
                    submitter_ip_hash,
                    content_hash,
                },
                auto_approve,
                extracted_urls,
            )
            .await?;

        // 13a. Notify admin channels (fire-and-forget; never fails submit).
        ctx.notify.notify(NewCommentInfo {
            id: new_id,
            target_path: req.target_path.clone(),
            comment_type: "native".to_string(),
            author_name: hook_name.clone(),
            author_url: hook_url.clone(),
            content: hook_content.clone(),
            honeypot: is_honeypot,
            is_reply: parent_id.is_some(),
        });

        // 13b. Moderation webhook through the ONE shared T16 adapter (sync
        // awaits the decision, async emits). Sync decisions apply on the
        // plain write path (no machine transition, no extra event) — folding
        // them into `Moderation::transition` stays a follow-up.
        let mut final_status = ctx.config.default_comment_status.clone();
        if let Some(sink) = ctx.moderation_sink {
            let submitter_stats = if let Some(ref ip) = hook_ip {
                ctx.repo.submitter_stats(ip).await.ok()
            } else {
                None
            };
            let parent_chain = if parent_id.is_some() {
                ctx.repo.get_comment_chain(new_id).await.ok().flatten()
            } else {
                None
            };
            let (total, approved, spam, pending, deleted, first_seen) =
                submitter_stats.unwrap_or((0, 0, 0, 0, 0, None));
            let parents = parent_chain.map(|(_, chain)| {
                chain
                    .into_iter()
                    .map(|p| {
                        serde_json::json!({
                            "id": p.id, "author_name": p.author_name, "content": p.content, "depth": p.depth,
                        })
                    })
                    .collect::<Vec<_>>()
            });
            let payload = comment_created_payload(
                &CommentCreated {
                    id: new_id,
                    target_path: &req.target_path,
                    comment_type: "native",
                    author_name: &hook_name,
                    author_url: hook_url.as_deref(),
                    author_avatar: hook_avatar.as_deref(),
                    content: &hook_content,
                    honeypot: is_honeypot,
                    parent_id,
                    depth,
                    submitter_ip: hook_ip.as_deref(),
                    delete_token: &delete_token_str,
                    content_hash: hook_content_hash.as_deref(),
                    is_reply: parent_id.is_some(),
                    parents,
                    submitter_total: total,
                    submitter_approved: approved,
                    submitter_spam: spam,
                    submitter_pending: pending,
                    submitter_deleted: deleted,
                    submitter_first_seen: first_seen.as_deref(),
                },
                &format!("/api/admin/comments/{new_id}"),
            );
            if let Some(decision) = sink.deliver(&payload, ctx.moderation_is_sync).await {
                let _ = ctx.repo.update_status(new_id, decision.as_str()).await;
                final_status = decision.to_string();
            }
        }

        tracing::debug!(id = new_id, status = %final_status, "comment stored");

        Ok(StoredComment {
            id: new_id,
            delete_token: delete_token_str,
            status: final_status,
        })
    }
}

/// Validate and resolve the parent_id for a new comment.
/// Returns (parent_id_to_store, computed_depth).
async fn resolve_parent(
    form_parent_id: &Option<i64>,
    target_path: &str,
    repo: &Repo,
    max_depth: i64,
) -> Result<(Option<i64>, i64), AppError> {
    let Some(pid) = form_parent_id else {
        return Ok((None, 0));
    };
    let pid = *pid;

    if max_depth == 0 {
        return Err(AppError::BadRequest(
            "threaded replies are disabled on this server".to_string(),
        ));
    }

    let parent = repo
        .get_comment(pid)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("parent comment {pid} not found")))?;

    if parent.status != Status::Approved.as_str() {
        return Err(AppError::BadRequest(format!(
            "parent comment {pid} is not approved (status: {})",
            parent.status
        )));
    }

    if parent.target_path != target_path {
        return Err(AppError::BadRequest(format!(
            "parent comment {pid} belongs to a different page ('{}'), not '{target_path}'",
            parent.target_path
        )));
    }

    if parent.depth >= max_depth {
        return Err(AppError::BadRequest(format!(
            "nesting depth exceeded: parent comment {pid} is at depth {}, max allowed is {max_depth}",
            parent.depth
        )));
    }

    let depth = parent.depth + 1;
    Ok((Some(pid), depth))
}

async fn resolve_author(
    author_url: Option<&str>,
    github_username: Option<&str>,
    cleaned_name: String,
    github: &Arc<dyn GitHubLookup>,
) -> (String, Option<String>) {
    // Priority 1: user provided their own website — always use it.
    if let Some(url) = author_url {
        return (cleaned_name, Some(url.to_string()));
    }

    // Priority 2: no author_url, but github_username given — derive URL.
    // (Shape pre-validated by the caller; trim once here.)
    if let Some(gh) = github_username {
        let gh = gh.trim();
        if !gh.is_empty() {
            let github_url = Some(format!("https://github.com/{gh}"));
            if cleaned_name.is_empty() {
                if let Some(profile) = github.lookup(gh).await {
                    return (profile.name, github_url);
                }
                return (gh.to_string(), github_url);
            }
            return (cleaned_name, github_url);
        }
    }

    // Priority 3: name only, no enrichment.
    (cleaned_name, None)
}

/// Resolve a profile picture URL. Tries multiple strategies in priority order:
///
/// 1. GitHub API avatar (if `author_url` is a github.com URL)
/// 2. (webmentions feature) h-card photo + favicon from author's page
/// 3. GitHub API avatar (if `github_username` was provided)
/// 4. DiceBear generated avatar from author URL domain
/// 5. DiceBear generated avatar from GitHub username
/// 6. DiceBear from a generic seed (absolute last resort)
async fn resolve_avatar(
    resolved_url: &Option<String>,
    raw_author_url: Option<&str>,
    github_username: Option<&str>,
    github: &Arc<dyn GitHubLookup>,
    #[cfg(feature = "webmentions")] http_client: &reqwest::Client,
) -> Option<String> {
    // Priority 1: If author_url is a GitHub profile, get avatar via API.
    if let Some(url) = resolved_url {
        if let Some(username) = crate::github::extract_github_username(url) {
            if let Some(profile) = github.lookup(&username).await {
                return Some(profile.avatar_url);
            }
        }
    }

    // Priority 2: Fetch the author's page and try h-card photo + favicon.
    #[cfg(feature = "webmentions")]
    if let Some(url) = resolved_url {
        if let Ok(parsed) = Url::parse(url) {
            let avatar = fetch_page_avatar(http_client, &parsed).await;
            if avatar.is_some() {
                return avatar;
            }
        }
    }

    // Priority 3: GitHub avatar from `github_username` form field.
    if let Some(gh) = github_username {
        let gh = gh.trim();
        if !gh.is_empty() {
            if let Some(profile) = github.lookup(gh).await {
                return Some(profile.avatar_url);
            }
        }
    }

    // Priority 4: DiceBear generated avatar from the author URL domain.
    if let Some(url) = raw_author_url {
        if let Ok(parsed) = Url::parse(url) {
            if let Some(domain) = parsed.host_str() {
                return Some(format!(
                    "https://api.dicebear.com/7.x/notionists/svg?seed={domain}"
                ));
            }
        }
    }

    // Priority 5: DiceBear from GitHub username.
    if let Some(gh) = github_username {
        let gh = gh.trim();
        if !gh.is_empty() {
            return Some(format!(
                "https://api.dicebear.com/7.x/notionists/svg?seed={gh}"
            ));
        }
    }

    // Priority 6: DiceBear from a generic seed (absolute last resort).
    Some("https://api.dicebear.com/7.x/notionists/svg?seed=anonymous".to_string())
}

/// Fetch a URL, parse the HTML, and try to extract an avatar.
/// Tries h-card photo first, then favicon.
///
/// The untrusted author URL goes through [`crate::fetch::SafeFetcher`] — the
/// same guarded door as webmention fetches — never a bare client. Any refusal
/// (blocked target, redirect loop, oversized body, network error) returns
/// `None` so the caller falls back to dicebear, like every other failure.
#[cfg(feature = "webmentions")]
async fn fetch_page_avatar(_http_client: &reqwest::Client, url: &Url) -> Option<String> {
    // NOTE: the shared client is deliberately unused here — SafeFetcher owns
    // its redirect-disabled client so per-hop SSRF checks cannot be skipped.
    let fetched = crate::fetch::SafeFetcher::new().fetch(url).await.ok()?;

    // Try h-card photo first (reads the single FetchedDoc parse tree).
    use crate::mf2;
    if let Some(parsed) = mf2::parse_h_entry(&fetched.doc) {
        if let Some(avatar) = parsed.author_avatar {
            if let Ok(abs) = url.join(&avatar) {
                return Some(abs.to_string());
            }
        }
        // Also check the author's u-photo directly.
        let text = fetched.text();
        if let Some(avatar) = mf2::extract_photo(&text, url) {
            return Some(avatar);
        }
    }

    // Fallback to favicon.
    let text = fetched.text();
    crate::avatar::best_favicon(&text, url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::db::pool::{create_pool, run_migrations};
    use crate::db::repo::Repo;
    use crate::github::StubGitHub;

    struct FakeNotify {
        infos: Mutex<Vec<NewCommentInfo>>,
    }

    impl FakeNotify {
        fn new() -> Self {
            Self {
                infos: Mutex::new(Vec::new()),
            }
        }

        fn count(&self) -> usize {
            self.infos.lock().expect("notify lock").len()
        }
    }

    impl Notify for FakeNotify {
        fn notify(&self, info: NewCommentInfo) {
            self.infos.lock().expect("notify lock").push(info);
        }
    }

    /// Recording extractor: proves WHAT text reaches extraction (the
    /// sanitized-content invariant) while delegating to the real scan.
    struct RecordExtractor {
        seen: Mutex<Vec<String>>,
    }

    impl RecordExtractor {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl UrlStore for RecordExtractor {
        fn extract(&self, sanitized_content: &str) -> Vec<(String, String, String)> {
            self.seen
                .lock()
                .expect("extractor lock")
                .push(sanitized_content.to_string());
            crate::sanitize::extract_urls(sanitized_content)
        }
    }

    struct StubSink {
        decision: Option<Status>,
        emitted: Mutex<Vec<serde_json::Value>>,
        decided: Mutex<Vec<serde_json::Value>>,
    }

    impl StubSink {
        fn with_decision(decision: Option<Status>) -> Self {
            Self {
                decision,
                emitted: Mutex::new(Vec::new()),
                decided: Mutex::new(Vec::new()),
            }
        }

        fn emitted(&self) -> Vec<serde_json::Value> {
            self.emitted.lock().expect("sink lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl ModerationSink for StubSink {
        fn emit(&self, payload: serde_json::Value) {
            self.emitted.lock().expect("sink lock").push(payload);
        }

        async fn decide(&self, payload: &serde_json::Value) -> Option<Status> {
            self.decided
                .lock()
                .expect("sink lock")
                .push(payload.clone());
            self.decision
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        config: Config,
        repo: Repo,
        github: Arc<dyn GitHubLookup>,
        language: LanguageGate,
        limiter: Limiter,
        client: reqwest::Client,
        notify: FakeNotify,
        urls: RealUrlStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("t19_ingress.db");
            let pool = create_pool(&path.to_string_lossy()).unwrap();
            run_migrations(&pool, None).unwrap();
            let config = Config {
                admin_token: "test".to_string(),
                ..Config::default()
            };
            let language = LanguageGate::new(&config);
            Self {
                _dir: dir,
                config,
                repo: Repo::new(pool),
                github: Arc::new(StubGitHub),
                language,
                limiter: Limiter::new(),
                client: reqwest::Client::new(),
                notify: FakeNotify::new(),
                urls: RealUrlStore,
            }
        }

        fn ctx<'s>(&'s self, sink: Option<&'s dyn ModerationSink>, is_sync: bool) -> SubmitCtx<'s> {
            SubmitCtx {
                config: &self.config,
                repo: &self.repo,
                github: &self.github,
                language: &self.language,
                limiter: &self.limiter,
                http_client: &self.client,
                peer_ip: "127.0.0.1".parse().unwrap(),
                peer_limiter_key: "ip:127.0.0.1:test".to_string(),
                notify: &self.notify,
                urls: &self.urls,
                moderation_sink: sink,
                moderation_is_sync: is_sync,
            }
        }

        fn request(&self, target: &str, author: &str, content: &str) -> SubmitRequest {
            SubmitRequest {
                target_path: target.to_string(),
                author_name: author.to_string(),
                author_url: None,
                github_username: None,
                content: content.to_string(),
                parent_id: None,
                website: None,
                extra_fields: HashMap::new(),
                turnstile_token: None,
            }
        }
    }

    // ── Ordering invariants ───────────────────────────────

    #[tokio::test]
    async fn content_hash_reflects_raw_input() {
        let f = Fixture::new();
        let raw = "<p>Hello World</p>";
        let stored = Ingress::submit(f.request("/t19-hash", "Ada", raw), &f.ctx(None, false))
            .await
            .unwrap();
        let row = f.repo.get_comment(stored.id).await.unwrap().unwrap();
        assert_eq!(
            row.content_hash.as_deref(),
            Some(crate::sanitize::content_hash(raw).as_str()),
            "hash must reflect the raw input, not the sanitized form"
        );
    }

    #[tokio::test]
    async fn language_gate_sees_sanitized_text() {
        // Japanese confined to a <script> block ammonia strips: the gate
        // must see the surviving English paragraph and pass under allow=en.
        // (Gate-on-raw would see the dominant Japanese and reject.)
        let mut f = Fixture::new();
        f.config.comment_lang_allowed = vec!["en".to_string()];
        f.language = LanguageGate::new(&f.config);
        let raw = "<script>これは日本語のコメントですこれは日本語のコメントですこれは日本語のコメントです</script><p>This is a perfectly normal English comment that should pass.</p>";
        let stored = Ingress::submit(f.request("/t19-gate", "Ada", raw), &f.ctx(None, false))
            .await
            .unwrap();
        let row = f.repo.get_comment(stored.id).await.unwrap().unwrap();
        assert!(
            !row.content.contains("日本語"),
            "script stripped before store"
        );
    }

    #[tokio::test]
    async fn url_rows_never_come_from_stripped_tags() {
        // A URL that exists ONLY inside a stripped <script> block must not
        // become a URL row; a rendered link must.
        let f = Fixture::new();
        let raw = "<script>var x = '<a href=\"https://ghost.example/p\">x</a>';</script><p><a href=\"https://real.example/p\">real</a></p>";
        let stored = Ingress::submit(f.request("/t19-urls", "Ada", raw), &f.ctx(None, false))
            .await
            .unwrap();
        let rows = f.repo.get_comment_urls(stored.id).await.unwrap();
        assert_eq!(rows.len(), 1, "only the rendered link survives: {rows:?}");
        assert_eq!(rows[0].url, "https://real.example/p");
        assert!(
            !rows.iter().any(|r| r.domain == "ghost.example"),
            "stripped-tag URL must never persist"
        );
    }

    #[tokio::test]
    async fn extractor_receives_sanitized_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t19_ext.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        let config = Config {
            admin_token: "test".to_string(),
            ..Config::default()
        };
        let language = LanguageGate::new(&config);
        let repo = Repo::new(pool);
        let github: Arc<dyn GitHubLookup> = Arc::new(StubGitHub);
        let limiter = Limiter::new();
        let client = reqwest::Client::new();
        let notify = FakeNotify::new();
        let recorder = RecordExtractor::new();
        let ctx = SubmitCtx {
            config: &config,
            repo: &repo,
            github: &github,
            language: &language,
            limiter: &limiter,
            http_client: &client,
            peer_ip: "127.0.0.1".parse().unwrap(),
            peer_limiter_key: "ip:127.0.0.1:extract".to_string(),
            notify: &notify,
            urls: &recorder,
            moderation_sink: None,
            moderation_is_sync: false,
        };
        let raw = "<script>alert(1)</script><p>hi</p>";
        Ingress::submit(
            SubmitRequest {
                target_path: "/t19-ext".to_string(),
                author_name: "Ada".to_string(),
                author_url: None,
                github_username: None,
                content: raw.to_string(),
                parent_id: None,
                website: None,
                extra_fields: HashMap::new(),
                turnstile_token: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        let seen = recorder.seen.lock().expect("lock").clone();
        assert_eq!(seen.len(), 1);
        assert!(
            !seen[0].contains("<script>"),
            "extraction input must be post-sanitize: {}",
            seen[0]
        );
    }

    // ── Honeypot (S1) ────────────────────────────────────

    #[tokio::test]
    async fn honeypot_field_honored_and_legacy_inert() {
        // RED (S1/B2): the old handler read `website` regardless of config.
        let mut f = Fixture::new();
        f.config.honeypot_field = "company".to_string();
        // Filling the CONFIGURED field flags…
        let mut req = f.request("/t19-hp-a", "Bot", "spam");
        req.extra_fields
            .insert("company".to_string(), "spam co".to_string());
        let stored = Ingress::submit(req, &f.ctx(None, false)).await.unwrap();
        let row = f.repo.get_comment(stored.id).await.unwrap().unwrap();
        assert!(row.honeypot, "configured field must flag");
        // …while the legacy `website` field is inert.
        let mut req = f.request("/t19-hp-b", "Bot", "spam");
        req.website = Some("spammer.example".to_string());
        let stored = Ingress::submit(req, &f.ctx(None, false)).await.unwrap();
        let row = f.repo.get_comment(stored.id).await.unwrap().unwrap();
        assert!(
            !row.honeypot,
            "legacy field must be inert under a custom name"
        );
    }

    #[tokio::test]
    async fn honeypot_default_still_flags_website() {
        let f = Fixture::new();
        assert_eq!(f.config.honeypot_field, "website");
        let mut req = f.request("/t19-hp-c", "Bot", "spam");
        req.website = Some("spammer.example".to_string());
        let stored = Ingress::submit(req, &f.ctx(None, false)).await.unwrap();
        assert!(
            f.repo
                .get_comment(stored.id)
                .await
                .unwrap()
                .unwrap()
                .honeypot
        );
    }

    #[test]
    fn honeypot_blank_config_falls_back_to_website() {
        assert!(honeypot_filled(Some("x"), &HashMap::new(), ""));
        assert!(honeypot_filled(Some("x"), &HashMap::new(), "   "));
        assert!(!honeypot_filled(Some(""), &HashMap::new(), ""));
    }

    // ── Identity (S2) at the ingress boundary ────────────

    #[tokio::test]
    async fn bidi_spoof_stripped_from_stored_name() {
        let f = Fixture::new();
        let stored = Ingress::submit(
            f.request("/t19-bidi", "ab\u{202E}cd\u{200B}", "hi"),
            &f.ctx(None, false),
        )
        .await
        .unwrap();
        assert_eq!(
            f.repo
                .get_comment(stored.id)
                .await
                .unwrap()
                .unwrap()
                .author_name,
            "abcd"
        );
    }

    #[tokio::test]
    async fn hostile_github_username_rejected_before_interpolation() {
        let f = Fixture::new();
        for bad in ["a<b>\n", "../x", "a b", "-lead", "trail-", "x--y"] {
            let mut req = f.request("/t19-gh", "Ada", "hi");
            req.github_username = Some(bad.to_string());
            let err = Ingress::submit(req, &f.ctx(None, false)).await.unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{bad:?} must be a 400, got {err:?}"
            );
        }
    }

    // ── Delete tokens (S3) ───────────────────────────────

    #[test]
    fn tokens_are_128bit_unique_and_hex() {
        // RED (S3/B3): the old 16-hex DefaultHasher token was deterministic
        // in (ip, time) and 64-bit.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let tok = generate_delete_token().unwrap();
            assert_eq!(tok.len(), 32, "128-bit hex: {tok}");
            assert!(
                tok.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert!(seen.insert(tok), "tokens must be unique");
        }
    }

    #[test]
    fn token_generation_has_no_weak_fallback_path() {
        // Failure-policy pin (G4 major): `getrandom` exposes no failure
        // injection, and the OS RNG cannot be failed from inside a test, so
        // a behavioral failure test is not practically writable. This
        // structure test pins the next best thing on the SLICED function
        // body (the file legitimately narrates the old bug elsewhere):
        // exactly one RNG call site (`getrandom::fill`), no `/dev/urandom`
        // read, no `DefaultHasher`, and a `Result` return so the compiler
        // enforces `?`-propagation to the 500 path. If a weak path is ever
        // reintroduced inside the generator, this fails.
        let src = include_str!("ingress.rs");
        let start = src
            .find("pub fn generate_delete_token")
            .expect("fn present");
        let tail = &src[start..];
        let end = tail.find("\n}\n").expect("fn end") + 3;
        let body = &tail[..end];
        assert_eq!(
            body.matches("getrandom::fill").count(),
            1,
            "exactly one RNG call site in the generator"
        );
        for banned in ["/dev/urandom", "DefaultHasher", "fallback"] {
            assert!(
                !body.contains(banned),
                "weak-token path must not exist in the generator, found: {banned}"
            );
        }
        assert!(
            body.contains("-> Result<String, AppError>"),
            "the generator must return Result so failures propagate to the 500 path"
        );
    }

    #[tokio::test]
    async fn sequential_submits_get_underivable_tokens() {
        // No inputs → nothing to derive from: two submits from the same peer
        // in the same instant still differ, and neither embeds the peer.
        let f = Fixture::new();
        let a = Ingress::submit(f.request("/t19-tok", "A", "one"), &f.ctx(None, false))
            .await
            .unwrap();
        let b = Ingress::submit(f.request("/t19-tok", "B", "two"), &f.ctx(None, false))
            .await
            .unwrap();
        assert_ne!(a.delete_token, b.delete_token);
        assert!(!a.delete_token.contains("127.0.0.1"));
    }

    // ── Effects through the shared sink ──────────────────

    #[tokio::test]
    async fn sync_decision_applies_and_async_emits() {
        let f = Fixture::new();
        let sync_sink = StubSink::with_decision(Some(Status::Approved));
        let stored = Ingress::submit(
            f.request("/t19-sync", "Ada", "hi"),
            &f.ctx(Some(&sync_sink), true),
        )
        .await
        .unwrap();
        assert_eq!(stored.status, "approved");
        assert_eq!(
            f.repo.get_comment(stored.id).await.unwrap().unwrap().status,
            "approved"
        );

        let async_sink = StubSink::with_decision(Some(Status::Approved));
        let stored = Ingress::submit(
            f.request("/t19-async", "Ada", "hi"),
            &f.ctx(Some(&async_sink), false),
        )
        .await
        .unwrap();
        assert_eq!(stored.status, "pending", "async never applies the decision");
        assert_eq!(async_sink.emitted().len(), 1, "async emits exactly once");
    }

    #[tokio::test]
    async fn notify_effect_fires_per_submit() {
        let f = Fixture::new();
        Ingress::submit(f.request("/t19-n", "Ada", "hi"), &f.ctx(None, false))
            .await
            .unwrap();
        assert_eq!(f.notify.count(), 1);
        let info = f.notify.infos.lock().expect("lock").clone();
        assert_eq!(info[0].author_name, "Ada");
    }

    // ── Double-submit documents no-idempotency (B9) ──────

    #[tokio::test]
    async fn double_submit_stores_twice_with_same_hash() {
        // Deliberate: no idempotency keys — the engine dedups post-hoc via
        // content_hash lookup. Pin the behavior so a future "fix" is a
        // conscious contract change, not an accident.
        let f = Fixture::new();
        let a = Ingress::submit(
            f.request("/t19-dup", "Ada", "same words"),
            &f.ctx(None, false),
        )
        .await
        .unwrap();
        let b = Ingress::submit(
            f.request("/t19-dup", "Ada", "same words"),
            &f.ctx(None, false),
        )
        .await
        .unwrap();
        assert_ne!(a.id, b.id, "no dedup on write");
        let (ra, rb) = (
            f.repo.get_comment(a.id).await.unwrap().unwrap(),
            f.repo.get_comment(b.id).await.unwrap().unwrap(),
        );
        assert_eq!(ra.content_hash, rb.content_hash);
    }
}
