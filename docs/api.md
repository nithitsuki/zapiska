# API

Interactive API docs are available at `/swagger-ui/`.
The OpenAPI document is available at `/api-docs/openapi.json`.

All paths use the zapiska origin.

## Public API

### GET /api/comments

Return approved comments for one path.

The response is a flat list. The `parent_id` value links a reply to its
parent. A frontend can build the thread tree.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `path` | string | Required | Main site path. It must start with `/`. |
| `limit` | integer | `50` | Maximum result count. The server clamps it to `1` through `100`. |
| `sort` | string | `newest` | `newest` or `oldest`. |
| `before` | integer | None | For `newest`, return IDs below this value. |
| `after` | integer | None | For `oldest`, return IDs above this value. |

Response:

```json
{
  "total": 1,
  "comments": [
    {
      "id": 42,
      "comment_type": "native",
      "author_name": "Alice",
      "author_url": "https://alice.blog",
      "author_avatar": "https://alice.blog/avatar.jpg",
      "content": "<p>Great post.</p>",
      "created_at": "2026-07-03 16:40:00",
      "parent_id": null,
      "depth": 0,
      "reactions": {
        "👍": 5
      }
    }
  ]
}
```

`total` counts approved comments for the path. The `reactions` object contains
approved reaction counts only.

Errors:

- `400` for a missing or invalid path.
- `429` when the read limit is reached.

### GET /feed.xml

Return an RSS 2.0 feed of approved comments.

Omit `path` for the global feed. Add `path` for one page.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `path` | string | None | Main site path. |
| `limit` | integer | `50` | Maximum item count. The server clamps it to `1` through `100`. |

The response has this content type:

```text
application/rss+xml; charset=utf-8
```

The feed escapes XML text. It converts SQLite timestamps to RFC 822 dates.
Malformed dates use the Unix epoch. Global item titles include the page path.

### POST /api/comment

Create a native comment.

Content type:

```text
application/x-www-form-urlencoded
```

Form fields:

| Field | Required | Description |
|---|---|---|
| `target_path` | Yes | Main site path. |
| `author_name` | Yes, unless `github_username` is set | Display name. |
| `author_url` | No | Absolute HTTP or HTTPS URL. |
| `github_username` | No | GitHub login for URL and avatar lookup. |
| `content` | Yes | HTML input. The server sanitizes it. |
| `parent_id` | No | Approved parent comment ID. Replies need `MAX_THREAD_DEPTH > 0`. |
| `website` | No | Honeypot field when `HONEYPOT_FIELD` is unset (default `website`). When the operator renames the trap, custom forms must use the configured name: a non-empty value in the configured field marks the comment, any other trap-looking field is inert. |
| `cf-turnstile-response` | Conditional | Required when Turnstile is enabled. |

The server validates the path, name, and author URL. It sanitizes content and
then applies the language gate when the gate is enabled.

The default status is `pending`. Set `DEFAULT_COMMENT_STATUS=approved` to
publish native comments immediately.

Response `201`:

```json
{
  "delete_token": "0123456789abcdef0123456789abcdef",
  "status": "pending"
}
```

The response does not include the comment ID. The ID is available from the
admin API and moderation webhook.

Errors:

- `400` for validation or Turnstile failure.
- `413` for a body above `MAX_BODY_SIZE`.
- `429` for a route or daily limit.
- `503` when Turnstile verification is not available.

### POST /api/comment/{id}/delete

Set a comment status to `deleted` when the token matches.

Request:

```json
{
  "token": "0123456789abcdef0123456789abcdef"
}
```

Response `200`:

```json
{
  "success": true
}
```

The route uses the native rate limit. It returns `404` when the comment does
not exist or the token does not match.

A successful delete emits one `comment.status_changed` event
(`old_status` → `deleted`, `changed_by: "self"`).

### POST /api/comment/{id}/reaction

Create or change a reaction on an approved comment.

Request:

```json
{
  "reaction": "👍"
}
```

The reaction must be in `REACTIONS_SET`.

The default `REACTIONS_ALLOWED=admin` mode needs the admin token. In
`REACTIONS_ALLOWED=anyone` mode, the server identifies the caller with a hash
of the peer IP.

New and changed reactions start as `pending`. A repeated active reaction is a
no-op. Only approved reactions appear in public counts.

Response `201` for a new or changed reaction:

```json
{
  "id": 1,
  "reaction": "👍",
  "status": "pending",
  "changed": true
}
```

Response `200` for an active repeated reaction:

```json
{
  "id": 1,
  "reaction": "👍",
  "status": "pending",
  "changed": false
}
```

Errors:

- `400` for an invalid reaction or an unapproved comment.
- `401` when admin mode has no valid token.
- `404` when the comment does not exist.

### DELETE /api/comment/{id}/reaction

Delete the active reaction for the caller and comment.

Response:

```json
{
  "success": true
}
```

The route returns `success: false` when no active reaction exists.
The public CORS preflight does not advertise `DELETE`.

### POST /api/webmention

Receive a W3C webmention. This route exists only with the `webmentions`
feature.

Content type:

```text
application/x-www-form-urlencoded
```

Form fields:

| Field | Required | Description |
|---|---|---|
| `source` | Yes | Source page URL. |
| `target` | Yes | Main site URL. Its parsed origin must equal `PUBLIC_TARGET_ORIGIN`. |

The server rejects invalid URLs and equal source and target URLs. It places the
job in a bounded queue and returns `202`.

Errors:

- `400` for invalid input or an origin mismatch.
- `413` for a body above `MAX_BODY_SIZE`.
- `429` for the webmention rate or domain limit.
- `503` when the worker queue is full.

The worker fetches the source, checks the backlink, parses h-entry data, and
upserts the comment by source and target path. The first sighting can trigger
notifications. Update pings do not trigger a new notification.

## Authentication

Protected admin routes accept either a bearer header or a session cookie.

```text
Authorization: Bearer ADMIN_TOKEN
```

The login route sets the cookie. The cookie is `__Host-admin_token` with
`Path=/`, HttpOnly, `Secure`, and SameSite=Lax, and lasts 30 days. The
`__Host-` prefix plus `Secure` means browsers only send it over HTTPS —
serve the dashboard over TLS.

### POST /api/admin/login

Request:

```json
{
  "token": "your-admin-token"
}
```

Response `200` sets `__Host-admin_token` and returns:

```json
{
  "success": true
}
```

Wrong tokens return `401`.

### POST /api/admin/logout

Clear the admin session cookie.

Response `200`:

```json
{
  "success": true
}
```

## Admin API

All routes in this section need admin authentication.

Admin comment objects include:

```text
id, target_path, comment_type, source_url, author_name, author_url,
author_avatar, content, status, created_at, parent_id, depth, honeypot,
delete_token, submitter_ip, submitter_ip_hash, content_hash
```

The IP fields are present only when IP storage is enabled. `submitter_ip` is the
raw stored peer IP. `submitter_ip_hash` is its salted or unsalted SHA-256 hash.

### GET /api/admin/pending

List pending comments in newest-first order.

Parameters:

| Parameter | Default | Description |
|---|---:|---|
| `limit` | `50` | Maximum result count. |
| `before` | None | Return IDs below this value. |
| `path` | None | Limit results to one path. |

### GET /api/admin/paths

List paths that have comments.

### GET /api/admin/comments

List comments with filters.

| Parameter | Default | Description |
|---|---|---|
| `status` | `pending` | `pending`, `approved`, `spam`, `deleted`, or `all`. |
| `limit` | `50` | Maximum result count. |
| `before` | None | Return IDs below this value. |
| `path` | None | Limit results to one path. |
| `ip` | None | Match the stored raw peer IP. |
| `content_hash` | None | Match normalized content hashes. |

### GET /api/admin/comments/{id}

Return one comment and its ancestor chain.

```json
{
  "comment": {
    "id": 42,
    "status": "pending"
  },
  "parents": [
    {
      "id": 40,
      "status": "approved"
    }
  ]
}
```

The `parents` list starts with the direct parent and ends at the root.

### POST /api/admin/moderate

Change one comment status.

Request:

```json
{
  "id": 42,
  "action": "approved"
}
```

Valid actions are `approved`, `spam`, `deleted`, and `pending`.

The route is limited by `RATE_LIMIT_ADMIN_MODERATE`.

Each actual change emits one `comment.status_changed` webhook event;
a same-status change is a no-op and emits nothing.

### POST /api/admin/moderate/batch

Change many comment statuses.

Request:

```json
{
  "actions": [
    {
      "id": 42,
      "action": "approved"
    },
    {
      "id": 43,
      "action": "spam"
    }
  ]
}
```

Each item is processed independently. An item error appears in its result.
Each changed item emits one `comment.status_changed` event.

### GET /api/admin/comments/{id}/urls

Return URLs extracted from one native comment.

```json
{
  "comment_id": 42,
  "urls": [
    {
      "id": 1,
      "comment_id": 42,
      "url": "https://example.com/page",
      "domain": "example.com",
      "url_hash": "h:a1b2c3"
    }
  ]
}
```

The extractor reads the SANITIZED comment content (post-`ammonia`). It accepts absolute,
double-quoted HTTP and HTTPS `href` values.

### GET /api/admin/urls/lookup

Use `url_hash` to find comments that contain one URL. Use `domain` to list URLs
from one domain.

### GET /api/admin/authors/lookup

Find author activity with one or more signals.

| Parameter | Description |
|---|---|
| `ip` | Stored raw peer IP. |
| `author_name` | Exact author name. |
| `author_url` | Exact author URL. |
| `combine` | `true` combines signals with OR. The default uses AND. |

The response contains status counts, first and last timestamps, and recent
comments.

### POST /api/admin/comments/context

Return context for several comments in one request.

Request:

```json
{
  "comment_ids": [42, 43],
  "include_parents": true,
  "include_author_stats": true,
  "include_urls": false
}
```

The response can include parent chains, author statistics, and extracted URLs.

### GET /api/admin/reactions

List reactions with comment context.

Parameters:

| Parameter | Default | Description |
|---|---:|---|
| `status` | None | `pending`, `approved`, `spam`, `deleted`, or `all`. |
| `limit` | `50` | Maximum result count. |
| `before` | None | Return IDs below this value. |

### POST /api/admin/reactions/moderate

Change one reaction status.

Request:

```json
{
  "id": 1,
  "action": "approved"
}
```

Valid actions are `approved`, `spam`, `deleted`, and `pending`.

Each actual change emits one `reaction.status_changed` event.

The request accepts an optional `expected_emoji` carrying the reviewed emoji.
A stale value answers `400` so the moderator re-reads instead of approving sight-unseen.

### POST /api/admin/reactions/moderate/batch

Change many reaction statuses. The request and result shape matches comment
moderation batch requests.

Each changed reaction emits one `reaction.status_changed` event. Approvals
compare-and-swap on the reviewed emoji, so an emoji change racing an
approval keeps the new emoji pending.

### GET /api/admin/export

Return an export document for backup or migration.

```json
{
  "version": 1,
  "exported_at": "2026-08-07T16:18:40Z",
  "ip_hash_salted": false,
  "comments": [],
  "webmention_seen": [],
  "comment_urls": [],
  "github_profiles": [],
  "comment_reactions": []
}
```

The comments array contains all comment columns and all statuses. The reaction
array contains `id`, `comment_id`, `reaction`, `identifier`, `status`,
`created_at`, and `updated_at`. `ip_hash_salted` records whether the exporting
server had `IP_HASH_SECRET` set. The secret itself is never exported.

### POST /api/admin/import

Restore an export document.

```sh
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  --data @backup.json \
  http://localhost:3000/api/admin/import
```

The route accepts version `1` and a body up to 16 MiB.

The import process:

- Sorts comments by ID.
- Restores parent rows before child rows.
- Re-sanitizes comment content.
- Checks every row's fields: comments exactly as a native submission, URL,
  ledger, profile, and reaction rows against structural bounds.
- Re-derives each comment IP hash from its raw IP with this server's secret.
- Upserts webmention state and GitHub profiles.
- Replaces URL rows for each comment in one atomic commit.
- Restores reaction rows.
- Skips invalid and orphaned rows in every section without stopping the
  import. Only storage-health failures abort the restore, and the abort
  response still carries the per-section counts so far: fix the cause and
  re-import, which is idempotent and heals to the full state.
- Refuses into a live database before the first write when an exported
  comment or reaction ID holds different data, unless the body sets
  `"force": true` to overwrite colliding rows explicitly after review.

Response:

```json
{
  "comments_imported": 42,
  "comments_skipped": 1,
  "webmention_seen_imported": 3,
  "webmention_seen_skipped": 0,
  "comment_urls_imported": 17,
  "comment_urls_skipped": 0,
  "github_profiles_imported": 5,
  "github_profiles_skipped": 0,
  "comment_reactions_imported": 12,
  "comment_reactions_skipped": 0,
  "ip_hashes_recomputed": 40,
  "warning": null
}
```

`warning` is set when the export salt flag mismatches this server while
salted identities are present. Back up `.env` (`IP_HASH_SECRET`) alongside
every export. See [Deployment](deployment.md).

### GET /api/admin/status

Versions, database health, and non-secret configuration in one body for
the dashboard OPS tab:

```json
{
  "version": "0.2.0",
  "schema_version": 8,
  "export_version": 1,
  "health": "ok",
  "honeypot_field": "website",
  "webhook": { "configured": true, "mode": "async", "signed": true },
  "notify": { "telegram": true, "slack": false, "discord": false, "batch_secs": 60, "batch_threshold": 20, "batch_granularity": "page" },
  "trust_proxy": false,
  "reactions": { "allowed": "admin", "set": ["👍", "❤️", "😄", "😮", "😢", "😡"] },
  "default_comment_status": "pending",
  "max_thread_depth": 0,
  "turnstile_enabled": false,
  "language": { "allowed": [], "blocked": [], "emoji": "always" },
  "privacy": { "store_ip_address": false, "ip_hash_salted": true },
  "limits": { "native_comment_daily_cap": 50, "webmention_domain_hourly_cap": 10, "native_burst": 100, "native_window_secs": 60, "webmention_burst": 60, "webmention_window_secs": 60, "read_burst": 300, "read_window_secs": 60, "admin_moderate_burst": 30, "admin_moderate_window_secs": 60 },
  "db_quick_check": true,
  "worker_backlog": 64
}
```

Secret values (tokens, webhook URLs, salts) never appear — presence is
reported as booleans only.

## Health

### GET /healthz

Return `ok` with status `200` when the database answers `SELECT 1` through
the pool. Return `unavailable` with status `503` when it does not: the
endpoint is a readiness probe, and the Docker and compose health checks flip
unhealthy on database failure instead of staying green.

### GET /api/version

Return the binary and data-format versions:

```json
{
  "version": "0.2.0",
  "schema_version": 8,
  "export_version": 1
}
```

`zapiska --version` prints the same crate version without needing any
configuration.

## Client identity (proxies)

With the default `TRUST_PROXY=false` the server ignores forwarded IP
headers and keys rate limits, quotas, and IP hashes on the TCP peer
address. Set `TRUST_PROXY=true` only when every byte arrives via a proxy
you control: the server then prefers the edge-set `CF-Connecting-IP` when
present and valid (Cloudflare sets it to the single visitor IP itself,
unlike the append-only `X-Forwarded-For` whose leftmost entry a client can
spoof), falling back to leftmost `X-Forwarded-For`, `X-Real-IP`, the first
`Forwarded for=`, then the peer. A client that can reach the origin
directly can set any of these headers itself — lock the origin down
(Cloudflare proxy/Tunnel, or a firewall allow-listing Cloudflare IPs).

## CORS

The public router uses `ALLOWED_CORS_ORIGIN`.

The value can be one origin, a comma-separated list, or `*`.
Configured origins use a 600 second preflight cache. Wildcard CORS does not set
a preflight cache value.

The public CORS methods are `GET`, `POST`, and `OPTIONS`.
Protected admin routes do not advertise CORS.

## Error response

API errors use this shape:

```json
{
  "error": "human-readable reason",
  "code": "rate_limited"
}
```

Every `429` uses that shape, whichever limiter fired: the per-IP governors
and the handler-side quotas (daily IP cap, hourly domain cap) all return
`{"error", "code": "rate_limited"}` with a `Retry-After` header carrying the
hint in seconds.

See [Deployment](deployment.md), [Security](security.md), and
[Moderation engine](moderation-engine.md).
