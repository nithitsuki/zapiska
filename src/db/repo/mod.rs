use super::RepoError;
use super::pool::SqlitePool;

use serde::{Deserialize, Serialize};

type RepoResult<T> = Result<T, RepoError>;

// ── Data types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub target_path: String,
    pub comment_type: String,
    pub source_url: Option<String>,
    pub author_name: String,
    pub author_url: Option<String>,
    pub author_avatar: Option<String>,
    pub content: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// ID of the parent comment, if this is a reply. NULL means top-level.
    pub parent_id: Option<i64>,
    /// Nesting depth (0 = top-level, 1 = reply to top-level, etc.).
    /// Enforced at the application layer to cap at MAX_DEPTH.
    pub depth: i64,
    /// Set to true if the comment was caught by the honeypot anti-spam field.
    pub honeypot: bool,
    /// Optional token that lets the author delete their own comment.
    pub delete_token: Option<String>,
    /// Submitter IP address (only stored when STORE_IP_ADDRESS is enabled).
    pub submitter_ip: Option<String>,
    /// SHA-256 hash of submitter IP (only stored when STORE_IP_ADDRESS is enabled).
    pub submitter_ip_hash: Option<String>,
    /// SHA-256 hash of normalized content (for duplicate/spam detection).
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewComment {
    pub target_path: String,
    pub comment_type: String,
    pub source_url: Option<String>,
    pub author_name: String,
    pub author_url: Option<String>,
    pub author_avatar: Option<String>,
    pub content: String,
    /// ID of the parent comment, if this is a reply. NULL means top-level.
    pub parent_id: Option<i64>,
    /// Nesting depth (0 = top-level). Computed at insert time from the parent's depth.
    pub depth: i64,
    /// Set to true if the honeypot anti-spam field was triggered.
    pub honeypot: bool,
    /// Random token for self-service deletion. Returned to the author on submit.
    pub delete_token: Option<String>,
    /// Submitter IP address (only stored when STORE_IP_ADDRESS is enabled).
    pub submitter_ip: Option<String>,
    /// SHA-256 hash of submitter IP (only stored when STORE_IP_ADDRESS is enabled).
    pub submitter_ip_hash: Option<String>,
    /// SHA-256 hash of normalized content (for duplicate/spam detection).
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebmentionSeen {
    pub source: String,
    pub target: String,
    pub last_seen_at: String,
    pub last_status: String,
}

#[derive(Debug, Clone)]
pub struct NewWebmentionSeen {
    pub source: String,
    pub target: String,
    pub last_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubProfile {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: String,
    pub cached_at: String,
    pub valid: bool,
}

#[derive(Debug, Clone)]
pub struct NewGithubProfile {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: String,
    pub valid: bool,
}

// ── Repository ──────────────────────────────────────────────

#[derive(Clone)]
pub struct Repo {
    pool: SqlitePool,
    /// Pool acquisitions performed by this handle (shared across clones).
    /// Test-only accounting behind [`Repo::acquire_count`]: proves the T15
    /// units run on ONE connection instead of N round trips.
    acquire_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Repo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            acquire_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Expose the underlying pool for direct SQL (used in tests).
    #[doc(hidden)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Number of pool connections acquired through this handle (test probe).
    #[doc(hidden)]
    pub fn acquire_count(&self) -> u64 {
        self.acquire_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reset the acquire probe (test probe).
    #[doc(hidden)]
    pub fn reset_acquire_count(&self) {
        self.acquire_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    async fn spawn<F, T>(&self, f: F) -> RepoResult<T>
    where
        F: FnOnce(&rusqlite::Connection) -> RepoResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        let counter = self.acquire_count.clone();
        tokio::task::spawn_blocking(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let conn = pool
                .get()
                .map_err(|e| RepoError::Other(format!("pool acquire: {}", e)))?;
            f(&conn)
        })
        .await
        .map_err(|e| RepoError::Other(format!("spawn_blocking: {}", e)))?
    }

    // T15 seam — one pooled connection shared by N statements. Chose two
    // plain methods (`with_conn` = caller-owned statements on one connection,
    // `with_tx` = the same wrapped in `BEGIN IMMEDIATE`) over a `UnitOfWork`
    // struct because every caller needs at most one closure lifetime and no
    // state crosses an await: a struct would only re-borrow the connection.
    /// Run `f` on ONE pooled connection (one acquire, many statements).
    /// Read batches (export snapshot, list+count+counts) share the connection
    /// without taking a write lock; wrap in an explicit `conn.transaction()`
    /// for a single WAL read snapshot.
    pub async fn with_conn<F, T>(&self, f: F) -> RepoResult<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> RepoResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        let counter = self.acquire_count.clone();
        tokio::task::spawn_blocking(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut conn = pool
                .get()
                .map_err(|e| RepoError::Other(format!("pool acquire: {}", e)))?;
            f(&mut conn)
        })
        .await
        .map_err(|e| RepoError::Other(format!("spawn_blocking: {}", e)))?
    }

    /// Run `f` inside one `BEGIN IMMEDIATE` transaction on ONE connection.
    /// Writers serialize at `BEGIN`: contention surfaces as the T14 `Busy`
    /// variant (after the pool's `busy_timeout`) for callers to retry with
    /// backoff — never a hang. `Err` rolls back, `Ok` commits.
    /// Do not open transactions inside `f` (no savepoints): nesting fails
    /// loud as `RepoError::Other` — call the `*_on_conn` helpers instead.
    pub async fn with_tx<F, T>(&self, f: F) -> RepoResult<T>
    where
        F: FnOnce(&rusqlite::Transaction) -> RepoResult<T> + Send + 'static,
        T: Send + 'static,
    {
        self.with_conn(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(RepoError::from)?;
            let out = f(&tx)?;
            tx.commit().map_err(RepoError::from)?;
            Ok(out)
        })
        .await
    }

    /// Whole-database read snapshot for the admin export: all five tables
    /// from ONE connection inside one deferred read transaction, so a
    /// comment created mid-export cannot appear in only some arrays (B-22).
    /// WAL makes the shared read lock cheap. Import stays per-row (T17).
    pub async fn export_snapshot(&self) -> RepoResult<ExportSnapshot> {
        self.with_conn(move |conn| {
            let tx = conn.transaction().map_err(RepoError::from)?;
            let comments = comments::list_all_comments_on_conn(&tx)?;
            let seen = webmentions::list_all_seen_on_conn(&tx)?;
            let urls = urls::list_all_urls_on_conn(&tx)?;
            let profiles = github_profiles::list_all_profiles_on_conn(&tx)?;
            let reactions = reactions::list_all_reactions_on_conn(&tx)?;
            tx.commit().map_err(RepoError::from)?;
            Ok(ExportSnapshot {
                comments,
                seen,
                urls,
                profiles,
                reactions,
            })
        })
        .await
    }
}

/// One-connection export read (see [`Repo::export_snapshot`]).
#[derive(Debug)]
pub struct ExportSnapshot {
    pub comments: Vec<Comment>,
    pub seen: Vec<WebmentionSeen>,
    pub urls: Vec<CommentUrl>,
    pub profiles: Vec<GithubProfile>,
    pub reactions: Vec<CommentReaction>,
}

/// What a failing extracted-URL row means for its comment transaction.
/// URLs are derived moderation-lookup data, never the primary record: an
/// invalid ROW (Constraint) is skipped with a warn log and the comment still
/// commits; a STORAGE failure (Busy/Io/Other, T14) aborts the whole unit so
/// no torn comment-without-URLs survives silently (B-7/B-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlErrorAction {
    SkipRow,
    Abort,
}

pub(crate) fn url_error_policy(err: &RepoError) -> UrlErrorAction {
    match err {
        RepoError::Constraint(_) => UrlErrorAction::SkipRow,
        RepoError::Busy(_) | RepoError::Io(_) | RepoError::Other(_) | RepoError::NotFound(_) => {
            UrlErrorAction::Abort
        }
    }
}

// ── Sub-modules ─────────────────────────────────────────────

mod comments;
mod github_profiles;
mod reactions;
mod urls;
mod webmentions;

pub use reactions::{CommentReaction, ReactionWithComment};
pub use urls::{CommentUrl, UrlCommentRef, UrlStats};

// ── Helpers ─────────────────────────────────────────────────

/// Map a SQLite row to a Comment. The SELECT column order must be:
///   id, target_path, comment_type, source_url, author_name, author_url,
///   author_avatar, content, status, created_at, updated_at, parent_id, depth,
///   honeypot, delete_token, submitter_ip, content_hash, submitter_ip_hash
///
/// `COMMENT_COLUMNS` is the single source of truth for that list: every
/// comment read builds its SELECT from it, so adding a column is a one-line
/// change here plus the mapper below instead of ~12 SQL string edits.
const COMMENT_COLUMNS: &str = "id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip, content_hash, submitter_ip_hash";

/// Build a comment read query from one predicate template. Optional filters
/// use `(?N IS NULL OR ...)` so one prepared statement covers both the
/// filtered and unfiltered cases — no dual-query cursor branches.
fn select_comments(where_clause: &str, order: &str) -> String {
    format!("SELECT {COMMENT_COLUMNS} FROM comments WHERE {where_clause} ORDER BY {order}")
}

fn row_to_comment(row: &rusqlite::Row) -> rusqlite::Result<Comment> {
    Ok(Comment {
        id: row.get(0)?,
        target_path: row.get(1)?,
        comment_type: row.get(2)?,
        source_url: row.get(3)?,
        author_name: row.get(4)?,
        author_url: row.get(5)?,
        author_avatar: row.get(6)?,
        content: row.get(7)?,
        status: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        parent_id: row.get(11)?,
        depth: row.get(12)?,
        honeypot: row.get::<_, i64>(13)? != 0,
        delete_token: row.get(14)?,
        submitter_ip: row.get(15)?,
        content_hash: row.get(16)?,
        submitter_ip_hash: row.get(17)?,
    })
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod t15_seam_tests {
    use super::*;
    use crate::db::pool::{create_pool, run_migrations};

    fn setup_repo() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t15_seam.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    fn native_comment(target: &str) -> NewComment {
        NewComment {
            target_path: target.to_string(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: "T15".to_string(),
            author_url: None,
            author_avatar: None,
            content: "seam probe".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
        }
    }

    #[tokio::test]
    async fn with_tx_commits_on_ok() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .with_tx(move |tx| {
                tx.execute(
                    "INSERT INTO comments (target_path, comment_type, author_name, content)
                     VALUES ('/t15-tx', 'native', 'T15', 'tx commit')",
                    [],
                )
                .map_err(RepoError::from)?;
                Ok(tx.last_insert_rowid())
            })
            .await
            .unwrap();
        assert!(id > 0);
        assert!(repo.get_comment(id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn with_tx_rolls_back_on_error_leaving_no_rows() {
        let (repo, _dir) = setup_repo();
        let before: i64 = repo
            .with_conn(move |conn| {
                conn.query_row("SELECT count(*) FROM comments", [], |r| r.get(0))
                    .map_err(RepoError::from)
            })
            .await
            .unwrap();
        let err = repo
            .with_tx(move |tx| {
                tx.execute(
                    "INSERT INTO comments (target_path, comment_type, author_name, content)
                     VALUES ('/t15-rollback', 'native', 'T15', 'must vanish')",
                    [],
                )
                .map_err(RepoError::from)?;
                tx.execute(
                    "INSERT INTO comment_urls (comment_id, url, domain, url_hash)
                     VALUES (999999, 'https://x.example/', 'x.example', 'h')",
                    [],
                )
                .map_err(RepoError::from)?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Constraint(_)));
        let after: i64 = repo
            .with_conn(move |conn| {
                conn.query_row("SELECT count(*) FROM comments", [], |r| r.get(0))
                    .map_err(RepoError::from)
            })
            .await
            .unwrap();
        assert_eq!(before, after, "rolled-back insert must leave no comment");
        let ghosts = repo
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM comments WHERE target_path = '/t15-rollback'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(RepoError::from)
            })
            .await
            .unwrap();
        assert_eq!(ghosts, 0);
    }

    #[tokio::test]
    async fn with_conn_runs_on_a_single_acquire() {
        let (repo, _dir) = setup_repo();
        repo.reset_acquire_count();
        repo.with_conn(move |conn| {
            conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .map_err(RepoError::from)?;
            conn.query_row("SELECT 2", [], |r| r.get::<_, i64>(0))
                .map_err(RepoError::from)?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(repo.acquire_count(), 1);
    }

    #[tokio::test]
    async fn with_tx_runs_on_a_single_acquire() {
        let (repo, _dir) = setup_repo();
        repo.reset_acquire_count();
        repo.with_tx(move |tx| {
            tx.execute(
                "INSERT INTO comments (target_path, comment_type, author_name, content)
                     VALUES ('/t15-acq', 'native', 'T15', 'one acquire')",
                [],
            )
            .map_err(RepoError::from)?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(repo.acquire_count(), 1);
    }

    #[tokio::test]
    async fn busy_error_surfaces_as_busy_variant_for_retry_callers() {
        let err = RepoError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_string()),
        ));
        assert!(
            matches!(err, RepoError::Busy(_)),
            "BEGIN IMMEDIATE contention must surface as Busy so callers retry with backoff"
        );
        // A closure-level Busy propagates through with_tx untouched (no commit).
        let (repo, _dir) = setup_repo();
        let err = repo
            .with_tx(move |_tx| Err::<(), _>(RepoError::Busy("locked".to_string())))
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Busy(_)));
    }

    #[tokio::test]
    async fn concurrent_with_tx_serializes_without_hang() {
        let (repo, _dir) = setup_repo();
        let mut handles = Vec::new();
        for i in 0..4 {
            let r = repo.clone();
            handles.push(tokio::spawn(async move {
                r.with_tx(move |tx| {
                    tx.execute(
                        "INSERT INTO comments (target_path, comment_type, author_name, content)
                         VALUES ('/t15-conc', 'native', 'T15', ?1)",
                        rusqlite::params![format!("writer {i}")],
                    )
                    .map_err(RepoError::from)?;
                    Ok(())
                })
                .await
            }));
        }
        let guarded = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            for h in handles {
                h.await.unwrap().unwrap();
            }
        })
        .await;
        assert!(
            guarded.is_ok(),
            "concurrent IMMEDIATE writers must serialize, not hang"
        );
        let n: i64 = repo
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM comments WHERE target_path = '/t15-conc'",
                    [],
                    |r| r.get(0),
                )
                .map_err(RepoError::from)
            })
            .await
            .unwrap();
        assert_eq!(n, 4);
    }

    #[tokio::test]
    async fn export_snapshot_is_one_acquire_and_matches_five_reads() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .insert_comment(native_comment("/t15-exp"))
            .await
            .unwrap();
        repo.update_status(id, "approved").await.unwrap();
        repo.insert_urls(
            id,
            vec![(
                "https://u.example/".to_string(),
                "u.example".to_string(),
                "h1".to_string(),
            )],
        )
        .await
        .unwrap();
        repo.upsert_webmention_seen(NewWebmentionSeen {
            source: "https://s.example/p".to_string(),
            target: "https://site.example/t15-exp".to_string(),
            last_status: "alive".to_string(),
        })
        .await
        .unwrap();
        repo.upsert_github_profile(NewGithubProfile {
            login: "t15user".to_string(),
            name: None,
            avatar_url: "https://avatars.example/u/1".to_string(),
            valid: true,
        })
        .await
        .unwrap();
        let (rid, _) = repo.upsert_reaction(id, "👍", "h:t15").await.unwrap();
        repo.update_reaction_status(rid, "approved").await.unwrap();

        repo.reset_acquire_count();
        let snap = repo.export_snapshot().await.unwrap();
        assert_eq!(
            repo.acquire_count(),
            1,
            "export must run its five reads on ONE connection"
        );
        assert_eq!(
            snap.comments.len(),
            repo.list_all_comments().await.unwrap().len()
        );
        assert_eq!(
            snap.seen.len(),
            repo.list_all_webmention_seen().await.unwrap().len()
        );
        assert_eq!(
            snap.urls.len(),
            repo.list_all_comment_urls().await.unwrap().len()
        );
        assert_eq!(
            snap.profiles.len(),
            repo.list_all_github_profiles().await.unwrap().len()
        );
        assert_eq!(
            snap.reactions.len(),
            repo.list_all_comment_reactions().await.unwrap().len()
        );
        assert!(!snap.comments.is_empty());
    }

    #[tokio::test]
    async fn native_create_is_one_acquire_and_atomic() {
        let (repo, _dir) = setup_repo();
        repo.reset_acquire_count();
        let id = repo
            .create_native_comment(
                native_comment("/t15-native"),
                true,
                vec![(
                    "https://n.example/".to_string(),
                    "n.example".to_string(),
                    "hn".to_string(),
                )],
            )
            .await
            .unwrap();
        assert_eq!(repo.acquire_count(), 1);
        let c = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(c.status, "approved");
        assert_eq!(repo.get_comment_urls(id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn webmention_upsert_with_seen_is_one_acquire_and_atomic() {
        let (repo, _dir) = setup_repo();
        let mut c = native_comment("/t15-wm");
        c.comment_type = "webmention".to_string();
        c.source_url = Some("https://src.example/t15".to_string());
        repo.reset_acquire_count();
        let id = repo
            .upsert_webmention_with_seen(
                c,
                NewWebmentionSeen {
                    source: "https://src.example/t15".to_string(),
                    target: "https://site.example/t15-wm".to_string(),
                    last_status: "alive".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(repo.acquire_count(), 1);
        assert!(repo.get_comment(id).await.unwrap().is_some());
        let seen = repo
            .get_webmention_seen("https://src.example/t15", "https://site.example/t15-wm")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.last_status, "alive");
    }

    #[tokio::test]
    async fn webmention_upsert_with_seen_rolls_back_both_on_mid_unit_failure() {
        let (repo, _dir) = setup_repo();
        // The seen row carries a CHECK (alive|gone): a bogus status fails the
        // SECOND statement, after the comment upsert already ran — a forced
        // mid-unit failure. Both writes must vanish.
        let mut c = native_comment("/t15-wm-fail");
        c.comment_type = "webmention".to_string();
        c.source_url = Some("https://src.example/t15-fail".to_string());
        let err = repo
            .upsert_webmention_with_seen(
                c,
                NewWebmentionSeen {
                    source: "https://src.example/t15-fail".to_string(),
                    target: "https://site.example/t15-wm-fail".to_string(),
                    last_status: "bogus-status".to_string(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Constraint(_)));
        assert!(
            repo.get_comment_by_source("https://src.example/t15-fail")
                .await
                .unwrap()
                .is_none(),
            "rolled-back mention must be absent"
        );
        assert!(
            repo.get_webmention_seen(
                "https://src.example/t15-fail",
                "https://site.example/t15-wm-fail"
            )
            .await
            .unwrap()
            .is_none(),
            "rolled-back seen must be absent"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::pool::create_pool;
    use crate::db::pool::run_migrations;
    use tempfile::tempdir;

    fn setup_repo() -> (Repo, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("repo_test.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    // ── Comment tests ──

    #[tokio::test]
    async fn insert_and_get_comment() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .insert_comment(NewComment {
                target_path: "/blog/hello".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Alice".to_string(),
                author_url: Some("https://alice.blog".to_string()),
                author_avatar: None,
                content: "Great post!".to_string(),
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
        let comment = repo.get_comment(id).await.unwrap().expect("comment exists");
        assert_eq!(comment.author_name, "Alice");
        assert_eq!(comment.status, "pending");
        assert_eq!(comment.target_path, "/blog/hello");
    }

    #[tokio::test]
    async fn comment_status_defaults_to_pending() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .insert_comment(NewComment {
                target_path: "/test".to_string(),
                comment_type: "webmention".to_string(),
                source_url: Some("https://remote.example/post".to_string()),
                author_name: "Bob".to_string(),
                author_url: None,
                author_avatar: None,
                content: "mentioned this".to_string(),
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
        let comment = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(comment.status, "pending");
    }

    #[tokio::test]
    async fn upsert_by_source_creates_new() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .upsert_by_source(NewComment {
                target_path: "/post".to_string(),
                comment_type: "webmention".to_string(),
                source_url: Some("https://remote.example/source".to_string()),
                author_name: "Charlie".to_string(),
                author_url: None,
                author_avatar: None,
                content: "first mention".to_string(),
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
        let comment = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(comment.content, "first mention");
    }

    #[tokio::test]
    async fn upsert_by_source_overwrites_preserving_approved_status() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .upsert_by_source(NewComment {
                target_path: "/post".to_string(),
                comment_type: "webmention".to_string(),
                source_url: Some("https://remote.example/source".to_string()),
                author_name: "Charlie".to_string(),
                author_url: None,
                author_avatar: None,
                content: "first mention".to_string(),
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
        let id2 = repo
            .upsert_by_source(NewComment {
                target_path: "/post".to_string(),
                comment_type: "webmention".to_string(),
                source_url: Some("https://remote.example/source".to_string()),
                author_name: "Charlie".to_string(),
                author_url: Some("https://charlie.blog".to_string()),
                author_avatar: Some("https://charlie.blog/photo.jpg".to_string()),
                content: "updated mention with richer data".to_string(),
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
        assert_eq!(id, id2);
        let comment = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(comment.status, "approved");
        assert_eq!(comment.content, "updated mention with richer data");
        assert_eq!(comment.author_url, Some("https://charlie.blog".to_string()));
    }

    #[tokio::test]
    async fn list_approved_returns_only_approved() {
        let (repo, _dir) = setup_repo();
        let id1 = repo
            .insert_comment(NewComment {
                target_path: "/page".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "A".to_string(),
                author_url: None,
                author_avatar: None,
                content: "one".to_string(),
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
        let id2 = repo
            .insert_comment(NewComment {
                target_path: "/page".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "B".to_string(),
                author_url: None,
                author_avatar: None,
                content: "two".to_string(),
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
        let _id3 = repo
            .insert_comment(NewComment {
                target_path: "/page".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "C".to_string(),
                author_url: None,
                author_avatar: None,
                content: "three".to_string(),
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
        repo.update_status(id1, "approved").await.unwrap();
        repo.update_status(id2, "spam").await.unwrap();
        let approved = repo.list_approved("/page", 100, None).await.unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, id1);
    }

    #[tokio::test]
    async fn list_approved_pagination() {
        let (repo, _dir) = setup_repo();
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = repo
                .insert_comment(NewComment {
                    target_path: "/paginated".to_string(),
                    comment_type: "native".to_string(),
                    source_url: None,
                    author_name: format!("User{i}"),
                    author_url: None,
                    author_avatar: None,
                    content: format!("comment {i}"),
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
            ids.push(id);
        }
        let page = repo.list_approved("/paginated", 2, None).await.unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, 5);
        assert_eq!(page[1].id, 4);
        let page2 = repo.list_approved("/paginated", 2, Some(4)).await.unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].id, 3);
        assert_eq!(page2[1].id, 2);
    }

    #[tokio::test]
    async fn list_approved_oldest_ascending_with_after_cursor() {
        let (repo, _dir) = setup_repo();
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = repo
                .insert_comment(NewComment {
                    target_path: "/oldest".to_string(),
                    comment_type: "native".to_string(),
                    source_url: None,
                    author_name: format!("User{i}"),
                    author_url: None,
                    author_avatar: None,
                    content: format!("comment {i}"),
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
            ids.push(id);
        }
        // Ascending order: oldest (smallest id) first.
        let page = repo.list_approved_oldest("/oldest", 2, None).await.unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, 1);
        assert_eq!(page[1].id, 2);
        // After-cursor: id > 2 → next two.
        let page2 = repo
            .list_approved_oldest("/oldest", 2, Some(2))
            .await
            .unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].id, 3);
        assert_eq!(page2[1].id, 4);
        // Only approved comments are returned.
        let _ = ids;
    }

    #[tokio::test]
    async fn count_approved_reflects_approved_only() {
        let (repo, _dir) = setup_repo();
        repo.insert_comment(NewComment {
            target_path: "/count-test".to_string(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: "A".to_string(),
            author_url: None,
            author_avatar: None,
            content: "a".to_string(),
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
        let id = repo
            .insert_comment(NewComment {
                target_path: "/count-test".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "B".to_string(),
                author_url: None,
                author_avatar: None,
                content: "b".to_string(),
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
        assert_eq!(repo.count_approved("/count-test").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn list_pending_newest_first() {
        let (repo, _dir) = setup_repo();
        let _id1 = repo
            .insert_comment(NewComment {
                target_path: "/mod".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "First".to_string(),
                author_url: None,
                author_avatar: None,
                content: "first".to_string(),
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
        let _id2 = repo
            .insert_comment(NewComment {
                target_path: "/mod".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Second".to_string(),
                author_url: None,
                author_avatar: None,
                content: "second".to_string(),
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
        let pending = repo.list_pending(10, None, None).await.unwrap();
        assert!(pending.len() >= 2);
        assert_eq!(pending[0].author_name, "Second");
        assert_eq!(pending[1].author_name, "First");
    }

    #[tokio::test]
    async fn update_status_approve_then_spam() {
        let (repo, _dir) = setup_repo();
        let id = repo
            .insert_comment(NewComment {
                target_path: "/m".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "ModMe".to_string(),
                author_url: None,
                author_avatar: None,
                content: "moderate me".to_string(),
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
        assert_eq!(
            repo.get_comment(id).await.unwrap().unwrap().status,
            "approved"
        );
        repo.update_status(id, "spam").await.unwrap();
        assert_eq!(repo.get_comment(id).await.unwrap().unwrap().status, "spam");
        repo.update_status(id, "deleted").await.unwrap();
        assert_eq!(
            repo.get_comment(id).await.unwrap().unwrap().status,
            "deleted"
        );
    }

    #[tokio::test]
    async fn update_status_nonexistent_returns_not_found() {
        let (repo, _dir) = setup_repo();
        let err = repo.update_status(999, "approved").await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)));
    }

    #[tokio::test]
    async fn sql_injection_attempt_fails_safely() {
        let (repo, _dir) = setup_repo();
        let malicious_content = "'; DROP TABLE comments;--".to_string();
        let id = repo
            .insert_comment(NewComment {
                target_path: "/sqli-test".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Hacker".to_string(),
                author_url: None,
                author_avatar: None,
                content: malicious_content.clone(),
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
        let comment = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(comment.content, malicious_content);
        let id2 = repo
            .insert_comment(NewComment {
                target_path: "/sqli-test".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Survivor".to_string(),
                author_url: None,
                author_avatar: None,
                content: "still alive".to_string(),
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
        assert!(id2 > 0);
    }

    #[tokio::test]
    async fn multiple_concurrent_spawns_dont_panic() {
        let (repo, _dir) = setup_repo();
        let mut handles = Vec::new();
        for i in 0..10 {
            let r = repo.clone();
            handles.push(tokio::spawn(async move {
                r.insert_comment(NewComment {
                    target_path: "/concurrent".to_string(),
                    comment_type: "native".to_string(),
                    source_url: None,
                    author_name: format!("Concurrent{i}"),
                    author_url: None,
                    author_avatar: None,
                    content: "hello".to_string(),
                    parent_id: None,
                    depth: 0,
                    honeypot: false,
                    delete_token: None,
                    submitter_ip: None,
                    submitter_ip_hash: None,
                    content_hash: None,
                })
                .await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        let count = repo.count_approved("/concurrent").await.unwrap();
        assert_eq!(count, 0);
    }

    // ── Webmention seen tests ──

    #[tokio::test]
    async fn webmention_seen_upsert_and_read() {
        let (repo, _dir) = setup_repo();
        repo.upsert_webmention_seen(NewWebmentionSeen {
            source: "https://src.example/post".to_string(),
            target: "https://nithitsuki.com/blog".to_string(),
            last_status: "alive".to_string(),
        })
        .await
        .unwrap();
        let seen = repo
            .get_webmention_seen("https://src.example/post", "https://nithitsuki.com/blog")
            .await
            .unwrap()
            .expect("exists");
        assert_eq!(seen.last_status, "alive");
        repo.upsert_webmention_seen(NewWebmentionSeen {
            source: "https://src.example/post".to_string(),
            target: "https://nithitsuki.com/blog".to_string(),
            last_status: "gone".to_string(),
        })
        .await
        .unwrap();
        let seen = repo
            .get_webmention_seen("https://src.example/post", "https://nithitsuki.com/blog")
            .await
            .unwrap()
            .expect("exists after update");
        assert_eq!(seen.last_status, "gone");
    }

    #[tokio::test]
    async fn webmention_seen_not_found_returns_none() {
        let (repo, _dir) = setup_repo();
        let seen = repo
            .get_webmention_seen("https://unknown.example", "https://nithitsuki.com/x")
            .await
            .unwrap();
        assert!(seen.is_none());
    }

    // ── GitHub profile tests ──

    #[tokio::test]
    async fn github_profile_upsert_and_read() {
        let (repo, _dir) = setup_repo();
        repo.upsert_github_profile(NewGithubProfile {
            login: "alice".to_string(),
            name: Some("Alice Green".to_string()),
            avatar_url: "https://avatars.githubusercontent.com/u/123".to_string(),
            valid: true,
        })
        .await
        .unwrap();
        let profile = repo
            .get_github_profile("alice")
            .await
            .unwrap()
            .expect("exists");
        assert_eq!(profile.name, Some("Alice Green".to_string()));
        assert!(profile.valid);
        repo.upsert_github_profile(NewGithubProfile {
            login: "nonexistent999".to_string(),
            name: None,
            avatar_url: "".to_string(),
            valid: false,
        })
        .await
        .unwrap();
        let neg = repo
            .get_github_profile("nonexistent999")
            .await
            .unwrap()
            .expect("negative cache entry exists");
        assert!(!neg.valid);
    }

    #[tokio::test]
    async fn github_profile_not_found_returns_none() {
        let (repo, _dir) = setup_repo();
        let profile = repo.get_github_profile("nobody").await.unwrap();
        assert!(profile.is_none());
    }

    // ── Reaction tests ──

    async fn seed_approved(repo: &Repo) -> i64 {
        let id = repo
            .insert_comment(NewComment {
                target_path: "/react".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Alice".to_string(),
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
        repo.update_status(id, "approved").await.unwrap();
        id
    }

    #[tokio::test]
    async fn reaction_upsert_new_change_and_noop() {
        let (repo, _dir) = setup_repo();
        let comment_id = seed_approved(&repo).await;

        // New reaction → pending, changed.
        let (id, changed) = repo
            .upsert_reaction(comment_id, "👍", "h:abc")
            .await
            .unwrap();
        assert!(changed);
        let reaction = repo.get_reaction(id).await.unwrap().unwrap();
        assert_eq!(reaction.status, "pending");
        assert_eq!(reaction.reaction, "👍");

        // Same reaction again → no-op, moderation state preserved.
        let (id2, changed) = repo
            .upsert_reaction(comment_id, "👍", "h:abc")
            .await
            .unwrap();
        assert_eq!(id, id2);
        assert!(!changed);

        // Change emoji → update + reset to pending.
        let (id3, changed) = repo
            .upsert_reaction(comment_id, "❤️", "h:abc")
            .await
            .unwrap();
        assert_eq!(id, id3);
        assert!(changed);
        let reaction = repo.get_reaction(id).await.unwrap().unwrap();
        assert_eq!(reaction.reaction, "❤️");
        assert_eq!(reaction.status, "pending");

        // After approval, re-clicking the same emoji keeps it approved.
        repo.update_reaction_status(id, "approved").await.unwrap();
        let (_, changed) = repo
            .upsert_reaction(comment_id, "❤️", "h:abc")
            .await
            .unwrap();
        assert!(!changed);
        assert_eq!(
            repo.get_reaction(id).await.unwrap().unwrap().status,
            "approved"
        );

        // A spam row allows a fresh reaction from the same identifier.
        repo.update_reaction_status(id, "spam").await.unwrap();
        let (_, changed) = repo
            .upsert_reaction(comment_id, "👍", "h:abc")
            .await
            .unwrap();
        assert!(changed);
        assert_eq!(
            repo.get_reaction(id).await.unwrap().unwrap().status,
            "pending"
        );
    }

    #[tokio::test]
    async fn reaction_delete_removes_active_only() {
        let (repo, _dir) = setup_repo();
        let comment_id = seed_approved(&repo).await;
        repo.upsert_reaction(comment_id, "👍", "h:abc")
            .await
            .unwrap();
        assert!(repo.delete_reaction(comment_id, "h:abc").await.unwrap());
        assert!(!repo.delete_reaction(comment_id, "h:abc").await.unwrap());
        // Deleted rows don't count as active.
        let counts = repo.reaction_counts(&[comment_id]).await.unwrap();
        assert!(counts.is_empty());
    }

    #[tokio::test]
    async fn reaction_counts_only_approved() {
        let (repo, _dir) = setup_repo();
        let c1 = seed_approved(&repo).await;
        let c2 = seed_approved(&repo).await;

        // c1: two approved 👍 + one pending ❤️; c2: one approved 👍.
        let (r1, _) = repo.upsert_reaction(c1, "👍", "h:one").await.unwrap();
        let (r2, _) = repo.upsert_reaction(c1, "👍", "h:two").await.unwrap();
        let (r3, _) = repo.upsert_reaction(c1, "❤️", "h:three").await.unwrap();
        let (r4, _) = repo.upsert_reaction(c2, "👍", "h:one").await.unwrap();
        for id in [r1, r2, r4] {
            repo.update_reaction_status(id, "approved").await.unwrap();
        }
        let _ = r3; // stays pending

        let counts = repo.reaction_counts(&[c1, c2]).await.unwrap();
        assert_eq!(counts[&c1].get("👍"), Some(&2));
        assert_eq!(counts[&c1].get("❤️"), None, "pending reactions excluded");
        assert_eq!(counts[&c2].get("👍"), Some(&1));
        assert!(repo.reaction_counts(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reaction_list_filters_by_status_with_context() {
        let (repo, _dir) = setup_repo();
        let comment_id = seed_approved(&repo).await;
        let (id, _) = repo
            .upsert_reaction(comment_id, "😄", "h:abc")
            .await
            .unwrap();
        let pending = repo
            .list_reactions(Some("pending"), 10, None)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].target_path, "/react");
        assert_eq!(pending[0].comment_author, "Alice");
        assert_eq!(pending[0].comment_status, "approved");
        assert_eq!(pending[0].reaction, "😄");
        assert!(
            repo.list_reactions(Some("approved"), 10, None)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
