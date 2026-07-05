# zapiska

A comment and webmention engine. Self-hosted, one binary, SQLite-backed.

Submit comments via a form, accept webmentions from other sites, serve them all through a JSON API. Every entry passes through a pending queue. What happens next is up to your moderation system.

## Quick start

```sh
echo 'ADMIN_TOKEN="your-secret"' > .env
cargo run --release
```

Server starts at `http://127.0.0.1:3000`. Put it behind nginx or Caddy for TLS.

## Features

- **Comment submission** — form-based, with threaded replies (4 levels)
- **Webmention support** — W3C standard, auto-fetches and parses source pages
- **JSON API** — embeddable JS widget included, or build your own frontend
- **Admin API** — moderate, batch, filter by IP/status/path, get parent chains
- **Rate limiting** — per-IP, per-domain, daily caps. Configurable limits
- **Honeypot** — marks suspected spam submissions for your moderation tool
- **SSRF protection** — DNS blocklist, redirect policy on outbound fetches
- **GitHub enrichment** — resolves author names and avatars via the GitHub API
- **Optional IP storage** — enable `STORE_IP_ADDRESS` for spam analysis

## Quick embed

```html
<div id="nc-comments"></div>
<script id="nc-comments"
  src="https://your-server.com/embed/comments.js"
  data-path="/blog/post-1"></script>
```

The widget renders threaded replies with inline reply forms. See [embed docs](embed/README.md).

## Webmention endpoint

```html
<link rel="webmention" href="https://your-server.com/api/webmention" />
```

## API

The admin API is how external moderation tools control zapiska. Fetch pending comments, get parent context, filter by IP, look up author identity across signals, check duplicate content by hash, cross-reference URLs, batch-moderate, bulk fetch context. Everything a stateless moderation engine needs.

See [docs/api.md](docs/api.md) for the full reference. OpenAPI UI at `/swagger-ui/`.

## Configuration

Key environment variables:

| Variable | Default | What it does |
|---|---|---|
| `ADMIN_TOKEN` | required | Auth token for the admin API |
| `DATABASE_PATH` | `./comments.db` | Where data lives |
| `ALLOWED_CORS_ORIGIN` | `https://nithitsuki.com` | Your blog's origin |
| `DEFAULT_COMMENT_STATUS` | `pending` | `pending` = manual review, `approved` = auto-publish |
| `MAX_THREAD_DEPTH` | `0` | Nesting depth for replies. 0 = disabled. |
| `MAX_COMMENTS_PER_IP_PER_DAY` | `50` | Per-IP daily comment cap |
| `MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR` | `10` | Per-domain hourly webmention cap |
| `STORE_IP_ADDRESS` | `false` | Store submitter IPs for spam analysis |
| `MODERATION_WEBHOOK_URL` | (unset) | URL for external moderation engine |
| `HONEYPOT_FIELD` | `website` | Anti-spam honeypot field name |

All config vars are in [docs/deployment.md](docs/deployment.md).

## Deployment

```sh
docker build -t zapiska .
docker run -e ADMIN_TOKEN=your-secret -v data:/data zapiska
```

See [docs/deployment.md](docs/deployment.md) for systemd, nginx, Docker, and feature flags.

## Development

```sh
cargo test
```

See [docs/development.md](docs/development.md).

## License

MIT
