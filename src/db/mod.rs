pub mod pool;
pub mod repo;

pub use pool::*;
pub use repo::*;

#[derive(Debug)]
pub enum RepoError {
    /// A SQLite constraint violation (UNIQUE, FOREIGN KEY, CHECK, NOT NULL,
    /// PRIMARY KEY…): the row is invalid. Callers with per-row skip policies
    /// (e.g. restore) act on this variant; see T17.
    Constraint(String),
    /// The database is locked by another connection (`SQLITE_BUSY`,
    /// `SQLITE_LOCKED` incl. `SQLITE_BUSY_SNAPSHOT`): retry with backoff.
    Busy(String),
    /// A storage I/O failure (`SQLITE_IOERR` family, `SQLITE_FULL`,
    /// `SQLITE_CANTOPEN`): retryable once the environment recovers.
    Io(String),
    /// Every other storage failure: corrupt/read-only/misuse errors and all
    /// row-decode errors (`FromSql*`, `InvalidColumn*`, …). Decode failures
    /// mean the stored data or the query no longer matches the code — never
    /// a per-row validity signal — so they are `Other`, not `Constraint`.
    /// Non-SQLite plumbing failures (pool acquire, task join) are `Other`
    /// too: they carry no SQLite result code to classify by.
    Other(String),
    NotFound(String),
}

/// Mapping table (rusqlite 0.39 `Error` → `RepoError`), decided from the
/// `libsqlite3-sys` primary result code (`extended_code & 0xFF`, exposed as
/// `ffi::Error::code`), so every extended code of a family classifies with
/// its primary:
///
/// | SQLite primary (extended examples)                                   | Variant    |
/// |----------------------------------------------------------------------|------------|
/// | `SQLITE_CONSTRAINT` (`UNIQUE` 2067, `FOREIGNKEY` 787, `CHECK` 275,   | `Constraint` |
/// |  `NOTNULL`, `PRIMARYKEY`, `TRIGGER`, …)                             |            |
/// | `SQLITE_BUSY` (incl. `BUSY_SNAPSHOT` 517), `SQLITE_LOCKED`           | `Busy`       |
/// | `SQLITE_IOERR` (incl. `IOERR_WRITE` 778, …), `SQLITE_FULL`,          | `Io`         |
/// |  `SQLITE_CANTOPEN`                                                   |            |
/// | everything else `SqliteFailure` (`CORRUPT`, `NOTADB`, `NOMEM`,       | `Other`      |
/// |  `READONLY`, `INTERRUPT`, `ABORT`, `MISMATCH`, `MISUSE`, …)          |            |
/// | non-`SqliteFailure` (`FromSqlConversionFailure`,                    | `Other`      |
/// |  `IntegralValueOutOfRange`, `Utf8Error`, `InvalidColumnType/Index/  |            |
/// |  Name`, `InvalidParameter*`, `ToSqlConversionFailure`, …)           |            |
///
/// Single conversion point: every rusqlite boundary in `repo/*` maps via
/// this `From` impl, so classification policy lives here, not at call sites.
impl From<rusqlite::Error> for RepoError {
    fn from(e: rusqlite::Error) -> Self {
        match &e {
            rusqlite::Error::SqliteFailure(sqlite_err, _) => {
                use rusqlite::ffi::ErrorCode as Code;
                let msg = e.to_string();
                match sqlite_err.code {
                    Code::ConstraintViolation => Self::Constraint(msg),
                    Code::DatabaseBusy | Code::DatabaseLocked => Self::Busy(msg),
                    Code::SystemIoFailure | Code::DiskFull | Code::CannotOpen => Self::Io(msg),
                    _ => Self::Other(msg),
                }
            }
            _ => Self::Other(e.to_string()),
        }
    }
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Byte-identical to the old `Internal` display: no API change.
            Self::Constraint(msg) | Self::Busy(msg) | Self::Io(msg) | Self::Other(msg) => {
                write!(f, "repo internal error: {msg}")
            }
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
        }
    }
}

impl std::error::Error for RepoError {}

impl From<RepoError> for crate::error::AppError {
    fn from(e: RepoError) -> Self {
        match e {
            // All storage failures stay 500s, exactly as before.
            RepoError::Constraint(msg)
            | RepoError::Busy(msg)
            | RepoError::Io(msg)
            | RepoError::Other(msg) => Self::Internal(msg),
            RepoError::NotFound(msg) => Self::NotFound(msg),
        }
    }
}

#[cfg(test)]
mod t14_typed_error_tests {
    use super::*;
    use crate::db::pool::{create_pool, run_migrations};

    /// Synthesize the `rusqlite::Error` SQLite would return for a given
    /// extended result code (e.g. `SQLITE_CONSTRAINT_UNIQUE`).
    fn sqlite_failure(extended_code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(extended_code),
            Some("synthesized failure".to_string()),
        )
    }

    #[test]
    fn unique_violation_maps_to_constraint() {
        let err = RepoError::from(sqlite_failure(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE));
        assert!(matches!(err, RepoError::Constraint(_)));
    }

    #[test]
    fn fk_violation_maps_to_constraint() {
        let err = RepoError::from(sqlite_failure(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY));
        assert!(matches!(err, RepoError::Constraint(_)));
    }

    #[test]
    fn check_violation_maps_to_constraint() {
        let err = RepoError::from(sqlite_failure(rusqlite::ffi::SQLITE_CONSTRAINT_CHECK));
        assert!(matches!(err, RepoError::Constraint(_)));
    }

    #[test]
    fn busy_and_locked_map_to_busy() {
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
            rusqlite::ffi::SQLITE_LOCKED,
        ] {
            let err = RepoError::from(sqlite_failure(code));
            assert!(matches!(err, RepoError::Busy(_)), "code {code}");
        }
    }

    #[test]
    fn io_family_maps_to_io() {
        for code in [
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_IOERR_WRITE,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_CANTOPEN,
        ] {
            let err = RepoError::from(sqlite_failure(code));
            assert!(matches!(err, RepoError::Io(_)), "code {code}");
        }
    }

    #[test]
    fn other_sqlite_failures_map_to_other() {
        for code in [
            rusqlite::ffi::SQLITE_CORRUPT,
            rusqlite::ffi::SQLITE_NOTADB,
            rusqlite::ffi::SQLITE_NOMEM,
            rusqlite::ffi::SQLITE_READONLY,
            rusqlite::ffi::SQLITE_INTERRUPT,
            rusqlite::ffi::SQLITE_MISMATCH,
            rusqlite::ffi::SQLITE_MISUSE,
        ] {
            let err = RepoError::from(sqlite_failure(code));
            assert!(matches!(err, RepoError::Other(_)), "code {code}");
        }
    }

    #[test]
    fn row_decode_errors_map_to_other() {
        let integral = RepoError::from(rusqlite::Error::IntegralValueOutOfRange(0, 1000));
        assert!(matches!(integral, RepoError::Other(_)));
        let coltype = RepoError::from(rusqlite::Error::InvalidColumnType(
            0,
            "x".to_string(),
            rusqlite::types::Type::Text,
        ));
        assert!(matches!(coltype, RepoError::Other(_)));
        let conv = RepoError::from(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("bad value")),
        ));
        assert!(matches!(conv, RepoError::Other(_)));
    }

    #[test]
    fn display_and_http_mapping_unchanged_per_variant() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        // (variant, legacy display, http status)
        let cases: Vec<(RepoError, &str, StatusCode)> = vec![
            (
                RepoError::Constraint("boom".to_string()),
                "repo internal error: boom",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                RepoError::Busy("boom".to_string()),
                "repo internal error: boom",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                RepoError::Io("boom".to_string()),
                "repo internal error: boom",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                RepoError::Other("boom".to_string()),
                "repo internal error: boom",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                RepoError::NotFound("gone".to_string()),
                "not found: gone",
                StatusCode::NOT_FOUND,
            ),
        ];
        for (err, display, status) in cases {
            assert_eq!(err.to_string(), display);
            let resp = crate::error::AppError::from(err).into_response();
            assert_eq!(resp.status(), status);
        }
    }

    fn setup_repo() -> (crate::db::Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t14_test.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (crate::db::Repo::new(pool), dir)
    }

    fn native_comment(source: Option<&str>) -> crate::db::NewComment {
        crate::db::NewComment {
            target_path: "/t14".to_string(),
            comment_type: "webmention".to_string(),
            source_url: source.map(|s| s.to_string()),
            author_name: "T14".to_string(),
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
    async fn duplicate_source_target_surfaces_as_constraint() {
        let (repo, _dir) = setup_repo();
        repo.insert_comment(native_comment(Some("https://dup.example/post")))
            .await
            .unwrap();
        let err = repo
            .insert_comment(native_comment(Some("https://dup.example/post")))
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Constraint(_)));
    }

    #[tokio::test]
    async fn orphaned_reaction_upsert_surfaces_as_constraint() {
        let (repo, _dir) = setup_repo();
        let err = repo
            .upsert_reaction(999_999, "👍", "h:orphan")
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Constraint(_)));
    }

    #[tokio::test]
    async fn orphaned_url_insert_surfaces_as_constraint() {
        let (repo, _dir) = setup_repo();
        let err = repo
            .insert_urls(
                999_999,
                vec![(
                    "https://x.example/".to_string(),
                    "x.example".to_string(),
                    "h".to_string(),
                )],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Constraint(_)));
    }

    #[tokio::test]
    async fn import_orphan_reaction_surfaces_as_constraint() {
        let (repo, _dir) = setup_repo();
        let err = repo
            .import_comment_reaction(crate::db::CommentReaction {
                id: 1,
                comment_id: 999_999,
                reaction: "👍".to_string(),
                identifier: "h:orphan".to_string(),
                status: "approved".to_string(),
                created_at: "2026-01-01 00:00:00".to_string(),
                updated_at: "2026-01-01 00:00:00".to_string(),
            })
            .await
            .unwrap_err();
        // Flowing, not yet acted on: the import handler still `?`-aborts
        // (T17 owns the skip-and-count policy).
        assert!(matches!(err, RepoError::Constraint(_)));
    }

    #[tokio::test]
    async fn check_violation_surfaces_as_constraint() {
        let (repo, _dir) = setup_repo();
        let id = repo.insert_comment(native_comment(None)).await.unwrap();
        let err = repo.update_status(id, "bogus-status").await.unwrap_err();
        assert!(matches!(err, RepoError::Constraint(_)));
    }
}
