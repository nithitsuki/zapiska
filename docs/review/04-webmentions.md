# Webmention worker & outbound fetch — findings

Area review of the webmention ingress → bounded channel → worker → SSRF-safe
fetch → microformats parse → store pipeline, plus the surrounding outbound HTTP
(GitHub lookup, avatar fetch) and the `webmention_seen` ledger.

## What I read

- `src/worker.rs` — job types, `spawn_worker_for_state`, `process_job` (the whole
  pipeline), author/GitHub resolution helpers, `derive_target_path`,
  `handle_gone_source`.
- `src/http/webmention_post.rs` — ingress: URL scheme checks, per-domain hourly
  cap, origin match, `source != target`, `try_send` → 202 / 503 / 500.
- `src/ssrf.rs` — `BLOCKED_NETS`, `is_blocked_ip`, `is_blocked_host`,
  `resolve_and_check`, `ssrf_safe_redirect_policy`, `registrable_domain`.
- `src/http/reqwest_client.rs` — `build_client`, `fetch_url`, `FetchError`,
  `is_loopback`.
- `src/mf2.rs` — selector statics, `has_backlink`, `urls_match`, `parse_h_entry`,
  `parse_author`, `extract_photo`.
- `src/avatar.rs` — `best_favicon` + `og_image` (pure parser, *no HTTP*).
- `src/github.rs` — `GitHubLookup` trait, `StubGitHub`, `RealGitHub` (cache +
  API), `extract_github_username`, naive ISO8601 helper.
- `src/db/repo/webmentions.rs` — `get_webmention_seen`, `upsert_webmention_seen`,
  `list_all_webmention_seen`.
- `src/db/repo/github_profiles.rs` — positive/negative cache storage.
- `src/config.rs` — `fetch_timeout_ms` (default 4000), `worker_backlog` (default
  64), `max_webmentions_per_domain_per_hour` (default 10),
  `RATE_LIMIT_WEBMENTION` 60/60.
- `docs/security.md` (SSRF, rate-limit, shutdown sections), `SPEC.md`
  (Webmentions, `webmention_seen`, shutdown, out-of-scope), `docs/architecture.md`
  (Webmention flow, client notes, shutdown).
- Call sites: `src/main.rs` (client build, worker spawn), `src/state.rs` (shared
  `http_client`), `src/http/comment_post.rs` (`resolve_avatar` →
  `fetch_page_avatar`), `src/db/repo/comments.rs` (`upsert_by_source`,
  `get_comment_by_source`), `tests/worker_notify.rs`.
- `Cargo.toml` — reqwest 0.13.4 features `["json", "form"]` (defaults stay; **no
  `gzip`**, so no auto-decompression — decompression-bomb concern mostly N/A).

Top-level verdict: the *blocklist* is a genuinely deep, well-tested module; the
*SSRF-safety guarantee* around it is split across files and is enforced only by
calling convention — and exactly one production call site (native-comment avatar
fetch) violates it on the first hop. The worker loop is a god-loop with a test
escape hatch threaded through it, and the `webmention_seen` gone-tombstone has a
resurrection bug.

## Friction

### 1. `ssrf.rs` + `reqwest_client.rs` — safety is a property of a function, not of the client, and one caller doesn't call it

The coherent deep thing here is the *blocklist*: `BLOCKED_NETS`
(ssrf.rs:18-37), `is_blocked_ip` with IPv4-mapped decoding (ssrf.rs:40-53),
`is_blocked_host` (ssrf.rs:56-62). That is deep knowledge with a tiny interface
and excellent unit tests — keep it.

But "SSRF-safe fetch" is a **guarantee that leaks across two files and is only
true when the caller uses the right function**. `build_client` (reqwest_client.rs:13-24)
bakes in the redirect policy so the `Client` *looks* safe; `fetch_url`
(reqwest_client.rs:32-72) is what actually performs `resolve_and_check`
(reqwest_client.rs:48, which hits `lookup_host` at ssrf.rs:107) before the
connect. Nothing in the type system says "this client is only safe if you call
`fetch_url`" — the comment on the shared client (state.rs:28-30) even claims it
is used for "all outbound requests," which is false.

The redirect policy (ssrf.rs:122-139) is a *shallow* belt-and-suspenders: it is
synchronous, so it can only check host strings and literal IPs, never resolve
DNS. Its own comment admits this (ssrf.rs:129-131). So the vector it is meant
to close — a redirect into a private network — stays open whenever the redirect
target is a hostname (see Bulletproofing #2), and the docs explicitly disclaim
rebinding defense (security.md:30-31, architecture.md:157-159). The policy is
the symptom of missing capability (async resolution per hop), not a real seam.

**Deletion test:** delete `fetch_url` and the first-hop check disappears
entirely — nothing else calls `resolve_and_check` for the webmention path.
Delete the redirect policy and the client degrades to "do not treat as complete
defence" per the docs. Delete the blocklist and both collapse. The blocklist is
deep; the policy is shallow; the *guarantee* is the missing module — it lives in
neither file, it lives in a convention that one caller already broke.

### 2. `comment_post.rs::fetch_page_avatar` — production SSRF bypass on the first hop (the broken convention)

`fetch_page_avatar` (comment_post.rs:561-591) does:

```rust
let resp = http_client.get(url.as_str()).timeout(...).send().await.ok()?;
```

with **no `resolve_and_check`** before the request. The URL comes from the
native commenter's `author_url` form field (resolve_avatar priority 2,
comment_post.rs:512-521), which is validated only for *format* (absolute
http(s) with a host). The shared client's redirect policy does not apply to the
first hop. So `author_url=https://169.254.169.254/...`, `http://10.0.0.5/...`,
or `http://[::1]/...` is fetched directly from the origin — cloud metadata,
internal services, loopback. This is the seam between `avatar.rs` (pure parser,
fine — the correct place for favicon *selection* logic) and the caller
(comment_post.rs), which re-implements its own ad-hoc fetch instead of going
through `fetch_url`. **Deletion test:** delete `fetch_url` and nothing else
guards this path; delete `avatar.rs` and the parse is easy to re-add inline —
the module boundary is between "parse" (good) and "fetch" (unguarded).

### 3. `worker.rs::process_job` — the god loop with a test escape hatch in the interface

`process_job` (worker.rs:94-213) takes 8 arguments
(`#[allow(clippy::too_many_arguments)]` at worker.rs:93 and 57) and, in one
function, owns: idempotency/tombstone policy (106-120), fetch policy
(123-130), backlink policy (133-144), parse policy (147), author enrichment
(policy: dicebear fallbacks at 229-246, GitHub at 258-270), sanitize policy
(154-159), store policy — upsert + `is_new` notification suppression (164-201)
— and seen-ledger transitions (204-209). Any change to "what counts as a valid
mention" touches this one loop.

`allow_loopback: bool` is threaded through the *production* signature
(worker.rs:101) down into `fetch_url` (reqwest_client.rs:32-36, 47) purely so
integration tests can hit wiremock on 127.0.0.1 (tests/worker_notify.rs:158,
188). It is a wide interface: a boolean that flips a security invariant, forced
into the seam by the absence of a test-only constructor. And because the tests
pass a *plain* client (tests/worker_notify.rs:100) plus `allow_loopback=true`,
the production redirect policy + DNS check path is never exercised end-to-end
by any test.

**Deletion test:** the pipeline steps can't be deleted/replaced independently —
rip out GitHub enrichment and you must edit the loop; swap the parser and you
must edit the loop and its types; the only real seams that survive are the
already-good ones (`GitHubLookup` trait, `NotificationBatcher`). The loop is
where fetch/parse/store policies congeal with no locality payoff: the worker's
own helper block `resolve_author_info`/`domain_fallback`/`resolve_github`
(218-270) is a de-facto module (author enrichment) that lives inside the
function instead of behind an interface.

### 4. `mf2.rs` — one module, two jobs, two parses of the same untrusted HTML

`has_backlink` (mf2.rs:60-74) and `parse_h_entry` (mf2.rs:92-130) each run a
full `Html::parse_document` on the fetched body — the worker parses the entire
(unbounded) untrusted HTML **twice** back-to-back (worker.rs:133, 147). The
backlink policy lives here as CSS selectors: "any `<a>/<link>` whose href
equals, or resolves to, the exact target string" (mf2.rs:76-88) — with no
normalization, so a fragment (`https://site/post#fn`) or case/trailing-slash
variant fails to match. That is worker *domain policy* frozen into a
selector-matcher file named after a format.

**Deletion test:** delete `parse_h_entry` and the worker loses content but
backlink checking survives; delete `has_backlink` and mention verification
vanishes with it. Two independent policies sharing one filename. Neither is
shallow enough to dissolve, but the *leak* is that the worker knows "backlink =
string/resolution match" is implemented in HTML-selector terms, and the *cost*
is duplicate parsing of attacker-controlled input.

### 5. `github.rs::RealGitHub` — good adapter seam, two unused knobs and one caching hole

`GitHubLookup` (github.rs:13-16) with real + stub is the model seam and
`RealGitHub` is a solid adapter — cache-first, positive 30d / negative 1h
(github.rs:85-99), 404 negative-cached (github.rs:131-143). Two frictions:

- `RealGitHub::new` takes `_timeout_ms` (github.rs:58) which is **never used**;
  the knob exists, does nothing, and the param is repeated at the call site
  (main.rs:82). Interface promises a control it doesn't wire.
- Non-success other than 404 (e.g. **403/429 rate-limit**) is logged and
  discarded with no cache write (github.rs:144-151). Every webmention whose
  h-card author is a GitHub profile then re-hits `api.github.com` live — an
  attacker can burn the operator's token quota with rotated source domains (see
  Bulletproofing #13).

SSRF relevance: low — the request host is fixed (`api_base`, github.rs:102,
production `https://api.github.com`) and `extract_github_username` (github.rs:32-43)
requires an exact `github.com`/`www.github.com` host, so it is not a host
confusion vector; the redirect policy still defends the (rare) API redirect.

### 6. `webmention_seen` — the gone-tombstone is a one-way door

The ledger itself (db/repo/webmentions.rs:7-48) is a clean, minimal upsert. The
policy around it is in the worker: when `last_status == "gone"`, `process_job`
**deletes the comment and returns early without fetching** (worker.rs:109-120).
A source that 410s once (or serves one backlink-less 200 — worker.rs:133-144
flips a previously-alive source to `gone`!) is never re-checked: every later
ping hits the early return, the source's restored backlink is never verified,
the comment is never restored, and `last_status` stays `gone` forever. The
alive→gone half works; gone→alive is unrepresentable. That is a domain
lifecycle bug wearing a "seen ledger" coat — the ledger is fine, the state
machine in the worker is not.

## Bulletproofing brainstorm

| # | Scenario | Worst case | Likelihood | Impact | Current mitigation | Gap |
|---|---|---|---|---|---|---|
| 1 | **DNS rebinding, first hop** — attacker-controlled DNS for the pinged source returns a public IP to `resolve_and_check` (ssrf.rs:107) then a private IP to reqwest's own connect-time resolution (reqwest_client.rs:51) | SSRF to internal network; internal HTML responses exfiltrated **through the stored comment** (e-content/author fields end up public) | med | high | Check-then-connect belt-and-suspenders (reqwest_client.rs:46-49); docs explicitly disclaim rebinding defense (security.md:30-31, architecture.md:157-159) | TOCTOU between check and connect; no address pinning (`reqwest` re-resolves); no per-hop async check on redirects |
| 2 | **Rebinding via redirect** — attacker's page 302s to a *public-hostname* target that resolves to 127.0.0.1; the policy (ssrf.rs:122-139) is sync-only, string/literal-IP only | Same as #1, plus no need to rebind the first hop | med | high | Redirect policy stops on blocked host *strings* and literal IPs (ssrf.rs:126-136) | Hostnames that resolve to private IPs are followed; trailing-dot forms (`http://localhost.`, `http://db.internal.`) dodge the string checks because `is_blocked_host` is dot-sensitive and the policy never resolves DNS |
| 3 | **Native-comment avatar fetch, first hop** — `author_url=https://169.254.169.254/...` or `http://10.0.0.5/...` fetched with no check at all (comment_post.rs:561-567, driven from 512-521) | Cloud metadata credential theft / internal endpoint hits / blind port probe from the origin box if the host is a cloud VM | **high** (trivially exploitable, no DNS tricks, literal IPs unblocked) | high | Only the redirect policy (doesn't apply to first hop); `author_url` is format-validated only | The SSRF seam (`fetch_url`) is not used here at all — the one production caller that bypasses it |
| 4 | **Unbounded response body** — ping a source serving a 1 GiB identity body (or an HTML page whose `resp.text()` balloon is amplified by two back-to-back HTML parses, worker.rs:133+147) | OOM / worker stall → queue fills → 503s for legit pingers | **high** (trivial to serve; per-IP cap 30/60s and per-domain cap 10/h are cheap to rotate) | med-high | `fetch_timeout_ms` (4s total, reqwest_client.rs:19) bounds *time*, not bytes; `max_content_len` (config.rs:205) truncates only after full download | No response size cap; `resp.text()` (reqwest_client.rs:68) materialises the whole body; `scraper` DOM multiplies memory several-fold, parsed twice |
| 5 | **Gone-tombstone + transient outage** — source 410s once, or a previously-alive source returns a 200 error page without the backlink (worker.rs:133-144 flips it to `gone`) | Legitimate mention permanently deleted; re-ping is eaten by the early return (worker.rs:109-120); author-side retries never resurrect it | med | med | `webmention_seen` alive/gone tracking; re-ping deletes (again) and returns | gone→alive re-check absent; no fetch on the gone path; no grace period before tombstoning (one backlink-less 200 kills a 3-year-old mention) |
| 6 | **Redirect loop, unbounded** — custom policy with no hop count (SPEC.md:303, architecture.md:159, security.md:29-30) — loop until the 4s total timeout | Worker burns its whole per-job budget on one job; with a few slow/broken sources the single worker saturates and the queue 503s | low-med | med | Total timeout bounds each job | Policy never counts hops (`attempt.follow()` unconditionally, ssrf.rs:137); no fail-closed hop cap |
| 7 | **Slow-body trickle** — server dribbles body bytes forever | Worker occupies ~4s per trickling job → ~0.25 jobs/s throughput on a single worker; queue saturates | med | med | Total request timeout covers body reads (reqwest `.timeout`) | Single worker, serial loop (main.rs:91); no concurrency, no per-job SLA |
| 8 | **Queue: full/closed/lost** —full queue → 503 (good, webmention_post.rs:93-97); closed → 500; no drain on shutdown (SPEC.md:434, architecture.md:236-241) | Deploy/restart silently drops accepted (202) pings; senders never learn | med | med | Bounded channel + explicit 503 backpressure at ingress | No persistence, no drain, no retry, no concurrency; jobs lost on process stop — documented, unfixed |
| 9 | **Fetch failure retry policy: none** — timeout/5xx/reset → warn + drop (worker.rs:123-130); no backoff, no re-check | A flaky-but-real blog's mention is dropped forever (recovery only if the pinger re-pings) | med | low-med | 410 → `Gone` → ledger + comment delete is the only structured failure path | Transient HTTP errors indistinguishable from permanent; no retry queue; no periodic re-check of `gone`/failed sources |
| 10 | **IPv6 gaps** — only `::1/128`, `fc00::/7`, `fe80::/10` blocked (ssrf.rs:32); NAT64 `64:ff9b::/96`, 6to4 `2002::/16`, Teredo `2001::/32` with embedded private IPv4 pass; zone-ID `[fe80::1%25eth0]` fails `IpAddr::from_str` in the policy | Private-range bypass through translated/tunnelled IPv6; link-local connect via zone-ID redirect | low | med | v4-mapped v6 handled (ssrf.rs:41-49); tests cover mapped forms (ssrf.rs:254-270) | No allowlist of global-unicast (`2000::/3`) with embedded-IPv4 re-check for `64:ff9b::` |
| 11 | **Obfuscated localhost / literal IPs** — `127.1`, `0x7f000001`, `2130706433`, trailing-dot `localhost.` | First hop: **safe** — WHATWG normalization means `host_str()` returns `127.0.0.1` for all numeric forms, which the check catches; trailing dot caught by DNS resolution. Redirect hop: unsafe (see #2) | low (first hop), med (redirect) | med | `fetch_url` checks the *serialized* host, not the raw form | Policy layer only; numeric forms fine, trailing-dot/other-string forms not |
| 12 | **Non-HTML content (binary/zip/PDF)** — `resp.text()` decodes whatever arrives regardless of Content-Type; body may be huge | Memory spent decoding a 100 MB PDF as text; a binary page with no backlink flips an alive source to `gone` (see #5) | med | low-med | Backlink check requires actual `<a>/<link>` — binary almost always fails (→ tombstone risk) | No Content-Type gate, no size cap; interplay with #5 makes binary/payload pages a *state-corruption* vector, not just a waste |
| 13 | **GitHub lookup abuse** — attacker pings webmentions whose h-card author points at `github.com/<user>` (or posts native comments with `github_username`), each a live API hit on a 403/429-not-cached miss (github.rs:144-151) | Token rate limit exhausted → all further enrichment silently `None`; no negative cache to absorb the burst | med | med | Positive 30d / negative 1h cache (github.rs:85-99); per-domain 10/h and per-IP 30/60s caps slow the pump | 403/429 uncached; no per-username throttle; `_timeout_ms` knob unused; GitHub lookup is not an SSRF vector (fixed host) but is a quota-burn vector |
| 14 | **Backlink match normalization** — fragment, trailing slash, query, or case variant on the *target* side fails `urls_match` (mf2.rs:76-88) | False negatives for legit mentions; a honest ping with `?utm=` or `#id` on the link is rejected | med | low | Resolves relative hrefs against the target origin (mf2.rs:82-87) | Fragment/query/case handling absent; `derive_target_path` (worker.rs:275-289) strips to path but the backlink check uses the raw target string — two different normalizations in the same pipeline |
| 15 | **`is_new` notification logic** — keyed by source URL only (worker.rs:164), while comments dedupe by `(source_url, target_path)` (comments.rs:38-46); one source linking to two pages → second page's comment gets no admin notification | Missed moderation signal for a page the source genuinely mentioned | low | low | `get_comment_by_source(...).is_none()` is the cheapest check | Should be keyed by the same pair as the upsert conflict target |

## Candidate deepening opportunities

1. **`SafeFetcher` — one deep module owning "fetch or refuse"** (merge/absorb
   `reqwest_client.rs`, fold the blocklist guarantee in)
   - **Module:** `src/fetch.rs` (or `src/http/safe_fetch.rs`).
   - **Seam:** a `SafeFetcher` struct built once (client + blocklist + caps +
     hop cap) with a single async method, e.g. `fetch_text(&Url) -> Result<FetchedDoc, FetchError>`.
     Internally: normalize host → `resolve_and_check` → **disable reqwest
     redirect following and re-implement redirects manually**, checking each hop
     with a fresh `resolve_and_check` before following (this closes the
     sync-policy hole for named hosts, trailing dots, and rebinding), enforcing
     a hop count (default ~5, per SPEC's absent "five-hop limit"
     architecture.md:159) and a response byte cap.
   - **Plain-English shape:** "There is one guarded door for every outbound
     fetch; a caller cannot construct a fetch that skips the check." The
     `Client` with the baked-in policy disappears as a public artifact — that
     is what falsely promises safety today (see Friction #1).
   - **Tests that would improve:** wiremock serving 302 → `http://localhost./`,
     302 → public name resolving to 127.0.0.1, loop of 50 redirects (assert
     fail-closed ≤N), 10 MiB body → `TooLarge`, 410 → `Gone`.

2. **Route the native avatar fetch through the same door**
   - **Module/seam:** `comment_post.rs::fetch_page_avatar` (comment_post.rs:561)
     stops building its own request; it calls `SafeFetcher::fetch_text` (or
     `fetch_url` today) and falls back to dicebear on failure, like every other
     sender.
   - **Plain-English shape:** "An author URL is fetched the same way a webmention
     source is fetched — blocked targets just produce the fallback avatar
     instead of a connection."
   - **Tests that would improve:** post a native comment with
     `author_url=http://169.254.169.254/` / `http://127.0.0.1:port/meta` /
     `http://[::1]/` and assert (a) no request reaches the target, (b) avatar
     falls back; a wiremock-based assert that the SSRF client (not a plain one)
     made the request.

3. **FetchedDoc — avoid the double parse and bound memory in one seam**
   - **Module:** part of the fetch seam (`FetchedDoc { html, doc }` where `doc`
     is the single `scraper::Html`), plus a byte cap while streaming.
   - **Seam:** `mf2::has_backlink` and `mf2::parse_h_entry` take
     `&scraper::Html` instead of `&str`, and the worker calls them against the
     one parsed document (worker.rs:133/147 today parse twice).
   - **Plain-English shape:** "Fetching a page parses it once, with a hard byte
     budget, and both the backlink check and the h-entry parse read the same
     tree."
   - **Tests that would improve:** a property test that `has_backlink` +
     `parse_h_entry` on 10k random HTML docs consume one parse (implicit);
     explicit: body cap test; fragment/case normalization decision test for
     `urls_match`.

4. **A `WebmentionProcessor` behind the loop, with a `SourceFetcher` adapter —**
   kill the god loop and the `allow_loopback` knob
   - **Module:** `worker.rs` refactored: `WebmentionProcessor { repo, fetcher:
     Arc<dyn SourceFetcher>, github, notifier, policies }` with
     `process(&WebmentionJob) -> Result<...>`; the loop in
     `spawn_worker_for_state` (worker.rs:58-87) stays 15 lines.
   - **Seam:** `SourceFetcher` trait — production impl = `SafeFetcher`,
     test impl = wiremock-backed. `allow_loopback` (worker.rs:101,
     reqwest_client.rs:32-36/47) dies: tests inject the test adapter instead of
     loosening the production check; the production signature stops carrying a
     test-escape boolean.
   - **Plain-English shape:** "The worker decides *what* to do with a
     fetched source; the fetcher decides *how* a URL becomes bytes — and the
     two are swapped independently."
   - **Tests that would improve:** reuse tests/worker_notify.rs with the test
     fetcher and no `allow_loopback`; a "pipeline policy" test matrix:
     alive→gone→alive, backlink-missing grace, status preservation.

5. **Fix the gone-resurrection lifecycle (and make the state machine explicit)**
   - **Module/seam:** a small `seen` policy — either inside `WebmentionProcessor`
     or as `webmention_seen` repo methods `mark_alive(mark_gone(...))` — so the
     worker no longer hand-rolls transitions (worker.rs:106-120, 133-144,
     204-209, 292-305).
   - **Plain-English shape:** "A re-ping after a source went gone still fetches
     and verifies; find the backlink and the comment comes back (status
     decided explicitly, e.g. restored to `pending` or to its pre-gone
     status); a single backlink-less 200 does not tombstone a long-alive
     source without a grace window."
   - **Tests that would improve:** full cycle test: ping → alive comment →
     source 410s → ping → comment deleted + seen=gone → source restored with
     backlink → ping → comment re-created, seen=alive; transient-blip test
     (alive source returns 200-without-backlink once, then normal again).

6. **GitHub lookup hardening — wire the knobs, cache the 400-class misses**
   - **Module:** `github.rs::RealGitHub` only.
   - **Seam/plain-English shape:** "Every outcome of the GitHub API is cached
     for some finite time — including rate-limit responses — and the timeout
     argument actually sets the timeout." Implement: use `_timeout_ms`
     (github.rs:58) or drop the param; on 403/429 write a short negative cache
     entry (e.g. 10-60 min) so a burst can't re-hammer the API; consider a
     per-username concurrency/throttle guard.
   - **Tests that would improve:** wiremock 429 → `lookup` returns None → second
     `lookup` within TTL makes **zero** additional requests; assert the
     constructed client's timeout matches `_timeout_ms`.