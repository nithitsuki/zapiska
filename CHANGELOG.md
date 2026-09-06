# Changelog

All notable changes to zapiska are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/).

## [Unreleased]

### Added

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

### Changed

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

### Fixed

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
