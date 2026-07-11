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

### Admin API auth

All admin endpoints require either an `Authorization: Bearer <token>` header or an `admin_token` cookie, compared against `ADMIN_TOKEN` with `subtle::ConstantTimeEq`. Both values zero-padded to the same length. Token never logged — config display redacts it. The admin API is designed for external moderation engines and automation scripts.

### Rate limiting

Per-IP via `tower_governor` keyed on socket peer address:

| Route | Limit (env vars) | Default |
|---|---|---|
| POST /api/comment | `RATE_LIMIT_NATIVE` / `RATE_LIMIT_NATIVE_WINDOW` | 50 per 60s |
| POST /api/webmention | `RATE_LIMIT_WEBMENTION` / `RATE_LIMIT_WEBMENTION_WINDOW` | 30 per 60s |
| GET /api/comments | `RATE_LIMIT_READ` / `RATE_LIMIT_READ_WINDOW` | 60 per 60s |
| Admin | `RATE_LIMIT_ADMIN_MODERATE` / `RATE_LIMIT_ADMIN_MODERATE_WINDOW` | 10 per 60s |

Keyed on TCP peer IP, not `X-Forwarded-For`, so spoofed headers can't reset the bucket. Burst and window are configurable through environment variables.

### Cloudflare Turnstile (optional)

When `TURNSTILE_ENABLED=true`, native comment submissions must include a `cf-turnstile-response` token from the Cloudflare widget. zapiska:

- Verifies the token against `TURNSTILE_VERIFY_URL` (default Cloudflare siteverify) on the server. **Never** calls siteverify from the browser; the widget secret is held by the backend only.
- Forwards the submitter's IP as `remoteip` so Cloudflare can apply its own abuse signals.
- **Fails closed**: if siteverify returns `success: false` (invalid/expired/replayed token), the comment is rejected with `400 { "code": "turnstile_failed" }` and **not stored**. If siteverify is unreachable (timeout, network error), zapiska returns `503` and the comment is also not stored.
- Never logs the secret. The token itself is not logged (it's transient). The widget's public **sitekey** is safe to expose in HTML; the **secret** is loaded once at startup from `TURNSTILE_SECRET_KEY` and held in memory.

The form field name (`cf-turnstile-response`) matches the widget's automatic hidden input, so no manual hidden input is required.

### Body size

`RequestBodyLimitLayer` rejects bodies over `MAX_BODY_SIZE` (default 8192 bytes) with 413.

### Submitter IP privacy

When `STORE_IP_ADDRESS=true`, submitter IPs are **SHA-256 hashed** before they're written to the SQLite database — the raw IP never touches disk. The hash is deterministic (same IP always produces the same hash), so it can still be used for spam reputation, but the original address cannot be recovered from the stored data.

An optional `IP_HASH_SECRET` salt can be set to prevent rainbow table attacks. When set, the salt is mixed into the hash — even if the secret is later compromised, previously stored hashes cannot be reversed.

The in-memory rate limiter (`tower_governor` and per-IP daily caps) still operates on raw IPs, which are only held in RAM and are never persisted.

### CORS

`ALLOWED_CORS_ORIGIN` accepts a single origin, comma-separated list, or `*`.
`Access-Control-Allow-Origin` is set when the request's `Origin` matches any
entry in the list. Admin routes not advertised in preflight.

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