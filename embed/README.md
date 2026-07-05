# Comments embed

This is the client-side script and form reference for embedding zapiska on your site.
Replace `comments.your-site.example` with the domain you host zapiska on.

## Widget

Add a `<div id="nc-comments"></div>` where you want comments to appear, then drop in the script:

```html
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

- `data-path` — the path on your main site this page maps to (required). Must match what `PUBLIC_TARGET_ORIGIN` resolves to.
- `data-limit` — max comments to show (optional, defaults to 50).

The script hits `GET /api/comments?path=...&limit=...` and renders approved comments. Content gets sanitised server-side (ammonia) and client-side. Author names and URLs are text-escaped, never `innerHTML`.

> `ALLOWED_CORS_ORIGIN` must be set to your main site's origin (e.g. `https://your-site.example`) for the browser fetch to succeed.

## Form

To post a comment:

```
POST https://comments.your-site.example/api/comment
Content-Type: application/x-www-form-urlencoded
```

Fields: `target_path`, `author_name`, `content`, and optionally `author_url`, `github_username`, `parent_id` (for threaded replies), and `website` (honeypot — leave empty). See [docs/api.md](../docs/api.md) for the full reference.

A minimal HTML form:

```html
<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text"     name="author_name" placeholder="Name" required>
  <input type="url"      name="author_url"   placeholder="Website (optional)">
  <textarea name="content" required></textarea>
  <!-- honeypot: humans won't see this, bots will fill it -->
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

## Webmention discovery

If you build with the `webmentions` feature (default), advertise the endpoint in `<head>`:

```html
<link rel="webmention" href="https://comments.your-site.example/api/webmention" />
```

Other IndieWeb sites will then send pings to your zapiska instance whenever they link to a page on your main site.

## Cloudflare Turnstile (optional)

When `TURNSTILE_ENABLED=true` is set on the server, the form must include a Turnstile widget that sends a `cf-turnstile-response` token. The widget renders that hidden input for you — do **not** add it manually.

```html
<!-- in <head> or just before the form -->
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>

<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text" name="author_name" placeholder="Name" required>
  <input type="url"  name="author_url"   placeholder="Website (optional)">
  <textarea name="content" required></textarea>
  <div class="cf-turnstile" data-sitekey="0x4AAAAAAA-your-public-sitekey"></div>
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

Without a valid token, `POST /api/comment` returns `400 { "code": "turnstile_failed" }` and the comment is **not stored**. If Cloudflare's siteverify is unreachable, zapiska returns `503` and fails closed.

See [deployment.md](../docs/deployment.md#cloudflare-turnstile-optional-bot-protection) for the env vars and [getting-started.md](../docs/getting-started.md) for the end-to-end walkthrough.