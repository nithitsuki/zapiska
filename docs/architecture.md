# Architecture

zapiska is one Rust process with an Axum HTTP layer and a SQLite data store.
The process has an optional webmention worker.

```text
HTTP router
    |
    +-- public read API
    +-- native comment API
    +-- reaction API
    +-- RSS feed
    +-- webmention API --> bounded worker queue --> source fetch and parse
    +-- protected admin API
    |
SQLite connection pool
```

The database uses WAL mode. Repository methods run blocking SQLite work inside
`spawn_blocking`.

## Source modules

```text
src/
  config.rs             Environment configuration
  error.rs              HTTP and repository errors
  state.rs              Shared application state
  identity.rs           Author identity normalization (native + import)
  ingress.rs            Native submission pipeline (CommentIngress)
  language.rs           Native comment language gate
  sanitize.rs           HTML cleaning, hashes, and URL extraction
  validate.rs            Path, URL, and field checks
  timeutil.rs            Date and time helpers
  github.rs              GitHub lookup and profile cache interface
  avatar.rs              Favicon and avatar helpers
  ip_hash.rs             Peer IP hash helper
  turnstile.rs           Turnstile siteverify client
  worker.rs              Webmention worker
  mf2.rs                 Microformats parser
  ssrf.rs                Host and IP checks
  fetch.rs               One guarded outbound-fetch door (SafeFetcher)
  notify/
    mod.rs              Notification dispatch
    batcher.rs          In-memory notification windows
    telegram.rs         Telegram message format
    slack.rs            Slack message format
    discord.rs          Discord message format
  db/
    pool.rs              SQLite pool and migrations
    repo/
      comments.rs        Comment storage and moderation
      reactions.rs       Reaction storage and counts
      urls.rs             Extracted URL storage and lookup
      webmentions.rs     Webmention ledger storage
      github_profiles.rs GitHub cache storage
  http/
    mod.rs               Router and route layers
    layers.rs            CORS, body limit, and rate limit builders
    comments_read.rs     Public comment read API
    comment_post.rs      Native comment and deletion API
    reactions.rs         Public reaction API
    feed.rs              RSS feed
    webhook.rs           Shared fire-and-forget webhook sender
    webmention_post.rs   Webmention ingress
    admin/
      auth.rs             Login, logout, and token checks
      comments.rs         Admin comment queries
      moderate.rs         Comment moderation
      reactions.rs        Reaction moderation
      data.rs             JSON export and import
      lookup.rs           Author, URL, path, and context lookup
```

The modules `worker`, `mf2`, `ssrf`, `fetch`, and `webmention_post`
compile only with the `webmentions` feature (`http/reqwest_client` remains
only as a re-export shim over `fetch`).

## Feature flags

The default feature set is `comments,webmentions`.

| Feature | State | Result |
|---|---|---|
| `comments` | Empty compatibility feature | Comment, reaction, feed, and admin code remains compiled. |
| `webmentions` | Default and optional | Webmention ingress, worker, microformats parsing, and SSRF code. |

The `comments` feature is empty. The comments-only build disables
`webmentions` modules with `--no-default-features --features comments`.

## Native comment flow

`CommentIngress::submit` (`src/ingress.rs`) owns the 13-step native flow
with effect seams (`Notify`, the T16 `ModerationSink`, `UrlStore` — each
with in-memory fakes in tests). The HTTP handler (`comment_post.rs`) is a
thin adapter: form → `SubmitRequest` → `submit` → response. The sequence is:

1. Honeypot flag (reads `config.honeypot_field`, fallback `website`; the
   served widget emits the configured name via server-side substitution).
2. Turnstile verification when enabled (fail-closed).
3. Per-IP daily cap (flagged submissions consume quota too).
4. Validation of the path, author identity (shared `identity` rules with
   import: `Cc` + bidi/format spoof chars stripped, `github_username`
   shape-checked before URL interpolation, `MAX_AUTHOR_LEN` enforced),
   and author URL (absolute HTTP/HTTPS with a host).
5. Content hash on the RAW input (moderation lookup key, never a constraint).
6. Sanitization with `ammonia` plus truncation to `MAX_CONTENT_LEN`.
7. Language gate on the SANITIZED content when configured (hard block).
8. Author name, URL, and avatar resolution (GitHub enrichment).
9. Parent comment check when `parent_id` is present.
10. Delete-token generation (128-bit CSPRNG) and peer IP/hash capture.
11. Atomic store: comment row, initial status, and extracted-URL rows in ONE
    `BEGIN IMMEDIATE` commit (T15 unit). URL extraction reads the SANITIZED
    content, so URLs inside stripped tags never become rows.
12. Admin notification (batched digest or immediate).
13. Moderation webhook through the ONE shared T16 `deliver` adapter (sync
    awaits the decision and applies it on the plain write path; async emits).

The content hash helps a moderation service find repeated content. The server
does not reject duplicate content by hash, and there are no idempotency keys:
two identical concurrent POSTs store two rows (B9, deliberate — the engine
dedups post-hoc via `content_hash` lookup).

Follow-ups (not fixed here): language-gate quarantine tier (B6 — the gate
stays a hard block) and Unicode body-limit parity (B7 — non-ASCII authors
hit `MAX_BODY_SIZE` before `MAX_CONTENT_LEN`; ~680 emoji chars effective).

## Threaded replies

Top-level comments use `parent_id = null` and `depth = 0`.

For a reply, the parent must:

- Exist.
- Have `status = approved`.
- Use the same `target_path`.
- Have a depth below `MAX_THREAD_DEPTH`.

The server clamps `MAX_THREAD_DEPTH` to `0` through `10`. The default value is
`0`, so replies are disabled by default.

The public API returns a flat list. The supplied widget builds the tree in the
browser. The API supports `newest` and `oldest` order with matching cursors.

## Reaction flow

The reaction handler checks the configured reaction set and the comment status.
Only approved comments accept reactions.

The default mode requires the admin token. In `anyone` mode, the server uses a
hash of the peer IP as the reaction identifier.

The repository stores one active reaction for each comment and identifier.
New and changed reactions start as `pending`. Only approved reactions appear
in public counts. A repeated reaction is a no-op.

Reaction creation and status changes can send moderation webhook events.

## Webmention flow

The webmention endpoint is available only with the `webmentions` feature.

1. The handler parses `source` and `target` as absolute HTTP or HTTPS URLs.
2. The handler compares the target origin with `PUBLIC_TARGET_ORIGIN`.
3. The handler rejects equal source and target URLs.
4. The handler sends the job to a bounded channel and returns `202`.
5. SafeFetcher checks each hop's hostname and resolved IP addresses, with a
   5-hop redirect cap and a 1 MiB streaming body cap.
6. `WebmentionProcessor` (`src/worker.rs`) owns the policy behind a
   `SourceFetcher` adapter: production wires SafeFetcher (never the shared
   HTTP client, which serves operator-configured endpoints only), tests
   inject a canned mock — the old `allow_loopback` production parameter is
   gone. The spawn loop is a thin drain over `process`.
7. The processor re-fetches the source in EVERY ledger state
   (`unknown`/`alive`/`gone`): a re-ping after `gone` re-verifies, so
   gone→alive resurrection works.
8. The processor checks that the source links to the target, then parses
   h-entry and h-card data from the single fetched parse.
9. The processor upserts the comment by (source URL, target path) plus the
   ledger row in one commit: updates keep their moderation status while
   content and `content_hash` (computed by the worker over the raw
   e-content) refresh; `is_new` notifications key on the pair, so a second
   page mentioned by the same source notifies on its own.
10. A 410 (or a second consecutive backlink-less fetch) deletes EVERY
    comment the source owns, across all target paths, through the T16
    moderation machine (one `comment.status_changed` event per deleted
    comment); a restored backlink brings the comment back as `pending`
    through the same machine (deleted→pending fires, clearing tokens per
    B10).

The queue capacity is `WORKER_BACKLOG`, with a default of `64`. A full queue
returns `503`. A source update keeps the existing moderation status.

SafeFetcher permits HTTP and HTTPS. It resolves and checks the source
before the request and re-checks every redirect target with a fresh
resolution, so named-host and trailing-dot redirects into private networks
are refused. Redirects are capped at five hops (fail-closed) and bodies at
1 MiB streamed; the fetched page is parsed exactly once and shared by the
backlink check and the h-entry parse. The backlink match ignores URL
fragments but treats query, path case, and trailing slash as significant —
deliberately no utm-stripping (a tracking-param allowlist drifts and can
be gamed; the pinger controls both sides, so exactness costs nothing).

The seen ledger (`webmention_seen`) is a per-pair state machine with
grace: the `gone` row is the first-miss memory, so one backlink-less 200
never deletes — deletion waits for the confirmed second observation
(`MISSES_TO_TOMBSTONE = 2`, a code constant pinned by the blip/cycle
tests). `Repo::record_seen_alive` / `record_seen_gone` own the writes;
the processor owns the transitions.

## Database

The database has five tables:

| Table | Purpose |
|---|---|
| `comments` | Native comments, webmentions, status, reply data, and hashes. |
| `webmention_seen` | Source and target state for repeated and gone mentions. |
| `comment_urls` | Normalized URLs found in native comment content. |
| `github_profiles` | Positive and negative GitHub profile cache entries. |
| `comment_reactions` | Reaction identity, value, status, and timestamps. |

The connection pool sets these pragmas:

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;
```

One process serves one database file: the batcher windows, the `Limiter`
counts, and the governor buckets live in memory, so a second process would
split quotas and double-send digests. `AppState::start` claims a
`<database>.lock` sibling file holding its PID and refuses when another live
instance holds it; a stale file from a crash is reclaimed by PID liveness on
Linux, and a clean shutdown releases it after the notification drain. Two
instances with different database files never block each other. See
[ADR-0001](adr/0001-single-process-topology.md) and
[Deployment](deployment.md).

Schema changes are versioned with `PRAGMA user_version` in
`src/db/pool.rs`. The `MIGRATIONS` array is the single source of truth for
upgrades: entry `MIGRATIONS[N]` holds the DDL that brings a database from
version N-1 to N, the stamp written after a run is the step index, and
`LATEST_SCHEMA_VERSION` is derived from the array length. Fresh databases
get the canonical `migrations/schema.sql` snapshot and are stamped. Legacy
`user_version = 0` databases run every step in order (column additions go
through the existence-checked `add_column_if_missing` safety net, every
other statement is `IF NOT EXISTS`, so interrupted and partial upgrades
resume cleanly) and are then stamped. A database newer than the binary
refuses startup instead of running against an unknown schema. Add one
`MIGRATIONS` entry and update `migrations/schema.sql` together for every
future schema change; the `stepped_v0_upgrade_matches_fresh_install` test
diffs object sets, column shapes, and index definitions between a stepped
v0 upgrade and a fresh install, and
`never_altered_table_definitions_match_snapshot` pins exact `CREATE TABLE`
text (modulo whitespace) for tables no step ever ALTERs, catching CHECK and
table-UNIQUE drift the shape diff cannot see. The `comments` table is
excluded from the text pin because ALTER history legitimately rewrites its
stored definition.

Comment reads build on one `COMMENT_COLUMNS` list plus a single
`select_comments` query builder in `src/db/repo/`, with `row_to_comment` as
the only row mapper; optional filters use `(?N IS NULL OR ...)` predicates
so one prepared statement covers the filtered and unfiltered cases.

Storage failures return a typed `RepoError` converted once at the rusqlite
boundary: `Constraint` (UNIQUE, FOREIGN KEY, CHECK — the row is invalid),
`Busy` and `Io` (retryable: locked, I/O, full, unopenable), or `Other`. All
four render the same message and HTTP status as before; the type exists so
restore skip-and-count (T17) and transactional retries (T15) can branch on
it structurally instead of matching message strings.

Related writes share one pooled connection and one `BEGIN IMMEDIATE`
transaction through `Repo::with_conn` (caller-owned statements, one acquire)
and `Repo::with_tx` (the same wrapped in an immediate transaction; contention
surfaces as `Busy` for backoff retry, never a hang). Native comment store
(insert plus auto-approve status plus extracted-URL rows), webmention store
(upsert plus ledger row), and gone handling (ledger `gone` plus comment
deletion) each commit atomically: a mid-unit failure rolls everything back.
An invalid URL row (empty fields, `Constraint`) is skipped with a warn log
while the comment still commits; `Busy`/`Io`/`Other` abort the whole unit.
Reaction approval is a compare-and-swap on the seen emoji
(`WHERE id = ? AND status = 'pending' AND reaction = ?`), so an emoji change
racing an approval leaves the new emoji pending instead of approving it
sight-unseen. The admin single and batch reaction routes carry the reviewed
value as `expected_emoji`: a stale value is rejected with `400`, an absent
value approves whatever is stored. The admin export reads all five tables on one connection in one
deferred read transaction (a single WAL snapshot); the public read API batches
list plus total plus reaction counts per request the same way. Import stays
per-row (T17 owns restore).

## Middleware and route scope

The router is assembled from route groups (`src/http/routes.rs`), each with
its route set and layer recipe in one constructor:

- `native_write_routes` — comment submission, deletion, and both reaction
  methods share one governor bucket and the form body limit.
- `public_read_routes` — the JSON read API and the RSS feed share one
  governor bucket.
- `session_routes` — login is throttled per IP on its own bucket; logout is
  unthrottled.
- `webmention_routes` — webmention ingress with the form body limit and its
  own bucket (only with the `webmentions` feature).
- `protected_admin_routes` — the authenticated admin group (path list in
  `ADMIN_ROUTE_PATHS`): single and batch comment moderation, single and
  batch reaction moderation, and export each draw on their own bucket of the
  admin budget.

`routes::compose` wraps the public router in the CORS layer first and merges
the protected admin group after it, so admin responses never advertise CORS.
The ordering invariant is that function boundary, covered by a test that
asserts every listed admin path lacks CORS headers.

Client identity (`src/http/peer.rs`) resolves the caller to one normalized
peer address per request (IPv4-mapped addresses canonicalize to IPv4).
Governors (via `ClientIdentityExtractor`), the in-memory `Limiter`, and IP
hashing all consume that identity, so proxy handling (`TRUST_PROXY`) flips
in one place without touching handlers.

The public router also contains health, embed, Swagger, and session routes.
The protected admin route group is merged after the CORS layer. Admin routes
do not advertise CORS.

Configured CORS values can be one origin, a comma-separated list, or `*`.
Configured origins use a 600 second preflight cache. Wildcard CORS does not set
that cache value. The preflight methods are `GET`, `POST`, and `OPTIONS`.

Default rate limits are a burst bucket plus a sustained rate. The sustained
column is what the governor enforces after the burst is spent
(`burst / window` requests per second). A config-level test re-derives this
table from `Config::default`, so the numbers cannot drift from the code:

| Route group | Burst | Window | Sustained |
|---|---:|---:|---:|
| Native comment, deletion, and reactions | 100 | 60 seconds | 1.67/s |
| Webmention ingress | 60 | 60 seconds | 1.00/s |
| Public comments and RSS | 300 | 60 seconds | 5.00/s |
| Single comment moderation | 30 | 60 seconds | 0.50/s |
| Login, batch moderation, reaction moderation, export (own bucket each) | 30 | 60 seconds | 0.50/s |

<!-- RATE-LIMITS: native=100/60 webmention=60/60 read=300/60 admin=30/60 -->

The admin rate limit covers single moderation, login, batch moderation
(both comment and reaction batches), single reaction moderation, and export.
Other admin routes do not have this governor. Admin authentication is
required for the protected admin route group.

## Notification flow

Admin notifications go through one `Channel` seam
(`src/notify/channel.rs`): Telegram, Slack, and Discord are thin adapters
that own only their wire format and escape rules. Shared dispatch formats
one message per adapter and delivers with a shared retry policy: up to three
attempts with backoff on transient failures (network errors, 429, 5xx), then
a logged drop. Delivery is fire-and-forget with bounded retry (duplicates
possible on timeout), log-and-drop — failures never
fail the comment request. Permanent failures (other 4xx, Telegram `ok:
false`) are attempted once. Adding a channel is one new adapter file plus
the registration checklist in `Notifier::channels`.

Per-channel size budgets: Telegram 4096 characters, Slack 3000 (section
block), Discord 2000. Formatters shrink preview text, then names, then the
commenter list; the moderation footer survives every shrink stage. Telegram
needs both a bot token and a chat ID. Slack and Discord need a webhook URL.

The notification batcher (`src/notify/batcher.rs`) collects comments into
fixed windows per page. The first comment opens a window that flushes
exactly at `opened_at + NOTIFY_BATCH_SECS` — later comments never extend it
— or early when `NOTIFY_BATCH_THRESHOLD` is reached.
`NOTIFY_BATCH_GRANULARITY=global` shares one site-wide window. One timer
task is armed per window, when the window opens. The window clock is an
injected `Clock` seam (fake clock in tests).

Set `NOTIFY_BATCH_SECS=0` for immediate delivery. Delivery runs in spawned
tasks with a timeout. Delivery failure does not fail the comment request.

Open windows flush on shutdown: after the server stops accepting
connections, `NotificationBatcher::drain` delivers each open window as a
final digest (channels concurrently, same retry policy, awaited), then
awaits spawned in-flight sends. Total shutdown latency is bounded to 12 s.
The drain runs twice so a webmention job finishing mid-drain is still
caught; a job completing after the second pass's checks is best-effort.
`Retry-After` on 429s is honored up to a 2 s cap per wait.

## Shutdown

The process listens for Ctrl+C and SIGTERM. The shutdown signal stops the Axum
server, then the shutdown drain flushes open notification windows as final
digests. The current shutdown function does not drain the webmention queue or
wait for worker jobs. Queued webmentions can be lost when the process stops.

## Errors

JSON errors use this shape:

```json
{
  "error": "human-readable reason",
  "code": "rate_limited"
}
```

The API uses status codes `200`, `201`, `202`, `400`, `401`, `404`, `413`,
`429`, `500`, and `503`.

See [API](api.md), [Security](security.md), and [Deployment](deployment.md).
