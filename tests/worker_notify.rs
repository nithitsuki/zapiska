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
    Gone,
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

    fn with_gone(url: &str) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(HashMap::from([(url.to_string(), Canned::Gone)])),
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
            Some(Canned::Gone) => Err(FetchError::Gone(url.to_string())),
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

    // All assembly rides AppState::start_with_github (same path as
    // production AppState::start, minus the GitHub adapter).
    let config = zapiska::config::Config {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        admin_token: "test".to_string(),
        database_path: path.to_string_lossy().to_string(),
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
    let state = zapiska::state::AppState::start_with_github(config, Arc::new(StubGitHub))
        .expect("worker_notify start");

    (state.repo, state.http_client, state.notifier, dir)
}

fn processor(
    repo: Repo,
    fetcher: Arc<dyn SourceFetcher>,
    client: reqwest::Client,
    notifier: Arc<NotificationBatcher>,
    sink: Option<Arc<dyn zapiska::moderation::ModerationSink>>,
) -> WebmentionProcessor {
    let github: Arc<dyn zapiska::github::GitHubLookup> = Arc::new(StubGitHub);
    WebmentionProcessor::with_fetcher(
        repo,
        fetcher,
        github,
        notifier,
        client,
        "https://nithitsuki.com".parse().expect("test origin valid"),
        2000,
        Duration::from_secs(5),
        sink,
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
    let proc = processor(repo, fetcher, client, notifier, None);
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
    let proc = processor(repo, fetcher, client, notifier, None);

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

#[tokio::test]
async fn prod_wiring_gone_delete_emits_signed_status_changed() {
    // T20-F1: what IS pinned here is the shared sink builder plus
    // processor emission — `Config::worker_moderation_sink` (the same
    // builder `AppState::start` feeds the worker spawn) produces the sink,
    // and a gone-delete through a hand-built processor carrying
    // `Some(that sink)` POSTs a SIGNED comment.status_changed. NOT pinned:
    // the spawned worker itself (it stays idle — no jobs are sent to it),
    // so the spawn call's argument plumbing is review-only. The hook mock
    // is the ONLY webhook URL configured anywhere (no notify channels),
    // so the exactly-one assertion also proves nothing else emits there.
    let hook = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&hook)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let config = zapiska::config::Config {
        admin_token: "test".to_string(),
        database_path: dir.path().join("f1.db").to_string_lossy().to_string(),
        moderation_webhook_url: Some(format!("{}/hook", hook.uri())),
        webhook_signing_secret: Some("s3cr3t".to_string()),
        ..zapiska::config::Config::default()
    };
    // Same assembly the production spawn rides (plus the real worker,
    // idle here — no jobs are sent to it).
    let state = zapiska::state::AppState::start_with_github(config.clone(), Arc::new(StubGitHub))
        .expect("f1 start");

    // Seed: a live approved mention with an alive ledger row.
    let source = "https://src.example/f1-gone";
    let target = "https://nithitsuki.com/blog/f1-gone";
    let id = state
        .repo
        .insert_comment(zapiska::db::repo::NewComment {
            target_path: "/blog/f1-gone".to_string(),
            comment_type: "webmention".to_string(),
            source_url: Some(source.to_string()),
            author_name: "Remote Author".to_string(),
            author_url: None,
            author_avatar: None,
            content: "Fading post".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
        })
        .await
        .unwrap();
    state.repo.update_status(id, "approved").await.unwrap();
    state
        .repo
        .upsert_webmention_seen(zapiska::db::repo::NewWebmentionSeen {
            source: source.to_string(),
            target: target.to_string(),
            last_status: "alive".to_string(),
        })
        .await
        .unwrap();

    // The production sink builder App::start feeds the worker spawn.
    let sink = config
        .worker_moderation_sink(&state.http_client)
        .expect("sink configured");
    let proc = processor(
        state.repo.clone(),
        CannedFetcher::with_gone(source),
        state.http_client.clone(),
        state.notifier.clone(),
        Some(Arc::new(sink)),
    );
    proc.process(&WebmentionJob {
        source: source.to_string(),
        target: target.to_string(),
    })
    .await
    .unwrap();

    let reqs = poll_telegram_requests(&hook, 1).await;
    assert_eq!(reqs.len(), 1, "gone-delete must emit exactly once");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["event"], "comment.status_changed");
    assert_eq!(body["old_status"], "approved");
    assert_eq!(body["new_status"], "deleted");
    let ts = reqs[0]
        .headers
        .get(zapiska::http::webhook::TIMESTAMP_HEADER)
        .expect("timestamp header proves the shared signed sink");
    let sig = reqs[0]
        .headers
        .get(zapiska::http::webhook::SIGNATURE_HEADER)
        .expect("signature header proves the shared signed sink");
    assert!(
        zapiska::http::webhook::verify_body(
            "s3cr3t",
            Some(ts.to_str().unwrap()),
            Some(sig.to_str().unwrap()),
            &reqs[0].body,
            zapiska::http::webhook::timestamp_now()
        ),
        "emission must verify under the configured secret"
    );
    assert_eq!(
        state.repo.get_comment(id).await.unwrap().unwrap().status,
        "deleted"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        hook.received_requests().await.unwrap_or_default().len(),
        1,
        "nothing else may emit to the webhook URL"
    );
}
