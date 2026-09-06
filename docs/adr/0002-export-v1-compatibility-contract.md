# Export v1 as the load-bearing compatibility contract

The admin export (`GET /api/admin/export`, `version: 1`) is the portable path between backends, machines, and restores: all five tables plus `ip_hash_salted`, never the `IP_HASH_SECRET` itself. Import refuses any other version loud instead of guessing, and serde ignores unknown fields, so additive fields stay within v1 while any breaking reshape must bump the version.

## Consequences

A future v2 must add a version bump plus a refuse-or-migrate rule in `Repo::restore` — v1 files stay importable. Operators back up `.env` (`IP_HASH_SECRET`) next to the JSON: comment hashes re-derive on import, but anyone-mode reaction identities orphan on a lost secret, and that is reported as a `warning`, not an error.
