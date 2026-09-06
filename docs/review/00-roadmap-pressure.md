# 00 — Roadmap pressure map

What the planned features (`.FEATURE_IDEAS.md`, README, SPEC) will demand from
the base architecture. Written during the 2026-09-06 review so candidate
deepening opportunities are judged against tomorrow, not just today.

## Planned/backlog features

| Feature | Status | What it pulls on architecturally |
|---|---|---|
| Voting, pinning, verification | planned | The reaction/signal concept |
| Disqus + WordPress import | planned | The import module, content pipeline |
| Logged-in users (email, sessions, OAuth) | planned | The identity concept — the deepest shock |
| Storage backends (Postgres, JSON) | deferred | The storage seam, export format stability |

## Pressure per feature

### 1. Logged-in users — identity becomes first-class

Today **identity** is thin: an author name string, optional URL, an IP-derived
hash, an admin token. Registered users change the picture:

- **Reactions**: `anyone` mode keys reactions by peer-address hash. With
  accounts, a reaction should key by account, survive IP changes, and not be
  droppable by secret rotation. The reaction identity abstraction must span
  "anonymous peer hash" and "account".
- **Deletion**: the delete-token model only makes sense for anonymous
  submitters. Accounts need self-service deletion/sign-in-gated edits.
- **Statuses**: moderation may become trust-weighted (known user vs anonymous),
  which pulls on the moderation concept (who may transition, what fires).
- **Peer data**: `STORE_IP_ADDRESS` and IP hashes get a second life as abuse
  signals tied to accounts.
- **Export format**: an export version bump or additive fields will be needed;
  version-1 files must still import.

### 2. Voting / pinning / verification — the reaction concept generalizes

Reactions are now: a configured set, one active row per (comment, identity),
own status lifecycle. Voting is a numeric score; pinning is an admin-controlled
visibility flag per comment; verification is a boolean on identity. Before
adding these, the base question is: does "signal on a comment owned by an
identity, with moderation" deserve its own concept, or does the reaction table
grow columns forever?

### 3. Third-party import — the import module needs adapter seams

Today import accepts one format: zapiska export version 1. Disqus/WXR import
means: foreign ID mapping, thread reconstruction from foreign parent chains,
status mapping (disqus approved ⊂ ours), re-running the whole content pipeline
(sanitize → truncate → hash → language gate?) over foreign content, and
preserving original timestamps. The import module needs a seam where "zapiska
export v1" is one adapter and "WXR" another — without the format logic
sprawling into the admin handler.

### 4. Storage backends — the storage seam is already planned

`.FEATURE_IDEAS.md` records the intent: `CommentStore` trait + delegating
store enum, tokio-postgres + deadpool for Postgres, Cargo features for backend
selection, keep full JSON parity. Every SQL statement and every
rusqlite-specific type that leaks into handlers or the worker caps the seam
before it exists. The export version-1 document is the portable contract that
makes backend migration survivable — its stability is an architectural
constraint, not a convenience.

## Product-level use-case pressure (current features)

- **BYO moderation** (README): external rules engines / LLM moderation services
  are a first-class audience. The moderation webhook is the contract with them:
  payload schema, idempotency, retry behavior, and sync-mode failure semantics
  (what happens to a comment when the moderation service is down — is the
  submitter rejected, or is the comment stored pending?).
- **Long-lived single-site deployment**: years of comments, feeds that grow
  without bound, exports that grow. Paging and streaming behavior of
  "everything" endpoints matters over time.
- **Single-process topology**: in-memory rate limits, daily caps, and
  notification windows quietly assume one process. If that is a deliberate
  product decision ("one site, one process"), it should be a recorded decision
  (ADR) so nobody triples the instance and silently splits the limits.
- **Backup/restore loop**: the IP_HASH_SECRET warning in `main.rs` shows the
  backup contract is already load-bearing: restore = DB + `.env` together.
  Any new secret (Turnstile, OAuth client secrets later, TURNSTILE keys) must
  stay in that same contract, and the export must keep stating what it cannot
  carry.

## What to watch in the findings

- Whether repository SQL leaks across seams today (backend-enum pressure).
- Whether the content pipeline (sanitize → hash → gate → extract → store) is
  one module or a choreography the handler owns.
- Whether reactions are already a separate concept or a comment column.
- Whether webhook/moderation contracts are versioned and idempotent.