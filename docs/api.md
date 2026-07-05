# API

Interactive docs at `/swagger-ui/` when the server is running. Raw OpenAPI 3.1 spec at `/api-docs/openapi.json`.

## Public

### GET /api/comments

Approved comments for a path. Returns a flat list — the embed widget builds the thread tree client-side.

`parent_id` is the ID of the parent comment (null for top-level). `depth` is the nesting level (0 = top-level, max 4).

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
      "created_at": "2026-07-03 16:40:00",
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

Submit a native comment. Creates a `pending` entry.

`Content-Type: application/x-www-form-urlencoded`

| Field | Required | Description |
|---|---|---|
| `target_path` | yes | Path on the main site. |
| `author_name` | yes | Display name (max 100 chars). |
| `content` | yes | HTML content (ammonia sanitized, max 2000 chars). |
| `author_url` | no | Author's website (absolute http/https). |
| `github_username` | no | GitHub username for profile enrichment. |
| `parent_id` | no | ID of the parent comment for threaded replies. Parent must exist, be approved, on the same path, and at depth < 4. |

Response: 201 (empty body).

Errors: 400 (validation), 429 (rate limited).

Author resolution:
1. `github_username` → GitHub API name + avatar (cached 30d)
2. `author_url` → name from form, avatar from `icon.horse/<domain>`
3. Neither → name from form, no avatar/URL

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

## Auth

All admin endpoints need either:
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
      "created_at": "2026-07-03 17:00:00"
    }
  ]
}
```

Errors: 401 (no/invalid token).

---

### GET /api/admin/comments

List comments by status, with optional path filter. Useful for building moderation queues.

| Param | Type | Default | Description |
|---|---|---|---|
| `status` | string | `pending` | One of: `pending`, `approved`, `spam`, `deleted`, `all`. |
| `limit` | int | 50 | Max results (clamped 1-100). |
| `before` | int | — | Cursor. |
| `path` | string | — | Filter by target_path. |

Response (200): Same shape as `/api/admin/pending`.

Errors: 401.

---

### GET /api/admin/comments/{id}

Fetch a single comment with its ancestor chain. The `parents` array walks from immediate parent up to the root comment. Empty for top-level comments.

Response (200):

```json
{
  "comment": {
    "id": 10,
    "target_path": "/blog/hello",
    "comment_type": "native",
    "source_url": null,
    "author_name": "Charlie",
    "author_url": null,
    "author_avatar": null,
    "content": "<p>Does it support Markdown?</p>",
    "status": "pending",
    "parent_id": 9,
    "depth": 2,
    "created_at": "2026-07-04 12:00:00"
  },
  "parents": [
    {
      "id": 9,
      "author_name": "Bob",
      "content": "<p>Up to 4 levels.</p>",
      "depth": 1,
      "created_at": "2026-07-04 11:30:00"
    },
    {
      "id": 8,
      "author_name": "Alice",
      "content": "<p>How does threading work?</p>",
      "depth": 0,
      "created_at": "2026-07-04 11:00:00"
    }
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
    { "id": 11, "action": "spam" },
    { "id": 999, "action": "approved" }
  ]
}
```

Response (200):

```json
{
  "results": [
    { "id": 10, "status": "approved", "error": null },
    { "id": 11, "status": "spam", "error": null },
    { "id": 999, "status": "", "error": "comment 999 not found" }
  ]
}
```

Errors: 401 (top-level — no auth at all). Individual action failures appear as `error` in each result.

---

## Health

### GET /healthz

Returns 200 with body `ok`.

---

## CORS

Public endpoints (`/api/comment`, `/api/webmention`, `/api/comments`) return `Access-Control-Allow-Origin` only for the configured `ALLOWED_CORS_ORIGIN`. Admin endpoints don't advertise CORS.
