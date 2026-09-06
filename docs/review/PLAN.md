# Plan — implement all 10 candidates + 12 small wins, safely

## Spec (G1 — ≤50 lines)

- **Goal:** implement every review candidate (SafeFetcher, CommentIngress,
  Moderation, Unit-of-work, Restore, Notify, App::start, Validated Config,
  WebmentionProcessor, Migrations-as-data) plus small wins S1–S12, with no
  regressions and no live security holes left.
- **Scope:** `src/`, `tests/`, `migrations/schema.sql`, `SPEC.md`,
  `docs/`, `CHANGELOG.md`, new `docs/adr/`, `.github/workflows/ci.yml`
  (smoke tests only). No new features; no export-v1 break; no new deps
  unless a phase justifies one.
- **Non-goals:** logged-in users, voting/pinning, Disqus/WXR import, Postgres
  backend (roadmap pressure only — we keep their seams open, we don't build
  them). No `main` branch pushes without G5 approval.
- **Done looks like:** each ticket below lands green (`cargo fmt --check`,
  `cargo clippy -- -D warnings`, `cargo test` default + comments-only), with
  its hard tests failing on the pre-fix code (mutation/spot-check proof),
  docs updated in the same commit, and an independent reviewer sign-off.
- **Risks:** L — architecture + security + wide-blast-radius refactors.
  Rollback = revert the single phase commit. Export v1 stays importable.
  Migrations never downgrade (newer-than-binary still refuses).

## Ordering principle

Security holes first (no architecture churn), then foundation (config/test
assembly), then storage (the dependency everyone stands on), then the
pipelines that use storage, then assembly/docs. Wide refactors use
expand–contract so CI stays green batch to batch.

## Phase 0 — Safety harness (blocks everything)

- **T01 Baseline + branch.** Record `cargo test` (default + comments-only),
  clippy, fmt. Commit the current WIP or move it to
  `architecture-deepening` branch. Create `.scratch/arch-deepening/`.
- Seams: none (no behavior change). Proof: baseline log in the ticket.
- Docs: `docs/review/README.md` links this plan.

## Phase 1 — Live holes, no churn (all parallelizable, land in order)

- **T02 `ip_hash` single impl (S8).** One `hash_ip` used by storage, backfill,
  import. Stability tests lock output for (input, secret).
- **T03 Config validation 8a (no interface change).** Validate
  `PUBLIC_TARGET_ORIGIN` at load (kills two request/worker panics), unify bool
  parsing, fix `NOTIFY_BATCH_*` error labels, redact webhook URLs in
  `redacted_display` + fix the test that asserts them visible, warn on
  half-configured Telegram.
- **T04 `Config::default()` (7a).** Env defaults centralized; tests use
  `..Config::default()`. Loaner test: default == from_env(empty).
- **T05 SafeFetcher core (card 1 + S4 core).** New module: normalize →
  resolve_and_check → manual per-hop redirect re-check → hop cap 5 → byte cap
  → `FetchedDoc` (single parse). Avatar fetch routed through it. Old
  `fetch_url`/policy becomes internal or deleted.
- **T06 Discord escape hotfix + budget (6a).** `escape` for Discord
  (@everyone/mentions/markdown), per-channel size budgets. Adversarial tests.
- **T07 GitHub hardening (S5).** Wire `_timeout_ms`, cache 403/429 short-TTL.
- **T08 Diagnostics (S7).** `healthz` does `SELECT 1` (200/503); optional
  startup `quick_check`; actionable error for legacy duplicate unique index.
- **T09 Admin guards (S12).** Throttle login + batch/export; `Secure` +
  `__Host-` cookie; startup warn on public bind without proxy config.
- Hard tests (Phase 1): avatar SSRF (169.254.169.254/loopback/link-local →
  no connection + fallback); redirect-to-`localhost.` closed; 50-hop loop
  fail-closed; 10 MiB → TooLarge; garbage origin → ConfigError not panic;
  redaction asserts URLs absent; Discord `@everyone` neutralized via wiremock.
- Docs: `security.md` (SSRF guarantee, cookie, throttles), `deployment.md`
  (healthcheck, env), `CHANGELOG.md`.

## Phase 2 — HTTP honesty (blocked by T04)

- **T10 Routes module (S11) + governor honesty (S10).** Layer recipe in one
  constructor; CORS-vs-admin merge becomes a function boundary; governor takes
  (burst, window) with honest sustained-rate math; docs tables re-derived by
  test so 50/30/60/10→100/60/300/30 drift can't recur. 429 JSON shape unified.
- **T11 ClientIdentity seam (S9).** One normalized peer resolution
  (IPv4-mapped → v4); governors + Limiter + ip_hash consume it; `TRUST_PROXY`
  flips resolution without touching handlers. XFF-spoof tests.
- Docs: `architecture.md` (middleware), `api.md` (429 shape), `security.md`.

## Phase 3 — Storage foundation (blocked by Phase 1; T12→T13→T14→T15 linear, same files)

- **T12 Column mapping (S6).** One `COMMENT_COLUMNS` + parameterized builder;
  12 duplicated SQL strings collapse. Round-trip tests per query variant.
- **T13 Migrations as data (card 10).** `MIGRATIONS: &[&str]`, stamp = index,
  snapshot derived or parity-tested. Parity test: stepped v0 upgrade vs fresh
  install diff over `sqlite_master` + `table_info` (catches the v8 class).
- **T14 Typed storage errors (D5).** `Constraint` / `Busy`+`Io` (retryable) /
  `Other`. HTTP mapping unchanged. Import + reactions implement policy on the
  type, not substrings.
- **T15 Unit of work (card 4).** `Repo::with_conn`/transaction seam
  (`BEGIN IMMEDIATE`); comment+approve+URLs one commit; mention+seen one
  commit; export one read transaction; reaction approve CAS
  (`WHERE status='pending'`). Rollback + single-acquire + concurrency tests.
- Docs: `architecture.md` (database), `deployment.md` (backup atomicity).

## Phase 4 — Moderation machine (card 3, blocked by T15)

- **T16 `Moderation::transition(comment, to, actor)`.** `Status` enum; validity
  + effects + cascade in one place; sync/async webhook adapters replace the
  two copy-pasted loops and four hand-built payloads. Batch + self-delete fire
  exactly one event. Five whitelists collapse.
- Hard tests: batch fires `status_changed` (fails today); self-delete fires;
  exactly-once per path; invalid transition rejected; approve-vs-emoji-change
  race keeps the new emoji pending.
- Docs: `moderation-engine.md` (contract), `api.md` (events), SPEC (statuses).

## Phase 5 — Restore service (card 5, blocked by T14+T15+T16)

- **T17 `Repo::restore(export, policy) -> RestoreReport`.** uniform
  skip-and-count (never `?`-abort with counts lost); live-DB id-overlap
  refuse/warn; salt re-derivation beside hash logic; URL/profile row validation
  (length caps). Handler keeps auth + JSON only.
- Hard tests: FK-broken reaction skips (fails today); live-DB collision
  refuses; crash-mid-import → re-import idempotent; mixed counts correct;
  export-while-traffic consistent (single read txn).
- Docs: `deployment.md` (restore procedure, delete-token sensitivity),
  `architecture.md`, SPEC (export/import).

## Phase 6 — Submission + notification (blocked by T05+T15+T16; T18 parallel-safe)

- **T18 Notify full (card 6).** `Channel` adapter (format + deliver) + shared
  dispatch/retry/budget; batcher fixed-window from `opened_at`, one timer per
  window, `Clock` seam, drain on existing shutdown hook. `push` signature
  stays stable so this can run parallel to T19 on a separate worktree.
- **T19 CommentIngress (card 2 + S1+S2+S3).** `Ingress::submit(form, ctx)` owns
  ordering with `Notify`/`ModerationSink`/`UrlStore` effect seams (in-memory
  fakes in tests — two adapters justify each seam). Folds in: honeypot honors
  `HONEYPOT_FIELD` (S1), Identity normalization shared native/import/reactions
  + bidi strip + github_username validation + MAX_AUTHOR_LEN everywhere (S2),
  CSPRNG delete tokens cleared on re-approve + webhook signing (S3).
- Hard tests: ordering invariants (hash-on-raw, gate-on-sanitized,
  no URLs from stripped tags); native + webmention share store path;
  honeypot consumes quota + flagged; over-quota 429 before side effects;
  U+202E stripped; tokens unique/underivable; unsigned webhook rejected.
- Docs: `architecture.md` (pipeline), `security.md` (honeypot, tokens,
  webhook auth), `embed/README.md` (honeypot field), SPEC (pipeline steps
  renumbered once, truthfully).

## Phase 7 — Webmention depth (card 9, blocked by T05+T15+T16+T19)

- **T20 `WebmentionProcessor` + `SourceFetcher` + gone lifecycle (W5).**
  Processor owns policy; fetcher adapter (prod = SafeFetcher, tests = mock —
  `allow_loopback` dies); seen-state machine with grace window; multi-target
  reads scoped by (source, target); backlink normalization decided + tested;
  `is_new` keyed by the upsert pair; single parse of `FetchedDoc`.
- Hard tests: alive→gone→alive cycle; transient backlink-less 200 doesn't
  tombstone; update preserves status + refreshes content_hash; one source →
  two pages = two notifications; 410 deletes both; fragment/query/case match
  policy pinned.
- Docs: `architecture.md` (worker), SPEC (webmention flow), `security.md`
  (fetch limits).

## Phase 8 — Assembly + typed config + sweep (blocked by all above)

- **T21 Typed Config subgroups (8b, expand–contract).** Add typed accessors
  beside strings → migrate consumers in batches (notify, moderation, limits)
  → contract (remove strings). Consumers never re-derive enums.
- **T22 `App::start(config)` (7b).** One builder for main + all tests; raw SQL
  leaves `main.rs` for the repo layer; startup smoke test (temp DB →
  healthz); newer-schema fails loud at assembly; secret warning covered.
- **T23 Docs, ADRs, CI.** Three ADRs (single-process topology, export-v1
  contract, at-most-once notifications) + per-candidate ADRs where decided;
  `CONTEXT.md` only if domain terms changed (it shouldn't — these are
  modules, not domain); SPEC + all `docs/` reconciled; CI: compose smoke
  (up + healthy) + qemu `--version`/boot per Linux target; un-hardcode test
  counts in `development.md`.
- Docs: everything above; `CHANGELOG.md` release notes.

## Seams under test (TDD confirmation needed before any red cycle)

`SafeFetcher::fetch` · `Ingress::submit` (+ `Notify`, `ModerationSink`,
`UrlStore` fakes) · `Moderation::transition` (+ sink adapters) ·
`Repo::with_conn`/transaction · `Repo::restore` · `Channel` adapters +
batcher `Clock` · `App::start` · Config subgroup parsers ·
`WebmentionProcessor::process` (+ `SourceFetcher` mock) · `MIGRATIONS` parity.
No tests at internals; no horizontal slicing (one vertical slice per red→green).

## Subagent strategy (ship-quality: coordinator + workers + fresh reviewers)

- Coordinator (this session) plans, runs gates G0–G6, never writes feature
  code except XS hotfixes (T06 Discord escape, T09 cookie flag) directly.
- One worker subagent per ticket (general agent, fresh session), given spec +
  plan + checks (`fmt`, `clippy -D warnings`, target test file, then suite).
  Workers grade their own tests by deliberate breakage (spot-check fail).
- One reviewer subagent per phase (fresh context, never the author) judging
  spec/diff/security/docs; critical+major must be fixed + re-checked.
- Parallelism: Phase 1 tickets parallel (disjoint files, interface-stable);
  T18 may run parallel to T19 on a separate worktree (stable `push`
  signature); everything else follows the blocking edges (frontier rule).
- G5 ship gate per phase: diff + proof + review + risks + rollback, approved
  by you before commit. Commits: one per ticket, message references ticket.

## What I need from you

1. Approve the order + blocking edges (or request merges/splits).
2. Confirm the seams list above (TDD requires pre-agreed seams).
3. Confirm risk = L with full gates (or waive to faster mode for hotfixes).
