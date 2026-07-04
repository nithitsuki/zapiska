# Security

## Threat surface

Four public endpoints accept untrusted input that gets persisted to SQLite and later served to browsers. Two admin endpoints control comment visibility.

## Controls

### XSS

All comment content goes through [ammonia](https://crates.io/crates/ammonia) before storage. Default policy strips `<script>`, `<style>`, `<iframe>`, `<object>`, event handlers, `javascript:` URLs, and `style` attributes. Keeps `<a>`, `<p>`, `<code>`, `<em>`, `<strong>`, `<blockquote>`, etc.

The client-side embed script (`embed/comments.js`) provides a second sanitization layer before `innerHTML`. Author metadata (name, URL, avatar) is inserted as text or attribute values, never HTML.

### SSRF

The webmention worker fetches arbitrary URLs. Protections:

1. **DNS + IP blocklist** — resolves hostname, checks every IP against: all private ranges, CGNAT, link-local (including AWS metadata `169.254.169.254`), IPv4-mapped IPv6 rechecked against IPv4 list. Hostnames `localhost`, `*.local`, `*.internal`, `*.localhost` blocked by string match.
2. **Custom redirect policy** — re-checks each redirect hop.
3. **HTTPS-only** — `https_only(true)` on the reqwest client.
4. **Timeout** — 4s default via `FETCH_TIMEOUT_MS`.

The GitHub enrichment service uses the same shared client with `redirect::Policy::none()`.

### Admin auth

Middleware extracts `Authorization: Bearer <token>`, compares against `ADMIN_TOKEN` with `subtle::ConstantTimeEq`. Both values zero-padded to the same length. Missing header still goes through the comparison (against empty string). Token never logged — config display redacts it.

### Rate limiting

Per-IP via `tower_governor` keyed on socket peer address:

| Route | Limit | Burst |
|---|---|---|
| POST /api/comment | 5 / 60s | 5 |
| POST /api/webmention | 30 / 60s | 30 |
| GET /api/comments | 60 / 60s | 60 |
| Admin | unlimited | (auth-gated) |

Keyed on TCP peer IP, not `X-Forwarded-For`, so spoofed headers can't reset the bucket.

### Body size

`RequestBodyLimitLayer` rejects bodies over `MAX_BODY_SIZE` (default 8192 bytes) with 413.

### CORS

Single origin (`ALLOWED_CORS_ORIGIN`, default `https://nithitsuki.com`). `Access-Control-Allow-Origin` only present when request's `Origin` matches. Admin routes not advertised in preflight.

### SQL injection

All queries use parameterized `params![]` macro. No string interpolation.

### Input validation

- `target_path`: starts with `/`, no `//`, `..`, `\`, control chars, max 1024 chars.
- `author_url`: absolute http/https with a host.
- `author_name`: control chars stripped, whitespace trimmed, empty rejected.
- `content`: truncated to `MAX_CONTENT_LEN` after sanitization.
- Schema `CHECK` constraints enforce `comment_type` and `status` values.
- Webmention `target` matched via `Url::origin()`, not string prefix (prevents `nithitsuki.com.evil.com` bypasses).

### Idempotency

Webmentions upsert on `(source_url, target_path)` — re-sends update content but preserve moderation status. A source returning 410 marks the comment `deleted`. The `webmention_seen` table tracks alive/gone per `(source, target)` pair.