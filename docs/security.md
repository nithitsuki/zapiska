# Security

zapiska accepts untrusted form data, JSON data, URLs, and HTML. The server
stores comment data and later sends approved data to browsers and feeds.

## Content safety

Native and webmention content passes through `ammonia` before storage. The
default policy removes scripts, event handlers, style attributes, iframes,
objects, and unsafe URL schemes.

The widget sanitizes content again before it uses `innerHTML`. It writes author
names and URLs as text or attribute values. A custom frontend must follow the
same rule.

## Server-side request forgery

All outbound fetches of untrusted URLs — webmention sources and native
comment author pages (avatar lookup) — go through `SafeFetcher`
(`src/fetch.rs`), the single guarded door. Before and during a fetch, it:

1. Rejects non-HTTP(S) schemes and hostless URLs.
2. Rejects `localhost`, `.local`, `.internal`, and `.localhost` host names
   (trailing-dot forms like `localhost.` included).
3. Resolves the host name and rejects blocked IPv4 and IPv6 ranges.
4. Follows redirects manually with redirect-following disabled, re-resolving
   and re-checking **every** redirect target before connecting.
5. Fails closed past 5 redirect hops (`TooManyRedirects`).
6. Caps bodies at 1 MiB, enforced while streaming (a `Content-Length`
   pre-check plus a chunked-read limit — oversized bodies never
   materialize), and parses the survivor exactly once.

Any refusal (blocked target, redirect loop, oversized body, `410 Gone`,
network error) is a typed `FetchError`; avatar lookup maps every refusal to
the dicebear fallback, and the worker maps `Gone` to the seen-ledger delete
path. The shared HTTP client is only for operator-configured endpoints
(GitHub API, moderation webhooks, notifications, Turnstile) and must never
fetch untrusted URLs directly.

The blocked ranges include private, loopback, link-local, CGNAT,
documentation (including TEST-NET-2/3), benchmark, unspecified, and
IPv4-mapped IPv6 ranges.

The fetcher permits HTTP and HTTPS with a ten second connection timeout.
Webmention fetches use the configured `FETCH_TIMEOUT_MS` (threaded from the
worker spawn args into every job); author-page avatar fetches use the 4 s
default, matching the default configuration.

Do not treat the fetcher as a complete defence against DNS rebinding:
resolution happens before connect without address pinning, so a hostile DNS
name that flips between the check and the connection can still slip
through. IPv6 translation ranges (NAT64, 6to4, Teredo with embedded private
IPv4) are likewise not yet re-checked — see the follow-up note in
`src/ssrf.rs`.

## Admin authentication

Protected `/api/admin/*` routes accept either of these credentials:

- `Authorization: Bearer <ADMIN_TOKEN>`
- `__Host-admin_token=<ADMIN_TOKEN>` session cookie

The token comparison uses `subtle::ConstantTimeEq`. The login and logout routes
are outside the protected admin route group. The login route exchanges the
token for a 30 day cookie.

The session cookie is `__Host-admin_token` with `Path=/`, `HttpOnly`,
`Secure`, and `SameSite=Lax`. The `__Host-` prefix tells browsers to reject
the cookie over plain HTTP, so a network observer on an unencrypted fetch
cannot capture the token. Terminate TLS at the reverse proxy and redirect
HTTP to HTTPS so the cookie is only ever set and sent over HTTPS.

Login attempts are throttled per IP on the same budget as single-comment
moderation (same burst and window values, separate buckets): rapid guessing
against a weak token trips `429` before the handler runs. Batch moderation
(both comment and reaction batches) and the full-database export share that
budget too, so a leaked token cannot be used to bulk-modify or dump the
database at line rate.

The token is loaded at startup and is redacted in the startup configuration
log. Do not put the token in source control.

## Rate limits

Client identity comes from one seam (`src/http/peer.rs`): the TCP peer
address, normalized so IPv4-mapped IPv6 (`::ffff:1.2.3.4`) canonicalizes to
IPv4. Governors, the in-memory limiter, and IP hashes all consume that
identity, so one client never gets two buckets.

By default the server does not trust `X-Forwarded-For`, `X-Real-IP`, or
`Forwarded` — spoofed headers are ignored. Set `TRUST_PROXY=true` only when
a reverse proxy you control overwrites those headers; the server then reads
the leftmost `X-Forwarded-For` entry, else `X-Real-IP`, else the first
`Forwarded for=`, else the peer. Behind a proxy without `TRUST_PROXY`,
every visitor shares the proxy's address and one quota.

Upgrade note: IPv4-mapped peers (`::ffff:a.b.c.d`, seen on non-default
dual-stack binds) now key and hash as plain IPv4. The in-memory per-IP
daily caps reset on the upgrade restart (as on any restart). Anyone-mode
reaction identifiers stored under the old mapped hash no longer match —
affected users re-react and the orphaned rows stay. With
`STORE_IP_ADDRESS=true`, new `submitter_ip_hash` rows use the normalized
form while history keeps the mapped form. No code migration is provided:
this bites only non-default dual-stack binds combined with anyone-mode
reactions or IP storage.

| Route group | Default burst | Default window | Sustained |
|---|---:|---:|---:|
| Native comment submission, deletion, and reactions | 100 | 60 seconds | 1.67/s |
| Webmention ingress | 60 | 60 seconds | 1.00/s |
| Public comments and RSS | 300 | 60 seconds | 5.00/s |
| Single comment moderation | 30 | 60 seconds | 0.50/s |
| Login, batch moderation, reaction moderation, export (own bucket each) | 30 | 60 seconds | 0.50/s |

<!-- RATE-LIMITS: native=100/60 webmention=60/60 read=300/60 admin=30/60 -->

The sustained column is the rate the governor enforces after the burst is
spent (`burst / window` per second). A config-level test re-derives this
table from the code defaults, so the numbers cannot drift.

The native comment, deletion, and reaction routes share the native limit. The
comments and RSS routes share the read limit. The single moderation route has
the admin moderation limit. The login, batch moderation, single reaction
moderation, reaction batch moderation, and export routes share those same
admin moderation values with their own per-route buckets.
Other admin routes do not have this governor.

Both the governor 429s and the handler-side quota 429s return the documented
JSON shape (`{"error", "code": "rate_limited"}`) with a `Retry-After`
header. See [API](api.md).

The in-memory limiter also applies these caps:

- `MAX_COMMENTS_PER_IP_PER_DAY` limits native comments.
- `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR` limits source domains.

Process restart clears the in-memory counters.

## Delete tokens

The server creates a 16 character hexadecimal delete token for each native
comment. The current generator uses a standard library hash of the peer
address, time, and process counter. It is not a cryptographic token.

The deletion route uses the native rate limit and returns the same `404` result
for a missing comment and a wrong token. A future release should replace the
token generator with a cryptographically secure random source.

## Turnstile

When `TURNSTILE_ENABLED=true`, native comments need a valid
`cf-turnstile-response` value.

The server sends the token and the peer IP to the configured siteverify URL.
The server never sends the secret to the browser.

The server rejects an invalid token with `400` and code `turnstile_failed`.
If siteverify is not reachable, the server returns `503`. In both cases, the
comment is not stored.

## Request limits

The global request body limit is `MAX_BODY_SIZE`, with a default of 8192 bytes.
The import route has a separate 16 MiB limit.

## IP data

When `STORE_IP_ADDRESS=false`, comment rows do not store IP data.

When `STORE_IP_ADDRESS=true`, comment rows store both:

- `submitter_ip`, the raw peer IP used by admin lookup.
- `submitter_ip_hash`, a deterministic SHA-256 hash used for identity checks.

Set `IP_HASH_SECRET` to add a salt to the hash. The rate limiter still keeps the
peer IP in memory.

If you enable IP storage, restrict database access and state this choice in
your privacy notice.

## Cross-origin requests

`ALLOWED_CORS_ORIGIN` accepts one origin, a comma-separated list, or `*`.

CORS applies to the public router. It includes public comments, native comment
submission, reactions, RSS, health, the embed script, Swagger, login, and
logout. Protected admin routes are outside this layer.

The configured-origin layer advertises `GET`, `POST`, and `OPTIONS`. It does
not advertise `DELETE`. The wildcard layer also omits a preflight cache value.

## SQL safety

Most repository queries use SQLite parameters. The author lookup still builds
its filter conditions with escaped string values. It is not the same as a
parameterized query and remains a refactor item.

## Input validation

The server checks these values:

- `target_path` starts with `/`, has no `//`, `..`, backslash, or control code,
  and has a maximum length of 1024 characters.
- `author_url` is an absolute HTTP or HTTPS URL with a host.
- `author_name` has control codes removed, leading and trailing spaces removed,
  and a maximum length of `MAX_AUTHOR_LEN`.
- `content` is sanitized and limited to `MAX_CONTENT_LEN` characters.
- `comment_type` and `status` use SQLite check constraints.
- A webmention target has the same parsed origin as `PUBLIC_TARGET_ORIGIN`.

The native handler reads `website` as the honeypot field. The
`HONEYPOT_FIELD` setting is loaded but does not change that field name.

## Import safety

The admin import route accepts only export version `1`.

Imported comments are checked for ID, path, type, status, name, URL, parent
ordering, and depth. Content is sanitized again. A failed comment row is
skipped and counted. URL rows for missing comments are skipped.

Imported reactions are checked for ID, comment ID, value length, status, and
identifier length. The current reaction import can still return an error when
the database rejects a row. Keep export files private.

## URL extraction

Native comment URL extraction reads the original form content. It does not read
the sanitized or truncated content.

The extractor accepts only case-insensitive, double-quoted `href="..."` values
with absolute HTTP or HTTPS URLs. It removes fragments, lowercases the result,
and removes duplicate hashes. It scans the original bytes, so Unicode case
conversion cannot shift a slice offset. It does not inspect webmention content.

## Logging

The server logs structured events through `tracing`.

The server does not log comment content or the admin token.
Request spans can include the peer IP.
Webhook URLs can appear in warning logs and startup configuration output.
Do not put credentials in webhook URL query values.

Review log access when IP storage or external notifications are enabled.

## Notifications

Comment author names and content are attacker-controlled. The Telegram and
Slack formatters escape `&`, `<`, and `>`. The Discord formatter also breaks
`@everyone` and `@here` with a zero-width space and backslash-escapes Discord
markdown, so a comment cannot ping the admin channel or reformat the alert.
Each Discord payload stays within the 2000-character channel limit. Previews
and name lists shrink first, and the moderation footer is kept through every
shrink stage. Only the last-resort cut, reached when the unshrunk fields
alone exceed the limit, can remove it.

## Security testing

The pentest suite checks XSS, unsafe URL schemes, XML injection, SQL injection
payloads, path traversal, CRLF and NUL input, and reaction payloads.

Run it with:

```sh
cargo test --test pentest
```

See [Development](development.md) for the complete test commands.
