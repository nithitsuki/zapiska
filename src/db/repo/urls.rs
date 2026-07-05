use rusqlite::params;
use serde::Serialize;

use super::{CommentsRepo, RepoError, RepoResult};

/// An extracted URL from a comment.
#[derive(Debug, Clone, Serialize)]
pub struct CommentUrl {
    pub id: i64,
    pub comment_id: i64,
    pub url: String,
    pub domain: String,
    pub url_hash: String,
}

/// Stats for a URL or domain across the site.
#[derive(Debug, Clone, Serialize)]
pub struct UrlStats {
    pub url: String,
    pub domain: String,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub total_occurrences: i64,
    pub unique_ips: i64,
    pub unique_author_names: Vec<String>,
    pub comments: Vec<UrlCommentRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UrlCommentRef {
    pub id: i64,
    pub status: String,
    pub target_path: String,
    pub created_at: String,
}

impl CommentsRepo {
    /// Insert extracted URLs for a comment.
    pub async fn insert_urls(
        &self,
        comment_id: i64,
        urls: Vec<(String, String, String)>,
    ) -> RepoResult<()> {
        self.spawn(move |conn| {
            for (url, domain, url_hash) in &urls {
                conn.execute(
                    "INSERT INTO comment_urls (comment_id, url, domain, url_hash) VALUES (?1, ?2, ?3, ?4)",
                    params![comment_id, url, domain, url_hash],
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            }
            Ok(())
        })
        .await
    }

    /// Get all URLs for a specific comment.
    pub async fn get_comment_urls(&self, comment_id: i64) -> RepoResult<Vec<CommentUrl>> {
        self.spawn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT id, comment_id, url, domain, url_hash FROM comment_urls WHERE comment_id = ?1 ORDER BY id")
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            let rows = stmt
                .query_map(params![comment_id], |row| {
                    Ok(CommentUrl {
                        id: row.get(0)?,
                        comment_id: row.get(1)?,
                        url: row.get(2)?,
                        domain: row.get(3)?,
                        url_hash: row.get(4)?,
                    })
                })
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row.map_err(|e| RepoError::Internal(e.to_string()))?);
            }
            Ok(result)
        })
        .await
    }

    /// Look up all comments containing a specific normalized URL.
    pub async fn lookup_url(&self, url_hash: &str) -> RepoResult<UrlStats> {
        let url_hash = url_hash.to_string();
        self.spawn(move |conn| {
            let (url, domain): (String, String) = conn
                .query_row(
                    "SELECT url, domain FROM comment_urls WHERE url_hash = ?1 LIMIT 1",
                    params![url_hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| RepoError::NotFound(format!("url hash {url_hash} not found")))?;

            let total_occurrences: i64 = conn
                .query_row(
                    "SELECT count(*) FROM comment_urls WHERE url_hash = ?1",
                    params![url_hash],
                    |row| row.get(0),
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;

            let first_seen: Option<String> = conn
                .query_row(
                    "SELECT MIN(c.created_at) FROM comment_urls cu JOIN comments c ON c.id = cu.comment_id WHERE cu.url_hash = ?1",
                    params![url_hash],
                    |row| row.get(0),
                )
                .ok();

            let last_seen: Option<String> = conn
                .query_row(
                    "SELECT MAX(c.created_at) FROM comment_urls cu JOIN comments c ON c.id = cu.comment_id WHERE cu.url_hash = ?1",
                    params![url_hash],
                    |row| row.get(0),
                )
                .ok();

            let unique_ips: i64 = conn
                .query_row(
                    "SELECT count(DISTINCT c.submitter_ip) FROM comment_urls cu JOIN comments c ON c.id = cu.comment_id WHERE cu.url_hash = ?1 AND c.submitter_ip IS NOT NULL",
                    params![url_hash],
                    |row| row.get(0),
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;

            let mut stmt = conn
                .prepare("SELECT DISTINCT c.author_name FROM comment_urls cu JOIN comments c ON c.id = cu.comment_id WHERE cu.url_hash = ?1")
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            let unique_author_names: Vec<String> = stmt
                .query_map(params![url_hash], |row| row.get(0))
                .map_err(|e| RepoError::Internal(e.to_string()))?
                .filter_map(|r| r.ok())
                .collect();

            let mut stmt2 = conn
                .prepare("SELECT c.id, c.status, c.target_path, c.created_at FROM comment_urls cu JOIN comments c ON c.id = cu.comment_id WHERE cu.url_hash = ?1 ORDER BY c.created_at DESC LIMIT 50")
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            let comments: Vec<UrlCommentRef> = stmt2
                .query_map(params![url_hash], |row| {
                    Ok(UrlCommentRef {
                        id: row.get(0)?,
                        status: row.get(1)?,
                        target_path: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                })
                .map_err(|e| RepoError::Internal(e.to_string()))?
                .filter_map(|r| r.ok())
                .collect();

            Ok(UrlStats {
                url,
                domain,
                first_seen,
                last_seen,
                total_occurrences,
                unique_ips,
                unique_author_names,
                comments,
            })
        })
        .await
    }

    /// Look up all URLs from a domain, returning per-URL summaries.
    pub async fn lookup_domain(&self, domain: &str) -> RepoResult<Vec<String>> {
        let domain = domain.to_string();
        self.spawn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT url_hash FROM comment_urls WHERE domain = ?1 ORDER BY url",
                )
                .map_err(|e| RepoError::Internal(e.to_string()))?;
            let hashes: Vec<String> = stmt
                .query_map(params![domain], |row| row.get(0))
                .map_err(|e| RepoError::Internal(e.to_string()))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(hashes)
        })
        .await
    }
}
