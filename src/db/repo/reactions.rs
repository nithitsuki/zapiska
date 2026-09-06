use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::{Repo, RepoError, RepoResult};

/// A single reaction row. One row per (comment, identifier) — changing the
/// emoji updates the row and resets it to `pending` for re-moderation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentReaction {
    pub id: i64,
    pub comment_id: i64,
    pub reaction: String,
    /// SHA-256 hash of the reactor's IP (`h:` prefix), or `admin` in
    /// admin-only mode.
    pub identifier: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A reaction joined with its comment's context (admin moderation views).
#[derive(Debug, Clone, Serialize)]
pub struct ReactionWithComment {
    pub id: i64,
    pub comment_id: i64,
    pub target_path: String,
    pub comment_author: String,
    pub comment_status: String,
    pub reaction: String,
    pub status: String,
    pub created_at: String,
}

/// Approved reaction counts on the caller's connection (T15 read batches).
pub(crate) fn reaction_counts_on_conn(
    conn: &rusqlite::Connection,
    comment_ids: &[i64],
) -> RepoResult<HashMap<i64, HashMap<String, i64>>> {
    if comment_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; comment_ids.len()].join(",");
    let sql = format!(
        "SELECT comment_id, reaction, count(*)
         FROM comment_reactions
         WHERE status = 'approved' AND comment_id IN ({placeholders})
         GROUP BY comment_id, reaction"
    );
    let mut stmt = conn.prepare(&sql).map_err(RepoError::from)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(comment_ids), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(RepoError::from)?;
    let mut counts: HashMap<i64, HashMap<String, i64>> = HashMap::new();
    for row in rows {
        let (comment_id, reaction, n) = row.map_err(RepoError::from)?;
        *counts
            .entry(comment_id)
            .or_default()
            .entry(reaction)
            .or_insert(0) += n;
    }
    Ok(counts)
}

/// Every reaction row on the caller's connection (export snapshot).
pub(crate) fn list_all_reactions_on_conn(
    conn: &rusqlite::Connection,
) -> RepoResult<Vec<CommentReaction>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, comment_id, reaction, identifier, status, created_at, updated_at
             FROM comment_reactions
             ORDER BY id",
        )
        .map_err(RepoError::from)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(CommentReaction {
                id: row.get(0)?,
                comment_id: row.get(1)?,
                reaction: row.get(2)?,
                identifier: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })
        .map_err(RepoError::from)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(RepoError::from)?);
    }
    Ok(result)
}

impl Repo {
    /// Upsert a reaction for (comment, identifier). Returns `(id, changed)`.
    /// - Same reaction, still active (pending/approved): no-op, `changed=false`
    ///   — moderation state is preserved.
    /// - Different reaction, or a previously spam/deleted row: the row is
    ///   updated and reset to `pending` so the moderation engine re-evaluates.
    pub async fn upsert_reaction(
        &self,
        comment_id: i64,
        reaction: &str,
        identifier: &str,
    ) -> RepoResult<(i64, bool)> {
        let reaction = reaction.to_string();
        let identifier = identifier.to_string();
        self.spawn(move |conn| {
            let existing: Option<(i64, String, String)> = conn
                .query_row(
                    "SELECT id, reaction, status FROM comment_reactions
                     WHERE comment_id = ?1 AND identifier = ?2",
                    params![comment_id, identifier],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(RepoError::from)?;

            let active = matches!(
                existing.as_ref().map(|(_, _, s)| s.as_str()),
                Some("pending") | Some("approved")
            );
            if let Some((id, existing_reaction, _)) = existing {
                if active && existing_reaction == reaction {
                    return Ok((id, false));
                }
            }

            conn.execute(
                "INSERT INTO comment_reactions (comment_id, reaction, identifier, status)
                 VALUES (?1, ?2, ?3, 'pending')
                 ON CONFLICT(comment_id, identifier) DO UPDATE SET
                     reaction = excluded.reaction,
                     status = 'pending',
                     updated_at = datetime('now')",
                params![comment_id, reaction, identifier],
            )
            .map_err(RepoError::from)?;

            let id: i64 = conn
                .query_row(
                    "SELECT id FROM comment_reactions WHERE comment_id = ?1 AND identifier = ?2",
                    params![comment_id, identifier],
                    |row| row.get(0),
                )
                .map_err(RepoError::from)?;
            Ok((id, true))
        })
        .await
    }

    /// List reactions (optionally by status) with their comment context,
    /// newest first. Supports `before` cursor and `limit`.
    pub async fn list_reactions(
        &self,
        status: Option<&str>,
        limit: i64,
        before: Option<i64>,
    ) -> RepoResult<Vec<ReactionWithComment>> {
        let status = status.unwrap_or("").to_string();
        let before = before.unwrap_or(0);
        self.spawn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT r.id, r.comment_id, c.target_path, c.author_name, c.status,
                            r.reaction, r.status, r.created_at
                     FROM comment_reactions r
                     JOIN comments c ON c.id = r.comment_id
                     WHERE (?1 = '' OR r.status = ?1)
                       AND (?2 = 0 OR r.id < ?2)
                     ORDER BY r.id DESC
                     LIMIT ?3",
                )
                .map_err(RepoError::from)?;
            let rows = stmt
                .query_map(params![status, before, limit], |row| {
                    Ok(ReactionWithComment {
                        id: row.get(0)?,
                        comment_id: row.get(1)?,
                        target_path: row.get(2)?,
                        comment_author: row.get(3)?,
                        comment_status: row.get(4)?,
                        reaction: row.get(5)?,
                        status: row.get(6)?,
                        created_at: row.get(7)?,
                    })
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

    /// Set a reaction's moderation status. `NotFound` when the row doesn't exist.
    pub async fn update_reaction_status(&self, id: i64, status: &str) -> RepoResult<()> {
        let status = status.to_string();
        self.spawn(move |conn| {
            let affected = conn
                .execute(
                    "UPDATE comment_reactions SET status = ?1, updated_at = datetime('now')
                     WHERE id = ?2",
                    params![status, id],
                )
                .map_err(RepoError::from)?;
            if affected == 0 {
                return Err(RepoError::NotFound(format!("reaction id {id} not found")));
            }
            Ok(())
        })
        .await
    }

    /// Approve a reaction only if it is still the pending emoji the moderator
    /// reviewed (F9/B-18 compare-and-swap). Returns `true` when the approve
    /// applied, `false` when the row moved on (emoji swapped, already
    /// moderated) so the caller re-reads instead of approving sight-unseen,
    /// and `NotFound` when the id never existed. The status predicate alone
    /// cannot catch an emoji swap (a swap resets to `pending`, still
    /// matching), so the seen emoji is part of the compared value.
    pub async fn approve_reaction_cas(&self, id: i64, expected_reaction: &str) -> RepoResult<bool> {
        let expected_reaction = expected_reaction.to_string();
        self.with_conn(move |conn| {
            let applied = conn
                .execute(
                    "UPDATE comment_reactions SET status = 'approved', updated_at = datetime('now')
                     WHERE id = ?1 AND status = 'pending' AND reaction = ?2",
                    params![id, expected_reaction],
                )
                .map_err(RepoError::from)?;
            if applied == 1 {
                return Ok(true);
            }
            let exists: i64 = conn
                .query_row(
                    "SELECT count(*) FROM comment_reactions WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .map_err(RepoError::from)?;
            if exists == 0 {
                return Err(RepoError::NotFound(format!("reaction id {id} not found")));
            }
            Ok(false)
        })
        .await
    }

    /// Fetch a single reaction row (for status-change webhooks and tests).
    pub async fn get_reaction(&self, id: i64) -> RepoResult<Option<CommentReaction>> {
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT id, comment_id, reaction, identifier, status, created_at, updated_at
                 FROM comment_reactions WHERE id = ?1",
                params![id],
                |row| {
                    Ok(CommentReaction {
                        id: row.get(0)?,
                        comment_id: row.get(1)?,
                        reaction: row.get(2)?,
                        identifier: row.get(3)?,
                        status: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(RepoError::from)
        })
        .await
    }

    /// Remove an active reaction (sets status to `deleted`). Returns `true`
    /// if a row was actually removed.
    pub async fn delete_reaction(&self, comment_id: i64, identifier: &str) -> RepoResult<bool> {
        let identifier = identifier.to_string();
        self.spawn(move |conn| {
            let affected = conn
                .execute(
                    "UPDATE comment_reactions SET status = 'deleted', updated_at = datetime('now')
                     WHERE comment_id = ?1 AND identifier = ?2
                       AND status IN ('pending', 'approved')",
                    params![comment_id, identifier],
                )
                .map_err(RepoError::from)?;
            Ok(affected > 0)
        })
        .await
    }

    /// Approved reaction counts per comment: `{comment_id: {emoji: count}}`.
    pub async fn reaction_counts(
        &self,
        comment_ids: &[i64],
    ) -> RepoResult<HashMap<i64, HashMap<String, i64>>> {
        if comment_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids = comment_ids.to_vec();
        self.spawn(move |conn| reaction_counts_on_conn(conn, &ids))
            .await
    }

    /// Dump every reaction row (admin JSON export).
    pub async fn list_all_comment_reactions(&self) -> RepoResult<Vec<CommentReaction>> {
        self.spawn(list_all_reactions_on_conn).await
    }

    /// Import a reaction row, preserving id and status (idempotent upsert).
    pub async fn import_comment_reaction(&self, input: CommentReaction) -> RepoResult<()> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO comment_reactions (id, comment_id, reaction, identifier, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                     comment_id = excluded.comment_id,
                     reaction = excluded.reaction,
                     identifier = excluded.identifier,
                     status = excluded.status,
                     created_at = excluded.created_at,
                     updated_at = excluded.updated_at",
                params![
                    input.id,
                    input.comment_id,
                    input.reaction,
                    input.identifier,
                    input.status,
                    input.created_at,
                    input.updated_at,
                ],
            )
            .map_err(RepoError::from)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod t15_reaction_cas_tests {
    use super::super::{NewComment, Repo};
    use crate::db::RepoError;
    use crate::db::pool::{create_pool, run_migrations};

    fn setup() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t15_cas.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    async fn seed_approved(repo: &Repo) -> i64 {
        let id = repo
            .insert_comment(NewComment {
                target_path: "/t15-cas".to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: "T15".to_string(),
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
    async fn cas_approves_a_pending_reaction_of_the_seen_emoji() {
        let (repo, _dir) = setup();
        let cid = seed_approved(&repo).await;
        let (rid, _) = repo.upsert_reaction(cid, "❤️", "h:t15cas").await.unwrap();
        assert!(repo.approve_reaction_cas(rid, "❤️").await.unwrap());
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn emoji_change_racing_approval_keeps_the_new_emoji_pending() {
        // F9/B-18 interleaving: the moderator reads ❤️ (pending) and clicks
        // approve; before the UPDATE lands, the user swaps the emoji to 😄
        // (upsert resets the row to pending with the new value). The approve
        // CAS compares the seen emoji, so it must NOT apply — the unmoderated
        // 😄 stays pending instead of being approved sight-unseen.
        let (repo, _dir) = setup();
        let cid = seed_approved(&repo).await;
        let (rid, _) = repo.upsert_reaction(cid, "❤️", "h:t15race").await.unwrap();
        // Moderator's view: ❤️ pending. Racer swaps the emoji first.
        let (_, changed) = repo.upsert_reaction(cid, "😄", "h:t15race").await.unwrap();
        assert!(changed);
        // Stale approve for the seen emoji now fails cleanly.
        assert!(!repo.approve_reaction_cas(rid, "❤️").await.unwrap());
        let row = repo.get_reaction(rid).await.unwrap().unwrap();
        assert_eq!(row.reaction, "😄");
        assert_eq!(row.status, "pending", "unmoderated emoji must stay pending");
        // Approving the CURRENT emoji still works.
        assert!(repo.approve_reaction_cas(rid, "😄").await.unwrap());
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn concurrent_approves_have_exactly_one_winner() {
        // Two moderators approve the same pending reaction at once (barrier
        // releases both together): the serialized CAS admits exactly one —
        // the loser re-reads instead of double-applying.
        let (repo, _dir) = setup();
        let cid = seed_approved(&repo).await;
        let (rid, _) = repo.upsert_reaction(cid, "👍", "h:t15duel").await.unwrap();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let run = |repo: Repo, barrier: std::sync::Arc<tokio::sync::Barrier>| async move {
            barrier.wait().await;
            repo.approve_reaction_cas(rid, "👍").await.unwrap()
        };
        let (a, b) = tokio::join!(
            run(repo.clone(), barrier.clone()),
            run(repo.clone(), barrier.clone())
        );
        assert!(a != b, "exactly one approve must apply, got {a}/{b}");
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn cas_reports_false_for_non_pending_rows_and_not_found_for_missing() {
        let (repo, _dir) = setup();
        let cid = seed_approved(&repo).await;
        let (rid, _) = repo.upsert_reaction(cid, "👍", "h:t15np").await.unwrap();
        repo.update_reaction_status(rid, "spam").await.unwrap();
        assert!(!repo.approve_reaction_cas(rid, "👍").await.unwrap());
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().status,
            "spam"
        );
        let err = repo.approve_reaction_cas(999_999, "👍").await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)));
    }
}
