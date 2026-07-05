use std::sync::Arc;

use reqwest::Client;
use tokio::sync::mpsc;
use url::Url;

use crate::db::repo::{CommentsRepo, NewComment, NewWebmentionSeen};
use crate::github::{GitHubLookup, Profile};
use crate::http::reqwest_client::{FetchError, fetch_url};
use crate::mf2::{ParsedMention, has_backlink, parse_h_entry};
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
pub fn spawn_worker_for_state(
    mut rx: JobReceiver,
    repo: CommentsRepo,
    client: Client,
    github: Arc<dyn GitHubLookup>,
    target_origin: String,
    max_content_len: usize,
    _timeout_ms: u64,
) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            if let Err(e) = process_job(
                &job,
                &repo,
                &client,
                &github,
                &target_origin,
                max_content_len,
                false,
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
pub async fn process_job(
    job: &WebmentionJob,
    repo: &CommentsRepo,
    client: &Client,
    github: &Arc<dyn GitHubLookup>,
    target_origin: &str,
    max_content_len: usize,
    allow_loopback: bool,
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

    // 2. SSRF-safe fetch of the source URL.
    let html = match fetch_url(client, &job.source, allow_loopback).await {
        Ok(h) => h,
        Err(FetchError::Gone(_)) => {
            handle_gone_source(job, repo).await;
            return Ok(());
        }
        Err(e) => return Err(WorkerError::from(e)),
    };

    // 3. Verify the source page contains a link to the target.
    if !has_backlink(&html, &job.target) {
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

    // 4. Parse h-entry and resolve author info.
    let parsed = parse_h_entry(&html);
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
            content_hash: None,
        })
        .await?;

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
        && let Some(username) = extract_github_username(url)
        && let Some(Profile { name, avatar_url }) = github.lookup(&username).await
    {
        return (name, Some(avatar_url));
    }
    (author_name.to_string(), None)
}

/// Extract GitHub username from a URL like `https://github.com/username`.
fn extract_github_username(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let username = parsed.path().trim_start_matches('/').split('/').next()?;
    if username.is_empty() {
        return None;
    }
    Some(username.to_string())
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
async fn handle_gone_source(job: &WebmentionJob, repo: &CommentsRepo) {
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
