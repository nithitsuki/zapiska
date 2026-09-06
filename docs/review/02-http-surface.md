# Review: HTTP surface & middleware

Scope: router composition (`src/http/mod.rs`), layers (`layers.rs`), public handlers
(`comments_read.rs`, `feed.rs`, `comment_post.rs`, `reactions.rs`, `webmention_post.rs`),
admin surface (`admin/mod.rs`, `admin/auth.rs`, `admin/data.rs`, `admin/moderate.rs`,
`admin/lookup.rs`), state (`state.rs` Limiter), shutdown (`shutdown.rs`, `main.rs`),
config rate-limit/CORS/session parts, `embed/comments.js`, and the spec/docs
(`SPEC.md`, `docs/security.md`, `docs/api.md`, `docs/architecture.md`, `docs/deployment.md`).

## What I read

| File | What it is | Notes |
|---|---|---|
| `src/http/mod.rs` (1051 lines) | Router composition + health/version/embed handlers | Recent diff added `/api/version` + tests. Composition is three merged blocks with ordering-sensitive layer calls. |
| `src/http/layers.rs` (95) | CORS, body-limit, governor builders | Four near-identical governor constructors; `PeerIpKeyExtractor` everywhere. |
| `src/http/comments_read.rs` (130) | `GET /api/comments` | Cursor/sort handling, reaction counts. |
| `src/http/feed.rs` (370) | RSS 2.0, hand-rolled XML + RFC822 | Self-contained, well unit-tested; duplicates the 1..=100 cap with the read API. |
| `src/http/shutdown.rs` (25) | ctrl-C + SIGTERM future | Pure signal plumbing; all shutdown *policy* lives elsewhere/none. |
| `src/http/comment_post.rs` (1324) | `POST /api/comment`, delete | Fat handler: ~300 lines of orchestration, mis-numbered step comments. |
| `src/http/reactions.rs` (623) | Reaction add/remove | Reuses `request_has_admin_token` directly (good seam). |
| `src/http/webmention_post.rs` (249) | Webmention ingress | Duplicates the full 45-field `Config` literal in its backlog test. |
| `src/http/admin/mod.rs` (1198) | Admin facade + `validate_token` | Re-export hub; `#[cfg(test)]` duplicated at lines 36–37. |
| `src/http/admin/auth.rs` (107) | Login/logout + `admin_auth` middleware | Cookie has no `Secure`; login has no rate limit. |
| `src/http/admin/data.rs` | Export/import | 16 MiB import cap; export is an unthrottled full-DB dump. |
| `src/http/admin/moderate.rs`, `lookup.rs` | Moderation, lookup | Batch moderate has no governor. |
| `src/state.rs` (133) | In-memory `Limiter` | Fixed-window daily/hourly caps, checked *inside* handlers. |
| `src/config.rs` (rate-limit/CORS/admin parts) | Config | Defaults bumped to 100/60/300/30 in the recent diff. |
| `src/main.rs` (127) | Server bootstrap | `with_graceful_shutdown`; no drain of worker/notifier. |
| `docs/*`, `SPEC.md` | Spec + docs | Docs still claim rate-limit defaults 50/30/60/10 (stale). |
| `embed/comments.js` (495) | Widget | Server-sanitized content, `innerHTML` after DOMPurify/regex fallback. |

Direct quotes worth keeping: `docs/security.md:50-51` ("Rate limits use the TCP peer
address. The server does not trust `X-Forwarded-For`.") and `docs/architecture.md:236-241`
(shutdown does not drain the webmention queue).

## Friction

### F1. `http/mod.rs` — the router is a shallow, ordering-sensitive sandwich

The composition root mixes three concerns that each live fragmented:

- **Policy-in-plumbing.** Which endpoint gets which governor, which gets a body
  limit, and in what order is expressed as raw tower layer calls repeated per
  route: `/api/comment`, `/api/comment/{id}/delete`, `/api/comment/{id}/reaction`
  (×2 methods), `/api/comments`, `/feed.xml` (mod.rs:109-148), webmention
  (mod.rs:151-158), plus admin moderate (mod.rs:66-68) and import (mod.rs:89-91).
  There is no route-group concept — "the native write group" means "any route
  whose definition I remembered to clone the `.layer(body_limit).layer(GovernorLayer{...})`
  pair onto". Adding a route that should be rate-limited requires the author to
  know the pattern exists and replicate it.
- **Ordering invariants live in comments, not structure.** The line
  `router.layer(cors).merge(admin_routes)` (mod.rs:163) is the only thing keeping
  CORS off the protected admin routes; the invariant "cors must be layered before
  the admin merge" is guarded solely by the comment at mod.rs:160-162 (which cites
  "SPEC §6.7" — the SPEC has no numbered sections, so the reference itself is stale).
  Reorder those two lines and every admin route answers preflight; no test pins the
  merge order except `cors_does_not_apply_to_admin_routes` (mod.rs:1027), which only
  catches a *regression*, not a mis-edited future composition.
- **The `::<_, Infallible>` turbofish.** `RequestBodyLimitLayer` needs its inner
  error type annotated at every application site (mod.rs:112, 120, 154). The layer's
  generic signature leaks into the router; the "recipe" for a public POST is
  "body limit layer + governor layer + type annotation", and a test must know it too
  (the 429 test at mod.rs:449-459 rebuilds the stack by hand instead of exercising
  `build_app` with a tightened config).
- **Two different body-limit mechanisms in one router.** Public form routes use
  `RequestBodyLimitLayer` (8 KB); import uses `DefaultBodyLimit::max(16 MiB)`
  (mod.rs:89-91). Both are "body limits" but they're different APIs with different
  behaviors (a layer vs an extractor limit), and the 8 KB-vs-16 MiB split is a
  one-off exception rather than a group property.

Why it hurts: the *what* (policy) and the *how* (tower plumbing) are entangled, so
the callers — future route authors and tests — must internalize ordering and
type-annotation trivia. `cors`/`body_limit`/governor construction is already factored
into `layers.rs` (good seam), but the *application* of those layers is not a module
at all; it's imperative text in `build_app`.

Deletion test: delete `layers.rs` and `build_app` still compiles only after importing
the constructors from somewhere else — the layer-*application* policy (which group,
what order) is inseparable from the router text. The shallow part is the repeated
per-route recipe; it should be a reuseable unit ("the native write group").

### F2. One rate-limiting concept, two implementations, no seam

There are two per-IP rate limiters in the codebase with the same audience but
different owners:

| | `tower_governor` | in-memory `Limiter` |
|---|---|---|
| Where | layer, `layers.rs:59-94` + per-route in `mod.rs` | inside handlers, `state.rs:59-84` |
| Semantics | GCRA token bucket | fixed window (day / hour) |
| Keyed by | `PeerIpKeyExtractor` (peer socket) | `ip_daily_key` / `domain_hourly_key` (peer socket) |
| Consumed by | 8 public routes | `comment_post.rs:114-129`, `webmention_post.rs:44-60` |

The near-identical key-extraction + reject-error boilerplate is duplicated in the
two handlers (comment_post.rs:115-129 vs webmention_post.rs:44-60): build key, call
`check_and_increment`, map `false` to `AppError::RateLimited` with the *right*
`retry_after_secs`. Neither handler knows about the other's convention; there is no
"rate-limit this request" interface a handler can call that would make the quota
semantics a property of one module.

Worse, the **meanings of the knobs disagree**:

- `Limiter` caps are honest fixed windows: 50 comments per IP per *day*,
  10 webmentions per domain per *hour*.
- The governor knobs are named `*_burst` + `*_window_secs` (config.rs:62-76), but
  `layers.rs:60-64` feeds the *window* value into `GovernorConfigBuilder::per_second(...)`.
  With defaults (100 burst, 60 window) the server actually implements a token bucket
  of **100 burst refilled at 60 tokens/second** — i.e. sustained ~60 req/s, not
  "100 per 60 seconds" as every doc table (architecture.md:209-216, security.md:53-58)
  and the config names promise. The read default (300 burst, window 60) likewise
  permits **300 immediately then 60/s sustained (~3 600 requests/minute)** — not
  300/minute. In every case the "window" value silently becomes the per-second
  refill rate; an operator tuning `RATE_LIMIT_READ_WINDOW` to raise the "window" is
  unknowingly raising the *per-second* refill rate. This is a genuine semantics lie
  in a security control's interface.

Why it hurts: two modules own the word "rate limit" with incompatible semantics;
changing IP handling (e.g., adding trusted-proxy XFF) touches four places
(governor key extractor, both handlers' `ConnectInfo` use, `ip_hash`, the Limiter
keys) — there is no single "client identity" concept to fix.

Deletion test: delete `state.rs::Limiter` and its two call sites recover instantly —
the real work (policy, key format, error mapping) lives in the handlers. Deleting
`layers.rs` leaves the governor policy fragmented across mod.rs route literals.
Both fail the test for the same reason: the depth is in the wrong modules.

### F3. `comment_post.rs::create_comment` — the fat handler (business logic behind a 300-line function)

The native comment handler is the whole submission pipeline in one function
(comment_post.rs:55-354): honeypot → Turnstile → per-IP daily cap → validation →
sanitize → language gate → GitHub/GitHub-avatar resolution (outbound HTTP) →
parent/depth resolution → delete-token generation → IP-storage decision → insert →
auto-approve → URL extraction → notification → moderation webhook (sync **or**
async, with payload building, submitter stats, and parent-chain queries inline at
comment_post.rs:271-304). The step comments are mis-numbered (…4, 4, 6, 5, "6",
and three different "9"s at lines 236/241/247).

Why it hurts:
- **The seam that should exist doesn't.** A `SubmissionService`/`submit(Submission)`
  with the policy inside and the handler as a thin adapter would let the pipeline be
  reasoned about and tested without HTTP (currently each branch is only reachable
  through `oneshot` tests with form bodies — see the 30+ integration tests in
  comment_post.rs:593-1324).
- **Ordering is behavioral law hidden in a function body.** The daily-IP-cap check
  runs *before* validation/sanitization (comment_post.rs:114-129), so rejected
  submissions still consume quota — deliberate, and yes, also how honeypot comments
  are already counted before moderation. Nothing at the interface documents this.
- **Two nearly identical webhook decision loops** exist (comments
  comment_post.rs:305-345 vs reactions reactions.rs:91-134) — sync/async handling,
  allowed-action matching, status application — duplicated instead of shared via the
  `webhook.rs` seam (which only covers the fire-and-forget POST).

Deletion test: delete the handler and the entire submission policy disappears with
it — the policy has no other home. That's backwards: the policy should outlive the
HTTP entry point.

### F4. `admin/` — policy split across the facade and the router

- All admin *rate-limiting policy* lives outside the admin module, in `http/mod.rs`:
  only `/api/admin/moderate` gets a governor (mod.rs:66-68); **batch moderate, all
  moderation GETs, export, import, lookup, and reaction moderation have no governor**
  (consistent with security.md:62's admission). Adding a hot admin endpoint from
  `admin/comments.rs` cannot accidentally buy a rate limit; the router must be
  touched. `moderate_batch` (mod.rs:70-73) accepts an unbounded `actions` array — a
  single request can churn every comment row (authed-only, so low risk, but the cap
  asymmetry vs the single route is odd).
- `admin/mod.rs` is a re-export hub (admin/mod.rs:11-23) + `validate_token`.
  `validate_token` is the deep part and is correctly shared by the middleware and
  login (auth.rs:41-60) — good. The friction is that the *session* policy (cookie
  flags, max-age) lives in `auth.rs:set_cookie_value` while the *route* policy
  (which routes are public vs protected, which are CORS'd, which are limited) lives
  in `mod.rs`; the facade gives you a place to put "the admin route group" as a
  concept, and it isn't used for that.
- Cosmetic but real: duplicated `#[cfg(test)]` at admin/mod.rs:36-37.
- `login`/`logout` sit on the public router (mod.rs:107-108) and therefore *do* get
  CORS (documented in security.md:117-119 — consistent, but it means the login
  endpoint answers cross-origin preflight and echoes the allowed origin; with
  `ALLOWED_CORS_ORIGIN=*` any website can POST login and read the
  success/failure oracle).

Deletion test: delete `admin/mod.rs` and the re-exports vanish but so does
`validate_token` — the hub is a fine facade. The actually-deletable-without-loss
thing is the *rate-limit coverage asymmetry*: nothing in `admin/` encodes "this
endpoint is throttled", so the guarantee is invisible and un-testable from the
module side.

### F5. `shutdown.rs` — a shallow module whose policy is absent

17 lines of signal plumbing (correct), but the graceful-shutdown *behavior* —
drain the webmention worker queue, flush notification windows, bound in-flight
drain time — lives nowhere. `main.rs:120-126` asks for graceful shutdown and the
worker task is simply dropped when `main` returns (worker.rs:49-87 loop ends when
the channel closes). `docs/architecture.md:236-241` and `SPEC.md:430-434` both
document the gap. This module passes the deletion test (it's pure plumbing) — the
point is that the module *for shutdown policy* doesn't exist.

### F6. `feed.rs` vs `comments_read.rs` — locality of the read-API policy

Both clamp to 1..=100 / default 50, but express it twice: `feed.rs:21-23` (`MAX_ITEMS`)
with a comment "matches the read API cap", and an inline `.clamp(1, 100)` in
comments_read.rs:81. The dependency between the two is a comment, not a shared
constant. The rfc822/XML code in feed.rs is otherwise a well-sealed deep module
(good unit tests at feed.rs:260-369). The friction is small: one shared
`read_limit()` helper (or a `ReadQuery`-style shared struct) would make the
coupling structural.

### F7. `test_support.rs` — the Config literal is the real interface, and it's drifting

`test_state()` hand-builds a 45-field `Config` (test_support.rs:57-103) because no
`Config::default()` exists. The same literal is duplicated again in
webmention_post.rs:181-227. The recent default bump (config.rs:316-325) changed the
real defaults to 100/60/300/30 but **test_support.rs:82-89 still hardcodes the old
50/30/60/10**, so every rate-limit test runs against defaults that no longer match
production defaults — and nothing fails, because the drift is silent. That is the
locality cost of an un-defaulted config struct: every consumer must own a copy of
"what a default looks like". A `Config::default()` (with `from_env` overriding) would
delete ~90 duplicated lines and make the next default change one-touch.

## Bulletproofing brainstorm

### B1. Reverse proxy → per-IP limits and IP identity collapse to the proxy's address

- Scenario: The documented deployment is nginx/Caddy TLS termination
  (deployment.md:191-224). Every visitor's TCP peer is 127.0.0.1 or the proxy's
  address. `PeerIpKeyExtractor` (layers.rs:63), `ConnectInfo` in handlers
  (comment_post.rs:57), `ip_daily_key` (state.rs:118), `ip_hash`
  (comment_post.rs:203), and Turnstile's remoteip (comment_post.rs:86) all see the
  proxy.
- Worst case: any 50 comments/day across the whole site saturate
  `MAX_COMMENTS_PER_IP_PER_DAY` for *everyone* (a single prolific reader DoSes the
  form for all readers); with `STORE_IP_ADDRESS=true` every comment stores the same
  "submitter IP" — admin "lookup by IP", spam statistics, and reaction identity all
  collapse into one giant bucket.
- Likelihood: high — the docs prescribe exactly this topology. Impact: medium/high.
- Current mitigation: none in code; the choice is documented (security.md:50-51) and
  deployment.md:222-224 warns in prose.
- Gap: the deployment snippet at deployment.md:205-210 even sets `X-Real-IP`, which
  the server silently ignores — an operator following the docs believes identity is
  preserved when it isn't. There is no trusted-proxy escape hatch (no
  `TRUST_PROXY=true` + XFF parsing, no PROXY protocol), and there is no startup
  warning when `X-Forwarded-For`/`X-Real-IP` is present on the first request.
- Worth noting: the docs already chose "don't trust XFF" — that is a *defensible*
  choice for a self-hosted box; the gap is offering no supported path to correct
  identity when the operator does front the app with a proxy it controls.

### B2. XFF/Forwarded header spoofing

- If someone later adds XFF parsing naively (or via a governor key extractor that
  reads headers), any client can spoof `X-Forwarded-For: 1.2.3.4` to burn
  someone else's quota or evade their own. As of today there is no header parsing,
  so there is **no attack** — this is a "keep the invariant" item, and it's the
  flip side of B1. The ratchet is: any future XFF support must be gated on a
  trusted-proxy allowlist/flag; the current security.md:50 note is the guard text.

### B3. IPv4-mapped IPv6 addresses (`::ffff:1.2.3.4`)

- Scenario: operator binds to `[::]` (or the Docker/proxy stack normalizes to
  mapped form). A v4 client arrives as `::ffff:a.b.c.d`. `IpAddr` keeps the mapped
  form.
- Effects: `ip_daily_key` formats differently (`ip:::ffff:...` vs `ip:a.b.c.d:`),
  governor's per-peer buckets differ per family, `ip_hash` output differs, and the
  self-service `delete_token` (hashing the address, comment_post.rs:360-373) differs
  — a client toggling between v4 and v6 stacks gets doubled quota **and loses its
  delete token mid-thread**.
- Likelihood: medium (only when bound to IPv6 wildcard). Impact: low. Current
  mitigation: none; default `BIND_ADDR` is `127.0.0.1:3000`. Gap: canonicalize the
  address once in the (currently missing) client-identity seam via
  `to_ipv4_mapped()`.

### B4. Multi-instance / multi-worker deployments vs in-memory limits

- Scenario: two zapiska processes behind a load balancer (docker replicas) or the
  operator scaling instances.
- Effects: governor buckets and the `Limiter` are per-process, so all quota limits
  multiply by instance count; restarts reset them (documented, security.md:69).
  Two writers on one SQLite WAL file additionally raise `SQLITE_BUSY` risk
  (busy_timeout is 5 s; architecture.md:178).
- Likelihood: low for this project's real deployment; medium if anyone reaches for
  "scale the comments server". Impact: medium (silent limit weakening). Mitigation:
  none. Gap: an explicit "single instance" statement in deployment docs, or moving
  the `Limiter` counts into SQLite if multi-instance ever becomes a goal.

### B5. Rate-limit bypass by IP rotation / botnets

- Any per-IP scheme loses to rotating source addresses. Inherent. The layered
  defenses (honeypot comment_post.rs:63-66, Turnstile comment_post.rs:71-112,
  content-hash dedup) are the real answer and exist. The one thing worth noting:
  an attacker with a big pool can also *burn* others' quota behind a shared proxy
  (see B1) — the two scenarios compound.

### B6. CORS misconfiguration — list vs wildcard, and the login oracle

- List mode (`predicate`, layers.rs:40-52) correctly rejects non-listed origins and
  is unit-tested (mod.rs:354-432). Wildcard (`AllowOrigin::any`) echoes any origin
  and, per security.md:122, skips the preflight cache — both fine for a read/post
  public API.
- The sharp edge is that the **admin login route is under the CORS layer**
  (mod.rs:107-108 + 163; documented in security.md:117-119): with `*`, any webpage
  can `POST /api/admin/login` cross-origin, read `{"success": true/false}` (a token
  oracle + credential-stuffing vector), and any allowed origin can do the same
  against a single-origin config. The cookie itself is protected by SameSite=Lax,
  so cross-site *session takeover* is not the issue — the oracle is.
- Likelihood: low (requires the operator to set `*` or to list a foreign origin).
  Impact: low–medium. Mitigation: the CORS scope comment (mod.rs:160-162) only
  covers the protected group, not login. Gap: split login/logout into the
  no-CORS group (they are server-side session endpoints just like the rest of
  admin), or at minimum document that `*` exposes the login oracle.

### B7. Body-limit bypass via chunked encoding

- `RequestBodyLimitLayer` (tower-http) wraps the body as `Limited`, enforcing on
  **bytes read**, not Content-Length — chunked transfer cannot bypass the 8 KB cap.
  Coverable and safe. The only related soft spot: the reaction POST has no body
  limit layer (mod.rs:126-131) and relies on axum's 2 MB `Json` default — a 2 MB
  JSON `{reaction:...}` is tolerable; not a real bypass.

### B8. Read-API cursor semantics at boundaries

- `before`+`after` both supplied → whichever matches the sort wins; the other is
  silently ignored (comments_read.rs:93-106). Negative cursors → empty page. Fine
  behavior, undocumented.
- Deleted/moderated comments between page fetches shift pages (duplicates or skips),
  which is normal for cursor APIs, but the widget (comments.js:171-194) builds the
  tree from a single page only, so a **deep thread deeper than `limit` appears
  broken** — children whose parent is on an earlier page render as roots.
  `limit` default 50: a thread >50 deep (depth max is 10, but siblings+replies can
  exceed 50 rows) silently orphans replies. A page-size following query
  (`parent_id IN …` depth-limited) would fix the widget; the API is fine.
- Imported IDs (admin import preserves foreign IDs, admin/mod.rs tests, api.md:523)
  interact fine with cursors because cursors are plain `id <`/`id >` cuts.

### B9. RSS feed abuse

- Bounded: `MAX_ITEMS`/`DEFAULT_ITEMS` cap 1..=100 (feed.rs:21-23); no-path =
  global feed across the site (feed.rs:69-77), which is the intended feature and is
  under the read governor (mod.rs:143-148). Worst case is a feed reader polling /
  60s (300/min governor) — negligible. Missing conditional GET (no ETag /
  If-Modified-Since on feed.xml or /api/comments) means readers re-download up to
  100 items per poll; low impact, easy win.

### B10. Graceful shutdown gaps

- The signal future (shutdown.rs) is fine; `with_graceful_shutdown` (main.rs:124)
  drains in-flight requests. Gaps:
  1. **No drain of the webmention worker or notification batcher** — queued jobs
     and open notification windows are dropped when main returns (worker.rs:49-87;
     architecture.md:236-241). A deploy during a webmention burst loses pings.
  2. **No shutdown timeout** — a hung in-flight request (e.g., a sync moderation
     webhook at 10 s timeout, comment_post.rs:310) delays shutdown only briefly,
     but a stalled client connection holds shutdown open indefinitely.
- Likelihood: medium (every deploy). Impact: medium (silent data loss of pings).
  Gap: `tokio::select!` a drain timer (e.g., 5-10 s) around the graceful shutdown,
  and have shutdown close the `wm_sender` and flush the batcher.

### B11. Admin session cookie flags

- `admin_token=…; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000` (auth.rs:28-33)
  — **no `Secure` flag**. The recommended deployment is TLS-terminated nginx
  (deployment.md:193), so in the happy path the cookie is set over HTTPS, but a
  browser will still send a non-Secure cookie over plain `http://` to the same host,
  and the doc's nginx snippet does not force-redirect HTTP→HTTPS. A network
  observer on an unencrypted admin fetch captures the raw admin token (the cookie
  *is* the token, with a 30-day life and no rotation).
- Likelihood: low–medium (needs an http: fetch of the admin dashboard/login or a
  MITM). Impact: high (admin token = full moderation + IP dump + export).
- Gap: add `Secure` (+ optionally `__Host-` prefix = requires `Secure` + `Path=/`
  + no Domain, which all hold). Cheap, high-value.

### B12. Login brute force / admin rate-limit coverage

- `POST /api/admin/login` has **no governor and no per-IP limiter** — a pure
  constant-time compare (auth.rs:26-34) against an operator-chosen token, callable
  at line rate, and CORS-exposed (B6). If `ADMIN_TOKEN` is weak, offline-online
  guessing is unrestricted.
- Admin coverage overall: only `/api/admin/moderate` is governed (mod.rs:66-68);
  batch moderation, all moderation/list GETs, **export**, and import are unthrottled
  (auth-gated only). A leaked or low-entropy token lets an attacker bulk-modify or
  dump the entire database (export includes raw IPs) without any rate governor.
- Likelihood: low (token entropy is the real control) but the *asymmetry* is a
  latent trap. Impact: high. Mitigation: constant-time compare + HttpOnly (good).
  Gap: a governor (or `Limiter` key) on `/api/admin/login` (lockout by IP), and a
  burst limit on batch moderate + export.

### B13. Health endpoint vs Docker healthcheck

- `/healthz` returns a static `"ok"` (mod.rs:192-194) — no DB reachability, no
  pool state. The Docker healthcheck (deployment.md:159) and systemd probe
  (deployment.md:341) will report healthy while SQLite is wedged (e.g., disk full:
  writes fail, healthz still 200). Liveness-only is a legitimate choice; the gap is
  that nothing tells the operator *why* the container stays "healthy" during a
  write failure — a cheap `SELECT 1` (or a `?probe=db`) would close it.
- Likelihood: low. Impact: low–medium.

### B14. Slowloris / no request timeouts

- `axum::serve` over plain TCP with no read/header timeout and no tower-http
  timeout layer (Cargo.toml:33 features are `cors`,`limit`,`trace` — no `timeout`):
  a client that dribbles bytes holds a connection (and an FD, and a body buffer)
  indefinitely.
- Likelihood: medium if the port is exposed without a proxy (default BIND is
  127.0.0.1 — this is most likely behind nginx, which is the believed deployment;
  then nginx owns this). Impact: medium (FD/connection exhaustion).
- Gap: document "must be fronted by a proxy that owns timeouts", or add a
  header-timeout (tower-http `TimeoutLayer` on the outer router).

### B15. HTTP/2 and WebSockets — confirmed absent

- No TLS/ALPN in `axum::serve` (main.rs:120-124) → application-level HTTP/2 is
  not negotiated; h2 terminates at nginx/Caddy (deployment.md:199 `listen 443 ssl
  http2`). Consistent with design.
- No WebSocket handler anywhere in the router surfaces (all routes are GET/POST/
  DELETE JSON/forms); an upgrade attempt gets a normal 4xx. Confirmed none.

### B16. `embed/comments.js` XSS surface

- Server sanitizes with ammonia; the widget sanitizes again with DOMPurify when
  present, else a regex fallback (comments.js:144-157). Author names/URLs render via
  `textContent` (good); content via `innerHTML` *after* sanitization (comments.js:248).
  Residual surfaces:
  1. The regex fallback strips `script`/`iframe`/`on*` but not `javascript:` URLs
     in `href`/`src`, and the avatar `<img src>` (comments.js:213-222) is never
     scheme-checked — server-side validation (`validate_http_url`) should make
     stored data safe, but the widget's `data-api-origin` attribute (comments.js:47-56)
     lets an operator point the widget at an arbitrary server; a malicious origin
     could return `author_url: "javascript:..."` or `author_avatar: "javascript:..."`
     and the fallback sanitizer would pass them through (avatar is a clickable/
     loadable URL). Low likelihood (operator-controlled attribute), but the client
     has no scheme allowlist of its own.
  2. Reactions render via `textContent` (comments.js:257-262) — safe.
  3. `data-link-target` flows into `a.target` unvalidated (comments.js:78, 229) —
     can be `_self` etc.; cosmetic only.
- Impact: low (server-side validation is the fence; the widget fallback is
  defense-in-depth). Gap: client-side scheme check for `author_url`/`author_avatar`
  in the no-DOMPurify path.

### B17. 429 response-shape inconsistency

- Handler-side caps return the documented JSON shape `{"error", "code":
  "rate_limited"}` with a `Retry-After` (error.rs:73-78, 108-114) — matching
  api.md:581-590. Governor-side 429s come from `tower_governor`'s own error and are
  **not** the JSON shape (plain "Too Many Requests"). A client that only follows
  api.md sees two different 429 bodies depending on which limiter fired. The
  body-limit 413 is axum's default text as well. Minor, but it's the API's own
  documented error contract that is inconsistent.

## Candidate deepening opportunities

1. **`http::routes` — one module owning the layer recipe.**
   Seam: `build_app` calls `routes::public_read_routes()` / `routes::native_write_routes()`
   / `routes::admin_routes(state)` instead of raw route literals; matching
   `layers.rs` constructors already provide the building blocks.
   Shape: the "native write group" (comment POST, delete, reaction POST/DELETE:
   shared governor, shared body limit, same order) becomes a single constructor that
   takes `(Router, Governor, BodyLimit)` — the ordering and the `<_, Infallible>`
   turbofish live in exactly one place, inside the module. Adding a new write route
   means one line. The merge/CORS invariant is enforced by `build_app` calling
   `routes::public().with_cors(cors).merge(routes::admin())` — the "admin must not
   inherit CORS" property becomes a function boundary, not a line order.
   Tests that improve: a test that calls the group constructor with a tight governor
   and asserts 429-from-the-real-composition (replacing the hand-rolled stack at
   mod.rs:434-482); a test that asserts *every* admin route lacks `Access-Control-Allow-Origin`
   (iterating the admin route list, not one hand-picked path).

2. **A single client-identity seam, fed by one key extractor.**
   Seam: an `Peer` value extracted once per request (`impl FromRequestParts` or a
   middleware that resolves peer → canonical identity) carrying the *normalized*
   address (IPv4-mapped → v4) and the `Limiter` key in one place; both governors and
   the `Limiter` and `ip_hash` consume it.
   Shape: `ClientIdentity { ip: IpAddr }` with `identity.ip()` — overnight the four
   different "the client's IP" computations (governor extractor, `ConnectInfo` in
   two handlers + reactions, `ip_hash`) collapse to one, and the B1/B3 fixes become
   one-touch (a `TRUST_PROXY` flag flips how the seam resolves the address without
   touching handlers).
   Tests that improve: same client sent as `::ffff:127.0.0.1` and `127.0.0.1` must
   produce the same Limiter key, the same `ip_hash`, the same delete token and the
   same governor bucket; a spoofed `X-Forwarded-For` must not change identity when
   `TRUST_PROXY` is unset.

3. **Rename/clarify the governor knobs (or the constructor).**
   Seam: `layers.rs:governor_config` — the spot where config semantics meet
   governor semantics.
   Shape: change the exported config names to honest ones (`RATE_LIMIT_NATIVE_BURST`
   + `RATE_LIMIT_NATIVE_REFILL_PER_SEC`) — or, cheaper, keep names but make
   `governor_config` take a `(burst, window)` and compute the GCRA refill rate the
   operator actually means (`burst / window` per second minimum), and document the
   real sustained rate next to each default. Either way: the docs table
   (architecture.md:209-216, security.md:53-58) must be re-derived from config
   defaults so the 50/30/60/10→100/60/300/30 drift can't recur.
   Tests that improve: a config-level test asserting "defaults render as their own
   table" (already exists for the new values, config.rs:670-683) — extend it to
   assert the *effective* sustained rate the governor will enforce, so the 
   semantics lie is caught the moment the numbers change.

4. **`CommentSubmissionService` — pull the pipeline out of the handler.**
   Seam: `comment_post.rs::create_comment` becomes a thin adapter; the pipeline
   (validate → sanitize → gates → insert → notify → webhook) becomes
   `submission::submit(&state, Submission) -> SubmissionOutcome`, with ordering
   (IP cap before validate, honeypot counts toward cap) stated in its doc comment
   and covered by direct unit tests instead of only `oneshot` integration tests.
   Shape: `Submission` is a typed struct (the handler's job is only form→struct;
   the service's is policy), and the webhook decision loop is the one shared
   implementation used by both comments and reactions (removing the duplicated
   sync/async loop at comment_post.rs:305-345 vs reactions.rs:91-134).
   Tests that improve: unit tests for ordering (a honeypot submission must consume
   quota and still be stored flagged; an over-quota submission must return 429
   *before* any storage/notification side effect) — currently only provable by
   reading the handler.

5. **`Config::default()` — kill the drift machine.**
   Seam: `config.rs` gains `impl Default for Config` with the same values as the
   env defaults; `from_env` overlays env on top. `test_support.rs:57-103` and the
   duplicated literal in webmention_post.rs:181-227 collapse to
   `Config { admin_token: "test".into(), ..Config::default() }`.
   Shape: the config struct becomes the single source of truth for defaults; adding
   a field or changing a default can no longer silently desync prod vs tests (the
   exact failure mode that produced test_support's stale 50/30/60/10).
   Tests that improve: a loaner test asserting `Config::default()` equals
   `Config::from_env()` with an empty-ish env (currently the `rate_limit_defaults`
   test only covers eight fields by hand).

6. **`update` the docs table + cookie hardening as a bundle (not a module, a guard).**
   Shape: docs/architecture.md:209-216 and security.md:53-58 tables become
   generated-by-verification statements (checked by a doc test or a CI grep against
   config defaults), the admin cookie gains `Secure` (auth.rs:28-33), and the
   startup log warns when the server is bound to a non-loopback address with no
   trust-proxy configuration.
   Tests that improve: `login_sets_cookie` (admin/mod.rs:424-446) asserts `Secure`
   and `__Host-`; a startup test that binds a probe listener on a public
   `0.0.0.0`-style address and asserts the warning is emitted.