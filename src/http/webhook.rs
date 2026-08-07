//! Fire-and-forget webhook delivery, shared by comment and reaction events.

/// POST a JSON payload to `url` on a spawned task. Failures are logged at
/// `warn` and never affect the caller.
pub(crate) fn fire(
    client: &reqwest::Client,
    url: &str,
    payload: serde_json::Value,
    timeout_secs: u64,
) {
    let client = client.clone();
    let url = url.to_string();
    tokio::spawn(async move {
        match client
            .post(&url)
            .json(&payload)
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .send()
            .await
        {
            Ok(r) => {
                tracing::debug!(webhook = %url, status = %r.status(), "webhook sent")
            }
            Err(e) => tracing::warn!(webhook = %url, err = %e, "webhook failed"),
        }
    });
}
