use rusqlite::OptionalExtension;
use rusqlite::params;

use super::{NewWebmentionSeen, Repo, RepoError, RepoResult, WebmentionSeen};

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

    /// Dump the whole webmention ledger (admin JSON export).
    pub async fn list_all_webmention_seen(&self) -> RepoResult<Vec<WebmentionSeen>> {
        self.spawn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT source, target, last_seen_at, last_status
                     FROM webmention_seen
                     ORDER BY source, target",
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(WebmentionSeen {
                        source: row.get(0)?,
                        target: row.get(1)?,
                        last_seen_at: row.get(2)?,
                        last_status: row.get(3)?,
                    })
                })
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row.map_err(|e| RepoError::Internal(e.to_string()))?);
            }
            Ok(result)
        })
        .await
    }
}
