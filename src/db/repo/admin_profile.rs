//! Verified owner profile: the site owner's public identity, used to render
//! the verification checkmark and to author verified comments. A single
//! singleton row (`id = 1`); empty fields mean "not set".

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{Repo, RepoError, RepoResult};

/// The site owner's public profile. Presence booleans on the wire are
/// derived from non-empty fields; no secrets live here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminProfile {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub github_username: String,
    #[serde(default)]
    pub website_url: String,
    #[serde(default)]
    pub avatar_url: String,
}

const PROFILE_COLUMNS: &str = "display_name, github_username, website_url, avatar_url";

fn row_to_profile(row: &rusqlite::Row) -> rusqlite::Result<AdminProfile> {
    Ok(AdminProfile {
        display_name: row.get(0)?,
        github_username: row.get(1)?,
        website_url: row.get(2)?,
        avatar_url: row.get(3)?,
    })
}

impl Repo {
    /// The current owner profile. Returns a default (all empty) when the
    /// singleton row has not been set — a fresh deploy has no owner profile
    /// until the operator fills it in.
    pub async fn get_admin_profile(&self) -> RepoResult<AdminProfile> {
        self.spawn(move |conn| {
            conn.query_row(
                &format!("SELECT {PROFILE_COLUMNS} FROM admin_profile WHERE id = 1"),
                [],
                row_to_profile,
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(AdminProfile::default()),
                other => Err(RepoError::from(other)),
            })
        })
        .await
    }

    /// Upsert the owner profile (singleton row id = 1).
    pub async fn set_admin_profile(&self, profile: AdminProfile) -> RepoResult<()> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO admin_profile (id, display_name, github_username, website_url, avatar_url)
                 VALUES (1, ?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                     display_name = excluded.display_name,
                     github_username = excluded.github_username,
                     website_url = excluded.website_url,
                     avatar_url = excluded.avatar_url",
                params![
                    profile.display_name,
                    profile.github_username,
                    profile.website_url,
                    profile.avatar_url,
                ],
            )
            .map_err(RepoError::from)?;
            Ok(())
        })
        .await
    }
}
