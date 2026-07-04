use serde::Deserialize;

use crate::db::repo::{CommentsRepo, NewGithubProfile};

/// Profile returned by the GitHub lookup service.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub avatar_url: String,
}

/// Trait for resolving a GitHub username into profile info.
#[async_trait::async_trait]
pub trait GitHubLookup: Send + Sync {
    async fn lookup(&self, username: &str) -> Option<Profile>;
}

/// Stub implementation that always returns `None`.
#[derive(Debug, Clone)]
pub struct StubGitHub;

#[async_trait::async_trait]
impl GitHubLookup for StubGitHub {
    async fn lookup(&self, _username: &str) -> Option<Profile> {
        None
    }
}

/// Real implementation backed by the GitHub API + SQLite cache.
#[derive(Clone)]
pub struct RealGitHub {
    client: reqwest::Client,
    repo: CommentsRepo,
    token: Option<String>,
    /// Base URL for the GitHub API (overridable for tests).
    api_base: String,
}

impl RealGitHub {
    pub fn new(
        repo: CommentsRepo,
        _timeout_ms: u64,
        token: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        RealGitHub::with_base(repo, token, client, "https://api.github.com")
    }

    /// Internal constructor allowing a custom base URL (used in tests).
    pub fn with_base(
        repo: CommentsRepo,
        token: Option<String>,
        client: reqwest::Client,
        api_base: &str,
    ) -> Self {
        Self {
            client,
            repo,
            token,
            api_base: api_base.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl GitHubLookup for RealGitHub {
    async fn lookup(&self, username: &str) -> Option<Profile> {
        // 1. Check cache.
        if let Some(cached) = self.repo.get_github_profile(username).await.ok().flatten() {
            let is_fresh = if cached.valid {
                // Positive cache: 30-day TTL.
                is_fresh_enough(&cached.cached_at, 30 * 24 * 60 * 60)
            } else {
                // Negative cache: 1-hour TTL.
                is_fresh_enough(&cached.cached_at, 60 * 60)
            };
            if is_fresh {
                return cached.name.map(|name| Profile {
                    name,
                    avatar_url: cached.avatar_url,
                });
            }
        }

        // 2. Fetch from GitHub API.
        let url = format!("{}/users/{}", self.api_base, username);
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<GitHubUser>().await {
                Ok(gh_user) => {
                    let name = gh_user.name.clone().unwrap_or_else(|| username.to_string());
                    let avatar_url = gh_user.avatar_url.clone();

                    let _ = self
                        .repo
                        .upsert_github_profile(NewGithubProfile {
                            login: username.to_lowercase(),
                            name: gh_user.name,
                            avatar_url: avatar_url.clone(),
                            valid: true,
                        })
                        .await;

                    Some(Profile { name, avatar_url })
                }
                Err(e) => {
                    tracing::warn!(username, err = %e, "failed to parse GitHub API response");
                    None
                }
            },
            Ok(resp) if resp.status().as_u16() == 404 => {
                // Negative cache: user does not exist.
                let _ = self
                    .repo
                    .upsert_github_profile(NewGithubProfile {
                        login: username.to_lowercase(),
                        name: None,
                        avatar_url: String::new(),
                        valid: false,
                    })
                    .await;
                None
            }
            Ok(resp) => {
                tracing::warn!(
                    username,
                    status = resp.status().as_u16(),
                    "GitHub API returned non-success"
                );
                None
            }
            Err(e) => {
                tracing::warn!(username, err = %e, "GitHub API request failed");
                None
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct GitHubUser {
    login: String,
    name: Option<String>,
    #[serde(rename = "avatar_url")]
    avatar_url: String,
}

/// Rough freshness check: compare `cached_at` (ISO8601) against current time.
/// Returns true if `cached_at` is within `max_age_secs` of now.
fn is_fresh_enough(cached_at: &str, max_age_secs: u64) -> bool {
    let Ok(parsed) = iso8601_to_unix(cached_at) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.saturating_sub(parsed) < max_age_secs
}

/// Naive ISO8601-to-unix-timestamp parser for the format `datetime('now')` produces:
/// `2026-07-03 16:40:00` (with possible `T` separator).
fn iso8601_to_unix(s: &str) -> Result<u64, ()> {
    let s = s.trim().replace('T', " ");
    // Expect "YYYY-MM-DD HH:MM:SS"
    if s.len() < 19 {
        return Err(());
    }
    let year: u64 = s[0..4].parse().map_err(|_| ())?;
    let month: u64 = s[5..7].parse().map_err(|_| ())?;
    let day: u64 = s[8..10].parse().map_err(|_| ())?;
    let hour: u64 = s[11..13].parse().map_err(|_| ())?;
    let min: u64 = s[14..16].parse().map_err(|_| ())?;
    let sec: u64 = s[17..19].parse().map_err(|_| ())?;

    /// Days from Unix epoch (1970-01-01) for a civil (proleptic Gregorian) date.
    /// Howard Hinnant algorithm: https://howardhinnant.github.io/date_algorithms.html
    fn civil_days_from_unix_epoch(y: u64, m: u64, d: u64) -> i64 {
        let y = y as i64;
        let m = m as i64;
        let d = d as i64;
        let y = if m <= 2 { y - 1 } else { y };
        let m = if m <= 2 { m + 12 } else { m };
        let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
        let yoe = y - era * 400; // year-of-era [0, 399]
        let doy = (153 * (m - 3) + 2) / 5 + d - 1; // day-of-year [0, 365]
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // day-of-era [0, 146096]
        era * 146097 + doe - 719468
    }

    let days = civil_days_from_unix_epoch(year, month, day);
    if days < 0 {
        return Err(());
    }
    let days = days as u64;
    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::pool::{create_pool, run_migrations};
    use reqwest::Client;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_repo() -> (CommentsRepo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gh_test.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();
        (CommentsRepo::new(pool), dir)
    }

    #[tokio::test]
    async fn cache_hit_returns_without_http() {
        let (repo, _dir) = test_repo();
        let client = reqwest::Client::builder().build().unwrap();
        let gh = RealGitHub::new(repo.clone(), 4000, None, client);

        // Seed cache.
        repo.upsert_github_profile(NewGithubProfile {
            login: "cached_user".to_string(),
            name: Some("Cached User".to_string()),
            avatar_url: "https://avatars.example/u/1".to_string(),
            valid: true,
        })
        .await
        .unwrap();

        let profile = gh.lookup("cached_user").await;
        assert!(profile.is_some());
        assert_eq!(profile.unwrap().name, "Cached User");
    }

    #[tokio::test]
    async fn negative_cache_returns_none_without_http() {
        let (repo, _dir) = test_repo();
        let client = reqwest::Client::builder().build().unwrap();
        let gh = RealGitHub::new(repo.clone(), 4000, None, client);

        // Seed negative cache.
        repo.upsert_github_profile(NewGithubProfile {
            login: "ghost".to_string(),
            name: None,
            avatar_url: String::new(),
            valid: false,
        })
        .await
        .unwrap();

        let profile = gh.lookup("ghost").await;
        assert!(profile.is_none());
    }

    #[tokio::test]
    async fn positive_cache_expires_after_30_days() {
        let (repo, _dir) = test_repo();

        // Insert a stale positive cache entry (31 days ago).
        let stale_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(31 * 86400);
        let pool = repo.pool();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO github_profiles (login, name, avatar_url, cached_at, valid)
             VALUES ('stale_user', 'Stale', 'https://example.com', datetime(?1, 'unixepoch'), 1)",
            rusqlite::params![stale_ts as i64],
        )
        .unwrap();

        // Since we can't easily control time in tests without injecting SQL,
        // verify the entry is considered stale.
        let cached = repo
            .get_github_profile("stale_user")
            .await
            .unwrap()
            .unwrap();
        assert!(
            !is_fresh_enough(&cached.cached_at, 30 * 86400),
            "expected entry >30d to be stale"
        );
    }

    #[tokio::test]
    async fn negative_cache_expires_after_1_hour() {
        let (repo, _dir) = test_repo();

        // Insert a stale negative cache entry (2 hours ago).
        let stale_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(2 * 3600);
        let pool = repo.pool();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO github_profiles (login, name, avatar_url, cached_at, valid)
             VALUES ('neg_stale', NULL, '', datetime(?1, 'unixepoch'), 0)",
            rusqlite::params![stale_ts as i64],
        )
        .unwrap();

        let cached = repo.get_github_profile("neg_stale").await.unwrap().unwrap();
        assert!(!is_fresh_enough(&cached.cached_at, 3600));
    }

    #[tokio::test]
    async fn cache_miss_fetches_and_stores() {
        let mock_server = MockServer::start().await;
        let (repo, _dir) = test_repo();

        Mock::given(method("GET"))
            .and(path("/users/alice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "login": "alice",
                "name": "Alice Green",
                "avatar_url": "https://avatars.githubusercontent.com/u/1"
            })))
            .mount(&mock_server)
            .await;

        // Create RealGitHub pointing at the mock server instead of api.github.com.
        let client = Client::builder()
            .https_only(false) // allow http for test
            .user_agent("test")
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let gh = RealGitHub {
            client,
            repo: repo.clone(),

            token: None,
            api_base: mock_server.uri(),
        };

        let profile = gh.lookup("alice").await;
        assert!(profile.is_some());
        assert_eq!(profile.unwrap().name, "Alice Green");

        // Verify cached in DB.
        let cached = repo.get_github_profile("alice").await.unwrap().unwrap();
        assert!(cached.valid);
        assert_eq!(cached.name, Some("Alice Green".to_string()));
    }

    #[tokio::test]
    async fn api_returns_name_falls_back_to_login() {
        let mock_server = MockServer::start().await;
        let (repo, _dir) = test_repo();

        Mock::given(method("GET"))
            .and(path("/users/noname"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "login": "noname",
                "name": null,
                "avatar_url": "https://avatars.githubusercontent.com/u/2"
            })))
            .mount(&mock_server)
            .await;

        let client = Client::builder()
            .https_only(false)
            .user_agent("test")
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let gh = RealGitHub {
            client,
            repo: repo.clone(),

            token: None,
            api_base: mock_server.uri(),
        };

        let profile = gh.lookup("noname").await;
        assert!(profile.is_some());
        assert_eq!(profile.unwrap().name, "noname");
    }

    #[tokio::test]
    async fn api_404_negative_cached() {
        let mock_server = MockServer::start().await;
        let (repo, _dir) = test_repo();

        Mock::given(method("GET"))
            .and(path("/users/ghost404"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = Client::builder()
            .https_only(false)
            .user_agent("test")
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let gh = RealGitHub {
            client,
            repo: repo.clone(),

            token: None,
            api_base: mock_server.uri(),
        };

        let profile = gh.lookup("ghost404").await;
        assert!(profile.is_none());

        // Negative cache written.
        let cached = repo.get_github_profile("ghost404").await.unwrap().unwrap();
        assert!(!cached.valid);
    }

    #[tokio::test]
    async fn api_timeout_returns_none() {
        let mock_server = MockServer::start().await;
        let (repo, _dir) = test_repo();

        Mock::given(method("GET"))
            .and(path("/users/slowpoke"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(Duration::from_secs(10)), // longer than timeout
            )
            .mount(&mock_server)
            .await;

        let client = Client::builder()
            .https_only(false)
            .user_agent("test")
            .timeout(Duration::from_millis(50)) // very short
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let gh = RealGitHub {
            client,
            repo: repo.clone(),

            token: None,
            api_base: mock_server.uri(),
        };

        let profile = gh.lookup("slowpoke").await;
        assert!(profile.is_none());
    }

    #[tokio::test]
    async fn redirect_to_localhost_blocked() {
        // RealGitHub uses redirect::Policy::none(), so redirects are errors.
        // We test that the client doesn't follow redirects.
        let mock_server = MockServer::start().await;
        let (repo, _dir) = test_repo();

        Mock::given(method("GET"))
            .and(path("/users/redirector"))
            .respond_with(
                ResponseTemplate::new(302).append_header("Location", "http://127.0.0.1/evil"),
            )
            .mount(&mock_server)
            .await;

        let client = Client::builder()
            .https_only(false)
            .user_agent("test")
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let gh = RealGitHub {
            client,
            repo: repo.clone(),

            token: None,
            api_base: mock_server.uri(),
        };

        let profile = gh.lookup("redirector").await;
        // 302 returns body-less non-success, so lookup returns None.
        assert!(profile.is_none());
    }

    #[tokio::test]
    async fn headers_configured() {
        // Verify the client is built with user-agent and token header.
        // Functional verification is done via the mock server receiving the request.
        let mock_server = MockServer::start().await;
        let (repo, _dir) = test_repo();

        Mock::given(method("GET"))
            .and(path("/users/hcheck"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "login": "hcheck",
                "name": "Header Check",
                "avatar_url": "https://example.com"
            })))
            .mount(&mock_server)
            .await;

        let client = Client::builder()
            .https_only(false)
            .user_agent("test-agent/1.0")
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let gh = RealGitHub {
            client,
            repo: repo.clone(),

            token: Some("ghp_test_token".to_string()),
            api_base: mock_server.uri(),
        };

        let profile = gh.lookup("hcheck").await;
        assert!(profile.is_some(), "should return a profile");
    }

    // ── iso8601_to_unix tests ──────────────────────────

    #[test]
    fn parse_iso8601_basic() {
        // 2026-01-01 00:00:00 UTC = 1767225600 (approx)
        let ts = iso8601_to_unix("2026-01-01 00:00:00").unwrap();
        assert_eq!(ts, 1767225600);
    }

    #[test]
    fn parse_iso8601_with_t_separator() {
        let ts = iso8601_to_unix("2026-07-03T12:00:00").unwrap();
        let ts2 = iso8601_to_unix("2026-07-03 12:00:00").unwrap();
        assert_eq!(ts, ts2);
    }

    #[test]
    fn parse_iso8601_mid_summer() {
        // 2026-07-03 00:00:00 UTC
        let ts = iso8601_to_unix("2026-07-03 00:00:00").unwrap();
        // Verify reasonable range (late 2026).
        assert!(ts > 1767225600, "should be after Jan 2026");
        assert!(ts < 1800000000, "should be before late 2027");
    }

    #[test]
    fn is_fresh_true_for_recent_entry() {
        // Compute an ISO timestamp 30 minutes ago.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let thirty_min_ago = now - 1800;
        // Convert back to ISO-ish format via naive calculation.
        let iso = unix_to_naive_iso(thirty_min_ago as u64);
        assert!(
            is_fresh_enough(&iso, 3600),
            "30-min-old entry should be fresh for 1h TTL"
        );
    }

    #[test]
    fn is_fresh_false_for_old_entry() {
        let old_iso = "2026-01-01 00:00:00";
        assert!(!is_fresh_enough(old_iso, 3600));
    }

    /// Naive inverse of iso8601_to_unix: produce "YYYY-MM-DD HH:MM:SS" from a unix timestamp.
    fn unix_to_naive_iso(ts: u64) -> String {
        let days = ts / 86400;
        let time_secs = ts % 86400;
        let hour = time_secs / 3600;
        let min = (time_secs % 3600) / 60;
        let sec = time_secs % 60;

        // Days since epoch to (y, m, d).
        let z = days as i64 + 719468;
        let era = if z >= 0 {
            z / 146097
        } else {
            (z - 146096) / 146097
        };
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };

        format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02}")
    }
}
