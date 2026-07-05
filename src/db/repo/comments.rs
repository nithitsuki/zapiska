use rusqlite::params;
use rusqlite::OptionalExtension;

use super::{Comment, NewComment, RepoError, RepoResult, CommentsRepo, row_to_comment};

impl CommentsRepo {
    pub async fn insert_comment(&self, input: NewComment) -> RepoResult<i64> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO comments (target_path, comment_type, source_url, author_name, author_url, author_avatar, content, parent_id, depth, honeypot, delete_token, submitter_ip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                "INSERT INTO comments (target_path, comment_type, source_url, author_name, author_url, author_avatar, content, parent_id, depth, honeypot, delete_token, submitter_ip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
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
                ],
            )
            .map_err(|e| RepoError::Internal(e.to_string()))?;
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
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                     FROM comments
                     WHERE target_path = ?1 AND status = 'approved' AND id < ?2
                     ORDER BY id DESC
                     LIMIT ?3",
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?
            } else {
                conn.prepare(
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
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
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                     FROM comments
                     WHERE status = 'pending' AND target_path = ?1 AND id < ?2
                     ORDER BY id DESC
                     LIMIT ?3",
                    true, true,
                ),
                (Some(_), None) => (
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                     FROM comments
                     WHERE status = 'pending' AND target_path = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                    true, false,
                ),
                (None, Some(_)) => (
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                     FROM comments
                     WHERE status = 'pending' AND id < ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                    false, true,
                ),
                (None, None) => (
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                     FROM comments
                     WHERE status = 'pending'
                     ORDER BY id DESC
                     LIMIT ?1",
                    false, false,
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
        ip: Option<&str>,
    ) -> RepoResult<Vec<Comment>> {
        let status_val = status.unwrap_or("").to_string();
        let path_val = path.unwrap_or("").to_string();
        let before_val = before.unwrap_or(0);
        let ip_val = ip.unwrap_or("").to_string();

        self.spawn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                     FROM comments
                     WHERE (?1 = '' OR ?1 = 'all' OR status = ?1)
                       AND (?2 = '' OR target_path = ?2)
                       AND (?3 = 0 OR id < ?3)
                       AND (?4 = '' OR submitter_ip = ?4)
                     ORDER BY id DESC
                     LIMIT ?5",
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;

            let rows = stmt
                .query_map(params![status_val, path_val, before_val, ip_val, limit], row_to_comment)
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
                "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                 FROM comments WHERE id = ?1",
                params![id],
                row_to_comment,
            )
            .optional()
            .map_err(|e| RepoError::Internal(e.to_string()))
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
                RepoError::Internal(format!(
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
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            Ok(affected > 0)
        })
        .await
    }

    pub async fn get_comment_by_source(&self, source_url: &str) -> RepoResult<Option<Comment>> {
        let source_url = source_url.to_string();
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT id, target_path, comment_type, source_url, author_name, author_url, author_avatar, content, status, created_at, updated_at, parent_id, depth, honeypot, delete_token, submitter_ip
                 FROM comments WHERE source_url = ?1",
                params![source_url],
                row_to_comment,
            )
            .optional()
            .map_err(|e| RepoError::Internal(e.to_string()))
        })
        .await
    }
}
