# Review 01 — Data & storage layer

## What I read

- `src/db/pool.rs` — 548 lines, the hottest file (+475 changed lines in the working tree: versioned `PRAGMA user_version` migrations, existence-checked `ADD COLUMN`, refuse-newer-DB, error propagation instead of `let _ =` swallowing, a full test battery).
- `src/db/mod.rs` — 31 lines: `RepoError` (two-variant string bag) + blanket re-exports.
- `src/db/repo/mod.rs` — 915 lines: all data types, `Repo`, the `spawn()` plumbing, `row_to_comment`, and ~740 lines of tests.
- `src/db/repo/comments.rs` — 679 lines: comment CRUD, upsert-by-source, moderation, author lookup, export/import primitives (18-column positional mapping).
- `src/db/repo/reactions.rs` — 307 lines: reaction upsert with changed/noop semantics, moderation, counts, import.
- `src/db/repo/urls.rs` — 226 lines: extracted-URL storage, per-comment/domain lookup, export/import.
- `src/db/repo/webmentions.rs` — 78 lines; `src/db/repo/github_profiles.rs` — 80 lines: thin upsert/get/dump wrappers.
- `src/http/admin/data.rs` — 425 lines: JSON export/import (recently rewritten: +59 lines adding `ip_hash_salted`, hash re-derivation, salt-mismatch warning).
- `migrations/schema.sql` — 86 lines: the canonical snapshot (5 tables, partial unique index, CHECKs).
- `docs/architecture.md` — 257 lines (Database section: pragmas, versioning story); `SPEC.md` — 459 lines (Data model, Export/import, Security rules); `docs/deployment.md` — 344 lines (SQLite and backups section).
- Cross-read for seams: `src/main.rs` (127), `src/http/comment_post.rs` (store + URL + notify flow, lines 180–359), `src/http/comments_read.rs` (130), `src/http/mod.rs` (route/body-limit wiring), `src/http/layers.rs` (95), `src/worker.rs` (webmention upsert flow, 90–305), `src/github.rs` (repo use in tests), `src/http/admin/mod.rs` (import tests).

---

## Friction

### F1. No unit-of-work seam — every repo call is one connection, one autocommit

**Module:** `src/db/repo/mod.rs` (`Repo::spawn`, lines 115–129) and every method in `repo/*`.

Every method re-implements the same plumbing: capture args by value, clone the pool, `spawn_blocking`, `pool.get()`, run SQL, map errors. More importantly, **the connection and the implicit transaction end where each method ends**. Anything that touches two tables must be orchestrated by the *caller* as a sequence of separate `spawn_blocking` round trips:

- Native comment store: `insert_comment` → `update_status` (auto-approve) → `insert_urls` is three pool acquisitions (`http/comment_post.rs:216–245`); the URL insertion error is swallowed with `let _ =` (line 244).
- Webmention store: `upsert_by_source` then `upsert_webmention_seen` are separate commits (`worker.rs:165–209`).
- Read API: `list_approved` + `count_approved` + `reaction_counts` = three spawns and three connection acquires per request (`comments_read.rs:97,107,111`).
- `get_comment_chain` is an N+1 loop of `self.get_comment(...).await` — up to `MAX_THREAD_DEPTH` separate spawns/pool acquisitions per webhook payload (`comments.rs:465–484`).
- Import is one spawn per row (`data.rs:153,174,194,198,218,225`).
- Export is five sequential spawns, so a "backup" is five snapshots, not one (see B-22).

**Why it hurts:** the interface promises "store a comment"; the implementation reality — "each call is a round trip you cannot compose" — leaks to every handler/worker. There is no seam where a caller can say "run these N queries on one connection, one transaction". Crash windows between related writes produce torn state (comment without URLs, mention without `seen`), and the import/export consistency gaps below all trace back to this. The atomicity question from the brief — "are comment store + URL extraction + notification atomic?" — is answered *no* by construction of this seam, with the URL write being the quiet casualty (`let _ =`).

**Deletion test:** there is nothing to delete — the problem is a *missing* intermediate module. The `spawn` helper is the only "interface" and it is 1:1 with "one blocking call". Complexity is not concentrated anywhere; it is smeared across every method and every caller that must re-learn the cost model.

### F2. `row_to_comment` + the 18-column SELECT list, duplicated ~12 times

**Module:** `src/db/repo/comments.rs`.

The same 18-column select list appears verbatim in `list_approved` (81, 90), `list_approved_oldest` (129, 138), `list_approved_global` (169), `list_pending` (248, 256, 264, 272), `list_comments` (338, 350), `get_comment` (450), `get_comment_by_source` (592), `list_all_comments` (609) — and `row_to_comment` (`mod.rs:149–170`) maps by position with a comment that *begs* the reader to keep the order in sync ("The SELECT column order must be…").

**Why it hurts:** adding a column means editing ~12 SQL strings plus the row mapper plus `NewComment` plus `import_comment`. Reordering a column silently corrupts every read (no type mismatch at the boundary — `String`→`Option<String>` just shifts). The interface of `repo/comments.rs` is wide (21 methods) and its implementation is one long repetition; the module has no *inside* that the repetition hides.

**Deletion test:** nothing to delete; the complexity is real. But it is *unconcentrated* repetition — the fix is to concentrate it (one column list, one mapper, one parameterized query builder), which is the definition of deepening.

### F3. Two sources of truth for schema: `schema.sql` + inline versioned DDL in pool.rs

**Module:** `migrations/schema.sql` ⊗ `src/db/pool.rs` (`run_migrations`, lines 107–258).

The fresh-install path and the `current == LATEST` path execute the whole `schema.sql` snapshot; the legacy/versioned path re-declares the same tables and columns inside `if current < N` blocks (`pool.rs:142–242`). The comment at `schema.sql:1–4` admits the manual-sync discipline: "Do not rely on this file alone for upgrades."

**Why it hurts:** a future schema change must be written *twice* (new `if current < 9` block + new `schema.sql` section), and the two copies can drift silently. The v8 reactions migration is the proof: it was *only* in `schema.sql` (reachable by fresh installs) until this working-tree change made it an explicit versioned step — meaning v0–v7 databases would have kept missing the table forever if nobody had noticed. There is no test that the union of versioned steps equals the snapshot. Also note `pool.rs:118–124`: on every single startup, the entire snapshot is re-executed as a "defensive" measure — cheap today (IF NOT EXISTS), but it means `schema.sql` syntax errors or a schema that stops being idempotent take the process down at boot with a raw rusqlite error.

**Deletion test:** delete either half and one class of database (fresh vs legacy) breaks — neither is removable; the *manual-sync interface between them* is the shallow artifact. The pairing should be made structural (one list of steps where step N's DDL *is* the versioned step, snapshot derived or asserted equal — see D2).

### F4. Restore orchestration lives in the HTTP handler

**Module:** `src/http/admin/data.rs` (`import`, lines 95–255 — a handler module).

The restore concept — ordering (comments before URLs/reactions), parent-before-child sorting, per-row skip semantics, FK-orphan tolerance, salt re-derivation, salt-mismatch warning — is domain logic built from primitive repo calls in an Axum handler. The repo side (`import_comment` `comments.rs:630`, `import_comment_reaction` `reactions.rs:280`, `delete_urls_for_comment`/`insert_urls` `urls.rs:215/39`) is pure pass-through SQL: the *restore intelligence* is invisible to the db layer. This is why the error-handling is inconsistent inside one function: comments and URL rows skip-and-continue (`data.rs:153–157, 193–202`) while a single failing reaction or webmention row aborts the whole import with `?` (`data.rs:167–174, 218, 224–234`) — the handler had to invent the policy twice, with no shared vocabulary.

**Why it hurts:** the seam between HTTP and storage is drawn at the wrong place. A future feature (import in bulk, import to a different table, migration tooling) must re-invent ordering/FK/salt logic in another caller. The `?`-aborts also produce a 500 on a *partially completed* import — the response body (which carries counts) never reaches the operator.

**Deletion test:** the logic is all genuinely needed (not slough), which means it is *not shallow* — but it is misplaced: the same complexity concentrated in the db layer would serve more callers and could be unit-tested without HTTP plumbing.

### F5. `RepoError::Internal(String)` — the type wall gives handlers nothing

**Module:** `src/db/mod.rs:7–11`.

Every storage failure becomes a string in a two-variant error. Handlers cannot tell a UNIQUE/FK constraint violation (row is bad, skip it) from an IO/disk error (stop everything) from a busy-timeout (retry). The import handler's asymmetry in F4 is a direct consequence — `import_comment`'s error is inspected only via `is_err()`.

**Why it hurts:** error-deciding logic (retry vs skip vs abort) has nowhere to live except bespoke `match`es on substrings, and `Internal` maps to a bare 500 (`mod.rs:25–30`). A busy/disk-full burst during import is indistinguishable from "this row is garbage", which is exactly the situation that decides — incorrectly — whether to abort 10,000 rows of restore.

### F6. Silent row swallowing in the read path

**Module:** `src/db/repo/comments.rs`, `urls.rs`.

`list_pending` collects with `.filter_map(|r| r.ok())` (`comments.rs:290–311`) — a row that fails to decode (schema drift, corrupt page) *silently vanishes from the moderation queue*. Same pattern in `lookup_author` (571) and `urls.rs` (133, 149, 178). In the *moderation inbox* of all places, a dropped row is invisible until someone cross-checks counts. Meanwhile `list_comments` (366–369) does the right thing (propagate row errors) — the two conventions coexist in the same file.

**Why it hurts:** corruption shows up as "a pending comment disappeared" rather than an error — the worst failure mode for an operator to diagnose. The interface promises `Vec<Comment>`; it can deliver a *filtered* `Vec<Comment>` under some conditions and nobody can tell.

### F7. `lookup_author` assembles SQL by string formatting (in the repo, no less)

**Module:** `src/db/repo/comments.rs:506–586`.

`conditions.push(format!("submitter_ip = '{}'", v.replace('\'', "''")));` — hand-escaped interpolation to build a WHERE clause. SPEC.md:427–428 acknowledges it as a known exception. It is parameterizable trivially (build `submitter_ip = ?1` + params). It works, but it is exactly the pattern the rest of the file carefully avoids, sitting one file away from the SQL-injection regression test (`mod.rs:603`).

### F8. Shallow-by-convention submodules: `webmentions.rs`, `github_profiles.rs`, and the widened `upsert_by_source`

**Module:** `src/db/repo/webmentions.rs` (78 lines), `github_profiles.rs` (80 lines).

Each is three tiny methods whose interface is nearly identical to their implementation. Deletion test: deleting them just relocates the code into `repo/mod.rs` — complexity neither concentrates nor disappears. They are *harmless* namespacing, not harmful depth. The real locality wound is the opposite direction: **one concept — the webmention write path — spans three files** (`worker.rs` handler logic + `comments.rs::upsert_by_source` + `webmentions.rs::upsert_webmention_seen`) with no module that owns "store this mention" as a unit.

### F9. `upsert_reaction` is a read-modify-write split across three statements

**Module:** `src/db/repo/reactions.rs:43–93`.

SELECT → decide → INSERT … ON CONFLICT → re-SELECT the id, all on one connection but **not in a transaction**. Two concurrent clicks: both may read "no row", both report `changed=true` (cosmetic). The semantic race is worse: `update_reaction_status` (line 142) is a blind `UPDATE … WHERE id=?` — a moderator approving a reaction while the user swaps the emoji can approve the *new* emoji that was never moderated, because the resets and the approval are separate statements with no ordering guarantee. The schema comment says "changing the emoji resets to pending for re-moderation" — an invariant the code enforces only in the absence of concurrency.

### F10. Migration artifacts: `Repo::pool()`, dead params, debug-only SQL

**Module:** `src/db/repo/mod.rs:109–113`, `pool.rs:255`.

- `pool()` is a `#[doc(hidden)]` escape hatch that hands out the raw pool; today only tests use it (`github.rs:288,320`), but the facade's seal is already broken.
- `list_approved`/`list_approved_oldest`/`list_pending` branch on `Option<i64>` with `let Some(_cursor) = before` to pick *one of two full SQL strings* — the cursor is unused in the predicate selection; two duplicated queries per method where one parameterized query would do (`comments.rs:79–97, 127–145`).
- `debug_assert!(table_exists(&conn, "comments")?)` (`pool.rs:255`) runs a live query with `?` propagation inside an assert — a refactor trap.

---

## Bulletproofing brainstorm

### Migration paths

**B-1. Fresh install, `user_version = 0`, no tables.**
Works: full snapshot then stamp (`pool.rs:130–135`), tested (`pool.rs:320–324`).
Worst case: snapshot interrupted mid-batch (disk full) — partial tables, no stamp; recovery lands in the legacy path which existence-checks everything and re-runs the canonical snapshot. Self-healing, good.
Likelihood: low · Impact: low · Mitigation: idempotent catch-up path + tests. Gap: none significant.

**B-2. Legacy `user_version = 0` DB with tables (pre-versioning era).**
Works: existence-checked `ADD COLUMN`/`CREATE TABLE` run, then snapshot, then stamp (`pool.rs:137–250`), tested with a simulated v1-era schema (`pool.rs:327–396`). Strong improvement over the old swallow-everything `let _` code.
Gap: the test fabricates a *canonical* v1 schema; a real v0 DB could differ in unknown ways (e.g., an index or column name from an older codebase). No fuzz/unknown-shape coverage.
Likelihood: low · Impact: low · Mitigation: as above. Gap: test only the canonical legacy shape.

**B-3. Legacy DB holding duplicate `(source_url, target_path)` rows.**
Scenario: the partial unique index `idx_comments_source_target` (`schema.sql:39–41`) predates some v0 databases. The upgrade path always re-runs the snapshot, whose `CREATE UNIQUE INDEX` *fails* if duplicates exist. Worst case: startup aborts with a raw SQLite error, no guidance, and the fix requires manual dedup SQL — with no documented procedure. Not covered by any test.
Likelihood: low–med · Impact: med (service won't start; operator faces raw SQL) · Mitigation: none. Gap: a dedup pre-step (keep newest row per pair) or a clear, actionable error with the offending rows' ids; at minimum a test that seeds duplicates and asserts the boot fails *loud and clearly* rather than cryptically.

**B-4. Newer-than-binary DB.**
Refused with a clear upgrade message (`pool.rs:111–116`), tested (`pool.rs:399–411`). The *right* answer. No gap.

**B-5. Concurrent migration vs traffic.**
Structure avoids the race: `run_migrations` runs on one connection before the listener binds (`main.rs:44–46, 117`) — there is no traffic window. Two processes pointing at the same file (mis-config, docker scale) would both run migrations concurrently; IF NOT EXISTS + existence checks + 5 s busy_timeout make this mostly harmless, and it is a config error anyway.
Worst case: none realistic. Likelihood: very low · Impact: low · Mitigation: pre-bind ordering. Gap: none.

**B-6. Schema drift between `schema.sql` and the versioned steps (future change).**
Scenario: author adds a column only to `schema.sql` and bumps `LATEST` — every *existing* DB silently keeps the old shape (the snapshot's CREATE IF NOT EXISTS never adds columns; only `if current < N` blocks do). The v8 reactions table is the historical near-miss. The failure is invisible until a legacy-DB user hits a missing-column error at runtime.
Likelihood: high (this happens *on every schema change* until made impossible) · Impact: med (runtime errors on old installs, zero test coverage of the gap) · Mitigation: the convention comment (`pool.rs:6–17`). Gap: a test asserting the union of versioned steps matches the snapshot (see D2).

### WAL, crash recovery, storage failures

**B-7. Crash between related writes (comment ↔ URLs; mention ↔ seen).**
No transactions, separate commits (F1). Comment-without-URLs is permanent (URLs are only written once, `comment_post.rs:244`); mention-without-seen self-heals on the next ping because `upsert_by_source` is idempotent (`worker.rs:165–209`).
Worst case: silent loss of URL-lookup data, silent loss of moderation history. Likelihood: low–med (crash windows are small but the `let _ =` swallows *errors*, not just crashes — a busy-timeout during `insert_urls` loses the rows with zero trace) · Impact: med · Mitigation: SQLite WAL guarantees per-statement atomicity only. Gap: make comment+URLs one transaction, and log (not swallow) URL-write failure.

**B-8. WAL crash recovery / power loss.**
`synchronous=NORMAL` + WAL (`pool.rs:30–32`) is the documented safe-for-crash-recovery combo; the SQLite layer recovers on next open. Fine.
Gap: none beyond B-7's torn *multi-statement* writes.

**B-9. Disk full.**
Writes fail → 500s; a failed `insert_comment` surfaces (good), a failed `insert_urls` is swallowed (B-7); disk-full during import: comments skip per-row (fast, silent), reactions abort the whole remainder (F4); disk-full during migration: boot panic with raw error (`main.rs:46`), no guidance. `healthz` doesn't touch the DB (`http/mod.rs:192–194`), so orchestrators keep the container "healthy" while every write 500s.
Likelihood: med (comment data grows; volumes fill) · Impact: med–high · Mitigation: busy_timeout means the *process* survives; per-row skip in comments import. Gap: no disk/DB health signal, no pre-flight check, abort-vs-skip policy inconsistent (F4).

**B-10. Read-only filesystem / file.**
Pool creation is lazy — the failure surfaces at `run_migrations` (= boot panic, loud) or at first request if the file was already migrated on a writable FS. WAL requires directory write access; a writable file in a read-only dir fails at first `journal_mode` set.
Worst case: service up, every write 500s when the FS flips to read-only mid-run. Likelihood: low–med · Impact: med · Mitigation: boot-time failure for the common case; pragmas set per connection. Gap: `healthz` again reports ok.

**B-11. Corrupt DB file (bit rot, bad restore, partial copy).**
Per-query errors → 500s; `list_pending`'s `filter_map(|r| r.ok())` (F6) turns corrupt rows into *silent disappearance* from the moderation queue. No `PRAGMA integrity_check`/`quick_check` anywhere; no detection until a query touches the bad page.
Likelihood: low (SQLite is robust; corrupt backups are the realistic source) · Impact: high (silent data loss in moderation; no early warning) · Mitigation: none. Gap: optional startup quick_check (config-gated), and hard-error (not filter) on row decode failure (F6).

**B-12. Pool exhaustion & `spawn_blocking` saturation.**
`create_pool` never sets `max_size` (`pool.rs:38–44`) → r2d2 default 10 connections. `pool.get()` is the *blocking* variant with no timeout (`repo/mod.rs:122–124`) → waits forever; each waiter occupies a Tokio blocking-pool thread (default 512) → all DB-backed handlers pile up, memory grows, requests hang rather than failing fast. Triggers: disk-slow DB, one huge import, a burst of `lookup_author`/`submitter_stats` scans (no indexes on author_name/author_url/submitter_ip — `comments.rs:377` etc. are full scans holding a connection).
Likelihood: low–med · Impact: med–high (hang, not error; no circuit breaker) · Mitigation: busy_timeout is SQLite-side only (5 s, `pool.rs:31`); per-route rate limits cap request rate. Gap: `max_size` decision, `get_timeout`, and a bounded-concurrency seam for panics-on-saturation.

**B-13. Long-running per-request queries.**
`lookup_url` issues 7 queries in one spawn (fine on one connection), `submitter_stats` 6 queries, `get_comment_chain` up to depth×spawns. Each holds a pool connection for the whole closure; under load these extend B-12's exposure. No indexes on `author_name`, `author_url`, `submitter_ip`, `content_hash` (admin filters; `list_comments` with `ip=` or `content_hash=` scans). At personal-blog scale this is fine; it is a latency cliff, not a correctness one.
Likelihood: low · Impact: low–med · Mitigation: queries are single-connection. Gap: none urgent; note in docs.

### Transaction boundaries

**B-14. Comment store + URL extraction + notification atomicity.**
Store is atomic alone; URL write is a separate commit with swallowed errors (B-7); notifications are fire-and-forget after the commit (correct — the comment must survive permanently even if Telegram is down). The webhook payload enrichment (`comment_post.rs:272–281`) runs `submitter_stats` + `get_comment_chain` *after* the comment commit — reads are best-effort `.ok()`, fine.
Verdict: notifications correctly decoupled; URL write should join the comment's transaction or at least log loudly.

**B-15. Webmention upsert + `webmention_seen` + gone-deletion ordering.**
Not atomic (F1), but every interleaving self-heals: comment upsert is idempotent; `handle_gone_source`'s seen-then-delete (`worker.rs:292–305`) is re-entrant because the next ping re-checks `seen.gone` → deletes again (`worker.rs:109–120`). The one asymmetry: `handle_gone_source` deletes only *one* comment per source (B-18).
Likelihood: low · Impact: low · Mitigation: idempotent upserts. Gap: none beyond B-18.

### Webmention / reaction upsert semantics

**B-16. Multi-target source (`get_comment_by_source` ambiguity).**
`get_comment_by_source` (`comments.rs:588–600`) fetches by `source_url` alone and returns the *first* row, but the unique key is `(source_url, target_path)` (`schema.sql:39–41`) — one blog post mentioning two zapiska pages legitimately produces two rows. Consequences: `is_new` in `worker.rs:164` is wrong for the second target (notification for the second mention is skipped, since it looks like an update of the first); `handle_gone_source` deletes only the first matching comment; the "previously gone" delete branch (`worker.rs:109–120`) is likewise partial.
Likelihood: med — a post linking to two pages of the same site is a normal webmention pattern · Impact: med (missed notifications; stale comments survive a 410) · Mitigation: `upsert_by_source` itself is keyed correctly; only the *reads* are ambiguous. Gap: scope lookups by `(source, target)`.

**B-17. `upsert_by_source` leaves `content_hash` stale on update.**
The DO UPDATE refreshes author/content but not `content_hash` (`comments.rs:42–47`). After a mention updates its content, moderation lookup by content-hash finds the *old* hash — a stale row in the anti-spam index.
Likelihood: low · Impact: low · Mitigation: none. Gap: include `content_hash` in the update set (and decide who computes it for webmentions — currently the worker passes `None`).

**B-18. Reactions: UNIQUE-constraint races and moderation-vs-edit interleaving.**
The `INSERT … ON CONFLICT(comment_id, identifier)` is atomic and correct under concurrent clicks (writers serialize in WAL); the pre-SELECT (`reactions.rs:52–60`) is only for the `changed` flag and can race cosmetically. The real gap: moderator approve (`update_reaction_status`, blind UPDATE by id) racing a user emoji change can approve an emoji that was never pending (F9).
Likelihood: low (moderation is human-paced) · Impact: low–med (a moderation bypass for one reaction) · Mitigation: reset-to-pending on change is enforced in the non-racy path. Gap: compare-and-swap the status (e.g., `WHERE status='pending'`) or wrap upsert+status changes so the invariant holds under concurrency.

**B-19. Reaction import aborts the whole import on one bad row.**
`import_comment_reaction(...).await?` (`data.rs:218`) — a reaction whose `comment_id` failed validation/import (FK violation, `foreign_keys=ON` per `pool.rs:30`) aborts everything remaining, after comments/URLs already committed: a 500 on a half-done restore with no counts returned. Same `?` pattern for `webmention_seen` (data.rs:167–174) and profiles (225). Only the comment and URL sections skip-and-continue.
Likelihood: med (any real-world export with a moderation-deleted comment whose reaction rows remain orphaned — the exported data can't be FK-consistent by construction... actually reactions were FK-consistent at export time; orphan risk comes from *skipped* comments on the import side) · Impact: med (partially restored DB, misleading 500) · Mitigation: none. Gap: uniform skip-and-count policy; make counts/partial-state part of the response even on per-section errors (D3).

### Export / import edge cases

**B-20. Huge exports.**
Export loads all five tables into RAM (`data.rs:75–79`) and serializes one JSON blob with no pagination. A DB whose export exceeds 16 MiB produces a file that *its own import endpoint rejects* (`MAX_IMPORT_BODY_BYTES`, `data.rs:27`; enforced at `http/mod.rs:89–91`). No streaming, no chunked export.
Likelihood: low for a personal blog today, med over years (content 2 KB × 10k comments ≈ 20 MB+) · Impact: med–high (backup path stops working; operator discovers at migration time) · Mitigation: 16 MiB ceiling is deliberate and tested (`admin/mod.rs:923–968`). Gap: streamed export or a documented chunking contract; import cap that matches the export.

**B-21. Import into a live database — ID clobbering.**
`import_comment` upserts `ON CONFLICT(id) DO UPDATE` (`comments.rs:630–652`). Importing an old backup into a DB with live traffic silently **overwrites** live rows whose ids collide (status reverts, moderation decisions lost, content replaced), and new comments written during the import window are reverted too. There is no guard (e.g., refuse when the comments table is non-empty and max(id) ≥ imported min(id)), no warning, and no test (tests cover fresh-DB import only: `admin/mod.rs:690`).
Likelihood: med — "restore a backup" is one careless command away from pointing at the wrong (live) instance · Impact: high (silent clobber of live comments/moderation) · Mitigation: docs frame import as restore/migration; version check exists. Gap: detect and refuse or warn on id overlap with existing rows; or make import target explicit (`?replace` vs `?merge`).

**B-22. Export is five snapshots, not one.**
The five `list_all_*` calls run on different connections at different times (`data.rs:75–79`). A comment created mid-export can appear only in the URL/reaction arrays, or (worse) not at all while its URLs do. Imports tolerate orphans by skipping — meaning the *backup silently loses data* and nobody is told.
Likelihood: med (exports typically run while traffic is live — the docs' own live-backup recipe, `deployment.md:278–283`) · Impact: med · Mitigation: orphan-skip makes imports resilient. Gap: one connection, one read transaction for the whole export (WAL makes read-transactions cheap) — D1's seam.

**B-23. Parent-before-child ordering.**
Handled twice: sort by id (`data.rs:125`) + `parent_id < id` validation (`data.rs:310–314`) + DB FK as the last net. A skipped parent cascades to its children (FK fails → child skipped, counted) — correct and tested (`admin/mod.rs:974–1034`). Residual: imported depth is clamped 0–10 (`data.rs:315`) regardless of the importing server's `MAX_THREAD_DEPTH` config (a 0-depth server can receive threads). By-design restore semantics; worth a doc note, not a fix.
Likelihood: low · Impact: low.

**B-24. Partial import & crash mid-import.**
Per-row commits by design; a crash leaves a partial restore. Recovery: re-import is idempotent (upsert everywhere) *except* the URL section's delete-then-insert for each comment (`data.rs:189–204`): crash between `delete_urls_for_comment` and `insert_urls` loses that comment's URL rows until the *next* full re-import restores them (re-import does recover them — so idempotence holds across a rerun, but the intermediate state is a silent hole). Also `insert_urls` is one autocommit per URL row (`urls.rs:45–51`) — a crash mid-group leaves partial rows; again self-healing on rerun.
Likelihood: low · Impact: low–med · Mitigation: idempotent upserts + delete-then-insert discipline. Gap: batch each comment's URLs in one statement/transaction; consider a single restore transaction (validated-elsewhere, applied once) for crash atomicity.

**B-25. Secret rotation / lost `IP_HASH_SECRET`.**
The best-handled scenario in the codebase: export records `ip_hash_salted` (`data.rs:84`), import re-derives comment hashes from raw IPs (data.rs:140–149, counted), salt-mismatch returns a warning (data.rs:242–252, tested `admin/mod.rs:1099+`), startup logs a warning when hashes exist without a secret (`main.rs:51–71`), docs warn twice (`deployment.md:289–296`). Residual gap: reaction identifiers (`h:`-prefixed) cannot be re-derived — documented but irreparable; and raw IPs + `delete_token`s ride along in the backup file, which the docs don't call out as a sensitive document.
Likelihood: med (the documented failure mode) · Impact: med (orphaned reaction identities; doc'd) · Mitigation: strong. Gap: warn that delete tokens in the export allow comment deletion by anyone holding the file.

**B-26. 16 MiB body limit details.**
Enforced per-route via `DefaultBodyLimit` override (correct layering, tested for >8 KB `admin/mod.rs:923`). Gap: B-20's asymmetry (exports can exceed it); also the whole body is buffered by axum then parsed — an attacker with a valid token can send 16 MiB of JSON → import loops spawn-per-row over garbage — validate-early (version + shape) exists; fine.

**B-27. Import field-validation gaps (defense-in-depth holes).**
Comments are thoroughly re-validated (`data.rs:284–318` — status/type/path/URL/control-chars/sanitize). But `comment_urls` rows are inserted with *no length or shape checks* (`data.rs:179–205` passes `(url, domain, url_hash)` straight through), `webmention_seen` checks only `last_status`, profiles check nothing. All admin-authenticated, but the restore path is the one place untrusted structured data becomes SQL rows — cheap to close.
Likelihood: low (admin-only) · Impact: low–med · Mitigation: comment validation is thorough. Gap: length caps on URL/domain/hash fields, mirroring B-27-reaction checks (`data.rs:259–279`).

### Backup story (ops)

**B-28. File-copy backups under WAL.**
Docs prescribe stop-then-copy db + optional `-wal` (`deployment.md:269–276`) or the JSON export. Correct for clean stops, but: after a *crash* (service killed, host power loss) a user following the recipe copies db + stale/absent `-wal` and loses everything since the last checkpoint — the docs don't warn that a crash-latest copy must include `-wal`/`-shm`. There is no `sqlite3` backup-API/`VACUUM INTO` integration and no docker-volume guidance, so the live-safe options are export (B-22) or "stop the service".
Likelihood: med (operators do crash-recovery copies) · Impact: med–high (silent data loss up to last checkpoint) · Mitigation: docs do say "stop first"; JSON export works live. Gap: `VACUUM INTO`-style snapshot endpoint or explicit crash-copy guidance; a `PRAGMA wal_checkpoint(TRUNCATE)` step in the docs' stop procedure.

**B-29. `healthz` is a pure liveness probe.**
Returns "ok" with zero DB interaction (`http/mod.rs:192–194`); with a corrupt/read-only DB or full disk the container stays "healthy" (B-9/B-10/B-11 compound this).

---

## Candidate deepening opportunities

1. **A connection-scoped "unit of work" seam in the repo.**
   *Module:* `src/db/repo/` (the `spawn` helper, `mod.rs:115–129`).
   *Seam:* add a private `conn`-scoped API — e.g. `Repo::with_conn(|conn, &mut Ctx| …)` or a small `UnitOfWork` that owns one pooled connection and one rusqlite `Transaction` (serialized `BEGIN IMMEDIATE`), with the existing one-shot methods re-expressed on top of it.
   *Shape:* callers like comment-post (comment + auto-approve + URLs in one commit) and the worker (upsert + seen in one commit) become single transactions; read paths (list + count + reaction counts; export's five tables) become one-connection batched reads; import becomes one connection with a batched-insert fast path.
   *Tests:* forced mid-unit failure proves rollback (comment absent *and* URLs absent); read APIs assert single connection acquisition; a concurrency test proves `BEGIN IMMEDIATE` converts busy-timeouts into clean retry errors instead of the current read-modify-write races (F9, B-17).

2. **Single source of truth for schema: migrations as data.**
   *Module:* `src/db/pool.rs` + `migrations/schema.sql`.
   *Seam:* replace `if current < N` blocks with a `const MIGRATIONS: &[&str]` array (index = version → the stamp *is* the version), and derive the fresh-install snapshot from the same list; keep `schema.sql` as documentation or generate it.
   *Shape:* `run_migrations` becomes ~30 lines (iterate pending steps, idempotency helpers already exist); `table_exists`/`column_exists`/`add_column_if_missing` stay as the safety net for v0.
   *Tests:* a parity test that upgrades a fabricated v0 DB through all steps and diff-maps `sqlite_master` + `PRAGMA table_info` against a fresh install — the test B-6 says is missing today (it would have caught the v8 reactions near-miss).

3. **A restore service inside the db layer.**
   *Module:* move the orchestration from `src/http/admin/data.rs:95–255` into the storage layer as e.g. `Repo::restore(export, policy) -> RestoreReport` (handler keeps only auth + JSON).
   *Seam:* the repo gains the "restore" concept instead of pass-through `import_*` primitives; per-section failure handling becomes one policy (skip-and-count everywhere, never `?`-abort — F4); salt re-derivation and warnings move beside the hash logic they reference.
   *Shape:* HTTP handler becomes ~20 lines; `ImportResponse`/`RestoreReport` shared; live-DB id-overlap detection (B-21) implemented once, where the SQL lives.
   *Tests:* FK-broken reaction import skips, not aborts (B-19); import-into-live-DB with colliding ids refuses/warns (B-21); crash-mid-import → re-import idempotence (B-24); counts correct under mixed skip/import.

4. **Concentrate the comment row mapping.**
   *Module:* `src/db/repo/comments.rs` + `mod.rs`.
   *Seam:* one `const COMMENT_COLUMNS: &str` (the 18-column list), one parameterized `query_comments(conn, where, params)` builder, and `row_to_comment` as the only mapper; `list_approved`/`list_approved_oldest`/`list_pending`/`list_comments` collapse their 12 duplicated SQL strings into 2–3 parameterized templates (also removing the redundant `let Some(_cursor)` branches, F10).
   *Shape:* no interface change for callers — purely internal; a schema column addition becomes a 2-line change plus the mapper instead of 12 edits.
   *Tests:* an insert→read round-trip asserting every one of the 18 fields survives at least one path per query variant (catches a SELECT missing a column); a grep-friendly lint test is overkill — the round-trip suffices.

5. **Typed storage errors.**
   *Module:* `src/db/mod.rs` (`RepoError`).
   *Seam:* split `Internal(String)` at least into `Constraint(&str)` (FK/UNIQUE/CHECK — row is invalid), `Io`/`Busy` (retryable), and `Other`; keep `Display`/`AppError` mapping identical so the HTTP surface doesn't change.
   *Shape:* import (D3) and the reactions upsert (F9) can then implement skip/retry policies structurally instead of string-matching; `lookup_author` likewise switches to parameters (F7) since `Constraint` makes future misuse visible.
   *Tests:* each variant maps to the right HTTP status; constraint errors from import boundaries are asserted end-to-end (FK-skip behavior becomes a direct test of the error type).

6. **Operator-facing DB diagnostics.**
   *Module:* `src/main.rs` startup + `src/http/mod.rs` healthz.
   *Seam:* a small config-gated check: `PRAGMA quick_check` (or the cheaper `wal_checkpoint(TRUNCATE)` + file-space probe) at startup, and a DB-touching branch in `/healthz` (read `PRAGMA user_version` with a short timeout).
   *Shape:* corrupt-file and disk-full conditions surface in the orchestrator instead of only at request time (B-9/B-10/B-11/B-29); migrate the B-3 duplicate-unique-index failure into a pre-flight error with operator guidance.
   *Tests:* healthz returns non-200 against a read-only/locked DB; startup quick_check failure produces the documented message; seed-duplicate legacy DB asserts the actionable-error path (B-3).