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

Response: 201 with `{ "delete_token": "a1b2c3d4e5f6g7h8" }`. The `delete_token` is a 16-char hex string for self-service deletion.

Errors: 400 (validation), 429 (rate limited).

Author resolution (priority order):
1. `github_username` → GitHub API name + avatar (cached 30d)
2. `author_url` → name from form, URL kept as-is

Avatar resolution (priority order, independent of name):
1. `author_url` is a github.com URL → GitHub API avatar for that user
2. (webmentions feature) Fetch author's page → h-card photo → favicon parse
3. `author_url` domain → `https://icon.horse/<domain>` favicon proxy
4. `github_username` → GitHub API avatar (fallback)
5. `/embed/default-avatar.jpg` (local default, not hotlinked)

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

All admin endpoints return comment objects with these fields: `id`, `target_path`, `comment_type`, `source_url`, `author_name`, `author_url`, `author_avatar`, `content`, `status`, `parent_id`, `depth`, `honeypot`, `delete_token`, `submitter_ip`, `created_at`.

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
      "author_avatar": "https://icon.horse/bob.example",
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

Response (200): Same shape as `/api/admin/pending`. Each comment includes all fields.

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

## Health

### GET /healthz

Returns 200 with body `ok`.

---

## CORS

Public endpoints (`/api/comment`, `/api/webmention`, `/api/comments`) return `Access-Control-Allow-Origin` only for the configured `ALLOWED_CORS_ORIGIN`. Admin endpoints don't advertise CORS.
