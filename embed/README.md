# Comments embed

This is the client-side widget and form reference for embedding zapiska on your site.
Replace `comments.your-site.example` with your zapiska domain.

- [Widget](#widget) — drop-in threaded comment thread
- [Form](#form) — submit new comments
- [Webmention](#webmention-discovery) — receive IndieWeb pings
- [Turnstile](#cloudflare-turnstile-optional) — optional bot protection
- [Building a custom frontend](#building-a-custom-frontend) — use the raw API

---

## Widget

Add a `<div id="nc-comments"></div>` where comments should appear, then drop in the script:

```html
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

- `data-path` — the path on your main site this page maps to (required).
- `data-limit` — max comments to show (optional, defaults to 50).

The script fetches `GET /api/comments?path=...` and renders approved comments.
Content is sanitized server-side (ammonia) and re-sanitized client-side.
Author names and URLs are text-escaped, never `innerHTML`.

> `ALLOWED_CORS_ORIGIN` must be set to your main site's origin for the fetch to succeed.

### All data-* attributes

Every aspect of the widget is configurable through attributes on the `<script>` tag.
All are optional — the widget uses sensible defaults for everything.

#### API & data

| Attribute | Default | Description |
|---|---|---|
| `data-api-origin` | derived from `src` or `window.location.origin` | Override the API origin (if the JS is served from a different domain than the API). |
| `data-path` | `/` | The page path to fetch comments for. |
| `data-limit` | `50` | Max comments to fetch (passed to the API). |

#### Text overrides

Every text string can be replaced. Use `%d` in `data-heading-text` for the comment count.

| Attribute | Default | Description |
|---|---|---|
| `data-heading-text` | `Comments (%d)` | Section heading. `%d` is replaced with the comment count. |
| `data-empty-text` | `No comments yet.` | Shown when there are no approved comments. |
| `data-error-text` | `Comments could not be loaded.` | Shown on API fetch failure. |
| `data-reply-text` | `Reply` | Reply button label on each comment. |
| `data-submit-text` | `Submit` | Reply form submit button. |
| `data-cancel-text` | `Cancel` | Reply form cancel button. |
| `data-name-placeholder` | `Your name` | Reply form name input placeholder. |
| `data-website-placeholder` | `Website (optional)` | Reply form website input placeholder. |
| `data-reply-placeholder` | `Write your reply...` | Reply form textarea placeholder. |
| `data-pending-text` | `Reply submitted (pending approval).` | Shown after a reply is submitted successfully. |

#### Behavior

| Attribute | Default | Description |
|---|---|---|
| `data-hide-replies` | `false` | Set `"true"` to hide reply buttons on all comments (read-only thread). |
| `data-hide-heading` | `false` | Set `"true"` to suppress the "Comments (x)" heading. |
| `data-nostyles` | `false` | Set `"true"` to skip injecting default CSS entirely — you provide your own. Class names used: `.nc-comment`, `.nc-meta`, `.nc-avatar`, `.nc-author`, `.nc-date`, `.nc-body`, `.nc-reply-btn`, `.nc-reply-form`, `.nc-thread`, `.nc-heading`, `.nc-empty`, `.nc-error`. |
| `data-link-target` | `_blank` | `target` attribute for author name links. Set to `_self` to open in the same tab. |
| `data-avatar-size` | `24` | Avatar width/height in pixels. |
| `data-turnstile-sitekey` | _(unset)_ | When set, the reply form renders a Cloudflare Turnstile widget with this sitekey. The token is sent as `cf-turnstile-response`. Requires `TURNSTILE_ENABLED=true` on the server. |

### Style with your own CSS

The widget injects low-specificity class selectors that your own stylesheet
overrides naturally. Set `data-nostyles="true"` and write everything yourself.
The available class names are `.nc-comment`, `.nc-meta`, `.nc-avatar`,
`.nc-author`, `.nc-date`, `.nc-body`, `.nc-reply-btn`, `.nc-reply-form`,
`.nc-thread`, `.nc-heading`, `.nc-empty`, `.nc-error`.

### Fully custom heading

```html
<script id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/post-1"
  data-heading-text="%d thoughts"
  data-hide-replies="true"
></script>
```

---

## Form

To submit a top-level comment, POST to the API directly. The widget only renders
replies — you must build the comment form yourself.

```
POST https://comments.your-site.example/api/comment
Content-Type: application/x-www-form-urlencoded
```

Fields: `target_path`, `author_name`, `content`, and optionally `author_url`,
`github_username`, `parent_id` (for threaded replies), `website` (honeypot),
and `cf-turnstile-response` (when `TURNSTILE_ENABLED=true`).

A minimal HTML form:

```html
<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text"     name="author_name" placeholder="Name" required>
  <input type="url"      name="author_url"   placeholder="Website (optional)">
  <textarea name="content" required></textarea>
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

---

## Webmention discovery

If you build with the `webmentions` feature (default), advertise the endpoint
in `<head>`:

```html
<link rel="webmention" href="https://comments.your-site.example/api/webmention" />
```

---

## Cloudflare Turnstile (optional)

When `TURNSTILE_ENABLED=true` on the server, forms must include a Turnstile
widget that sends `cf-turnstile-response`. For your own top-level form:

```html
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text"  name="author_name" placeholder="Name" required>
  <input type="url"   name="author_url"  placeholder="Website (optional)">
  <textarea name="content" required></textarea>
  <div class="cf-turnstile" data-sitekey="0x4AAAAAAA-your-public-sitekey"></div>
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

For the widget's inline reply form, set `data-turnstile-sitekey` on the script
tag — the widget loads the Turnstile API, renders the widget, and includes
the token in the POST.

---

## Building a custom frontend

zapiska is backend-only. The widget is a convenience — you are not required to
use it. All data comes from two endpoints:

### Fetch comments

```js
fetch('https://comments.your-site.example/api/comments?path=/blog/post-1&limit=50')
  .then(function (r) { return r.json(); })
  .then(function (data) {
    console.log(data.total);       // total approved count
    console.log(data.comments);   // array of comment objects
  });
```

Each comment:

```json
{
  "id": 42,
  "comment_type": "native",
  "author_name": "Alice",
  "author_url": "https://alice.blog",
  "author_avatar": "https://alice.blog/avatar.jpg",
  "content": "<p>Great post!</p>",
  "created_at": "2026-07-03T16:40:00",
  "parent_id": null,
  "depth": 0
}
```

- `content` is already ammonia-sanitized HTML. Insert via `innerHTML`.
- `author_name`, `author_url`, `author_avatar` are **plain text** — escape or
  use `.textContent` / attribute values. Never put them through `innerHTML`.
- `parent_id` is `null` for top-level. Replies link via `parent_id` — build
  a tree client-side. Top-level sorted newest-first, replies oldest-first.

### Submit a comment

```js
var body = 'target_path=' + encodeURIComponent('/blog/post-1') +
           '&author_name=' + encodeURIComponent('Alice') +
           '&content=' + encodeURIComponent('<p>Nice!</p>');

fetch('https://comments.your-site.example/api/comment', {
  method: 'POST',
  headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
  body: body
})
.then(function (r) {
  if (!r.ok) throw new Error('HTTP ' + r.status);
  return r.json();
})
.then(function (data) {
  console.log(data.delete_token);  // for self-service deletion
});
```

For Turnstile, add `&cf-turnstile-response=<token>` to the body. Read the token
from `turnstile.getResponse()` (see the widget source for a complete example).

### Full API reference

See [docs/api.md](../docs/api.md) for all endpoints, admin API, moderation
webhooks, and error responses.