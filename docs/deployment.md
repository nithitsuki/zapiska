# Deployment

zapiska runs as a single process with one SQLite file. Put the process behind a
TLS reverse proxy for a public deployment.

## Build

Build with webmention support:

```sh
cargo build --release
```

Build a comments-only binary:

```sh
cargo build --release --no-default-features --features comments
```

The binary is `target/release/zapiska`.

`rusqlite` uses the bundled SQLite library. A system SQLite development package
is not required for the application build.

## Feature flags

| Feature | Default | Result |
|---|---|---|
| `comments` | Yes | Empty compatibility feature. Comment code remains compiled. |
| `webmentions` | Yes | Webmention ingress, worker, microformats parsing, and SSRF code. |

The comments-only command disables the `webmentions` feature.

## Environment

Copy the example file:

```sh
cp .env.example .env
```

The server needs `ADMIN_TOKEN`. The other values have defaults.

WARNING: Keep `ADMIN_TOKEN`, webhook URLs, and Turnstile secrets out of source
control. A leaked admin token gives access to protected data and actions.

### Required deployment values

| Variable | Default | Description |
|---|---|---|
| `ADMIN_TOKEN` | None | Token for protected admin routes. |
| `PUBLIC_TARGET_ORIGIN` | `https://nithitsuki.com` | Parsed origin for accepted webmention targets. Must be an absolute `http(s)` URL with a host; anything else fails startup. |
| `ALLOWED_CORS_ORIGIN` | `https://nithitsuki.com` | One origin, a comma-separated list, or `*`. |
| `DATABASE_PATH` | `./comments.db` | SQLite file path. |
| `BIND_ADDR` | `127.0.0.1:3000` | Listen address. |

### Limits and network values

| Variable | Default | Description |
|---|---:|---|
| `MAX_CONTENT_LEN` | `2000` | Maximum stored content length in characters. |
| `MAX_AUTHOR_LEN` | `100` | Maximum author name length in characters. |
| `MAX_BODY_SIZE` | `8192` | Global request body limit in bytes. |
| `FETCH_TIMEOUT_MS` | `4000` | Outbound request timeout. |
| `WORKER_BACKLOG` | `64` | Webmention queue capacity. |
| `RUST_LOG` | `info` | `tracing` filter. |

### Rate limit values

| Variable | Default | Route |
|---|---:|---|
| `RATE_LIMIT_NATIVE` | `100` | Native comments, deletion, and reactions. |
| `RATE_LIMIT_NATIVE_WINDOW` | `60` | Native limit window in seconds. |
| `RATE_LIMIT_WEBMENTION` | `60` | Webmention ingress burst. |
| `RATE_LIMIT_WEBMENTION_WINDOW` | `60` | Webmention limit window in seconds. |
| `RATE_LIMIT_READ` | `300` | Public comments and RSS. |
| `RATE_LIMIT_READ_WINDOW` | `60` | Read limit window in seconds. |
| `RATE_LIMIT_ADMIN_MODERATE` | `30` | Single comment moderation. |
| `RATE_LIMIT_ADMIN_MODERATE_WINDOW` | `60` | Admin moderation window in seconds. |

The limits use the TCP peer IP. The server does not trust forwarded IP headers.

### Moderation and privacy values

| Variable | Default | Description |
|---|---|---|
| `DEFAULT_COMMENT_STATUS` | `pending` | Initial status for native comments. |
| `MODERATION_WEBHOOK_URL` | Unset | External moderation webhook URL. |
| `MODERATION_WEBHOOK_MODE` | `async` | `async` or `sync`. |
| `STORE_IP_ADDRESS` | `false` | Store raw and hashed peer IP values. Accepts `true` (any case) or `1`. |
| `IP_HASH_SECRET` | Unset | Salt for the stored IP hash. |
| `MAX_COMMENTS_PER_IP_PER_DAY` | `50` | Native comment daily cap. Zero disables the cap. |
| `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR` | `10` | Webmention domain cap. Zero disables the cap. |
| `HONEYPOT_FIELD` | `website` | Loaded setting. The current form handler reads `website`. |

### Notification values

| Variable | Default | Description |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | Unset | Telegram bot token. |
| `TELEGRAM_CHAT_ID` | Unset | Telegram destination. |
| `TELEGRAM_API_BASE` | `https://api.telegram.org` | Telegram API base URL. |
| `SLACK_WEBHOOK_URL` | Unset | Slack incoming webhook URL. |
| `DISCORD_WEBHOOK_URL` | Unset | Discord incoming webhook URL. |
| `NOTIFY_BATCH_SECS` | `60` | Notification window. Zero sends immediately. |
| `NOTIFY_BATCH_THRESHOLD` | `20` | Early flush count. Zero disables early flush. |
| `NOTIFY_BATCH_GRANULARITY` | `page` | `page` or `global`. |

Telegram needs both Telegram values. Slack and Discord need their webhook URL.
Notification delivery is asynchronous. It does not change the comment result.
Setting only one of the two Telegram values logs a startup warning and
disables Telegram delivery.

Batching uses fixed windows: the first comment on a page opens a window that
flushes exactly `NOTIFY_BATCH_SECS` later (or early once
`NOTIFY_BATCH_THRESHOLD` comments accumulate); steady traffic never extends
the window. One timer task is armed per window, when it opens. On shutdown
the server drains open windows as final digests after the listener stops, so
a restart during a burst still alerts the admin. Each delivery is attempted
up to three times with backoff on transient failures (network errors, 429,
5xx), then dropped with a warning log: bounded retry (duplicates possible
on timeout), log-and-drop — submissions never block on delivery. `Retry-After`
on 429s is honored up to a 2 s cap per wait. Shutdown drain latency is
bounded to 12 s total and also awaits in-flight sends; the drain runs twice
to catch a webmention job finishing mid-drain, which is otherwise the one
narrow best-effort case. Channel size limits are enforced at format
time (Telegram 4096, Slack 3000, Discord 2000 characters).

### Reaction values

| Variable | Default | Description |
|---|---|---|
| `REACTIONS_ALLOWED` | `admin` | `admin` or `anyone`. |
| `REACTIONS_SET` | `👍,❤️,😄,😮,😢,😡` | Comma-separated allowed values. |

The `anyone` mode uses a hash of the peer IP. Use it only with additional abuse
controls.

### Language values

| Variable | Default | Description |
|---|---|---|
| `COMMENT_LANG_ALLOWED` | Unset | ISO 639-1 allow list. |
| `COMMENT_LANG_BLOCKED` | Unset | ISO 639-1 block list. Ignored when an allow list exists. |
| `COMMENT_LANG_ALLOW_EMOJI` | `always` | `always`, `never`, or `if_unknown`. |
| `MAX_THREAD_DEPTH` | `0` | Reply depth. The server clamps this value to `0` through `10`. |

The language gate applies to native comments. It does not apply to webmentions.

### Turnstile values

| Variable | Default | Description |
|---|---|---|
| `TURNSTILE_ENABLED` | `false` | Require a valid Turnstile token for native comments. |
| `TURNSTILE_SECRET_KEY` | Unset | Required when Turnstile is enabled. |
| `TURNSTILE_VERIFY_URL` | Cloudflare siteverify URL | HTTPS endpoint for token checks. |

## Docker Compose

The repository includes `docker-compose.yml`.

```sh
cp .env.example .env
docker compose up -d --build
docker compose ps
curl http://127.0.0.1:3000/healthz
```

The compose file:

- Builds the local image.
- Passes `.env` values to the container.
- Sets `DATABASE_PATH=/data/comments.db`.
- Maps host `127.0.0.1:3000` to container port `3000`.
- Stores data in the `zapiska-data` volume.
- Runs a health check against `/healthz`.
- Restarts the container unless it is stopped.

Use these commands:

```sh
```

`docker compose down` keeps the named volume. Remove the volume only when you
intend to remove the database.

The compose file builds locally. To use a GHCR image, use `docker run` or an
image-specific compose file.

## GHCR image

Version tags build `linux/amd64` and `linux/arm64` images.

```sh
  --name zapiska \
  -p 127.0.0.1:3000:3000 \
  -e BIND_ADDR=0.0.0.0:3000 \
  -e ADMIN_TOKEN=your-secret \
  -e PUBLIC_TARGET_ORIGIN=https://your-site.example \
  -e ALLOWED_CORS_ORIGIN=https://your-site.example \
  -v zapiska-data:/data \
  ghcr.io/nithitsuki/zapiska:v0.2.0
```

Public repositories allow anonymous image pulls. Private repositories need a
GHCR login.

## Reverse proxy

The bare-metal default is `127.0.0.1:3000`. Terminate TLS at the proxy.

### nginx

```nginx
server {
    listen 443 ssl http2;
    server_name comments.your-site.example;

    ssl_certificate /etc/letsencrypt/live/comments.your-site.example/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/comments.your-site.example/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### Caddy

```caddy
comments.your-site.example {
    reverse_proxy 127.0.0.1:3000
}
```

The rate limiter keys on one normalized client identity per request
(`src/http/peer.rs`): the TCP peer address, with IPv4-mapped IPv6
canonicalized to IPv4. A reverse proxy makes many visitors share one peer
address and one quota. Set `TRUST_PROXY=true` only when the proxy
overwrites the client-IP headers — the server then reads the leftmost
`X-Forwarded-For` entry, else `X-Real-IP`, else the first `Forwarded for=`,
else the peer. The edge must overwrite, not append: with append-only
forwarding any client can send its own `X-Forwarded-For` and pick another
client's identity and quota. (`proxy_set_header` in the nginx snippet above
overwrites; confirm the same for any other edge before enabling the flag.)

## systemd

The repository includes [`deploy/zapiska.service`](../deploy/zapiska.service).
It uses this layout:

- Binary: `/opt/zapiska/zapiska`
- Environment: `/etc/zapiska/zapiska.env`
- Database: `/opt/zapiska/comments.db`

Install it with:

```sh
sudo useradd -r -d /opt/zapiska -s /usr/sbin/nologin zapiska
sudo install -d -o zapiska -g zapiska /opt/zapiska /etc/zapiska
sudo cp target/release/zapiska /opt/zapiska/
sudo cp .env.example /etc/zapiska/zapiska.env
sudo cp deploy/zapiska.service /etc/systemd/system/zapiska.service
sudo systemctl daemon-reload
sudo systemctl enable --now zapiska
```

Set the required values before the service starts.

## OpenRC

The repository includes [`deploy/zapiska.openrc`](../deploy/zapiska.openrc).

```sh
adduser -S -h /opt/zapiska zapiska
install -d -o zapiska -g zapiska /opt/zapiska /etc/zapiska
cp target/release/zapiska /opt/zapiska/
cp .env.example /etc/zapiska/zapiska.env
cp deploy/zapiska.openrc /etc/init.d/zapiska
chmod +x /etc/init.d/zapiska
rc-update add zapiska default
rc-service zapiska start
```

## SQLite and backups

The database is one SQLite file. Connections use WAL mode, foreign keys, a
5000 millisecond busy timeout, and `synchronous = NORMAL`.

For a file backup, stop the service first:

```sh
sudo systemctl stop zapiska
cp /opt/zapiska/comments.db /backups/comments.db
cp /opt/zapiska/comments.db-wal /backups/comments.db-wal 2>/dev/null || true
sudo systemctl start zapiska
```

For a live backup, use the authenticated JSON export:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://127.0.0.1:3000/api/admin/export > backup.json
```

The export reads all five tables in one transaction on one connection — a
single WAL snapshot — so it stays consistent while traffic is live: a comment
created mid-export cannot appear in only some of the arrays.

The export includes comments, webmention state, extracted URLs, GitHub
profiles, reactions, and an `ip_hash_salted` flag recording whether
`IP_HASH_SECRET` was set on the exporting server.

WARNING: The export never contains `IP_HASH_SECRET` itself, but stored IP
hashes are salted with it. Back up your `.env` (or at least the secret)
alongside `backup.json`. A lost or rotated secret splits IP-hash continuity:
comment hashes are re-derived on import where a raw IP exists, but
anyone-mode reaction identities cannot be re-derived and are orphaned. The
import response carries a `warning` when it detects this mismatch, and the
server logs a warning at startup when the database holds hashes but the
secret is unset.

### Restore procedure

Restore targets an empty database. To migrate or recover:

```sh
# 1. Stop the service and start it against a fresh database file
#    (or point DATABASE_PATH at an empty file).
# 2. Restore the export:
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  --data @backup.json \
  http://127.0.0.1:3000/api/admin/import
```

The response reports per-section `*_imported` and `*_skipped` counts plus
`ip_hashes_recomputed` and a salt-mismatch `warning` when one applies.
Skipped rows never abort the restore: fix the source data and re-import —
re-importing the same document is idempotent, so a retried or crashed
import heals to the full state.

Overlap refusal: importing into a database whose comment or reaction IDs
already hold different data is refused with `400` before the first write —
an old backup never silently reverts live moderation decisions. Re-run with
`"force": true` added to the import body only to deliberately overwrite
colliding live rows after review.

WARNING: The export file contains raw submitter IPs and `delete_token`
values alongside content. Anyone holding `backup.json` can delete any
imported native comment that still carries its token, and can read every
stored peer address. Treat the export as a secret document: restrict file
permissions, encrypt off-site copies, and never publish it.

Salt rotation: keep `IP_HASH_SECRET` stable and back it up with `.env`.
Comment hashes are re-derived on import where a raw IP exists, but
anyone-mode reaction identities cannot be re-derived — a lost or rotated
secret orphans them (old voters look like strangers). The import response
warns when it detects this mismatch.

### Legacy duplicate `(source_url, target_path)` rows

Very old databases may predate the partial unique index
`idx_comments_source_target` and hold two rows with the same
`(source_url, target_path)` pair. Startup refuses to boot with an error
naming the index and the offending pairs instead of failing on a raw SQLite
message. Dedup first, keeping the newest row per pair, then restart:

```sql
DELETE FROM comments WHERE id NOT IN (
  SELECT MAX(id) FROM comments
  WHERE source_url IS NOT NULL
  GROUP BY source_url, target_path
);
```

## Turnstile

Turnstile is off by default. When enabled, every native comment submission
needs a valid `cf-turnstile-response` value.

1. Create a Turnstile widget in the Cloudflare dashboard.
2. Add the main site host and local development hosts.
3. Set `TURNSTILE_ENABLED=true` and `TURNSTILE_SECRET_KEY`.
4. Add the public sitekey to the comment form.
5. Restart zapiska.

The server checks the token with Cloudflare. It returns `400` for a failed
check and `503` when the verify endpoint is not reachable. The comment is not
stored in either case.

See [the Turnstile skill](../.skills/configure-turnstile/SKILL.md) for form
examples.

## Logging

Logs use JSON output from `tracing`. Set `RUST_LOG=debug` for more detail.
The server redacts the admin, GitHub, Telegram, and Turnstile secret values,
and shows only the host plus a truncated path for Slack, Discord, and
moderation webhook URLs. Review log access because request spans can include
the peer IP.

## Updates

From source:

```sh
cargo build --release
sudo systemctl restart zapiska
```

With Docker:

```sh
```

Schema creation is idempotent and runs at startup.

## Health check

```sh
curl http://127.0.0.1:3000/healthz
```

A healthy server answers `ok` with status `200`. The endpoint issues
`SELECT 1` through the connection pool (bounded to two seconds), so it is a
readiness probe, not just liveness: a wedged database (full disk,
corruption, lost volume) answers `unavailable` with status `503`, and the
Docker and compose health checks flip unhealthy accordingly.

## Startup integrity gate

| Variable | Default | Description |
|---|---|---|
| `DB_QUICK_CHECK` | `true` | Run `PRAGMA quick_check` at startup and refuse to start when the database reports corruption. Accepts `true` (any case) or `1`. Set to `false` only to bypass the gate for recovery. |

`quick_check` does most of the checking of `PRAGMA integrity_check` but runs
much faster, so the boot cost is small. A failure names the
corruption and tells you to restore from backup (or re-import a known-good
JSON export) before starting.
