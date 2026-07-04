use rusqlite::OptionalExtension;
use rusqlite::params;

use super::RepoError;
use super::pool::SqlitePool;

type RepoResult<T> = Result<T, RepoError>;

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

    // ── Comments ──────────────────────────────────────────────

    pub async fn insert_comment(&self, input: NewComment) -> RepoResult<i64> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO comments (target_path, comment_type, source_url, author_name, author_url, author_avatar, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    input.target_path,
                    input.comment_type,
                    input.source_url,
                    input.author_name,
                    input.author_url,
                    input.author_avatar,
                    input.content,
                ],
            )
            .map_err(|e| RepoError::Internal(e.to_string()))?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn upsert_by_source(&self, input: NewComment) -> RepoResult<i64> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO comments (target_path, comment_type, source_url, author_name, author_url, author_avatar, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(source_url, target_path)
                 WHERE source_url IS NOT NULL
                 DO UPDATE SET
                     author_name = excluded.author_name,
                     author_url = excluded.author_url,
                     author_avatar = excluded.author_avatar,
                     content = excluded.content,
                     updated_at = datetime('now')",
                params![
                    input.target_path,
                    input.comment_type,
                    input.source_url,
                    input.author_name,
                    input.author_url,
                    input.author_avatar,
                    input.content,
                ],
            )
            .map_err(|e| RepoError::Internal(e.to_string()))?;

            // SQLite's last_insert_rowid() returns the rowid of the inserted row
            // (new insert) or the rowid of the conflicting row (UPDATE branch).
            // It is always > 0 after a successful UPSERT.
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn list_approved(
        &self,
        path: &str,
        limit: i64,
        before: Option<i64>,
    ) -> RepoResult<Vec<Comment>> {
        let path = path.to_string();
        self.spawn(move |conn| {
            let mut stmt = if let Some(_cursor) = before {
                conn.prepare(
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                     FROM comments
                     WHERE target_path = ?1 AND status = 'approved' AND id < ?2
                     ORDER BY id DESC
                     LIMIT ?3",
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?
            } else {
                conn.prepare(
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                     FROM comments
                     WHERE target_path = ?1 AND status = 'approved'
                     ORDER BY id DESC
                     LIMIT ?2",
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?
            };

            let rows = if let Some(cursor) = before {
                stmt.query_map(params![path, cursor, limit], row_to_comment)
                    .map_err(|e| RepoError::Internal(e.to_string()))?
            } else {
                stmt.query_map(params![path, limit], row_to_comment)
                    .map_err(|e| RepoError::Internal(e.to_string()))?
            };

            let mut comments = Vec::new();
            for row in rows {
                comments.push(row.map_err(|e| RepoError::Internal(e.to_string()))?);
            }
            Ok(comments)
        })
        .await
    }

    pub async fn count_approved(&self, path: &str) -> RepoResult<i64> {
        let path = path.to_string();
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM comments WHERE target_path = ?1 AND status = 'approved'",
                params![path],
                |row| row.get(0),
            )
            .map_err(|e| RepoError::Internal(e.to_string()))
        })
        .await
    }

    pub async fn list_pending(
        &self,
        limit: i64,
        before: Option<i64>,
        path: Option<&str>,
    ) -> RepoResult<Vec<Comment>> {
        let path = path.map(|s| s.to_string());
        self.spawn(move |conn| {
            let (sql, has_path, has_cursor) = match (&path, before) {
                (Some(_), Some(_)) => (
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                     FROM comments
                     WHERE status = 'pending' AND target_path = ?1 AND id < ?2
                     ORDER BY id DESC
                     LIMIT ?3",
                    true,
                    true,
                ),
                (Some(_), None) => (
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                     FROM comments
                     WHERE status = 'pending' AND target_path = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                    true,
                    false,
                ),
                (None, Some(_)) => (
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                     FROM comments
                     WHERE status = 'pending' AND id < ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                    false,
                    true,
                ),
                (None, None) => (
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                     FROM comments
                     WHERE status = 'pending'
                     ORDER BY id DESC
                     LIMIT ?1",
                    false,
                    false,
                ),
            };

            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| RepoError::Internal(e.to_string()))?;

            let rows: Vec<Comment> = match (has_path, has_cursor) {
                (true, true) => {
                    let p = path.as_deref().unwrap();
                    stmt.query_map(params![p, before.unwrap(), limit], row_to_comment)
                        .map_err(|e| RepoError::Internal(e.to_string()))?
                        .filter_map(|r| r.ok())
                        .collect()
                }
                (true, false) => {
                    let p = path.as_deref().unwrap();
                    stmt.query_map(params![p, limit], row_to_comment)
                        .map_err(|e| RepoError::Internal(e.to_string()))?
                        .filter_map(|r| r.ok())
                        .collect()
                }
                (false, true) => {
                    stmt.query_map(params![before.unwrap(), limit], row_to_comment)
                        .map_err(|e| RepoError::Internal(e.to_string()))?
                        .filter_map(|r| r.ok())
                        .collect()
                }
                (false, false) => {
                    stmt.query_map(params![limit], row_to_comment)
                        .map_err(|e| RepoError::Internal(e.to_string()))?
                        .filter_map(|r| r.ok())
                        .collect()
                }
            };
            Ok(rows)
        })
        .await
    }

    pub async fn list_comments(
        &self,
        status: Option<&str>,
        limit: i64,
        before: Option<i64>,
        path: Option<&str>,
    ) -> RepoResult<Vec<Comment>> {
        let status_val = status.unwrap_or("").to_string();
        let path_val = path.unwrap_or("").to_string();
        let before_val = before.unwrap_or(0);

        self.spawn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                     FROM comments
                     WHERE (?1 = '' OR ?1 = 'all' OR status = ?1)
                       AND (?2 = '' OR target_path = ?2)
                       AND (?3 = 0 OR id < ?3)
                     ORDER BY id DESC
                     LIMIT ?4",
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;

            let rows = stmt
                .query_map(params![status_val, path_val, before_val, limit], row_to_comment)
                .map_err(|e| RepoError::Internal(e.to_string()))?;

            let mut comments = Vec::new();
            for row in rows {
                comments.push(row.map_err(|e| RepoError::Internal(e.to_string()))?);
            }
            Ok(comments)
        })
        .await
    }

    pub async fn update_status(&self, id: i64, status: &str) -> RepoResult<()> {
        let status = status.to_string();
        self.spawn(move |conn| {
            let affected = conn
                .execute(
                    "UPDATE comments SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![status, id],
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            if affected == 0 {
                return Err(RepoError::NotFound(format!("comment id {} not found", id)));
            }
            Ok(())
        })
        .await
    }

    pub async fn get_comment(&self, id: i64) -> RepoResult<Option<Comment>> {
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                 FROM comments WHERE id = ?1",
                params![id],
                row_to_comment,
            )
            .optional()
            .map_err(|e| RepoError::Internal(e.to_string()))
        })
        .await
    }

    pub async fn get_comment_by_source(&self, source_url: &str) -> RepoResult<Option<Comment>> {
        let source_url = source_url.to_string();
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at
                 FROM comments WHERE source_url = ?1",
                params![source_url],
                row_to_comment,
            )
            .optional()
            .map_err(|e| RepoError::Internal(e.to_string()))
        })
        .await
    }

    // ── Webmention seen ──────────────────────────────────────

    pub async fn get_webmention_seen(
        &self,
        source: &str,
        target: &str,
    ) -> RepoResult<Option<WebmentionSeen>> {
        let source = source.to_string();
        let target = target.to_string();
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT source, target, last_seen_at, last_status
                 FROM webmention_seen WHERE source = ?1 AND target = ?2",
                params![source, target],
                |row| {
                    Ok(WebmentionSeen {
                        source: row.get(0)?,
                        target: row.get(1)?,
                        last_seen_at: row.get(2)?,
                        last_status: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|e| RepoError::Internal(e.to_string()))
        })
        .await
    }

    pub async fn upsert_webmention_seen(&self, input: NewWebmentionSeen) -> RepoResult<()> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO webmention_seen (source, target, last_status)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(source, target) DO UPDATE SET
                     last_seen_at = datetime('now'),
                     last_status = excluded.last_status",
                params![input.source, input.target, input.last_status],
            )
            .map_err(|e| RepoError::Internal(e.to_string()))?;
            Ok(())
        })
        .await
    }

    // ── GitHub profiles ───────────────────────────────────────

    pub async fn get_github_profile(&self, login: &str) -> RepoResult<Option<GithubProfile>> {
        let login = login.to_string();
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT login, name, avatar_url, cached_at, valid
                 FROM github_profiles WHERE login = ?1",
                params![login],
                |row| {
                    let valid_int: i64 = row.get(4)?;
                    Ok(GithubProfile {
                        login: row.get(0)?,
                        name: row.get(1)?,
                        avatar_url: row.get(2)?,
                        cached_at: row.get(3)?,
                        valid: valid_int != 0,
                    })
                },
            )
            .optional()
            .map_err(|e| RepoError::Internal(e.to_string()))
        })
        .await
    }

    pub async fn upsert_github_profile(&self, input: NewGithubProfile) -> RepoResult<()> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO github_profiles (login, name, avatar_url, valid)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(login) DO UPDATE SET
                     name = excluded.name,
                     avatar_url = excluded.avatar_url,
                     cached_at = datetime('now'),
                     valid = excluded.valid",
                params![
                    input.login,
                    input.name,
                    input.avatar_url,
                    input.valid as i64
                ],
            )
            .map_err(|e| RepoError::Internal(e.to_string()))?;
            Ok(())
        })
        .await
    }
}

// ── Helpers ─────────────────────────────────────────────────

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
            })
            .await
            .unwrap();

        let comment = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(comment.content, "first mention");
    }

    #[tokio::test]
    async fn upsert_by_source_overwrites_preserving_approved_status() {
        let (repo, _dir) = setup_repo();

        // Insert initial row.
        let id = repo
            .upsert_by_source(NewComment {
                target_path: "/post".to_string(),
                comment_type: "webmention".to_string(),
                source_url: Some("https://remote.example/source".to_string()),
                author_name: "Charlie".to_string(),
                author_url: None,
                author_avatar: None,
                content: "first mention".to_string(),
            })
            .await
            .unwrap();

        // Manually approve it.
        repo.update_status(id, "approved").await.unwrap();

        // Simulate a re-ping (idempotent upsert).
        let id2 = repo
            .upsert_by_source(NewComment {
                target_path: "/post".to_string(),
                comment_type: "webmention".to_string(),
                source_url: Some("https://remote.example/source".to_string()),
                author_name: "Charlie".to_string(),
                author_url: Some("https://charlie.blog".to_string()),
                author_avatar: Some("https://charlie.blog/photo.jpg".to_string()),
                content: "updated mention with richer data".to_string(),
            })
            .await
            .unwrap();

        // Same row — id should match.
        assert_eq!(id, id2);

        let comment = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(
            comment.status, "approved",
            "approved status preserved on upsert"
        );
        assert_eq!(comment.content, "updated mention with richer data");
        assert_eq!(comment.author_url, Some("https://charlie.blog".to_string()));
    }

    #[tokio::test]
    async fn list_approved_returns_only_approved() {
        let (repo, _dir) = setup_repo();

        // Insert for path /page
        let id1 = repo
            .insert_comment(NewComment {
                target_path: "/page".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "A".to_string(),
                author_url: None,
                author_avatar: None,
                content: "one".to_string(),
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
            })
            .await
            .unwrap();

        repo.update_status(id1, "approved").await.unwrap();
        repo.update_status(id2, "spam").await.unwrap();
        // id3 stays pending.

        let approved = repo.list_approved("/page", 100, None).await.unwrap();
        assert_eq!(approved.len(), 1, "only one approved row");
        assert_eq!(approved[0].id, id1);
    }

    #[tokio::test]
    async fn list_approved_pagination() {
        let (repo, _dir) = setup_repo();

        // Insert 5 comments, approve all.
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = repo
                .insert_comment(NewComment {
                    target_path: "/paginated".to_string(),
                    comment_type: "native".to_string(),
                    source_url: None,
                    author_name: format!("User{}", i),
                    author_url: None,
                    author_avatar: None,
                    content: format!("comment {}", i),
                })
                .await
                .unwrap();
            repo.update_status(id, "approved").await.unwrap();
            ids.push(id);
        }
        // ids are 1..=5, order is 5,4,3,2,1 (DESC).

        let page = repo.list_approved("/paginated", 2, None).await.unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, 5);
        assert_eq!(page[1].id, 4);

        // cursor-based: before = 4
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
        })
        .await
        .unwrap();

        // Approve one, then count.
        let id = repo
            .insert_comment(NewComment {
                target_path: "/count-test".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "B".to_string(),
                author_url: None,
                author_avatar: None,
                content: "b".to_string(),
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
            })
            .await
            .unwrap();

        // Approve
        repo.update_status(id, "approved").await.unwrap();
        assert_eq!(
            repo.get_comment(id).await.unwrap().unwrap().status,
            "approved"
        );

        // Then spam
        repo.update_status(id, "spam").await.unwrap();
        assert_eq!(repo.get_comment(id).await.unwrap().unwrap().status, "spam");

        // Delete
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

        // Update to gone.
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

        // Negative cache (404).
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
            })
            .await
            .unwrap();

        // The comment was stored correctly (DROP TABLE is just text).
        let comment = repo.get_comment(id).await.unwrap().expect("comment exists");
        assert_eq!(comment.content, malicious_content);

        // The comments table still exists (another insert works).
        let id2 = repo
            .insert_comment(NewComment {
                target_path: "/sqli-test".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Survivor".to_string(),
                author_url: None,
                author_avatar: None,
                content: "still alive".to_string(),
            })
            .await
            .unwrap();
        assert!(id2 > 0, "second insert succeeded — table not dropped");
    }

    #[tokio::test]
    async fn multiple_concurrent_spawns_dont_panic() {
        let (repo, _dir) = setup_repo();

        // Spawn several concurrent operations
        let mut handles = Vec::new();
        for i in 0..10 {
            let r = repo.clone();
            handles.push(tokio::spawn(async move {
                r.insert_comment(NewComment {
                    target_path: "/concurrent".to_string(),
                    comment_type: "native".to_string(),
                    source_url: None,
                    author_name: format!("Concurrent{}", i),
                    author_url: None,
                    author_avatar: None,
                    content: "hello".to_string(),
                })
                .await
            }));
        }

        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        let count = repo.count_approved("/concurrent").await.unwrap();
        assert_eq!(count, 0, "none approved yet");
    }
}
