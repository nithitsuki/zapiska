# Architecture

Single Rust binary, three layers:

```
HTTP ingress (Axum + tower middleware)
        |
        +---> Public read path (GET /api/comments)
        +---> Native comment ingest (POST /api/comment)
        +---> Webmention ingest (POST /api/webmention) --[mpsc]--> Worker (optional)
        +---> Admin endpoints (GET/POST /api/admin/*, cookie + bearer auth)
        |
SQLite (r2d2 pool, WAL mode, all access via spawn_blocking)
```

## Modules

```
src/
  config.rs          Environment config (19 vars)
  error.rs           AppError enum + IntoResponse + Display (6 HTTP status variants)
  state.rs           AppState: config, pool, repo, github, wm_sender, http_client
  sanitize.rs        HTML sanitization via ammonia
  validate.rs        Input validation: target_path, URLs, control chars
  ssrf.rs            Private-IP blocklist, DNS resolve+check (webmentions only)
  github.rs          GitHubLookup trait + RealGitHub (cached API calls)
  mf2.rs             Microformats2 parser — h-entry, h-card (webmentions only)
  worker.rs          Webmention worker pipeline — fetch → verify → parse → upsert
  openapi.rs         OpenAPI 3.1 spec, feature-gated for webmention paths
  db/
    pool.rs          r2d2 SQLite pool + pragmas + migrations
    repo/
      mod.rs         Data types (Comment, NewComment), CommentsRepo core, row_to_comment
      comments.rs    Comment CRUD: insert, list, moderate, get, threaded chain
      webmentions.rs webmention_seen operations
      github_profiles.rs GitHub profile cache operations
  http/
    mod.rs           Router builder + middleware stack
    layers.rs        CORS, body limit, rate-limit configs
    shutdown.rs      SIGTERM/SIGINT handler
    comments_read.rs GET /api/comments
    comment_post.rs  POST /api/comment — includes parent_id validation for threading
    webmention_post.rs  POST /api/webmention (webmentions only)
    admin.rs         Admin auth middleware + all admin endpoints
    reqwest_client.rs  SSRF-safe reqwest client builder (webmentions only)
    test_support.rs  Shared test helpers
```

Module visibility is feature-gated: `mf2`, `ssrf`, `worker`, `webmention_post`, `reqwest_client` are only compiled when the `webmentions` feature is enabled.

## Data flow

### Native comment
1. POST form data to `/api/comment`.
2. Validates fields (target_path, author_url, content length, parent_id if present).
3. Content sanitized with ammonia.
4. Author resolved: GitHub API (if `github_username`), h-card/favicon/DiceBear (if `author_url`), or plain name.
5. If `parent_id` provided: parent must exist, be approved, on the same path, and depth < 4. Child depth = parent depth + 1.
6. Row inserted with `status = 'pending'`.
7. Admin approves/spams/deletes via `/api/admin/*`.

### Threaded replies
- Top-level comments have `parent_id = null`, `depth = 0`.
- Replies store the parent's ID and compute `depth = parent.depth + 1`.
- Max nesting: 4 levels (depth 0–4).
- The public API returns a flat list with `parent_id`; the embed widget builds the thread tree client-side.
- Top-level sorted newest-first, replies sorted oldest-first within each parent.

### Webmention
1. POST `source` and `target` to `/api/webmention`.
2. Validates both are absolute http(s), `target` origin matches `PUBLIC_TARGET_ORIGIN`.
3. Job enqueued via `tokio::sync::mpsc`; returns 202 immediately.
4. Background worker fetches source via SSRF-safe client.
5. Verifies source HTML links to `target` (backlink check).
6. Parses `h-entry` microformat for content + author.
7. Upserts comment idempotently via `ON CONFLICT`. Webmentions are always top-level (parent_id = null, depth = 0).
8. `webmention_seen` tracks alive/gone for deletion handling.
9. Source returns 410 → comment marked deleted.

## Database

One SQLite file, three tables:

- **comments** — native + webmention entries, status workflow (pending → approved/spam/deleted), parent_id and depth for threaded replies
- **webmention_seen** — idempotency + deletion tracking
- **github_profiles** — 30-day cache for GitHub lookups

See `migrations/schema.sql` for the full schema.

## Middleware stack

Outer to inner:
1. `RequestBodyLimitLayer` — rejects bodies over `MAX_BODY_SIZE` (413).
2. `CorsLayer` — single allowed origin, GET/POST/OPTIONS, 10-min preflight cache.
3. `tower_governor` rate limiting per route (all configurable via env vars — see [`deployment.md`](deployment.md)):
   - Native comments: 5 req / 60s
   - Webmentions: 30 req / 60s
   - Public read: 60 req / 60s
   - Admin: unlimited (auth-gated)
4. Admin routes: `admin_auth` middleware — checks `Authorization: Bearer` header then `admin_token` cookie with constant-time comparison. The admin API is the interface for external moderation systems.

## Error handling

All endpoints return JSON:

```json
{ "error": "human-readable reason", "code": "rate_limited" }
```

Status codes: 200, 201, 202, 400, 401, 404, 429 (with `Retry-After`), 500, 503.

The repo layer uses `RepoError` (`Internal`, `NotFound`) with a `From` impl to `AppError`. Keeps DB decoupled from HTTP.

## Feature flags

See [deployment.md](deployment.md) for compile-time feature configuration.
