use super::RepoError;
use super::pool::SqlitePool;

type RepoResult<T> = Result<T, RepoError>;

// ── Data types ──────────────────────────────────────────────

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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
pub struct CommentsRepo {
    pool: SqlitePool,
}

impl CommentsRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Expose the underlying pool for direct SQL (used in tests).
    #[doc(hidden)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn spawn<F, T>(&self, f: F) -> RepoResult<T>
    where
        F: FnOnce(&rusqlite::Connection) -> RepoResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool
                .get()
                .map_err(|e| RepoError::Internal(format!("pool acquire: {}", e)))?;
            f(&conn)
        })
        .await
        .map_err(|e| RepoError::Internal(format!("spawn_blocking: {}", e)))?
    }
}

// ── Sub-modules ─────────────────────────────────────────────

mod comments;
mod github_profiles;
mod urls;
mod webmentions;

// ── Helpers ─────────────────────────────────────────────────

/// Map a SQLite row to a Comment. The SELECT column order must be:
///   id, target_path, comment_type, source_url, author_name, author_url,
///   author_avatar, content, status, created_at, updated_at, parent_id, depth,
///   honeypot, delete_token, submitter_ip, content_hash, submitter_ip_hash
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
mod tests {
    use super::*;
    use crate::db::pool::create_pool;
    use crate::db::pool::run_migrations;
    use tempfile::tempdir;

    fn setup_repo() -> (CommentsRepo, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("repo_test.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();
        (CommentsRepo::new(pool), dir)
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
}
