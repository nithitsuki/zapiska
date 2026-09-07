<p align="center">
  <img src="assets/logo_transparent.png" alt="zapiska" width="100%">
</p>

<p align="center">
  <strong>A self-hosted comment and webmention engine.</strong>
  <br>
  One Rust binary. One SQLite file. Your data.
  <br>
  <a href="docs/getting-started.md">Getting started</a> |
  <a href="docs/api.md">API</a> |
  <a href="embed/README.md">Embed</a> |
  <a href="https://discord.gg/Q9fjx3gynN">Discord</a> |
  <a href="https://github.com/sponsors/nithitsuki">Sponsor</a>
</p>

---

# Features

## Backend only

Use the supplied widget or build your own frontend. The public API returns JSON.

## Bring your own moderation

Use the admin API, a rules engine, an LLM, or another moderation service.
Comments and reactions stay pending until a moderator approves them.

## Threaded replies

Replies use `parent_id` and `depth`. Set `MAX_THREAD_DEPTH` above `0` to enable
threading. The server clamps the value to `0` through `10`.

## Reactions

The default reaction set is `👍,❤️,😄,😮,😢,😡`. Reactions use the same pending,
approved, spam, and deleted states as comments. Only approved counts appear in
the public read API.

## Verified owner

Set a public owner profile (name, GitHub, website, avatar) in the dashboard
PROFILE tab or `PUT /api/admin/profile`, then post as yourself: the comment
appears immediately, approved, with a ✓ checkmark visitors can trust. The
checkmark — never the bare name — is the trust signal: anyone can submit any
author name, but no public path can set the flag.

## RSS feeds

Use `/feed.xml` for approved comments across the site. Add `path` to select one
page. Feed items use RSS 2.0 and RFC 822 dates.

## Language filtering

Use ISO 639-1 allow or block lists for native comments. The gate is off by
default. Configure a separate policy for emoji-heavy or unknown text.

## Notifications

Send new comment and webmention alerts to Telegram, Slack, or Discord. The
in-memory batcher groups alerts by page or across the site. Set the window to
`0` for immediate delivery.

## Export and import

Export and restore all five SQLite tables with JSON. The export includes
comments, webmention state, extracted URLs, GitHub profiles, and reactions.
Imports preserve IDs and statuses, re-sanitize comment content, and skip bad
rows without stopping the whole import.

## Webmention support

The default `webmentions` feature receives W3C webmentions, verifies backlinks,
fetches source pages, parses h-entry and h-card data, and stores mentions.

Build without webmentions when you need a comments-only binary:

```sh
cargo build --release --no-default-features --features comments
```

## Protection

The server provides body limits, per-route rate limits, daily and domain caps,
honeypot flags, content hashes for moderation lookup, HTML sanitization, URL
tracking, and optional Cloudflare Turnstile verification.

`content_hash` helps a moderation service find repeated content. It does not
reject or merge duplicate comments.

## Small deployment

The binary uses Rust and SQLite. SQLite is compiled with the bundled feature.
No PostgreSQL, Redis, Node.js, or JavaScript runtime is required on the server.

See [Getting started](docs/getting-started.md), [Deployment](docs/deployment.md),
[API](docs/api.md), [Security](docs/security.md), and [Architecture](docs/architecture.md).
