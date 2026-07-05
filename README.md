<p align="center">
  <img src="assets/logo_transparent.png" alt="zapiska" width="100%">
</p>

<p align="center">
  <strong>A comment and webmention engine.</strong>
  <br>
  Self-hosted. One binary. Your data.
</p>

---

zapiska is a backend for blog comments and webmentions. You style the frontend. You choose the moderation. We handle the plumbing.

## features

**Style it all you want** — zapiska is backend only. Use the included JS widget, build your own frontend, or pipe comments into a static site. The API gives you data; how it looks is up to you.

**Bring your own moderation** — hook in an LLM, a rules engine, a community blocklist, or the built-in dashboard. zapiska doesn't decide what spam means — that's your call. Community moderation tools plug in via the admin API.

**Threaded replies** — nested conversations up to 4 levels deep. Sorted oldest-first within each thread. Flat mode available (set depth to 0).

**Webmention support** — accepts W3C webmentions from other sites. Auto-fetches source pages, parses author profiles, pulls in avatars.

**Built-in spam protection** — rate limiting, per-IP daily caps, per-domain caps, honeypot fields, content hash dedup, URL cross-referencing. All configurable, none of it decides what's spam for you.

**Profile pictures built-in** — GitHub avatars, h-card photos, favicon extraction, DiceBear fallback. No external service required.

**Admin API** — every moderation action is an API call. Query by status, path, IP, content hash, or URL. Fetch parent chains and author stats. Moderate singly or in batch. Your tool talks to zapiska, not the other way around.

**One binary, zero dependencies** — Rust + SQLite. No Postgres, no Redis, no JS runtime. ~10MB. Runs on a $5 VPS or a Raspberry Pi.

## quick start

```sh
echo 'ADMIN_TOKEN="your-secret"' > .env
cargo run --release
```

Server starts at `http://127.0.0.1:3000`. Open `/admin` to log in.

## embed

```html
<div id="nc-comments"></div>
<script
  id="nc-comments"
  src="https://your-server.com/embed/comments.js"
  data-path="/blog/post-1"></script>
```

## links

[Admin API reference](docs/api.md) · [Deployment guide](docs/deployment.md) · [Building a moderation engine](docs/moderation-engine.md) · [Configuration](docs/deployment.md)
