# Single-process topology with an advisory lock

zapiska keeps notification windows, the per-IP/per-domain `Limiter`, and the governor buckets in memory, so two processes on one SQLite file would silently split quotas and double-send digests. We run one process per database (the self-hosted single-site scope needs nothing more) instead of shared state in Redis or the database, and enforce it with a std-only `<database>.lock` sibling file holding the holder PID: atomic claim, Linux `/proc` liveness with a PID-reuse guard for stale reclaim, refusal with a remedy otherwise.

## Consequences

SQLite's 5 s `busy_timeout` still serializes writers, so a second process corrupts nothing — it just gets refused at startup. Stale detection is Linux-only; elsewhere a leftover lockfile needs one manual delete. Two instances with different database files never block each other.
