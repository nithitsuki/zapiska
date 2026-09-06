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

The modules `worker`, `mf2`, `ssrf`, `webmention_post`, and
`reqwest_client` compile only with the `webmentions` feature.

## Feature flags

The default feature set is `comments,webmentions`.

| Feature | State | Result |
|---|---|---|
| `comments` | Empty compatibility feature | Comment, reaction, feed, and admin code remains compiled. |
| `webmentions` | Default and optional | Webmention ingress, worker, microformats parsing, and SSRF code. |

The `comments` feature is empty. The comments-only build disables
`webmentions` modules with `--no-default-features --features comments`.

## Native comment flow

The native flow has this sequence:

1. The handler reads a form-encoded request.
2. Route rate limiting and the request body limit run before the handler.
3. The handler checks the honeypot, Turnstile, and the daily IP cap.
4. The handler validates the path, name, and author URL.
5. The handler sanitizes the content with `ammonia`.
6. The language gate checks the sanitized content when configured.
7. The handler resolves the author name, URL, and avatar.
8. The handler checks the parent comment when `parent_id` is present.
9. The repository stores the comment with its configured initial status.
10. The handler extracts double-quoted absolute HTTP and HTTPS URLs from the
    original form content.
11. The notification batcher receives a new comment event.
12. The moderation webhook receives the event when configured.

The content hash helps a moderation service find repeated content. The server
does not reject duplicate content by hash.

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
5. The worker checks the source hostname and resolved IP addresses.
6. The worker fetches the source through the shared HTTP client.
7. The worker checks that the source links to the target.
8. The worker parses h-entry and h-card data.
9. The worker upserts the comment by source URL and target path.
10. The worker records the source state in `webmention_seen`.

The queue capacity is `WORKER_BACKLOG`, with a default of `64`. A full queue
returns `503`. A source update keeps the existing moderation status.

The current client permits HTTP and HTTPS. The client checks the source before
the request. Its custom redirect policy checks each redirect host and literal
IP against the blocklist. The client does not set a five-hop redirect limit.

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

Schema changes are versioned with `PRAGMA user_version` in
`src/db/pool.rs` (`LATEST_SCHEMA_VERSION`, currently 8). Fresh databases get
the canonical `migrations/schema.sql` snapshot and are stamped. Legacy
`user_version = 0` databases run an idempotent catch-up (existence-checked
`ADD COLUMN` / `CREATE TABLE`, never blind `ALTER`s) and are then stamped.
A database newer than the binary refuses startup instead of running against
an unknown schema. Add a new `if current < N` block and bump `LATEST` for
every future schema change.
Comment reads build on one `COMMENT_COLUMNS` list plus a single
`select_comments` query builder in `src/db/repo/`, with `row_to_comment` as
the only row mapper; optional filters use `(?N IS NULL OR ...)` predicates
so one prepared statement covers the filtered and unfiltered cases.

## Middleware and route scope

The public router has these layers and routes:

- Body limits on native comment and webmention requests.
- Native rate limits on comment submission, deletion, and reactions.
- Read rate limits on the JSON read API and RSS feed.
- Webmention rate limits on webmention ingress.
- CORS on the public router.

The public router also contains health, embed, Swagger, admin login, and admin
logout routes. The protected admin route group is merged after the CORS layer.
Admin routes do not advertise CORS.

Configured CORS values can be one origin, a comma-separated list, or `*`.
Configured origins use a 600 second preflight cache. Wildcard CORS does not set
that cache value. The preflight methods are `GET`, `POST`, and `OPTIONS`.

Default rate limits are:

| Route group | Burst | Window |
|---|---:|---:|
| Native comment, deletion, and reactions | 50 | 60 seconds |
| Webmention ingress | 30 | 60 seconds |
| Public comments and RSS | 60 | 60 seconds |
| Single comment moderation | 10 | 60 seconds |

The admin rate limit does not cover every admin route. Admin authentication is
required for the protected admin route group.

## Notification flow

The notification batcher is in memory. It supports Telegram, Slack, and
Discord. Telegram needs both a bot token and a chat ID. Slack and Discord need a
webhook URL.

With the default settings, a new event opens a window for its page. The window
ends after `NOTIFY_BATCH_SECS`. A count of `NOTIFY_BATCH_THRESHOLD` flushes the
window early. `NOTIFY_BATCH_GRANULARITY=global` uses one window for the site.

Set `NOTIFY_BATCH_SECS=0` for immediate delivery. Delivery runs in spawned tasks
with a timeout. Delivery failure does not fail the comment request.

Open notification windows are lost when the process stops.

## Shutdown

The process listens for Ctrl+C and SIGTERM. The shutdown signal stops the Axum
server. The current shutdown function does not drain the webmention queue or
wait for worker jobs. Queued webmentions and open notification windows can be
lost when the process stops.

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
