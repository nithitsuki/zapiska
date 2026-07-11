# Specification

A self-hosted comment engine in Rust (Axum + SQLite) for the IndieWeb. Handles:

1. Native comment form submissions via a public API.
2. Incoming [W3C Webmentions](https://www.w3.org/TR/webmention/) from other IndieWeb sites.
3. Public read API (JSON, CORS-enabled) for embedding approved comments on `nithitsuki.com` via a client-side `<script>`.
4. Bearer-token-protected admin endpoints for manual moderation.

Runs locally behind a reverse proxy at `https://webmention.nithitsuki.com`. The main site it serves is `https://nithitsuki.com`; webmention `target` URLs must reference the main site.

---

## 1. Architecture

The app is async. HTTP handlers are decoupled from heavy outbound work (webmention fetches) via an mpsc channel.

```
                ┌─────────────────────────────────┐
                │         AXUM WEB ROUTER         │
                │  (tower layers: body-limit,    │
                │   CORS, rate-limit, tracing)    │
                └───┬──────────┬──────────┬───────┘
                    │          │          │
   POST /api/comment    POST /api/webmention   GET /api/comments
   (native form)         (federated ping)       (public read)
        │                     │                     │
        ▼                     ▼                     ▼
  ┌────────────┐       ┌──────────────┐       ┌──────────────┐
  │ Validate + │       │ Validate     │       │ SQLite read  │
  │ ammonia    │       │ target origin│       │ (spawn_blk)  │
  │ sanitize   │       │ enqueue to   │       └──────┬───────┘
  │ content    │       │ mpsc channel │              │
  └─────┬──────┘       └──────┬───────┘              │
        │                     │ 202 Accepted         │
        │                     ▼                      │
        │             ┌──────────────────────┐       │
        │             │ Tokio async worker   │       │
        │             │  - SSRF-safe fetch   │       │
        │             │  - verify backlink   │       │
        │             │  - parse h-entry     │       │
        │             │  - parse h-card      │       │
        │             │  - GitHub enrichment │       │
        │             │    (cached API)      │       │
        │             └──────────┬───────────┘       │
        │                        │                   │
        ▼                        ▼                   ▼
   ┌──────────────────────────────────────────────────────┐
   │           SQLite Database Store (comments.db)        │
   │   comments.status: 'pending' (default)               │
   │   github_profiles cache (30-day TTL)                 │
   └──────────────────────────┬───────────────────────────┘
                              │
              POST /api/admin/moderate  +  GET /api/admin/pending
              (Bearer ADMIN_TOKEN)         (Bearer ADMIN_TOKEN)
                              │
                              ▼
   ┌──────────────────────────────────────────────────────┐
   │           Public Read API (/api/comments)            │
   │           - returns status='approved' only           │
   └──────────────────────────────────────────────────────┘
```

### Async rules

- All SQLite ops go through `spawn_blocking` — disk I/O never blocks the async runtime.
- Webmention worker uses a bounded `tokio::sync::mpsc::channel`. HTTP handler pushes a `(source, target)` job and returns `202` immediately.
- Backlog capped at 64 (default). Overflow gets `503` instead of unbounded buffering.
- Graceful shutdown drains the queue and waits for in-flight fetches before exit.

---

### Feature flags

Webmentions are optional at compile time. Building without them strips the webmention endpoint, background worker, SSRF guards, and microformats parser:

```sh
cargo build --release --no-default-features --features comments
```

| Feature | Default | Description |
|---|---|---|
| `comments` | on | Native comment submission, threaded read API, admin API, embed widget |
| `webmentions` | on | W3C webmention ingress, background worker, h-entry parsing, SSRF protection, avatar favicon extraction |

---

## 2. Configuration

Everything comes from environment variables at startup. No secrets in code.

| Variable | Default | Description |
|---|---|---|---|
| `ADMIN_TOKEN` | *required, no default* | Bearer token for `/api/admin/*`. Loaded once; compared in constant time. |
| `BIND_ADDR` | `127.0.0.1:3000` | Listen address. Localhost-only because we sit behind a reverse proxy. |
| `PUBLIC_TARGET_ORIGIN` | `https://nithitsuki.com` | The canonical origin of the main site this server rates mentions for. `target` must start with this. |
| `ALLOWED_CORS_ORIGIN` | `https://nithitsuki.com` | Single origin the read API is callable from. Reflected verbatim in `Access-Control-Allow-Origin`. |
| `DATABASE_PATH` | `./comments.db` | SQLite file path. |
| `GITHUB_TOKEN` | *(optional)* | Personal access token (minimal scope). Optional — raises anonymous GitHub API limit from 60/hr to 5000/hr. Strongly recommended. |
| `MAX_CONTENT_LEN` | `2000` | Max `content` length in chars. |
| `MAX_AUTHOR_LEN` | `100` | Max `author_name` length in chars. |
| `MAX_BODY_SIZE` | `8192` | Per-request body limit bytes (tower). |
| `FETCH_TIMEOUT_MS` | `4000` | Outbound HTTP budget for source/GitHub fetches. |
| `WORKER_BACKLOG` | `64` | Bounded mpsc capacity; overflow returns `503`. |
| `HONEYPOT_FIELD` | `website` | Name of the honeypot form field. When non-empty, the submission is stored with `honeypot = 1` (flagged, not discarded). |
| `STORE_IP_ADDRESS` | `false` | Store submitter IPs in the database for spam analysis. When enabled, IPs are SHA-256 hashed before storage — raw IPs never touch disk. |
| `IP_HASH_SECRET` | *(unset)* | Optional salt mixed into the IP hash to prevent rainbow table attacks. Only used when `STORE_IP_ADDRESS=true`. |
| `MAX_COMMENTS_PER_IP_PER_DAY` | `50` | Per-IP daily native comment cap. `0` = unlimited. |
| `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR` | `10` | Per-source-domain hourly webmention cap. `0` = unlimited. |
| `MODERATION_WEBHOOK_URL` | *(unset)* | URL for external moderation webhook. POSTs full enriched payload on each submission. |
| `MODERATION_WEBHOOK_MODE` | `async` | `async` = fire-and-forget; `sync` = wait for response, apply returned `action`. |
| `DEFAULT_COMMENT_STATUS` | `pending` | Initial moderation status. `pending` = requires review; `approved` = auto-publish. |
| `MAX_THREAD_DEPTH` | `0` | Maximum nesting depth for replies. `0` = nesting disabled. Clamped 0-10. |
| `RUST_LOG` | `info` | `tracing` filter directive. |

Startup fails fast if `ADMIN_TOKEN` is unset.

---

## 3. Dependencies

Crates in `[dependencies]`:

| Crate | Purpose |
|---|---|
| `axum` | HTTP router, layers. |
| `tokio` (full) | Async runtime, mpsc, spawn_blocking, signal handling. |
| `tower` / `tower-http` | Body limit, CORS, trace layers. |
| `tower_governor` | Per-IP rate limiting on ingestion routes. |
| `rusqlite` | SQLite driver. |
| `r2d2` + `r2d2_sqlite` | Connection pool for `spawn_blocking` SQLite ops. |
| `reqwest` (rustls, no default features + `https`) | Outbound fetches for webmention source + GitHub API. |
| `ammonia` | HTML sanitizer for stored content. |
| `scraper` | CSS-selector parsing for microformats2 (`h-entry`, `h-card`). |
| `url` | URL parsing/validation. |
| `serde` + `serde_json` | (De)serialization. |
| `tracing` + `tracing-subscriber` | Structured logging. |
| `subtle` | Constant-time admin token compare. |

---

## 4. Database Schema

A SQLite file (`comments.db`) with two tables.

### 4.1 `comments`

| Field | Type | Modifiers | Description |
|---|---|---|---|
| `id` | INTEGER | PRIMARY KEY AUTOINCREMENT | Row id. |
| `target_path` | TEXT | NOT NULL | Destination local path on the main site (e.g., `/blog/hello`). Always starts with `/`, no `//`, no control chars, ≤1024 chars. |
| `comment_type` | TEXT | NOT NULL CHECK IN (`native`,`webmention`) | Ingestion vector. |
| `source_url` | TEXT | NULLABLE | For webmentions: the remote `source` URL. NULL for native. |
| `author_name` | TEXT | NOT NULL | Cleaned name. ≤`MAX_AUTHOR_LEN`. |
| `author_url` | TEXT | NULLABLE | Absolute `http(s)` URL (validated via `url` crate) or NULL. |
| `author_avatar` | TEXT | NULLABLE | Absolute `http(s)` URL or NULL. |
| `content` | TEXT | NOT NULL | Ammonia-sanitized HTML. ≤`MAX_CONTENT_LEN` after sanitization. |
| `status` | TEXT | NOT NULL DEFAULT `pending` CHECK IN (`pending`,`approved`,`spam`,`deleted`) | Moderation status. |
| `created_at` | TEXT | NOT NULL DEFAULT (datetime 'now') | RFC3339 / ISO8601 stamp. |
| `updated_at` | TEXT | NOT NULL DEFAULT (datetime 'now') | Updated when row is re-processed (webmention update) or moderated. |
| `parent_id` | INTEGER | NULLABLE REFERENCES comments(id) | ID of the parent comment for threaded replies. NULL = top-level. |
| `depth` | INTEGER | NOT NULL DEFAULT 0 | Nesting depth (0 = top-level, max 4). |
| `honeypot` | INTEGER | NOT NULL DEFAULT 0 | 1 if the honeypot anti-spam field was triggered. |
| `delete_token` | TEXT | NULLABLE | Random token for self-service comment deletion. |
| `submitter_ip` | TEXT | NULLABLE | SHA-256 hash of the submitter IP address (prefix `h:` + 64 hex chars). Only stored when `STORE_IP_ADDRESS` is enabled. |
| `content_hash` | TEXT | NULLABLE | SipHash of normalized content (for duplicate detection). Prefix `h:` + 16 hex chars. |

Indexes:
- `CREATE INDEX idx_comments_read ON comments(target_path, status, created_at);` — hot read path.
- `CREATE UNIQUE INDEX idx_comments_source_target ON comments(source_url, target_path) WHERE source_url IS NOT NULL;` — webmention idempotency lookup.
- `CREATE INDEX idx_comments_parent ON comments(parent_id);` — reply lookups.

### 4.2 `webmention_seen` (idempotency / deletion handling)

Lightweight ledger of *processed* webmention pings so duplicate re-sends and deletions are handled correctly.

| Field | Type | Modifiers | Description |
|---|---|---|---|
| `source` | TEXT | NOT NULL | Source URL. |
| `target` | TEXT | NOT NULL | Target URL. |
| `last_seen_at` | TEXT | NOT NULL | Last successful ping timestamp. |
| `last_status` | TEXT | NOT NULL | `alive` or `gone` (410 / no backlink). |
| PRIMARY KEY (`source`, `target`) | | | Natural composite key. |

Lookup order on each ping: if `(source,target)` exists with `last_status='alive'`, this is an *update* — overwrite the matching `comments` row (status preserved if already `approved`/`spam`). If `last_status='gone'`, set the matching comment's status to `deleted`.

### 4.3 `comment_urls` (extracted URLs)

URLs extracted from comment content at store time. Enables cross-comment tracking without maintaining your own database.

| Field | Type | Modifiers | Description |
|---|---|---|---|
| `id` | INTEGER | PRIMARY KEY AUTOINCREMENT | Row id. |
| `comment_id` | INTEGER | NOT NULL REFERENCES comments(id) | Parent comment. |
| `url` | TEXT | NOT NULL | Normalized URL (lowercase, no fragment). |
| `domain` | TEXT | NOT NULL | Hostname extracted from URL. |
| `url_hash` | TEXT | NOT NULL | SipHash of normalized URL (prefix `h:`). |

Indexes: `idx_comment_urls_comment`, `idx_comment_urls_domain`, `idx_comment_urls_hash`.

### 4.4 `github_profiles` (30-day cache)

| Field | Type | Modifiers | Description |
|---|---|---|---|
| `login` | TEXT | PRIMARY KEY | Lowercased GitHub username. |
| `name` | TEXT | NULLABLE | `name` from GitHub API, fallback `login`. |
| `avatar_url` | TEXT | NOT NULL | `avatar_url` from GitHub API. |
| `cached_at` | TEXT | NOT NULL | Insert/refresh timestamp. |
| `valid` | INTEGER | NOT NULL | 1 if user exists, 0 if 404 (negative cache, 1h TTL). |

A row is fresh for 30 days (`valid=1`) or 1 hour (`valid=0`); otherwise re-fetched.

### 4.5 SQLite pragmas (set on every pool connection)

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;
```

Schema is idempotent (embedded SQL via `include_str!`).

---

## 5. Endpoints

### 5.1 `POST /api/comment`

`Content-Type: application/x-www-form-urlencoded`. Rate-limited per-IP (5 req / 60s). Body limit `MAX_BODY_SIZE`.

Form fields:

```
target_path=/guestbook
author_name=Bob Vance
author_url=https://vancerefrigeration.com   (optional)
github_username=bobvance                    (optional)
parent_id=42                               (optional, for threaded replies)
content=Terrific project setup!
```

Processing rules:

1. Validate field lengths: `content` ≤ `MAX_CONTENT_LEN`, `author_name` ≤ `MAX_AUTHOR_LEN`, `target_path` ≤ 1024.
2. Validate `target_path`: must start with `/`, must not contain `//`, must not contain control chars (`\x00`-`\x1F`).
3. If `author_url` present: parse with `url` crate; reject unless scheme is `http`/`https` and host is non-empty.
4. If `parent_id` present: parent must exist, be approved, on the same `target_path`, and at depth < `MAX_THREAD_DEPTH`. Depth is set to `parent.depth + 1`. If `MAX_THREAD_DEPTH` is 0, nesting is disabled entirely.
5. Normalise & sanitise:
   - `content` → `ammonia::clean` (default policy). Result truncated to `MAX_CONTENT_LEN`.
   - `author_name` → strip control chars, trim whitespace.
6. Author resolution (priority order):
   1. If `github_username`: look up `github_profiles` cache; on miss/refresh fetch `GET https://api.github.com/users/<login>` (with `User-Agent:` header, `FETCH_TIMEOUT_MS` timeout, optional `Authorization: Bearer <GITHUB_TOKEN>`). On `200` store `name ?? login` and `avatar_url`. On `404`, negative-cache `valid=0` for 1h, set `author_name` to the form's `author_name`.
   2. Avatar resolution (separate from name, priority order):
      1. If `author_url` is a GitHub profile URL → GitHub API avatar.
      2. (webmentions feature) Fetch author's page → h-card photo → favicon parse.
      3. icon.horse favicon service.
      4. GitHub username fallback (if provided).
      5. `/embed/default-avatar.jpg` (served locally, not hotlinked).
7. Honeypot check: if the honeypot field is non-empty, `honeypot` flag is set to `1` on the stored comment. The moderation system decides what to do.
8. A `delete_token` is generated for every submission and returned in the response body: `{ "delete_token": "a1b2c3d4e5f6g7h8" }`.
9. Insert with `status='pending'`, `comment_type='native'`.
10. Respond `201 Created` with `{ "delete_token": "..." }`. On validation failure respond `400 Bad Request` with a JSON `{ "error": "..." }` body. **Never echo raw user input back.**

#### `POST /api/comment/{id}/delete`

Self-service comment deletion. Requires the `delete_token` returned from the original submission.

`Content-Type: application/json`

```json
{ "token": "a1b2c3d4e5f6g7h8" }
```

If the token matches the stored `delete_token` for the comment, the status is set to `deleted`. The endpoint is rate-limited to prevent brute-forcing tokens.

Response (200): `{ "success": true }`

Errors: 404 (comment not found or token doesn't match, returned as a single case to avoid leaking valid IDs).

### 5.2 `POST /api/webmention`

`Content-Type: application/x-www-form-urlencoded`. Rate-limited per-IP (30 req / 60s — gentler than native since pings come from other sites). Body limit `MAX_BODY_SIZE`.

Form fields:

```
source=https://alice.blog/hello-world
target=https://nithitsuki.com/blog/hello
```

Processing rules:

1. Parse `source` and `target` with `url` crate. Reject (`400`) if either fails to parse or is not an absolute `http(s)` URL.
2. Reject (`400`) if `target` does not start with `${PUBLIC_TARGET_ORIGIN}`.
3. Reject (`400`) if `source` and `target` are equal (per spec).
4. Enqueue `(source, target)` onto the mpsc channel. If the channel is full, respond `503 Service Unavailable` so the remote sender retries.
5. Respond `202 Accepted` immediately. Body must not be required to process the response per spec.

Background worker (per job, all inside async worker task):

1. **Idempotency+ deletion check** against `webmention_seen`:
   - If `(source,target)` has `last_status='gone'` → look up existing `comments` row by `(source_url,target_path)`; if `approved`, set `status='deleted'`. Re-respond internally as handled; skip fetch.
2. **SSRF-safe fetch** of `source` (see §6.1):
   - Resolve hostname (do **not** redirect-block here — reqwest follows redirects, but every resolved IP at each hop must pass the private-range check).
   - GET with `FETCH_TIMEOUT_MS`, max 5 redirects, `User-Agent: webmention.nithitsuki.com`.
   - On `410 Gone` → record `webmention_seen.last_status='gone'`, set existing approved comment to `deleted`. Done.
   - On other fetch failure (timeout, 4xx/5xx, blocked IP) → record `webmention_seen.last_status` unchanged; drop job silently (logged at `warn`).
3. **Backlink verification (W3C requirement):** parse the fetched HTML with `scraper`; search for any `<a>`, `<link>`, or `<area>` whose `href` (resolved absolute) equals `target`. If none found → reject the ping: write `webmention_seen.last_status='gone'` only if the source previously existed; do **not** store a comment. Drop the job.
4. **h-entry parse** (`scraper`): find the first `.h-entry` (or `.hentry` legacy).
   - `content`: `.e-content` HTML → `ammonia::clean → trunc to `MAX_CONTENT_LEN`. If none, take `.p-summary`; if none, take `.p-name`. If absolutely nothing, use a placeholder "Mentioned this page." string.
   - `author`: `.p-author` (may be a nested `.h-card` or a plain string). Resolve `author_name` = `.p-name` (or text). `author_url` = `.u-url`. `author_avatar` = `.u-photo`. If `.p-author` is a plain URL/`u-url`, fetch its `h-card` once (same SSRF-safe fetch). If nothing found, fallback `author_name` = registrable domain of `source`, `author_url` = `source`, `author_avatar` = `https://icon.horse/<domain>`.
5. **Idempotent upsert**: look up `comments` by `(source_url,target_path)`. If exists: overwrite `content`/`author_*` (status preserved — already-approved stays approved). If missing: insert new row with `status='pending'` (manual review required on first sighting).
6. Update `webmention_seen` with `last_seen_at=now`, `last_status='alive'`.
7. All errors are logged with `tracing`; no response is sent (the `202` was already returned).

`target_path` is derived from `target` by stripping `${PUBLIC_TARGET_ORIGIN}` (leaving `"/"` if root).

### 5.3 `GET /api/comments`

`Content-Type: application/json`. CORS: single origin, never `*`. Rate-limited per-IP (60 req / 60s).

Query params:

- `path` (required): e.g., `/blog/hello`. Returns `400` if missing/invalid (`/`-prefix, no `//`).
- `limit` (optional, default `50`, max `100`).
- `before` (optional): return rows with `id < before` (cursor pagination).

Response (`200 OK`):

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
      "content": "<p>Mentioned your page…</p>",
      "created_at": "2026-07-03T16:40:00Z",
      "parent_id": null,
      "depth": 0
    }
  ]
}
```

The embed widget builds a thread tree client-side from the flat `parent_id` list. Top-level comments sorted newest-first, replies sorted oldest-first within each parent.

`total` is the count of `approved` comments for `path` (cached/cheap; reorder against `comments` is fine).

Also handle `OPTIONS` preflight for the CORS route.

### 5.4 Admin endpoints

All admin endpoints need either `Authorization: Bearer <ADMIN_TOKEN>` (header) or `admin_token=<ADMIN_TOKEN>` (cookie, set via `POST /api/admin/login`). Token comparison is constant-time (`subtle::ConstantTimeEq`), runs even on malformed headers. On mismatch: `401`.

See [docs/api.md](docs/api.md) for the full admin API reference, including cookie auth, single-comment lookup with parent chain, batch moderation, and IP filtering.

#### `POST /api/admin/login`

Exchange a token for an `HttpOnly` session cookie (30-day expiry). Body: `{ "token": "..." }`. Response: `{ "success": true }` + `Set-Cookie` header.

#### `POST /api/admin/logout`

Clear the session cookie.

#### `GET /api/admin/pending`

Pending comments, newest-first. Supports `limit`, `before`, `path` filters. Returns full comment data including `parent_id`, `depth`, `honeypot`, `submitter_ip`.

#### `GET /api/admin/comments`

List comments by status: `pending`, `approved`, `spam`, `deleted`, or `all`. Supports `limit`, `before`, `path`, and `ip` (filter by submitter IP address). Designed for moderation engine integration.

#### `GET /api/admin/comments/{id}`

Fetch a single comment with its full ancestor chain. Returns `{ "comment": {...}, "parents": [...] }` where `parents` is ordered from immediate parent up to the root. Lets a moderation engine see the full thread context.

#### `POST /api/admin/moderate`

Approve, spam, delete, or revert a comment. Body: `{ "id": 42, "action": "approved" }`. Valid actions: `approved`, `spam`, `deleted`, `pending`. Responds `{ "id": 42, "status": "approved" }`.

#### `POST /api/admin/moderate/batch`

Moderate multiple comments in one request. Body: `{ "actions": [{ "id": 1, "action": "approved" }, ...] }`. Returns per-item results with individual errors. Designed for automated moderation pipelines.

---

## 6. Security

### 6.1 SSRF

Before any outbound request (and at every redirect hop), resolve the hostname and check every resolved IP. Reject if any IP falls in:

- IPv4: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10` (CGNAT), `127.0.0.0/8`, `169.254.0.0/16` (link-local — includes AWS metadata `169.254.169.254`), `172.16.0.0/12`, `192.0.0.0/24`, `192.0.2.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `240.0.0.0/4`.
- IPv6: `::1/128`, `fc00::/7` (unique-local), `fe80::/10` (link-local), `::ffff:0:0/96` (IPv4-mapped — re-check against the IPv4 list).
- String hosts `localhost`, `*.local`, `*.internal`, `*.localhost` are rejected outright.

Store the parsed IP set per request; **do not** re-resolve between validation and connect (DNS rebinding mitigation). Implement by constructing `reqwest` with a custom `redirect::Policy` that re-runs the validator on each `Location`; reject on a forbidden target by returning a redirect-stopping error.

Reject requests whose host **fails to resolve** (`400` / drop job). Fail closed.

### 6.2 Outbound timeouts

Single process-wide `reqwest::Client` built with:

- `timeout(Duration::from_millis(FETCH_TIMEOUT_MS))`,
- `connect_timeout(...)` (2s),
- `redirect::Policy::limited(5) + custom SSRF policy`,
- `user_agent("webmention.nithitsuki.com (+https://webmention.nithitsuki.com)")`,
- `https_only(true)` (TLS-only; reject plaintext remote sources — prevents trivial MITM tampering of parsed author data),
- rustls TLS backend (no OpenSSL).

### 6.3 SQLite concurrency

All SQL is dispatched via `spawn_blocking` against an `r2d2` pool. WAL mode allows concurrent reads while a moderation write is in flight. `busy_timeout=5000ms` absorbs brief writer contention.

### 6.4 XSS mitigation

- `content` is always `ammonia::clean`-ed before storage, regardless of source (native form or h-entry HTML). Default ammonia policy (strips scripts, event handlers, `style`, `iframe`, etc.) is the baseline; **do not** widen it.
- `author_name`, `author_url`, `author_avatar`, `target_path` are stored as text and **always rendered as escaped text** in the JSON API — the API returns them as plain JSON strings (no HTML), so client-side renders must escape on display. Document this contract.

### 6.5 Rate limiting & anti-spam

- `tower_governor` (in-memory, per-IP, token-bucket or fixed-window) on `/api/comment` (5 req / 60s) and `/api/webmention` (30 req / 60s). No external store — acceptable for local hosting; restart clears buckets.
- `GET /api/*` gets a liberal per-IP limit; admin routes have none (they're token-gated).
- **Per-IP daily cap**: `MAX_COMMENTS_PER_IP_PER_DAY` (default 50). Tracks native comment submissions per IP per day in an in-memory limiter. Returns `429 Retry-After: 86400` when exceeded. Catches 1/min steady spammers.
- **Per-domain hourly webmention cap**: `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR` (default 10). Tracks webmentions per source domain per hour. Returns `429 Retry-After: 3600` when exceeded.
- **Honeypot**: A hidden form field (`HONEYPOT_FIELD`, default `"website"`) that bots auto-fill. When non-empty, the submission is stored with `honeypot = 1` but not discarded — the moderation system decides what to do.

### 6.6 Body size & parsing

- Global `tower::limit::RequestBodyLimitLayer(MAX_BODY_SIZE)`.
- URL-decode form bodies with a strict decoder (reject NULs, reject overlong sequences).
- All inputs trimmed of trailing whitespace; control characters stripped from text fields.

### 6.7 Admin auth

- `ADMIN_TOKEN` loaded once at startup into a `String` held by the router state.
- Compared with `subtle::ConstantTimeEq` against the incoming header bytes (base64/utf-8). A missing/malformed header still triggers a comparison against a fixed placeholder to keep timing constant.
- All `/api/admin/*` routes are **not** advertised in CORS preflight (`Access-Control-Allow-Methods` lists only `GET, OPTIONS`) — those routes are server-side only.

### 6.8 No path-traversal / host-injection in `target_path`

`target_path` from native form and from webmention `target` derivation both pass through:

1. Must begin with `/`.
2. Must not contain `//`, `..`, control chars, `\`.
3. Length ≤ 1024.
Reject otherwise (`400` for native; drop+log for webmention).

---

## 7. Microformats Parsing Detail

Worker HTML inspection using `scraper`:

```
[fetch source HTML] -> [verify backlink to target exists]
                          │
            no: drop ping (log; record 'gone' only if previously 'alive')
            yes:
              v
        [find .h-entry (or .hentry)]
                          │
        ┌─────────────────┼──────────────────────┐
        ▼                 ▼                        ▼
  .e-content (HTML)   .p-name (title)        .p-author
  ammonia clean      fallback content       │
                                            ▼
                                ┌───────────────────────┐
                                │ nested .h-card?      │
                                │ yes: .p-name,         │
                                │      .u-url, .u-photo │
                                │ no:  plain text       │
                                │      (name only)      │
                                └───────────────────────┘
                                            │
                                  fallback (domain of source):
                                    author_name = registrable domain
                                    author_url  = source
                                    author_avatar = https://icon.horse/<domain>
```

If `content` after sanitization is empty, use a stable placeholder: `"Mentioned this page."`. Never store an empty content (`NOT NULL`).

---

## 8. Embedding on nithitsuki.com (client side)

`nithitsuki.com` loads a small `<script id="nc-comments" data-path="/blog/hello" ...></script>` near where comments should appear. The script (served by you, hosted on `nithitsuki.com` static:

```
GET https://webmention.nithitsuki.com/api/comments?path=<data-path>&limit=50
```

The CORS header `Access-Control-Allow-Origin: https://nithitsuki.com` (single, fixed) is required for the fetch to succeed from the main site's origin. Preflight (`OPTIONS`) is supported and cached (`Access-Control-Max-Age: 600`).

The script renders the JSON client-side. Two **security contracts** that must hold:

1. The script MUST treat `content` as sanitized HTML (it is already ammonia-cleaned server-side). Insert via `innerHTML`.
2. The script MUST treat `author_name`, `author_url`, `author_avatar` as plain text / attribute values (escape on insert). Never put these into `innerHTML`.

These two contracts together mean even a malicious source URL cannot break out of the avatar/name rendering.

The native comment **form** on `nithitsuki.com` also posts to `https://webmention.nithitsuki.com/api/comment`; that route is also CORS-public for `POST` (add `POST` to `Access-Control-Allow-Methods` on `/api/comment`), with explicit preflight.

`webmention.nithitsuki.com` additionally advertises its endpoint to the IndieWeb by exposing (out of scope of this server's code, but documented here) on `nithitsuki.com`:

```html
<link rel="webmention" href="https://webmention.nithitsuki.com/api/webmention" />
```

---

## 9. Logging & Diagnostics

- `tracing` with `tracing-subscriber` JSON formatter.
- Spans: per-request (`method`, `path`, `peer_ip`, `status`, `latency_ms`).
- Worker jobs emit at `info` for successful upserts, `warn` for fetch failures / blocked SSRF attempts / 410 deletions, `error` for DB failures.
- Never log raw `content`, `author_url`, or `ADMIN_TOKEN` (token absent from logs entirely). `target_path` and `id` are safe.
- Startup logs the resolved config (with `ADMIN_TOKEN` redacted) at `info`.

---

## 10. Graceful Shutdown

- `tokio::signal::ctrl_c()` + `SIGTERM` (via `tokio::signal::unix`).
- On signal: stop accepting new HTTP connections (Axum's `with_graceful_shutdown`), drain the mpsc channel to completion, wait up to `FETCH_TIMEOUT_MS × 2` for in-flight worker fetches, close the SQLite pool.
- In-flight worker errors during shutdown are logged; **already-queued** jobs persist? No — they live only in memory; document that a crash between `202` and worker commit may drop the ping. Acceptable per W3C (sender may retry).

---

## 11. Error Response Conventions

All public endpoints return JSON error bodies:

```json
{ "error": "human-readable reason", "code": "rate_limited" }
```

Status codes in use:
- `201` (native ingest ok), `202` (webmention accepted), `200` (read / moderate ok).
- `400` (validation failure, bad target, missing field).
- `401` (admin token missing/wrong).
- `404` (moderate id not found).
- `429` (rate limited — include `Retry-After` header).
- `500` (unexpected internal error — generic message; details in logs only).
- `503` (worker backlog full).

---

## 12. Out of Scope (explicitly)

- Outbound/send webmentions (this server only *receives* pings).
- Authentication for comment *authors* (no login; manual approval is the gate).
- Multi-user / multi-tenant (single site: `nithitsuki.com`).
- Image upload / media hosting (avatars are remote URLs only).
- WebSub / Salmention (updates to already-approved mentions are re-processed but not pushed onward).
- Apple News / Atom feed ingestion.