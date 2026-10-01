use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
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

/// Why the webmention consumer task ended. The supervisor task (see
/// [`spawn_worker_for_processor`]) resolves to this value, so a consumer
/// death is programmatically observable — by `main` at shutdown and by the
/// shared watch channel `/healthz` reads — not only logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerExit {
    /// The loop exited on the shutdown signal or channel close: normal.
    Graceful,
    /// The consumer task panicked. `source`/`target` are the job that was in
    /// flight, when known.
    Panicked {
        source: Option<String>,
        target: Option<String>,
        message: String,
    },
    /// The consumer task was aborted (not observed today; fail loudly anyway).
    Cancelled,
}

/// Shared handle to the spawned worker supervisor task. `JoinHandle` is not
/// `Clone`, and every handler's `AppState` is: the slot behind an
/// `Arc<Mutex<..>>` lets all clones observe one handle, and `main` takes and
/// awaits it once at shutdown.
pub type WorkerHandle = Arc<std::sync::Mutex<Option<JoinHandle<WorkerExit>>>>;

pub fn channel(buffer: usize) -> (JobSender, JobReceiver) {
    mpsc::channel(buffer)
}

// ── Retry and drain policy ──────────────────────────────────

/// Total processing attempts for one job: the first try plus two retries.
/// Matches the notification channel's bounded-retry budget.
const MAX_JOB_ATTEMPTS: u32 = 3;

/// First retry delay. Attempt N waits `RETRY_BASE_DELAY * 2^(N-1)`.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

/// Cap on one retry delay.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

/// Total time one job may spend retrying before the worker drops it. This
/// is a safety net over the attempt cap; with [`MAX_JOB_ATTEMPTS`] and
/// [`RETRY_BASE_DELAY`] the two retry waits sum below it.
const RETRY_TOTAL_BUDGET: Duration = Duration::from_secs(120);

/// Bound on the shutdown drain: buffered jobs get this long to finish
/// before the worker exits. `main` waits for this bound plus a small tail.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on the retry bookkeeping map. A retry that is dropped after
/// re-enqueue (a full queue) is never seen again, so its record would
/// linger. The key comes from an unauthenticated caller, so the map must
/// not grow without limit.
const MAX_TRACKED_RETRIES: usize = 4096;

/// Bounded exponential-backoff policy for retryable jobs. Production uses
/// [`RetryPolicy::DEFAULT`]; tests build short-delay policies to prove the
/// attempt cap and the total-budget branch without slow sleeps.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    total_budget: Duration,
}

impl RetryPolicy {
    pub const DEFAULT: Self = Self {
        max_attempts: MAX_JOB_ATTEMPTS,
        base_delay: RETRY_BASE_DELAY,
        max_delay: RETRY_MAX_DELAY,
        total_budget: RETRY_TOTAL_BUDGET,
    };

    /// Wait before retry `attempt` (1-based), or `None` when the attempt cap
    /// or the total time budget is exhausted (drop the job).
    fn delay_for(&self, attempt: u32, elapsed: Duration) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        let shift = (attempt - 1).min(31);
        let delay = self
            .base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay);
        if elapsed.saturating_add(delay) > self.total_budget {
            return None;
        }
        Some(delay)
    }
}

/// Per-pair retry bookkeeping for the worker loop: the number of retryable
/// failures already seen, and when the first one happened (for the budget).
#[derive(Clone, Copy)]
struct RetryState {
    attempts: u32,
    first_failure: Instant,
}

/// Classify a processing error. `true` means a retry can succeed.
///
/// Retryable: fetch timeout/connect/transport errors, body-read failures,
/// 5xx responses, and transient storage contention (`Busy`/`Io`).
/// Terminal: a missing backlink, every 4xx (410 included), bad/blocked
/// URLs, redirect and body caps, and all other storage/moderation errors.
fn is_retryable(err: &WorkerError) -> bool {
    match err {
        WorkerError::Fetch(e) => fetch_is_retryable(e),
        WorkerError::Repo(crate::db::RepoError::Busy(_))
        | WorkerError::Repo(crate::db::RepoError::Io(_)) => true,
        _ => false,
    }
}

fn fetch_is_retryable(err: &FetchError) -> bool {
    match err {
        FetchError::Http { .. } | FetchError::BodyRead { .. } => true,
        FetchError::HttpStatus { status, .. } => *status >= 500,
        FetchError::Gone(_)
        | FetchError::InvalidUrl { .. }
        | FetchError::NoHost(_)
        | FetchError::UnsupportedScheme(_)
        | FetchError::Blocked(_)
        | FetchError::TooManyRedirects(_)
        | FetchError::TooLarge { .. } => false,
    }
}

/// Drop the retry record with the oldest first-failure timestamp. Runs only
/// when the map hits [`MAX_TRACKED_RETRIES`], so the linear scan is off the
/// hot path.
fn evict_oldest_retry(retries: &mut HashMap<(String, String), RetryState>) {
    let oldest = retries
        .iter()
        .min_by_key(|(_, state)| state.first_failure)
        .map(|(key, _)| key.clone());
    match oldest {
        Some(key) => {
            retries.remove(&key);
        }
        None => {
            tracing::warn!("retry map at capacity but empty; nothing to evict");
        }
    }
}

/// Spawn a no-op worker that drains the channel (for tests / scaffolding).
pub fn spawn_worker(mut rx: JobReceiver) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            tracing::debug!(source = %job.source, target = %job.target, "no-op worker drained job");
        }
    });
}

/// Spawn the background worker that processes webmention jobs. This builds
/// the production processor (its `SafeFetcher` door carries the configured
/// fetch timeout, its moderation sink carries the configured webhook URL +
/// signing secret) and hands it to [`spawn_worker_for_processor`], which owns
/// the loop: draining, the bounded retry policy, and the shutdown drain.
/// The worker holds a sender clone so retries can re-enter the queue; the
/// shutdown signal, not channel close, ends the loop.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker_for_state(
    tx: JobSender,
    rx: JobReceiver,
    repo: Repo,
    client: Client,
    github: Arc<dyn GitHubLookup>,
    target_origin: Url,
    max_content_len: usize,
    timeout_ms: u64,
    notifier: Arc<NotificationBatcher>,
    moderation_sink: Option<Arc<dyn ModerationSink>>,
    shutdown: watch::Receiver<bool>,
    exit_tx: watch::Sender<Option<WorkerExit>>,
) -> WorkerHandle {
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
    let handle =
        spawn_worker_for_processor(tx, rx, processor, shutdown, RetryPolicy::DEFAULT, exit_tx);
    Arc::new(std::sync::Mutex::new(Some(handle)))
}

/// Spawn the loop over an explicit processor. Tests inject a mock fetcher
/// (and a counting moderation sink) here; production arrives via
/// [`spawn_worker_for_state`].
///
/// The loop takes `tx` as well as `rx` because a retryable failure
/// re-enters the SAME bounded channel. The worker is the only consumer, so
/// the requeue must be `try_send` (see [`process_one`]): an awaited `send`
/// on a full queue would block the task that drains it.
///
/// A panic in the consumer must not stop at a bare `JoinHandle` that nobody
/// reads, so the returned task is a SUPERVISOR: it awaits the consumer,
/// classifies the outcome as a [`WorkerExit`], logs a panic at error level
/// with the in-flight job, and publishes the outcome on `exit_tx`.
///
/// Per-job panic ISOLATION (catching a panic inside [`process_one`] and
/// continuing) is a deliberate NON-GOAL. A panic inside the consumer means
/// the single-consumer invariant is already broken. Swallowing it would
/// leave a half-dead worker that looks healthy, so this fails loud instead:
/// `main` exits non-zero and the orchestrator restarts the process.
pub fn spawn_worker_for_processor(
    tx: JobSender,
    mut rx: JobReceiver,
    processor: WebmentionProcessor,
    mut shutdown: watch::Receiver<bool>,
    policy: RetryPolicy,
    exit_tx: watch::Sender<Option<WorkerExit>>,
) -> JoinHandle<WorkerExit> {
    // The job currently inside `process_one`. The consumer sets it before the
    // call and clears it after; a panic leaves it set, so the supervisor can
    // name the job that died. A `std::sync::Mutex` guard is never held across
    // an await.
    let in_flight: Arc<std::sync::Mutex<Option<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(None));

    let consumer = {
        let in_flight = Arc::clone(&in_flight);
        tokio::spawn(async move {
            let mut retries: HashMap<(String, String), RetryState> = HashMap::new();
            loop {
                tokio::select! {
                    biased;
                    // Shutdown wins over new work: stop accepting jobs, drain
                    // what is already buffered, then exit. The channel never
                    // closes on its own because every handler holds a sender,
                    // so the explicit signal (not `recv` returning `None`) ends
                    // the loop.
                    _ = shutdown.changed() => {
                        drain_bounded(&mut rx, &processor, DRAIN_TIMEOUT).await;
                        break;
                    }
                    maybe = rx.recv() => {
                        match maybe {
                            Some(job) => {
                                *in_flight.lock().expect("in-flight lock") =
                                    Some((job.source.clone(), job.target.clone()));
                                process_one(&tx, &processor, &job, &mut retries, policy).await;
                                *in_flight.lock().expect("in-flight lock") = None;
                            }
                            None => break,
                        }
                    }
                }
            }
            tracing::warn!("webmention worker stopped");
        })
    };

    tokio::spawn(async move {
        let exit = match consumer.await {
            Ok(()) => WorkerExit::Graceful,
            Err(join) if join.is_panic() => {
                let (source, target) = in_flight
                    .lock()
                    .expect("in-flight lock")
                    .clone()
                    .map_or((None, None), |(s, t)| (Some(s), Some(t)));
                let message = panic_message(join);
                tracing::error!(
                    source = source.as_deref().unwrap_or("<unknown>"),
                    target = target.as_deref().unwrap_or("<unknown>"),
                    panic = %message,
                    "webmention worker panicked; the consumer is dead and every \
                     future webmention will fail until the process restarts"
                );
                WorkerExit::Panicked {
                    source,
                    target,
                    message,
                }
            }
            Err(join) => {
                debug_assert!(join.is_cancelled());
                tracing::error!("webmention worker task was cancelled");
                WorkerExit::Cancelled
            }
        };
        // Publish before returning. `main` watches this to stop serving, and
        // `/healthz` reads the latest value. No receiver (some tests) is fine:
        // the send error is expected and ignored.
        let _ = exit_tx.send(Some(exit.clone()));
        exit
    })
}

/// Extract a `JoinError`'s panic payload into a `String`. The payload is
/// whatever the panicking task passed to `panic!`, usually a `String` or a
/// `&'static str`. Any other payload becomes a fixed placeholder.
fn panic_message(err: tokio::task::JoinError) -> String {
    let payload = err.into_panic();
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(text) => (*text).to_string(),
            Err(_) => "panic payload was not a string".to_string(),
        },
    }
}

/// Process one job and schedule a bounded retry on a transient failure.
///
/// A retry re-enters the same bounded queue with `try_send` only. The
/// worker is the only consumer, so an awaiting `send` on a full queue would
/// block the one task that could drain it and wedge the process. A full
/// queue drops the retry with a warning.
async fn process_one(
    tx: &JobSender,
    processor: &WebmentionProcessor,
    job: &WebmentionJob,
    retries: &mut HashMap<(String, String), RetryState>,
    policy: RetryPolicy,
) {
    let key = (job.source.clone(), job.target.clone());
    match processor.process(job).await {
        Ok(()) => {
            retries.remove(&key);
        }
        Err(e) if is_retryable(&e) => {
            let (attempt, elapsed) = {
                // Bound the map. The key comes from an unauthenticated
                // caller, and a retry dropped after re-enqueue (a full
                // queue) is never observed again, so without this the map
                // grows without limit.
                if retries.len() >= MAX_TRACKED_RETRIES {
                    evict_oldest_retry(retries);
                }
                let state = retries.entry(key.clone()).or_insert_with(|| RetryState {
                    attempts: 0,
                    first_failure: Instant::now(),
                });
                state.attempts += 1;
                (state.attempts, state.first_failure.elapsed())
            };
            match policy.delay_for(attempt, elapsed) {
                Some(delay) => {
                    // Never sleep in this loop. This task is the only
                    // consumer, so a sleep here stalls every other queued
                    // webmention for the whole backoff window, and delays
                    // the shutdown signal by the same amount because the
                    // `select!` cannot observe it mid-sleep. Hand the delay
                    // to a detached task that sleeps and then re-enqueues
                    // with `try_send`.
                    let tx = tx.clone();
                    let job = job.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        if tx.try_send(job).is_err() {
                            tracing::warn!("webmention retry dropped: worker queue full");
                        }
                    });
                }
                None => {
                    retries.remove(&key);
                    tracing::warn!(
                        source = %job.source,
                        target = %job.target,
                        attempt,
                        err = %e,
                        "webmention retries exhausted; dropping job"
                    );
                }
            }
        }
        Err(e) => {
            retries.remove(&key);
            tracing::warn!(
                source = %job.source,
                target = %job.target,
                err = %e,
                "webmention worker error (terminal)"
            );
        }
    }
}

/// Drain the jobs already buffered in the channel, awaiting each one.
/// Bounded: tests inject a short timeout to prove the deadline; production
/// uses [`DRAIN_TIMEOUT`]. Jobs still buffered when the deadline expires
/// are abandoned (best-effort, like the notification drain).
pub(crate) async fn drain_bounded(
    rx: &mut JobReceiver,
    processor: &WebmentionProcessor,
    timeout: Duration,
) {
    let _ = tokio::time::timeout(timeout, drain_inner(rx, processor)).await;
}

async fn drain_inner(rx: &mut JobReceiver, processor: &WebmentionProcessor) {
    while let Ok(job) = rx.try_recv() {
        if let Err(e) = processor.process(&job).await {
            tracing::warn!(
                source = %job.source,
                target = %job.target,
                err = %e,
                "webmention drain error"
            );
        }
    }
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
    /// fetch outcome instead of loosening the production SSRF check. The
    /// extra variants let retry/drain tests inject a transient 5xx, a fixed
    /// number of failures, or a fetch that waits on a gate.
    #[derive(Clone)]
    enum MockOutcome {
        Html(String),
        Gone,
        Status(u16),
        /// Fail with `status` for the first `remaining` calls, then return
        /// `html`.
        Flaky {
            remaining: u32,
            status: u16,
            html: String,
        },
        /// Wait for `gate` before returning `html`, so a test can hold a job
        /// in flight while it queues another and signals shutdown.
        Gated {
            gate: Arc<tokio::sync::Notify>,
            html: String,
        },
        /// Panic on fetch, to force a panic inside the consumer and prove the
        /// supervisor reports and logs it.
        Panic,
    }

    struct MockFetcher {
        responses: Mutex<HashMap<String, MockOutcome>>,
        calls: Mutex<HashMap<String, u32>>,
    }

    impl MockFetcher {
        fn with_html(url: &str, html: String) -> Arc<Self> {
            let mock = Self {
                responses: Mutex::new(HashMap::new()),
                calls: Mutex::new(HashMap::new()),
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

        fn set_status(&self, url: &str, status: u16) {
            self.responses
                .lock()
                .expect("mock lock")
                .insert(url.to_string(), MockOutcome::Status(status));
        }

        fn set_flaky(&self, url: &str, failures: u32, html: String) {
            self.responses.lock().expect("mock lock").insert(
                url.to_string(),
                MockOutcome::Flaky {
                    remaining: failures,
                    status: 503,
                    html,
                },
            );
        }

        fn set_gated(&self, url: &str, gate: Arc<tokio::sync::Notify>, html: String) {
            self.responses
                .lock()
                .expect("mock lock")
                .insert(url.to_string(), MockOutcome::Gated { gate, html });
        }

        fn set_panic(&self, url: &str) {
            self.responses
                .lock()
                .expect("mock lock")
                .insert(url.to_string(), MockOutcome::Panic);
        }

        fn calls_for(&self, url: &str) -> u32 {
            self.calls
                .lock()
                .expect("mock lock")
                .get(url)
                .copied()
                .unwrap_or(0)
        }
    }

    fn ok_doc(url: &str, body: String) -> Result<FetchedDoc, FetchError> {
        let parsed = Url::parse(url).map_err(|e| FetchError::InvalidUrl {
            url: url.to_string(),
            source: e,
        })?;
        Ok(FetchedDoc {
            url: parsed,
            status: reqwest::StatusCode::OK,
            bytes: body.as_bytes().to_vec(),
            // Single parse, like the production door: every consumer reads
            // this one tree.
            doc: scraper::Html::parse_document(&body),
        })
    }

    #[async_trait::async_trait]
    impl SourceFetcher for MockFetcher {
        async fn fetch_source(&self, url: &str) -> Result<FetchedDoc, FetchError> {
            *self
                .calls
                .lock()
                .expect("mock lock")
                .entry(url.to_string())
                .or_insert(0) += 1;
            let outcome = self.responses.lock().expect("mock lock").get(url).cloned();
            match outcome {
                Some(MockOutcome::Html(body)) => ok_doc(url, body),
                Some(MockOutcome::Gone) => Err(FetchError::Gone(url.to_string())),
                Some(MockOutcome::Status(status)) => Err(FetchError::HttpStatus {
                    url: url.to_string(),
                    status,
                }),
                Some(MockOutcome::Flaky {
                    remaining,
                    status,
                    html,
                }) => {
                    if remaining > 0 {
                        self.responses.lock().expect("mock lock").insert(
                            url.to_string(),
                            MockOutcome::Flaky {
                                remaining: remaining - 1,
                                status,
                                html,
                            },
                        );
                        Err(FetchError::HttpStatus {
                            url: url.to_string(),
                            status,
                        })
                    } else {
                        ok_doc(url, html)
                    }
                }
                Some(MockOutcome::Gated { gate, html }) => {
                    gate.notified().await;
                    ok_doc(url, html)
                }
                Some(MockOutcome::Panic) => panic!("mock fetcher exploded for {url}"),
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

    /// Poll `f` until it is true or the bound (4 s) expires.
    async fn wait_until(mut f: impl FnMut() -> bool, what: &str) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Poll until the worker stored the comment for `(source, path)`.
    async fn wait_for_comment(repo: &Repo, source: &str, path: &str) {
        for _ in 0..400 {
            if repo
                .get_comment_by_source_and_target(source, path)
                .await
                .ok()
                .flatten()
                .is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for comment {source} -> {path}");
    }

    /// Poll until `url` has been fetched at least `n` times.
    async fn wait_for_calls(mock: &Arc<MockFetcher>, url: &str, n: u32) {
        for _ in 0..400 {
            if mock.calls_for(url) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {n} calls to {url}");
    }

    /// A policy whose first retry waits far longer than any test waits.
    /// A consumer that sleeps through its own backoff would blow every
    /// deadline below; one that hands the delay to a detached task does not.
    fn slow_retry_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_secs(30),
            max_delay: Duration::from_secs(60),
            total_budget: Duration::from_secs(300),
        }
    }

    /// Exit sender for tests that do not assert on the published signal. The
    /// dropped receiver is fine: the supervisor ignores a send with no
    /// receiver.
    fn exit_tx() -> watch::Sender<Option<WorkerExit>> {
        watch::channel(None).0
    }

    #[tokio::test]
    async fn pending_retry_delay_does_not_block_the_consumer() {
        // Regression: the worker is the only consumer, so sleeping through
        // its own backoff window would stall every other queued webmention
        // for that window. Job A fails retryably and parks a 30s retry. Job
        // B must still be processed now.
        let (repo, _dir) = setup_repo("t21-retry-not-blocking.db");
        let stuck = "https://src.example/stuck";
        let moving = "https://src.example/moving";
        let mock = MockFetcher::with_html(
            moving,
            mention_html("https://nithitsuki.com/blog/t21-mv", "moved on"),
        );
        mock.set_flaky(
            stuck,
            1,
            mention_html("https://nithitsuki.com/blog/t21-stuck", "later"),
        );
        let (tx, rx) = channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let _worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock.clone(), None),
            shutdown_rx,
            slow_retry_policy(),
            exit_tx(),
        );

        tx.send(job(stuck, "https://nithitsuki.com/blog/t21-stuck"))
            .await
            .unwrap();
        // Job A has now failed once, so a 30s retry is pending.
        wait_for_calls(&mock, stuck, 1).await;

        tx.send(job(moving, "https://nithitsuki.com/blog/t21-mv"))
            .await
            .unwrap();

        // Must land far inside the 30s backoff window.
        tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_comment(&repo, moving, "/blog/t21-mv"),
        )
        .await
        .expect("a pending retry must not stall the only consumer");
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn shutdown_is_observed_while_a_retry_delay_is_pending() {
        // Regression: a sleep inside the consumer loop hides the shutdown
        // signal until it ends, so `main`'s bounded await would expire and
        // the queued job would be lost. With a 30s retry pending, the
        // worker must still exit promptly.
        let (repo, _dir) = setup_repo("t21-retry-shutdown.db");
        let stuck = "https://src.example/shutdown-stuck";
        let mock = MockFetcher::with_html(
            stuck,
            mention_html("https://nithitsuki.com/blog/t21-sd", "later"),
        );
        mock.set_flaky(
            stuck,
            1,
            mention_html("https://nithitsuki.com/blog/t21-sd", "later"),
        );
        let (tx, rx) = channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock.clone(), None),
            shutdown_rx,
            slow_retry_policy(),
            exit_tx(),
        );

        tx.send(job(stuck, "https://nithitsuki.com/blog/t21-sd"))
            .await
            .unwrap();
        wait_for_calls(&mock, stuck, 1).await;
        let _ = shutdown_tx.send(true);

        let exit = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("shutdown must not wait out a pending retry delay")
            .expect("supervisor must not panic");
        assert_eq!(
            exit,
            WorkerExit::Graceful,
            "the shutdown signal must land on the graceful path"
        );
    }

    #[tokio::test]
    async fn worker_reports_graceful_on_normal_shutdown() {
        // The supervisor must resolve to `Graceful` on the ordinary path, so
        // `Panicked`/`Cancelled` are provably distinct rather than the only
        // reachable outcome.
        let (repo, _dir) = setup_repo("t21-exit-graceful.db");
        let mock = MockFetcher::with_html(
            "https://src.example/graceful",
            mention_html("https://nithitsuki.com/blog/t21-graceful", "x"),
        );
        let (tx, rx) = channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (exit_tx, exit_rx) = watch::channel(None);
        let worker = spawn_worker_for_processor(
            tx,
            rx,
            processor(repo, mock, None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx,
        );

        shutdown_tx.send(true).unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("worker must exit after the shutdown signal")
            .expect("supervisor must not itself panic");
        assert_eq!(exit, WorkerExit::Graceful);
        assert_eq!(
            *exit_rx.borrow(),
            Some(WorkerExit::Graceful),
            "the published signal must match the returned outcome"
        );
    }

    #[tokio::test]
    async fn panicking_consumer_reports_panicked_with_job_identity() {
        // A panic inside the consumer must be observable, must distinguish
        // itself from a graceful exit, and must carry the in-flight job's
        // source and target — the same values the supervisor logs at error
        // level.
        let (repo, _dir) = setup_repo("t21-exit-panic.db");
        let source = "https://src.example/boom";
        let target = "https://nithitsuki.com/blog/t21-boom";
        let mock = MockFetcher::with_html(source, String::new());
        mock.set_panic(source);
        let (tx, rx) = channel(4);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (exit_tx, exit_rx) = watch::channel(None);
        let worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo, mock, None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx,
        );

        tx.send(job(source, target)).await.unwrap();

        let exit = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("a panicking consumer must not hang the supervisor")
            .expect("the supervisor itself must not panic");
        match &exit {
            WorkerExit::Panicked {
                source: s,
                target: t,
                message,
            } => {
                assert_eq!(s.as_deref(), Some(source), "panic must name the source");
                assert_eq!(t.as_deref(), Some(target), "panic must name the target");
                assert!(
                    message.contains("mock fetcher exploded"),
                    "panic payload must survive: {message}"
                );
            }
            other => panic!("a panic must not read as {other:?}"),
        }
        assert_eq!(
            *exit_rx.borrow(),
            Some(exit.clone()),
            "the shared signal (what /healthz and main read) must carry the panic"
        );
        assert_ne!(exit, WorkerExit::Graceful);
        assert_ne!(exit, WorkerExit::Cancelled);
    }

    /// Process-global capture of `tracing` output. A global subscriber is
    /// used instead of a thread-local one on purpose: `tracing` caches each
    /// callsite's interest against the global max level, and a thread-local
    /// default set after the supervisor's callsite first registered can miss
    /// the event when the full suite runs in parallel. The global subscriber
    /// makes callsite registration see the ERROR level, so the assertion is
    /// deterministic. The buffer is shared, so the test matches on values
    /// unique to its own panic.
    static CAPTURE_LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    static CAPTURE_INSTALLED: std::sync::Once = std::sync::Once::new();

    #[derive(Clone, Copy)]
    struct GlobalCaptureWriter;

    impl std::io::Write for GlobalCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            CAPTURE_LOG
                .lock()
                .expect("capture lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for GlobalCaptureWriter {
        type Writer = GlobalCaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            GlobalCaptureWriter
        }
    }

    fn capture_log() -> String {
        String::from_utf8_lossy(&CAPTURE_LOG.lock().expect("capture lock")).into_owned()
    }

    #[tokio::test]
    async fn panicked_worker_logs_error_with_job_identity() {
        // Install the capture subscriber once for the process. This test may
        // run before or after the other panic test and in parallel with it,
        // so it asserts on substrings unique to its own job below.
        CAPTURE_INSTALLED.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(GlobalCaptureWriter)
                .with_max_level(tracing::Level::ERROR)
                .with_target(false)
                .without_time()
                .finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
        CAPTURE_LOG.lock().expect("capture lock").clear();

        let (repo, _dir) = setup_repo("t21-log-panic.db");
        let source = "https://src.example/log-boom";
        let target = "https://nithitsuki.com/blog/t21-log-boom";
        let mock = MockFetcher::with_html(source, String::new());
        mock.set_panic(source);
        let (tx, rx) = channel(4);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo, mock, None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx(),
        );
        tx.send(job(source, target)).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("supervisor must resolve after the panic")
            .expect("supervisor must not panic");

        let log = capture_log();
        // `with_max_level(ERROR)` filters the subscriber to ERROR: a warn or
        // info log for this panic would leave the buffer without the line,
        // so the substring assertions below also prove the level.
        assert!(
            log.contains("ERROR"),
            "panic must log at error level: {log}"
        );
        assert!(
            log.contains("webmention worker panicked"),
            "log must name the death: {log}"
        );
        assert!(log.contains(source), "log must carry the source: {log}");
        assert!(log.contains(target), "log must carry the target: {log}");
        assert!(
            log.contains("mock fetcher exploded"),
            "log must carry the panic payload: {log}"
        );
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
        // The spawn loop hands jobs to processor.process.
        let (repo, _dir) = setup_repo("t20-spawn.db");
        let source = "https://src.example/spawn";
        let target = "https://nithitsuki.com/blog/t20-spawn";
        let mock = MockFetcher::with_html(source, mention_html(target, "Spawned"));
        let (tx, rx) = channel(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock, None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx(),
        );
        tx.send(job(source, target)).await.unwrap();
        wait_for_comment(&repo, source, "/blog/t20-spawn").await;
    }

    // ── E3: retry and shutdown-drain behavior ──────────────

    #[test]
    fn retry_delay_is_exponential_bounded_and_budgeted() {
        // The default policy is exponential and never exceeds the attempt
        // cap: attempts 1 and 2 get 250 ms and 500 ms, attempt 3 is dropped.
        let policy = RetryPolicy::DEFAULT;
        assert_eq!(policy.delay_for(1, Duration::ZERO), Some(RETRY_BASE_DELAY));
        assert_eq!(
            policy.delay_for(2, Duration::ZERO),
            Some(RETRY_BASE_DELAY * 2)
        );
        assert_eq!(
            policy.delay_for(MAX_JOB_ATTEMPTS, Duration::ZERO),
            None,
            "attempt cap drops the job"
        );

        // Cap branch: the delay never exceeds `max_delay` even at high
        // attempts (uses a policy wide enough to reach it).
        let capped = RetryPolicy {
            max_attempts: 40,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(250),
            total_budget: Duration::from_secs(3600),
        };
        assert_eq!(capped.delay_for(20, Duration::ZERO), Some(capped.max_delay));

        // Budget branch: a spent budget drops the job even with attempts
        // left.
        let budgeted = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            total_budget: Duration::from_millis(150),
        };
        assert_eq!(
            budgeted.delay_for(2, Duration::from_millis(120)),
            None,
            "elapsed + delay over the budget drops the job"
        );
    }

    #[test]
    fn error_classification_is_terminal_or_retryable() {
        // Terminal: a missing backlink, 410, other 4xx, bad/blocked URLs,
        // caps, and non-contention storage errors.
        assert!(!is_retryable(&WorkerError::NoBacklink));
        assert!(!is_retryable(&WorkerError::InvalidTarget("x".to_string())));
        assert!(!is_retryable(&WorkerError::OriginMismatch("x".to_string())));
        assert!(!is_retryable(&WorkerError::Fetch(FetchError::Gone(
            "x".to_string()
        ))));
        for status in [400, 403, 404, 410, 429] {
            assert!(
                !is_retryable(&WorkerError::Fetch(FetchError::HttpStatus {
                    url: "https://x.example/".to_string(),
                    status,
                })),
                "4xx {status} is terminal"
            );
        }
        assert!(!is_retryable(&WorkerError::Fetch(FetchError::Blocked(
            "x".to_string()
        ))));
        assert!(!is_retryable(&WorkerError::Fetch(FetchError::TooLarge {
            url: "x".to_string(),
            limit: 10,
        })));
        assert!(!is_retryable(&WorkerError::Repo(
            crate::db::RepoError::Constraint("x".to_string())
        )));

        // Retryable: 5xx and transient storage contention.
        for status in [500, 502, 503, 504] {
            assert!(
                is_retryable(&WorkerError::Fetch(FetchError::HttpStatus {
                    url: "https://x.example/".to_string(),
                    status,
                })),
                "5xx {status} is retryable"
            );
        }
        assert!(is_retryable(&WorkerError::Repo(
            crate::db::RepoError::Busy("x".to_string())
        )));
        assert!(is_retryable(&WorkerError::Repo(crate::db::RepoError::Io(
            "x".to_string()
        ))));
    }

    #[tokio::test]
    async fn transport_error_is_retryable() {
        // A real reqwest transport error (invalid URL, no network touched)
        // classifies as retryable through the `FetchError::Http` arm.
        let source = Client::new()
            .get("http://")
            .send()
            .await
            .expect_err("invalid URL must fail to send");
        assert!(is_retryable(&WorkerError::Fetch(FetchError::Http {
            url: "http://".to_string(),
            source,
        })));
    }

    #[tokio::test]
    async fn retryable_error_is_retried_then_succeeds() {
        // A transient 503 is retried once and then succeeds; the comment
        // lands and the URL is fetched exactly twice.
        let (repo, _dir) = setup_repo("t21-retry-success.db");
        let source = "https://src.example/retry";
        let target = "https://nithitsuki.com/blog/t21-retry";
        let mock = MockFetcher::with_html(source, mention_html(target, "Recovered"));
        mock.set_flaky(source, 1, mention_html(target, "Recovered"));
        let (tx, rx) = channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let _worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock.clone(), None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx(),
        );

        tx.send(job(source, target)).await.unwrap();
        wait_for_comment(&repo, source, "/blog/t21-retry").await;
        assert_eq!(
            mock.calls_for(source),
            2,
            "one retry after the transient 503"
        );
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn terminal_no_backlink_is_not_retried() {
        // `NoBacklink` is terminal: the source is fetched once, no retry.
        let (repo, _dir) = setup_repo("t21-terminal-nobacklink.db");
        let source = "https://src.example/noback";
        let target = "https://nithitsuki.com/blog/t21-noback";
        let mock = MockFetcher::with_html(source, unlink_html());
        let (tx, rx) = channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let _worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock.clone(), None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx(),
        );

        tx.send(job(source, target)).await.unwrap();
        // Wait past one retry delay: a retry would show as a second fetch.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            mock.calls_for(source),
            1,
            "terminal NoBacklink must not retry"
        );
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn gone_is_not_retried() {
        // A 410 is handled inside `process` (confirm-gone) and returns Ok:
        // the source is fetched exactly once and no retry is scheduled.
        let (repo, _dir) = setup_repo("t21-terminal-gone.db");
        let source = "https://src.example/gone";
        let target = "https://nithitsuki.com/blog/t21-gone";
        let mock = MockFetcher::with_html(source, mention_html(target, "x"));
        mock.set_gone(source);
        let (tx, rx) = channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let _worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock.clone(), None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx(),
        );

        tx.send(job(source, target)).await.unwrap();
        wait_until(|| mock.calls_for(source) == 1, "the gone fetch").await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(mock.calls_for(source), 1, "410 must not retry");
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn retry_cap_drops_the_job_after_max_attempts() {
        // A permanently failing source is attempted exactly
        // `MAX_JOB_ATTEMPTS` times, then dropped. The worker stays alive.
        let (repo, _dir) = setup_repo("t21-retry-cap.db");
        let bad = "https://src.example/always-503";
        let good = "https://src.example/after";
        let target = "https://nithitsuki.com/blog/t21-cap";
        let mock = MockFetcher::with_html(bad, mention_html(target, "x"));
        mock.set_status(bad, 503);
        mock.set_html(good, mention_html(target, "After"));
        let (tx, rx) = channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let _worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock.clone(), None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx(),
        );

        tx.send(job(bad, target)).await.unwrap();
        // Two retry waits (250 ms + 500 ms) plus slack.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            mock.calls_for(bad),
            MAX_JOB_ATTEMPTS,
            "the attempt cap must stop the retries"
        );

        // The worker is not wedged: a healthy job still flows.
        tx.send(job(good, target)).await.unwrap();
        wait_for_comment(&repo, good, "/blog/t21-cap").await;
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn full_queue_drops_the_retry_instead_of_deadlocking() {
        // The worker is the only consumer and the retry re-enters the same
        // bounded channel. With the single slot held by another job, the
        // retry must be dropped (try_send), never awaited (which would
        // deadlock). Bounded so a regression fails fast instead of hanging.
        tokio::time::timeout(Duration::from_secs(5), async {
            let (repo, _dir) = setup_repo("t21-full.db");
            let flaky = "https://src.example/flaky";
            let queued = "https://src.example/queued";
            let target = "https://nithitsuki.com/blog/t21-full";
            let mock = MockFetcher::with_html(flaky, mention_html(target, "Flaky"));
            mock.set_flaky(flaky, 1, mention_html(target, "Flaky"));
            mock.set_html(queued, mention_html(target, "Queued"));
            let (tx, rx) = channel(1);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let _worker = spawn_worker_for_processor(
                tx.clone(),
                rx,
                processor(repo.clone(), mock.clone(), None),
                shutdown_rx,
                RetryPolicy::DEFAULT,
                exit_tx(),
            );

            // The worker takes `flaky`; the send of `queued` fills the one
            // remaining slot while the worker backs off before its retry.
            tx.send(job(flaky, target)).await.unwrap();
            tx.send(job(queued, target)).await.unwrap();

            // `queued` must still be processed: the dropped retry must not
            // block the consumer.
            wait_for_comment(&repo, queued, "/blog/t21-full").await;
            assert_eq!(
                mock.calls_for(flaky),
                1,
                "the retry was dropped on the full queue, not sent"
            );
            let _ = shutdown_tx.send(true);
        })
        .await
        .expect("worker must not deadlock when a retry meets a full queue");
    }

    #[tokio::test]
    async fn shutdown_drains_buffered_jobs_before_exit() {
        // A job buffered behind an in-flight job is still processed after
        // the shutdown signal fires: the drain arm of the select must run.
        let (repo, _dir) = setup_repo("t21-drain-shutdown.db");
        let slow = "https://src.example/slow";
        let fast = "https://src.example/fast";
        let target = "https://nithitsuki.com/blog/t21-drain";
        let gate = Arc::new(tokio::sync::Notify::new());
        let mock = MockFetcher::with_html(fast, mention_html(target, "Fast"));
        mock.set_gated(slow, gate.clone(), mention_html(target, "Slow"));
        let (tx, rx) = channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let _worker = spawn_worker_for_processor(
            tx.clone(),
            rx,
            processor(repo.clone(), mock.clone(), None),
            shutdown_rx,
            RetryPolicy::DEFAULT,
            exit_tx(),
        );

        tx.send(job(slow, target)).await.unwrap();
        // The worker is now blocked inside the gated fetch.
        wait_until(|| mock.calls_for(slow) == 1, "the slow fetch to start").await;
        // Buffer the fast job behind it, then signal shutdown.
        tx.send(job(fast, target)).await.unwrap();
        shutdown_tx.send(true).unwrap();
        // Release the slow fetch so the loop can reach the shutdown arm.
        gate.notify_one();

        wait_for_comment(&repo, fast, "/blog/t21-drain").await;
    }

    #[tokio::test]
    async fn drain_respects_its_deadline() {
        // A drain over a job that never completes must return at the
        // deadline, not run forever. The outer bound turns a missing
        // deadline into a clean failure instead of a hung suite.
        let (repo, _dir) = setup_repo("t21-drain-deadline.db");
        let source = "https://src.example/hang";
        let target = "https://nithitsuki.com/blog/t21-hang";
        let gate = Arc::new(tokio::sync::Notify::new());
        let mock = MockFetcher::with_html(source, mention_html(target, "x"));
        mock.set_gated(source, gate.clone(), mention_html(target, "x"));
        let p = processor(repo.clone(), mock.clone(), None);
        let (tx, mut rx) = channel(1);
        tx.try_send(job(source, target)).unwrap();

        let start = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            drain_bounded(&mut rx, &p, Duration::from_millis(100)),
        )
        .await;
        assert!(outcome.is_ok(), "drain must return, not run forever");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "drain should use its budget: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "drain must respect its deadline: {elapsed:?}"
        );
    }
}
