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
| `PUBLIC_TARGET_ORIGIN` | `https://nithitsuki.com` | Parsed origin for accepted webmention targets. |
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
| `STORE_IP_ADDRESS` | `false` | Store raw and hashed peer IP values. |
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

The rate limiter uses the TCP peer address. A reverse proxy can make many
visitors share one peer address. Use a deployment path that preserves the
client address when per-client limits matter.

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
The server redacts the admin, GitHub, Telegram, and Turnstile secret values.
Review log access because request spans can include the peer IP.

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

The response is `ok` with status `200`.
