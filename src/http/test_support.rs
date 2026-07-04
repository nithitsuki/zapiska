#[cfg(test)]
pub(crate) mod helpers {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::config::Config;
    use crate::db::pool::{create_pool, run_migrations};
    use crate::db::repo::CommentsRepo;
    use crate::github::{GitHubLookup, StubGitHub};
    use crate::state::AppState;
    use crate::worker;

    /// A test-ready AppState + a TempDir that keeps the DB file alive.
    pub fn test_state() -> (AppState, TempDir) {
        test_state_with_github(Arc::new(StubGitHub))
    }

    /// Like `test_state` but with a custom GitHubLookup implementation.
    pub fn test_state_with_github(github: Arc<dyn GitHubLookup>) -> (AppState, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("test.db");
        let pool = create_pool(&path.to_string_lossy()).expect("pool");
        run_migrations(&pool).expect("migrations");
        let repo = CommentsRepo::new(pool.clone());
        let (wm_sender, rx) = worker::channel(64);
        worker::spawn_worker(rx);

        let state = AppState {
            config: Config {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                public_target_origin: "https://nithitsuki.com".to_string(),
                allowed_cors_origin: "https://nithitsuki.com".to_string(),
                admin_token: "test".to_string(),
                database_path: ":memory:".to_string(),
                github_token: None,
                max_content_len: 2000,
                max_author_len: 100,
                max_body_size: 8192,
                fetch_timeout_ms: 4000,
                worker_backlog: 64,
                rust_log: "info".to_string(),
            },
            pool,
            repo,
            github,
            wm_sender,
            http_client: reqwest::Client::builder()
                .build()
                .expect("test reqwest client"),
        };

        (state, dir)
    }

    /// Helper to build an HTTP request for testing.
    pub fn request(method: axum::http::Method, uri: &str) -> axum::http::Request<axum::body::Body> {
        let is_write = method == axum::http::Method::POST || method == axum::http::Method::PUT;
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .extension(axum::extract::ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))))
            .body(axum::body::Body::empty())
            .unwrap();
        if is_write {
            req.headers_mut()
                .insert(axum::http::header::CONTENT_LENGTH, 0u64.into());
        }
        req
    }

    pub fn form_request(uri: &str, body: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header(
                axum::http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(axum::http::header::CONTENT_LENGTH, body.len())
            .extension(axum::extract::ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))))
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap()
    }
}
