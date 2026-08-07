# Deployment

## Build

```sh
# With webmention support (default)
cargo build --release

# Comments-only (smaller binary, no webmentions)
cargo build --release --no-default-features --features comments
```

Binary ends up at `target/release/zapiska`.

## Feature flags

The binary can be compiled with or without webmention support:

| Feature | Default | Components |
|---|---|---|
| `comments` | on | Comment submission, threaded read API, moderation dashboard, embed widget, GitHub enrichment |
| `webmentions` | on | W3C webmention endpoint, background worker, SSRF protection, h-entry/microformats parser |

Disabling `webmentions` removes the webmention endpoint, worker, SSRF guards, and ~30 transitive dependencies (scraper, html5ever, ipnet). Comment features are unaffected.

## Run

```sh
export ADMIN_TOKEN="a-long-random-secret"
export GITHUB_TOKEN="ghp_your_github_pat"
./target/release/zapiska
```

The server binds `127.0.0.1:3000` by default. Put it behind a reverse proxy (nginx, Caddy, Traefik) that handles TLS.

## nginx example

Swap `comments.your-site.example` for the subdomain you host zapiska on, and point the TLS paths at your cert files.

```nginx
server {
    listen 443 ssl http2;
    server_name comments.your-site.example;

    ssl_certificate     /etc/letsencrypt/live/comments.your-site.example/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/comments.your-site.example/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Real-IP $remote_addr;
    }
}

server {
    listen 80;
    server_name comments.your-site.example;
    return 301 https://$host$request_uri;
}
```

## systemd service (bare-metal)

Ready-made template: [`deploy/zapiska.service`](../deploy/zapiska.service).
Conventions: binary at `/opt/zapiska/zapiska`, env at `/etc/zapiska/zapiska.env`.

```sh
sudo useradd -r -d /opt/zapiska -s /usr/sbin/nologin zapiska
sudo install -d -o zapiska -g zapiska /opt/zapiska /etc/zapiska
sudo cp target/release/zapiska /opt/zapiska/
sudo cp .env.example /etc/zapiska/zapiska.env
sudo $EDITOR /etc/zapiska/zapiska.env      # set ADMIN_TOKEN, origins
sudo cp deploy/zapiska.service /etc/systemd/system/zapiska.service
sudo systemctl daemon-reload
sudo systemctl enable --now zapiska
sudo journalctl -u zapiska -f              # watch the logs
```

Environment file (`/etc/zapiska/zapiska.env`) — see [.env.example](../.env.example) for the full list:

```
ADMIN_TOKEN=your-secret-here
GITHUB_TOKEN=ghp_optional_token
PUBLIC_TARGET_ORIGIN=https://your-site.example
ALLOWED_CORS_ORIGIN=https://your-site.example
DATABASE_PATH=/opt/zapiska/comments.db
RUST_LOG=info
```

## OpenRC service (Alpine / Gentoo)

Ready-made template: [`deploy/zapiska.openrc`](../deploy/zapiska.openrc).

```sh
adduser -S -h /opt/zapiska zapiska
install -d -o zapiska -g zapiska /opt/zapiska /etc/zapiska
cp target/release/zapiska /opt/zapiska/
cp .env.example /etc/zapiska/zapiska.env
vi /etc/zapiska/zapiska.env                # set ADMIN_TOKEN, origins
cp deploy/zapiska.openrc /etc/init.d/zapiska
chmod +x /etc/init.d/zapiska
rc-update add zapiska default
rc-service zapiska start
rc-service zapiska status
```

## Docker (recommended)

Single-command deploy. The repo ships a canonical `docker-compose.yml` that
builds the image, wires up a persistent volume, healthcheck, and restart
policy, and passes through every configuration variable from your `.env`:

```sh
cp .env.example .env       # set ADMIN_TOKEN at minimum
docker compose up -d       # build + run — that's it
```

`docker compose up` fails fast with `set ADMIN_TOKEN in your .env file` if
the token is missing. Everything else has a sane default; uncomment the
sections you need (notifications, Turnstile, GitHub token).

Useful commands:

```sh
docker compose logs -f zapiska   # follow logs
docker compose ps                # status incl. health (healthy/unhealthy)
docker compose down              # stop; the zapiska-data volume survives
docker compose pull/build        # update the image
```

Manual `docker run` is also supported — the image has a built-in healthcheck
against `/healthz`:

```sh
docker build -t zapiska .
docker run -d \
  -p 3000:3000 \
  -e ADMIN_TOKEN=your-secret \
  -v zapiska-data:/data \
  zapiska
```

Updating: `git pull && docker compose up -d --build`.

The container runs as a non-root user (`appuser`), stores SQLite under
`/data` (a named volume), and binds to `127.0.0.1:3000` on the host by
default — put it behind a reverse proxy that handles TLS.

### Pre-built images on GHCR

On every `v*` tag, CI builds a multi-arch image (`linux/amd64`,
`linux/arm64`) and pushes it to **GitHub Container Registry**:

```
ghcr.io/<owner>/zapiska:<version>     # e.g. ghcr.io/nithitsuki/zapiska:v0.1.0
ghcr.io/<owner>/zapiska:<major>.<minor>
ghcr.io/<owner>/zapiska:latest
```

Usage:

```sh
docker pull ghcr.io/nithitsuki/zapiska:v0.1.0
docker run -d -p 3000:3000 -e ADMIN_TOKEN=your-secret \
  -v zapiska-data:/data ghcr.io/nithitsuki/zapiska:v0.1.0
```

- For **public** repos the images are pullable anonymously — no login needed.
- For **private** repos, log in first: `echo $GITHUB_TOKEN | docker login ghcr.io -u <user> --password-stdin`.
- Images only exist **after a release tag is pushed** — `docker compose up` builds from source instead, which always works.

## SQLite

Database is a single file (`./comments.db` by default). Runs in WAL mode:

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;
```

To back it up, copy the file (and the `-wal` file if present).

## GitHub token

Optional. Without it the server uses anonymous GitHub API (60 req/hr). With a PAT the limit goes to 5000/hr. No special scope needed.

Results are cached in `github_profiles` table: 30 days for positive, 1 hour for negative (user doesn't exist).

## Health check

```sh
curl http://127.0.0.1:3000/healthz
# -> ok
```

## Logging

All output goes to stdout as structured JSON via `tracing`. Redirect to a file:

```sh
./zapiska >> /var/log/zapiska.log 2>&1
```

Or use systemd's built-in journal (`journalctl -u zapiska`). Log level is controlled by `RUST_LOG` env var (default `info`). Set to `debug` for verbose output, `warn` for errors only.

Never logs raw `content`, `author_url`, or `ADMIN_TOKEN`. Safe fields: `id`, `target_path`, `status`, `peer_ip`.

## Graceful shutdown

Handles SIGTERM and SIGINT. Stops accepting connections, drains the webmention worker queue, then exits.

## Cloudflare Turnstile (optional bot protection)

Turnstile is **off by default**. When enabled, native comment submissions are rejected unless they include a valid `cf-turnstile-response` token from the Cloudflare Turnstile widget. zapiska verifies the token against Cloudflare's `siteverify` endpoint directly from the Rust backend — no Worker, no extra runtime.

| Variable | Default | Description |
|---|---|---|
| `TURNSTILE_ENABLED` | `false` | Master switch. When `false`, the `cf-turnstile-response` field is ignored entirely. |
| `TURNSTILE_SECRET_KEY` | *(required when enabled)* | The Turnstile secret key from the Cloudflare dashboard. Loaded once at startup, never logged, never written to disk. Startup fails fast if `TURNSTILE_ENABLED=true` and this is unset. |
| `TURNSTILE_VERIFY_URL` | `https://challenges.cloudflare.com/turnstile/v0/siteverify` | Override for the siteverify endpoint. Use only for tests or proxies. Must be `https://`. |

To enable:

1. Create a widget at https://dash.cloudflare.com → Turnstile. Note the **sitekey** (public) and the **secret**.
2. Add the widget's allowed hostnames — your main site origin (where the form lives), plus `localhost` and `127.0.0.1` for local dev.
3. Set `TURNSTILE_ENABLED=true` and `TURNSTILE_SECRET_KEY=<secret>` in zapiska's env.
4. Add the Turnstile widget to your comment form (see [getting-started.md](getting-started.md) and [../embed/README.md](../embed/README.md)).
5. Restart zapiska. Submissions without a valid token now return `400 { "code": "turnstile_failed" }`. If zapiska can't reach Cloudflare's siteverify endpoint, it returns `503` and **fails closed** — no comment is stored.

The sitekey is public and lives in your HTML; the secret lives only in zapiska's env / Worker secret store. Keep them separate.

## Script-based moderation

The admin API is designed to be consumed by automated moderation scripts. Typical workflow:

```sh
# 1. Log in to get a session cookie
curl -c cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d '{"token":"your-admin-token"}' \
  http://localhost:3000/api/admin/login

# 2. Fetch pending comments (with parent context via {id} endpoint)
curl -b cookies.txt http://localhost:3000/api/admin/comments?status=pending

# 3. For each pending comment, fetch its parent chain for context
curl -b cookies.txt http://localhost:3000/api/admin/comments/42

# 4. Submit moderation decisions in batch
curl -b cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d '{"actions":[
    {"id":42,"action":"approved"},
    {"id":43,"action":"spam"}
  ]}' \
  http://localhost:3000/api/admin/moderate/batch
```

## Updating

```sh
git pull
cargo build --release

# systemd
sudo systemctl restart zapiska

# OpenRC
sudo rc-service zapiska restart

# Docker
docker compose up -d --build

# Pre-built image (tagged releases only)
docker pull ghcr.io/<owner>/zapiska:latest
docker compose up -d   # or: docker run with the image above
```

Schema migrations are idempotent and run automatically on startup.
