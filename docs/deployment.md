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

```nginx
server {
    listen 443 ssl http2;
    server_name webmention.nithitsuki.com;

    ssl_certificate     /etc/ssl/certs/nithitsuki.com.pem;
    ssl_certificate_key /etc/ssl/private/nithitsuki.com.key;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Real-IP $remote_addr;
    }
}

server {
    listen 80;
    server_name webmention.nithitsuki.com;
    return 301 https://$host$request_uri;
}
```

## systemd service

```ini
[Unit]
Description=zapiska comment server
After=network.target

[Service]
Type=simple
User=zapiska
WorkingDirectory=/opt/zapiska
EnvironmentFile=/opt/zapiska/.env
ExecStart=/opt/zapiska/zapiska
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Environment file (`/opt/zapiska/.env`):

```
ADMIN_TOKEN=your-secret-here
GITHUB_TOKEN=ghp_optional_token
PUBLIC_TARGET_ORIGIN=https://nithitsuki.com
ALLOWED_CORS_ORIGIN=https://nithitsuki.com
DATABASE_PATH=/opt/zapiska/comments.db
RUST_LOG=info
```

## Docker

Build the image:

```sh
docker build -t zapiska .
```

Run it:

```sh
docker run -d \
  -p 3000:3000 \
  -e ADMIN_TOKEN=your-secret \
  -v zapiska-data:/data \
  zapiska
```

Or use docker-compose (see [docker-compose.yml](../docker-compose.yml)).

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
sudo systemctl restart webmention
```

Schema migrations are idempotent and run automatically on startup.
