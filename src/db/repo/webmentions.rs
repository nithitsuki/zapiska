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

    /// Record the `(source, target)` pair as alive: the source fetched and
    /// the backlink verified. The explicit "mark" half of the seen state
    /// machine (the other half is [`Repo::record_seen_gone`]); the processor
    /// owns the transitions, these own the writes.
    pub async fn record_seen_alive(&self, source: &str, target: &str) -> RepoResult<()> {
        self.upsert_webmention_seen(NewWebmentionSeen {
            source: source.to_string(),
            target: target.to_string(),
            last_status: "alive".to_string(),
        })
        .await
    }

    /// Record the `(source, target)` pair as gone: a 410, or a backlink-less
    /// fetch. This flips the ledger only — it never deletes comments by
    /// itself. Deletion is a second, confirmed step through the moderation
    /// machine (see the grace policy in `crate::worker`), so a single
    /// transient miss cannot tombstone a live mention.
    pub async fn record_seen_gone(&self, source: &str, target: &str) -> RepoResult<()> {
        self.upsert_webmention_seen(NewWebmentionSeen {
            source: source.to_string(),
            target: target.to_string(),
            last_status: "gone".to_string(),
        })
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
            let id = super::comments::upsert_by_source_on_conn(tx, &comment, false)?;
            upsert_seen_on_conn(tx, &seen)?;
            Ok(id)
        })
        .await
    }
}

#[cfg(test)]
mod t15_webmention_unit_tests {
    use super::super::Repo;
    use crate::db::pool::{create_pool, run_migrations};

    fn setup() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t15_wm.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    #[tokio::test]
    async fn record_seen_marks_flip_alive_and_gone() {
        // The explicit state-machine writes: alive on verified fetch, gone
        // on 410/miss, alive again on resurrection — gone→alive is
        // representable at the storage level.
        let (repo, _dir) = setup();
        repo.record_seen_alive("https://src.example/s", "https://site.example/t")
            .await
            .unwrap();
        let seen = repo
            .get_webmention_seen("https://src.example/s", "https://site.example/t")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.last_status, "alive");
        repo.record_seen_gone("https://src.example/s", "https://site.example/t")
            .await
            .unwrap();
        let seen = repo
            .get_webmention_seen("https://src.example/s", "https://site.example/t")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.last_status, "gone");
        repo.record_seen_alive("https://src.example/s", "https://site.example/t")
            .await
            .unwrap();
        let seen = repo
            .get_webmention_seen("https://src.example/s", "https://site.example/t")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.last_status, "alive");
    }
}
