# Specification

zapiska is a self-hosted comment and webmention engine.
It uses Rust, Axum, Tokio, and SQLite.

The server supports:

1. Native comments.
2. Threaded replies.
3. Approved comment reads.
4. Reactions with moderation status.
5. RSS feeds.
6. W3C webmention receipt.
7. Admin moderation and lookup.
8. JSON export and import.
9. Telegram, Slack, and Discord notifications.

The default target origin is `https://nithitsuki.com`.
Deployments must set `PUBLIC_TARGET_ORIGIN` to their own site origin.

## Architecture

The process has these layers:

```text
Axum router
    |
    +-- public comments
    +-- native comments
    +-- reactions
    +-- RSS
    +-- webmentions --> bounded Tokio channel --> worker
    +-- admin routes
    |
SQLite pool and repository
```

All SQLite work runs inside `spawn_blocking`.
The database uses WAL mode.

## Feature flags

```toml
default = ["comments", "webmentions"]
comments = []
webmentions = ["scraper", "ipnet"]
```

The `comments` feature is empty. It remains for the comments-only build.

The `webmentions` feature controls the webmention endpoint, worker, parser,
SSRF module, and webmention-specific avatar fetches.

Build without webmentions:

```sh
cargo build --release --no-default-features --features comments
```

Language detection is a runtime feature. The `whatlang` dependency is compiled
in every build. The language gate is off unless a language list is configured.

## Configuration

The server reads environment variables at startup.

| Variable | Default | Description |
|---|---|---|
| `ADMIN_TOKEN` | Required | Token for protected admin routes. |
| `BIND_ADDR` | `127.0.0.1:3000` | Listen address. |
| `PUBLIC_TARGET_ORIGIN` | `https://nithitsuki.com` | Accepted webmention target origin. |
| `ALLOWED_CORS_ORIGIN` | `https://nithitsuki.com` | One origin, a list, or `*`. |
| `DATABASE_PATH` | `./comments.db` | SQLite file path. |
| `GITHUB_TOKEN` | Unset | Optional GitHub API token. |
| `MAX_CONTENT_LEN` | `2000` | Stored content limit in characters. |
| `MAX_AUTHOR_LEN` | `100` | Author name limit in characters. |
| `MAX_BODY_SIZE` | `8192` | Global body limit in bytes. |
| `FETCH_TIMEOUT_MS` | `4000` | Outbound request timeout. |
| `WORKER_BACKLOG` | `64` | Webmention queue capacity. |
| `RUST_LOG` | `info` | Log filter. |
| `MAX_COMMENTS_PER_IP_PER_DAY` | `50` | Native comment daily cap. |
| `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR` | `10` | Webmention domain cap. |
| `STORE_IP_ADDRESS` | `false` | Store raw and hashed peer IP values. |
| `IP_HASH_SECRET` | Unset | Salt for the IP hash. |
| `MODERATION_WEBHOOK_URL` | Unset | External moderation webhook. |
| `MODERATION_WEBHOOK_MODE` | `async` | `async` or `sync`. |
| `DEFAULT_COMMENT_STATUS` | `pending` | Initial native comment status. |
| `MAX_THREAD_DEPTH` | `0` | Reply depth. Clamped to `0` through `10`. |
| `TELEGRAM_BOT_TOKEN` | Unset | Telegram bot token. |
| `TELEGRAM_CHAT_ID` | Unset | Telegram destination. |
| `TELEGRAM_API_BASE` | Telegram API URL | Telegram API override. |
| `SLACK_WEBHOOK_URL` | Unset | Slack webhook. |
| `DISCORD_WEBHOOK_URL` | Unset | Discord webhook. |
| `NOTIFY_BATCH_SECS` | `60` | Notification window. Zero sends immediately. |
| `NOTIFY_BATCH_THRESHOLD` | `20` | Early notification flush count. |
| `NOTIFY_BATCH_GRANULARITY` | `page` | `page` or `global`. |
| `REACTIONS_ALLOWED` | `admin` | `admin` or `anyone`. |
| `REACTIONS_SET` | Six emoji values | Allowed reaction values. |
| `COMMENT_LANG_ALLOWED` | Unset | ISO 639-1 allow list. |
| `COMMENT_LANG_BLOCKED` | Unset | ISO 639-1 block list. |
| `COMMENT_LANG_ALLOW_EMOJI` | `always` | `always`, `never`, or `if_unknown`. |
| `TURNSTILE_ENABLED` | `false` | Require Turnstile for native comments. |
| `TURNSTILE_SECRET_KEY` | Unset | Turnstile secret. |
| `TURNSTILE_VERIFY_URL` | Cloudflare URL | Turnstile verify endpoint. |

Rate limit variables are:

| Variable | Default |
|---|---:|
| `RATE_LIMIT_NATIVE` | `50` |
| `RATE_LIMIT_NATIVE_WINDOW` | `60` |
| `RATE_LIMIT_WEBMENTION` | `30` |
| `RATE_LIMIT_WEBMENTION_WINDOW` | `60` |
| `RATE_LIMIT_READ` | `60` |
| `RATE_LIMIT_READ_WINDOW` | `60` |
| `RATE_LIMIT_ADMIN_MODERATE` | `10` |
| `RATE_LIMIT_ADMIN_MODERATE_WINDOW` | `60` |

`HONEYPOT_FIELD` is loaded from the environment. The current native handler
uses the `website` field regardless of this value.

## Data model

SQLite has five tables.

### comments

The `comments` table has these fields:

| Field | Meaning |
|---|---|
| `id` | Autoincrement row ID. |
| `target_path` | Local path on the target site. |
| `comment_type` | `native` or `webmention`. |
| `source_url` | Webmention source URL or null. |
| `author_name` | Cleaned author name. |
| `author_url` | Absolute HTTP or HTTPS URL or null. |
| `author_avatar` | Avatar URL or null. |
| `content` | Sanitized HTML. |
| `status` | `pending`, `approved`, `spam`, or `deleted`. |
| `created_at` | Creation timestamp. |
| `updated_at` | Last update timestamp. |
| `parent_id` | Parent comment ID or null. |
| `depth` | Reply depth. |
| `honeypot` | Honeypot flag. |
| `delete_token` | Native self-delete token or null. |
| `submitter_ip` | Raw peer IP when IP storage is enabled. |
| `submitter_ip_hash` | Salted or unsalted SHA-256 IP hash. |
| `content_hash` | Hash of normalized input content. |

### webmention_seen

This table tracks source and target pairs.
The status is `alive` or `gone`.

### comment_urls

This table stores URLs found in native comment form HTML.
Each row has a normalized URL, domain, and URL hash.

### github_profiles

This table stores positive and negative GitHub profile cache entries.
Positive entries use a 30 day cache. Negative entries use a one hour cache.

### comment_reactions

This table stores one row for each comment and reaction identifier.
The status uses the same four values as the `comments` table.

## Native comments

### Request

`POST /api/comment` uses form encoding.

```text
target_path=/blog/hello
author_name=Alice
author_url=https://alice.blog
github_username=alice
parent_id=42
content=Great post.
website=
cf-turnstile-response=token
```

### Processing

1. Apply the native rate limit and body limit.
2. Check Turnstile when enabled.
3. Check the per-IP daily cap.
4. Validate the target path and author fields.
5. Compute the content hash.
6. Sanitize and truncate content.
7. Apply the language gate when enabled.
8. Resolve author and avatar data.
9. Check the parent comment.
10. Store the row.
11. Extract native comment URLs.
12. Queue notifications.
13. Send the moderation webhook.

The content hash supports moderation lookup. It does not reject duplicate rows.

### Response

The server returns `201` with:

```json
{
  "delete_token": "0123456789abcdef",
  "status": "pending"
}
```

The status can be `pending`, `approved`, `spam`, or `deleted` after a sync
moderation decision.

The response does not include the comment ID.

### Self deletion

`POST /api/comment/{id}/delete` accepts:

```json
{
  "token": "0123456789abcdef"
}
```

The route uses the native rate limit. A missing row and a wrong token both
return `404`.

## Threaded replies

Replies need `MAX_THREAD_DEPTH > 0`.

The parent must exist, be approved, and use the same target path.
The child depth is the parent depth plus one.

The server clamps configured depth to `0` through `10`.

## Reactions

`POST /api/comment/{id}/reaction` accepts a configured reaction value.
The target comment must be approved.

In admin mode, the bearer token identifies the reaction as `admin`.
In anyone mode, the peer IP hash identifies the reaction.

The database has one active row for each comment and identifier.
Changing a reaction resets its status to `pending`.
Repeating an active reaction does not change its status.

`DELETE /api/comment/{id}/reaction` marks the active reaction as deleted.

Only approved reactions appear in the public `reactions` object.

## Language gate

The gate applies to native comments after HTML sanitization.

The detector uses `whatlang` and an internal ISO 639-3 value.
Configuration uses ISO 639-1 values.

When an allow list exists, it has precedence over the block list.
The gate returns `400` and stores no rejected comment.

Unknown content uses the emoji policy:

- `always` accepts unknown content.
- `never` rejects emoji-heavy content.
- `if_unknown` accepts emoji-heavy content and rejects other unknown content.

## Webmentions

`POST /api/webmention` exists only with `webmentions`.

The handler:

1. Parses absolute source and target URLs.
2. Compares the target origin with `PUBLIC_TARGET_ORIGIN`.
3. Rejects equal source and target URLs.
4. Queues the job.
5. Returns `202`.

The worker:

1. Checks the source host and resolved IP values.
2. Fetches the source through the shared client.
3. Checks for a link to the target.
4. Parses h-entry and h-card data.
5. Sanitizes the selected content.
6. Upserts the comment by source and target path.
7. Records the result in `webmention_seen`.

The worker uses a bounded queue. A full queue returns `503`.
The worker stores webmentions as top-level comments.

The shared client permits HTTP and HTTPS. It uses the configured request
timeout and a ten second connection timeout. The redirect policy checks hosts
and literal IP values. It does not set a redirect count.

## Public read API

`GET /api/comments` needs `path`.

The default order is newest first. `before` returns IDs below the cursor.
`sort=oldest` returns oldest first. `after` returns IDs above the cursor.

The response contains only approved comments and approved reaction counts.

## RSS

`GET /feed.xml` returns approved comments in RSS 2.0.

Without `path`, the feed contains comments from all paths.
With `path`, the feed contains comments for one path.

The feed escapes XML text and converts timestamps to RFC 822.

## Admin routes

Protected routes accept a bearer token or an admin session cookie.

The admin API provides:

- Pending comment listing.
- Path listing.
- Comment status listing.
- Parent chain lookup.
- Single and batch comment moderation.
- Extracted URL lookup.
- Author lookup.
- Bulk context lookup.
- Reaction listing and moderation.
- Full JSON export and import.

The single comment moderation route uses the admin rate limit.
The batch route processes items independently.

## Export and import

`GET /api/admin/export` returns version `1`.
The export contains:

- All comment rows.
- All webmention ledger rows.
- All extracted URL rows.
- All GitHub profile rows.
- All reaction rows.

`POST /api/admin/import` accepts version `1` and a body up to 16 MiB.

The import sorts comments by ID and restores parents before children.
It re-sanitizes content and checks selected field values.
It skips failed comment and URL rows and reports their counts.

## Notifications

The notification batcher supports Telegram, Slack, and Discord.

Telegram needs a bot token and chat ID.
Slack and Discord need a webhook URL.

The batcher groups events by page by default.
Use `global` to use one site-wide window.
Set the window to `0` for immediate delivery.

The batcher stores open windows in memory.
Open windows are lost when the process stops.

Webmention updates do not create a new notification.

## Middleware

The public router uses:

- Route body limits.
- Per-route rate limits.
- CORS.

The protected admin routes are merged outside the CORS layer.
The public CORS methods are `GET`, `POST`, and `OPTIONS`.

The public router also contains health, embed, Swagger, login, and logout.

## Security rules

The server must:

- Sanitize stored HTML.
- Validate HTTP and HTTPS author URLs.
- Validate target paths.
- Check webmention source addresses.
- Compare admin tokens in constant time.
- Limit request bodies.
- Limit native, webmention, read, and single moderation routes.

Most repository queries use SQLite parameters.
The author lookup still builds escaped filter expressions.

## Shutdown

The process listens for Ctrl+C and SIGTERM.
The current shutdown handler stops the Axum server.
It does not drain the webmention queue or notification batcher.

## Error response

API errors use:

```json
{
  "error": "human-readable reason",
  "code": "rate_limited"
}
```

The API uses status codes `200`, `201`, `202`, `400`, `401`, `404`, `413`,
`429`, `500`, and `503`.

## Out of scope

- Outbound webmention sending.
- Author login.
- Multi-user administration.
- Multi-tenant hosting.
- Image upload.
- Media hosting.
- WebSub.
- Salmention.
