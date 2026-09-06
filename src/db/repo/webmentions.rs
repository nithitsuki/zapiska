use rusqlite::OptionalExtension;
use rusqlite::params;

use super::{NewComment, NewWebmentionSeen, Repo, RepoError, RepoResult, WebmentionSeen};

/// Ledger upsert on the caller's connection (T15 units).
pub(crate) fn upsert_seen_on_conn(
    conn: &rusqlite::Connection,
    input: &NewWebmentionSeen,
) -> RepoResult<()> {
    conn.execute(
        "INSERT INTO webmention_seen (source, target, last_status)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(source, target) DO UPDATE SET
             last_seen_at = datetime('now'),
             last_status = excluded.last_status",
        params![input.source, input.target, input.last_status],
    )
    .map_err(RepoError::from)?;
    Ok(())
}

/// Whole ledger on the caller's connection (export snapshot).
pub(crate) fn list_all_seen_on_conn(
    conn: &rusqlite::Connection,
) -> RepoResult<Vec<WebmentionSeen>> {
    let mut stmt = conn
        .prepare(
            "SELECT source, target, last_seen_at, last_status
             FROM webmention_seen
             ORDER BY source, target",
        )
        .map_err(RepoError::from)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(WebmentionSeen {
                source: row.get(0)?,
                target: row.get(1)?,
                last_seen_at: row.get(2)?,
                last_status: row.get(3)?,
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
            .map_err(RepoError::from)
        })
        .await
    }

    pub async fn upsert_webmention_seen(&self, input: NewWebmentionSeen) -> RepoResult<()> {
        self.spawn(move |conn| upsert_seen_on_conn(conn, &input))
            .await
    }

    /// Dump the whole webmention ledger (admin JSON export).
    pub async fn list_all_webmention_seen(&self) -> RepoResult<Vec<WebmentionSeen>> {
        self.spawn(list_all_seen_on_conn).await
    }

    /// Upsert a mention comment and record its ledger row in ONE
    /// `BEGIN IMMEDIATE` commit (B-15). Either both land or neither does.
    pub async fn upsert_webmention_with_seen(
        &self,
        comment: NewComment,
        seen: NewWebmentionSeen,
    ) -> RepoResult<i64> {
        self.with_tx(move |tx| {
            let id = super::comments::upsert_by_source_on_conn(tx, &comment)?;
            upsert_seen_on_conn(tx, &seen)?;
            Ok(id)
        })
        .await
    }

    /// Record a 410-gone source and delete its comment in ONE
    /// `BEGIN IMMEDIATE` commit: ledger `gone` plus `deleted` status for a
    /// pending/approved row (idempotent when no comment exists).
    pub async fn mark_webmention_gone(&self, source: &str, target: &str) -> RepoResult<()> {
        let source = source.to_string();
        let target = target.to_string();
        self.with_tx(move |tx| {
            upsert_seen_on_conn(
                tx,
                &NewWebmentionSeen {
                    source: source.clone(),
                    target: target.clone(),
                    last_status: "gone".to_string(),
                },
            )?;
            if let Some(comment) = super::comments::get_by_source_on_conn(tx, &source)?
                && (comment.status == "approved" || comment.status == "pending")
            {
                super::comments::update_status_on_conn(tx, comment.id, "deleted")?;
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod t15_webmention_unit_tests {
    use super::super::{NewComment, NewWebmentionSeen, Repo};
    use crate::db::pool::{create_pool, run_migrations};

    fn setup() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t15_wm.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    fn mention(source: &str, target_path: &str) -> NewComment {
        NewComment {
            target_path: target_path.to_string(),
            comment_type: "webmention".to_string(),
            source_url: Some(source.to_string()),
            author_name: "T15".to_string(),
            author_url: None,
            author_avatar: None,
            content: "mention".to_string(),
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
    async fn gone_marks_seen_gone_and_deletes_comment_atomically() {
        let (repo, _dir) = setup();
        repo.upsert_webmention_with_seen(
            mention("https://src.example/gone", "/t15-gone"),
            NewWebmentionSeen {
                source: "https://src.example/gone".to_string(),
                target: "https://site.example/t15-gone".to_string(),
                last_status: "alive".to_string(),
            },
        )
        .await
        .unwrap();
        let id = repo
            .get_comment_by_source("https://src.example/gone")
            .await
            .unwrap()
            .unwrap()
            .id;
        repo.update_status(id, "approved").await.unwrap();

        repo.reset_acquire_count();
        repo.mark_webmention_gone("https://src.example/gone", "https://site.example/t15-gone")
            .await
            .unwrap();
        assert_eq!(
            repo.acquire_count(),
            1,
            "seen+delete must share ONE connection"
        );
        let seen = repo
            .get_webmention_seen("https://src.example/gone", "https://site.example/t15-gone")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.last_status, "gone");
        assert_eq!(
            repo.get_comment(id).await.unwrap().unwrap().status,
            "deleted"
        );
    }

    #[tokio::test]
    async fn gone_is_idempotent_without_a_comment_row() {
        let (repo, _dir) = setup();
        repo.mark_webmention_gone(
            "https://src.example/nothing",
            "https://site.example/nowhere",
        )
        .await
        .unwrap();
        let seen = repo
            .get_webmention_seen(
                "https://src.example/nothing",
                "https://site.example/nowhere",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.last_status, "gone");
    }
}
