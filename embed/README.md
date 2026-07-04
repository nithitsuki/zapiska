# Comments embed

## Widget

Add a `<div id="nc-comments"></div>` where you want comments to appear, then drop in the script:

```html
<script
  id="nc-comments"
  src="https://webmention.nithitsuki.com/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

- `data-path` — the path on nithitsuki.com this page maps to (required).
- `data-limit` — max comments to show (optional, defaults to 50).

The script hits `GET /api/comments?path=...&limit=...` and renders approved comments. Content gets sanitised server-side (ammonia) and client-side. Author names and URLs are text-escaped, never innerHTML.

## Form

To post a comment:

```
POST https://webmention.nithitsuki.com/api/comment
Content-Type: application/x-www-form-urlencoded
```

Fields: `target_path`, `author_name`, `content`, and optionally `author_url` and `github_username`.

## Webmention discovery

Stick this in `<head>` to point to the Webmention endpoint:

```html
<link rel="webmention" href="https://webmention.nithitsuki.com/api/webmention" />
```
