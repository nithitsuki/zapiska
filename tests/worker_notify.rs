//! Worker notification behavior: a webmention must notify admin channels on
//! the FIRST sighting of a source URL, and must NOT re-notify on updates.
#![cfg(feature = "webmentions")]

use std::sync::Arc;

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use zapiska::db::pool::{create_pool, run_migrations};
use zapiska::db::repo::Repo;
use zapiska::github::StubGitHub;
use zapiska::notify::NotificationBatcher;
use zapiska::worker::{WebmentionJob, process_job};

const TARGET: &str = "https://nithitsuki.com/blog/wm-notify";

/// Source page with a backlink to TARGET and an h-entry author.
fn source_html() -> String {
    format!(
        r#"<!DOCTYPE html>
<html><head><title>Remote post</title></head>
<body>
<article class="h-entry">
  <a class="u-url" href="https://remote.example/post"></a>
  <div class="p-author h-card"><a class="u-url" href="https://remote.example">Remote Author</a></div>
  <div class="e-content"><p>Nice article, thanks!</p></div>
</article>
<a href="{TARGET}">backlink</a>
</body></html>"#
    )
}

async fn setup(
    server: &MockServer,
) -> (
    Repo,
    reqwest::Client,
    Arc<NotificationBatcher>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.db");
    let pool = create_pool(&path.to_string_lossy()).unwrap();
    run_migrations(&pool, None).unwrap();
    let repo = Repo::new(pool.clone());

    // Notifications: Telegram only, immediate mode (no batching).
    let config = zapiska::config::Config {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        admin_token: "test".to_string(),
        database_path: ":memory:".to_string(),
        rate_limit_native_burst: 50,
        rate_limit_webmention_burst: 30,
        rate_limit_read_burst: 60,
        rate_limit_admin_moderate_burst: 10,
        telegram_bot_token: Some("TOK:secret".to_string()),
        telegram_chat_id: Some("@test".to_string()),
        telegram_api_base: server.uri(),
        notify_batch_secs: 0,
        notify_batch_threshold: 0,
        reactions_set: vec!["👍".to_string()],
        ..zapiska::config::Config::default()
    };
    let notifier = Arc::new(NotificationBatcher::new(&config));

    (
        repo,
        reqwest::Client::builder().build().unwrap(),
        notifier,
        dir,
    )
}

async fn poll_telegram_requests(server: &MockServer, expected: usize) -> Vec<wiremock::Request> {
    for _ in 0..100 {
        let posts: Vec<_> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method == "POST")
            .collect();
        if posts.len() >= expected {
            return posts;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == "POST")
        .collect()
}

#[tokio::test]
async fn first_sighting_notifies_update_does_not() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true, "result": {"message_id": 1}
        })))
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(source_html()))
        .mount(&server)
        .await;

    let (repo, client, notifier, _dir) = setup(&server).await;
    let github: Arc<dyn zapiska::github::GitHubLookup> = Arc::new(StubGitHub);
    let job = WebmentionJob {
        source: format!("{}/post", server.uri()),
        target: TARGET.to_string(),
    };

    // First processing: new comment → notification fires.
    process_job(
        &job,
        &repo,
        &client,
        &github,
        "https://nithitsuki.com",
        2000,
        true,
        &notifier,
    )
    .await
    .unwrap();
    let reqs = poll_telegram_requests(&server, 1).await;
    assert_eq!(reqs.len(), 1, "exactly one notification for a new mention");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let text = body["text"].as_str().unwrap();
    assert!(
        text.contains("/blog/wm-notify"),
        "target path in message: {text}"
    );
    assert!(
        text.contains("Remote Author"),
        "h-entry author in message: {text}"
    );
    assert!(
        text.contains("Nice article"),
        "h-entry content in message: {text}"
    );

    // Second processing (update with same source): no new notification.
    process_job(
        &job,
        &repo,
        &client,
        &github,
        "https://nithitsuki.com",
        2000,
        true,
        &notifier,
    )
    .await
    .unwrap();
    let reqs = poll_telegram_requests(&server, 1).await;
    assert_eq!(reqs.len(), 1, "updates must not re-notify");
}
