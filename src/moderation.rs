//! Moderation status machine: one owner for validity, effects, and the
//! webhook sink.
//!
//! Placement: this lives at the crate root (`crate::moderation`) rather than
//! inside `http::admin::moderate` because both the protected admin handlers
//! *and* the public submission handlers (`comment_post`, `reactions`) depend
//! on it — putting it under `admin` would invert that dependency and force
//! public routes to import from the admin surface.

use std::str::FromStr;

use crate::db::repo::Repo;
use crate::error::AppError;

// ── Status ────────────────────────────────────────────────────

/// The lifecycle state of a comment or reaction. Only `Approved` content is
/// public. Stored in SQLite as text (`pending`/`approved`/`spam`/`deleted`,
/// enforced by a `CHECK`) — no migration: the enum is the boundary type and
/// `as_str()` is the only thing that reaches storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Pending,
    Approved,
    Spam,
    Deleted,
}

impl Status {
    /// The storage/wire spelling. The single source of truth — every SQL
    /// write and webhook payload goes through this, never a literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Spam => "spam",
            Self::Deleted => "deleted",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Rejects anything outside the four statuses with the exact historical
/// admin error text, so the five hand-written `matches!(action, …)`
/// whitelists collapse into one parse with no message drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidStatus(pub String);

impl std::fmt::Display for InvalidStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid action '{}', must be one of: approved, spam, deleted, pending",
            self.0
        )
    }
}

impl std::error::Error for InvalidStatus {}

impl FromStr for Status {
    type Err = InvalidStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "spam" => Ok(Self::Spam),
            "deleted" => Ok(Self::Deleted),
            other => Err(InvalidStatus(other.to_string())),
        }
    }
}

// ── Actor ─────────────────────────────────────────────────────

/// Who performs a transition. Decides validity (see [`is_valid_transition`])
/// and fills the `changed_by` webhook field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// Admin API caller (single + batch, comments + reactions).
    Admin,
    /// External moderation service answering a sync `*.created` webhook.
    Webhook,
    /// Comment owner presenting the delete token.
    SelfDelete,
}

impl Actor {
    /// The `changed_by` spelling. Kept byte-identical to the historical
    /// `comment.status_changed` payloads (`admin`); `webhook`/`self` are new
    /// values for paths that never emitted before.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Webhook => "webhook",
            Self::SelfDelete => "self",
        }
    }
}

// ── Validity ──────────────────────────────────────────────────

/// Transition validity table. `deleted` is terminal except via admin
/// re-approve: only [`Actor::Admin`] may leave `deleted` (reviving a
/// self-deleted row also clears its delete token — B10). The webhook decides
/// live content only and can never revive. Owners may only delete live
/// content, never revive or re-queue. Same-status is always allowed and is a
/// no-op (no write, no event).
///
/// | from \ to            | pending | approved | spam | deleted |
/// |----------------------|---------|----------|------|---------|
/// | pending (admin)      | noop    | ok       | ok   | ok      |
/// | approved (admin)     | ok      | noop     | ok   | ok      |
/// | spam (admin)         | ok      | ok       | noop | ok      |
/// | deleted (admin)      | ok*     | ok*      | ok*  | noop    |
/// | any live (webhook)   | ok      | ok       | ok   | ok      |
/// | deleted (webhook)    | REJECT  | REJECT   | REJECT | noop  |
/// | live → deleted (self)| —      | —        | —    | ok      |
/// | self any other move  | REJECT  | REJECT   | REJECT | —     |
///
/// `*` clears the delete token in the same transaction.
#[must_use]
pub fn is_valid_transition(from: Status, to: Status, actor: Actor) -> bool {
    if from == to {
        return true;
    }
    match actor {
        Actor::Admin => true,
        Actor::Webhook => from != Status::Deleted,
        Actor::SelfDelete => to == Status::Deleted && from != Status::Deleted,
    }
}

fn check_transition(from: Status, to: Status, actor: Actor) -> Result<(), AppError> {
    if is_valid_transition(from, to, actor) {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "invalid transition from '{from}' to '{to}' for '{}'",
        actor.as_str()
    )))
}

// ── Payloads ──────────────────────────────────────────────────

/// `comment.status_changed` event. Schema is byte-identical to the
/// historical single-moderate payload so existing engines keep parsing.
#[must_use]
pub fn comment_status_payload(
    id: i64,
    old: Status,
    new: Status,
    changed_by: Actor,
) -> serde_json::Value {
    serde_json::json!({
        "event": "comment.status_changed",
        "id": id,
        "old_status": old.as_str(),
        "new_status": new.as_str(),
        "changed_by": changed_by.as_str(),
    })
}

/// `reaction.status_changed` event. Schema is byte-identical to the
/// historical reaction-moderate payload.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn reaction_status_payload(
    id: i64,
    comment_id: i64,
    reaction: &str,
    old: Status,
    new: Status,
    changed_by: Actor,
) -> serde_json::Value {
    serde_json::json!({
        "event": "reaction.status_changed",
        "id": id,
        "comment_id": comment_id,
        "reaction": reaction,
        "old_status": old.as_str(),
        "new_status": new.as_str(),
        "changed_by": changed_by.as_str(),
    })
}

/// Fields for the enriched `comment.created` event. Carries every field the
/// historical `comment_post.rs` payload built (including `delete_token` and
/// the raw `submitter_ip` — auth/payload shape is intentionally unchanged;
/// signing is a separate small-win batch).
pub struct CommentCreated<'a> {
    pub id: i64,
    pub target_path: &'a str,
    pub comment_type: &'a str,
    pub author_name: &'a str,
    pub author_url: Option<&'a str>,
    pub author_avatar: Option<&'a str>,
    pub content: &'a str,
    pub honeypot: bool,
    pub parent_id: Option<i64>,
    pub depth: i64,
    pub submitter_ip: Option<&'a str>,
    pub delete_token: &'a str,
    pub content_hash: Option<&'a str>,
    pub is_reply: bool,
    /// `None` for a top-level comment (serializes as `null`, as before).
    pub parents: Option<Vec<serde_json::Value>>,
    pub submitter_total: i64,
    pub submitter_approved: i64,
    pub submitter_spam: i64,
    pub submitter_pending: i64,
    pub submitter_deleted: i64,
    pub submitter_first_seen: Option<&'a str>,
}

/// Build the `comment.created` payload. One builder replaces the hand-built
/// block in `comment_post.rs`.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn comment_created_payload(c: &CommentCreated<'_>, admin_url: &str) -> serde_json::Value {
    serde_json::json!({
        "event": "comment.created",
        "id": c.id, "target_path": c.target_path, "comment_type": c.comment_type,
        "author_name": c.author_name, "author_url": c.author_url, "author_avatar": c.author_avatar,
        "content": c.content,
        "honeypot": c.honeypot, "parent_id": c.parent_id, "depth": c.depth,
        "submitter_ip": c.submitter_ip, "delete_token": c.delete_token,
        "content_hash": c.content_hash, "is_reply": c.is_reply,
        "parents": c.parents,
        "submitter": { "ip": c.submitter_ip, "total_comments": c.submitter_total,
            "approved_comments": c.submitter_approved, "spam_comments": c.submitter_spam,
            "pending_comments": c.submitter_pending, "deleted_comments": c.submitter_deleted,
            "first_seen": c.submitter_first_seen },
        "admin_url": admin_url,
    })
}

/// Build the `reaction.created` payload. One builder replaces the hand-built
/// block in `reactions.rs`.
#[must_use]
pub fn reaction_created_payload(
    id: i64,
    comment_id: i64,
    reaction: &str,
    target_path: &str,
    is_admin: bool,
) -> serde_json::Value {
    serde_json::json!({
        "event": "reaction.created",
        "id": id,
        "comment_id": comment_id,
        "reaction": reaction,
        "status": "pending",
        "target_path": target_path,
        "is_admin": is_admin,
        "admin_url": "/api/admin/reactions",
    })
}

// ── Sink ──────────────────────────────────────────────────────

/// The webhook sink seam. `emit` is the async (fire-and-forget) adapter;
/// `decide` is the sync (10 s POST + `action` parse) adapter. Both share the
/// payload builders above, so the two copy-pasted sync loops and four
/// hand-built payload blocks collapse here. Tests use [`CountingSink`].
#[async_trait::async_trait]
pub trait ModerationSink: Send + Sync {
    /// Fire-and-forget delivery. Never fails the caller.
    fn emit(&self, payload: serde_json::Value);
    /// Sync delivery: POST the payload, await a JSON `{"action": …}`
    /// decision, parse it to a [`Status`]. `None` on any failure or any
    /// non-whitelisted action (the caller keeps the current status).
    async fn decide(&self, payload: &serde_json::Value) -> Option<Status>;
}

/// Production sink: delivers to the configured moderation webhook URL.
pub struct WebhookSink {
    client: reqwest::Client,
    url: String,
    timeout_secs: u64,
}

impl WebhookSink {
    #[must_use]
    pub fn new(client: reqwest::Client, url: String, timeout_secs: u64) -> Self {
        Self {
            client,
            url,
            timeout_secs,
        }
    }

    /// Sink for `*.status_changed` events (historical 5 s timeout).
    #[must_use]
    pub fn status_sink(client: &reqwest::Client, url: &str) -> Self {
        Self::new(client.clone(), url.to_string(), 5)
    }

    /// Sink for `*.created` events (historical 10 s timeout).
    #[must_use]
    pub fn created_sink(client: &reqwest::Client, url: &str) -> Self {
        Self::new(client.clone(), url.to_string(), 10)
    }
}

#[async_trait::async_trait]
impl ModerationSink for WebhookSink {
    fn emit(&self, payload: serde_json::Value) {
        crate::http::webhook::fire(&self.client, &self.url, payload, self.timeout_secs);
    }

    async fn decide(&self, payload: &serde_json::Value) -> Option<Status> {
        request_decision(&self.client, &self.url, payload).await
    }
}

/// Shared sync adapter behind [`ModerationSink::decide`]: POST with a 10 s
/// timeout and parse the `action` field. Replaces the copy-pasted loops in
/// `comment_post.rs` and `reactions.rs` (warn-and-keep-status on every
/// failure, exactly as before).
pub async fn request_decision(
    client: &reqwest::Client,
    url: &str,
    payload: &serde_json::Value,
) -> Option<Status> {
    match client
        .post(url)
        .json(payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
            Ok(decision) => decision["action"]
                .as_str()
                .and_then(|a| a.parse::<Status>().ok()),
            Err(_) => None,
        },
        Ok(r) => {
            tracing::warn!(webhook = %url, status = %r.status(), "sync webhook returned error");
            None
        }
        Err(e) => {
            tracing::warn!(webhook = %url, err = %e, "sync webhook failed");
            None
        }
    }
}

/// In-memory sink for tests: counts emissions and records payloads so every
/// transition path can assert exactly-once delivery. `decide` answers the
/// stubbed value.
pub struct CountingSink {
    events: std::sync::Mutex<Vec<serde_json::Value>>,
    decision: std::sync::Mutex<Option<Status>>,
}

impl CountingSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
            decision: std::sync::Mutex::new(None),
        }
    }

    /// Number of emitted events.
    #[must_use]
    pub fn count(&self) -> usize {
        self.events.lock().expect("counting sink lock").len()
    }

    /// A snapshot of the emitted payloads.
    #[must_use]
    pub fn payloads(&self) -> Vec<serde_json::Value> {
        self.events.lock().expect("counting sink lock").clone()
    }
}

impl Default for CountingSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ModerationSink for CountingSink {
    fn emit(&self, payload: serde_json::Value) {
        self.events
            .lock()
            .expect("counting sink lock")
            .push(payload);
    }

    async fn decide(&self, _payload: &serde_json::Value) -> Option<Status> {
        *self.decision.lock().expect("counting sink lock")
    }
}

// ── Transitions ───────────────────────────────────────────────

/// What a transition did. `changed == false` means a same-status no-op: no
/// write, no event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionOutcome {
    pub old: Status,
    pub new: Status,
    pub changed: bool,
}

/// The status machine. Owns validity ([`is_valid_transition`]), the status
/// write (via the repo, inside T15's `with_tx` where the write is multi-step
/// — admin revive clearing the delete token), and the sink emission
/// (exactly one event when `changed`, zero otherwise).
pub struct Moderation;

impl Moderation {
    /// Moderate one comment as `actor`. Parses the stored status, enforces
    /// validity, performs the write, and emits one
    /// `comment.status_changed` event when the status actually changed.
    pub async fn transition_comment(
        repo: &Repo,
        sink: Option<&dyn ModerationSink>,
        id: i64,
        to: Status,
        actor: Actor,
    ) -> Result<TransitionOutcome, AppError> {
        let comment = repo
            .get_comment(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("comment {id} not found")))?;
        let old: Status = comment.status.parse().map_err(|_| {
            AppError::Internal(format!(
                "comment {id} has corrupt status '{}'",
                comment.status
            ))
        })?;
        if old == to {
            return Ok(TransitionOutcome {
                old,
                new: to,
                changed: false,
            });
        }
        check_transition(old, to, actor)?;
        // Admin revive out of `deleted` retires the self-delete secret (B10)
        // in the SAME commit as the status write — never a torn
        // approved-with-live-token row.
        let clear_token = old == Status::Deleted && actor == Actor::Admin;
        repo.set_comment_status(id, to.as_str(), clear_token)
            .await?;
        if let Some(sink) = sink {
            sink.emit(comment_status_payload(id, old, to, actor));
        }
        Ok(TransitionOutcome {
            old,
            new: to,
            changed: true,
        })
    }

    /// Moderate one reaction as `actor`. The pending→approved path goes
    /// through T15's [`Repo::approve_reaction_cas`] against the seen emoji so
    /// an emoji swap racing the approval keeps the new emoji pending;
    /// approving a decided (spam/deleted) row is an explicit override and
    /// uses the plain status write, as does every non-approve path. Emits one
    /// `reaction.status_changed` event when the status actually changed.
    ///
    /// `expected_reaction`: the emoji the moderator reviewed. Consulted only
    /// for pending→approved. `None` falls back to the currently stored emoji
    /// (still atomic against a swap in the fetch→write window and serializes
    /// concurrent approves to one winner — the loser re-reads via the 400
    /// below instead of emitting a duplicate event); `Some(stale)` rejects a
    /// stale approval instead of approving sight-unseen.
    pub async fn transition_reaction(
        repo: &Repo,
        sink: Option<&dyn ModerationSink>,
        id: i64,
        to: Status,
        actor: Actor,
        expected_reaction: Option<&str>,
    ) -> Result<TransitionOutcome, AppError> {
        let reaction = repo
            .get_reaction(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("reaction {id} not found")))?;
        let old: Status = reaction.status.parse().map_err(|_| {
            AppError::Internal(format!(
                "reaction {id} has corrupt status '{}'",
                reaction.status
            ))
        })?;
        if old == to {
            return Ok(TransitionOutcome {
                old,
                new: to,
                changed: false,
            });
        }
        check_transition(old, to, actor)?;
        // CAS guards ONLY the pending→approved race. The predicate is
        // `WHERE status='pending' AND reaction=?`, so it can solely match a
        // pending row: routing a spam/deleted approve through it always
        // misses (M1 regression — both are valid explicit overrides of prior
        // moderation and worked under HEAD). An emoji swap always lands back
        // on pending, hence only a pending approve can race one sight-unseen;
        // approving a decided row is a conscious override, so `expected_*`
        // is ignored there and the plain write applies.
        if to == Status::Approved && old == Status::Pending {
            let expected = expected_reaction.unwrap_or(&reaction.reaction);
            let applied = repo.approve_reaction_cas(id, expected).await?;
            if !applied {
                return Err(AppError::BadRequest(
                    "reaction changed during moderation, re-read before approving".to_string(),
                ));
            }
        } else {
            repo.update_reaction_status(id, to.as_str()).await?;
        }
        if let Some(sink) = sink {
            sink.emit(reaction_status_payload(
                id,
                reaction.comment_id,
                &reaction.reaction,
                old,
                to,
                actor,
            ));
        }
        Ok(TransitionOutcome {
            old,
            new: to,
            changed: true,
        })
    }

    /// Self-service delete by token. Returns `false` (no event) when the id
    /// is unknown, the token mismatches, or the row is already deleted —
    /// the caller maps that to 404, exactly as before. On success emits one
    /// `comment.status_changed` event with `changed_by: "self"`.
    pub async fn self_delete_comment(
        repo: &Repo,
        sink: Option<&dyn ModerationSink>,
        id: i64,
        token: &str,
    ) -> Result<bool, AppError> {
        let old: Option<Status> = repo
            .get_comment(id)
            .await?
            .map(|c| {
                c.status.parse().map_err(|_| {
                    AppError::Internal(format!("comment {id} has corrupt status '{}'", c.status))
                })
            })
            .transpose()?;
        let Some(old) = old else {
            return Ok(false);
        };
        if old == Status::Deleted {
            return Ok(false);
        }
        if !repo.delete_by_token(id, token).await? {
            return Ok(false);
        }
        if let Some(sink) = sink {
            sink.emit(comment_status_payload(
                id,
                old,
                Status::Deleted,
                Actor::SelfDelete,
            ));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::pool::{create_pool, run_migrations};
    use crate::db::repo::{NewComment, Repo};

    fn setup_repo() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t16_mod.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        (Repo::new(pool), dir)
    }

    fn native(target: &str, token: Option<&str>) -> NewComment {
        NewComment {
            target_path: target.to_string(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: "T16".to_string(),
            author_url: None,
            author_avatar: None,
            content: "t16 probe".to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: token.map(str::to_string),
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
        }
    }

    async fn seed_comment(repo: &Repo, target: &str, token: Option<&str>) -> i64 {
        repo.insert_comment(native(target, token)).await.unwrap()
    }

    async fn seed_reaction(repo: &Repo, emoji: &str, ident: &str) -> (i64, i64) {
        let cid = seed_comment(repo, "/t16-react", None).await;
        repo.update_status(cid, "approved").await.unwrap();
        let (rid, _) = repo.upsert_reaction(cid, emoji, ident).await.unwrap();
        (cid, rid)
    }

    #[test]
    fn status_roundtrips_through_text() {
        for (s, text) in [
            (Status::Pending, "pending"),
            (Status::Approved, "approved"),
            (Status::Spam, "spam"),
            (Status::Deleted, "deleted"),
        ] {
            assert_eq!(s.as_str(), text);
            assert_eq!(s.to_string(), text);
            assert_eq!(text.parse::<Status>().unwrap(), s);
        }
    }

    #[test]
    fn status_rejects_unknown_actions_with_admin_text() {
        let err = "publish".parse::<Status>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid action 'publish', must be one of: approved, spam, deleted, pending"
        );
    }

    #[test]
    fn validity_table_deleted_terminal_except_admin() {
        // Admin may move anywhere, including out of deleted.
        for from in [
            Status::Pending,
            Status::Approved,
            Status::Spam,
            Status::Deleted,
        ] {
            for to in [
                Status::Pending,
                Status::Approved,
                Status::Spam,
                Status::Deleted,
            ] {
                assert!(
                    is_valid_transition(from, to, Actor::Admin),
                    "admin {from}->{to}"
                );
            }
        }
        // Webhook decides live content only; never revives deleted.
        for to in [
            Status::Pending,
            Status::Approved,
            Status::Spam,
            Status::Deleted,
        ] {
            assert!(is_valid_transition(Status::Pending, to, Actor::Webhook));
        }
        for to in [Status::Pending, Status::Approved, Status::Spam] {
            assert!(!is_valid_transition(Status::Deleted, to, Actor::Webhook));
        }
        assert!(is_valid_transition(
            Status::Deleted,
            Status::Deleted,
            Actor::Webhook
        ));
        // Owners may only delete live content.
        for from in [Status::Pending, Status::Approved, Status::Spam] {
            assert!(is_valid_transition(
                from,
                Status::Deleted,
                Actor::SelfDelete
            ));
            for to in [Status::Pending, Status::Approved, Status::Spam] {
                if from == to {
                    continue; // same-status no-op is always allowed
                }
                assert!(!is_valid_transition(from, to, Actor::SelfDelete));
            }
        }
        assert!(!is_valid_transition(
            Status::Deleted,
            Status::Approved,
            Actor::SelfDelete
        ));
        assert!(is_valid_transition(
            Status::Deleted,
            Status::Deleted,
            Actor::SelfDelete
        ));
    }

    #[tokio::test]
    async fn comment_transition_emits_exactly_one_event_per_path() {
        let (repo, _dir) = setup_repo();
        for to in [
            Status::Approved,
            Status::Spam,
            Status::Deleted,
            Status::Pending,
        ] {
            let id = seed_comment(&repo, "/t16-once", None).await;
            // Seed away from the target so every path actually changes.
            let start = if to == Status::Pending {
                Status::Spam
            } else {
                Status::Pending
            };
            if start != Status::Pending {
                repo.update_status(id, start.as_str()).await.unwrap();
            }
            let sink = CountingSink::new();
            let out = Moderation::transition_comment(&repo, Some(&sink), id, to, Actor::Admin)
                .await
                .unwrap();
            assert!(out.changed);
            assert_eq!(sink.count(), 1, "exactly one event for ->{to}");
            let p = &sink.payloads()[0];
            assert_eq!(p["event"], "comment.status_changed");
            assert_eq!(p["id"], id);
            assert_eq!(p["new_status"], to.as_str());
            assert_eq!(p["changed_by"], "admin");
            assert_eq!(
                repo.get_comment(id).await.unwrap().unwrap().status,
                to.as_str()
            );
        }
    }

    #[tokio::test]
    async fn comment_transition_noop_writes_nothing_and_emits_nothing() {
        let (repo, _dir) = setup_repo();
        let id = seed_comment(&repo, "/t16-noop", None).await;
        let sink = CountingSink::new();
        let out =
            Moderation::transition_comment(&repo, Some(&sink), id, Status::Pending, Actor::Admin)
                .await
                .unwrap();
        assert!(!out.changed);
        assert_eq!(sink.count(), 0, "same-status must not emit");
    }

    #[tokio::test]
    async fn comment_transition_rejects_webhook_revive_and_unknown() {
        let (repo, _dir) = setup_repo();
        let id = seed_comment(&repo, "/t16-reject", None).await;
        repo.update_status(id, "deleted").await.unwrap();
        let sink = CountingSink::new();
        let err = Moderation::transition_comment(
            &repo,
            Some(&sink),
            id,
            Status::Approved,
            Actor::Webhook,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        assert_eq!(sink.count(), 0, "rejected transition must not emit");
        assert_eq!(
            repo.get_comment(id).await.unwrap().unwrap().status,
            "deleted"
        );
        // Unknown action never reaches the machine: the parse rejects first.
        assert!("bogus".parse::<Status>().is_err());
    }

    #[tokio::test]
    async fn self_delete_fires_exactly_one_event_real() {
        let (repo, _dir) = setup_repo();
        let id = seed_comment(&repo, "/t16-selfdel", Some("tok-self")).await;
        let sink = CountingSink::new();
        assert!(
            Moderation::self_delete_comment(&repo, Some(&sink), id, "tok-self")
                .await
                .unwrap()
        );
        assert_eq!(sink.count(), 1);
        let p = &sink.payloads()[0];
        assert_eq!(p["event"], "comment.status_changed");
        assert_eq!(p["old_status"], "pending");
        assert_eq!(p["new_status"], "deleted");
        assert_eq!(p["changed_by"], "self");
        assert_eq!(
            repo.get_comment(id).await.unwrap().unwrap().status,
            "deleted"
        );
    }

    #[tokio::test]
    async fn self_delete_wrong_token_emits_nothing() {
        let (repo, _dir) = setup_repo();
        let id = seed_comment(&repo, "/t16-selfbad", Some("tok-ok")).await;
        let sink = CountingSink::new();
        assert!(
            !Moderation::self_delete_comment(&repo, Some(&sink), id, "tok-wrong")
                .await
                .unwrap()
        );
        assert_eq!(sink.count(), 0);
    }

    #[tokio::test]
    async fn admin_reapprove_clears_delete_token_atomically() {
        let (repo, _dir) = setup_repo();
        let id = seed_comment(&repo, "/t16-token", Some("tok-revive")).await;
        let sink = CountingSink::new();
        assert!(
            Moderation::self_delete_comment(&repo, Some(&sink), id, "tok-revive")
                .await
                .unwrap()
        );
        assert!(
            repo.get_comment(id)
                .await
                .unwrap()
                .unwrap()
                .delete_token
                .is_some(),
            "self-delete keeps the token row (B10 precondition)"
        );
        let sink2 = CountingSink::new();
        let out =
            Moderation::transition_comment(&repo, Some(&sink2), id, Status::Approved, Actor::Admin)
                .await
                .unwrap();
        assert!(out.changed);
        let row = repo.get_comment(id).await.unwrap().unwrap();
        assert_eq!(row.status, "approved");
        assert!(
            row.delete_token.is_none(),
            "admin re-approve after self-delete clears the token (B10)"
        );
        assert_eq!(sink2.count(), 1);
    }

    #[tokio::test]
    async fn reaction_approve_race_keeps_new_emoji_pending() {
        let (repo, _dir) = setup_repo();
        let (_cid, rid) = seed_reaction(&repo, "❤️", "h:t16race").await;
        // Moderator reviewed ❤️; the owner swaps to 😄 first.
        let (_, changed) = repo.upsert_reaction(_cid, "😄", "h:t16race").await.unwrap();
        assert!(changed);
        // Stale approval for the seen emoji must fail and emit nothing.
        let sink = CountingSink::new();
        let err = Moderation::transition_reaction(
            &repo,
            Some(&sink),
            rid,
            Status::Approved,
            Actor::Admin,
            Some("❤️"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        assert_eq!(sink.count(), 0);
        let row = repo.get_reaction(rid).await.unwrap().unwrap();
        assert_eq!(row.reaction, "😄");
        assert_eq!(row.status, "pending", "unmoderated emoji stays pending");
        // Approving the current emoji works and emits once.
        let sink2 = CountingSink::new();
        let out = Moderation::transition_reaction(
            &repo,
            Some(&sink2),
            rid,
            Status::Approved,
            Actor::Admin,
            Some("😄"),
        )
        .await
        .unwrap();
        assert!(out.changed);
        assert_eq!(sink2.count(), 1);
        let p = &sink2.payloads()[0];
        assert_eq!(p["event"], "reaction.status_changed");
        assert_eq!(p["new_status"], "approved");
    }

    // M1 regression: approving a decided row is an explicit override, not a
    // pending race — spam→approved and deleted→approved must succeed.
    #[tokio::test]
    async fn reaction_approve_from_spam_succeeds() {
        let (repo, _dir) = setup_repo();
        let (_cid, rid) = seed_reaction(&repo, "👍", "h:t16m1spam").await;
        repo.update_reaction_status(rid, "spam").await.unwrap();
        let sink = CountingSink::new();
        let out = Moderation::transition_reaction(
            &repo,
            Some(&sink),
            rid,
            Status::Approved,
            Actor::Admin,
            None,
        )
        .await
        .unwrap();
        assert!(out.changed);
        assert_eq!(sink.count(), 1);
        let p = &sink.payloads()[0];
        assert_eq!(p["event"], "reaction.status_changed");
        assert_eq!(p["old_status"], "spam");
        assert_eq!(p["new_status"], "approved");
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn reaction_approve_from_deleted_succeeds() {
        let (repo, _dir) = setup_repo();
        let (_cid, rid) = seed_reaction(&repo, "👍", "h:t16m1del").await;
        repo.update_reaction_status(rid, "deleted").await.unwrap();
        let sink = CountingSink::new();
        let out = Moderation::transition_reaction(
            &repo,
            Some(&sink),
            rid,
            Status::Approved,
            Actor::Admin,
            None,
        )
        .await
        .unwrap();
        assert!(out.changed);
        assert_eq!(sink.count(), 1);
        let p = &sink.payloads()[0];
        assert_eq!(p["event"], "reaction.status_changed");
        assert_eq!(p["old_status"], "deleted");
        assert_eq!(p["new_status"], "approved");
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().status,
            "approved"
        );
    }

    #[tokio::test]
    async fn reaction_non_approve_paths_emit_once() {
        let (repo, _dir) = setup_repo();
        let (_cid, rid) = seed_reaction(&repo, "👍", "h:t16spam").await;
        let sink = CountingSink::new();
        let out = Moderation::transition_reaction(
            &repo,
            Some(&sink),
            rid,
            Status::Spam,
            Actor::Admin,
            None,
        )
        .await
        .unwrap();
        assert!(out.changed);
        assert_eq!(sink.count(), 1);
        assert_eq!(
            repo.get_reaction(rid).await.unwrap().unwrap().status,
            "spam"
        );
    }

    #[tokio::test]
    async fn status_payload_schemas_share_event_shape() {
        let c = comment_status_payload(42, Status::Pending, Status::Approved, Actor::Admin);
        assert_eq!(c["event"], "comment.status_changed");
        assert_eq!(c["id"], 42);
        assert_eq!(c["old_status"], "pending");
        assert_eq!(c["new_status"], "approved");
        assert_eq!(c["changed_by"], "admin");
        let r = reaction_status_payload(7, 42, "👍", Status::Pending, Status::Spam, Actor::Webhook);
        assert_eq!(r["event"], "reaction.status_changed");
        assert_eq!(r["comment_id"], 42);
        assert_eq!(r["reaction"], "👍");
        assert_eq!(r["changed_by"], "webhook");
        let created = reaction_created_payload(7, 42, "👍", "/blog/hello", true);
        assert_eq!(created["event"], "reaction.created");
        assert_eq!(created["status"], "pending");
    }

    #[tokio::test]
    async fn request_decision_accepts_whitelisted_and_ignores_rest() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"action": "approved"})),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let payload = serde_json::json!({"event": "comment.created"});
        assert_eq!(
            request_decision(&client, &server.uri(), &payload).await,
            Some(Status::Approved)
        );

        let server2 = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"action": "publish"})),
            )
            .mount(&server2)
            .await;
        assert_eq!(
            request_decision(&client, &server2.uri(), &payload).await,
            None
        );

        let server3 = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server3)
            .await;
        assert_eq!(
            request_decision(&client, &server3.uri(), &payload).await,
            None
        );
    }
}
