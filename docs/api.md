# API

Interactive docs at `/swagger-ui/` when the server is running. Raw OpenAPI 3.1 spec at `/api-docs/openapi.json`.

## Public

### GET /api/comments

Approved comments for a path. Returns a flat list — the embed widget builds the thread tree client-side.

`parent_id` is the ID of the parent comment (null for top-level). `depth` is the nesting level (0 = top-level, up to `MAX_THREAD_DEPTH`).

| Param | Type | Default | Description |
|---|---|---|---|
| `path` | string | **required** | Path on the main site (e.g. `/blog/hello`). Starts with `/`. |
| `limit` | int | 50 | Max results (clamped 1-100). |
| `before` | int | — | Cursor: rows with `id < before`. |

Response (200):

```json
{
  "total": 137,
  "comments": [
    {
      "id": 42,
      "comment_type": "webmention",
      "author_name": "Alice",
      "author_url": "https://alice.blog",
      "author_avatar": "https://alice.blog/me.jpg",
      "content": "<p>Mentioned your page</p>",
      "created_at": "2026-07-03T16:40:00",
      "parent_id": null,
      "depth": 0
    }
  ]
}
```

`total` counts all approved comments for the path. Only `status = 'approved'` comments are included.

Errors: 400 (path missing/invalid).

---

### POST /api/comment

Submit a native comment. Creates a `pending` (or `approved`, depending on `DEFAULT_COMMENT_STATUS`) entry.

`Content-Type: application/x-www-form-urlencoded`

| Field | Required | Description |
|---|---|---|
| `target_path` | yes | Path on the main site. |
| `author_name` | yes | Display name (max 100 chars). |
| `content` | yes | HTML content (ammonia sanitized, max 2000 chars). |
| `author_url` | no | Author's website (absolute http/https). |
| `github_username` | no | GitHub username for profile enrichment. |
| `parent_id` | no | ID of the parent comment for threaded replies. Requires `MAX_THREAD_DEPTH > 0`. |
| `website` | no | Honeypot field — if non-empty the comment is stored with `honeypot = 1`. |
| `cf-turnstile-response` | conditional | Cloudflare Turnstile token. Required when `TURNSTILE_ENABLED=true`; ignored otherwise. Rendered automatically by the widget or by the explicit Turnstile script tag. |

Response: 201 with `{ "delete_token": "a1b2c3d4e5f6g7h8" }`. The `delete_token` is a 16-char hex string for self-service deletion. When `MODERATION_WEBHOOK_MODE=sync`, the response also includes the final moderation status: `{ "delete_token": "...", "status": "approved" }`.

Errors: 400 (validation, or Turnstile verification failure with `code: "turnstile_failed"`), 429 (rate limited), 503 (Turnstile siteverify endpoint unreachable — fail closed).

Author resolution (priority order):
1. `github_username` → GitHub API name + avatar (cached 30d)
2. `author_url` → name from form, URL kept as-is

Avatar resolution (priority order, independent of name):
1. `author_url` is a github.com URL → GitHub API avatar
2. (webmentions feature) Fetch author's page → h-card photo → favicon
3. `github_username` → GitHub API avatar
4. `author_url` domain → DiceBear generated avatar (consistent per domain)
5. `github_username` → DiceBear generated avatar (consistent per user)
6. DiceBear from a generic seed (anonymous fallback)

---

### POST /api/webmention

Requires the `webmentions` feature (default: on). Incoming W3C webmention. Enqueues background processing.

`Content-Type: application/x-www-form-urlencoded`

| Field | Required | Description |
|---|---|---|
| `source` | yes | URL of the page with a backlink. |
| `target` | yes | URL being linked to. Must match `PUBLIC_TARGET_ORIGIN`. |

Response: 202 Accepted (async processing).

Errors: 400 (invalid URL, origin mismatch, source=target), 503 (backlog full).

---

### POST /api/comment/{id}/delete

Self-service comment deletion. Uses the `delete_token` returned from the original submission.

`Content-Type: application/json`

```json
{ "token": "a1b2c3d4e5f6g7h8" }
```

Response (200): `{ "success": true }`

Errors: 404 (comment not found or token doesn't match).

---

## Auth

All `/api/admin/*` endpoints need either:
- `Authorization: Bearer <ADMIN_TOKEN>` header, or
- `admin_token=<ADMIN_TOKEN>` cookie (set via `POST /api/admin/login`)

Token comparison uses `subtle::ConstantTimeEq` (timing-safe).

### POST /api/admin/login

Exchange a token for a session cookie.

`Content-Type: application/json`

```json
{ "token": "your-admin-token" }
```

Response (200): `Set-Cookie: admin_token=<token>; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000` + `{ "success": true }`

Errors: 401 (wrong token).

### POST /api/admin/logout

Clear the session cookie.

Response (200): `Set-Cookie: admin_token=; Path=/; Max-Age=0` + `{ "success": true }`

---

## Admin

All admin endpoints return comment objects with these fields: `id`, `target_path`, `comment_type`, `source_url`, `author_name`, `author_url`, `author_avatar`, `content`, `status`, `parent_id`, `depth`, `honeypot`, `delete_token`, `submitter_ip`, `content_hash`, `created_at`.

### GET /api/admin/pending

Pending comments, newest first.

| Param | Type | Default | Description |
|---|---|---|---|
| `limit` | int | 50 | Max results (clamped 1-100). |
| `before` | int | — | Cursor. |
| `path` | string | — | Filter by target_path. |

Response (200):

```json
{
  "comments": [
    {
      "id": 7,
      "target_path": "/blog/hello",
      "comment_type": "native",
      "source_url": null,
      "author_name": "Bob",
      "author_url": "https://bob.example",
      "author_avatar": "https://api.dicebear.com/7.x/notionists/svg?seed=bob.example",
      "content": "<p>Hi!</p>",
      "status": "pending",
      "parent_id": null,
      "depth": 0,
      "honeypot": false,
      "delete_token": null,
      "submitter_ip": "192.168.1.1",
      "created_at": "2026-07-03T17:00:00"
    }
  ]
}
```

Errors: 401.

---

### GET /api/admin/comments

List comments by status, with optional path and IP filters.

| Param | Type | Default | Description |
|---|---|---|---|
| `status` | string | `pending` | One of: `pending`, `approved`, `spam`, `deleted`, `all`. |
| `limit` | int | 50 | Max results (clamped 1-100). |
| `before` | int | — | Cursor. |
| `path` | string | — | Filter by target_path. |
| `ip` | string | — | Filter by submitter IP address (requires `STORE_IP_ADDRESS=true`). |
| `content_hash` | string | — | Filter by content hash (for duplicate detection). |

Response (200): Same shape as `/api/admin/pending`. Each comment includes all fields plus `content_hash`.

Errors: 401.

---

### GET /api/admin/comments/{id}

Fetch a single comment with its ancestor chain. The `parents` array walks from immediate parent up to the root comment. Empty for top-level comments.

Response (200):

```json
{
  "comment": { "...all PendingComment fields..." },
  "parents": [
    { "...parent comment fields..." },
    { "...grandparent comment fields..." }
  ]
}
```

Errors: 401, 404.

---

### POST /api/admin/moderate

Approve, spam, delete, or revert a comment.

`Content-Type: application/json`

```json
{ "id": 42, "action": "approved" }
```

Valid actions: `approved`, `spam`, `deleted`, `pending`.

Response (200): `{ "id": 42, "status": "approved" }`

Errors: 400 (invalid action), 401, 404.

---

### POST /api/admin/moderate/batch

Moderate multiple comments in one request. Each action is processed independently — errors for individual items don't affect others.

`Content-Type: application/json`

```json
{
  "actions": [
    { "id": 10, "action": "approved" },
    { "id": 11, "action": "spam" }
  ]
}
```

Response (200):

```json
{
  "results": [
    { "id": 10, "status": "approved", "error": null },
    { "id": 11, "status": "spam", "error": null }
  ]
}
```

Errors: 401 (top-level). Individual action failures appear as `error` in each result.

---

### GET /api/admin/comments/{id}/urls

List extracted URLs for a specific comment. URLs are extracted from comment content at store time.

Response (200):

```json
{
  "comment_id": 42,
  "urls": [
    { "id": 1, "comment_id": 42, "url": "https://example.com/page", "domain": "example.com", "url_hash": "h:a1b2..." }
  ]
}
```

Errors: 401.

---

### GET /api/admin/urls/lookup

Look up all comments containing a URL or from a domain.

| Param | Description |
|---|---|
| `url_hash` | Hash of the normalized URL (returned from `/{id}/urls`). Returns stats + all matching comments. |
| `domain` | Domain name. Returns all unique URLs from this domain. |

Response (200) for `?url_hash=`:

```json
{
  "url": "https://spam.example/buy-now",
  "domain": "spam.example",
  "first_seen": "2026-07-01T12:00:00",
  "last_seen": "2026-07-05T14:00:00",
  "total_occurrences": 12,
  "unique_ips": 3,
  "unique_author_names": ["buy_now_user", "click_me"],
  "comments": [
    { "id": 10, "status": "spam", "target_path": "/blog/post-1", "created_at": "..." }
  ]
}
```

Errors: 401, 404 (hash not found).

---

### GET /api/admin/authors/lookup

Resolve an author identity and return aggregated stats. Queries by one or more signals.

| Param | Type | Description |
|---|---|---|
| `ip` | string | Submitter IP address. |
| `author_name` | string | Exact author name. |
| `author_url` | string | Author URL. |
| `combine` | bool | If true, merge results across all signals with OR (wider net). Default: false (AND, narrower). |

Response (200):

```json
{
  "total_comments": 23,
  "approved": 12,
  "spam": 8,
  "pending": 2,
  "deleted": 1,
  "first_seen": "2026-01-15T08:30:00",
  "last_seen": "2026-07-05T14:22:00",
  "recent_comments": [
    { "id": 42, "target_path": "/blog/hello", "status": "pending", "created_at": "..." }
  ]
}
```

Returns `{ "total_comments": 0 }` if no match.

Errors: 401.

---

### POST /api/admin/comments/context

Fetch context for multiple comments in one call. Reduces N API calls to 1 for batch processing.

`Content-Type: application/json`

```json
{
  "comment_ids": [42, 43, 44],
  "include_parents": true,
  "include_author_stats": true,
  "include_urls": false
}
```

Response (200):

```json
{
  "comments": [
    {
      "id": 42,
      "target_path": "/blog/hello",
      "comment_type": "native",
      "author_name": "Alice",
      "content": "<p>Hi</p>",
      "status": "pending",
      "parent_id": null,
      "depth": 0,
      "created_at": "2026-07-05T14:00:00",
      "parents": [
        { "id": 40, "author_name": "Bob", "depth": 0, "created_at": "..." }
      ],
      "author_stats": {
        "total_comments": 15,
        "approved": 4,
        "spam": 8,
        "pending": 3,
        "deleted": 0,
        "first_seen": "2026-06-01T12:00:00"
      }
    }
  ]
}
```

Errors: 401.

---

## Health

### GET /healthz

Returns 200 with body `ok`.

---

## CORS

Public endpoints (`/api/comment`, `/api/webmention`, `/api/comments`) return `Access-Control-Allow-Origin` only for the configured `ALLOWED_CORS_ORIGIN`. Admin endpoints don't advertise CORS.
