# Getting started with zapiska

This guide assumes you already know how to self-host a website: you have a server with a public IP, can put a service behind a reverse proxy, can get a TLS cert, and are comfortable with the shell.

By the end you'll have zapiska running on `https://comments.your-site.example`, serving comments for `https://your-site.example`.

> Time: ~15 minutes if you've got Rust or Docker handy.

---

## What you're running

zapiska is a single Rust binary backed by one SQLite file. There is no Node, no Postgres, no Redis. You don't run migrations by hand — they apply on startup.

It exposes:

- A public **read** API (`GET /api/comments`) your site calls to show approved comments.
- A public **submit** API (`POST /api/comment`) your comment form posts to.
- A **webmention** endpoint (`POST /api/webmention`) other IndieWeb sites ping.
- **Admin** endpoints (`/api/admin/*`) you call to approve, spam, delete.
- A static **embed widget** at `/embed/comments.js`.

See [api.md](api.md) for the full reference and [architecture.md](architecture.md) for the layout.

---

## 0. Prerequisites

- A Linux server (a $5 VPS is plenty).
- The main website you want comments on, reachable at some origin like `https://your-site.example`.
- A subdomain for zapiska, e.g. `comments.your-site.example`. Point its A/AAAA record at the same server.
- A reverse proxy with TLS (e.g. nginx + certbot, or Caddy with automatic TLS).
- One secret: a long random string for `ADMIN_TOKEN`.

Generate that now:

```sh
openssl rand -base64 32
```

---

## 1. Decide how you want to run it

Pick one of:

- **Docker** — fastest path, no Rust toolchain needed.
- **Pre-built binary** — easiest if a release tarball is available.
- **Build from source** — full control; picks up optional feature flags.

### Option A: Docker (recommended to start)

```sh
git clone https://github.com/nithitsuki/zapiska.git   # or your fork
cd zapiska
cp docker-compose-example.yml docker-compose.yml
cp .env.example .env
$EDITOR .env               # set ADMIN_TOKEN, PUBLIC_TARGET_ORIGIN, ALLOWED_CORS_ORIGIN
$EDITOR docker-compose.yml # same env vars go here too (or read from .env)
docker compose up -d --build
```

The container binds `0.0.0.0:3000` internally and writes SQLite to `/data`. The example compose file maps that port to **localhost:3000** on the host so you can sit a reverse proxy in front of it without exposing it to the open internet. The `zapiska-data` named volume keeps your DB across restarts.

### Option B: Pre-built binary

If a release tarball is published for your architecture, download and unzip it into `/opt/zapiska/`. Skip to [step 2](#2-point-it-at-your-site).

### Option C: Build from source

You need Rust 1.85+ (edition 2024) and `libsqlite3-dev` + `pkg-config`:

```sh
sudo apt-get install -y libsqlite3-dev pkg-config     # Debian/Ubuntu
# or: sudo dnf install -y sqlite-devel pkgconf-pkg-config   (Fedora)

git clone https://github.com/nithitsuki/zapiska.git
cd zapiska

cargo build --release
# binary ends up at target/release/zapiska
```

To build without webmention support (smaller binary, fewer deps):

```sh
cargo build --release --no-default-features --features comments
```

---

## 2. Point it at your site

Everything is configured through environment variables. The ones you must set for any real deployment:

| Variable | Example | Why |
|---|---|---|
| `ADMIN_TOKEN` | (output of `openssl rand -base64 32`) | Authenticates the admin API. **Required.** |
| `PUBLIC_TARGET_ORIGIN` | `https://your-site.example` | The site you accept comments/webmentions for. `target` URLs must start with this. |
| `ALLOWED_CORS_ORIGIN` | `https://your-site.example` | The single origin your browser will fetch the read API from. CORS is rejected for anything else. |
| `DATABASE_PATH` | `/opt/zapiska/comments.db` (or `/data/comments.db` in Docker) | Where the SQLite file lives. Back this path up. |
| `BIND_ADDR` | `127.0.0.1:3000` (default) | Listen address. Keep localhost-only if you're behind a reverse proxy. |

Recommended but optional:

| Variable | Why |
|---|---|
| `GITHUB_TOKEN` | Raises the GitHub API rate limit from 60/hr to 5000/hr when commenters supply a `github_username`. No scopes needed. |
| `STORE_IP_ADDRESS=true` | Lets your moderation logic do IP-based reputation. Off by default for privacy. |
| `DEFAULT_COMMENT_STATUS` | `pending` (safe, manual review) or `approved` (immediate, your engine reverts spam later). |

Full list: [deployment.md](deployment.md). The `.env.example` file is the canonical commented reference.

### Quick local sanity check

```sh
export ADMIN_TOKEN=$(openssl rand -base64 32)
export PUBLIC_TARGET_ORIGIN=https://your-site.example
export ALLOWED_CORS_ORIGIN=https://your-site.example
./target/release/zapiska   # or: docker compose up
```

In another shell:

```sh
curl http://127.0.0.1:3000/healthz
# -> ok

curl 'http://127.0.0.1:3000/api/comments?path=/'
# -> {"total":0,"comments":[]}

curl -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:3000/api/admin/pending
# -> {"comments":[]}
```

Interactive API docs are served at `/swagger-ui/` while the server runs.

---

## 3. Put it behind a reverse proxy

zapiska binds `127.0.0.1:3000` by default. It expects TLS to be terminated upstream. Below are minimal nginx and Caddy examples — pick whichever you use.

### nginx (with certbot)

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

> **Note on rate limiting.** zapiska rate-limits by TCP peer IP. Behind a reverse proxy, that peer is the proxy itself, not the real client. If you need per-client limits to work correctly across multiple real clients, run zapiska directly or front it with a layer that still passes distinct source IPs (e.g. a localhost-bound `proxy_pass` per real client, or terminate TLS in zapiska itself). For most personal-blog use cases the per-IP caps are a backstop and `tower_governor` on the proxy IP is fine.

### Caddy (automatic TLS)

```caddy
comments.your-site.example {
    reverse_proxy 127.0.0.1:3000
}
```

### systemd unit (for non-Docker deployments)

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
sudo journalctl -u zapiska -f   # watch it start
```

---

## 4. Add comments to your site

### Show approved comments

Drop this where you want the thread to appear:

```html
<div id="nc-comments"></div>
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"></script>
```

`data-path` must match the path on your main site (the stuff after `PUBLIC_TARGET_ORIGIN`). For `https://your-site.example/blog/hello-world` it's `/blog/hello-world`.

The widget fetches `GET /api/comments?path=...&limit=50` and renders approved comments. `content` is ammonia-cleaned server-side and still re-sanitised client-side; author fields are inserted as text, never `innerHTML`.

### Accept new comments

```html
<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text"   name="author_name" placeholder="Name" required>
  <input type="url"    name="author_url"   placeholder="Website (optional)">
  <textarea name="content" required></textarea>
  <!-- honeypot: bots fill this, humans never see it -->
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

The response is `201` with `{ "delete_token": "..." }`. Keep that token if you want self-service deletion (`POST /api/comment/{id}/delete`).

### Advertise webmention support (optional, but recommended)

In your main site's `<head>`:

```html
<link rel="webmention" href="https://comments.your-site.example/api/webmention" />
```

Only do this if you build with the `webmentions` feature (default). Disabling it strips the webmention endpoint, worker, SSRF guards, and the microformats parser.

### Add Cloudflare Turnstile (optional bot protection)

Turnstile is **off by default** — comment submissions go straight through. To require a human-solving widget:

1. Create a widget at https://dash.cloudflare.com → Turnstile. Note the **sitekey** (public) and **secret** (private). Add your main site's hostname (and `localhost`, `127.0.0.1` for dev) to the widget's allowed domains.
2. Set on zapiska:
   ```env
   TURNSTILE_ENABLED=true
   TURNSTILE_SECRET_KEY=0x4AAAAAAA...   # the secret, never the sitekey
   ```
   Restart zapiska. Startup will fail fast if `TURNSTILE_ENABLED=true` and the secret is missing.
3. Load the widget script in your page (or per-form):
   ```html
   <script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
   ```
4. Add the widget container inside your comment form. The widget renders the `cf-turnstile-response` hidden input automatically — do **not** add it yourself:
   ```html
   <form action="https://comments.your-site.example/api/comment" method="POST">
     <input type="hidden" name="target_path" value="/blog/hello-world">
     <input type="text" name="author_name" placeholder="Name" required>
     <textarea name="content" required></textarea>
     <div class="cf-turnstile" data-sitekey="0x4AAAAAAA..."></div>
     <input type="text" name="website" style="display:none">  <!-- honeypot -->
     <button type="submit">Send</button>
   </form>
   ```

When enabled, zapiska verifies the token server-side against Cloudflare's `siteverify` before storing the comment. Invalid/expired/replayed tokens return `400 { "code": "turnstile_failed" }` and the comment is not stored. If zapiska can't reach Cloudflare's siteverify endpoint, it returns `503` and **fails closed**. See [deployment.md](deployment.md#cloudflare-turnstile-optional-bot-protection) for the full env reference and [security.md](security.md#cloudflare-turnstile-optional) for the threat model.

---

## 5. Moderate

New comments land with `status = 'pending'` (unless `DEFAULT_COMMENT_STATUS=approved`). They are not served by `GET /api/comments` until you approve them.

Easiest path: log in to get a session cookie and call the admin API.

```sh
# Get a session cookie (HttpOnly, 30-day)
curl -c cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d "{\"token\":\"$ADMIN_TOKEN\"}" \
  https://comments.your-site.example/api/admin/login

# List pending comments
curl -b cookies.txt https://comments.your-site.example/api/admin/pending

# Approve / spam / delete
curl -b cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d '{"id":1,"action":"approved"}' \
  https://comments.your-site.example/api/admin/moderate
```

A minimal admin dashboard is included at `embed/admin.html` if you'd rather click buttons. Point it at your instance and log in with `ADMIN_TOKEN`.

### Automate moderation

Once you're happy with the workflow, hook an external moderation engine in. Either:

- Set `MODERATION_WEBHOOK_URL` and zapiska will `POST` the payload to your service on every submission (fire-and-forget by default, or `MODERATION_WEBHOOK_MODE=sync` to wait for an action), **or**
- Have your service poll `GET /api/admin/comments?status=pending` and batch-moderate via `POST /api/admin/moderate/batch`.

The full walkthrough (rules engine, LLM, Flask webhook server) is in [moderation-engine.md](moderation-engine.md).

---

## 6. Back up

The entire state is one SQLite file plus its WAL:

```sh
cp /data/comments.db  /backups/comments-$(date +%F).db
cp /data/comments.db-wal /backups/comments-$(date +%F).db-wal 2>/dev/null || true
```

For systemd deployments, swap `/data/` for your `DATABASE_PATH` directory. For Docker, `docker compose exec zapiska cat /data/comments.db > backup.db` works in a pinch, but a cold file copy from the host volume is safer.

---

## 7. Update

```sh
# From source
git pull
cargo build --release
sudo systemctl restart zapiska

# Docker
git pull
docker compose up -d --build
```

Schema migrations run automatically on startup — they're idempotent.

---

## 8. Troubleshooting

- **`failed to load configuration: ADMIN_TOKEN is required but not set`** — your `.env` isn't on the path or the var is empty. zapiska loads `.env` from the working directory automatically (via `dotenvy`); under systemd set `EnvironmentFile=/opt/zapiska/.env` on the unit.
- **Browser fetch to `/api/comments` is blocked by CORS.** — `ALLOWED_CORS_ORIGIN` does not match your main site's exact origin (including scheme). Fix the env var and restart.
- **Webmention returns `400 invalid target`.** — the `target` URL you received doesn't start with `PUBLIC_TARGET_ORIGIN`. Either fix the sender or correct the env var.
- **`503 Service Unavailable` from `POST /api/webmention`.** — the worker backlog is full (`WORKER_BACKLOG`, default 64). Raise it or investigate a fetch storm.
- **Comments are accepted but my site shows nothing.** — they're still `pending`. Approve them via the admin API, or set `DEFAULT_COMMENT_STATUS=approved` if you prefer immediate posts.
- **Rate limits don't seem per-client.** — you're probably hitting zapiska through a single reverse proxy. See the note in [step 3](#3-put-it-behind-a-reverse-proxy).

---

## Next steps

- [api.md](api.md) — full HTTP reference.
- [deployment.md](deployment.md) — feature flags, logging, systemd/Docker details, backups.
- [moderation-engine.md](moderation-engine.md) — building your own spam pipeline.
- [security.md](security.md) — the threat model and how each control is implemented.