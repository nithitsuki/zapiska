use rusqlite::OptionalExtension;
use rusqlite::params;

use super::{CommentsRepo, GithubProfile, NewGithubProfile, RepoError, RepoResult};

impl CommentsRepo {
    pub async fn get_github_profile(&self, login: &str) -> RepoResult<Option<GithubProfile>> {
        let login = login.to_string();
        self.spawn(move |conn| {
            conn.query_row(
                "SELECT login, name, avatar_url, cached_at, valid
                 FROM github_profiles WHERE login = ?1",
                params![login],
                |row| {
                    let valid_int: i64 = row.get(4)?;
                    Ok(GithubProfile {
                        login: row.get(0)?,
                        name: row.get(1)?,
                        avatar_url: row.get(2)?,
                        cached_at: row.get(3)?,
                        valid: valid_int != 0,
                    })
                },
            )
            .optional()
            .map_err(|e| RepoError::Internal(e.to_string()))
        })
        .await
    }

    pub async fn upsert_github_profile(&self, input: NewGithubProfile) -> RepoResult<()> {
        self.spawn(move |conn| {
            conn.execute(
                "INSERT INTO github_profiles (login, name, avatar_url, valid)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(login) DO UPDATE SET
                     name = excluded.name,
                     avatar_url = excluded.avatar_url,
                     cached_at = datetime('now'),
                     valid = excluded.valid",
                params![
                    input.login,
                    input.name,
                    input.avatar_url,
                    input.valid as i64
                ],
            )
            .map_err(|e| RepoError::Internal(e.to_string()))?;
            Ok(())
        })
        .await
    }
}
