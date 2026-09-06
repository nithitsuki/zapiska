use rusqlite::OptionalExtension;
use rusqlite::params;
use std::collections::HashMap;

use super::select_comments;
use super::{
    COMMENT_COLUMNS, Comment, NewComment, Repo, RepoError, RepoResult, UrlErrorAction,
    row_to_comment, url_error_policy,
};

/// Connection-scoped SQL behind the async wrappers: each takes the caller's
/// `&Connection` (a live `Transaction` derefs to one) so single-shot methods
/// and multi-statement T15 units share the exact same statements.
pub(crate) fn insert_comment_on_conn(
    conn: &rusqlite::Connection,
    input: &NewComment,
) -> RepoResult<i64> {
    conn.execute(
        "INSERT INTO comments (target_path, comment_type, source_url, author_name, author_url, author_avatar, content, parent_id, depth, honeypot, delete_token, submitter_ip, content_hash, submitter_ip_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            input.target_path,
            input.comment_type,
            input.source_url,
            input.author_name,
            input.author_url,
            input.author_avatar,
            input.content,
            input.parent_id,
            input.depth,
            input.honeypot as i64,
            input.delete_token,
            input.submitter_ip,
            input.content_hash,
            input.submitter_ip_hash,
        ],
    )
    .map_err(RepoError::from)?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn upsert_by_source_on_conn(
    conn: &rusqlite::Connection,
    input: &NewComment,
) -> RepoResult<i64> {
    conn.execute(
        "INSERT INTO comments (target_path, comment_type, source_url, author_name, author_url, author_avatar, content, parent_id, depth, honeypot, delete_token, submitter_ip, content_hash, submitter_ip_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
            input.parent_id,
            input.depth,
            input.honeypot as i64,
            input.delete_token,
            input.submitter_ip,
            input.content_hash,
            input.submitter_ip_hash,
        ],
    )
    .map_err(RepoError::from)?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn update_status_on_conn(
    conn: &rusqlite::Connection,
    id: i64,
    status: &str,
) -> RepoResult<()> {
    let affected = conn
        .execute(
            "UPDATE comments SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![status, id],
        )
        .map_err(RepoError::from)?;
    if affected == 0 {
        return Err(RepoError::NotFound(format!("comment id {} not found", id)));
    }
    Ok(())
}

pub(crate) fn get_by_source_on_conn(
    conn: &rusqlite::Connection,
    source_url: &str,
) -> RepoResult<Option<Comment>> {
    let sql = select_comments("source_url = ?1", "id ASC");
    conn.query_row(&sql, params![source_url], row_to_comment)
        .optional()
        .map_err(RepoError::from)
}

pub(crate) fn list_approved_on_conn(
    conn: &rusqlite::Connection,
    path: &str,
    limit: i64,
    before: Option<i64>,
) -> RepoResult<Vec<Comment>> {
    let sql = format!(
        "{} LIMIT ?3",
        select_comments(
            "target_path = ?1 AND status = 'approved' AND (?2 IS NULL OR id < ?2)",
            "id DESC",
        )
    );
    let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;
    let rows = stmt
        .query_map(params![path, before, limit], row_to_comment)
        .map_err(RepoError::from)?;
    let mut comments = Vec::new();
    for row in rows {
        comments.push(row.map_err(RepoError::from)?);
    }
    Ok(comments)
}

pub(crate) fn list_approved_oldest_on_conn(
    conn: &rusqlite::Connection,
    path: &str,
    limit: i64,
    after: Option<i64>,
) -> RepoResult<Vec<Comment>> {
    let sql = format!(
        "{} LIMIT ?3",
        select_comments(
            "target_path = ?1 AND status = 'approved' AND (?2 IS NULL OR id > ?2)",
            "id ASC",
        )
    );
    let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;
    let rows = stmt
        .query_map(params![path, after, limit], row_to_comment)
        .map_err(RepoError::from)?;
    let mut comments = Vec::new();
    for row in rows {
        comments.push(row.map_err(RepoError::from)?);
    }
    Ok(comments)
}

pub(crate) fn count_approved_on_conn(conn: &rusqlite::Connection, path: &str) -> RepoResult<i64> {
    conn.query_row(
        "SELECT count(*) FROM comments WHERE target_path = ?1 AND status = 'approved'",
        params![path],
        |row| row.get(0),
    )
    .map_err(RepoError::from)
}

pub(crate) fn list_all_comments_on_conn(conn: &rusqlite::Connection) -> RepoResult<Vec<Comment>> {
    let sql = format!("SELECT {COMMENT_COLUMNS} FROM comments ORDER BY id ASC");
    let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;
    let rows = stmt
        .query_map([], row_to_comment)
        .map_err(RepoError::from)?;
    let mut comments = Vec::new();
    for row in rows {
        comments.push(row.map_err(RepoError::from)?);
    }
    Ok(comments)
}

impl Repo {
    pub async fn insert_comment(&self, input: NewComment) -> RepoResult<i64> {
        self.spawn(move |conn| insert_comment_on_conn(conn, &input))
            .await
    }

    pub async fn upsert_by_source(&self, input: NewComment) -> RepoResult<i64> {
        self.spawn(move |conn| upsert_by_source_on_conn(conn, &input))
            .await
    }

    /// Store a native comment with its auto-approve status and extracted-URL
    /// rows in ONE `BEGIN IMMEDIATE` commit (B-7/B-14). Replaces the old
    /// three-acquire `insert → update_status → insert_urls` sequence whose
    /// URL step swallowed every error with `let _ =`.
    ///
    /// URL policy ([`url_error_policy`]): an invalid row (empty fields, or a
    /// storage `Constraint`) is skipped with a warn log and the comment still
    /// commits; `Busy`/`Io`/`Other` abort the whole unit (rollback, no torn
    /// comment-without-URLs).
    pub async fn create_native_comment(
        &self,
        input: NewComment,
        auto_approve: bool,
        urls: Vec<(String, String, String)>,
    ) -> RepoResult<i64> {
        self.with_tx(move |tx| {
            let id = insert_comment_on_conn(tx, &input)?;
            if auto_approve {
                update_status_on_conn(tx, id, "approved")?;
            }
            let mut skipped_urls = 0u32;
            for (url, domain, url_hash) in &urls {
                if url.is_empty() || domain.is_empty() || url_hash.is_empty() {
                    skipped_urls += 1;
                    tracing::warn!(
                        comment_id = id,
                        url = %url,
                        skipped = skipped_urls,
                        "skipping invalid extracted-URL row (empty field)"
                    );
                    continue;
                }
                match super::urls::insert_url_on_conn(tx, id, url, domain, url_hash) {
                    Ok(()) => {}
                    Err(e) => match url_error_policy(&e) {
                        UrlErrorAction::SkipRow => {
                            skipped_urls += 1;
                            tracing::warn!(
                                comment_id = id,
                                url = %url,
                                skipped = skipped_urls,
                                err = %e,
                                "skipping bad URL row"
                            );
                        }
                        UrlErrorAction::Abort => return Err(e),
                    },
                }
            }
            Ok(id)
        })
        .await
    }

    /// Newest-first page plus its total and approved reaction counts, all on
    /// ONE connection (read batch for `GET /api/comments?sort=newest`).
    pub async fn list_approved_page(
        &self,
        path: &str,
        limit: i64,
        before: Option<i64>,
    ) -> RepoResult<(Vec<Comment>, i64, HashMap<i64, HashMap<String, i64>>)> {
        let path = path.to_string();
        self.with_conn(move |conn| {
            let comments = list_approved_on_conn(conn, &path, limit, before)?;
            let total = count_approved_on_conn(conn, &path)?;
            let ids: Vec<i64> = comments.iter().map(|c| c.id).collect();
            let counts = super::reactions::reaction_counts_on_conn(conn, &ids)?;
            Ok((comments, total, counts))
        })
        .await
    }

    /// Oldest-first page plus its total and approved reaction counts, all on
    /// ONE connection (read batch for `GET /api/comments?sort=oldest`).
    pub async fn list_approved_oldest_page(
        &self,
        path: &str,
        limit: i64,
        after: Option<i64>,
    ) -> RepoResult<(Vec<Comment>, i64, HashMap<i64, HashMap<String, i64>>)> {
        let path = path.to_string();
        self.with_conn(move |conn| {
            let comments = list_approved_oldest_on_conn(conn, &path, limit, after)?;
            let total = count_approved_on_conn(conn, &path)?;
            let ids: Vec<i64> = comments.iter().map(|c| c.id).collect();
            let counts = super::reactions::reaction_counts_on_conn(conn, &ids)?;
            Ok((comments, total, counts))
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
        self.spawn(move |conn| list_approved_on_conn(conn, &path, limit, before))
            .await
    }

    /// List approved comments for a path, oldest first (ascending id order).
    /// `after` is the cursor: rows with `id > after`. Used by
    /// `GET /api/comments?sort=oldest`.
    pub async fn list_approved_oldest(
        &self,
        path: &str,
        limit: i64,
        after: Option<i64>,
    ) -> RepoResult<Vec<Comment>> {
        let path = path.to_string();
        self.spawn(move |conn| list_approved_oldest_on_conn(conn, &path, limit, after))
            .await
    }

    /// List approved comments across ALL paths, newest first (global feed).
    pub async fn list_approved_global(&self, limit: i64) -> RepoResult<Vec<Comment>> {
        self.spawn(move |conn| {
            let sql = format!(
                "{} LIMIT ?1",
                select_comments("status = 'approved'", "id DESC")
            );
            let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;
            let rows = stmt
                .query_map(params![limit], row_to_comment)
                .map_err(RepoError::from)?;
            let mut comments = Vec::new();
            for row in rows {
                comments.push(row.map_err(RepoError::from)?);
            }
            Ok(comments)
        })
        .await
    }

    /// List all distinct paths that have comments, with counts per status.
    pub async fn list_paths(&self) -> RepoResult<Vec<(String, i64, i64, i64, i64, i64)>> {
        self.spawn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT target_path,
                            count(*) as total,
                            sum(CASE WHEN status='approved' THEN 1 ELSE 0 END),
                            sum(CASE WHEN status='spam' THEN 1 ELSE 0 END),
                            sum(CASE WHEN status='pending' THEN 1 ELSE 0 END),
                            sum(CASE WHEN status='deleted' THEN 1 ELSE 0 END)
                     FROM comments
                     GROUP BY target_path
                     ORDER BY MAX(created_at) DESC",
                )
                .map_err(RepoError::from)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .map_err(RepoError::from)?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row.map_err(RepoError::from)?);
            }
            Ok(result)
        })
        .await
    }

    pub async fn count_approved(&self, path: &str) -> RepoResult<i64> {
        let path = path.to_string();
        self.spawn(move |conn| count_approved_on_conn(conn, &path))
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
            let sql = format!(
                "{} LIMIT ?3",
                select_comments(
                    "status = 'pending' AND (?1 IS NULL OR target_path = ?1) AND (?2 IS NULL OR id < ?2)",
                    "id DESC",
                )
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(RepoError::from)?;

            let rows: Vec<Comment> = stmt
                .query_map(params![path, before, limit], row_to_comment)
                .map_err(RepoError::from)?
                .filter_map(|r| r.ok())
                .collect();
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
        ip: Option<&str>,
        content_hash: Option<&str>,
    ) -> RepoResult<Vec<Comment>> {
        let status_val = status.unwrap_or("").to_string();
        let path_val = path.unwrap_or("").to_string();
        let before_val = before.unwrap_or(0);
        let ip_val = ip.unwrap_or("").to_string();
        let ch_val = content_hash.unwrap_or("").to_string();

        self.spawn(move |conn| {
            // When path contains '%', use LIKE (supports wildcards like /blog/%).
            // Otherwise exact match (or empty = no filter).
            let path_cmp = if path_val.contains('%') {
                "target_path LIKE ?2"
            } else {
                "target_path = ?2"
            };
            let sql = format!(
                "{} LIMIT ?6",
                select_comments(
                    &format!(
                        "(?1 = '' OR ?1 = 'all' OR status = ?1)
                       AND (?2 = '' OR {path_cmp})
                       AND (?3 = 0 OR id < ?3)
                       AND (?4 = '' OR submitter_ip = ?4)
                       AND (?5 = '' OR content_hash = ?5)"
                    ),
                    "id DESC",
                )
            );
            let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;

            let rows = stmt
                .query_map(
                    params![status_val, path_val, before_val, ip_val, ch_val, limit],
                    row_to_comment,
                )
                .map_err(RepoError::from)?;

            let mut comments = Vec::new();
            for row in rows {
                comments.push(row.map_err(RepoError::from)?);
            }
            Ok(comments)
        })
        .await
    }

    /// Get submitter stats for a given IP address.
    /// Returns (total, approved, spam, pending, deleted, first_seen).
    pub async fn submitter_stats(
        &self,
        ip: &str,
    ) -> RepoResult<(i64, i64, i64, i64, i64, Option<String>)> {
        let ip = ip.to_string();
        self.spawn(move |conn| {
            let total: i64 = conn
                .query_row(
                    "SELECT count(*) FROM comments WHERE submitter_ip = ?1",
                    rusqlite::params![ip],
                    |row| row.get(0),
                )
                .map_err(RepoError::from)?;
            let approved: i64 = conn
                .query_row(
                    "SELECT count(*) FROM comments WHERE submitter_ip = ?1 AND status = 'approved'",
                    rusqlite::params![ip],
                    |row| row.get(0),
                )
                .map_err(RepoError::from)?;
            let spam: i64 = conn
                .query_row(
                    "SELECT count(*) FROM comments WHERE submitter_ip = ?1 AND status = 'spam'",
                    rusqlite::params![ip],
                    |row| row.get(0),
                )
                .map_err(RepoError::from)?;
            let pending: i64 = conn
                .query_row(
                    "SELECT count(*) FROM comments WHERE submitter_ip = ?1 AND status = 'pending'",
                    rusqlite::params![ip],
                    |row| row.get(0),
                )
                .map_err(RepoError::from)?;
            let deleted: i64 = conn
                .query_row(
                    "SELECT count(*) FROM comments WHERE submitter_ip = ?1 AND status = 'deleted'",
                    rusqlite::params![ip],
                    |row| row.get(0),
                )
                .map_err(RepoError::from)?;
            let first_seen: Option<String> = conn
                .query_row(
                    "SELECT MIN(created_at) FROM comments WHERE submitter_ip = ?1",
                    rusqlite::params![ip],
                    |row| row.get(0),
                )
                .ok();
            Ok((total, approved, spam, pending, deleted, first_seen))
        })
        .await
    }

    pub async fn update_status(&self, id: i64, status: &str) -> RepoResult<()> {
        let status = status.to_string();
        self.spawn(move |conn| update_status_on_conn(conn, id, &status))
            .await
    }

    /// Set a comment's moderation status, optionally retiring its delete
    /// token in the SAME `BEGIN IMMEDIATE` commit (T16/B10: an admin revive
    /// out of `deleted` must never leave a torn approved-with-live-token
    /// row). Single-step callers pass `clear_delete_token = false`.
    pub async fn set_comment_status(
        &self,
        id: i64,
        status: &str,
        clear_delete_token: bool,
    ) -> RepoResult<()> {
        let status = status.to_string();
        self.with_tx(move |tx| {
            update_status_on_conn(tx, id, &status)?;
            if clear_delete_token {
                tx.execute(
                    "UPDATE comments SET delete_token = NULL WHERE id = ?1",
                    params![id],
                )
                .map_err(RepoError::from)?;
            }
            Ok(())
        })
        .await
    }

    pub async fn get_comment(&self, id: i64) -> RepoResult<Option<Comment>> {
        self.spawn(move |conn| {
            let sql = select_comments("id = ?1", "id ASC");
            conn.query_row(&sql, params![id], row_to_comment)
                .optional()
                .map_err(RepoError::from)
        })
        .await
    }

    /// Fetch a comment and its entire ancestor chain (parent, grandparent, ..., root).
    /// Returns `(comment, [parent, grandparent, ..., root])`.
    /// The chain is ordered from immediate parent up to root.
    /// Returns `None` if the requested comment does not exist.
    pub async fn get_comment_chain(&self, id: i64) -> RepoResult<Option<(Comment, Vec<Comment>)>> {
        let comment = self.get_comment(id).await?;
        let Some(target) = comment else {
            return Ok(None);
        };

        let mut chain = Vec::new();
        let mut pid = target.parent_id;
        while let Some(current_pid) = pid {
            let parent = self.get_comment(current_pid).await?.ok_or_else(|| {
                RepoError::Constraint(format!(
                    "orphan comment: parent {current_pid} not found for comment {id}"
                ))
            })?;
            pid = parent.parent_id;
            chain.push(parent);
        }

        Ok(Some((target, chain)))
    }

    /// Delete a comment using its self-deletion token.
    /// Returns `Ok(true)` if deleted, `Ok(false)` if token doesn't match,
    /// `Err` if the comment doesn't exist or DB error.
    pub async fn delete_by_token(&self, id: i64, token: &str) -> RepoResult<bool> {
        let token = token.to_string();
        self.spawn(move |conn| {
            let affected = conn
                .execute(
                    "UPDATE comments SET status = 'deleted', updated_at = datetime('now')
                     WHERE id = ?1 AND delete_token = ?2 AND status != 'deleted'",
                    params![id, token],
                )
                .map_err(RepoError::from)?;
            Ok(affected > 0)
        })
        .await
    }

    /// Look up author identity stats across the site.
    /// Queries by IP, author_name, author_url, and/or github_username.
    pub async fn lookup_author(
        &self,
        ip: Option<&str>,
        author_name: Option<&str>,
        author_url: Option<&str>,
        combine: bool,
    ) -> RepoResult<serde_json::Value> {
        let ip = ip.map(|s| s.to_string());
        let author_name = author_name.map(|s| s.to_string());
        let author_url = author_url.map(|s| s.to_string());

        self.spawn(move |conn| {
            let mut conditions: Vec<String> = Vec::new();
            if let Some(ref v) = ip { conditions.push(format!("submitter_ip = '{}'", v.replace('\'', "''"))); }
            if let Some(ref v) = author_name { conditions.push(format!("author_name = '{}'", v.replace('\'', "''"))); }
            if let Some(ref v) = author_url { conditions.push(format!("author_url = '{}'", v.replace('\'', "''"))); }

            if conditions.is_empty() {
                return Ok(serde_json::json!({"error": "no signals provided"}));
            }

            let joiner = if combine { " OR " } else { " AND " };
            let where_clause = conditions.join(joiner);

            // Get aggregated stats
            let sql = format!(
                "SELECT count(*),
                        sum(CASE WHEN status='approved' THEN 1 ELSE 0 END),
                        sum(CASE WHEN status='spam' THEN 1 ELSE 0 END),
                        sum(CASE WHEN status='pending' THEN 1 ELSE 0 END),
                        sum(CASE WHEN status='deleted' THEN 1 ELSE 0 END),
                        MIN(created_at), MAX(created_at)
                 FROM comments WHERE {}", where_clause
            );
            let (total, approved, spam, pending, deleted, first, last): (i64, i64, i64, i64, i64, Option<String>, Option<String>) = conn
                .query_row(&sql, [], |row| {
                    Ok((
                        row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?, row.get::<_, i64>(4)?,
                        row.get(5)?, row.get(6)?,
                    ))
                })
                .map_err(RepoError::from)?;

            if total == 0 {
                return Ok(serde_json::json!({"total_comments": 0}));
            }

            // Recent comments
            let recent_sql = format!(
                "SELECT id, target_path, status, created_at, parent_id FROM comments WHERE {} ORDER BY created_at DESC LIMIT 10", where_clause
            );
            let mut stmt = conn.prepare(&recent_sql)
                .map_err(RepoError::from)?;
            let recent: Vec<serde_json::Value> = stmt
                .query_map([], |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_, i64>(0)?,
                        "target_path": row.get::<_, String>(1)?,
                        "status": row.get::<_, String>(2)?,
                        "created_at": row.get::<_, String>(3)?,
                        "parent_id": row.get::<_, Option<i64>>(4)?,
                    }))
                })
                .map_err(RepoError::from)?
                .filter_map(|r| r.ok())
                .collect();

            Ok(serde_json::json!({
                "total_comments": total,
                "approved": approved,
                "spam": spam,
                "pending": pending,
                "deleted": deleted,
                "first_seen": first,
                "last_seen": last,
                "recent_comments": recent,
            }))
        })
        .await
    }

    pub async fn get_comment_by_source(&self, source_url: &str) -> RepoResult<Option<Comment>> {
        let source_url = source_url.to_string();
        self.spawn(move |conn| get_by_source_on_conn(conn, &source_url))
            .await
    }

    /// Dump every comment (all statuses, all paths), oldest first.
    /// Used by the admin JSON export.
    pub async fn list_all_comments(&self) -> RepoResult<Vec<Comment>> {
        self.spawn(list_all_comments_on_conn).await
    }

    /// Import a full comment row, preserving its id, status, and timestamps.
    /// Upsert by id (ON CONFLICT updates content/author/status) so re-imports
    /// are idempotent. Parents sort before children in the export, so the
    /// foreign key on `parent_id` always resolves.
    pub async fn import_comment(&self, input: Comment) -> RepoResult<()> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO comments (id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip, content_hash, submitter_ip_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                 ON CONFLICT(id) DO UPDATE SET
                     target_path = excluded.target_path,
                     comment_type = excluded.comment_type,
                     source_url = excluded.source_url,
                     author_name = excluded.author_name,
                     author_url = excluded.author_url,
                     author_avatar = excluded.author_avatar,
                     content = excluded.content,
                     status = excluded.status,
                     created_at = excluded.created_at,
                     updated_at = excluded.updated_at,
                     parent_id = excluded.parent_id,
                     depth = excluded.depth,
                     honeypot = excluded.honeypot,
                     delete_token = excluded.delete_token,
                     submitter_ip = excluded.submitter_ip,
                     content_hash = excluded.content_hash,
                     submitter_ip_hash = excluded.submitter_ip_hash",
                params![
                    input.id,
                    input.target_path,
                    input.comment_type,
                    input.source_url,
                    input.author_name,
                    input.author_url,
                    input.author_avatar,
                    input.content,
                    input.status,
                    input.created_at,
                    input.updated_at,
                    input.parent_id,
                    input.depth,
                    input.honeypot as i64,
                    input.delete_token,
                    input.submitter_ip,
                    input.content_hash,
                    input.submitter_ip_hash,
                ],
            )
            .map_err(RepoError::from)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod t15_native_unit_tests {
    use super::super::{NewComment, Repo};
    use crate::db::RepoError;
    use crate::db::pool::{create_pool, run_migrations};

    fn setup() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t15_native.db");
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
            content: "native unit".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
        }
    }

    fn urls(n: usize) -> Vec<(String, String, String)> {
        (0..n)
            .map(|i| {
                (
                    format!("https://n15.example/p{i}"),
                    "n15.example".to_string(),
                    format!("hn15-{i}"),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn url_constraint_skips_row_with_log_but_commits_comment() {
        // Decision under test: a URL row that is itself invalid (Constraint)
        // is skipped with a warn log; the comment still commits. An empty
        // URL can never satisfy a moderation-lookup index, so it classifies
        // as Constraint without touching storage health.
        let (repo, _dir) = setup();
        let id = repo
            .create_native_comment(
                native_comment("/t15-url-skip"),
                false,
                vec![
                    (
                        "https://ok.example/".to_string(),
                        "ok.example".to_string(),
                        "hok".to_string(),
                    ),
                    ("".to_string(), "".to_string(), "".to_string()),
                ],
            )
            .await
            .unwrap();
        assert!(repo.get_comment(id).await.unwrap().is_some());
        let stored = repo.get_comment_urls(id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].url, "https://ok.example/");
    }

    #[tokio::test]
    async fn url_error_policy_fails_txn_on_busy_io_other() {
        // Decision under test: storage-health failures (Busy/Io/Other) abort
        // the whole comment transaction — never a torn comment-without-URLs.
        assert!(matches!(
            super::super::url_error_policy(&RepoError::Busy("locked".to_string())),
            super::super::UrlErrorAction::Abort
        ));
        assert!(matches!(
            super::super::url_error_policy(&RepoError::Io("disk".to_string())),
            super::super::UrlErrorAction::Abort
        ));
        assert!(matches!(
            super::super::url_error_policy(&RepoError::Other("corrupt".to_string())),
            super::super::UrlErrorAction::Abort
        ));
        assert!(matches!(
            super::super::url_error_policy(&RepoError::Constraint("bad row".to_string())),
            super::super::UrlErrorAction::SkipRow
        ));
    }

    #[tokio::test]
    async fn invalid_comment_type_fails_whole_unit_with_nothing_committed() {
        let (repo, _dir) = setup();
        let mut bad = native_comment("/t15-bad-type");
        bad.comment_type = "bogus".to_string();
        let err = repo
            .create_native_comment(bad, false, urls(2))
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Constraint(_)));
        let pending = repo.list_pending(100, None, None).await.unwrap();
        assert!(pending.iter().all(|c| c.target_path != "/t15-bad-type"));
    }

    #[tokio::test]
    async fn approved_page_batches_list_count_and_counts_in_one_acquire() {
        let (repo, _dir) = setup();
        for i in 0..3 {
            let mut c = native_comment("/t15-page");
            c.author_name = format!("P{i}");
            let id = repo.insert_comment(c).await.unwrap();
            repo.update_status(id, "approved").await.unwrap();
        }
        let ids: Vec<i64> = repo
            .list_approved("/t15-page", 10, None)
            .await
            .unwrap()
            .iter()
            .map(|c| c.id)
            .collect();
        let (r0, _) = repo.upsert_reaction(ids[0], "👍", "h:t15p").await.unwrap();
        repo.update_reaction_status(r0, "approved").await.unwrap();

        repo.reset_acquire_count();
        let (comments, total, counts) = repo
            .list_approved_page("/t15-page", 10, None)
            .await
            .unwrap();
        assert_eq!(
            repo.acquire_count(),
            1,
            "list+count+reaction_counts must share ONE connection"
        );
        assert_eq!(comments.len(), 3);
        assert_eq!(total, 3);
        assert_eq!(counts.get(&ids[0]).and_then(|m| m.get("👍")), Some(&1));

        repo.reset_acquire_count();
        let (comments, total, _) = repo
            .list_approved_oldest_page("/t15-page", 2, None)
            .await
            .unwrap();
        assert_eq!(repo.acquire_count(), 1);
        assert_eq!(comments.len(), 2);
        assert!(comments[0].id < comments[1].id);
        assert_eq!(total, 3);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{COMMENT_COLUMNS, NewComment, Repo};
    use crate::db::pool::{create_pool, run_migrations};

    fn setup() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("comment_roundtrip.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    /// A comment with every column populated distinctively, so a SELECT that
    /// drops or reorders a column fails the field assertions below.
    fn full_input(target_path: &str, parent_id: Option<i64>) -> NewComment {
        NewComment {
            target_path: target_path.to_string(),
            comment_type: "native".to_string(),
            source_url: Some("https://src.example/full".to_string()),
            author_name: "Full Fields".to_string(),
            author_url: Some("https://full.example/me".to_string()),
            author_avatar: Some("https://full.example/me.png".to_string()),
            content: "every column populated".to_string(),
            parent_id,
            depth: i64::from(parent_id.is_some()),
            honeypot: true,
            delete_token: Some("tok-full-fields".to_string()),
            submitter_ip: Some("9.9.9.9".to_string()),
            submitter_ip_hash: Some("h:full-fields".to_string()),
            content_hash: Some("ch:full-fields".to_string()),
        }
    }

    /// Seed a parent plus a fully-populated reply; returns the reply id.
    async fn seed_full_thread(repo: &Repo, target_path: &str) -> (i64, i64) {
        let parent = repo
            .insert_comment(NewComment {
                target_path: target_path.to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "Parent".to_string(),
                author_url: None,
                author_avatar: None,
                content: "parent".to_string(),
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
        let child = repo
            .insert_comment(full_input(target_path, Some(parent)))
            .await
            .unwrap();
        (parent, child)
    }

    /// Assert all 18 comment fields survived the read path.
    fn assert_full_fields(c: &super::super::Comment, target_path: &str, parent: i64, child: i64) {
        assert_eq!(c.id, child);
        assert_eq!(c.target_path, target_path);
        assert_eq!(c.comment_type, "native");
        assert_eq!(c.source_url, Some("https://src.example/full".to_string()));
        assert_eq!(c.author_name, "Full Fields");
        assert_eq!(c.author_url, Some("https://full.example/me".to_string()));
        assert_eq!(
            c.author_avatar,
            Some("https://full.example/me.png".to_string())
        );
        assert_eq!(c.content, "every column populated");
        assert!(!c.created_at.is_empty(), "created_at must survive");
        assert!(!c.updated_at.is_empty(), "updated_at must survive");
        assert_eq!(c.parent_id, Some(parent));
        assert_eq!(c.depth, 1);
        assert!(c.honeypot, "honeypot flag must survive");
        assert_eq!(c.delete_token, Some("tok-full-fields".to_string()));
        assert_eq!(c.submitter_ip, Some("9.9.9.9".to_string()));
        assert_eq!(c.content_hash, Some("ch:full-fields".to_string()));
        assert_eq!(c.submitter_ip_hash, Some("h:full-fields".to_string()));
    }

    #[tokio::test]
    async fn comment_columns_lists_all_18_fields() {
        assert_eq!(
            COMMENT_COLUMNS.split(',').count(),
            18,
            "COMMENT_COLUMNS must list all 18 comment columns: {COMMENT_COLUMNS}"
        );
    }

    #[tokio::test]
    async fn get_comment_roundtrip_preserves_all_fields() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/rt-get").await;
        let c = repo.get_comment(child).await.unwrap().unwrap();
        assert_full_fields(&c, "/rt-get", parent, child);
        assert_eq!(c.status, "pending");
    }

    #[tokio::test]
    async fn list_approved_roundtrip_preserves_all_fields() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/rt-approved").await;
        repo.update_status(parent, "approved").await.unwrap();
        repo.update_status(child, "approved").await.unwrap();
        // No cursor.
        let rows = repo.list_approved("/rt-approved", 10, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        let c = rows.iter().find(|c| c.id == child).unwrap();
        assert_full_fields(c, "/rt-approved", parent, child);
        assert_eq!(c.status, "approved");
        // With cursor (id < parent is empty; id < child+1 returns both).
        let rows = repo
            .list_approved("/rt-approved", 10, Some(child + 1))
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let rows = repo
            .list_approved("/rt-approved", 10, Some(child))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, parent);
    }

    #[tokio::test]
    async fn list_approved_oldest_roundtrip_preserves_all_fields() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/rt-oldest").await;
        repo.update_status(parent, "approved").await.unwrap();
        repo.update_status(child, "approved").await.unwrap();
        let rows = repo
            .list_approved_oldest("/rt-oldest", 10, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].id < rows[1].id, "oldest first");
        let c = rows.iter().find(|c| c.id == child).unwrap();
        assert_full_fields(c, "/rt-oldest", parent, child);
        // With after-cursor.
        let rows = repo
            .list_approved_oldest("/rt-oldest", 10, Some(parent))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_full_fields(&rows[0], "/rt-oldest", parent, child);
    }

    #[tokio::test]
    async fn list_approved_global_roundtrip_preserves_all_fields() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/rt-global").await;
        repo.update_status(parent, "approved").await.unwrap();
        repo.update_status(child, "approved").await.unwrap();
        let rows = repo.list_approved_global(10).await.unwrap();
        let c = rows.iter().find(|c| c.id == child).unwrap();
        assert_full_fields(c, "/rt-global", parent, child);
    }

    #[tokio::test]
    async fn list_pending_roundtrip_preserves_all_fields_in_every_combo() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/rt-pending").await;
        // Both rows stay pending. Combos: path x cursor.
        for (path, before) in [
            (None, None),
            (Some("/rt-pending"), None),
            (None, Some(child + 1)),
            (Some("/rt-pending"), Some(child + 1)),
        ] {
            let rows = repo.list_pending(10, before, path).await.unwrap();
            assert_eq!(rows.len(), 2, "combo path={path:?} before={before:?}");
            let c = rows.iter().find(|c| c.id == child).unwrap();
            assert_full_fields(c, "/rt-pending", parent, child);
            assert_eq!(c.status, "pending");
        }
        // Cursor below the reply excludes it.
        let rows = repo
            .list_pending(10, Some(child), Some("/rt-pending"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, parent);
        // Unrelated path matches nothing.
        let rows = repo
            .list_pending(10, None, Some("/rt-elsewhere"))
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn list_comments_roundtrip_preserves_all_fields_per_filter() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/blog/hello").await;
        // Exact path, no other filter.
        let rows = repo
            .list_comments(None, 10, None, Some("/blog/hello"), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let c = rows.iter().find(|c| c.id == child).unwrap();
        assert_full_fields(c, "/blog/hello", parent, child);
        // LIKE wildcard path.
        let rows = repo
            .list_comments(None, 10, None, Some("/blog/%"), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let c = rows.iter().find(|c| c.id == child).unwrap();
        assert_full_fields(c, "/blog/hello", parent, child);
        // Cursor variant.
        let rows = repo
            .list_comments(None, 10, Some(child + 1), Some("/blog/hello"), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        // IP filter.
        let rows = repo
            .list_comments(None, 10, None, None, Some("9.9.9.9"), None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_full_fields(&rows[0], "/blog/hello", parent, child);
        // Content-hash filter.
        let rows = repo
            .list_comments(None, 10, None, None, None, Some("ch:full-fields"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_full_fields(&rows[0], "/blog/hello", parent, child);
        // Status filter.
        let rows = repo
            .list_comments(Some("pending"), 10, None, Some("/blog/hello"), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn get_comment_by_source_roundtrip_preserves_all_fields() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/rt-source").await;
        let c = repo
            .get_comment_by_source("https://src.example/full")
            .await
            .unwrap()
            .unwrap();
        assert_full_fields(&c, "/rt-source", parent, child);
        assert!(
            repo.get_comment_by_source("https://src.example/missing")
                .await
                .unwrap()
                .is_none()
        );
        let _ = child;
    }

    #[tokio::test]
    async fn list_all_comments_roundtrip_preserves_all_fields() {
        let (repo, _dir) = setup();
        let (parent, child) = seed_full_thread(&repo, "/rt-all").await;
        let rows = repo.list_all_comments().await.unwrap();
        let c = rows.iter().find(|c| c.id == child).unwrap();
        assert_full_fields(c, "/rt-all", parent, child);
    }
}
