# Per-ticket decisions worth keeping

One paragraph each for the calls a future reader is most likely to "fix" back.

**SafeFetcher is the only outbound door for untrusted URLs** (webmention sources, author-page avatar lookups). Every hop re-resolves and re-checks, redirects cap at 5, bodies cap at 1 MiB while streaming. The shared HTTP client serves operator-configured endpoints only (GitHub, webhooks, notifications, Turnstile) and must never fetch user-supplied URLs — the old redirect-policy client survives solely as a re-export shim.

**Restore replays history without events.** `Repo::restore` writes statuses directly: no `Moderation::transition`, no `ModerationSink` emissions, no compare-and-swap. An import replaying thousands of approved rows must not fire thousands of `status_changed` events; only live paths (admin calls, self-delete, reaction upserts) transition and emit.

**Gone needs two consecutive observations** (`MISSES_TO_TOMBSTONE = 2`, pinned by test). One backlink-less 200 flips the ledger to `gone` but deletes nothing; the second miss (or a 410) deletes every comment the source owns across all target paths. A re-ping always re-fetches first, so gone→alive resurrection restores to `pending` through the moderation machine.

**Import refuses into a live database by default.** When an exported comment or reaction ID holds different data than the stored row, the restore aborts before the first write; `"force": true` overwrites only after explicit review. An old backup never silently reverts live moderation decisions, and re-importing the same document stays idempotent.

**CommentIngress owns the 13-step ordering, and the order is the contract** (`src/ingress.rs` docblock; SPEC mirrors it): honeypot flag, Turnstile, daily cap, validate, hash-on-raw, sanitize, language gate on sanitized text, author/avatar resolve, parent check, token mint, atomic store, notify-then-webhook. Hashing the raw input while gating and extracting URLs from the sanitized text is deliberate — each consumer sees the text it needs, and no URLs from stripped tags persist.
