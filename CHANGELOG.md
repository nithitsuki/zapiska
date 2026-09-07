# Changelog

All notable changes to zapiska are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/).

## [Unreleased]

### Added

- `GET /api/admin/status`: versions, database health, and non-secret
  configuration (honeypot field, webhook/notify/proxy/reactions/limits) in
  one body for the dashboard OPS tab. Secret values never leave the server.
- `CF-Connecting-IP` is preferred first under `TRUST_PROXY=true`: the
  Cloudflare edge sets it to the single visitor IP, unlike the append-only
  `X-Forwarded-For` whose leftmost entry a client can spoof. Only safe when
  every byte arrives via the trusted edge (direct origin access must be
  impossible).
- Admin dashboard rebuild: comments (exact path totals, per-item batch
  results, true undo, ancestor chains, extracted URLs, duplicate lookup),
  reactions moderation with reviewed-emoji pinning, author/URL lookup,
  export download and import restore with force, and an OPS tab.
- Verified owner identity: owner profile (`GET`/`PUT /api/admin/profile`),
  `POST /api/admin/comments` authoring approved+verified comments with a
  public ✓ checkmark, migration v9 (`verified` column, `admin_profile`
  table), and dashboard PROFILE tab with impostor flagging.
- Versioned SQLite migrations via `PRAGMA user_version`
  (`LATEST_SCHEMA_VERSION = 8` in `src/db/pool.rs`). Fresh databases get the
  canonical snapshot and a stamp. Legacy `user_version = 0` databases run an
  idempotent existence-checked catch-up and are stamped. A database newer
  than the binary refuses startup instead of running against unknown schema.
- `GET /api/version` returning `{ version, schema_version, export_version }`,
  all from their single source of truth. Listed in the OpenAPI document.
- `zapiska --version` / `-V` flag. Prints the crate version without needing
  any configuration (checked before `ADMIN_TOKEN` load).
- Admin export records `ip_hash_salted` (whether `IP_HASH_SECRET` was set).
  The secret itself is never exported.
- Admin import re-derives each comment IP hash from its raw IP with the
  importing server's secret (`ip_hashes_recomputed` in the response) and
  returns a `warning` when the export salt flag mismatches this server while
  salted identities are present.
- Startup warning when the database holds IP hashes but `IP_HASH_SECRET` is
  unset (lost or rotated secret splits IP-hash continuity).
- `DB_QUICK_CHECK` (default `true`): startup runs `PRAGMA quick_check` and
  refuses to start when the database reports corruption, with a message
  pointing at backup restore. Set to `false` only to bypass the gate for
  recovery.
- This `CHANGELOG.md`.
- `AppState::start(config)` / `start_with_github(config, github)` (T22):
  one assembly path for `main` and every test (pool, migrations with
  newer-than-binary refusal, `PRAGMA quick_check` gate, repo, notifier,
  language gate, GitHub adapter, webmention worker). Covered by a temp-DB
  → `healthz` smoke test, a newer-schema refusal test, and
  `IP_HASH_SECRET`-warning tests.
- The webmention worker spawn path now carries the moderation webhook sink
  (T20-F1: URL + signing secret from config via
  `Config::worker_moderation_sink`, shared with the comment/reaction
  paths): production gone-deletes POST a signed
  `comment.status_changed` event, pinned by a prod-wiring test (wiremock on
  the webhook URL asserts exactly one signed emission from a processor
  carrying the shared builder's sink — the spawned worker itself stays
  idle, so the spawn call's argument plumbing is review-only).

### Changed

- CI gains a compose smoke job (build, up, poll `/healthz` to `ok`, check
  `/api/version`, tear down) and per-target release smokes: natively
  runnable Linux binaries boot against a scratch database and must serve
  `/healthz` and `/api/version`, while the `aarch64` binary runs its
  `--version` under `qemu-aarch64` instead of a boot. `docs/development.md`
  describes the new jobs and no longer hardcodes test counts (it gives the
  commands that print them).
- SPEC.md and every guide reconciled with shipped behavior in one sweep:
  the 13-step ingress ordering matches the code (governors and body limits
  live in the route layers, not in `submit`); `TRUST_PROXY` and
  `DB_QUICK_CHECK` join the SPEC config table; shutdown and notification
  sections record the drain instead of loss; the security-rules list covers
  login/batch/export throttles; the API reference records the configured
  honeypot name, 32-character delete tokens, sanitized-content URL
  extraction, the `503` health case, and the full per-section import report
  with overlap refusal; the moderation guide records worker webhook
  emission, JSON 429s, and the real 30-request admin budget; deployment and
  getting-started fill the empty command blocks, repair the GHCR `docker
  run` snippet, qualify the peer-IP statement with `TRUST_PROXY`, and
  document the single-instance lock.

- `GET /healthz` is now a readiness probe: it issues `SELECT 1` through the
  connection pool (bounded to two seconds) and answers `200 ok` when healthy,
  `503 unavailable` when the database does not answer. Docker and compose
  health checks flip unhealthy on database failure instead of staying green.
- The admin session cookie is now `__Host-admin_token` with `Secure`
  (keeping `Path=/`, `HttpOnly`, `SameSite=Lax`). Existing sessions are
  invalidated: log in again after upgrading. Serve the admin dashboard over
  HTTPS so the `Secure` cookie is accepted.
- `POST /api/admin/login`, `POST /api/admin/moderate/batch`,
  `POST /api/admin/reactions/moderate`, `POST /api/admin/reactions/moderate/batch`,
  and `GET /api/admin/export`
  are now throttled per IP on the admin moderation budget (same burst and
  window values, separate per-route buckets). Rapid login guessing and
  bulk dump/modify abuse trip `429`.
- A legacy database holding duplicate `(source_url, target_path)` rows no
  longer aborts boot with a raw SQLite unique-index error: startup fails
  loud with the offending pairs and the dedup remedy (keep the newest row
  per pair, see the deployment docs).
- Rate-limit default bursts raised (windows stay 60 s): native `50` to `100`,
  webmention `30` to `60`, read `60` to `300`, admin moderate `10` to `30`.
  Operators with an existing `.env` copied from the old `.env.example` keep
  their pinned values: update or remove the `RATE_LIMIT_*` lines to pick up
  the new defaults.
- OpenAPI `info.version` now reads `env!("CARGO_PKG_VERSION")` instead of a
  hardcoded duplicate of the crate version.
- The v7 IP-hash backfill now calls the single `hash_ip` implementation in
  `src/ip_hash.rs` instead of a duplicated inline hash. `run_migrations`
  takes the secret as a parameter (from `Config`) instead of reading
  `IP_HASH_SECRET` from the environment. Stored hashes are unchanged for
  the same input and secret.
- `Config` is typed at the boundary (T21): `PUBLIC_TARGET_ORIGIN` is a
  `url::Url` (the two stale "validated at config load" panics in the
  webmention handler and worker are gone), and `NOTIFY_BATCH_GRANULARITY`,
  `MODERATION_WEBHOOK_MODE`, `DEFAULT_COMMENT_STATUS` (reusing
  `moderation::Status`), `REACTIONS_ALLOWED`, and
  `COMMENT_LANG_ALLOW_EMOJI` are enums parsed once at load — consumers
  (`NotificationBatcher`, `Ingress`, comment/reaction handlers,
  `LanguageGate`) read the typed values instead of re-deriving strings.
  Every env parse now fails loud: `MAX_COMMENTS_PER_IP_PER_DAY`,
  `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR`, and `MAX_THREAD_DEPTH` refuse
  boot on garbage instead of silently keeping defaults, and unvalidated
  `MODERATION_WEBHOOK_MODE` / `DEFAULT_COMMENT_STATUS` values are
  rejected (matching is case-insensitive like the other enums:
  `SYNC` / `APPROVED` now parse as such — previously they fell silently
  into the defaults — while garbage still fails loud). The dead `rust_log` field is gone (`tracing` always read
  `RUST_LOG` directly via `EnvFilter::from_default_env()`; runtime
  behavior unchanged — `RUST_LOG` in `.env`/compose still works). No
  renames, no default-value changes.

### Fixed

- Webmention gone/resurrection lifecycle: a re-ping after `gone`
  re-fetches and re-verifies instead of deleting blindly — gone→alive is
  representable again (a restored backlink brings the comment back as
  `pending` through the moderation machine: deleted→pending fires,
  clearing tokens per B10). A single
  backlink-less 200 flips the ledger but keeps the comment; deletion needs
  a second consecutive observation and then removes EVERY comment the
  source owns (all target paths) through the moderation machine, firing one
  `comment.status_changed` event per deleted comment. Mention updates keep
  their status and now refresh `content_hash` (computed by the worker over
  the raw e-content through the shared pipeline); `is_new` notifications
  key on the (source, target) pair, so a second page mentioned by the same
  source notifies on its own. Backlink matching stays fragment-ignored /
  query-significant / case-sensitive-path / significant-trailing-slash
  (deliberately no utm-stripping).
- Notification batching pins the fixed-window deadline
  (`opened_at + NOTIFY_BATCH_SECS`) with clock-driven tests — the old
  per-push timers were effectively fixed-window too (the first timer won),
  but armed O(N) tasks and the deadline was never pinned. One timer task is
  now armed per window (when it opens) instead of one per push. Open windows
  drain as final digests on graceful shutdown (bounded to 12 s, in-flight
  sends awaited) instead of being lost on restart.
- Batch comment moderation now emits one `comment.status_changed` event per
  changed item (the documented engine polling path previously fired
  nothing), and self-service delete by token emits one as well (previously
  silent). Same-status changes emit nothing on every path. Re-approving a
  self-deleted comment through the admin API clears its delete token in the
  same commit, so the old token cannot delete the revived comment.
- Closed the unauthenticated SSRF hole in native-comment avatar fetch:
  `author_url` pages are now fetched through `SafeFetcher`
  (`src/fetch.rs`), the single guarded door also used by webmention
  fetches. Every hop is resolved and checked against the blocklist (named
  hosts and trailing-dot forms included), redirects are capped at 5 hops
  (fail-closed), and bodies are capped at 1 MiB while streaming. Any refusal
  falls back to the dicebear avatar like every other fetch failure. The old
  redirect-policy client no longer exists as a fetch path
  (`src/http/reqwest_client.rs` is a re-export shim); the shared client is
  for operator-configured endpoints only. Blocklist gaps closed alongside:
  trailing-dot hostnames (`localhost.`), TEST-NET-2/3 documentation ranges,
  and IPv6 unspecified/documentation addresses. DNS-rebinding TOCTOU
  (check-then-connect without address pinning) and IPv6 translation ranges
  (NAT64/6to4/Teredo) remain documented limitations, not claimed defenses.
  Follow-up hardening: webmention fetches honor the configured
  `FETCH_TIMEOUT_MS` (threaded from the worker spawn args, pinned by test);
  URLs with userinfo credentials are refused before connecting without
  echoing the credentials into errors; the now-unreferenced synchronous
  redirect policy was deleted.
- `PUBLIC_TARGET_ORIGIN` is validated at startup (absolute `http(s)` URL with
  a host). A typo'd origin now fails boot with a `ConfigError` instead of
  panicking per webmention request and per queued worker job.
- `STORE_IP_ADDRESS` accepts `true` (any case) and `1`, like
  `TURNSTILE_ENABLED`. `STORE_IP_ADDRESS=TRUE` no longer silently disables IP
  storage.
- Startup configuration output redacts Slack, Discord, and moderation webhook
  URLs (host plus a truncated path prefix). These URLs embody bearer posting
  tokens and were previously printed verbatim.
- Setting only one of `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` logs a
  startup warning instead of silently disabling Telegram delivery.
- Discord notifications escape author names and comment text: `@everyone` and
  `@here` are broken with a zero-width space and Discord markdown
  (`**`, `||`, `` ` ``, `<>`, and others) is backslash-escaped, so a comment
  cannot ping or reformat the admin channel.
- Discord payloads are capped at the 2000-character channel limit. Previews
  and name lists shrink first and the moderation footer is kept through every
  shrink stage; only the last-resort cut, reached when the unshrunk fields
  alone exceed the limit, can remove it.
- GitHub lookup now honors its timeout argument: `RealGitHub` applies
  `timeout_ms` as a per-request timeout instead of silently ignoring it.
- GitHub 403/429 (rate-limit) responses are now negative-cached like 404s,
  so a burst cannot burn the operator's API quota with repeated lookups.
  Cache keys are lowercased once per lookup, so case variants of a username
  share one row instead of bypassing the negative cache.
- Legacy upgrade ordering: column additions now run before the canonical
  schema snapshot, whose indexes (for example `idx_comments_parent`) fail on
  databases still missing their columns.
- Migration DDL errors propagate instead of being swallowed by `let _ =`.
  Only the already-exists case is skipped, via existence checks.
- `Config` now has a `Default` implementation mirroring the documented
  environment defaults; `from_env` overlays environment variables on top of
  it. Test setups build on `..Config::default()` with per-test overrides.
- `NOTIFY_BATCH_SECS` and `NOTIFY_BATCH_THRESHOLD` parse failures now report
  `InvalidNotifyBatchSecs` / `InvalidNotifyBatchThreshold` (naming the right
  variable) instead of reusing the `RATE_LIMIT_*` error variants.
- Comment reads now build on one `COMMENT_COLUMNS` list plus a single
  `select_comments` query builder (`src/db/repo/`), with `row_to_comment` as
  the only row mapper. The twelve duplicated 18-column `SELECT` strings and
  the dual-query cursor branches collapse into one parameterized statement
  per read path (`(?N IS NULL OR ...)` predicates). No behavior change:
  rows and ordering are identical, pinned by round-trip tests asserting all
  eighteen fields per query variant.
- Schema upgrades are now data: `src/db/pool.rs` holds a `MIGRATIONS`
  array (index is the version, the stamp is the index, `LATEST` is derived
  from its length) applied by iterate-and-apply instead of `if current < N`
  blocks. Column additions still route through the existence-checked
  `add_column_if_missing` safety net; every other statement is
  `IF NOT EXISTS`. `migrations/schema.sql` stays the fresh-install snapshot
  artifact. A new parity test upgrades a fabricated v0 database through all
  steps and diffs `sqlite_master` plus `PRAGMA table_info` against a fresh
  install, so step/snapshot drift (the v8 reactions near-miss class) fails
  the suite. Refuse-newer-DB, idempotent catch-up, the duplicate-pair
  pre-flight, and the v7 backfill are unchanged.
- Rate-limit governors are now honest about `(burst, window)`: the GCRA
  refill interval is `window / burst` per cell, so the sustained rate after
  the burst matches the documented `burst / window` per second (native
  `100`/60 s ≈ 1.67/s, webmention `60`/60 s = 1/s, read `300`/60 s = 5/s,
  admin `30`/60 s = 0.5/s). Previously the window fed `per_second` directly
  (one cell per window), throttling sustained traffic far below the
  documented rates. Env var names are unchanged.
- Route groups own their layer recipe (`src/http/routes.rs`): native write,
  public read, session, webmention, and protected admin constructors bundle
  routes with their governor and body-limit layers in one order. CORS wraps
  the public router and the protected admin group merges after it in
  `routes::compose`, so the no-CORS invariant is a function boundary.
- Governor 429s now return the documented JSON shape
  (`{"error", "code": "rate_limited"}`) with `Retry-After`, matching
  handler-side quota 429s, instead of plain-text "Too Many Requests".
- `docs/architecture.md` and `docs/security.md` rate tables list the real
  defaults (100/60/300/30, not the stale 50/30/60/10) with sustained-rate
  columns and login/batch/export rows. A config-level test re-derives the
  tables from `Config::default`, so doc/code drift fails the suite.
- One client-identity seam (`src/http/peer.rs`): each request resolves to a
  single normalized peer address (IPv4-mapped IPv6 canonicalizes to IPv4)
  consumed by the governors, the in-memory `Limiter`, and IP hashing
  (submission, anyone-mode reactions, import re-derivation). A client on a
  mapped address no longer gets a double quota or a mismatched hash.
- New `TRUST_PROXY` flag (default `false`): unset, `X-Forwarded-For`,
  `X-Real-IP`, and `Forwarded` are ignored (spoof-proof); set, the leftmost
  `X-Forwarded-For` entry (else `X-Real-IP`, else `Forwarded for=`) identifies
  the client. Enable only behind a proxy you control that overwrites those
  headers.
- Upgrade note (client-identity normalization): `::ffff:a.b.c.d` peers now
  key and hash as `a.b.c.d`. The in-memory per-IP daily caps reset on the
  upgrade restart (as on any restart), so no quota action is needed.
  Anyone-mode reaction identifiers stored under the old mapped hash no
  longer match — affected users re-react and the orphaned rows stay. With
  `STORE_IP_ADDRESS=true`, new `submitter_ip_hash` rows use the normalized
  form while history keeps the mapped form. No code migration is provided:
  this bites only non-default dual-stack binds combined with anyone-mode
  reactions or IP storage.
- `RepoError::Internal(String)` is now typed (`src/db/mod.rs`): SQLite
  failures classify once, at the rusqlite boundary, into `Constraint`
  (UNIQUE/FOREIGN KEY/CHECK — the row is invalid), `Busy`
  (`SQLITE_BUSY`/`SQLITE_LOCKED` — retry with backoff), `Io`
  (`SQLITE_IOERR`/`SQLITE_FULL`/`SQLITE_CANTOPEN`), and `Other` (everything
  else, including row-decode errors). `Display` and the `AppError` mapping
  are byte-identical (all four still `500`), so no handler, import, or
  reaction behavior changes: existing `?`-aborts and upsert logic are
  untouched. Retry/skip policies now have a type to match on instead of
  error substrings.
- Related storage writes are now atomic (`src/db/repo/`): `Repo::with_conn`
  runs N statements on one pooled connection and `Repo::with_tx` wraps them
  in `BEGIN IMMEDIATE` (contention surfaces as `Busy` for backoff retry).
  Native comment store (insert plus auto-approve status plus extracted-URL
  rows), webmention store (upsert plus ledger row), and gone handling
  (ledger `gone` plus comment deletion) each commit or roll back as one
  unit — a mid-unit failure leaves no torn comment-without-URLs or
  mention-without-seen. Invalid URL rows (empty fields, `Constraint`) are
  skipped with a warn log while the comment still commits; `Busy`/`Io`/
  `Other` abort the whole unit. Reaction approval is a compare-and-swap on
  the seen emoji, so an emoji change racing an approval keeps the new emoji
  pending. The admin export reads all five tables on one connection in one
  read transaction (a single WAL snapshot), and the public read API batches
  list plus total plus reaction counts per request the same way. Import stays
  per-row. Follow-up (not fixed here): pool `max_size`/`get_timeout` (B-12)
  — pool acquisition still waits without a timeout.
- Moderation is now one status machine (`src/moderation.rs`): a `Status`
  enum (`pending`/`approved`/`spam`/`deleted`, parsed at the boundary,
  stored as text as before) replaces the five hand-written action
  whitelists, and `Moderation::transition` owns validity, the status write,
  and the webhook emission. `deleted` is terminal except via admin
  re-approve (the webhook can never revive, owners can only delete); an
  admin revive clears the delete token in the same commit. One
  `ModerationSink` (async fire + 10 s sync decision adapters) with one
  payload builder replaces the two copy-pasted sync loops and four
  hand-built payloads; event schemas are unchanged. Pending→approved
  reaction approvals compare-and-swap on the reviewed emoji (`expected_emoji`
  on the single and batch reaction routes: stale values are rejected with
  400, absent behaves as before), so an emoji change racing an approval keeps
  the new emoji pending; approvals from `spam`/`deleted` are explicit
  overrides via plain write.
- Import is now a storage-layer restore service (`Repo::restore` in
  `src/db/repo/restore.rs`; `POST /api/admin/import` keeps auth plus JSON
  only). Per-section handling is uniform: invalid and orphaned rows in every
  section (comments, ledger, URLs, profiles, reactions) skip with a
  per-section `*_skipped` count — a single bad reaction row no longer aborts
  the whole import with a 500 and lost counts. Row failures branch
  structurally on the typed storage error (`Constraint` skips;
  `Busy`/`Io`/`Other` abort for retry). Restoring into a live database whose
  comment or reaction IDs hold different data is refused with `400` before
  the first write instead of silently reverting live moderation decisions;
  pass `"force": true` in the import body to overwrite colliding rows
  explicitly. Re-importing the same document stays idempotent, each
  comment's URL rows are replaced in one atomic commit, restored statuses
  write directly with no moderation webhook emission and no
  compare-and-swap, and URL/ledger/profile rows get structural validation
  (shapes plus byte-length caps). The response gains
  `webmention_seen_skipped`, `comment_urls_skipped`,
  `github_profiles_skipped`, and `comment_reactions_skipped` alongside the
  existing counts.
- Notifications now go through one `Channel` adapter seam
  (`src/notify/channel.rs`): each channel owns only its wire format and
  escape rules (policies unchanged), while dispatch, retry, and size budgets
  are shared — the three near-identical `send` functions and the two
  hand-synced dispatch branches collapse into one loop over
  `Notifier::channels`, so a fourth channel is one new adapter file plus the
  registration checklist (`Config` fields, `Notifier` fields plus an
  `is_empty` arm, one `push` line). Delivery retries transient failures (network errors,
  429, 5xx) up to 3 attempts with backoff, then logs and drops; permanent
  failures (other 4xx, Telegram `ok: false`) are attempted once.
  Bounded retry (duplicates possible on timeout), log-and-drop: submission
  never blocks on delivery. `Retry-After` on 429s is honored up to a 2 s cap
  per wait. Telegram and Slack payloads are now capped at their channel
  limits (4096 / 3000 characters) with the same shrink-cheapest-first policy
  Discord already had; the moderation footer survives every shrink stage.
  Digest content and grouping are unchanged.
- Native submission is now one deep module (`CommentIngress::submit` in
  `src/ingress.rs`, placed at the crate root beside `moderation` because
  both HTTP and storage depend on the pipeline, never the reverse): the
  300-line handler thins to form→`submit`→response while `Ingress` owns the
  13-step ordering (honeypot → Turnstile → daily cap → validate → hash-on-raw
  → sanitize → language gate on sanitized text → author/avatar resolve →
  parent check → T15 atomic store → notify → moderation sink) behind `Notify`
  / `ModerationSink` (T16's, via its one shared sync/async `deliver`
  adapter — the comment and reaction `if sync/else emit` duplications
  collapse to one line each) / `UrlStore` effect seams with in-memory fakes
  in tests. Ordering invariants are pinned: the content hash reflects the
  raw input, the language gate sees sanitized text, and URL rows come from
  the sanitized content only (URLs inside tags ammonia strips no longer
  persist — B12 fixed). Double submits still store twice with one shared
  hash (B9, deliberate: no idempotency keys — the engine dedups post-hoc
  via `content_hash` lookup). Follow-ups left open: language-gate
  quarantine tier (B6, hard block stays) and Unicode body-limit parity (B7,
  ~680-emoji math documented, not fixed); sync-webhook decisions still apply
  on the plain write path (no machine transition event).
- Honeypot detection honors `HONEYPOT_FIELD` (fallback `website`): the
  configured field flags, any other trap-looking field is inert. The widget
  emits the configured name via server-side substitution into the served
  `/embed/comments.js` (the alternative — accepting both names — would keep
  the old name live as an unflagged bypass), and reply forms now actually
  send the trap value (previously created but never serialized, so JS
  replies could never trip it). `HONEYPOT_FIELD` is restricted to 1–64 chars
  of `[A-Za-z0-9_-]` at startup (anything else refuses boot rather than
  shipping a broken widget). The served script carries an ETag over (file
  bytes, trap name): renames change the ETag and clients revalidate
  (`If-None-Match` → `304`), so the hour-long `max-age` cache turns over
  correctly instead of reusing a stale trap name blindly.
- Author identity is one shared module (`src/identity.rs`, crate root so
  native and import share it without HTTP↔storage inversion): control AND
  bidi/format spoof characters (U+202E overrides, U+200B zero-width,
  isolates — ZWJ/ZWNJ deliberately kept for emoji) strip from names,
  `github_username` is shape-checked (1–39 chars, alphanumeric or single
  hyphens, never leading/trailing) before URL interpolation (hostile values
  are a `400`, never part of a URL), `MAX_AUTHOR_LEN` applies everywhere
  (the import path's hardcoded `100` is gone — over-long backup rows clamp,
  live input rejects), and `validate_http_url` now enforces the host its
  docs always claimed (defense in depth over the `url` crate's empty-host
  parse errors). Native/import parity is pinned by a table test.
- Delete tokens are 128-bit CSPRNG secrets (32 lowercase hex chars from the
  OS RNG via the `getrandom` crate — now a direct dependency, already
  vendored transitively, so no new audit surface — zero inputs, not derived
  from peer address or time), replacing
  the deterministic 64-bit `DefaultHasher` token that was crackable offline
  over the (IP, time) window. Uniqueness is statistical; the delete route
  stays rate-limited with same-404 semantics. An OS RNG failure aborts the
  submission with a 500 (fail-closed — no weak token is ever minted).
- Moderation webhooks can be HMAC-signed (additive `WEBHOOK_SIGNING_SECRET`,
  empty/unset sends the historical unsigned body): every emission — async
  and sync, `*.created` and `*.status_changed` — carries
  `X-Zapiska-Timestamp` and `X-Zapiska-Signature: v1=<hex>` (HMAC-SHA256 over
  `"<timestamp>.<raw JSON body>"`, no new dependencies — built on `sha2` +
  `subtle`). Consumers recompute, compare in constant time, and enforce a
  ±300 s replay window; a secret-holding consumer rejects unsigned or
  tampered bodies (see `docs/moderation-engine.md` for the verify sketch).
- Webmention processing is now a `WebmentionProcessor` behind a
  `SourceFetcher` adapter (`src/worker.rs` + `src/fetch.rs`): the spawn loop
  is a thin drain, production fetches go through `SafeFetcher` built with
  the configured `FETCH_TIMEOUT_MS` (the dead `from_config` constructor is
  gone), and tests inject a canned mock with no network. The old
  `process_job` / `process_job_with_timeout` functions and the
  `allow_loopback` production parameter are gone: nothing flips the SSRF
  check from the call side anymore.
- Single-instance enforcement (T23, ADR-0001): `AppState::start` claims a
  `<database>.lock` sibling file holding its PID before the pool opens and
  refuses when another live instance holds it, with the PID, the lock path,
  and the remedy in the error. A stale file from a crash is reclaimed by
  PID liveness on Linux (`/proc` plus a PID-reuse command-line guard);
  elsewhere existence alone refuses. A clean shutdown releases the lock
  after the notification drain (`release_db_lock`, called from `main`).
  Two instances with different database files never block each other.
  Pinned by lock-contention tests (second start refuses, dead-PID reclaim,
  release-then-restart, per-database independence). No new dependencies
  (deliberately std-only).
- Architecture decision records in `docs/adr/`: single-process topology,
  export v1 as the compatibility contract, notification bounded retry
  (duplicates possible on timeout — not at-most-once), and the per-ticket
  calls worth keeping (SafeFetcher as the only outbound door, restore
  without events, gone grace of two, refuse-by-default import, the
  13-step ingress ordering).

## [0.2.0] - 2026-08-07

### Added

- Reactions with moderation status and public approved counts.
- Language filtering for native comments (allow/block lists, emoji policy).
- RSS feeds (`/feed.xml`, global and per-path).
- Sortable public comment reads (`newest`/`oldest` with cursors).
- JSON export and import for all five tables.
- Telegram, Slack, and Discord notifications with batching.
- Pentest suite, Docker/CI hardening, ASD-STE100 docs rewrite.

## [0.1.0] - 2026-08-07

### Added

- Native comments with sanitization, threading, and honeypot flagging.
- W3C webmention receipt with backlink verification and h-entry parsing.
- Admin moderation, lookup, and bulk-context API with token/cookie auth.
- Content hashing, moderation webhooks, and daily/domain caps.
- Submitter IP storage with salted SHA-256 hashing (`IP_HASH_SECRET`).
- Cloudflare Turnstile verification for native comments.
- Per-route rate limits, CORS, and request body limits.
- Single-command Docker Compose deploy, GHCR multi-arch images,
  cross-platform release binaries, systemd/OpenRC templates.
- Swagger UI with OpenAPI document.

[Unreleased]: https://github.com/nithitsuki/zapiska/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/nithitsuki/zapiska/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/nithitsuki/zapiska/releases/tag/v0.1.0
