use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use reqwest::Client;

use crate::config::Config;
use crate::db::pool::SqlitePool;
use crate::db::repo::CommentsRepo;
use crate::github::GitHubLookup;
#[cfg(feature = "webmentions")]
use crate::worker::JobSender;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub pool: SqlitePool,
    pub repo: CommentsRepo,
    pub github: Arc<dyn GitHubLookup>,
    #[cfg(feature = "webmentions")]
    pub wm_sender: JobSender,
    /// Shared HTTP client (used for all outbound requests: GitHub enrichment,
    /// webmention fetches, and moderation webhooks).
    pub http_client: Client,
    /// In-memory rate limiter for per-IP daily caps and per-domain hourly caps.
    pub limiter: Arc<Limiter>,
}

/// Simple in-memory rate limiter keyed by `(prefix, period_key)`.
/// Period keys are date strings (YYYY-MM-DD) or hour strings (YYYY-MM-DD-HH).
/// Old period keys naturally expire — no one queries yesterday's date for today's limit.
/// The HashMap is cleaned up opportunistically when a new entry bumps into a stale key.
#[derive(Debug)]
pub struct Limiter {
    counts: Mutex<HashMap<String, u32>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Check and increment the count for a given key.
    /// Returns `true` if the operation is within the limit, `false` if it should be rejected.
    pub fn check_and_increment(&self, key: &str, limit: u32) -> bool {
        if limit == 0 {
            return true; // unlimited
        }
        let mut counts = self.counts.lock().expect("limiter lock");
        let count = counts.get(key).copied().unwrap_or(0);
        if count >= limit {
            return false;
        }
        counts.insert(key.to_string(), count + 1);

        // Opportunistic cleanup: if the map is over 10k entries, sweep stale keys.
        // A key is "stale" if its embedded date is older than yesterday.
        if counts.len() > 10_000 {
            let today = date_key();
            let yesterday = yesterday_key();
            counts.retain(|k, _| {
                // Keep keys that contain today's or yesterday's date prefix.
                // Keys look like "ip:2026-07-05" or "domain:2026-07-05-14".
                k.contains(&today) || k.contains(&yesterday)
            });
        }

        true
    }
}

/// Returns today's date as a key fragment: "2026-07-05".
fn date_key() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Days since epoch
    let days = secs / 86400;
    // Compute year/month/day from days since epoch (simple algorithm, good enough)
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn yesterday_key() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let (y, m, d) = days_to_ymd(days.saturating_sub(1));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Build a limiter key for per-IP-per-day tracking.
pub fn ip_daily_key(ip: &std::net::IpAddr) -> String {
    format!("ip:{ip}:{}", date_key())
}

/// Build a limiter key for per-domain-per-hour tracking.
pub fn domain_hourly_key(domain: &str) -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let hours = dur.as_secs() / 3600;
    let days = hours / 24;
    let (y, m, d) = days_to_ymd(days);
    let h = hours % 24;
    format!("dom:{domain}:{y:04}-{m:02}-{d:02}-{h:02}")
}
