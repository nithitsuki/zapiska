# Embed comments

This guide shows how to add the zapiska widget and how to use the API from a
custom frontend.

Replace `comments.your-site.example` with the zapiska host.

## Admin dashboard

Open `https://comments.your-site.example/admin` and sign in with
`ADMIN_TOKEN`. The dashboard covers comments (filter, moderate single and
batch with per-item results, ancestor chains, extracted URLs, duplicate
lookup by content hash), reactions (moderate with the reviewed emoji
pinned), author/URL lookup, JSON export download and import restore (with
`force` for live databases), and an OPS tab (versions, database health,
honeypot field, webhook/notify/proxy/reactions/limits status from
`GET /api/admin/status`). The session cookie is `__Host-admin_token`
(`Secure`, so the dashboard needs HTTPS).

## Widget

Add a container and the script:

```html
<div id="nc-comments"></div>
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

The server must include the main site origin in `ALLOWED_CORS_ORIGIN`.

The widget reads approved comments from `/api/comments`.
It builds the reply tree in the browser.
It renders approved reaction counts when they exist.

The default server value `MAX_THREAD_DEPTH=0` disables replies.
Set it above `0` before you use the reply form.

## Widget attributes

All attributes are optional.

### API values

| Attribute | Default | Description |
|---|---|---|
| `data-api-origin` | Derived from `src` | API origin when it differs from the script origin. |
| `data-path` | `/` | Main site path. |
| `data-limit` | `50` | Maximum comments to read. |

### Text values

| Attribute | Default | Description |
|---|---|---|
| `data-heading-text` | `Comments (%d)` | `%d` becomes the approved count. |
| `data-empty-text` | `No comments yet.` | Text for an empty thread. |
| `data-error-text` | `Comments could not be loaded.` | Text for a read error. |
| `data-reply-text` | `Reply` | Reply button text. |
| `data-submit-text` | `Submit` | Reply submit text. |
| `data-cancel-text` | `Cancel` | Reply cancel text. |
| `data-name-placeholder` | `Your name` | Name input placeholder. |
| `data-website-placeholder` | `Website (optional)` | Website input placeholder. |
| `data-reply-placeholder` | `Write your reply...` | Reply content placeholder. |
| `data-pending-text` | `Reply submitted (pending approval).` | Text after a successful reply. |

### Display values

| Attribute | Default | Description |
|---|---|---|
| `data-hide-replies` | `false` | Hide reply buttons. |
| `data-hide-heading` | `false` | Hide the heading. |
| `data-nostyles` | `false` | Do not add the default CSS. |
| `data-link-target` | `_blank` | Target for author links. |
| `data-avatar-size` | `24` | Avatar size in pixels. |
| `data-turnstile-sitekey` | Unset | Public Turnstile sitekey for reply forms. |

## Widget classes

Set `data-nostyles="true"` to supply all CSS.

The widget uses these classes:

```text
.nc-comment .nc-meta .nc-avatar .nc-author .nc-date .nc-body
.nc-reactions .nc-reply-btn .nc-reply-form .nc-thread
.nc-heading .nc-empty .nc-error
```

## Top-level form

The widget supplies reply forms. Create a top-level form on the main site.

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

The server reads the CONFIGURED honeypot field (`HONEYPOT_FIELD`, default
`website`). The widget's reply forms emit the configured name automatically
(the server substitutes it into the served `/embed/comments.js`), so widget
users never set this by hand. Custom top-level forms MUST use the configured
name: with `HONEYPOT_FIELD=company`, name the hidden input `company` —
`website` is inert and filling it does not flag.

The response contains `delete_token` and `status`. It does not contain the new
comment ID.

## Webmention discovery

When the server uses the `webmentions` feature, add this link to the main site:

```html
<link rel="webmention" href="https://comments.your-site.example/api/webmention">
```

## Turnstile

When the server enables Turnstile, every native comment and reply needs a valid
token.

For a top-level form, load the script and add the widget container:

```html
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
<div class="cf-turnstile" data-sitekey="public-sitekey"></div>
```

The widget creates `cf-turnstile-response`. Do not add that field by hand.

For inline replies, set `data-turnstile-sitekey` on the comments script.

## Custom frontend

Read comments with `fetch`:

```js
fetch('https://comments.your-site.example/api/comments?path=/blog/post-1&limit=50')
  .then(function (response) {
    if (!response.ok) throw new Error('HTTP ' + response.status);
    return response.json();
  })
  .then(function (data) {
    renderThread(data.comments, data.total);
  });
```

The read API supports these query values:

```text
sort=newest
before=42
sort=oldest
after=42
```

Use `before` with `newest`. Use `after` with `oldest`.

Each public comment contains:

```json
{
  "id": 42,
  "comment_type": "native",
  "author_name": "Alice",
  "author_url": "https://alice.blog",
  "author_avatar": "https://alice.blog/avatar.jpg",
  "content": "<p>Great post.</p>",
  "created_at": "2026-07-03 16:40:00",
  "parent_id": null,
  "depth": 0,
  "reactions": {
    "👍": 5
  }
}
```

The `content` value is sanitized HTML. Use `innerHTML` only for this value.
Use text or attribute values for author fields. Do not put author fields in
`innerHTML`.

Build the reply tree from `parent_id`:

```js
function buildTree(comments) {
  var nodes = {};
  var roots = [];

  comments.forEach(function (comment) {
    nodes[comment.id] = { comment: comment, children: [] };
  });

  comments.forEach(function (comment) {
    var node = nodes[comment.id];
    if (comment.parent_id && nodes[comment.parent_id]) {
      nodes[comment.parent_id].children.push(node);
    } else {
      roots.push(node);
    }
  });

  roots.sort(function (a, b) {
    return b.comment.id - a.comment.id;
  });

  Object.keys(nodes).forEach(function (id) {
    nodes[id].children.sort(function (a, b) {
      return a.comment.id - b.comment.id;
    });
  });

  return roots;
}
```

The widget uses newest roots and oldest replies. A custom frontend can honor
the selected API order instead.

## Submit with JavaScript

```js
var body = new URLSearchParams({
  target_path: '/blog/post-1',
  author_name: 'Alice',
  content: '<p>Nice post.</p>'
});

fetch('https://comments.your-site.example/api/comment', {
  method: 'POST',
  headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
  body: body
})
  .then(function (response) {
    if (!response.ok) throw new Error('HTTP ' + response.status);
    return response.json();
  })
  .then(function (data) {
    console.log(data.status);
  });
```

## Reactions

The public reaction route needs the admin token by default:

```js
fetch('https://comments.your-site.example/api/comment/42/reaction', {
  method: 'POST',
  headers: {
    'Authorization': 'Bearer ' + token,
    'Content-Type': 'application/json'
  },
  body: JSON.stringify({ reaction: '👍' })
});
```

Only approved reactions appear in `reactions` counts.
The public CORS preflight does not advertise `DELETE`.

## Verified owner checkmark

Comments with `verified: true` are authored through the admin owner
endpoint — render a green ✓ badge after the author name with the title
"Verified site owner". Tell visitors to trust the badge, never the bare
name: anyone can submit any author name, but no public path can set the
flag.

## Security rules

- Render `content` as sanitized HTML only.
- Render author values as text or attributes.
- Keep the admin token out of browser code unless the user accepts admin mode.
- Use HTTPS for public requests.
- Set the exact main site origin in `ALLOWED_CORS_ORIGIN`.

See [the full API reference](../docs/api.md).
