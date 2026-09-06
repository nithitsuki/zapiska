use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::mpsc;
use url::Url;

use crate::db::repo::{NewComment, NewWebmentionSeen, Repo};
use crate::fetch::{DEFAULT_FETCH_TIMEOUT, FetchError, SafeFetcher};
use crate::github::{GitHubLookup, Profile};
use crate::mf2::{ParsedMention, has_backlink, parse_h_entry};
use crate::notify::{NewCommentInfo, NotificationBatcher};
use crate::sanitize;
use crate::ssrf::registrable_domain;

// ── Error type ──────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("invalid target URL: {0}")]
    InvalidTarget(String),
    #[error("target origin mismatch: {0}")]
    OriginMismatch(String),
    #[error("no backlink to target found")]
    NoBacklink,
    #[error("fetch failed: {0}")]
    Fetch(#[from] FetchError),
    #[error("repo error: {0}")]
    Repo(#[from] crate::db::RepoError),
}

// ─── Job types ──────────────────────────────────────────────

/// A job representing an incoming webmention ping to be processed.
#[derive(Debug, Clone)]
pub struct WebmentionJob {
    pub source: String,
    pub target: String,
}

pub type JobSender = mpsc::Sender<WebmentionJob>;
type JobReceiver = mpsc::Receiver<WebmentionJob>;

pub fn channel(buffer: usize) -> (JobSender, JobReceiver) {
    mpsc::channel(buffer)
}

/// Spawn a no-op worker that drains the channel (for tests / scaffolding).
pub fn spawn_worker(mut rx: JobReceiver) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            tracing::debug!(source = %job.source, target = %job.target, "no-op worker drained job");
        }
    });
}

/// Spawn the background worker that processes webmention jobs.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker_for_state(
    mut rx: JobReceiver,
    repo: Repo,
    client: Client,
    github: Arc<dyn GitHubLookup>,
    target_origin: String,
    max_content_len: usize,
    timeout_ms: u64,
    notifier: Arc<NotificationBatcher>,
) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            if let Err(e) = process_job_with_timeout(
                &job,
                &repo,
                &client,
                &github,
                &target_origin,
                max_content_len,
                false,
                Duration::from_millis(timeout_ms),
                &notifier,
            )
            .await
            {
                tracing::warn!(source = %job.source, target = %job.target, err = %e, "webmention worker error");
            }
        }
        tracing::warn!("webmention worker channel closed");
    });
}

// ── Public API ──────────────────────────────────────────────

/// Process a single webmention job. Exposed as `pub` so integration tests can
/// call it directly. `allow_loopback` relaxes the SSRF check for mock servers.
/// Uses the default fetch timeout; the spawned worker uses
/// [`process_job_with_timeout`] with the configured `FETCH_TIMEOUT_MS`.
#[allow(clippy::too_many_arguments)]
pub async fn process_job(
    job: &WebmentionJob,
    repo: &Repo,
    client: &Client,
    github: &Arc<dyn GitHubLookup>,
    target_origin: &str,
    max_content_len: usize,
    allow_loopback: bool,
    notifier: &Arc<NotificationBatcher>,
) -> Result<(), WorkerError> {
    process_job_with_timeout(
        job,
        repo,
        client,
        github,
        target_origin,
        max_content_len,
        allow_loopback,
        DEFAULT_FETCH_TIMEOUT,
        notifier,
    )
    .await
}

/// [`process_job`] with an explicit fetch timeout. The spawned worker passes
/// the configured timeout through here; tests pin non-default budgets here.
#[allow(clippy::too_many_arguments)]
pub async fn process_job_with_timeout(
    job: &WebmentionJob,
    repo: &Repo,
    client: &Client,
    github: &Arc<dyn GitHubLookup>,
    target_origin: &str,
    max_content_len: usize,
    allow_loopback: bool,
    fetch_timeout: Duration,
    notifier: &Arc<NotificationBatcher>,
) -> Result<(), WorkerError> {
    let target_path = derive_target_path(&job.target, target_origin)?;

    // 1. Check idempotency: if previously gone, delete the comment.
    let seen = repo.get_webmention_seen(&job.source, &job.target).await?;

    if seen.as_ref().is_some_and(|s| s.last_status == "gone")
        && let Ok(Some(comment)) = repo.get_comment_by_source(&job.source).await
    {
        if comment.status == "approved" || comment.status == "pending" {
            let _ = repo.update_status(comment.id, "deleted").await;
        }
        tracing::info!(
            source = %job.source, target = %job.target,
            "webmention previously gone, comment deleted if approved"
        );
        return Ok(());
    }

    // 2. SSRF-safe fetch of the source URL through the one guarded door
    // (per-hop checks, hop cap, streaming byte cap; parsed once).
    // `allow_loopback` stays plumbed for mock-server tests (T20 owns the
    // Processor refactor that removes it).
    // NOTE: scraper::Html is !Send: `map` collapses the document into Send
    // data (bool + parsed mention) with no await in between, so no !Send
    // value is ever live across a later await.
    let fetched = SafeFetcher::new()
        .with_allow_loopback(allow_loopback)
        .with_timeout(fetch_timeout)
        .fetch_str(&job.source)
        .await;
    let outcome = fetched.map(|doc| {
        // 3. Backlink check + h-entry parse read the single parse tree.
        (has_backlink(&doc.doc, &job.target), parse_h_entry(&doc.doc))
    });
    let (backlink_ok, parsed): (bool, Option<ParsedMention>) = match outcome {
        Ok(found) => found,
        Err(FetchError::Gone(_)) => {
            handle_gone_source(job, repo).await;
            return Ok(());
        }
        Err(e) => return Err(WorkerError::from(e)),
    };

    // 3b. No-backlink policy (unchanged): tombstone a previously-alive
    // source, reject the job.
    if !backlink_ok {
        if seen.is_some_and(|s| s.last_status == "alive") {
            let _ = repo
                .upsert_webmention_seen(NewWebmentionSeen {
                    source: job.source.clone(),
                    target: job.target.clone(),
                    last_status: "gone".to_string(),
                })
                .await;
        }
        return Err(WorkerError::NoBacklink);
    }

    // 4. Author info from the h-entry parsed above (no re-parse).
    let (author_name, author_url, author_avatar) = resolve_author_info(&job.source, &parsed);

    // 5. GitHub enrichment if author URL points to GitHub.
    let (final_name, github_avatar) = resolve_github(&author_url, &author_name, github).await;
    let final_avatar = github_avatar.or(author_avatar);

    // 6. Sanitize content.
    let content = if let Some(entry) = parsed {
        sanitize::sanitize_html(&entry.content, max_content_len)
    } else {
        "Mentioned this page.".to_string()
    };

    // 7. Upsert into comments table. Webmentions are always top-level.
    //    Only the *first* sighting of a source is a new comment — later pings
    //    are updates and shouldn't spam the admin notification channels.
    let is_new = repo.get_comment_by_source(&job.source).await?.is_none();
    let comment_id = repo
        .upsert_by_source(NewComment {
            target_path,
            comment_type: "webmention".to_string(),
            source_url: Some(job.source.clone()),
            author_name: final_name,
            author_url: author_url.clone(),
            author_avatar: final_avatar,
            content,
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
        })
        .await?;

    // 7.5 Notify admin channels about the new mention (batched digests).
    if is_new && notifier.has_channels() {
        if let Ok(Some(comment)) = repo.get_comment(comment_id).await {
            notifier.push(
                client,
                NewCommentInfo {
                    id: comment.id,
                    target_path: comment.target_path,
                    comment_type: "webmention".to_string(),
                    author_name: comment.author_name,
                    author_url: comment.author_url,
                    content: comment.content,
                    honeypot: false,
                    is_reply: false,
                },
            );
        }
    }

    // 8. Record in webmention_seen as alive.
    repo.upsert_webmention_seen(NewWebmentionSeen {
        source: job.source.clone(),
        target: job.target.clone(),
        last_status: "alive".to_string(),
    })
    .await?;

    tracing::info!(id = comment_id, source = %job.source, "webmention processed");
    Ok(())
}

// ── Author resolution ───────────────────────────────────────

/// Resolve author name, url, and avatar from a parsed h-entry or use domain fallback.
fn resolve_author_info(
    source_url: &str,
    parsed: &Option<ParsedMention>,
) -> (String, Option<String>, Option<String>) {
    if let Some(entry) = parsed {
        let name = if entry.author_name.is_empty() {
            domain_fallback(source_url)
        } else {
            entry.author_name.clone()
        };

        let avatar = entry.author_avatar.clone().or_else(|| {
            entry
                .author_url
                .as_ref()
                .and_then(|u| Url::parse(u).ok())
                .and_then(|u| {
                    u.host_str()
                        .map(|h| format!("https://api.dicebear.com/7.x/notionists/svg?seed={h}"))
                })
        });

        (name, entry.author_url.clone(), avatar)
    } else {
        let domain = domain_fallback(source_url);
        let avatar = Some(format!(
            "https://api.dicebear.com/7.x/notionists/svg?seed={domain}"
        ));
        (domain, Some(source_url.to_string()), avatar)
    }
}

fn domain_fallback(url_str: &str) -> String {
    Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(registrable_domain))
        .unwrap_or_else(|| "unknown".to_string())
}

/// If the author URL is a GitHub profile page, try to enrich with the API.
async fn resolve_github(
    author_url: &Option<String>,
    author_name: &str,
    github: &Arc<dyn GitHubLookup>,
) -> (String, Option<String>) {
    if let Some(url) = author_url
        && let Some(username) = crate::github::extract_github_username(url)
        && let Some(Profile { name, avatar_url }) = github.lookup(&username).await
    {
        return (name, Some(avatar_url));
    }
    (author_name.to_string(), None)
}

// ── Helpers ─────────────────────────────────────────────────

/// Extract the path portion from a target URL, validated against the configured origin.
fn derive_target_path(target: &str, target_origin: &str) -> Result<String, WorkerError> {
    let parsed = Url::parse(target).map_err(|_| WorkerError::InvalidTarget(target.to_string()))?;
    let origin_url = Url::parse(target_origin).expect("target_origin validated at startup");

    if parsed.origin() != origin_url.origin() {
        return Err(WorkerError::OriginMismatch(target_origin.to_string()));
    }

    let path = parsed.path().to_string();
    Ok(if path.is_empty() {
        "/".to_string()
    } else {
        path
    })
}

/// Handle a 410 Gone source: mark as gone in webmention_seen and delete the comment.
async fn handle_gone_source(job: &WebmentionJob, repo: &Repo) {
    let _ = repo
        .upsert_webmention_seen(NewWebmentionSeen {
            source: job.source.clone(),
            target: job.target.clone(),
            last_status: "gone".to_string(),
        })
        .await;
    if let Ok(Some(comment)) = repo.get_comment_by_source(&job.source).await
        && (comment.status == "approved" || comment.status == "pending")
    {
        let _ = repo.update_status(comment.id, "deleted").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TIMEOUT_TARGET: &str = "https://nithitsuki.com/blog/wm-timeout";

    fn slow_source_html() -> String {
        format!(
            r#"<!DOCTYPE html><html><body>
<article class="h-entry"><div class="e-content"><p>Slow post</p></div></article>
<a href="{TIMEOUT_TARGET}">backlink</a></body></html>"#
        )
    }

    #[tokio::test]
    async fn fetch_timeout_smaller_than_origin_delay_fails() {
        // G4 blocker pin: the worker must honor a non-default fetch timeout.
        // Pre-fix the timeout was hardcoded to the default, so an 80 ms
        // budget against a 1200 ms origin must fail here — with the 4 s
        // default the same fetch would succeed.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(1200))
                    .set_body_string(slow_source_html()),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let pool =
            crate::db::pool::create_pool(&dir.path().join("wm-timeout.db").to_string_lossy())
                .unwrap();
        crate::db::pool::run_migrations(&pool, None).unwrap();
        let repo = Repo::new(pool);
        let github: Arc<dyn GitHubLookup> = Arc::new(crate::github::StubGitHub);
        let notifier = Arc::new(NotificationBatcher::new(&crate::config::Config::default()));
        let client = Client::builder().build().unwrap();
        let job = WebmentionJob {
            source: format!("{}/slow", server.uri()),
            target: TIMEOUT_TARGET.to_string(),
        };

        let err = process_job_with_timeout(
            &job,
            &repo,
            &client,
            &github,
            "https://nithitsuki.com",
            2000,
            true,
            Duration::from_millis(80),
            &notifier,
        )
        .await
        .expect_err("80 ms budget vs 1200 ms origin must time out");
        match err {
            WorkerError::Fetch(FetchError::Http { source, .. }) => {
                assert!(source.is_timeout(), "expected a timeout, got: {source}")
            }
            other => panic!("expected timeout Http error, got: {other}"),
        }
    }
}
