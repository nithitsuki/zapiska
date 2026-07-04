pub mod pool;
pub mod repo;

pub use pool::*;
pub use repo::*;

#[derive(Debug)]
pub enum RepoError {
    Internal(String),
    NotFound(String),
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal(msg) => write!(f, "repo internal error: {msg}"),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
        }
    }
}

impl std::error::Error for RepoError {}

impl From<RepoError> for crate::error::AppError {
    fn from(e: RepoError) -> Self {
        match e {
            RepoError::Internal(msg) => Self::Internal(msg),
            RepoError::NotFound(msg) => Self::NotFound(msg),
        }
    }
}
