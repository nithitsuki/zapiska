use r2d2::CustomizeConnection;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

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

/// Run schema migrations. Fresh databases get the full schema from schema.sql.
/// Existing databases get incremental ALTER TABLE migrations (errors silently
/// ignored when a column already exists).
pub fn run_migrations(pool: &SqlitePool) -> Result<(), Box<dyn std::error::Error>> {
    let conn = pool.get()?;
    let schema = include_str!("../../migrations/schema.sql");
    conn.execute_batch(schema)?;

    // Migration 2: nested comments — add parent_id, depth, and indexes.
    let _ = conn.execute(
        "ALTER TABLE comments ADD COLUMN parent_id INTEGER REFERENCES comments(id)",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE comments ADD COLUMN depth INTEGER NOT NULL DEFAULT 0",
        [],
    );
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_comments_parent ON comments(parent_id)",
    );

    // Migration 3: honeypot flag and self-deletion tokens.
    let _ = conn.execute(
        "ALTER TABLE comments ADD COLUMN honeypot INTEGER NOT NULL DEFAULT 0",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE comments ADD COLUMN delete_token TEXT",
        []);

    // Migration 4: optional submitter IP address storage.
    let _ = conn.execute(
        "ALTER TABLE comments ADD COLUMN submitter_ip TEXT",
        []);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn migrations_run_idempotently() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();
        run_migrations(&pool).unwrap(); // second call should be a no-op
    }

    #[test]
    fn pragmas_set_on_every_connection() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pragmas.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();

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
        let dir = tempdir().unwrap();
        let path = dir.path().join("check.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();

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
        let dir = tempdir().unwrap();
        let path = dir.path().join("check2.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();

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
        let dir = tempdir().unwrap();
        let path = dir.path().join("check3.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();
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
        let dir = tempdir().unwrap();
        let path = dir.path().join("default.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();
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
    fn idx_comments_read_exists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("idx.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();
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
