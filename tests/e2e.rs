use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use tempfile::tempdir;
#[cfg(feature = "webmentions")]
use wiremock::matchers::{method, path};
#[cfg(feature = "webmentions")]
use wiremock::{Mock, MockServer, ResponseTemplate};

use zapiska::config::Config;
use zapiska::db::pool::{create_pool, run_migrations};
use zapiska::db::repo::CommentsRepo;
use zapiska::http::build_app;
use zapiska::state::{AppState, Limiter};
#[cfg(feature = "webmentions")]
use zapiska::worker;

/// Helper: start a real server on a random port, return the base URL + state.
async fn start_server() -> (String, AppState) {
    let config = Config {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        public_target_origin: "https://nithitsuki.com".to_string(),
        allowed_cors_origin: "https://nithitsuki.com".to_string(),
        admin_token: "test-admin-token".to_string(),
        database_path: ":memory:".to_string(),
        github_token: None,
        max_content_len: 2000,
        max_author_len: 100,
        max_body_size: 8192,
        fetch_timeout_ms: 4000,
        worker_backlog: 64,
        rust_log: "info".to_string(),
    honeypot_field: "website".to_string(),
    max_comments_per_ip_per_day: 50,
    max_webmentions_per_domain_per_hour: 10,
    store_ip_address: false,
    moderation_webhook_url: None,
    moderation_webhook_mode: "async".to_string(),
    default_comment_status: "pending".to_string(),
    max_thread_depth: 0,
    };

    let dir = tempdir().unwrap();
    let path = dir.path().join("e2e.db");
    let pool = create_pool(&path.to_string_lossy()).unwrap();
    run_migrations(&pool).unwrap();
    let repo = CommentsRepo::new(pool.clone());

    let http_client = reqwest::Client::builder().build().unwrap();

    #[cfg(feature = "webmentions")]
    let (wm_sender, mut wm_receiver) = worker::channel(config.worker_backlog);
    #[cfg(feature = "webmentions")]
    tokio::spawn(async move {
        while let Some(job) = wm_receiver.recv().await {
            tracing::debug!(source = %job.source, "e2e test worker drained job");
        }
    });

    let state = AppState {
        config,
        pool,
        repo: repo.clone(),
        github: Arc::new(zapiska::github::StubGitHub),
        #[cfg(feature = "webmentions")]
        wm_sender,
        http_client: http_client.clone(),
        limiter: Arc::new(Limiter::new()),
    };

    let app = build_app(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // Keep the temp dir alive for the test duration.
    std::mem::forget(dir);

    (base, state)
}

fn urlencode(s: &str) -> String {
    s.replace(' ', "+")
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[tokio::test]
async fn e2e_native_post_then_moderate_then_read() {
    let (base, _state) = start_server().await;
    let client = Client::new();

    // ── Post a native comment ─────────────────────────────────
    let resp = client
        .post(format!("{}/api/comment", base))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("target_path", "/e2e-test"),
            ("author_name", "E2E User"),
            ("content", "Hello from e2e test!"),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "native comment created");

    // ── It should NOT appear in public read yet ───────────────
    let resp = client
        .get(format!("{}/api/comments?path=/e2e-test", base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 0, "no approved comments yet");

    // ── Find the pending comment's id via admin API ───────────
    let resp = client
        .get(format!("{}/api/admin/pending", base))
        .header("Authorization", "Bearer test-admin-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let pending: serde_json::Value = resp.json().await.unwrap();
    let comments = pending["comments"].as_array().unwrap();
    let pending_id = comments[0]["id"].as_i64().unwrap();

    // ── Approve it ────────────────────────────────────────────
    let approve = serde_json::json!({ "id": pending_id, "action": "approved" });
    let resp = client
        .post(format!("{}/api/admin/moderate", base))
        .header("Authorization", "Bearer test-admin-token")
        .json(&approve)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "moderation approved");

    // ── Now it should appear in public read ───────────────────
    let resp = client
        .get(format!("{}/api/comments?path=/e2e-test", base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 1, "one approved comment visible");
    assert_eq!(body["comments"][0]["author_name"], "E2E User");
    // Content is ammonia-sanitized (no HTML in this case).
    assert_eq!(body["comments"][0]["content"], "Hello from e2e test!");

    // Verify the JSON shape matches the spec contract.
    let c = &body["comments"][0];
    assert!(c.get("id").is_some());
    assert!(c.get("comment_type").is_some());
    assert!(c.get("author_name").is_some());
    assert!(c.get("content").is_some());
    assert!(c.get("created_at").is_some());
    // Internal fields MUST NOT be exposed.
    assert!(c.get("source_url").is_none(), "source_url must not leak");
    assert!(c.get("status").is_none(), "status must not leak");
    assert!(c.get("updated_at").is_none(), "updated_at must not leak");
    assert!(c.get("target_path").is_none(), "target_path must not leak");
}

#[tokio::test]
async fn e2e_native_post_stays_pending_in_public_read() {
    let (base, _state) = start_server().await;
    let client = Client::new();

    let resp = client
        .post(format!("{}/api/comment", base))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("target_path", "/e2e-private"),
            ("author_name", "Pending Only"),
            ("content", "Should not appear"),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Without approving, it should not be visible.
    let resp = client
        .get(format!("{}/api/comments?path=/e2e-private", base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 0, "non-approved comments not visible");
    assert!(body["comments"].as_array().unwrap().is_empty());
}

#[cfg(feature = "webmentions")]
#[tokio::test]
async fn e2e_webmention_full_lifecycle() {
    let source_mock = MockServer::start().await;

    // Source HTML contains a backlink to the target.
    Mock::given(method("GET"))
        .and(path("/post/hello"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html><body>
                    <div class="h-entry">
                        <span class="p-author">Remote Bob</span>
                        <div class="e-content"><p>Nice article!</p></div>
                    </div>
                    <a href="https://nithitsuki.com/blog/e2e-wm">backlink</a>
                </body></html>"#,
        ))
        .mount(&source_mock)
        .await;

    let (base, state) = start_server().await;
    let client = Client::new();

    // ── Send the webmention ───────────────────────────────────
    let source_url = format!("{}/post/hello", source_mock.uri());
    let resp = client
        .post(format!("{}/api/webmention", base))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("source", &source_url),
            ("target", "https://nithitsuki.com/blog/e2e-wm"),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "webmention accepted");

    // ── Manually process the job (bypassing SSRF loopback check) ─
    // This avoids the need for the worker thread; we call process_job
    // directly with allow_loopback=true so it can reach the wiremock.
    let job = zapiska::worker::WebmentionJob {
        source: source_url.clone(),
        target: "https://nithitsuki.com/blog/e2e-wm".to_string(),
    };
    let http_client = reqwest::Client::builder().build().unwrap();
    let github: Arc<dyn zapiska::github::GitHubLookup> =
        Arc::new(zapiska::github::StubGitHub);
    zapiska::worker::process_job(
        &job,
        &state.repo,
        &http_client,
        &github,
        "https://nithitsuki.com",
        state.config.max_content_len,
        true,
    )
    .await
    .unwrap();

    // ── Check it appears in the pending queue ─────────────────
    let resp = client
        .get(format!("{}/api/admin/pending", base))
        .header("Authorization", "Bearer test-admin-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let pending: serde_json::Value = resp.json().await.unwrap();
    let comments = pending["comments"].as_array().unwrap();

    // Find the webmention for this path.
    let wm: Vec<&serde_json::Value> = comments
        .iter()
        .filter(|c| c["target_path"] == "/blog/e2e-wm")
        .collect();
    assert!(!wm.is_empty(), "webmention should be in pending queue");
    let wm = wm[0];
    assert_eq!(wm["author_name"], "Remote Bob");
    assert!(wm["content"].as_str().unwrap().contains("Nice article!"));
    assert_eq!(wm["comment_type"], "webmention");
    assert_eq!(wm["status"], "pending");

    // ── Approve it ────────────────────────────────────────────
    let wm_id = wm["id"].as_i64().unwrap();
    let approve = serde_json::json!({ "id": wm_id, "action": "approved" });
    let resp = client
        .post(format!("{}/api/admin/moderate", base))
        .header("Authorization", "Bearer test-admin-token")
        .json(&approve)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // ── Verify it shows in public read ────────────────────────
    let resp = client
        .get(format!("{}/api/comments?path=/blog/e2e-wm", base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let read: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(read["total"], 1);
    assert_eq!(read["comments"][0]["author_name"], "Remote Bob");
}

#[tokio::test]
async fn e2e_rate_limit_blocks_flood_native() {
    let (base, _state) = start_server().await;
    let client = Client::new();
    let body = form_body(&[
        ("target_path", "/rate-limit-test"),
        ("author_name", "Flooder"),
        ("content", "spam"),
    ]);

    // Native comment limit is 5 per 60s with burst 5. Send 6 requests.
    for _ in 0..5 {
        let resp = client
            .post(format!("{}/api/comment", base))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert!(resp.status() == 201 || resp.status() == 429);
    }

    // 6th should be rate limited.
    let resp = client
        .post(format!("{}/api/comment", base))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429, "rate limit should block 6th request");
}
