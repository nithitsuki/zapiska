use std::sync::Arc;

use reqwest::Client;

use crate::config::Config;
use crate::db::pool::SqlitePool;
use crate::db::repo::CommentsRepo;
use crate::github::GitHubLookup;
use crate::worker::JobSender;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub pool: SqlitePool,
    pub repo: CommentsRepo,
    pub github: Arc<dyn GitHubLookup>,
    pub wm_sender: JobSender,
    /// SSRF-safe HTTP client for outbound fetches.
    pub http_client: Client,
}
