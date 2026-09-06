# Architecture Review — 2026-09-06

Running document for the base-architecture review of zapiska. The goal: surface
architectural friction and robustness gaps across **all** realistic use cases,
then deepen the modules that earn it.

## Domain vocabulary

zapiska is a self-hosted comment and webmention engine (Rust, Axum, Tokio,
SQLite). Core domain concepts (see `SPEC.md` and `docs/`):

- **comment** — a native comment or a webmention, stored in `comments`.
- **reaction** — one per comment per identity, moderation-statused.
- **webmention** — W3C mention, queued to a worker, source-fetched.
- **status** — `pending` / `approved` / `spam` / `deleted` lifecycle.
- **admin** — bearer-token or session-cookie moderation surface.
- **target path** — local path on the host site a comment attaches to.
- **identity** — author, IP, IP hash, GitHub profile, reaction identifier.

## Architecture vocabulary (deep modules)

From `/codebase-design`: **module** (anything with an interface + implementation),
**interface** (everything a caller must know), **depth** (behaviour per unit of
interface), **seam** (where behaviour can be altered without editing there),
**adapter** (a concrete thing satisfying an interface at a seam),
**leverage** (caller payoff from depth), **locality** (maintainer payoff:
change/bugs/knowledge concentrate).

## Process

1. Explore + directed research (parallel subagents) → this folder's findings.
2. Synthesize candidates → HTML report (in temp) + candidate tracker below.
3. Grilling loop on the picked candidate → ADRs as decisions crystallize.
4. **Buildout (approved 2026-09-06): `docs/review/PLAN.md` + `.scratch/arch-deepening/issues/` (23 tickets, T01–T23) — branch `arch/deepening`, risk L, full gates.**

## Baseline (T01, branch `arch/deepening` @ `294a7e1`)

`cargo fmt --check` clean · `cargo clippy --all-targets` clean ·
default: lib **334** + e2e **4** + pentest **14** + worker_notify **1** = **353** green ·
comments-only: lib **275** + e2e **3** = **278** green. Full log:
`.scratch/arch-deepening/baseline.log`.

## Findings — markdown trackers

| Area | File | Status | Contribution |
|---|---|---|---|
| Roadmap pressure (future features) | `docs/review/00-roadmap-pressure.md` | done | identity first-class, import adapter seams, storage seam, export stability |
| Data & storage layer | `docs/review/01-data-storage.md` | done | unit-of-work, migrations-as-data, restore service, column mapping, typed errors, diagnostics |
| HTTP surface & middleware | `docs/review/02-http-surface.md` | done | routes module, ClientIdentity seam, governor honesty, submission service, `Config::default()`, docs guards |
| Native comment pipeline & moderation | `docs/review/03-comment-pipeline.md` | done | CommentIngress, Moderation machine, SafeFetcher, honeypot plumbing, Identity normalization, trust-edge hardening |
| Webmention worker & outbound fetch | `docs/review/04-webmentions.md` | done | SafeFetcher (confirmed C3), FetchedDoc, WebmentionProcessor, gone-resurrection lifecycle, GitHub hardening |
| Notifications, ops, deploy, tests | `docs/review/05-ops-tests.md` | done | channel seam, batcher clock/timer, `App::start`, Config subgroups, single `ip_hash`, healthz |

## Candidate tracker

Consolidated from findings 00–05. Duplicates merged; strengths assigned by the
reviewer after the research phase. Full detail in the HTML report
(`/tmp/architecture-review-*.html`).

| # | Candidate | Strength | Files | Status |
|---|---|---|---|---|
| **SafeFetcher** | One guarded outbound door — SSRF-safe fetch (merged C3+W1+W2) | **Strong** | `ssrf.rs`, `reqwest_client.rs`, `comment_post.rs`, `worker.rs` | confirmed by 2 researchers — live hole in default build |
| **CommentIngress** | Submission pipeline as one deep module (merged C1+H4) | **Strong** | `comment_post.rs`, `webmention_post.rs`, `reactions.rs` | proposed |
| **Moderation** | Status machine owning transitions + effects | **Strong** | `admin/moderate.rs`, `reactions.rs`, repo | proposed — batch webhook asymmetry is a documented-contract break |
| **Unit-of-work** | Connection-scoped transaction seam in repo | **Strong** | `db/repo/mod.rs`, all repo files | proposed — root cause of atomicity gaps |
| **Restore service** | Export/import orchestration in db layer (D3; uses D5) | **Strong** | `http/admin/data.rs`, db layer | proposed — import-into-live clobbers IDs today |
| **Notify module** | Named channel seam + batcher clock (O1+O2) | **Strong** | `notify/*` | proposed — Discord `@everyone` injection is high-likelihood |
| **App::start** | Assembly seam + `Config::default()` (O3+H5) | Worth exploring | `main.rs`, `test_support.rs`, tests | proposed |
| **Validated Config** | Typed subgroups, fail-at-boot, redaction (O4) | **Strong** | `config.rs` | proposed — `PUBLIC_TARGET_ORIGIN` panics in request handlers today |
| **WebmentionProcessor** | Kill god loop + gone-resurrection fix (W4+W5) | Worth exploring | `worker.rs`, `db/repo/webmentions.rs` | proposed — W5 is a real domain bug |
| **Migrations as data** | Single schema source of truth, parity test (D2) | Worth exploring | `db/pool.rs`, `migrations/schema.sql` | proposed — v8 near-miss proves the drift risk |

### Small wins (fold into whichever candidate gets picked, or do standalone)

| # | Candidate | Strength | Files |
|---|---|---|---|
| S1 | Honeypot field plumbing — honor `HONEYPOT_FIELD` (C4) | Worth exploring | `comment_post.rs`, `config.rs`, `embed/comments.js` |
| S2 | Identity normalization — author rules shared by native/import/reactions (C5) | Worth exploring | `comment_post.rs`, `admin/data.rs`, `validate.rs` |
| S3 | Trust-edge hardening — CSPRNG delete tokens, webhook signing (C6) | Worth exploring | `comment_post.rs`, `webhook.rs` |
| S4 | FetchedDoc — parse once, byte cap (W3) | Worth exploring | `mf2.rs`, `worker.rs` |
| S5 | GitHub lookup hardening — wire `_timeout_ms`, cache 400-class misses (W6) | Worth exploring | `github.rs` |
| S6 | Concentrate comment column mapping — kill 12 duplicated SQL strings (D4) | Worth exploring | `db/repo/comments.rs` |
| S7 | Operator diagnostics — `quick_check`, DB-touching healthz (D6+O6) | Worth exploring | `main.rs`, `http/mod.rs` |
| S8 | `ip_hash` single implementation, stability-locked (O5) | Worth exploring | `ip_hash.rs`, `db/pool.rs` |
| S9 | ClientIdentity seam — one normalized peer resolution (H2) | Speculative | `http/*`, `state.rs`, `ip_hash.rs` |
| S10 | Governor knobs honest semantics + docs re-derived (H3) | Speculative | `http/layers.rs`, docs |
| S11 | `routes` module owning the layer recipe (H1) | Speculative | `http/mod.rs`, `http/*` |
| S12 | Admin rate-limit coverage — batch/export/login guards + `Secure` cookie (H6) | Speculative | `http/admin/*`, docs |

### ADR opportunities (decisions worth recording so future reviews don't re-litigate)

- **Single-process topology is the supported deployment** (in-memory batcher,
  limiter, governors). Either enforce with an advisory lock or record the
  decision with a "don't scale horizontally" note.
- **Export v1 is a load-bearing compatibility contract** — it is the only
  portable path between storage backends and restore targets. Stability is a
  feature.
- **Notification delivery is at-most-once and non-blocking** — a deliberate
  trade-off; worth recording before someone "fixes" it with retry queues.