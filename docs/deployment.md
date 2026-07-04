# Deployment

## Build

```sh
cargo build --release
```

Binary ends up at `target/release/zapiska`.

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
Description=webmention.nithitsuki.com
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

## Graceful shutdown

Handles SIGTERM and SIGINT. Stops accepting connections, drains the webmention worker queue, then exits.

## Updating

```sh
git pull
cargo build --release
sudo systemctl restart webmention
```

Schema migrations are idempotent and run automatically on startup.