# Skill: deploy the zapiska server

Use this skill when a user wants to deploy zapiska.

The skill covers Docker, a release binary, and a source build.

## Ask before you start

Confirm these values:

- Main site origin.
- zapiska origin.
- Deployment method.
- Reverse proxy and TLS plan.

Do not ask for a secret in chat. Tell the user where to set it.

## Prepare the host

The host needs:

- Linux or a compatible container host.
- A DNS record for the zapiska origin.
- A TLS reverse proxy for a public deployment.
- Rust 1.85 or later for a source build, or Docker.

Create an admin token:

```sh
openssl rand -base64 32
```

## Docker path

Run these commands in the repository:

```sh
cp .env.example .env
$EDITOR .env
docker compose up -d --build
curl http://127.0.0.1:3000/healthz
```

Set at least:

```env
ADMIN_TOKEN=replace-this-value
PUBLIC_TARGET_ORIGIN=https://your-site.example
ALLOWED_CORS_ORIGIN=https://your-site.example
```

The compose file sets the container listen address to `0.0.0.0:3000`.
It stores the database in the `zapiska-data` volume.

## Source path

Build the default binary:

```sh
cargo build --release
```

Build without webmention support:

```sh
cargo build --release --no-default-features --features comments
```

Copy the binary to `/opt/zapiska/`.
Create `/etc/zapiska/zapiska.env` from `.env.example`.

The bundled SQLite feature means that system SQLite headers are not required.

## Release binary path

Download the archive for the target platform from the GitHub release.
Copy the binary to `/opt/zapiska/`.
Create `/etc/zapiska/zapiska.env`.

## Reverse proxy

The bare-metal default is `127.0.0.1:3000`.
Terminate TLS in the proxy.

```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
    proxy_set_header X-Real-IP $remote_addr;
}
```

The rate limiter keys on one normalized client identity per request. By
default it uses the TCP peer address, so a proxy makes many visitors share
one quota. Set `TRUST_PROXY=true` only when every byte arrives via a proxy
you control (Cloudflare proxy/Tunnel, or a firewall allow-listing
Cloudflare IPs): the server then prefers the edge-set `CF-Connecting-IP`,
falling back to leftmost `X-Forwarded-For`, `X-Real-IP`, `Forwarded for=`,
then the peer. See the reverse-proxy section in `docs/deployment.md`.

## systemd

Use `deploy/zapiska.service`.
It expects:

- Binary at `/opt/zapiska/zapiska`.
- Environment at `/etc/zapiska/zapiska.env`.

```sh
sudo useradd -r -d /opt/zapiska -s /usr/sbin/nologin zapiska
sudo install -d -o zapiska -g zapiska /opt/zapiska /etc/zapiska
sudo cp target/release/zapiska /opt/zapiska/
sudo cp .env.example /etc/zapiska/zapiska.env
sudo cp deploy/zapiska.service /etc/systemd/system/zapiska.service
sudo systemctl daemon-reload
sudo systemctl enable --now zapiska
```

## OpenRC

Use `deploy/zapiska.openrc`.

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

## Validate

```sh
curl https://comments.your-site.example/healthz
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://comments.your-site.example/api/admin/pending
curl 'https://comments.your-site.example/api/comments?path=/'
```

Open `/swagger-ui/` to view the interactive API page.

## Troubleshoot

- Startup fails when `ADMIN_TOKEN` is empty or missing.
- Browser CORS needs an exact origin in `ALLOWED_CORS_ORIGIN`.
- A reply needs `MAX_THREAD_DEPTH` above `0`.
- Database writes need a writable `DATABASE_PATH` directory.
- A public deployment needs a valid TLS certificate.

See `docs/getting-started.md` and `docs/deployment.md` for more detail.
