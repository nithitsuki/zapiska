# Skill: Set up zapiska server

Use this skill when the user wants to deploy and run a zapiska instance.
zapiska is a Rust + SQLite self-hosted comment engine. This skill covers
Docker, pre-built binary, or build-from-source deployment behind a reverse
proxy with TLS.

## Before you start

Confirm (or ask for):

- The main site origin, e.g. `https://your-site.example`
- The subdomain for zapiska, e.g. `https://comments.your-site.example`
- Whether the user prefers Docker, a pre-built binary, or building from source
- Whether the server has a public IP and a reverse proxy (nginx/Caddy/Traefik)

## Step 1: Prepare the host

Requirements:

- Linux server with a public IP (a $5 VPS is fine)
- The subdomain A/AAAA record pointing at the server
- A reverse proxy that can terminate TLS
- Rust 1.85+ (if building from source) or Docker (if using containers)

Generate a strong admin token:

```sh
openssl rand -base64 32
```

## Step 2: Get the code

Clone the repository:

```sh
git clone https://github.com/nithitsuki/zapiska.git
cd zapiska
```

## Step 3: Choose and execute a deployment path

### Path A: Docker (recommended for most users)

1. Copy the example files:

   ```sh
   cp docker-compose-example.yml docker-compose.yml
   cp .env.example .env
   ```

2. Edit `.env` and `docker-compose.yml`. Set at minimum:

   - `ADMIN_TOKEN` — the token generated in Step 1
   - `PUBLIC_TARGET_ORIGIN=https://your-site.example`
   - `ALLOWED_CORS_ORIGIN=https://your-site.example`

3. Build and start:

   ```sh
   docker compose up -d --build
   ```

4. Verify the container is running and the health endpoint responds:

   ```sh
   curl http://127.0.0.1:3000/healthz
   # -> ok
   ```

### Path B: Build from source

1. Install dependencies:

   ```sh
   # Debian/Ubuntu
   sudo apt-get install -y libsqlite3-dev pkg-config
   ```

2. Build:

   ```sh
   cargo build --release
   ```

3. Copy the binary to a permanent location (e.g. `/opt/zapiska/zapiska`) and
   create an `.env` file with the required variables.

4. Run manually for a quick check:

   ```sh
   ./target/release/zapiska
   ```

5. Stop the manual process and proceed to create a systemd service (Step 5).

### Path C: Pre-built binary

If a release binary is available for the target architecture, download it,
place it in `/opt/zapiska/`, create an `.env` file, and continue with Step 5.

## Step 4: Configure environment variables

Required:

| Variable | Example |
|---|---|
| `ADMIN_TOKEN` | output of `openssl rand -base64 32` |
| `PUBLIC_TARGET_ORIGIN` | `https://your-site.example` |
| `ALLOWED_CORS_ORIGIN` | `https://your-site.example` |
| `DATABASE_PATH` | `/opt/zapiska/comments.db` (or `/data/comments.db` in Docker) |

Recommended optional variables:

- `GITHUB_TOKEN` — raises GitHub API rate limit for avatar lookup
- `STORE_IP_ADDRESS=true` — enables IP-based moderation signals
- `TURNSTILE_ENABLED=true` + `TURNSTILE_SECRET_KEY` — bot protection

Full reference: `docs/deployment.md` and `.env.example`.

## Step 5: Put it behind a reverse proxy

The server listens on `127.0.0.1:3000` by default. It expects TLS to be
terminated upstream.

### nginx

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

### Caddy

```caddy
comments.your-site.example {
    reverse_proxy 127.0.0.1:3000
}
```

## Step 6: Systemd service (non-Docker deployments)

Create `/etc/systemd/system/zapiska.service`:

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

Then:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now zapiska
sudo journalctl -u zapiska -f
```

## Step 7: Validate

From any shell that can reach the server:

```sh
export ADMIN_TOKEN=your-token-here
curl https://comments.your-site.example/healthz
# -> ok

curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://comments.your-site.example/api/admin/pending
# -> {"comments":[]}

curl 'https://comments.your-site.example/api/comments?path=/'
# -> {"total":0,"comments":[]}
```

Interactive API docs are at `/swagger-ui/`.

## Troubleshooting

- **Config error at startup**: ensure `ADMIN_TOKEN` is set and non-empty.
- **CORS errors in browser**: `ALLOWED_CORS_ORIGIN` must exactly match the
  main site's origin (scheme + host + port).
- **Database permission denied**: ensure the `zapiska` user owns the
  `DATABASE_PATH` directory (non-Docker) or the named volume is writable
  (Docker).
- **TLS errors**: verify the reverse proxy serves a valid certificate for
  `comments.your-site.example`.
