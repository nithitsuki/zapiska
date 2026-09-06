# Notification delivery is bounded retry, not at-most-once

Each channel POSTs up to 3 attempts with backoff (transient: network errors, 429 with `Retry-After` honored up to 2 s, 5xx; permanent 4xx and Telegram `ok: false` go once), then logs and drops. Dispatch never blocks submission — spawned on the hot path, awaited only by the shutdown drain. A timeout cannot tell "lost request" from "lost response", so a retry can duplicate a digest; that is accepted and documented, not claimed away.

## Considered Options

Durable at-least-once via an outbox table plus a deliver worker was rejected: admin digests do not justify the write path, retention, and replay semantics. True at-most-once (single attempt) was rejected the other way: one flaky POST should not eat an alert.

## Consequences

Admins may rarely get a digest twice after a slow channel; they never wait on delivery, and a dead channel costs one warn log per window, not a stuck submission.
