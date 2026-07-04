# API

Interactive docs at `/swagger-ui/` when the server is running. Raw OpenAPI 3.1 spec at `/api-docs/openapi.json`.

## Public

### GET /api/comments

Approved comments for a path.

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
      "created_at": "2026-07-03 16:40:00"
    }
  ]
}
```

`total` counts all approved comments for the path.

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

Response: 201 (empty body).

Errors: 400 (validation), 429 (rate limited).

Author resolution:
1. `github_username` → GitHub API name + avatar (cached 30d)
2. `author_url` → name from form, avatar from `icon.horse/<domain>`
3. Neither → name from form, no avatar/URL

---

### POST /api/webmention

Incoming W3C webmention. Enqueues background processing.

`Content-Type: application/x-www-form-urlencoded`

| Field | Required | Description |
|---|---|---|
| `source` | yes | URL of the page with a backlink. |
| `target` | yes | URL being linked to. Must match `PUBLIC_TARGET_ORIGIN`. |

Response: 202 Accepted (async processing).

Errors: 400 (invalid URL, origin mismatch, source=target), 503 (backlog full).

---

## Admin

All admin endpoints need `Authorization: Bearer <ADMIN_TOKEN>` (constant-time comparison).

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
      "created_at": "2026-07-03 17:00:00"
    }
  ]
}
```

Errors: 401 (no/invalid token).

---

### POST /api/admin/moderate

Approve, spam, or delete a comment.

`Content-Type: application/json`

```json
{ "id": 42, "action": "approved" }
```

Valid actions: `approved`, `spam`, `deleted`.

Response (200): `{ "id": 42, "status": "approved" }`

Errors: 400 (invalid action), 401 (no/invalid token), 404 (not found).

---

## Health

### GET /healthz

Returns 200 with body `ok`.

---

## CORS

Public endpoints (`/api/comment`, `/api/webmention`, `/api/comments`) return `Access-Control-Allow-Origin` only for the configured `ALLOWED_CORS_ORIGIN`. Admin endpoints don't advertise CORS.