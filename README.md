# zapiska

A self-hosted comment and webmention server in Rust (Axum + SQLite).

Handles native comment form submissions, incoming [W3C webmentions](https://www.w3.org/TR/webmention/), and serves approved comments via a JSON API + embeddable JS widget. All entries go through a pending moderation queue before they show up publicly.

## Quick start

```sh
export ADMIN_TOKEN="your-secret-admin-token"
cargo run --release
```

Server starts on `127.0.0.1:3000`. Point a reverse proxy at it or use a Cloudflare Tunnel.

Open `http://127.0.0.1:3000/swagger-ui/` for interactive API docs.

## Configuration

Everything goes through environment variables:

| Variable | Default | Description |
|---|---|---|
| `ADMIN_TOKEN` | **required** | Bearer token for admin moderation endpoints. |
| `BIND_ADDR` | `127.0.0.1:3000` | Listen address. |
| `PUBLIC_TARGET_ORIGIN` | `https://nithitsuki.com` | Main site origin. Webmention `target` must match this. |
| `ALLOWED_CORS_ORIGIN` | `https://nithitsuki.com` | Single origin allowed to read the public API cross-origin. |
| `DATABASE_PATH` | `./comments.db` | SQLite file path. |
| `GITHUB_TOKEN` | *(optional)* | GitHub PAT for higher API rate limits (5000/hr vs 60/hr). |
| `MAX_CONTENT_LEN` | `2000` | Max comment content length in chars. |
| `MAX_AUTHOR_LEN` | `100` | Max author name length in chars. |
| `MAX_BODY_SIZE` | `8192` | Max HTTP request body size in bytes. |
| `FETCH_TIMEOUT_MS` | `4000` | Outbound HTTP fetch timeout (webmentions, GitHub API). |
| `WORKER_BACKLOG` | `64` | Webmention worker channel capacity. Overflow returns 503. |
| `RUST_LOG` | `info` | Tracing log filter directive. |

## Embed

Drop this on any page to show comments:

```html
<div id="nc-comments"></div>
<script
  id="nc-comments"
  src="https://webmention.nithitsuki.com/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

For the comment form, POST to `https://webmention.nithitsuki.com/api/comment` with `application/x-www-form-urlencoded` fields: `target_path`, `author_name`, `content`, and optionally `author_url` and `github_username`.

To point to the webmention endpoint, add to your `<head>`:

```html
<link rel="webmention" href="https://webmention.nithitsuki.com/api/webmention" />
```

## Local development

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo run
```

See [docs/development.md](docs/development.md) for the full setup.

## More

- [Architecture](docs/architecture.md)
- [API reference](docs/api.md)
- [Security model](docs/security.md)
- [Deployment](docs/deployment.md)
- [Development](docs/development.md)
- [Specification](SPEC.md)

## License

MIT