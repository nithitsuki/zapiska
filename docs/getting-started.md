# Getting started

This guide deploys zapiska behind a TLS reverse proxy.

The guide assumes that you can use a shell and manage a server.

By the end, zapiska listens at a URL such as
`https://comments.your-site.example` and serves a site such as
`https://your-site.example`.

## What zapiska provides

zapiska is one Rust process with one SQLite file.

The server provides:

- A public approved comment API at `GET /api/comments`.
- A native comment API at `POST /api/comment`.
- A reaction API for approved comments.
- An RSS feed at `GET /feed.xml`.
- A webmention API when the `webmentions` feature is enabled.
- Protected admin routes for moderation, lookup, export, and import.
- A widget at `/embed/comments.js`.

See [API](api.md) for the full route list.

## Prerequisites

You need:

- A Linux server or a local machine.
- A domain or subdomain for zapiska.
- A TLS reverse proxy for a public deployment.
- A random admin token.

Create an admin token:

```sh
openssl rand -base64 32
```

Keep this value private.

## Choose a run method

Choose one method:

- Docker Compose.
- A release binary.
- A source build.

### Docker Compose

Clone the repository and start the service:

```sh
git clone https://github.com/nithitsuki/zapiska.git
cd zapiska
cp .env.example .env
$EDITOR .env
docker compose up -d --build
```

Set these values in `.env`:

```env
ADMIN_TOKEN=replace-this-value
PUBLIC_TARGET_ORIGIN=https://your-site.example
ALLOWED_CORS_ORIGIN=https://your-site.example
```

The compose file sets `BIND_ADDR=0.0.0.0:3000` inside the container. It maps
host `127.0.0.1:3000` to the container.

The database is stored in the `zapiska-data` volume.

Check the service:

```sh
docker compose ps
curl http://127.0.0.1:3000/healthz
```

### Release binary

Download the archive for your target from the GitHub release. Put the binary in
`/opt/zapiska/` and continue with [Configure zapiska](#configure-zapiska).

### Source build

Rust 1.85 or later is required.
SQLite development headers are not required because the build uses bundled
SQLite.

```sh
git clone https://github.com/nithitsuki/zapiska.git
cd zapiska
cargo build --release
```

The binary is `target/release/zapiska`.

Build without webmention support:

```sh
cargo build --release --no-default-features --features comments
```

## Configure zapiska

The server reads environment variables at startup.

The required value is:

```env
ADMIN_TOKEN=replace-this-value
```

Set the site values:

```env
PUBLIC_TARGET_ORIGIN=https://your-site.example
ALLOWED_CORS_ORIGIN=https://your-site.example
DATABASE_PATH=/opt/zapiska/comments.db
BIND_ADDR=127.0.0.1:3000
```

`PUBLIC_TARGET_ORIGIN` is compared by parsed origin. It is not a string prefix.

`ALLOWED_CORS_ORIGIN` can contain one origin, a comma-separated list, or `*`.
Use `*` only for local testing.

The full variable list is in [Deployment](deployment.md) and `.env.example`.

## Check the server

Run the binary:

```sh
./target/release/zapiska
```

Check health and the public API:

```sh
curl http://127.0.0.1:3000/healthz
curl 'http://127.0.0.1:3000/api/comments?path=/'
```

The first response is `ok`. The second response is an empty comments object.

Open the interactive API page at:

```text
http://127.0.0.1:3000/swagger-ui/
```

## Use a reverse proxy

The bare-metal server listens on `127.0.0.1:3000`.
Terminate TLS in nginx or Caddy.

Example nginx location:

```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
    proxy_set_header X-Real-IP $remote_addr;
}
```

See [Deployment](deployment.md) for a complete proxy and service example.

## Add the widget

Put this element where the comment thread must appear:

```html
<div id="nc-comments"></div>
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

The value of `data-path` must match the path after the main site origin.

The widget reads approved comments and builds the reply tree in the browser.
It renders sanitized `content` as HTML. It renders author values as text.

The server default is `MAX_THREAD_DEPTH=0`. Set a value above `0` before you
use reply forms.

See [Embed](../embed/README.md) for all widget attributes.

## Add a native comment form

Create the top-level form on your site:

```html
<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text" name="author_name" required>
  <input type="url" name="author_url">
  <textarea name="content" required></textarea>
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

The hidden `website` field is the current honeypot field. A filled honeypot is
stored with a flag. The server does not discard it.

The response contains a delete token and the final initial status. It does not
contain the new comment ID.

## Add webmention discovery

When webmentions are enabled, add this link to the main site head:

```html
<link rel="webmention" href="https://comments.your-site.example/api/webmention">
```

The worker verifies the source backlink before it stores a mention.

## Enable Turnstile

Turnstile is off by default.

1. Create a Turnstile widget.
2. Add the main site host to the widget.
3. Set `TURNSTILE_ENABLED=true`.
4. Set `TURNSTILE_SECRET_KEY` on the server.
5. Put the public sitekey in the form.

```html
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
<div class="cf-turnstile" data-sitekey="public-sitekey"></div>
```

The widget creates `cf-turnstile-response`. Do not create that hidden field by
hand.

The server checks the token. It returns `400` for a failed token and `503` when
Cloudflare cannot be reached. It does not store the comment in either case.

## Moderate comments

New native comments use `pending` status by default.

Authenticate:

```sh
curl -c cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d "{\"token\":\"$ADMIN_TOKEN\"}" \
  http://127.0.0.1:3000/api/admin/login
```

List pending comments:

```sh
curl -b cookies.txt http://127.0.0.1:3000/api/admin/pending
```

Approve one comment:

```sh
curl -b cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d '{"id":42,"action":"approved"}' \
  http://127.0.0.1:3000/api/admin/moderate
```

The valid actions are `approved`, `spam`, `deleted`, and `pending`.

## Configure reactions

The default mode requires the admin token:

```env
REACTIONS_ALLOWED=admin
REACTIONS_SET=👍,❤️,😄,😮,😢,😡
```

Set `REACTIONS_ALLOWED=anyone` to identify callers by a hash of the peer IP.
This mode has limited abuse protection. Use it only when the risk is known.

Moderate reactions with:

```sh
curl -b cookies.txt http://127.0.0.1:3000/api/admin/reactions?status=pending
```

## Configure language filtering

The language gate applies only to native comments and is off by default.

Allow English and German:

```env
COMMENT_LANG_ALLOWED=en,de
COMMENT_LANG_ALLOW_EMOJI=always
```

A whitelist takes precedence over `COMMENT_LANG_BLOCKED`.
Unknown and emoji-heavy text uses `COMMENT_LANG_ALLOW_EMOJI`.

The detector uses `whatlang`. It checks sanitized, tag-free content.

## Configure notifications

Set Telegram, Slack, or Discord values in the environment.

```env
TELEGRAM_BOT_TOKEN=replace-this-value
TELEGRAM_CHAT_ID=@alerts
SLACK_WEBHOOK_URL=https://hooks.slack.com/services/example
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/example
```

The default batcher groups events for 60 seconds by page.

```env
NOTIFY_BATCH_SECS=60
NOTIFY_BATCH_THRESHOLD=20
NOTIFY_BATCH_GRANULARITY=page
```

Set `NOTIFY_BATCH_SECS=0` for immediate delivery.
Delivery failure does not fail the comment request.

## Export data

Create a JSON export with admin authentication:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://127.0.0.1:3000/api/admin/export > backup.json
```

The export contains comments, webmention state, extracted URLs, GitHub
profiles, and reactions.

Restore it with:

```sh
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  --data @backup.json \
  http://127.0.0.1:3000/api/admin/import
```

The import limit is 16 MiB. It re-sanitizes comment content and skips failed
comment and URL rows.

## Back up the SQLite file

Stop the service before a file copy:

```sh
sudo systemctl stop zapiska
cp /opt/zapiska/comments.db /backups/comments.db
cp /opt/zapiska/comments.db-wal /backups/comments.db-wal 2>/dev/null || true
sudo systemctl start zapiska
```

The JSON export is safer for a live backup.

## Update zapiska

From source:

```sh
cargo build --release
sudo systemctl restart zapiska
```

With Docker:

```sh
```

Database schema creation runs at startup and is idempotent.

## Troubleshooting

### The server stops at startup

Check `ADMIN_TOKEN`. Check the environment file path.

### Browser requests fail with CORS

Set `ALLOWED_CORS_ORIGIN` to the exact scheme, host, and port of the main site.
Restart zapiska after the change.

### Replies return an error

Set `MAX_THREAD_DEPTH` above `0`. The parent comment must be approved and use
the same path.

### A comment is stored but does not appear

Check its status. The public API returns approved comments only.

### Rate limits use one shared client address

The server sees the reverse proxy address. Review the proxy and network path
when several visitors share one address.
