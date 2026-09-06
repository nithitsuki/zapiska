# 05 — Notifications, Operations, Deployment & Test Coverage

## What I read

Source:
- `src/notify/mod.rs` (408 l.) — `Notifier` config struct, `Digest`/`DigestPreview`, shared `by_line`/`strip_html`, channel dispatch (`deliver_new_comment` mod.rs:179, `deliver_digest_to_channels` mod.rs:199), one `spawn`-per-channel pattern.
- `src/notify/batcher.rs` (169 l.) — `NotificationBatcher`: per-page/global windows, threshold flush, timer-per-push spawn, in-memory `Mutex<HashMap>`. **Zero tests.**
- `src/notify/telegram.rs` / `slack.rs` / `discord.rs` — per-channel `send`/`spawn`/formatter/`escape`; formatter unit tests only.
- `tests/worker_notify.rs` (195 l., `webmentions`-gated) — webmention first-sighting notifies / update does not; **immediate mode only** (`notify_batch_secs: 0`, worker_notify.rs:87), Telegram only, wiremock happy path.
- `tests/e2e.rs` (383 l.) — native post→moderate→read, pending stays hidden, webmention lifecycle, rate-limit flood. 4 tests.
- `tests/pentest.rs` (691 l.) — adversarial XSS/XML/SQL/path/CRLF content through the full HTTP pipeline, with a wiremock standing in for notification endpoints — **but all notification config is `None`** (pentest.rs:76–80), so none of the malicious payloads ever flow through the notify formatters.
- `.github/workflows/ci.yml`, `Dockerfile`, `docker-compose.yml`, `deploy/zapiska.service`, `deploy/zapiska.openrc`, `.env.example`.
- `docs/deployment.md`, `docs/development.md`, `docs/security.md`, `CHANGELOG.md`.
- `src/main.rs` (127 l.) — startup sequence; `src/config.rs` (1207 l.) — 50-field `Config`, `from_env`, `redacted_display`, error variants, 29 tests.
- `src/http/webhook.rs`, `src/http/admin/data.rs` (export/import), `src/http/mod.rs` (routes, `healthz`), `src/http/webmention_post.rs`, `src/http/shutdown.rs`, `src/worker.rs`, `src/state.rs` (`Limiter`), `src/ip_hash.rs`, `src/db/pool.rs` (migrations v8), `src/github.rs`, `src/lib.rs`.

## Friction

### F1. `src/notify` — one module with an *implicit* channel seam (channels drift)

**Module:** `notify` (mod.rs + batcher.rs + telegram.rs + slack.rs + discord.rs).
**The shallow/leaky thing:** The module is genuinely one module — shared `Digest`, `Digest::from_batch`, `by_line`, `strip_html`, `MAX_*` caps are used by all three channels, and `Notifier` is a single config object. That part is good depth. What's leaking is the **channel adapter interface is never named**: there is no trait, no enum, no `Channel` — each channel is a pile of free functions (`build_single_payload`, `build_digest_payload`, `send`, `spawn`), and dispatch is two handwritten if-chains (mod.rs:179–196 and mod.rs:199–216) that must be kept in sync by hand. Adding a fourth channel means editing: the `Notifier` struct (mod.rs:37–45), `Notifier::new` (mod.rs:48–56), `is_empty` (mod.rs:60–63), both dispatch functions, plus a new file. Three parallel implementations already exist and **they have drifted**:

- **Escaping: three different implementations.** telegram.rs:59–68 (`escape` via `flat_map`), slack.rs:42–46 (`escape` via `replace`), **discord.rs: none at all** — `author_name`, `author_url`, and content are concatenated raw (discord.rs:41–61). The discord test even *locks in* the raw output (`text.contains("Alice & Bob <co>")`, discord.rs:349).
- **Transport: three near-identical `send` functions** (telegram.rs:10–41, slack.rs:10–26, discord.rs:10–22), each POST + 10 s timeout + non-2xx → `Err(String)`.
- **Error type: three ad-hoc `Result<StatusCode, String>`s**, no shared delivery error, no retry, no metrics.
- **Shared by design:** `strip_html` and `by_line` are shared and tested — so this is not full triplication; it's a module whose shared core is real but whose **seam is implicit**, and the unenforced part is exactly the part that drifted (Discord escaping).

**Why it hurts:** fixes and features must be replicated three times (e.g., "truncate at channel limit", "retry with backoff", "escape user content"); a fix in telegram.rs is invisible to discord.rs. The Discord injection gap (see B3) is the direct product.

**Deletion test:** Deleting `notify` entirely moves digest-building into the batcher and dispatch into comment_post/worker — callers lose leverage and the 2-call-site duplication grows. Deleting just the per-channel files leaves a working shared core. Verdict: **keep, but name the seam** — one `Channel` adapter (format + deliver) with the shared dispatch/retry/escaping policy in one place, adapters as thin as possible.

### F2. `batcher.rs` — timer-per-push and a sliding window, both untested

**Module:** `NotificationBatcher`.
**The shallow/leaky thing:** Two concrete behaviors that need scrutiny and have none:

1. **A flush task is spawned on *every* push** (batcher.rs:113, 119–134). A 1 000-comment flood spawns 1 000 sleeping tasks, each holding `Arc<Self>` + a `Client` clone; all but one will wake to a stale/absent entry and no-op (batcher.rs:142–143). The flood case is exactly the case batching exists for.
2. **The window is sliding, not fixed.** The task sleeps `window` *from its own spawn time* (`tokio::time::sleep(window)`, batcher.rs:131), not from `opened_at`. So a trickle below the threshold re-arms the window on every comment: a page receiving one comment every 59 s with `NOTIFY_BATCH_SECS=60` **never flushes** until traffic pauses. For sustained sub-threshold traffic, digest delay grows with traffic duration. The `opened_at` equality check (batcher.rs:142) then only serves stale-task suppression, not window semantics.

**Why it hurts:** O(comments) timer churn; digest latency is traffic-dependent and surprising; and none of this is pinned by tests — `batcher.rs` has **zero tests**, and every integration/e2e/pentest config uses `notify_batch_secs: 0` (immediate mode: worker_notify.rs:87, e2e.rs:59, pentest.rs:81, test_support.rs — let me confirm… yes, all four), so **the default batching path (60 s / 20 / page) is never exercised by any test in the repo**. Not even the threshold flush is covered.

**Deletion test:** Deleting the batcher would push windowing into the two call sites (comment_post.rs:250–262, worker.rs:185–201) and they'd each re-derive per-page/global keys — the shared state machine is the leverage; keep it. Verdict: **keep, deepen with a clock/storage seam** (see D2).

### F3. `Config` — a boundary object that is shallow in the wrong places (stringly-typed, inconsistently validated)

**Module:** `config.rs` (1207 l., 50 fields).
**The shallow/leaky thing:** `Config` is a data struct, not a god-behavior module — parsing/validation/redaction are centralised, which is good. The shallowness is that **it leaks its own typing**: three consumers re-derive enum semantics from strings:

- `notify_batch_granularity == "global"` — batcher.rs:50
- `default_comment_status == "approved"` — comment_post.rs:237
- `moderation_webhook_mode == "sync"` — comment_post.rs:269

Nothing stops an invalid state from being *representable* (only `from_env` happens to reject some — and inconsistently):

- `PUBLIC_TARGET_ORIGIN` is **never validated** at load (config.rs:188 just reads it), yet two sites `.expect("PUBLIC_TARGET_ORIGIN validated at config load")`: webmention_post.rs:72 (in a request handler — a panicking async task per request) and worker.rs:277 (worker task panic per job, job silently lost). A typo'd origin (`htts://…`) boots cleanly, healthz stays "ok", and the whole webmention pipeline dies with panics instead of a startup error.
- **Silent-default parses** next to fail-loud parses: `MAX_COMMENTS_PER_IP_PER_DAY` / `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR` / `MAX_THREAD_DEPTH` fall back to defaults on garbage (`unwrap_or(50)`, config.rs:258–264; `unwrap_or(0)`, config.rs:274–277) while `MAX_CONTENT_LEN` etc. fail startup (config.rs:205–213). `STORE_IP_ADDRESS` uses case-sensitive `== "true"` (config.rs:266) while `TURNSTILE_ENABLED` accepts `true|1` via `env_bool` (config.rs:279) — `STORE_IP_ADDRESS=TRUE` silently turns IP storage off.
- **Mislabeled errors:** `NOTIFY_BATCH_SECS`/`NOTIFY_BATCH_THRESHOLD` reuse `InvalidRateLimitWindow`/`InvalidRateLimitBurst` variants (config.rs:337–346), so a bad `NOTIFY_BATCH_SECS` reports "RATE_LIMIT_*_WINDOW must be a positive integer".
- `rust_log` is a dead field — read from env (config.rs:255) but never used (tracing uses `EnvFilter::from_default_env()`, main.rs:28–30); it exists only to be displayed.
- Half-configured Telegram (token without chat ID) is representable and **silently disables** the channel (mod.rs:60–63); no startup warning that a channel is partially configured.
- `redacted_display` prints **Slack and Discord webhook URLs verbatim** (config.rs:552–553) — these URLs embody bearer posting tokens — and the redaction test *asserts they are visible* (config.rs:1185–1191). `docs/security.md:174` papers over it ("Webhook URLs can appear in warning logs and startup configuration output").

**Deletion test:** Deleting `Config` moves field ownership into `AppState` + every module — but the pain is *already being paid*: the 50-field `Config` struct literal is hand-reconstructed in **five places** (e2e.rs:21–67, pentest.rs:43–89, worker_notify.rs:49–95, `http/test_support.rs:69`, config.rs:1098–1145 for the redaction test). Adding one field touches 5+ call sites today. Deleting `Config` outright would make that worse, not better. Verdict: **keep as the boundary, deepen the boundary**: typed subgroups (one `NotifyConfig` struct field), one validation policy (all fields fail loud, none silent-default), validate `PUBLIC_TARGET_ORIGIN` at load, redact webhook URLs, kill the false `.expect` comments.

### F4. `main.rs` — startup assembly with no seam; raw SQL in the entrypoint

**Module:** `main.rs` (127 l.).
**The shallow/leaky thing:** `main` hand-assembles the entire dependency graph: cfg-split client building (main.rs:37–42), pool + migrations (44–46), **a raw SQL `SELECT count(*) FROM comments…`** for the IP_HASH_SECRET warning (main.rs:51–71 — SQL knowledge in the entry point, bypassing `Repo`), `Notifier`/`LanguageGate`/`GitHub`/worker construction (77–100), and the 11-field `AppState` literal (102–113). There is no `App::start(config)` seam, so the assembly is **re-implemented by hand in every test file** — the same 50-field `Config` literals above, plus hand-built `AppState` literals in e2e.rs:86–97, pentest.rs:97–108, test_support.rs:104–112. Startup knowledge is duplicated, not shared; a new `AppState` field means editing main + 3 test assemblers.

**Deletion test:** Delete `main`, and the assembly still must exist somewhere — test_support has the closest thing to a builder. The right move is the reverse: **extract the assembly into `App::start(config)`**, have `main` call it, and have tests consume it. Also fold the `IP_HASH_SECRET` check into the repo layer (it already has a home: the export/import salt-mismatch logic in admin/data.rs).

**Fail-loud vs fail-silent audit of the startup sequence:** fail loud (`.expect`): Config (main.rs:32), pool (44), migrations (46), bind (118), client build (42). Fail silent or deferred: invalid `PUBLIC_TARGET_ORIGIN` (see F3) → runtime panics; partial Telegram config → silent disable; `dotenvy::dotenv()` errors ignored (main.rs:25, fine by design); batcher windows dropped on shutdown with **no drain** even though a graceful-shutdown hook exists (main.rs:124, shutdown.rs) — in-flight `tokio::spawn` deliveries are simply killed when the runtime drops.

### F5. `ip_hash` — the hashing algorithm exists in two copies

**Module:** `ip_hash.rs` (15 l.) + inline copy in `db/pool.rs:262–292`.
**The shallow/leaky thing:** `hash_ip` (ip_hash.rs:6–14) is `pub`, load-bearing (comment storage, reactions identity, import re-derivation admin/data.rs:141–147), and has **zero tests**. The v7 migration backfill re-implements the same SHA-256 + salt + `h:` prefix inline (pool.rs:276–289) and reads `IP_HASH_SECRET` **from the environment directly** (pool.rs:264) instead of receiving it — a second writer of the same knowledge. Deletion test: delete `ip_hash.rs` and pool.rs's copy stands alone; delete pool's copy and the backfill loses `hash_ip`. Verdict: **delete the duplication** — one function used by all three sites (comment_post.rs:203, pool.rs backfill, admin/data.rs), with stability tests (see D5).

## Bulletproofing brainstorm

### B1. Notification delivery failure (permanent or transient)
**Scenario:** Telegram bot blocked/rate-limited, Slack/Discord webhook deleted or misconfigured; or a transient network outage at flush time. Delivery is at-most-once fire-and-forget — telegram.rs:50–55, slack.rs:33–38, discord.rs:29–34 log `warn` and drop.
**Worst case:** a comment flood's digests never reach the admin; the moderation queue goes stale unseen; every futuredelivery still fails (e.g., 404 on a deleted webhook) with no operator signal beyond startup logs.
**Likelihood:** med. **Impact:** med.
**Current mitigation:** `tracing::warn!` per failure; failures never affect comment submission (by design, mod.rs:11–12).
**Gap:** no retry/backoff, no failure counter, no health check for channels, no dead-letter. Even a 3-attempt backoff on transient errors plus a debug-level delivery-error counter would close most of it.

### B2. Batched digests lost on restart (in-memory windows)
**Scenario:** `docker compose restart` or `systemctl restart` during an open window — the batch entry dies with the process; the comments are in SQLite but the digest is never sent. Documented tradeoff (batcher.rs:6–7, development.md:143).
**Worst case:** deploy during a comment burst → the operator never learns of the burst.
**Likelihood:** med (restarting during a burst is rare, but bursts are when restarts hurt). **Impact:** low–med (comments are stored; only the alert is lost).
**Current mitigation:** none beyond the doc comment. A graceful-shutdown hook *exists* (main.rs:124, shutdown.rs) but nothing drains the batcher.
**Gap:** drain open windows on shutdown (flush each entry before exit). Cheap, uses an existing seam. Persisting windows is a bigger change and likely not worth it.

### B3. Discord `@everyone` / markdown injection through comment content
**Scenario:** any commenter posts content containing `@everyone`, `@here`, `||spoiler||`, `**bold**`, `` `code` `` — the notify digests render it raw in Discord (discord.rs:41–61; no escaping anywhere in the file). `@everyone` is plain text and sails through ammonia sanitization (sanitize.rs), so it reaches the admin channel verbatim. Telegram and Slack escape (`&< >`, slack.rs:42–46, telegram.rs:59–68); Slack also neutralises `<!here>` via `<` escaping. Only Discord is exposed.
**Worst case:** a spammer pings every member of the Discord channel that hosts admin notifications, repeatedly; channel gets muted or the bot webhook gets deleted (which then triggers B1's silent loss).
**Likelihood:** **high** — trivial to do, content is attacker-controlled, and the raw-`@everyone` text is *asserted* in the unit test (discord.rs:349). **Impact:** med (harassment/reputation of the admin channel; no data loss).
**Current mitigation:** none for Discord. `strip_html` + 300-char previews bound the size but not the content.
**Gap:** a Discord `escape` (escape `@` in mention patterns, `*`, `_`, `` ` ``, `|`, or render previews as plain-text via embeds), plus adversarial formatter tests (see D1).

### B4. Message-size limits (Telegram 4096 / Discord 2000 / Slack block 3000)
**Scenario:** channel limits are hard API rejections. The formatters cap *previews* (300 chars, telegram.rs:72; 300, slack.rs:50; 300, discord.rs:39; digests 200, mod.rs:99) but never cap **total message length**, and two inputs are unbounded by the formatters: `author_name` (bounded only by configurable `MAX_AUTHOR_LEN`, config.rs:14, no upper clamp) and commenter-name lists in digests (8 names × up to `MAX_AUTHOR_LEN`).
**Worst case:** operator raises `MAX_AUTHOR_LEN` (or a webmention's h-entry author name is long, worker.rs:158–181 — webmention author names aren't clamped to `MAX_AUTHOR_LEN` at all) → digest exceeds the channel limit → API 400 → logged warning only (B1). Operator sees "telegram notification failed" with no hint why.
**Likelihood:** low–med. **Impact:** low (notification lost; comment pipeline unaffected).
**Current mitigation:** preview caps only; the `telegram_text_truncates_long_content` unit test (mod.rs:328–334) proves boundedness for one hardcoded case.
**Gap:** compute the total budget at message-build time and truncate the cheapest field (commenter list) to fit; tests that enumerate each channel's limit with maximal inputs.

### B5. Timer-per-push churn under flood (batcher)
**Scenario:** a spam burst (rate limiter caps at 100/min/IP but a botnet spans IPs) pushes thousands of comments into open windows; each push spawns a flush task (batcher.rs:113). Threshold flushes cap *message* counts (default 20/window) but not *task* counts.
**Worst case:** transient task/runtime churn at exactly the moment the server is under attack; at the extreme, notification dispatch competes with the HTTP critical path for runtime capacity.
**Likelihood:** med (floods are the stated use case for batching). **Impact:** low–med.
**Current mitigation:** threshold flush bounds window size; stale tasks no-op cheaply (batcher.rs:142–143).
**Gap:** spawn the window timer once per window (only when the key is newly inserted), not per push. Small change, removes the O(comments) timer growth and would come with the D2 tests.

### B6. Sliding window: sub-threshold digests never flush during sustained traffic
**Scenario:** F2's re-arm-per-push semantics. A page getting one comment per minute with `NOTIFY_BATCH_SECS=60` never produces a digest until traffic pauses; the admin believes the page is quiet.
**Worst case:** notification delay tracks traffic duration (indefinite for steady trickle); combined with a low threshold, a flood's early digests arrive *after* the flood.
**Likelihood:** med (steady trickle is a normal pattern for a quiet blog). **Impact:** low (delayed alert, not lost).
**Current mitigation:** threshold flush (`NOTIFY_BATCH_THRESHOLD=20`) rescues floods; nothing rescues trickles.
**Gap:** either sleep until `opened_at + window` (fixed window) or document the sliding semantics; the D2 fake-clock tests make the intended semantics explicit either way.

### B7. Env-var validation gaps
**Scenario:** typo'd or case-mismatched env values in `.env` files (see F3 for the catalogue: `PUBLIC_TARGET_ORIGIN` unvalidated; `STORE_IP_ADDRESS=TRUE` ignored; silent numeric defaults; `NOTIFY_BATCH_*` mislabeled errors; half-configured Telegram silently disabled).
**Worst case:** `PUBLIC_TARGET_ORIGIN=htts://example.com` → boot is green, healthz is green, every webmention request panics the handler (webmention_post.rs:72) and every queued job panics the worker (worker.rs:277); webmentions are dead with only stderr panics to show for it. Or `STORE_IP_ADDRESS=TRUE` → IPs silently not stored, defeating the operator's abuse-analytics (and privacy-notice) expectations.
**Likelihood:** med (env typos are the most common deployment error). **Impact:** med.
**Current mitigation:** `from_env` fails loud for ~15 variables; `ADMIN_TOKEN` required and enforced in compose (`:?`, docker-compose.yml:20).
**Gap:** validate `PUBLIC_TARGET_ORIGIN` (parse + scheme at load); warn at startup when a configured channel is partial; unify bool parsing and the fail-loud policy; correct the mislabeled `NOTIFY_BATCH_*` error variants.

### B8. Secret handling in logs
**Scenario:** startup log prints full Slack/Discord webhook URLs (config.rs:552–553); `redacted_display` is `info!`-level (main.rs:33) and webhook URLs in warning/debug logs (webhook.rs:22, 24 — moderation webhook URL logged *with any query credentials*). These URLs are bearer tokens for posting to the channel.
**Worst case:** a log aggregator, backup, or support screen-reader with read access to logs can silently post to the admin alert channel (impersonating the bot — combine with B3's `@everyone`).
**Likelihood:** med (logs are routinely shipped and shared). **Impact:** med.
**Current mitigation:** docs/security.md:174–175 acknowledges the behavior and tells operators not to put credentials in webhook URLs; docs/deployment.md:319 claims "the server redacts the admin, GitHub, Telegram, and Turnstile secret values" — Slack/Discord webhook URLs are missing from that list.
**Gap:** redact webhook URLs (host + path prefix, e.g. `https://hooks.slack.com/services/T0…`); delete the test assertions that demand full URLs visible (config.rs:1185–1191).

### B9. Docker healthcheck semantics
**Scenario:** `HEALTHCHECK` and compose (Dockerfile:64–65, docker-compose.yml:61–66) hit `/healthz`, which returns a static `"ok"` (http/mod.rs:192–194) — no DB probe, no pool check.
**Worst case:** the SQLite file is on a full volume or corrupted; boot succeeds *after* migrations (so healthz's first run is fine), then the pool has no usable connections (disk-full writes fail) — every comment post 500s while healthz stays green and `restart: unless-stopped` never triggers.
**Likelihood:** low–med. **Impact:** med (silent outage behind a green check).
**Current mitigation:** boot-time `expect`s on pool/migrations (main.rs:44–46) catch boot-time failures only.
**Gap:** make healthz issue `SELECT 1` through the pool (and fail 503 on error); the check then covers the degraded-after-boot case. Cheap, high value.

### B10. systemd hardening gaps
**Scenario:** `deploy/zapiska.service` hardens reasonably (NoNewPrivileges, PrivateTmp, ProtectSystem=full, ProtectHome, RestrictAddressFamilies, RestrictRealtime, LockPersonality, User=/Group=) but stops short: no `CapabilityBoundingSet`, `UMask`, `ProtectSystem=strict` + narrow `ReadWritePaths`, `PrivateDevices`, or `ProtectKernel*`. `Restart=on-failure` with `RestartSec=5` (zapiska.service:34–35) plus a config error (e.g., missing `ADMIN_TOKEN` in the env file) produces a restart loop until the unit gives up.
**Worst case:** a compromise of the network-facing process has a slightly wider syscall/fs surface than necessary; a boot-looping misconfig generates log noise.
**Likelihood:** low. **Impact:** low.
**Current mitigation:** the hardening that exists; `Type=simple`, `EnvironmentFile` perms.
**Gap:** `CapabilityBoundingSet=`, `UMask=0077`, `ProtectSystem=strict` with `ReadWritePaths` narrowed to the DB directory; a `RuntimeDirectory` for the WAL if the DB moves there.

### B11. Backup story for WAL SQLite
**Scenario:** backup via file copy. The docs get it right: stop the service first, copy `comments.db` + `-wal` (deployment.md:269–276), or use the live JSON export (deployment.md:278–296) — both are sound. No `sqlite3 .backup` path, no script, no cron guidance; compose has no backup automation. The JSON export includes delete tokens and, when stored, raw IPs — keep files private.
**Worst case:** (with docs-followed procedure) minimal; the residual risk is an operator copying only `comments.db` while the server runs (unsafe, but the docs explicitly warn against it) or losing the `.env`/`IP_HASH_SECRET` (docs warn at deployment.md:289–296 and the import warns at admin/data.rs:242–252).
**Likelihood:** low (procedure documented). **Impact:** high if it happens (all comments).
**Current mitigation:** documented stop-then-copy and export/import with salt-mismatch warnings; export/import is well tested (admin/mod.rs:597–1184: auth, round-trip, idempotency, version rejection, re-sanitization, size ceiling, orphan skipping, salt flag, hash recompute, warning logic).
**Gap:** an ops script (`zapiska-backup.sh`) or a documented `sqlite3 .backup` live-backup alternative; otherwise this is in good shape.

### B12. Single-instance assumption
**Scenario:** batcher, `Limiter` (state.rs:40–84), and the governor are all in-process. `docs/deployment.md:3` states "single process with one SQLite file", so the assumption is documented — but nothing *enforces* it: two containers on the same volume (`docker compose up --scale` or a second process on the same `DATABASE_PATH`) would both write WAL happily (busy_timeout 5 000 serializes) while **duplicating every notification**, halving rate limits, and splitting window state.
**Worst case:** doubled admin-channel spam + rate-limit bypass + confusing digest duplication.
**Likelihood:** low (docs are explicit). **Impact:** med.
**Current mitigation:** documentation only.
**Gap:** an advisory lock (e.g., `flock`/`PRAGMA locking_mode` check at startup, fail loud) or an explicit compose note. Cheap insurance.

### B13. Notification flood → Telegram API rate limit (immediate mode)
**Scenario:** `NOTIFY_BATCH_SECS=0` (immediate) plus a comment storm: every comment spawns a Telegram `sendMessage`; Telegram rate-limits bots (~30 msg/s) → 429s → logged warnings, messages silently dropped (at-most-once).
**Worst case:** the admin misses the exact flood they configured immediate mode to catch.
**Likelihood:** med under attack; low normally. **Impact:** low.
**Current mitigation:** none; batching (the default) is the mitigation.
**Gap:** document the tradeoff (deployment.md:105 says only "Zero sends immediately"); optionally log a running failure counter at `warn` when N consecutive deliveries fail.

### B14. Test-coverage gaps (module map and what's missing)
Zero-test modules (from `grep -c` across `src/`): **`notify/batcher.rs`** (core window/threshold/expiry state machine — the default config path), **`ip_hash.rs`** (load-bearing, see F5), **`state.rs`** (`Limiter`, date-key cleanup — restart-reset semantics untested), **`http/webhook.rs`** (moderation webhook fire), **`http/shutdown.rs`**, **`http/reqwest_client.rs`**, **`http/layers.rs`**, **`http/comments_read.rs`**, **`db/repo/{comments,webmentions,urls,reactions,github_profiles}.rs`** (repo behavior lives in repo/mod.rs's 21 tests), plus `main.rs`, `lib.rs`, `openapi.rs`. Channel transport paths (`send` error branches, non-2xx, malformed bodies) are unexercised: worker_notify's wiremock returns only the happy path (worker_notify.rs:132–137). Only Telegram is integration-tested; **Slack and Discord delivery are never exercised against a mock server**, and digest delivery (`deliver_digest_to_channels`) is untested end-to-end. Formatter tests feed one benign sample ("Alice & Bob <co>", notify/mod.rs:261–272); no adversarial content (B3's `@everyone`, control chars, 5 000-char names) flows through formatters — pentest's config disables all channels (pentest.rs:76–80).
**Worst case:** the flagship batching feature ships a Windows-meets-Telegram regression nobody notices; the Discord injection (B3) is invisible at review time.
**Likelihood:** high (it's already true today). **Impact:** med.
**Current mitigation:** 342/281 tests (development.md:66 — hardcoded counts that will drift); strong pentest suite for the store/render path; export/import thoroughly covered at the HTTP level (admin/mod.rs repo tests).
**Gap:** see D1–D6 for the seam-based shape.

### B15. CI matrix gaps
**Current state:** lint (fmt + clippy `-D warnings`), test (default features), test-comments-only (`--no-default-features --features comments`, ci.yml:44–55 — the matrix the docs promised, and it runs *tests*, not just builds 👍), release binaries (5 targets, ci.yml:61–84), docker-publish on tags (amd64+arm64, ci.yml:122–154).
**Gaps:**
- **Docker images are never smoked-tested**: docker-publish builds and pushes; nothing runs `docker compose up` + healthz in CI. A broken image (e.g., missing `wget`, wrong `ENV`, non-executable binary) ships to GHCR and only detonates on `latest` pulls.
- **Cross-compiled binaries are never executed**: the aarch64 job documents "verified locally with qemu" (ci.yml:97–100) as a comment — CI itself doesn't run the artifacts it ships. A `qemu-user` run of `zapiska --version` + a boot smoke test per Linux target would catch linker/glibc issues.
- Tests run on ubuntu-latest only, debug profile only (`cargo test`, release only in the release build job which doesn't run tests); `test-comments-only` inherits this.
- No "no features at all" combo (`--no-default-features` alone) — comments is empty so the two combos are nearly redundant; the interesting untested combo is default-without-`webmentions` exercised at *runtime* (which test-comments-only does cover via its test run — so this is mostly fine).
- The hardcoded test counts in development.md:66 (342/281) will silently drift.
**Worst case:** a tag ships a Docker image or aarch64 binary that fails on first pull (breaks `latest` consumers).
**Likelihood:** med (tag-time only, but tag-time is when it hurts). **Impact:** med.

## Candidate deepening opportunities

### D1 — `notify`: name the channel seam (deepest candidate)
**Module:** `src/notify/`.
**Seam:** a `Channel` trait or enum — `format_single(&NewCommentInfo) -> Wire`, `format_digest(&Digest) -> Wire`, `send(&Client, &Wire) -> Result<(), DeliveryError>` — with one shared dispatch loop, shared retry policy, shared escape utility (`escape_markdown` vs `escape_html` as channel-provided policies), and shared size-budgeting. The `Digest` (mod.rs:105) stays the shared core; the per-channel files become thin adapters.
**Plain-English shape of change:** "Each channel is one adapter that knows only its wire format and its escape rules; the module decides when, how many times, and with what budget it delivers." Callers (comment_post.rs:250–262, worker.rs:185–201) keep calling `notifier.push`; Config keeps one `notify` subgroup.
**Tests that would improve:** golden tests per adapter with maximal inputs (each channel's hard limit — 4 096/2 000/3 000); adversarial content (`@everyone`, `_x_`, `||y||`, `` `z` ``, `<script>`, 5 000-char names) asserting the channel receives escaped/neutralized output via wiremock; a delivery-retry test asserting at-least-N-attempts on 5xx then a logged drop; size-budget test proving the digest writer truncates the commenter list to fit.

### D2 — `batcher`: clock + timer seams (second deepest)
**Module:** `src/notify/batcher.rs`.
**Seam:** a `Clock` (`fn now() -> Instant`) injected via constructor, and one spawn-per-window (store the join handle/trigger decision inside the entry rather than per push).
**Plain-English shape of change:** "The batcher is a small state machine you can wind by hand: you tell it comments arrived and you turn the clock; it tells you when a digest flushes." Fixed-window semantics (sleep until `opened_at + window`) or an explicit documented sliding window; drain-on-shutdown hooking into the existing graceful-shutdown seam (main.rs:124).
**Tests that would improve:** fake-clock unit tests: window-closed flush, threshold flush mid-window, stale-task no-op (older task fires after a newer window opened), per-page vs global keying, immediate mode, restart-loss behaviour pinned as a test documenting the semantics; a drain test asserting pending entries flush on shutdown.

### D3 — startup assembly seam: `App::start(config)`
**Module:** `src/main.rs` + `src/http/test_support.rs` + tests.
**Seam:** one public `start(config) -> AppState` (or `Server`) builder that main, e2e.rs, pentest.rs, worker_notify.rs, and test_support all call; `Config::default()` for the 50-field literals with per-test overrides; the raw SQL IP_HASH_SECRET check (main.rs:51–71) moves into a `Repo`/db-layer function.
**Plain-English shape of change:** "Starting the server is one function you can call from a test; the entry point is a thin argument passed to it." Config gains a field becomes a one-file change (Config + builder), not five.
**Tests that would improve:** one "startup assembles and serves healthz" smoke test with a temp DB; a test that `start` with a newer-schema DB fails loudly (already unit-tested in pool.rs:399–411, now proven at the assembly level); a test asserting the IP_HASH_SECRET warning fires (or not) given hashed rows.

### D4 — `Config`: typed subgroups, one validation policy, redaction fix
**Module:** `src/config.rs`.
**Seam:** the boundary between env text and behavior. Group fields into `NotifyConfig`, `ModerationConfig`, `RateLimitConfig`…; parse enums once (`NotifyGranularity`, `CommentStatus`, `WebhookMode`, `StoreIpPolicy` via one bool parser); every parse fails loud identically; `PUBLIC_TARGET_ORIGIN` parsed as a `Url` at load (killing the two false `.expect`s, webmention_post.rs:72 and worker.rs:277); `redacted_display` prints webhook URL **host + redacted path**; corrected error variants for `NOTIFY_BATCH_*`; drop dead `rust_log`.
**Plain-English shape of change:** "Config stops handing strings to the rest of the program; it hands already-decided things. Bad input fails at boot, not at first request, and secrets don't appear in the boot log."
**Tests that would improve:** per-subgroup parse tests; `PUBLIC_TARGET_ORIGIN=garbage` → `ConfigError`; `STORE_IP_ADDRESS=TRUE` → enabled (or explicitly documented case-sensitivity with a test); redaction test asserting webhook URLs are *not* reproduced verbatim (replacing config.rs:1185–1191); differential test that every parse path errors rather than silently defaults.

### D5 — `ip_hash`: one implementation, stability-locked
**Module:** `src/ip_hash.rs`.
**Seam:** the single hashing function used by comment_post.rs:203, pool.rs:262–292 backfill, and admin/data.rs:141–147. Give `hash_ip` the secret by parameter (it already takes it) and have pool.rs read it from config-adjacent env once, not per-row.
**Plain-English shape of change:** "There is exactly one way to hash an IP, and tests promise it never changes its output for a given input and secret."
**Tests that would improve:** determinism (same input+secret → same hash, across repeated calls), salt changes output, `h:` prefix, hex length, IPv4/IPv6 handling, and a migration test that a v6-era DB's backfilled hashes equal `hash_ip` output (locking the two copies together — the test that catches drift before it happens).

### D6 — `/healthz` as a real readiness probe
**Module:** `src/http/mod.rs`.
**Seam:** health semantics behind the endpoint: `SELECT 1` through the pool, 200/503, exercised by Docker's HEALTHCHECK (Dockerfile:64–65, docker-compose.yml:61–66).
**Plain-English shape of change:** "'Green' means the database answers, not just that the process is alive."
**Tests that would improve:** healthz 200 on a healthy pool, 503 when the pool is closed (or the path is unreachable); a compose-level smoke test in CI (B15) asserting the container flips healthy within start-period — which also smoke-tests the Dockerfile.