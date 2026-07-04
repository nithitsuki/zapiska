use std::sync::Arc;

use reqwest::Client;
use tokio::sync::mpsc;

use crate::db::repo::{CommentsRepo, NewComment, NewWebmentionSeen};
use crate::github::{GitHubLookup, Profile};
use crate::http::reqwest_client::{FetchError, fetch_url};
use crate::mf2::{has_backlink, parse_h_entry};
use crate::sanitize;
use crate::ssrf::registrable_domain;

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
            // Production worker never allows loopback fetches.
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
) -> Result<(), String> {
    // 1. Check idempotency / deletion: lookup (source, target) in webmention_seen.
    let target_path = derive_target_path(&job.target, target_origin)?;

    let seen = repo
        .get_webmention_seen(&job.source, &job.target)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(ref s) = seen
        && s.last_status == "gone"
        && let Ok(Some(comment)) = repo.get_comment_by_source(&job.source).await
    {
        if comment.status == "approved" || comment.status == "pending" {
            let _ = repo.update_status(comment.id, "deleted").await;
        }
        tracing::info!(
            source = %job.source,
            target = %job.target,
            "webmention previously gone, comment deleted if approved"
        );
        return Ok(());
    }

    // 2. SSRF-safe fetch of the source URL.
    let html = match fetch_url(client, &job.source, allow_loopback).await {
        Ok(h) => h,
        Err(FetchError::Gone(_)) => {
            // Source is 410 Gone — record as gone and delete associated comment.
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
            return Ok(());
        }
        Err(e) => {
            return Err(format!("fetch failed: {e}"));
        }
    };

    // 3. Verify the source page contains a link to the target.
    if !has_backlink(&html, &job.target) {
        // If we previously had an `alive` entry for this source, mark it gone.
        if seen.is_some_and(|s| s.last_status == "alive") {
            let _ = repo
                .upsert_webmention_seen(NewWebmentionSeen {
                    source: job.source.clone(),
                    target: job.target.clone(),
                    last_status: "gone".to_string(),
                })
                .await;
        }
        return Err("no backlink to target found".to_string());
    }

    // 4. Parse h-entry from the HTML.
    let parsed = parse_h_entry(&html);

    // 5. Resolve author info.
    let (author_name, author_url, author_avatar) = if let Some(ref mention) = parsed {
        let name = if mention.author_name.is_empty() {
            // Fallback: registrable domain of the source URL.
            registrable_domain(
                url::Url::parse(&job.source)
                    .ok()
                    .and_then(|u| u.host_str().map(|s| s.to_string()))
                    .as_deref()
                    .unwrap_or("unknown"),
            )
        } else {
            mention.author_name.clone()
        };

        let avatar = mention.author_avatar.clone().or_else(|| {
            mention
                .author_url
                .as_ref()
                .and_then(|u| url::Url::parse(u).ok())
                .and_then(|u| u.host_str().map(|h| format!("https://icon.horse/{h}")))
        });

        (name, mention.author_url.clone(), avatar)
    } else {
        // No h-entry found. Use domain fallback.
        let domain = url::Url::parse(&job.source)
            .ok()
            .and_then(|u| u.host_str().map(registrable_domain))
            .unwrap_or_else(|| "unknown".to_string());
        let avatar = Some(format!("https://icon.horse/{}", domain));
        (domain, Some(job.source.clone()), avatar)
    };

    // 6. Resolve GitHub enrichment if author URL points to GitHub.
    let (final_name, github_avatar) = resolve_github(&author_url, &author_name, github).await;
    let final_avatar = github_avatar.or(author_avatar);

    // 7. Sanitize content.
    let content = if let Some(ref mention) = parsed {
        sanitize::sanitize_html(&mention.content, max_content_len)
    } else {
        "Mentioned this page.".to_string()
    };

    // 8. Upsert into comments table.
    let new_comment = NewComment {
        target_path,
        comment_type: "webmention".to_string(),
        source_url: Some(job.source.clone()),
        author_name: final_name,
        author_url: author_url.clone(),
        author_avatar: final_avatar,
        content,
    };

    let comment_id = repo
        .upsert_by_source(new_comment)
        .await
        .map_err(|e| e.to_string())?;

    // 9. Record in webmention_seen as alive.
    repo.upsert_webmention_seen(NewWebmentionSeen {
        source: job.source.clone(),
        target: job.target.clone(),
        last_status: "alive".to_string(),
    })
    .await
    .map_err(|e| e.to_string())?;

    tracing::info!(id = comment_id, source = %job.source, "webmention processed");
    Ok(())
}

/// Extract the path portion from a target URL, validated against the configured origin.
fn derive_target_path(target: &str, target_origin: &str) -> Result<String, String> {
    let parsed = url::Url::parse(target).map_err(|_| format!("invalid target URL: {target}"))?;
    let origin_url = url::Url::parse(target_origin).expect("target_origin validated at startup");

    if parsed.origin() != origin_url.origin() {
        return Err(format!(
            "target origin does not match configured origin '{}'",
            target_origin
        ));
    }

    let path = parsed.path().to_string();
    if path.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(path)
    }
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

/// Extract GitHub username from a URL like `https://github.com/username` or `https://www.github.com/username`.
fn extract_github_username(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
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
