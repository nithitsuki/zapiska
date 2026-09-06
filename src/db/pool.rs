use r2d2::CustomizeConnection;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

/// Latest schema version. Bump on every schema change and add a gated
/// `if current < N` block in [`run_migrations`].
///
/// History:
/// - 1: initial (comments base, webmention_seen, github_profiles).
/// - 2: threading (parent_id, depth, idx_comments_parent).
/// - 3: honeypot flag + delete_token.
/// - 4: submitter_ip.
/// - 5: content_hash.
/// - 6: comment_urls table.
/// - 7: submitter_ip_hash + backfill.
/// - 8: comment_reactions table (previously only in schema.sql, now explicit).
pub const LATEST_SCHEMA_VERSION: i64 = 8;

pub type SqlitePool = Pool<SqliteConnectionManager>;

#[derive(Debug)]
struct PragmaSetter;

impl CustomizeConnection<Connection, rusqlite::Error> for PragmaSetter {
    fn on_acquire(&self, conn: &mut Connection) -> Result<(), rusqlite::Error> {
        // journal_mode = WAL is set once in run_migrations and persists in the DB file.
        // Setting it here would need an exclusive lock and can race with other connections.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;",
        )?;
        Ok(())
    }
}

pub fn create_pool(database_path: &str) -> Result<SqlitePool, r2d2::Error> {
    let manager = SqliteConnectionManager::file(database_path);
    Pool::builder()
        .connection_customizer(Box::new(PragmaSetter))
        .min_idle(Some(0))
        .build(manager)
}

/// Read `PRAGMA user_version` — the source of truth for schema version.
/// Returns 0 for legacy databases created before version tracking.
fn user_version(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn set_user_version(conn: &Connection, version: i64) -> Result<(), rusqlite::Error> {
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, rusqlite::Error> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        rusqlite::params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, rusqlite::Error> {
    // Table name is internal (never user input); PRAGMA doesn't take params.
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for c in cols.flatten() {
        if c == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `ALTER TABLE ... ADD COLUMN` only when the column is missing. Real errors
/// propagate; the old `let _ =` swallow-everything behavior is gone.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    ddl: &str,
) -> Result<(), rusqlite::Error> {
    if column_exists(conn, table, column)? {
        return Ok(());
    }
    conn.execute_batch(ddl)?;
    // Verify the column actually appeared — catches typos and no-op DDL.
    if !column_exists(conn, table, column)? {
        return Err(rusqlite::Error::ExecuteReturnedResults);
    }
    Ok(())
}

/// Run schema migrations, versioned via `PRAGMA user_version`.
///
/// - Fresh databases (`user_version == 0`, no `comments` table) get the full
///   canonical schema from `schema.sql`, then are stamped `LATEST`.
/// - Legacy databases (`user_version == 0`, tables exist) run the same
///   catch-up path idempotently (column/table existence checks, not blind
///   `ALTER`s) and are then stamped `LATEST`. Existing rows are preserved.
/// - Versioned databases (`user_version >= 1`) apply only pending
///   `if current < N` migrations in order.
/// - A database newer than this binary (`user_version > LATEST`) is refused
///   with an error instead of silently running against an unknown schema.
///
/// The IP-hash secret is passed by the caller (from `Config`) instead of
/// being read from the environment here, so the v7 backfill hashes with the
/// same secret as every other `hash_ip` call site.
pub fn run_migrations(
    pool: &SqlitePool,
    ip_hash_secret: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = pool.get()?;
    let current: i64 = user_version(&conn)?;

    if current > LATEST_SCHEMA_VERSION {
        return Err(format!(
            "database schema version {current} is newer than supported {LATEST_SCHEMA_VERSION}: upgrade zapiska first"
        )
        .into());
    }

    if current == LATEST_SCHEMA_VERSION {
        // Defensive: ensure the canonical objects exist even if a previous
        // run was interrupted after stamping. CREATE IF NOT EXISTS is safe.
        let schema = include_str!("../../migrations/schema.sql");
        conn.execute_batch(schema)?;
        return Ok(());
    }

    // Fresh install: no comments table yet. The canonical schema already
    // contains every column/index, so one batch creates everything.
    // (This avoids the legacy trap where idx_comments_parent references
    // parent_id before the column exists.)
    if !table_exists(&conn, "comments")? {
        let schema = include_str!("../../migrations/schema.sql");
        conn.execute_batch(schema)?;
        set_user_version(&conn, LATEST_SCHEMA_VERSION)?;
        return Ok(());
    }

    // Legacy (v0 with tables) or versioned (v >= 1) upgrade path: add
    // columns BEFORE applying the canonical schema snapshot, because the
    // snapshot contains indexes (e.g. idx_comments_parent) that fail when
    // their columns are still missing.

    // v2: threaded replies.
    if current < 2 {
        add_column_if_missing(
            &conn,
            "comments",
            "parent_id",
            "ALTER TABLE comments ADD COLUMN parent_id INTEGER REFERENCES comments(id)",
        )?;
        add_column_if_missing(
            &conn,
            "comments",
            "depth",
            "ALTER TABLE comments ADD COLUMN depth INTEGER NOT NULL DEFAULT 0",
        )?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_comments_parent ON comments(parent_id)",
        )?;
    }

    // v3: honeypot flag and self-deletion tokens.
    if current < 3 {
        add_column_if_missing(
            &conn,
            "comments",
            "honeypot",
            "ALTER TABLE comments ADD COLUMN honeypot INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "comments",
            "delete_token",
            "ALTER TABLE comments ADD COLUMN delete_token TEXT",
        )?;
    }

    // v4: raw submitter IP storage.
    if current < 4 {
        add_column_if_missing(
            &conn,
            "comments",
            "submitter_ip",
            "ALTER TABLE comments ADD COLUMN submitter_ip TEXT",
        )?;
    }

    // v5: content hash for moderation lookup.
    if current < 5 {
        add_column_if_missing(
            &conn,
            "comments",
            "content_hash",
            "ALTER TABLE comments ADD COLUMN content_hash TEXT",
        )?;
    }

    // v6: extracted URLs for cross-comment tracking.
    if current < 6 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS comment_urls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                comment_id INTEGER NOT NULL REFERENCES comments(id),
                url TEXT NOT NULL,
                domain TEXT NOT NULL,
                url_hash TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_comment_urls_comment ON comment_urls(comment_id);
            CREATE INDEX IF NOT EXISTS idx_comment_urls_domain ON comment_urls(domain);
            CREATE INDEX IF NOT EXISTS idx_comment_urls_hash ON comment_urls(url_hash);",
        )?;
    }

    // v7: hashed submitter IP + backfill of existing rows.
    if current < 7 {
        add_column_if_missing(
            &conn,
            "comments",
            "submitter_ip_hash",
            "ALTER TABLE comments ADD COLUMN submitter_ip_hash TEXT",
        )?;
        backfill_ip_hashes(&conn, ip_hash_secret)?;
    }

    // v8: reactions (previously only reachable via the base schema snapshot;
    // now an explicit versioned step so legacy DBs have a guaranteed path).
    if current < 8 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS comment_reactions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                comment_id INTEGER NOT NULL REFERENCES comments(id),
                reaction TEXT NOT NULL,
                identifier TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'approved', 'spam', 'deleted')),
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE (comment_id, identifier)
            );
            CREATE INDEX IF NOT EXISTS idx_comment_reactions_read
                ON comment_reactions(comment_id, status);",
        )?;
    }

    // Canonical snapshot last: backfills any tables/indexes added to
    // schema.sql without an explicit versioned step (e.g. webmention_seen,
    // github_profiles for very old DBs). Safe now — all columns exist.
    {
        let schema = include_str!("../../migrations/schema.sql");
        conn.execute_batch(schema)?;
    }

    set_user_version(&conn, LATEST_SCHEMA_VERSION)?;

    // Keep the table_exists helper exercised for future migration authors.
    debug_assert!(table_exists(&conn, "comments")?);

    Ok(())
}

/// Populate `submitter_ip_hash` for rows that have a raw IP but no hash,
/// using the single [`crate::ip_hash::hash_ip`] implementation shared with
/// comment submission and admin import.
/// A single bad IP must not abort startup; prepare failures do abort.
fn backfill_ip_hashes(
    conn: &Connection,
    ip_hash_secret: Option<&str>,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, submitter_ip FROM comments WHERE submitter_ip IS NOT NULL AND submitter_ip_hash IS NULL",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .flatten()
        .collect();
    for (id, raw) in rows {
        if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
            let hashed = crate::ip_hash::hash_ip(&ip, ip_hash_secret);
            // Per-row errors ignored: one corrupt row must not block boot.
            let _ = conn.execute(
                "UPDATE comments SET submitter_ip_hash = ?1 WHERE id = ?2",
                rusqlite::params![hashed, id],
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_pool(name: &str) -> (SqlitePool, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        (pool, dir)
    }

    fn db_version(pool: &SqlitePool) -> i64 {
        let conn = pool.get().unwrap();
        user_version(&conn).unwrap()
    }

    #[test]
    fn migrations_run_idempotently() {
        let (pool, _dir) = test_pool("test.db");
        run_migrations(&pool, None).unwrap();
        run_migrations(&pool, None).unwrap(); // second call should be a no-op
        assert_eq!(db_version(&pool), LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn fresh_db_is_stamped_with_latest_version() {
        let (pool, _dir) = test_pool("fresh.db");
        run_migrations(&pool, None).unwrap();
        assert_eq!(db_version(&pool), LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn legacy_v0_db_upgrades_and_preserves_rows() {
        // Simulate a v1-era database: base tables without any later columns,
        // user_version == 0 (pre-tracking).
        let (pool, _dir) = test_pool("legacy.db");
        {
            let conn = pool.get().unwrap();
            conn.execute_batch(
                "CREATE TABLE comments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    target_path TEXT NOT NULL,
                    comment_type TEXT NOT NULL,
                    source_url TEXT,
                    author_name TEXT NOT NULL,
                    author_url TEXT,
                    author_avatar TEXT,
                    content TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE webmention_seen (
                    source TEXT NOT NULL, target TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
                    last_status TEXT NOT NULL, PRIMARY KEY (source, target)
                );
                CREATE TABLE github_profiles (
                    login TEXT PRIMARY KEY, name TEXT,
                    avatar_url TEXT NOT NULL,
                    cached_at TEXT NOT NULL DEFAULT (datetime('now')),
                    valid INTEGER NOT NULL DEFAULT 1
                );
                INSERT INTO comments (target_path, comment_type, author_name, content, status)
                    VALUES ('/legacy', 'native', 'Ada', 'first!', 'approved');",
            )
            .unwrap();
            assert_eq!(user_version(&conn).unwrap(), 0);
        }

        run_migrations(&pool, None).unwrap();

        let conn = pool.get().unwrap();
        assert_eq!(user_version(&conn).unwrap(), LATEST_SCHEMA_VERSION);
        // Late columns now exist.
        for col in [
            "parent_id",
            "depth",
            "honeypot",
            "delete_token",
            "submitter_ip",
            "submitter_ip_hash",
            "content_hash",
        ] {
            assert!(
                column_exists(&conn, "comments", col).unwrap(),
                "missing column {col} after upgrade"
            );
        }
        // Late tables backfilled.
        assert!(table_exists(&conn, "comment_urls").unwrap());
        assert!(table_exists(&conn, "comment_reactions").unwrap());
        // Row survived.
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM comments WHERE author_name = 'Ada'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn newer_db_than_binary_is_refused() {
        let (pool, _dir) = test_pool("future.db");
        run_migrations(&pool, None).unwrap();
        {
            let conn = pool.get().unwrap();
            set_user_version(&conn, LATEST_SCHEMA_VERSION + 1).unwrap();
        }
        let err = run_migrations(&pool, None).unwrap_err().to_string();
        assert!(
            err.contains("newer than supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn add_column_propagates_real_errors() {
        let (pool, _dir) = test_pool("badcol.db");
        run_migrations(&pool, None).unwrap();
        let conn = pool.get().unwrap();
        // Referencing a missing table is a real error, not "already exists".
        let err = add_column_if_missing(
            &conn,
            "no_such_table",
            "x",
            "ALTER TABLE no_such_table ADD COLUMN x TEXT",
        );
        assert!(err.is_err(), "real DDL errors must propagate");
    }

    #[test]
    fn pragmas_set_on_every_connection() {
        let (pool, _dir) = test_pool("pragmas.db");
        run_migrations(&pool, None).unwrap();

        let conn = pool.get().unwrap();
        // WAL mode persists in the DB file — check the journal_mode.
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal", "expected WAL journal mode");

        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1, "expected foreign_keys = ON");

        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000, "expected busy_timeout = 5000");

        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        // 0=OFF, 1=NORMAL, 2=FULL — we set NORMAL.
        assert_eq!(sync, 1, "expected synchronous = NORMAL");
    }

    #[test]
    fn comments_check_constraint_rejects_bad_comment_type() {
        let (pool, _dir) = test_pool("check.db");
        run_migrations(&pool, None).unwrap();

        let conn = pool.get().unwrap();
        let err = conn.execute(
            "INSERT INTO comments (target_path, comment_type, author_name, content)
             VALUES ('/test', 'invalid_type', 'Alice', 'hello')",
            [],
        );
        assert!(
            err.is_err(),
            "CHECK constraint should reject bad comment_type"
        );
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("CHECK"),
            "error should mention CHECK constraint"
        );
    }

    #[test]
    fn comments_check_constraint_rejects_bad_status() {
        let (pool, _dir) = test_pool("check2.db");
        run_migrations(&pool, None).unwrap();

        let conn = pool.get().unwrap();
        let err = conn.execute(
            "INSERT INTO comments (target_path, comment_type, author_name, content, status)
             VALUES ('/test', 'native', 'Alice', 'hello', 'bogus')",
            [],
        );
        assert!(err.is_err(), "CHECK constraint should reject bad status");
    }

    #[test]
    fn comments_target_path_check_rejects_no_slash() {
        let (pool, _dir) = test_pool("check3.db");
        run_migrations(&pool, None).unwrap();
        let conn = pool.get().unwrap();
        let err = conn.execute(
            "INSERT INTO comments (target_path, comment_type, author_name, content)
             VALUES ('no-leading-slash', 'native', 'Alice', 'hello')",
            [],
        );
        assert!(
            err.is_err(),
            "CHECK constraint should reject missing leading /"
        );
    }

    #[test]
    fn comments_status_defaults_to_pending() {
        let (pool, _dir) = test_pool("default.db");
        run_migrations(&pool, None).unwrap();
        let conn = pool.get().unwrap();

        conn.execute(
            "INSERT INTO comments (target_path, comment_type, author_name, content)
             VALUES ('/post', 'native', 'Bob', 'nice post')",
            [],
        )
        .unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM comments WHERE author_name = 'Bob'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn v7_backfill_hashes_match_hash_ip() {
        use crate::ip_hash::hash_ip;

        // A realistic pre-v7 database: every column a v6 database would
        // have, raw peer addresses stored, no hash column yet.
        fn fabricated_v6_db(name: &str) -> (SqlitePool, tempfile::TempDir) {
            let (pool, dir) = test_pool(name);
            {
                let conn = pool.get().unwrap();
                conn.execute_batch(
                    "CREATE TABLE comments (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        target_path TEXT NOT NULL,
                        comment_type TEXT NOT NULL,
                        source_url TEXT,
                        author_name TEXT NOT NULL,
                        author_url TEXT,
                        author_avatar TEXT,
                        content TEXT NOT NULL,
                        status TEXT NOT NULL DEFAULT 'pending',
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                        parent_id INTEGER REFERENCES comments(id),
                        depth INTEGER NOT NULL DEFAULT 0,
                        honeypot INTEGER NOT NULL DEFAULT 0,
                        delete_token TEXT,
                        submitter_ip TEXT,
                        content_hash TEXT
                    );
                    INSERT INTO comments (target_path, comment_type, author_name, content, submitter_ip)
                        VALUES ('/v4', 'native', 'Ada', 'hello', '1.2.3.4'),
                               ('/v6', 'native', 'Bob', 'hi', '::1'),
                               ('/bad', 'native', 'Cid', 'garbage ip', 'not-an-ip'),
                               ('/none', 'native', 'Dee', 'no ip', NULL);
                    PRAGMA user_version = 6;",
                )
                .unwrap();
            }
            (pool, dir)
        }

        for secret in [Some("parity-secret"), None] {
            let db_name = if secret.is_some() {
                "parity_salted.db"
            } else {
                "parity_plain.db"
            };
            let (pool, _dir) = fabricated_v6_db(db_name);
            run_migrations(&pool, secret).unwrap();

            let conn = pool.get().unwrap();
            let hash_for = |ip: &str| -> Option<String> {
                conn.query_row(
                    "SELECT submitter_ip_hash FROM comments WHERE submitter_ip = ?1",
                    rusqlite::params![ip],
                    |row| row.get(0),
                )
                .unwrap()
            };
            // Backfilled rows equal the single hash_ip implementation.
            for raw in ["1.2.3.4", "::1"] {
                let ip: std::net::IpAddr = raw.parse().unwrap();
                assert_eq!(
                    hash_for(raw).as_deref(),
                    Some(hash_ip(&ip, secret).as_str()),
                    "backfill must equal hash_ip for {raw}"
                );
                // The secret is actually plumbed, not silently dropped:
                // a salted backfill must differ from the unsalted hash.
                if secret.is_some() {
                    assert_ne!(
                        hash_for(raw).as_deref(),
                        Some(hash_ip(&ip, None).as_str()),
                        "salted backfill must differ from unsalted hash_ip"
                    );
                }
            }
            // An unparseable IP stays NULL; one bad row never aborts boot.
            assert_eq!(hash_for("not-an-ip"), None);
            let null_hash: Option<String> = conn
                .query_row(
                    "SELECT submitter_ip_hash FROM comments WHERE submitter_ip IS NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(null_hash, None);
            assert_eq!(db_version(&pool), LATEST_SCHEMA_VERSION);
        }
    }

    #[test]
    fn idx_comments_read_exists() {
        let (pool, _dir) = test_pool("idx.db");
        run_migrations(&pool, None).unwrap();
        let conn = pool.get().unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_comments_read'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "expected index idx_comments_read to exist");
    }
}
