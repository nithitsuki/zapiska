# Review — Native comment pipeline & moderation

Area: native comments, threaded replies, reactions, moderation statuses, spam
controls. Deep-module vocabulary per `docs/review/README.md` (module /
interface / depth / seam / adapter / leverage / locality).

## What I read

- `src/http/comment_post.rs` — the 13-step native pipeline in one handler
  (`create_comment`), self-service delete, `resolve_parent`, `resolve_author`,
  `resolve_avatar` (incl. server-side page fetch), `generate_delete_token`,
  1200 lines of tests.
- `src/http/reactions.rs` — `add_reaction` / `remove_reaction`, identity =
  admin or IP hash, duplicated sync/async webhook logic.
- `src/http/admin/moderate.rs` — single + batch status change; batch path does
  **not** fire the `comment.status_changed` webhook.
- `src/http/admin/comments.rs`, `src/http/admin/lookup.rs` — pending queue,
  list by status, chain lookup, URL/domain lookup, bulk context.
- `src/http/admin/reactions.rs` — reaction list + moderate single/batch (batch
  *does* fire its status webhook — asymmetry with comments).
- `src/validate.rs` — target_path + URL validation, control-char strip,
  clamp (a genuinely deep leaf module).
- `src/sanitize.rs` — ammonia clean + truncate, content hash (SipHash via
  `DefaultHasher`), URL extraction (raw-bytes, `href="`-only).
- `src/language.rs` — compiled `LanguageGate` (whatlang), emoji policy; well
  isolated behind one interface.
- `src/turnstile.rs`, `src/ip_hash.rs`, `src/timeutil.rs` — small leaf modules.
- `src/config.rs` (moderation/threading/reaction/language parts), `src/state.rs`
  (Limiter, `ip_daily_key`), `src/db/repo/comments.rs`, `src/db/repo/reactions.rs`,
  `migrations/schema.sql` (FKs ON, no dedup/`content_hash` index),
  `src/http/layers.rs` + `src/http/mod.rs` (route→governor wiring),
  `src/http/admin/auth.rs` (token/cookie), `src/http/webhook.rs` (fire-and-forget only).
- `docs/moderation-engine.md`, `docs/security.md`, `SPEC.md` (native,
  threaded, reactions, language gate, admin routes, import).
- `embed/comments.js` — widget sends hidden `website` honeypot and
  `author_url`, `cf-turnstile-response`.

## Friction

### F1. The "pipeline" is not a module — it's a 300-line handler with hand-maintained numbering

`create_comment` (`src/http/comment_post.rs:55-354`) is a flat sequence:
honeypot → Turnstile → per-IP daily cap → validate → hash → sanitize →
language gate → GitHub/author resolve → parent resolve → delete token → IP
hash → insert → auto-approve → URL extract → notifier → moderation webhook
(sync branch builds an enriched payload, fetches submitter stats + parent
chain inline). The step comments are internally inconsistent — two "4."s
(lines 173, 182), two "6."s (193, 200), three "9."s + a "9.5" (236, 241, 247)
— and they do not match the SPEC's 13-step numbering either. Depth: the
*interface* to the outside world is tiny (a `CommentForm` plus `State` in,
a 201 + `{delete_token, status}` out), but the handler exposes every step to
the maintainer, who must hold ordering contracts in their head (hash on *raw*
content, sanitize before language gate, URLs extracted from *raw* content,
notifier before the sync webhook decision).

**Deletion test:** delete `create_comment` and nothing remains with that
behaviour — resurrecting native ingestion means re-writing all 13 steps and
their ordering by hand. A `CommentIngress` module owning `submit()` would
pass with a one-line call. The assembly here also makes the *webmention*
handler (which builds its own `NewComment` + `update_status` flow in
`src/http/webmention_post.rs`) a parallel, divergent implementation of the
same concept.

### F2. The moderation concept is scattered — statuses are bare strings, transitions fire inconsistent effects

The four-status vocabulary (`pending/approved/spam/deleted`) exists as raw
strings in ~15+ sites. The *same* whitelist `matches!(action, "approved" |
"spam" | "deleted" | "pending")` is written at least five times:
`moderate.rs:96-98`, `comment_post.rs:317`, `reactions.rs:115`,
`admin/reactions.rs:110-112`, plus the sync-decision branch at
`comment_post.rs:317` and `reactions.rs:115`. `Config::default_comment_status`
(`config.rs:47`) and `MODERATION_WEBHOOK_MODE` are also strings.

Who may transition, and what fires on transition, is decided independently in
each caller:

- `repo.update_status` (`db/repo/comments.rs:430`) — bare write, fires nothing.
- `moderate()` fires `comment.status_changed` (`moderate.rs:158`), but
  `moderate_single`/`moderate_batch` (`moderate.rs:62-77`) — the moderation
  engine's **documented polling path** (`docs/moderation-engine.md:256`) —
  change status with **no webhook at all**.
- `delete_by_token` (`comment_post.rs:382-396` + `db/repo/comments.rs:489`)
  transitions to `deleted` with no status webhook.
- Reactions get a parallel machine (`update_reaction_status`,
  `upsert_reaction`, `delete_reaction`) with their own webhook shape
  (`admin/reactions.rs:115-136`), and their batch **does** fire the event —
  the asymmetry means an external consumer watching `comment.status_changed`
  sees single moderation but not batch or self-deletes.

Webhook payloads are hand-built `serde_json::json!` blocks in four places
(`comment_post.rs:290-303`, `reactions.rs:93-102`, `moderate.rs:113-119`,
`admin/reactions.rs:124-132`) with no shared schema, and the sync mode's
10s-timeout + decision-parse loop is copy-pasted between `comment_post.rs:305-345`
and `reactions.rs:103-130`.

**Deletion test:** delete `moderate.rs`'s `fire_status_webhook` and nothing
requires it — because nothing central owns "a status changed". The webhook
contract lives in *docs* and in each caller's goodwill. A single `Moderation`
module owning `enum Status`, `transition()`, and the sink would make this a
seam instead of a convention.

### F3. Validation is duplicated between handlers (and drifts)

- Native comments enforce "comment must be approved / same path / depth <
  max" in `resolve_parent` (`comment_post.rs:408-454`); reactions re-enforce
  "comment must be approved" in `add_reaction` (`reactions.rs:66-77`) with a
  near-identical error string ("parent comment {pid} is not approved …" vs
  "comment {id} is not approved …"). No shared rule.
- Author-name policy lives in the handler (`comment_post.rs:140-152`: strip
  control chars, trim, empty + GitHub fallback, `chars().count() >
  max_author_len`), and a *second, divergent* copy lives in import
  (`admin/data.rs:298-306`: rejects empty names outright, truncates at a
  **hardcoded 100** instead of `MAX_AUTHOR_LEN`, no GitHub fallback).
- `validate_http_url`'s doc says "with a host" (`validate.rs:60`) but the
  implementation checks only the scheme (`validate.rs:61-68`) — it never
  reads `host_str()`. Either the doc or the check is wrong; there is no
  central "valid ident" for author identity that all entry points share.

### F4. The content-safety "pipeline" is a string of shallow calls, with a split-brain contract

Six safety functions (honeypot flag, Turnstile verify, sanitize+truncate,
language gate, content hash, URL extraction) are called inline in a fixed
order; the ordering contracts exist nowhere but this handler and
`docs/security.md`. The subtle ones:

- `content_hash` runs on **raw** input before sanitize (`comment_post.rs:160`);
  `extract_urls` also scans **raw** form content (`comment_post.rs:242`),
  which per `docs/security.md:160-161` is deliberate — but it means URLs
  inside `<script>` blocks or disallowed elements get stored as URL rows
  even though ammonia will strip the tag from the saved content, and
  HTML-entity-encoded hrefs (`&amp;`) hash differently from what the browser
  decodes.
- honeypot detection reads `form.website` regardless of the loaded
  `HONEYPOT_FIELD` config (see B2).

None of these are a *module*; each is a function the handler must call at the
right time in the right order. The one genuinely deep piece here —
`LanguageGate` (`language.rs`) — is a good seam model: compiled at start,
one `check()` interface, engine isolated, unit-tested.

### F5. Author resolution is a leaky seam with an SSRF-shaped hole

`resolve_avatar` (`comment_post.rs:496-556`) priority 2 calls
`fetch_page_avatar` (`comment_post.rs:561-591`), which does a server-side
`GET` of the **user-supplied `author_url`** (4s timeout, page fetch, favicon
parse). This runs in the default build (`features = ["comments",
"webmentions"]`, `SPEC.md:44-61`). The webmention worker has a whole SSRF
guard (host allow-list, IP-range blocking — `docs/security.md:16-32`), but
this avatar path does **zero** checking: no `localhost`/private-range
rejection, no redirect policy. An unauthenticated commenter can point
`author_url` at `http://169.254.169.254/…` or any internal host and get the
server to fetch it, and can use the endpoint as a hit-probe against internal
ports. This is the single biggest robustness gap in the pipeline.

### F6. Identity & length semantics are only partially centralized

- `strip_control_chars` removes only `Cc` characters (`validate.rs:71-73`);
  format characters that enable spoofing — U+202E RTL override, U+200B
  zero-width, bidi isolates — survive into `author_name` and
  `github_username`. `github_username` is never control-stripped at all
  (only `author_name` is, `comment_post.rs:140`) and is interpolated into
  `https://github.com/{gh}` and a DiceBear seed unsanitized
  (`comment_post.rs:471, 548-551`).
- `MAX_CONTENT_LEN` is counted in `chars()` (`sanitize.rs:8`) while the body
  limit `MAX_BODY_SIZE` is bytes (`config.rs:225`); a fully-emoji comment is
  ~3× percent-encoded (12 bytes/char in the form body), so the *achievable*
  length is ~680 emoji chars or ~1360 Cyrillic chars, not 2000 — the
  advertised limit is only true for ASCII. Combining-mark sequences are
  counted per `char`, not per grapheme.

## Bulletproofing brainstorm

| # | Scenario | Worst case | L | I | Current mitigation | Gap |
|---|---|---|---|---|---|---|
| B1 | **SSRF via `author_url` avatar fetch** — post a comment with `author_url=http://169.254.169.254/latest/meta-data/` (or an internal admin port) | Cloud metadata exfil; internal host probing; blind port scan from the server IP | **High** (unauthenticated, default build, trivial payload) | **High** | 4s timeout, non-2xx abort, http/https-only (`comment_post.rs:561-570`) | No SSRF guard at all on this path — the webmention worker's host/IP checker (`docs/security.md:16-32`) is not reused; redirects unchecked |
| B2 | **Honeypot field mismatch** — operator sets `HONEYPOT_FIELD=company` per `SPEC.md:119-120`, renames the widget's hidden input to `company`; handler still reads `form.website` (`comment_post.rs:35,63`) | Honeypot is inert; spam streams through unflagged while the engine auto-spams genuinely `honeypot=true` comments (`docs/moderation-engine.md:283-287`) | **Med** (only when configured) | **Med** | Config + SPEC + security.md *document* the mismatch (`security.md:143-144`) | Dead config that silently disables a spam control; no test asserts the field name is honored |
| B3 | **Delete-token brute force** — token is 16 hex chars of `DefaultHasher` (SipHash, fixed keys) over (peer IP, time-ns, process counter) (`comment_post.rs:360-373`) | Offline crack of the ns-window for a known-IP comment (~2^31-2^34 candidates ≈ seconds of CPU); online attempts rate-limited but IPv6 rotation defeats per-IP limiting | **Low-Med** | **Med** | Delete route is rate-limited with native governor; same-404 for missing/wrong token (`security.md:71-79`) | Token is deterministic, not per-comment random; acknowledged in docs as a future fix; no per-comment secret, no CSPRNG |
| B4 | **Sync webhook stalls or lies** — sync mode holds the request up to 10s (`comment_post.rs:310`); webhook returns `action` after the notifier already announced the comment | Comment announced to Telegram/Slack as new, then silently spam/deleted; slow webhook ties up the IP's whole rate-limit budget | **Low-Med** | **Med** | 10s timeout; status applied only for whitelisted actions | Notifier fires *before* the sync decision (`comment_post.rs:247-262` vs 305-345); async mode is single-shot fire-and-forget (`webhook.rs:13-26`) — no retry/backoff, process restart loses in-flight events |
| B5 | **Reaction churn / spam revival** — a `spam`/`deleted` reaction re-POSTed by the same identifier flips back to `pending` (`db/repo/reactions.rs:62-80`); in `anyone` mode identity is only an IP hash (`reactions.rs:82`) | Spam decisions silently undone; webhook storm per emoji flip; rotating IPv6 proxies mint unlimited identities; shares the native comment budget (`mod.rs:109-134`) with no daily cap | **Med** (anyone mode is opt-in, "highly discouraged") | **Med** | `reactions_set` allow-list; UNIQUE(comment_id, identifier); same-reaction no-op keeps status | No cooldown on re-submission after a spam decision; anyone-mode identity trivially forgeable per `config.rs:104-107`'s own warning |
| B6 | **Language-gate false positives** — short text ("Hi"), code snippets, mixed-script comments are "unknown" and rejected under whitelist + `if_unknown` (`language.rs:126-139`, test at 382-389); detection is confidence-0.5 heuristic | Legit comments hard-rejected with 400, never stored, no appeal path; a few foreign words flip detection on a whitelisted site | **Med** (only when gate enabled) | **Med** | Well-isolated gate, configurable policies, clear 400 message | Gate is a hard block with no "quarantine as pending" tier — a probabilistic detector gets the strictest failure mode; code/URL-heavy comments are structurally undetectable |
| B7 | **Unicode length & body-limit mismatch** — emoji/Cyrillic comments hit `MAX_BODY_SIZE` (8192 B) long before the advertised 2000 chars (emoji ≈ 12 form bytes/char) | Users cannot post near-limit comments in non-ASCII languages; combining marks / bidi chars bypass display-length expectations | **Med** | **Low-Med** | Body limit catches the excess with 413; chars-count truncation is correct for ASCII | Effective max is script-dependent; no grapheme-aware counting; no test asserts non-ASCII length parity |
| B8 | **Parent race / orphan threads** — reply submitted while parent is concurrently marked spam/deleted; `resolve_parent` checks status (`comment_post.rs:431-436`) then insert is a separate statement | Approved child rendered publicly with `parent_id` pointing at a non-approved comment (`comments_read.rs:124`); admin chain lookup raises an internal error (`db/repo/comments.rs:474-478`) | **Low** (small window) | **Low-Med** | Parent must be approved at submit time; rate limits shrink the window | Check-then-insert is not a transaction; deleting/spamming a parent never cascades to children (soft-delete, FK RESTRICT) — subtree cleanup is left to the engine |
| B9 | **Double-submit duplicate rows** — two concurrent identical POSTs insert two rows; `content_hash` is computed but never checked (`comment_post.rs:160`, `SPEC.md:204`) | Duplicate pending comments in the queue; duplicate admin notifications | **Med** (double-click, retry clients) | **Low** | Rate limit; content_hash available for the engine to dedup by lookup | No unique index, no idempotency key, no client-supplied request ID; engine must dedup *post hoc* |
| B10 | **Delete-token then re-approve** — after self-delete, the token stays in the row (`db/repo/comments.rs:489-502`); admin re-approve reopens self-delete; delete fires no `status_changed` | Confusing revivify ability; cache-skew watchers miss the delete | **Med** (engines use the documented batch path) | **Low** | `status != 'deleted'` guard on delete | Token not cleared on re-approve; no webhook on the delete transition (see F2) |
| B11 | **`content_hash` is a 64-bit SipHash with fixed keys** (`sanitize.rs:37-39`) | Offline birthday ~2^32 finds colliding content strings → a hash-keyed moderation rule false-positives or is evaded | **Low** (needs an adversarial moderation engine) | **Low-Med** | Hash is lookup-only per spec | Not a cryptographic digest; `DefaultHasher` keys are public/constant |
| B12 | **URL rows from raw, pre-sanitize content** (`comment_post.rs:242`, `security.md:160-161`) | Junk/ghost URL rows (URLs inside stripped tags; `&amp;`-encoded hrefs hash differently from decoded links) pollute domain lookup and mislead engines | **Med** | **Low** | Raw-byte scan, dedup by hash, `href="`-only, absolute http(s) only | Extracts URLs that never render; entity decoding before hashing is absent |
| B13 | **Admin cookie / batch limits** — session cookie has no `Secure` flag (`auth.rs:28-33`); batch moderate is ungoverned (`mod.rs:34-93` wires the governor only to single moderate) | Cookie sent over plain HTTP on a LAN admin; stolen token can batch-moderate without throttle | **Low** | **Med** | HttpOnly + SameSite=Lax; constant-time compare; single route governor | No `Secure`; batch is the engine's recommended path and is unlimited — acceptable for a trusted token, but there is no admin audit trail of who/what |
| B14 | **Import bypass / drift** — import re-sanitizes but skips the language gate and honeypot semantics, truncates author at hardcoded 100 (`admin/data.rs:304`) regardless of `MAX_AUTHOR_LEN`, restores `approved` statuses for content that today's rules would reject | Rotated `IP_HASH_SECRET` orphans anyone-mode reaction identities (documented warning, `SPEC.md:363-366`); imported disallowed-language comments appear approved | **Low** (admin-only) | **Low-Med** | Field re-validation, parent-before-child ordering, re-sanitize, hash re-derivation | Rules drift from the live pipeline (F3); no re-run of the language gate; secret-rotation identity terms are not enforced |
| B15 | **Daily-cap charged before validation** (`comment_post.rs:114-129` before 135) | Garbage requests consume the day's budget; a NAT-shared IP's innocent user gets throttled by a bot on the same IP | **Low** | **Low** | Per-IP key, midnight reset, Turnstile failures happen *before* the cap so they don't consume it | Validation happens after the increment; cap is per-IP by design (documented) — collateral-DoS only |

## Candidate deepening opportunities

1. **`CommentIngress` — submission pipeline as one deep module.** Seam:
   replace the inline steps in `create_comment` with `Ingress::submit(form,
   ctx) -> StoredComment` that owns ordering (hash-on-raw →
   sanitize → language → parent check → store → effects). Effects become a
   small trait (`Notify`, `ModerationSink`, `UrlStore`) so handlers stop
   being the orchestrator and webmentions can reuse the store half.
   *Tests that would improve:* ordering invariants (content_hash reflects raw
   input; language gate sees sanitized text; URL rows never come from tags
   ammonia strips); a webmention and native comment going through the same
   storage path.

2. **`Moderation` — status machine owning transitions + effects.** Seam:
   `enum Status`, `fn transition(comment, to, actor)` centralizing validity,
   the webhook fire, and the missing cascade. Adapters: a
   `ModerationSink` trait with sync/async webhook adapters replacing the
   two copy-pasted 10s-sync loops (`comment_post.rs:305-345`,
   `reactions.rs:103-130`). *Tests that would improve:* batch moderate fires
   `comment.status_changed` (fails today); self-delete fires it too; every
   path emits exactly one event; the five copies of the action whitelist
   collapse into the enum.

3. **SSRF-safe outbound fetcher shared with the webmention worker.**
   Seam: reuse the worker's host/IP guard (per `docs/security.md:16-32`) as
   a `SafeFetcher` adapter used by both `fetch_page_avatar` and webmention
   fetch. *Tests that would improve:* `author_url` →
   `http://169.254.169.254/`, `http://localhost/`, private & link-local
   ranges refused before any network I/O; redirect targets re-checked —
   mirroring the worker's existing tests.

4. **Honeypot field plumbing.** Make `honeypot` detection read
   `config.honeypot_field` (fallback `website`) at deserialization, and have
   the widget emit the configured name. *Tests that would improve:*
   set `HONEYPOT_FIELD=company` → filling `company` flags and `website` no
   longer does; the `website`-as-real-field confusion is gone.

5. **`Identity` / normalization module.** Centralize author rules shared by
   native, reactions, and import: strip `Cc` **and** spoofing `Cf`/bidi
   characters, validate `github_username` shape before interpolating into
   URLs, use `MAX_AUTHOR_LEN` everywhere (kills the hardcoded 100 in
   `admin/data.rs:304`), fix `validate_http_url`'s claim-vs-behaviour
   (`validate.rs:60-68`). *Tests that would improve:* U+202E/U+200B stripped
   from names; `github_username="a<b>\n"` rejected; import honors config
   length; import and native accept/reject the same author set (parity
   table-test).

6. **Security hardening of the two trust edges.** (a) Replace
   `generate_delete_token`'s `DefaultHasher` with a CSPRNG 128-bit token and
   clear it on re-approve. (b) Sign webhook payloads with an HMAC secret (or
   at minimum stop shipping `delete_token` in `comment.created` unless
   explicitly enabled — `comment_post.rs:296`). *Tests that would improve:*
   tokens unique and not derivable from (ip, time); webhook consumer rejects
   unsigned payloads; delete token invalidated after an admin re-approves a
   previously self-deleted comment.

---

*Note: no code was modified; this is analysis only, for the review process in
`docs/review/README.md`.*