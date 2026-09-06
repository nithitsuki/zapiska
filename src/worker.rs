use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::mpsc;
use url::Url;

use crate::db::repo::{NewComment, NewWebmentionSeen, Repo, WebmentionSeen};
use crate::error::AppError;
use crate::fetch::{FetchError, SafeFetcher, SourceFetcher};
use crate::github::{GitHubLookup, Profile};
use crate::mf2::{ParsedMention, has_backlink, parse_h_entry};
use crate::moderation::{Actor, Moderation, ModerationSink, Status};
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
    #[error("moderation transition failed: {0}")]
    Moderation(String),
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

/// Spawn the background worker that processes webmention jobs. The loop
/// stays a thin drain: build the production processor (its `SafeFetcher`
/// door carries the configured fetch timeout, its moderation sink carries
/// the configured webhook URL + signing secret) and pump jobs through it.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker_for_state(
    rx: JobReceiver,
    repo: Repo,
    client: Client,
    github: Arc<dyn GitHubLookup>,
    target_origin: Url,
    max_content_len: usize,
    timeout_ms: u64,
    notifier: Arc<NotificationBatcher>,
    moderation_sink: Option<Arc<dyn ModerationSink>>,
) {
    let processor = WebmentionProcessor::new(
        repo,
        github,
        notifier,
        client,
        target_origin,
        max_content_len,
        Duration::from_millis(timeout_ms),
        moderation_sink,
    );
    spawn_worker_for_processor(rx, processor);
}

/// Spawn the loop over an explicit processor. Tests inject a mock fetcher
/// (and a counting moderation sink) here; production arrives via
/// [`spawn_worker_for_state`].
pub fn spawn_worker_for_processor(mut rx: JobReceiver, processor: WebmentionProcessor) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            if let Err(e) = processor.process(&job).await {
                tracing::warn!(source = %job.source, target = %job.target, err = %e, "webmention worker error");
            }
        }
        tracing::warn!("webmention worker channel closed");
    });
}

// ── WebmentionProcessor ───────────────────────────────────────

/// How many consecutive backlink-less observations tombstone a source.
///
/// The ledger's `gone` row IS the first-miss memory: a backlink-less 200 on
/// an `alive` pair flips the ledger to `gone` but keeps the comment. Only a
/// SECOND consecutive observation on the gone-state re-fetch (another miss,
/// or a 410) deletes through the moderation machine. A single transient
/// blip therefore never deletes — the next good fetch restores `alive`
/// with the comment untouched. No new flags: this is a code constant,
/// pinned by `grace_window_is_two_consecutive_misses` plus the blip/cycle
/// behavior tests. Values above 2 need a counter column: the binary
/// ledger encoding (`alive`/`gone`) only supports 2.
const MISSES_TO_TOMBSTONE: u32 = 2;

/// The processor's view of one `(source, target)` ledger row: the state
/// machine the old god-loop hand-rolled inline across three sites.
///
/// ```text
/// Unknown ──fetch ok + backlink──▶ Alive ──miss──▶ Gone ──fetch ok + backlink──▶ Alive
///   │                                │               │
///   ├── Gone(410): record gone        │               ├── miss/410: confirm (delete)
///   └── miss: reject, no write ───────┘               └── (delete needs the 2nd miss)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeenState {
    /// No ledger row: the pair was never verified alive.
    Unknown,
    /// Last verified fetch found the backlink.
    Alive,
    /// Last observation was a 410 or a backlink-less fetch.
    Gone,
}

impl SeenState {
    fn of(seen: Option<&WebmentionSeen>) -> Self {
        match seen.map(|s| s.last_status.as_str()) {
            Some("alive") => Self::Alive,
            Some("gone") => Self::Gone,
            _ => Self::Unknown,
        }
    }
}

/// Webmention policy behind a fetcher adapter: the processor decides *what*
/// to do with a fetched source (verify, store, tombstone, resurrect); the
/// [`SourceFetcher`] decides *how* a URL becomes bytes. Production wires
/// [`SafeFetcher`] (the guarded door); tests inject a canned mock with no
/// network — the old `allow_loopback` boolean that flipped a security
/// invariant from the production signature is gone.
pub struct WebmentionProcessor {
    repo: Repo,
    fetcher: Arc<dyn SourceFetcher>,
    github: Arc<dyn GitHubLookup>,
    notifier: Arc<NotificationBatcher>,
    client: Client,
    target_origin: Url,
    max_content_len: usize,
    fetch_timeout: Duration,
    moderation_sink: Option<Arc<dyn ModerationSink>>,
}

impl WebmentionProcessor {
    /// Production constructor: builds the [`SafeFetcher`] door with
    /// `fetch_timeout` (threaded from the worker spawn args — the processor
    /// owns the timeout config end to end) and carries the spawn path's
    /// moderation sink (webhook URL + signing secret from config, `None`
    /// when unconfigured) for the gone path's `status_changed` events.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Repo,
        github: Arc<dyn GitHubLookup>,
        notifier: Arc<NotificationBatcher>,
        client: Client,
        target_origin: Url,
        max_content_len: usize,
        fetch_timeout: Duration,
        moderation_sink: Option<Arc<dyn ModerationSink>>,
    ) -> Self {
        Self::with_fetcher(
            repo,
            Arc::new(SafeFetcher::new().with_timeout(fetch_timeout)),
            github,
            notifier,
            client,
            target_origin,
            max_content_len,
            fetch_timeout,
            moderation_sink,
        )
    }

    /// Inject any [`SourceFetcher`] (tests pass a canned mock — no network,
    /// no loopback escape hatch) plus an optional moderation sink for the
    /// gone path's `status_changed` events.
    #[allow(clippy::too_many_arguments)]
    pub fn with_fetcher(
        repo: Repo,
        fetcher: Arc<dyn SourceFetcher>,
        github: Arc<dyn GitHubLookup>,
        notifier: Arc<NotificationBatcher>,
        client: Client,
        target_origin: Url,
        max_content_len: usize,
        fetch_timeout: Duration,
        moderation_sink: Option<Arc<dyn ModerationSink>>,
    ) -> Self {
        Self {
            repo,
            fetcher,
            github,
            notifier,
            client,
            target_origin,
            max_content_len,
            fetch_timeout,
            moderation_sink,
        }
    }

    /// The timeout this processor was built with (plumbing pin: spawn args
    /// → processor → fetcher construction).
    #[must_use]
    pub fn fetch_timeout(&self) -> Duration {
        self.fetch_timeout
    }

    /// Process one webmention job: derive the target path, re-fetch the
    /// source in EVERY ledger state (gone→alive is representable — there is
    /// no early return without fetching), then verify and store.
    pub async fn process(&self, job: &WebmentionJob) -> Result<(), WorkerError> {
        let target_path = derive_target_path(&job.target, &self.target_origin)?;
        let seen = self
            .repo
            .get_webmention_seen(&job.source, &job.target)
            .await?;
        let state = SeenState::of(seen.as_ref());

        // Single-parse fetch: the backlink check and the h-entry parse read
        // the one FetchedDoc tree (never re-parsed).
        // NOTE: scraper::Html is !Send: `map` collapses the document into
        // Send data (bool + parsed mention) with no await in between, so no
        // !Send value is ever live across a later await.
        let outcome = self
            .fetcher
            .fetch_source(&job.source)
            .await
            .map(|doc| (has_backlink(&doc.doc, &job.target), parse_h_entry(&doc.doc)));
        let (backlink_ok, parsed): (bool, Option<ParsedMention>) = match outcome {
            Ok(found) => found,
            Err(FetchError::Gone(_)) => {
                self.confirm_gone(job).await?;
                return Ok(());
            }
            Err(e) => return Err(WorkerError::from(e)),
        };

        if !backlink_ok {
            self.note_miss(job, state).await?;
            return Err(WorkerError::NoBacklink);
        }

        self.store_mention(job, &target_path, parsed).await?;
        Ok(())
    }

    /// Backlink-less 200 policy by ledger state.
    ///
    /// Miss counting without a counter column (no migration): the ledger
    /// state tells us which of the `MISSES_TO_TOMBSTONE` observations this
    /// is — `Alive` means miss #1 (flip the ledger, keep the comment),
    /// `Gone` means miss #2 (confirm and delete through the machine).
    /// `Unknown` was never verified, so there is nothing to tombstone:
    /// reject and write nothing.
    async fn note_miss(&self, job: &WebmentionJob, state: SeenState) -> Result<(), WorkerError> {
        let miss_number = match state {
            SeenState::Unknown => return Ok(()),
            SeenState::Alive => 1,
            SeenState::Gone => 2,
        };
        if miss_number < MISSES_TO_TOMBSTONE {
            self.repo
                .record_seen_gone(&job.source, &job.target)
                .await
                .map_err(WorkerError::from)
        } else {
            self.confirm_gone(job).await
        }
    }

    /// Confirm a source is gone: ledger `gone` plus deletion of EVERY
    /// comment the source owns (all target paths — a 410 proves the source
    /// dead, implicating all its mentions) through the T16 moderation
    /// machine as [`Actor::Admin`] (the engine acting with operator
    /// authority: the only actor that can move any live comment, and the
    /// machine's validity table is unchanged). Each live deletion fires one
    /// `comment.status_changed` event; already-deleted rows are same-status
    /// no-ops (no write, no event); spam rows stay for moderators, as
    /// before. A vanished row (`NotFound`) means the end state already
    /// holds — skip it.
    ///
    /// Restore-vs-live distinction: gone-delete IS a live transition (the
    /// event fires), and so is resurrection ([`Self::store_mention`]
    /// restores deleted→pending through the same machine — both paths
    /// emit; only same-status no-ops stay silent).
    async fn confirm_gone(&self, job: &WebmentionJob) -> Result<(), WorkerError> {
        // Only this pair's ledger row flips here: sibling pairs' rows stay
        // `alive` until their next ping re-fetches (transient, converges —
        // their comments are already deleted above, so no stale content).
        self.repo.record_seen_gone(&job.source, &job.target).await?;
        let comments = self.repo.list_comments_by_source(&job.source).await?;
        for comment in comments {
            if comment.status == Status::Approved.as_str()
                || comment.status == Status::Pending.as_str()
            {
                match Moderation::transition_comment(
                    &self.repo,
                    self.moderation_sink.as_deref(),
                    comment.id,
                    Status::Deleted,
                    Actor::Admin,
                )
                .await
                {
                    Ok(_) => {}
                    Err(AppError::NotFound(_)) => {
                        tracing::debug!(
                            id = comment.id,
                            "gone-delete raced a deletion; end state holds"
                        );
                    }
                    Err(e) => return Err(WorkerError::Moderation(e.to_string())),
                }
            }
        }
        Ok(())
    }

    /// Verified-mention store: author/GitHub resolution, sanitize, then the
    /// T15 mention+seen unit. Update semantics (B-17): the row keeps its
    /// moderation status (the upsert never writes `status`) while content,
    /// author fields, and `content_hash` refresh. The hash is computed by
    /// the WORKER through the shared [`sanitize::content_hash`] pipeline
    /// over the RAW e-content — ingress parity (hash-on-raw,
    /// store-on-sanitized) — so repeat-mention lookup sees content changes.
    ///
    /// `is_new` keys on the upsert pair `(source, target_path)`: a second
    /// page mentioned by the same source notifies on its own.
    ///
    /// Resurrection (ledger was `gone`, backlink verified again): the
    /// upsert preserves the `deleted` status the gone path wrote, so a
    /// comment coming back is explicitly restored to `pending` through the
    /// T16 moderation machine as [`Actor::Admin`] — deleted→pending emits
    /// (clearing tokens per B10), enforces validity, and serializes the
    /// TOCTOU between the pair read and the restore.
    async fn store_mention(
        &self,
        job: &WebmentionJob,
        target_path: &str,
        parsed: Option<ParsedMention>,
    ) -> Result<(), WorkerError> {
        // Author info from the h-entry parsed above (no re-parse).
        let (author_name, author_url, author_avatar) = resolve_author_info(&job.source, &parsed);

        // GitHub enrichment if author URL points to GitHub.
        let (final_name, github_avatar) =
            resolve_github(&author_url, &author_name, &self.github).await;
        let final_avatar = github_avatar.or(author_avatar);

        // Sanitize content.
        let raw_content = parsed
            .map(|entry| entry.content)
            .unwrap_or_else(|| "Mentioned this page.".to_string());
        let content = sanitize::sanitize_html(&raw_content, self.max_content_len);
        let content_hash = Some(sanitize::content_hash(&raw_content));

        // Upsert pair read: only the FIRST sighting of a (source, target)
        // notifies — later pings are updates and must not spam the admin
        // channels (and a second page gets its own notification).
        let existing = self
            .repo
            .get_comment_by_source_and_target(&job.source, target_path)
            .await?;
        let was_deleted = existing
            .as_ref()
            .is_some_and(|c| c.status == Status::Deleted.as_str());
        let is_new = existing.is_none();

        // Upsert into comments table + record webmention_seen as alive in
        // ONE transaction (T15 unit of work): either both land or neither.
        let comment_id = self
            .repo
            .upsert_webmention_with_seen(
                NewComment {
                    target_path: target_path.to_string(),
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
                    content_hash,
                },
                NewWebmentionSeen {
                    source: job.source.clone(),
                    target: job.target.clone(),
                    last_status: "alive".to_string(),
                },
            )
            .await?;

        if was_deleted {
            // Restore through the machine (M1): deleted→pending emits,
            // clears tokens per B10, enforces validity, and serializes the
            // TOCTOU. A vanished row means the end state already holds.
            match Moderation::transition_comment(
                &self.repo,
                self.moderation_sink.as_deref(),
                comment_id,
                Status::Pending,
                Actor::Admin,
            )
            .await
            {
                Ok(_) => {}
                Err(AppError::NotFound(_)) => {
                    tracing::debug!(id = comment_id, "restore raced a deletion; end state holds");
                }
                Err(e) => return Err(WorkerError::Moderation(e.to_string())),
            }
        }

        // Notify admin channels about the new mention (batched digests).
        if is_new && self.notifier.has_channels() {
            if let Ok(Some(comment)) = self.repo.get_comment(comment_id).await {
                self.notifier.push(
                    &self.client,
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

        tracing::info!(id = comment_id, source = %job.source, "webmention processed");
        Ok(())
    }
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
fn derive_target_path(target: &str, target_origin: &Url) -> Result<String, WorkerError> {
    let parsed = Url::parse(target).map_err(|_| WorkerError::InvalidTarget(target.to_string()))?;

    if parsed.origin() != target_origin.origin() {
        return Err(WorkerError::OriginMismatch(target_origin.to_string()));
    }

    let path = parsed.path().to_string();
    Ok(if path.is_empty() {
        "/".to_string()
    } else {
        path
    })
}

#[cfg(test)]
mod t20_processor_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::config::Config;
    use crate::fetch::{FetchedDoc, SourceFetcher};
    use crate::github::StubGitHub;
    use crate::moderation::{CountingSink, ModerationSink};

    const ORIGIN: &str = "https://nithitsuki.com";

    fn mention_html(target: &str, text: &str) -> String {
        format!(
            r#"<!DOCTYPE html><html><body>
<article class="h-entry">
  <div class="p-author h-card"><span class="p-name">Remote Author</span></div>
  <div class="e-content"><p>{text}</p></div>
</article>
<a href="{target}">backlink</a></body></html>"#
        )
    }

    fn unlink_html() -> String {
        r#"<!DOCTYPE html><html><body>
<article class="h-entry">
  <div class="p-author h-card"><span class="p-name">Remote Author</span></div>
  <div class="e-content"><p>Still here, link moved.</p></div>
</article>
<a href="https://elsewhere.example/other">elsewhere</a></body></html>"#
            .to_string()
    }

    /// Canned source fetcher: scripted responses per URL, no network. The
    /// worker-level replacement for `allow_loopback=true` — tests steer the
    /// fetch outcome instead of loosening the production SSRF check.
    enum MockOutcome {
        Html(String),
        Gone,
    }

    struct MockFetcher {
        responses: Mutex<HashMap<String, MockOutcome>>,
    }

    impl MockFetcher {
        fn with_html(url: &str, html: String) -> Arc<Self> {
            let mock = Self {
                responses: Mutex::new(HashMap::new()),
            };
            mock.set_html(url, html);
            Arc::new(mock)
        }

        fn set_html(&self, url: &str, html: String) {
            self.responses
                .lock()
                .expect("mock lock")
                .insert(url.to_string(), MockOutcome::Html(html));
        }

        fn set_gone(&self, url: &str) {
            self.responses
                .lock()
                .expect("mock lock")
                .insert(url.to_string(), MockOutcome::Gone);
        }
    }

    #[async_trait::async_trait]
    impl SourceFetcher for MockFetcher {
        async fn fetch_source(&self, url: &str) -> Result<FetchedDoc, FetchError> {
            match self.responses.lock().expect("mock lock").get(url) {
                Some(MockOutcome::Html(body)) => {
                    let parsed = Url::parse(url).map_err(|e| FetchError::InvalidUrl {
                        url: url.to_string(),
                        source: e,
                    })?;
                    Ok(FetchedDoc {
                        url: parsed,
                        status: reqwest::StatusCode::OK,
                        bytes: body.as_bytes().to_vec(),
                        // Single parse, like the production door: every
                        // consumer reads this one tree.
                        doc: scraper::Html::parse_document(body),
                    })
                }
                Some(MockOutcome::Gone) => Err(FetchError::Gone(url.to_string())),
                None => Err(FetchError::HttpStatus {
                    url: url.to_string(),
                    status: 404,
                }),
            }
        }
    }

    fn setup_repo(name: &str) -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::pool::create_pool(&dir.path().join(name).to_string_lossy()).unwrap();
        crate::db::pool::run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    fn processor(
        repo: Repo,
        fetcher: Arc<dyn SourceFetcher>,
        sink: Option<Arc<dyn ModerationSink>>,
    ) -> WebmentionProcessor {
        let github: Arc<dyn GitHubLookup> = Arc::new(StubGitHub);
        let notifier = Arc::new(NotificationBatcher::new(&Config::default()));
        let client = Client::builder().build().unwrap();
        WebmentionProcessor::with_fetcher(
            repo,
            fetcher,
            github,
            notifier,
            client,
            ORIGIN.parse().expect("test origin valid"),
            2000,
            Duration::from_secs(5),
            sink,
        )
    }

    fn job(source: &str, target: &str) -> WebmentionJob {
        WebmentionJob {
            source: source.to_string(),
            target: target.to_string(),
        }
    }

    #[test]
    fn grace_window_is_two_consecutive_misses() {
        // Policy pin: the ledger's `gone` row is the first-miss memory, so
        // deletion needs a second consecutive observation. Changing the
        // grace means changing this number AND the blip/cycle tests.
        assert_eq!(MISSES_TO_TOMBSTONE, 2);
    }

    #[test]
    fn processor_owns_timeout_from_spawn_args() {
        // Timeout plumbing: spawn args → processor → fetcher construction.
        let (repo, _dir) = setup_repo("t20-timeout.db");
        let p = processor(
            repo,
            MockFetcher::with_html("https://x.example/", String::new()),
            None,
        );
        assert_eq!(p.fetch_timeout(), Duration::from_secs(5));
        let (repo2, _dir2) = setup_repo("t20-timeout2.db");
        let github: Arc<dyn GitHubLookup> = Arc::new(StubGitHub);
        let notifier = Arc::new(NotificationBatcher::new(&Config::default()));
        let client = Client::builder().build().unwrap();
        let prod = WebmentionProcessor::new(
            repo2,
            github,
            notifier,
            client,
            ORIGIN.parse().expect("test origin valid"),
            2000,
            Duration::from_millis(80),
            None,
        );
        assert_eq!(prod.fetch_timeout(), Duration::from_millis(80));
    }

    #[tokio::test]
    async fn alive_gone_alive_full_cycle() {
        let (repo, _dir) = setup_repo("t20-cycle.db");
        let source = "https://src.example/cycle";
        let target = "https://nithitsuki.com/blog/t20-cycle";
        let mock = MockFetcher::with_html(source, mention_html(target, "Hello"));
        let sink = Arc::new(CountingSink::new());
        let dyn_sink: Arc<dyn ModerationSink> = sink.clone();
        let p = processor(repo.clone(), mock.clone(), Some(dyn_sink));
        let job = job(source, target);

        // Alive: new comment, pending, ledger alive, no moderation event.
        p.process(&job).await.unwrap();
        let c = repo
            .get_comment_by_source_and_target(source, "/blog/t20-cycle")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(c.status, "pending");
        assert!(c.content.contains("Hello"));
        assert_eq!(
            repo.get_webmention_seen(source, target)
                .await
                .unwrap()
                .unwrap()
                .last_status,
            "alive"
        );
        assert_eq!(sink.count(), 0, "storing a mention is not a transition");

        // Source 410s: gone-delete runs through the moderation machine.
        repo.update_status(c.id, "approved").await.unwrap();
        mock.set_gone(source);
        p.process(&job).await.unwrap();
        assert_eq!(
            repo.get_comment(c.id).await.unwrap().unwrap().status,
            "deleted"
        );
        assert_eq!(
            repo.get_webmention_seen(source, target)
                .await
                .unwrap()
                .unwrap()
                .last_status,
            "gone"
        );
        assert_eq!(sink.count(), 1, "gone-delete fires status_changed");
        let payloads = sink.payloads();
        assert_eq!(payloads[0]["event"], "comment.status_changed");
        assert_eq!(payloads[0]["old_status"], "approved");
        assert_eq!(payloads[0]["new_status"], "deleted");

        // Source restored with the backlink: re-ping RE-FETCHES (no early
        // return) and resurrects the comment to pending THROUGH the
        // moderation machine — deleted→pending fires, clears tokens per
        // B10, and serializes the TOCTOU.
        mock.set_html(source, mention_html(target, "Hello again"));
        p.process(&job).await.unwrap();
        let revived = repo.get_comment(c.id).await.unwrap().unwrap();
        assert_eq!(revived.status, "pending", "resurrection lands on pending");
        assert!(revived.content.contains("Hello again"));
        assert_eq!(
            repo.get_webmention_seen(source, target)
                .await
                .unwrap()
                .unwrap()
                .last_status,
            "alive",
            "gone→alive is representable"
        );
        assert_eq!(sink.count(), 2, "restore fires deleted→pending");
        let payloads = sink.payloads();
        assert_eq!(payloads[1]["event"], "comment.status_changed");
        assert_eq!(payloads[1]["old_status"], "deleted");
        assert_eq!(payloads[1]["new_status"], "pending");
    }

    #[tokio::test]
    async fn transient_blip_does_not_tombstone() {
        let (repo, _dir) = setup_repo("t20-blip.db");
        let source = "https://src.example/blip";
        let target = "https://nithitsuki.com/blog/t20-blip";
        let mock = MockFetcher::with_html(source, mention_html(target, "Steady post"));
        let sink = Arc::new(CountingSink::new());
        let dyn_sink: Arc<dyn ModerationSink> = sink.clone();
        let p = processor(repo.clone(), mock.clone(), Some(dyn_sink));
        let job = job(source, target);

        p.process(&job).await.unwrap();
        let c = repo
            .get_comment_by_source_and_target(source, "/blog/t20-blip")
            .await
            .unwrap()
            .unwrap();
        repo.update_status(c.id, "approved").await.unwrap();

        // One backlink-less 200: the job fails, the ledger flips to gone
        // (first-miss memory), but the long-alive comment is NOT deleted.
        mock.set_html(source, unlink_html());
        let err = p.process(&job).await.unwrap_err();
        assert!(matches!(err, WorkerError::NoBacklink));
        assert_eq!(
            repo.get_webmention_seen(source, target)
                .await
                .unwrap()
                .unwrap()
                .last_status,
            "gone"
        );
        assert_eq!(
            repo.get_comment(c.id).await.unwrap().unwrap().status,
            "approved",
            "one miss must not tombstone"
        );
        assert_eq!(sink.count(), 0);

        // Source back to normal: alive again, status and content preserved.
        mock.set_html(source, mention_html(target, "Steady post"));
        p.process(&job).await.unwrap();
        let kept = repo.get_comment(c.id).await.unwrap().unwrap();
        assert_eq!(kept.status, "approved");
        assert!(kept.content.contains("Steady post"));
        assert_eq!(
            repo.get_webmention_seen(source, target)
                .await
                .unwrap()
                .unwrap()
                .last_status,
            "alive"
        );
        assert_eq!(sink.count(), 0, "blip recovery emits nothing");
    }

    #[tokio::test]
    async fn second_consecutive_miss_confirms_gone_and_deletes() {
        // The other half of the grace: TWO consecutive backlink-less 200s
        // (miss #2 of MISSES_TO_TOMBSTONE on the gone-state re-fetch)
        // confirm the source is dead and delete through the machine.
        let (repo, _dir) = setup_repo("t20-miss2.db");
        let source = "https://src.example/miss2";
        let target = "https://nithitsuki.com/blog/t20-miss2";
        let mock = MockFetcher::with_html(source, mention_html(target, "Fading post"));
        let sink = Arc::new(CountingSink::new());
        let dyn_sink: Arc<dyn ModerationSink> = sink.clone();
        let p = processor(repo.clone(), mock.clone(), Some(dyn_sink));
        let job = job(source, target);

        p.process(&job).await.unwrap();
        let c = repo
            .get_comment_by_source_and_target(source, "/blog/t20-miss2")
            .await
            .unwrap()
            .unwrap();
        repo.update_status(c.id, "approved").await.unwrap();

        mock.set_html(source, unlink_html());
        let err = p.process(&job).await.unwrap_err();
        assert!(matches!(err, WorkerError::NoBacklink));
        assert_eq!(
            repo.get_comment(c.id).await.unwrap().unwrap().status,
            "approved",
            "first miss keeps the comment"
        );

        let err = p.process(&job).await.unwrap_err();
        assert!(matches!(err, WorkerError::NoBacklink));
        assert_eq!(
            repo.get_comment(c.id).await.unwrap().unwrap().status,
            "deleted",
            "second consecutive miss deletes"
        );
        assert_eq!(sink.count(), 1, "confirmed gone fires one event");
        let payloads = sink.payloads();
        assert_eq!(payloads[0]["old_status"], "approved");
        assert_eq!(payloads[0]["new_status"], "deleted");
    }

    #[tokio::test]
    async fn multi_target_scopes_reads_and_fans_out_gone() {
        let (repo, _dir) = setup_repo("t20-multi.db");
        let source = "https://src.example/multi";
        let target_a = "https://nithitsuki.com/blog/t20-a";
        let target_b = "https://nithitsuki.com/blog/t20-b";
        // One source page linking BOTH pages.
        let both = format!(
            r#"<!DOCTYPE html><html><body>
<article class="h-entry">
  <div class="p-author h-card"><span class="p-name">Remote Author</span></div>
  <div class="e-content"><p>Two pages, one post.</p></div>
</article>
<a href="{target_a}">first</a><a href="{target_b}">second</a></body></html>"#
        );
        let mock = MockFetcher::with_html(source, both);
        let sink = Arc::new(CountingSink::new());
        let dyn_sink: Arc<dyn ModerationSink> = sink.clone();
        let p = processor(repo.clone(), mock.clone(), Some(dyn_sink));

        p.process(&job(source, target_a)).await.unwrap();
        p.process(&job(source, target_b)).await.unwrap();
        let a = repo
            .get_comment_by_source_and_target(source, "/blog/t20-a")
            .await
            .unwrap()
            .unwrap();
        let b = repo
            .get_comment_by_source_and_target(source, "/blog/t20-b")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(a.id, b.id, "one pair = one row");
        assert_eq!(a.target_path, "/blog/t20-a");
        assert_eq!(b.target_path, "/blog/t20-b");

        // A 410 on a re-ping of A deletes BOTH: the dead source implicates
        // every page it mentioned, not just the re-pinged pair.
        repo.update_status(a.id, "approved").await.unwrap();
        repo.update_status(b.id, "approved").await.unwrap();
        mock.set_gone(source);
        p.process(&job(source, target_a)).await.unwrap();
        assert_eq!(
            repo.get_comment(a.id).await.unwrap().unwrap().status,
            "deleted"
        );
        assert_eq!(
            repo.get_comment(b.id).await.unwrap().unwrap().status,
            "deleted",
            "gone fans out to all targets of the source"
        );
        assert_eq!(sink.count(), 2, "one event per deleted comment");
    }

    #[tokio::test]
    async fn update_preserves_status_and_refreshes_hash() {
        // B-17: the worker computes the lookup hash through the shared
        // content pipeline; an update keeps its moderation status while the
        // stored hash tracks the new content.
        let (repo, _dir) = setup_repo("t20-update.db");
        let source = "https://src.example/update";
        let target = "https://nithitsuki.com/blog/t20-update";
        let mock = MockFetcher::with_html(source, mention_html(target, "First words"));
        let p = processor(repo.clone(), mock.clone(), None);

        p.process(&job(source, target)).await.unwrap();
        let c = repo
            .get_comment_by_source_and_target(source, "/blog/t20-update")
            .await
            .unwrap()
            .unwrap();
        repo.update_status(c.id, "approved").await.unwrap();
        let hash_v1 = repo.get_comment(c.id).await.unwrap().unwrap().content_hash;
        assert!(hash_v1.is_some(), "worker always stores a hash");

        mock.set_html(source, mention_html(target, "Second thoughts, longer"));
        p.process(&job(source, target)).await.unwrap();
        let updated = repo.get_comment(c.id).await.unwrap().unwrap();
        assert_eq!(updated.status, "approved", "update preserves status");
        assert!(updated.content.contains("Second thoughts"));
        assert_eq!(
            updated.content_hash,
            Some(crate::sanitize::content_hash(
                "<p>Second thoughts, longer</p>"
            )),
            "hash tracks the new raw e-content through the shared pipeline"
        );
        assert_ne!(updated.content_hash, hash_v1);
    }

    #[tokio::test]
    async fn gone_unit_failure_surfaces_to_the_worker_warn_path() {
        // A failing gone path must be OBSERVABLE (Err to the spawn loop's
        // warn path), never swallowed. The comments half is broken (table
        // dropped) while the ledger still reads fine.
        let dir = tempfile::tempdir().unwrap();
        let pool =
            crate::db::pool::create_pool(&dir.path().join("t20-gone-err.db").to_string_lossy())
                .unwrap();
        crate::db::pool::run_migrations(&pool, None).unwrap();
        pool.get()
            .unwrap()
            .execute_batch("DROP TABLE comments;")
            .unwrap();
        let repo = Repo::new(pool);
        let source = "https://src.example/gone-err";
        let target = "https://nithitsuki.com/blog/t20-gone-err";
        let mock = MockFetcher::with_html(source, mention_html(target, "x"));
        mock.set_gone(source);
        let p = processor(repo, mock, None);

        let err = p
            .process(&job(source, target))
            .await
            .expect_err("a broken gone unit must surface, not return Ok");
        assert!(
            matches!(err, WorkerError::Repo(_)),
            "expected the repo failure, got: {err}"
        );
    }

    #[tokio::test]
    async fn spawn_loop_drains_jobs_through_the_processor() {
        // The spawn loop stays a thin drain: jobs in → processor.process.
        let (repo, _dir) = setup_repo("t20-spawn.db");
        let source = "https://src.example/spawn";
        let target = "https://nithitsuki.com/blog/t20-spawn";
        let mock = MockFetcher::with_html(source, mention_html(target, "Spawned"));
        let (tx, rx) = channel(8);
        spawn_worker_for_processor(rx, processor(repo.clone(), mock, None));
        tx.send(job(source, target)).await.unwrap();
        let mut comment = None;
        for _ in 0..100 {
            comment = repo
                .get_comment_by_source_and_target(source, "/blog/t20-spawn")
                .await
                .unwrap();
            if comment.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(comment.is_some(), "spawned worker must process the job");
    }
}
