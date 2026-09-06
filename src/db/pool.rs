use r2d2::CustomizeConnection;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

/// Versioned schema steps. The index IS the version: entry `MIGRATIONS[N]`
/// holds the DDL that brings a database from version N-1 to N, entry 0 is an
/// unused placeholder (pre-versioning legacy databases report `user_version`
/// 0), and the stamp written after a successful run is the step index.
/// [`LATEST_SCHEMA_VERSION`] is derived from the length so adding a version
/// is appending one entry — never a new `if current < N` branch plus a
/// separate snapshot hunk.
///
/// Rules for entries (enforced by the tests named below):
/// - Every `CREATE TABLE`/`CREATE INDEX` uses `IF NOT EXISTS`, so re-running
///   a step (interrupted upgrade, legacy catch-up) is a no-op for objects
///   that already exist.
/// - The only `ALTER` shape allowed is
///   `ALTER TABLE <table> ADD COLUMN <column> ...`, routed through the
///   existence-checked [`add_column_if_missing`] safety net by
///   [`apply_migration_step`] — never blind DDL.
/// - No semicolons inside string literals, no triggers or multi-statement
///   procedures: [`split_step_statements`] is a plain `;` split, and the
///   `migration_steps_pin_statement_counts_and_literals` test (exact
///   per-step statement counts plus a literal scan) fails if this rule or
///   the counts drift. Update the splitter before adding any such SQL.
/// - The union of all steps must equal `migrations/schema.sql`, which stays
///   the canonical fresh-install snapshot (kept as a docs-generated
///   artifact: deriving fresh-install SQL by concatenating steps would
///   rewrite `sqlite_master` CREATE text and column order, changing
///   fresh-install behavior for zero benefit). Two tests pin this from both
///   sides: `stepped_v0_upgrade_matches_fresh_install` diffs object sets,
///   column shapes, and index definitions between a stepped v0 upgrade and
///   a fresh install, while `never_altered_table_definitions_match_snapshot`
///   pins whitespace-normalized `CREATE TABLE` text for tables no step ever
///   ALTERs (catching CHECK / table-UNIQUE drift the DB diff cannot see).
///   `comments` is excluded from the text pin because ALTER history
///   legitimately rewrites its stored definition.
pub const MIGRATIONS: &[&str] = &[
    // 0: pre-versioning legacy. Never applied, never stamped.
    "",
    // 1: initial (comments base, webmention_seen, github_profiles).
    "CREATE TABLE IF NOT EXISTS comments (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        target_path TEXT NOT NULL
            CHECK (target_path LIKE '/%' AND length(target_path) <= 1024),
        comment_type TEXT NOT NULL
            CHECK (comment_type IN ('native', 'webmention')),
        source_url TEXT,
        author_name TEXT NOT NULL,
        author_url TEXT,
        author_avatar TEXT,
        content TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending'
            CHECK (status IN ('pending', 'approved', 'spam', 'deleted')),
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS webmention_seen (
        source TEXT NOT NULL,
        target TEXT NOT NULL,
        last_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
        last_status TEXT NOT NULL CHECK (last_status IN ('alive', 'gone')),
        PRIMARY KEY (source, target)
    );
    CREATE TABLE IF NOT EXISTS github_profiles (
        login TEXT PRIMARY KEY,
        name TEXT,
        avatar_url TEXT NOT NULL,
        cached_at TEXT NOT NULL DEFAULT (datetime('now')),
        valid INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX IF NOT EXISTS idx_comments_read
        ON comments(target_path, status, created_at);
    CREATE UNIQUE INDEX IF NOT EXISTS idx_comments_source_target
        ON comments(source_url, target_path)
        WHERE source_url IS NOT NULL;",
    // 2: threading (parent_id, depth, idx_comments_parent).
    "ALTER TABLE comments ADD COLUMN parent_id INTEGER REFERENCES comments(id);
    ALTER TABLE comments ADD COLUMN depth INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX IF NOT EXISTS idx_comments_parent ON comments(parent_id);",
    // 3: honeypot flag + delete_token.
    "ALTER TABLE comments ADD COLUMN honeypot INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE comments ADD COLUMN delete_token TEXT;",
    // 4: submitter_ip.
    "ALTER TABLE comments ADD COLUMN submitter_ip TEXT;",
    // 5: content_hash.
    "ALTER TABLE comments ADD COLUMN content_hash TEXT;",
    // 6: comment_urls table.
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
    // 7: submitter_ip_hash + backfill (the backfill itself is procedural —
    // see run_migrations — because it hashes with the caller's secret).
    "ALTER TABLE comments ADD COLUMN submitter_ip_hash TEXT;",
    // 8: comment_reactions table (previously only in schema.sql, now explicit).
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
];

/// Latest schema version. Derived from [`MIGRATIONS`] — the stamp IS the
/// step index, so this never needs a manual bump beside a new entry.
pub const LATEST_SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64 - 1;

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

/// Run `PRAGMA quick_check` against the database and refuse to serve traffic
/// when it reports anything but a clean `ok`. Corruption must fail loud at
/// boot (via [`crate::state::AppState::start`]), not as silent row loss at
/// request time.
pub fn quick_check(pool: &SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| {
        format!(
            "database PRAGMA quick_check failed: cannot acquire a connection ({e}); \
             check DATABASE_PATH and file permissions"
        )
    })?;
    let mut stmt = conn
        .prepare("PRAGMA quick_check")
        .map_err(|e| format!("database PRAGMA quick_check failed: cannot prepare probe ({e})"))?;
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("database PRAGMA quick_check failed: cannot run probe ({e})"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("database PRAGMA quick_check failed: cannot read probe rows ({e})"))?;
    if rows.len() == 1 && rows[0] == "ok" {
        return Ok(());
    }
    Err(format!(
        "database PRAGMA quick_check reported corruption: {}; \
         restore the SQLite file from backup (or re-import a known-good JSON export) before starting; \
         set DB_QUICK_CHECK=false only to bypass this gate for recovery",
        rows.join("; ")
    ))
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

/// Parse the single ALTER shape migration steps may use:
/// `ALTER TABLE <table> ADD COLUMN <column> ...`, with bare (unquoted,
/// single-token) table and column names. Returns the table and column so the
/// caller can route through the existence-checked [`add_column_if_missing`]
/// safety net. Any other statement returns `None` and runs verbatim via
/// `execute_batch`.
///
/// Quoted-identifier caveat: names containing whitespace or quoting
/// (`"my table"`, `[col]`, backticks) do NOT parse — the whitespace split
/// would misread them — so they fall through to verbatim execution, which is
/// not idempotent on re-run. Never use quoted identifiers in steps; keep
/// every step to the bare-identifier shape above.
fn parse_add_column(stmt: &str) -> Option<(&str, &str)> {
    let mut toks = stmt.split_whitespace();
    if !toks.next()?.eq_ignore_ascii_case("ALTER") {
        return None;
    }
    if !toks.next()?.eq_ignore_ascii_case("TABLE") {
        return None;
    }
    let table = toks.next()?;
    if !toks.next()?.eq_ignore_ascii_case("ADD") {
        return None;
    }
    if !toks.next()?.eq_ignore_ascii_case("COLUMN") {
        return None;
    }
    Some((table, toks.next()?))
}

/// Apply one [`MIGRATIONS`] step, statement by statement. Column additions
/// go through [`add_column_if_missing`] (a legacy v0 database may already
/// hold any subset of late columns, so blind `ALTER`s would abort the
/// upgrade); every other statement is already `IF NOT EXISTS` and therefore
/// a safe no-op on re-run. Real DDL errors propagate.
fn apply_migration_step(conn: &Connection, step_sql: &str) -> Result<(), rusqlite::Error> {
    for stmt in split_step_statements(step_sql) {
        if let Some((table, column)) = parse_add_column(stmt) {
            add_column_if_missing(conn, table, column, stmt)?;
        } else {
            conn.execute_batch(stmt)?;
        }
    }
    Ok(())
}

/// Split one [`MIGRATIONS`] entry into its runnable statements, dropping
/// empties. This is a plain `;` split, which is exact only because migration
/// SQL never embeds semicolons in string literals — pinned by the
/// `migration_steps_pin_statement_counts_and_literals` test.
fn split_step_statements(step_sql: &str) -> Vec<&str> {
    step_sql
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Run schema migrations, versioned via `PRAGMA user_version`.
///
/// - Fresh databases (`user_version == 0`, no `comments` table) get the full
///   canonical schema from `schema.sql`, then are stamped `LATEST`.
/// - Legacy databases (`user_version == 0`, tables exist) run every
///   [`MIGRATIONS`] step in order through [`apply_migration_step`]
///   (idempotent: existence-checked columns, `IF NOT EXISTS` objects) and
///   are then stamped `LATEST`. Existing rows are preserved.
/// - Versioned databases (`user_version >= 1`) apply only pending steps.
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

    // Legacy (v0 with tables) or versioned (v >= 1) upgrade path: the steps
    // ARE the upgrade — no trailing snapshot re-run, so
    // `stepped_v0_upgrade_matches_fresh_install` pins step-union == snapshot
    // instead of papering drift over. Column additions run inside the steps
    // (before the indexes that reference them) via apply_migration_step.
    ensure_no_duplicate_source_target(&conn)?;
    let start = (current + 1).max(1) as usize;
    for step in &MIGRATIONS[start..] {
        apply_migration_step(&conn, step)?;
    }

    // v7 backfill is procedural (it hashes with the caller's secret), so it
    // rides beside its step rather than inside it. Same gate as before:
    // only databases upgrading from below v7 backfill; fresh installs have
    // no rows and already-stamped databases already backfilled.
    if current < 7 {
        backfill_ip_hashes(&conn, ip_hash_secret)?;
    }

    set_user_version(&conn, LATEST_SCHEMA_VERSION)?;

    Ok(())
}

/// List `(source_url, target_path)` pairs held by more than one row.
/// Non-NULL duplicates are what the partial unique index
/// `idx_comments_source_target` rejects at snapshot time.
fn duplicate_source_target_pairs(
    conn: &Connection,
) -> Result<Vec<(String, String, i64)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT source_url, target_path, COUNT(*) FROM comments
         WHERE source_url IS NOT NULL
         GROUP BY source_url, target_path HAVING COUNT(*) > 1
         LIMIT 5",
    )?;
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
}

/// Pre-flight for the canonical snapshot: fail loud with the dedup remedy
/// instead of aborting boot on a raw SQLite unique-index error (B-3).
fn ensure_no_duplicate_source_target(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let dupes = duplicate_source_target_pairs(conn)?;
    if dupes.is_empty() {
        return Ok(());
    }
    Err(duplicate_source_target_error(&dupes))
}

fn duplicate_source_target_error(dupes: &[(String, String, i64)]) -> Box<dyn std::error::Error> {
    let samples: Vec<String> = dupes
        .iter()
        .map(|(s, t, n)| format!("source_url='{s}' target_path='{t}' ({n} rows)"))
        .collect();
    format!(
        "legacy database holds {} duplicate (source_url, target_path) group(s) conflicting with unique index idx_comments_source_target (first 5 shown: {}): dedup before upgrade, keeping the newest row per pair, for example: DELETE FROM comments WHERE id NOT IN (SELECT MAX(id) FROM comments WHERE source_url IS NOT NULL GROUP BY source_url, target_path); then restart",
        dupes.len(),
        samples.join("; ")
    )
    .into()
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

    #[test]
    fn quick_check_healthy_db_passes() {
        let (pool, _dir) = test_pool("q.db");
        run_migrations(&pool, None).unwrap();
        assert!(quick_check(&pool).is_ok());
    }

    #[test]
    fn quick_check_unreachable_db_fails_loud() {
        // A short connection timeout keeps the failure fast: r2d2 would
        // otherwise retry for its 30 s default before surfacing the error.
        let manager =
            r2d2_sqlite::SqliteConnectionManager::file("/nonexistent-dir-zapiska-quickcheck/q.db");
        let pool = r2d2::Pool::builder()
            .min_idle(Some(0))
            .connection_timeout(std::time::Duration::from_millis(200))
            .build(manager)
            .unwrap();
        let err = quick_check(&pool).unwrap_err();
        assert!(
            err.contains("quick_check"),
            "failure must name the check, got: {err}"
        );
    }

    fn db_version(pool: &SqlitePool) -> i64 {
        let conn = pool.get().unwrap();
        user_version(&conn).unwrap()
    }

    /// Canonical v1-era shape: base tables without any later columns and no
    /// version stamp. Shared by the legacy-upgrade and parity tests so both
    /// guard the same fabricated history.
    const V1_FIXTURE_SCHEMA: &str = "CREATE TABLE comments (
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
            VALUES ('/legacy', 'native', 'Ada', 'first!', 'approved');";

    // ── Migration parity (B-6/D2): stepped upgrade vs fresh snapshot ──

    #[derive(Debug, PartialEq, Eq)]
    struct ColumnShape {
        coltype: String,
        notnull: i64,
        dflt: Option<String>,
        pk: i64,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SchemaFingerprint {
        /// (type, name, table) for every table and index, minus SQLite
        /// internals (`sqlite_sequence`, auto-indexes).
        objects: std::collections::BTreeSet<(String, String, String)>,
        /// Table name → column name → shape. Compared as maps (not ordered
        /// lists) on purpose: history appends late columns in step order
        /// (`submitter_ip, content_hash, submitter_ip_hash`) while the
        /// fresh snapshot declares them in snapshot order
        /// (`submitter_ip, submitter_ip_hash, content_hash`) — same set,
        /// different positions, and no read path depends on positions.
        columns:
            std::collections::BTreeMap<String, std::collections::BTreeMap<String, ColumnShape>>,
        /// Index name → whitespace-normalized definition. Table definitions
        /// are excluded: ALTER history legitimately rewrites them, so raw
        /// text can never match the snapshot (see `columns` instead).
        index_sql: std::collections::BTreeMap<String, String>,
    }

    fn normalize_sql(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Tables created whole by a single step and never ALTERed afterward.
    /// Their step DDL and the snapshot DDL must be identical modulo
    /// whitespace — pinned by `never_altered_table_definitions_match_snapshot`,
    /// not by the DB parity test (which is blind here: `comments` is
    /// excluded because ALTER history legitimately rewrites its stored text,
    /// and the v0 fixture predates even the step-1 CHECKs, so a stepped
    /// upgrade keeps the fixture text for `webmention_seen`/`github_profiles`).
    const NEVER_ALTERED_TABLES: &[&str] = &[
        "comment_reactions",
        "comment_urls",
        "webmention_seen",
        "github_profiles",
    ];

    /// Map `CREATE TABLE` name → whitespace-normalized statement, for one SQL
    /// source (a MIGRATIONS entry or the snapshot). `--` line comments are
    /// stripped so comment placement never affects the comparison.
    fn create_table_defs(sql: &str) -> std::collections::BTreeMap<String, String> {
        let mut defs = std::collections::BTreeMap::new();
        for fragment in sql.split(';') {
            let mut code = String::new();
            for line in fragment.lines() {
                let line = match line.find("--") {
                    Some(i) => &line[..i],
                    None => line,
                };
                code.push_str(line);
                code.push('\n');
            }
            let stmt = code.trim();
            let mut toks = stmt.split_whitespace();
            if !toks
                .next()
                .is_some_and(|t| t.eq_ignore_ascii_case("CREATE"))
            {
                continue;
            }
            if !toks.next().is_some_and(|t| t.eq_ignore_ascii_case("TABLE")) {
                continue;
            }
            let mut name = toks.next().unwrap_or("");
            // Optional `IF NOT EXISTS` between TABLE and the name.
            if name.eq_ignore_ascii_case("IF") {
                for _ in 0..2 {
                    toks.next();
                }
                name = toks.next().unwrap_or("");
            }
            if name.is_empty() {
                continue;
            }
            defs.insert(name.trim_end_matches('(').to_string(), normalize_sql(stmt));
        }
        defs
    }

    fn fingerprint(conn: &Connection) -> SchemaFingerprint {
        let mut objects = std::collections::BTreeSet::new();
        let mut index_sql = std::collections::BTreeMap::new();
        let mut tables = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT type, name, tbl_name, sql FROM sqlite_master
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .unwrap();
            for row in rows {
                let (typ, name, tbl, sql): (String, String, String, Option<String>) = row.unwrap();
                if typ == "table" {
                    tables.push(name.clone());
                }
                if typ == "index" {
                    if let Some(def) = sql {
                        index_sql.insert(name.clone(), normalize_sql(&def));
                    }
                }
                objects.insert((typ, name, tbl));
            }
        }
        let mut columns = std::collections::BTreeMap::new();
        for table in &tables {
            // Table names are internal (never user input); PRAGMA takes no params.
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let mut cols = std::collections::BTreeMap::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(1)?,
                        ColumnShape {
                            coltype: row.get(2)?,
                            notnull: row.get(3)?,
                            dflt: row.get(4)?,
                            pk: row.get(5)?,
                        },
                    ))
                })
                .unwrap();
            for row in rows {
                let (name, shape) = row.unwrap();
                cols.insert(name, shape);
            }
            columns.insert(table.clone(), cols);
        }
        SchemaFingerprint {
            objects,
            columns,
            index_sql,
        }
    }

    /// Scanner behind the `migration_steps_pin_statement_counts_and_literals`
    /// pin: true when `;` appears inside a `'...'` or `"..."` string literal
    /// (`''` / `""` count as escapes, not terminators).
    fn contains_semicolon_in_literal(stmt: &str) -> bool {
        let mut chars = stmt.chars().peekable();
        let mut quote: Option<char> = None;
        while let Some(c) = chars.next() {
            match quote {
                None => {
                    if c == '\'' || c == '"' {
                        quote = Some(c);
                    }
                }
                Some(q) => {
                    if c == q {
                        // A doubled quote is an escape, not the terminator.
                        if chars.peek() == Some(&q) {
                            chars.next();
                        } else {
                            quote = None;
                        }
                    }
                }
            }
            if c == ';' && quote.is_some() {
                return true;
            }
        }
        false
    }

    #[test]
    fn splitter_assumption_probe_flags_semicolon_in_literal() {
        // Scratch probes for the `;`-splitter assumption behind
        // `apply_migration_step` — these strings are NOT migration SQL, and
        // this test never touches MIGRATIONS itself.
        assert!(contains_semicolon_in_literal(
            "ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'a;b'"
        ));
        assert!(contains_semicolon_in_literal(
            "CREATE TABLE t (a TEXT DEFAULT 'it''s; ok')"
        ));
        assert!(contains_semicolon_in_literal(r#"SELECT ";"#));
        assert!(!contains_semicolon_in_literal(
            "ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'plain'"
        ));
        assert!(!contains_semicolon_in_literal(
            "CHECK (status IN ('pending', 'approved'))"
        ));
        assert!(!contains_semicolon_in_literal("SELECT 1"));
    }

    #[test]
    fn migration_steps_pin_statement_counts_and_literals() {
        // Pin the splitter's input shape: exact per-step statement counts
        // plus no `;` inside any string literal across all MIGRATIONS
        // entries. Extend the counts deliberately when appending a version.
        const EXPECTED_STATEMENT_COUNTS: [usize; 9] = [0, 5, 3, 2, 1, 1, 4, 1, 2];
        assert_eq!(
            MIGRATIONS.len(),
            EXPECTED_STATEMENT_COUNTS.len(),
            "appending a version means extending EXPECTED_STATEMENT_COUNTS"
        );
        for (version, step) in MIGRATIONS.iter().enumerate() {
            let stmts = split_step_statements(step);
            assert_eq!(
                stmts.len(),
                EXPECTED_STATEMENT_COUNTS[version],
                "step {version} statement count changed; update the pin deliberately"
            );
            for stmt in stmts {
                assert!(
                    !contains_semicolon_in_literal(stmt),
                    "step {version} hides a `;` inside a string literal, breaking the `;`-splitter"
                );
            }
        }
    }

    #[test]
    fn never_altered_table_definitions_match_snapshot() {
        // Static pin for the CHECK / table-UNIQUE drift class the DB parity
        // test cannot see (see NEVER_ALTERED_TABLES): for tables no step ever
        // ALTERs, the step DDL and `schema.sql` must define them identically
        // modulo whitespace. A CHECK widened in one source only (or a
        // table-level UNIQUE added on one side) fails here.
        let snapshot = include_str!("../../migrations/schema.sql");
        let snapshot_defs = create_table_defs(snapshot);
        let mut step_defs = std::collections::BTreeMap::new();
        for step in MIGRATIONS {
            step_defs.extend(create_table_defs(step));
        }
        for table in NEVER_ALTERED_TABLES {
            let step_def = step_defs.get(*table);
            let snapshot_def = snapshot_defs.get(*table);
            assert!(
                step_def.is_some() && snapshot_def.is_some(),
                "CREATE TABLE {table} must exist in both MIGRATIONS and schema.sql"
            );
            assert_eq!(
                step_def, snapshot_def,
                "CREATE TABLE {table} diverged between MIGRATIONS and schema.sql"
            );
        }
    }

    #[test]
    fn stepped_v0_upgrade_matches_fresh_install() {
        // The union of versioned MIGRATIONS steps must equal the
        // fresh-install snapshot: upgrade a fabricated v0 DB through every
        // step, then diff sqlite_master + table_info against a fresh
        // install. Any step that stops matching schema.sql (a dropped
        // column, table, or index — the v8 reactions near-miss class) fails
        // this test.
        let (upgraded, _dir1) = test_pool("parity_upgraded.db");
        {
            let conn = upgraded.get().unwrap();
            conn.execute_batch(V1_FIXTURE_SCHEMA).unwrap();
            assert_eq!(user_version(&conn).unwrap(), 0);
        }
        run_migrations(&upgraded, None).unwrap();

        let (fresh, _dir2) = test_pool("parity_fresh.db");
        run_migrations(&fresh, None).unwrap();

        assert_eq!(db_version(&upgraded), LATEST_SCHEMA_VERSION);
        assert_eq!(db_version(&fresh), LATEST_SCHEMA_VERSION);

        let stepped = fingerprint(&upgraded.get().unwrap());
        let snapshot = fingerprint(&fresh.get().unwrap());
        assert_eq!(
            stepped, snapshot,
            "stepped v0 upgrade diverged from the fresh-install snapshot"
        );
    }

    #[test]
    fn ragged_v0_db_upgrades_and_matches_fresh_fingerprint() {
        // Ragged legacy shape: base comments with only SOME late columns
        // already present (partial history), webmention_seen present with
        // rows, github_profiles entirely missing, one index never created,
        // user_version == 0. The upgrade must fill exactly the gaps, keep
        // every row, and converge on the fresh-install shape.
        let (pool, _dir) = test_pool("ragged.db");
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
                    honeypot INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE webmention_seen (
                    source TEXT NOT NULL, target TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
                    last_status TEXT NOT NULL, PRIMARY KEY (source, target)
                );
                CREATE INDEX idx_comments_read
                    ON comments(target_path, status, created_at);
                INSERT INTO comments (target_path, comment_type, author_name, content, status)
                    VALUES ('/ragged', 'native', 'Rae', 'kept!', 'approved');
                INSERT INTO webmention_seen (source, target, last_status)
                    VALUES ('https://src.example/r', 'https://site.example/ragged', 'alive');",
            )
            .unwrap();
            assert_eq!(user_version(&conn).unwrap(), 0);
        }

        run_migrations(&pool, None).unwrap();
        assert_eq!(db_version(&pool), LATEST_SCHEMA_VERSION);

        let conn = pool.get().unwrap();
        // Rows survived.
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM comments WHERE author_name = 'Rae'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let seen: String = conn
            .query_row(
                "SELECT last_status FROM webmention_seen WHERE source = 'https://src.example/r'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seen, "alive");
        // Gaps filled: the missing table, the missing columns, the missing indexes.
        assert!(table_exists(&conn, "github_profiles").unwrap());
        assert!(table_exists(&conn, "comment_urls").unwrap());
        assert!(table_exists(&conn, "comment_reactions").unwrap());
        for col in [
            "delete_token",
            "submitter_ip",
            "content_hash",
            "submitter_ip_hash",
        ] {
            assert!(
                column_exists(&conn, "comments", col).unwrap(),
                "missing column {col} after ragged upgrade"
            );
        }
        for idx in [
            "idx_comments_source_target",
            "idx_comments_parent",
            "idx_comment_urls_hash",
            "idx_comment_reactions_read",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    rusqlite::params![idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing index {idx} after ragged upgrade");
        }
        // Converged: same shape as a fresh install.
        let (fresh, _dir2) = test_pool("ragged_fresh.db");
        run_migrations(&fresh, None).unwrap();
        assert_eq!(
            fingerprint(&conn),
            fingerprint(&fresh.get().unwrap()),
            "ragged v0 upgrade diverged from the fresh-install snapshot"
        );
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
            conn.execute_batch(V1_FIXTURE_SCHEMA).unwrap();
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

        // A realistic pre-v7 database: every table, column, and index a v6
        // database would have (base tables plus all objects through the v6
        // step), raw peer addresses stored, no hash column yet.
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
                    CREATE INDEX idx_comments_read
                        ON comments(target_path, status, created_at);
                    CREATE UNIQUE INDEX idx_comments_source_target
                        ON comments(source_url, target_path)
                        WHERE source_url IS NOT NULL;
                    CREATE INDEX idx_comments_parent ON comments(parent_id);
                    CREATE TABLE comment_urls (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        comment_id INTEGER NOT NULL REFERENCES comments(id),
                        url TEXT NOT NULL,
                        domain TEXT NOT NULL,
                        url_hash TEXT NOT NULL
                    );
                    CREATE INDEX idx_comment_urls_comment ON comment_urls(comment_id);
                    CREATE INDEX idx_comment_urls_domain ON comment_urls(domain);
                    CREATE INDEX idx_comment_urls_hash ON comment_urls(url_hash);
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
    fn legacy_duplicate_source_target_fails_with_actionable_error() {
        // A pre-versioning database that predates the partial unique index
        // idx_comments_source_target and holds two rows with the same
        // (source_url, target_path) pair.
        let (pool, _dir) = test_pool("dupes.db");
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
                INSERT INTO comments (target_path, comment_type, source_url, author_name, content, status)
                    VALUES ('/dup', 'webmention', 'https://src.example/post', 'Ada', 'first', 'approved'),
                           ('/dup', 'webmention', 'https://src.example/post', 'Bob', 'second', 'pending');",
            )
            .unwrap();
            assert_eq!(user_version(&conn).unwrap(), 0);
        }

        let err = run_migrations(&pool, None).unwrap_err().to_string();
        assert!(
            err.contains("idx_comments_source_target"),
            "error must name the conflicting index, got: {err}"
        );
        assert!(
            err.contains("https://src.example/post"),
            "error must identify the offending pair, got: {err}"
        );
        assert!(
            err.to_lowercase().contains("dedup") || err.contains("DELETE FROM"),
            "error must name the dedup remedy, got: {err}"
        );
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
