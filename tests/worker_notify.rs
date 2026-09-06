//! Worker notification behavior through [`WebmentionProcessor`]: a webmention
//! must notify admin channels on the FIRST sighting of a (source, target)
//! pair, must NOT re-notify on updates, and MUST notify again when the same
//! source mentions a second page (pair-keyed `is_new`).
//!
//! The source fetch is a canned [`SourceFetcher`] (no network, no loopback
//! escape hatch); only the Telegram delivery still hits wiremock.
#![cfg(feature = "webmentions")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use zapiska::db::pool::{create_pool, run_migrations};
use zapiska::db::repo::Repo;
use zapiska::fetch::{FetchError, FetchedDoc, SourceFetcher};
use zapiska::github::StubGitHub;
use zapiska::notify::NotificationBatcher;
use zapiska::worker::{WebmentionJob, WebmentionProcessor};

const TARGET: &str = "https://nithitsuki.com/blog/wm-notify";
const TARGET_A: &str = "https://nithitsuki.com/blog/wm-multi-a";
const TARGET_B: &str = "https://nithitsuki.com/blog/wm-multi-b";

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

/// One source page linking TWO targets (multi-target pair-keying proof).
fn two_target_html() -> String {
    format!(
        r#"<!DOCTYPE html>
<html><head><title>Remote post</title></head>
<body>
<article class="h-entry">
  <div class="p-author h-card"><a class="u-url" href="https://remote.example">Remote Author</a></div>
  <div class="e-content"><p>Two pages, one post.</p></div>
</article>
<a href="{TARGET_A}">first</a>
<a href="{TARGET_B}">second</a>
</body></html>"#
    )
}

/// Canned source fetcher: scripted HTML per URL, no network. The
/// integration-level replacement for `allow_loopback=true`.
enum Canned {
    Html(String),
}

struct CannedFetcher {
    responses: Mutex<HashMap<String, Canned>>,
}

impl CannedFetcher {
    fn with_html(url: &str, html: String) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(HashMap::from([(url.to_string(), Canned::Html(html))])),
        })
    }
}

#[async_trait::async_trait]
impl SourceFetcher for CannedFetcher {
    async fn fetch_source(&self, url: &str) -> Result<FetchedDoc, FetchError> {
        match self.responses.lock().expect("canned lock").get(url) {
            Some(Canned::Html(body)) => {
                let parsed = url::Url::parse(url).map_err(|e| FetchError::InvalidUrl {
                    url: url.to_string(),
                    source: e,
                })?;
                Ok(FetchedDoc {
                    url: parsed,
                    status: reqwest::StatusCode::OK,
                    bytes: body.as_bytes().to_vec(),
                    doc: scraper::Html::parse_document(body),
                })
            }
            None => Err(FetchError::HttpStatus {
                url: url.to_string(),
                status: 404,
            }),
        }
    }
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

fn processor(
    repo: Repo,
    fetcher: Arc<dyn SourceFetcher>,
    client: reqwest::Client,
    notifier: Arc<NotificationBatcher>,
) -> WebmentionProcessor {
    let github: Arc<dyn zapiska::github::GitHubLookup> = Arc::new(StubGitHub);
    WebmentionProcessor::with_fetcher(
        repo,
        fetcher,
        github,
        notifier,
        client,
        "https://nithitsuki.com".to_string(),
        2000,
        Duration::from_secs(5),
        None,
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

    let (repo, client, notifier, _dir) = setup(&server).await;
    let source = format!("{}/post", server.uri());
    let fetcher = CannedFetcher::with_html(&source, source_html());
    let proc = processor(repo, fetcher, client, notifier);
    let job = WebmentionJob {
        source: source.clone(),
        target: TARGET.to_string(),
    };

    // First processing: new comment → notification fires.
    proc.process(&job).await.unwrap();
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
    proc.process(&job).await.unwrap();
    let reqs = poll_telegram_requests(&server, 1).await;
    assert_eq!(reqs.len(), 1, "updates must not re-notify");
}

#[tokio::test]
async fn second_page_for_same_source_notifies() {
    // `is_new` keys on the upsert pair (source, target_path): one source
    // mentioning two pages is two first sightings → two notifications.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true, "result": {"message_id": 1}
        })))
        .mount(&server)
        .await;

    let (repo, client, notifier, _dir) = setup(&server).await;
    let source = format!("{}/two-pages", server.uri());
    let fetcher = CannedFetcher::with_html(&source, two_target_html());
    let proc = processor(repo, fetcher, client, notifier);

    proc.process(&WebmentionJob {
        source: source.clone(),
        target: TARGET_A.to_string(),
    })
    .await
    .unwrap();
    let reqs = poll_telegram_requests(&server, 1).await;
    assert_eq!(reqs.len(), 1, "first page notifies");

    proc.process(&WebmentionJob {
        source: source.clone(),
        target: TARGET_B.to_string(),
    })
    .await
    .unwrap();
    let reqs = poll_telegram_requests(&server, 2).await;
    assert_eq!(reqs.len(), 2, "second page notifies on its own");
    let texts: Vec<String> = reqs
        .iter()
        .map(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("/blog/wm-multi-a")),
        "first page messaged: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("/blog/wm-multi-b")),
        "second page messaged: {texts:?}"
    );
}
