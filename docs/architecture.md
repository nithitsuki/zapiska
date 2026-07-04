# Architecture

Single Rust binary, three layers:

```
HTTP ingress (Axum + tower middleware)
        |
        +---> Public read path (GET /api/comments)
        +---> Native comment ingest (POST /api/comment)
        +---> Webmention ingest (POST /api/webmention) --[mpsc]--> Worker
        +---> Admin moderation (GET /api/admin/*, auth middleware)
        |
SQLite (r2d2 pool, WAL mode, all access via spawn_blocking)
```

## Modules

```
src/
  config.rs          Environment config (12 vars)
  error.rs           AppError enum + IntoResponse (6 HTTP status variants)
  state.rs           AppState: config, pool, repo, github, wm_sender, http_client
  sanitize.rs        HTML sanitization via ammonia
  validate.rs        Input validation: target_path, URLs, control chars
  ssrf.rs            Private-IP blocklist, DNS resolve+check, registrable domain
  github.rs          GitHubLookup trait + RealGitHub (cached API calls)
  mf2.rs             Microformats2 parser (h-entry, h-card, e-content)
  worker.rs          Webmention worker pipeline (fetch → verify → parse → upsert)
  openapi.rs         OpenAPI 3.1 spec (utoipa)
  db/
    pool.rs          r2d2 SQLite pool + pragmas + migrations
    repo.rs          CommentsRepo: all SQL via tokio::spawn_blocking
  http/
    mod.rs           Router builder + middleware stack
    layers.rs        CORS, body limit, rate-limit
    shutdown.rs      SIGTERM/SIGINT handler
    comments_read.rs GET /api/comments
    comment_post.rs  POST /api/comment
    webmention_post.rs POST /api/webmention
    admin.rs         Admin auth + pending/moderate handlers
    reqwest_client.rs SSRF-safe reqwest client builder
    test_support.rs  Shared test helpers
```

## Data flow

### Native comment
1. POST form data to `/api/comment`.
2. Validates fields (target_path, author_url, content length).
3. Content sanitized with ammonia.
4. Author resolved: GitHub API (if `github_username`), icon.horse favicon (if `author_url`), or plain name.
5. Row inserted with `status = 'pending'`.
6. Admin approves/spams/deletes via `/api/admin/*`.

### Webmention
1. POST `source` and `target` to `/api/webmention`.
2. Validates both are absolute http(s), `target` origin matches `PUBLIC_TARGET_ORIGIN`.
3. Job enqueued via `tokio::sync::mpsc`; returns 202 immediately.
4. Background worker fetches source via SSRF-safe client.
5. Verifies source HTML links to `target` (backlink check).
6. Parses `h-entry` microformat for content + author.
7. Upserts comment idempotently via `ON CONFLICT`.
8. `webmention_seen` tracks alive/gone for deletion handling.
9. Source returns 410 → comment marked deleted.

## Database

Three tables in one SQLite file:
- **comments** — native + webmention entries, status workflow (pending → approved/spam/deleted)
- **webmention_seen** — idempotency + deletion tracking
- **github_profiles** — 30-day cache for GitHub lookups

See `migrations/schema.sql` for the full schema.

## Middleware stack

Outer to inner:
1. `RequestBodyLimitLayer` — rejects bodies over `MAX_BODY_SIZE` (413).
2. `CorsLayer` — single allowed origin, GET/POST/OPTIONS, 10-min preflight cache.
3. `tower_governor` rate limiting per route:
   - Native comments: 5 req / 60s
   - Webmentions: 30 req / 60s
   - Public read: 60 req / 60s
   - Admin: unlimited (auth-gated)
4. Admin routes: `admin_auth` middleware with constant-time bearer token check.

## Error handling

All public endpoints return JSON:

```json
{ "error": "human-readable reason", "code": "rate_limited" }
```

Status codes: 200, 201, 202, 400, 401, 404, 429 (with `Retry-After`), 500, 503.

The repo layer uses `RepoError` (`Internal`, `NotFound`) with a `From` impl to `AppError`. Keeps DB decoupled from HTTP.