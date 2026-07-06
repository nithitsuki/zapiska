# Skill: Build a zapiska frontend

Use this skill when the user wants to display comments on their site and/or
provide a form for visitors to submit comments. This skill is entirely
client-side: zapiska exposes JSON and accepts form-encoded POSTs.

## Before you start

Confirm:

- The zapiska server is already deployed and reachable at
  `https://comments.your-site.example`
- `ALLOWED_CORS_ORIGIN` on the server is set to the main site's origin
- The page path on the main site, e.g. `/blog/hello-world`

## Step 1: Choose integration style

Pick one path for the user:

- **Drop-in widget** (`embed/comments.js`) — fastest, handles threaded replies,
  but renders its own markup and styles.
- **Custom frontend** — fetch the JSON API and render however you like.

If the user wants to customize text/styling without writing a full frontend,
use the widget with `data-*` overrides.

If the user is in a framework (Next.js, Astro, SvelteKit, etc.), guide them
to use the same endpoints from their framework's client JS / fetch layer.

## Step 2: Render the comment thread

### Option A: Drop-in widget

Add to the page:

```html
<div id="nc-comments"></div>
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

Customize via attributes:

```html
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-heading-text="%d thoughts"
  data-reply-text="Reply to this"
  data-hide-replies="false"
  data-nostyles="false"
  data-avatar-size="32"
></script>
```

Available attributes (see `embed/README.md` for defaults and the full list):

- `data-path` (required)
- `data-limit`
- `data-heading-text` — use `%d` for count
- `data-empty-text`, `data-error-text`
- `data-reply-text`, `data-submit-text`, `data-cancel-text`
- `data-name-placeholder`, `data-website-placeholder`, `data-reply-placeholder`
- `data-pending-text`
- `data-hide-replies="true"` — read-only thread
- `data-hide-heading="true"`
- `data-nostyles="true"` — skip default CSS, style `.nc-*` classes yourself
- `data-link-target="_self"` — default is `_blank`
- `data-avatar-size` — pixels, default 24

### Option B: Custom frontend

Fetch comments directly:

```js
fetch('https://comments.your-site.example/api/comments?path=/blog/hello-world&limit=50')
  .then(r => r.json())
  .then(function (data) {
    // data.total = approved count
    // data.comments = flat array with parent_id / depth
    renderThread(data.comments, data.total);
  });
```

Build a tree client-side:

```js
function buildTree(comments) {
  const byId = {};
  const roots = [];
  comments.forEach(c => byId[c.id] = { comment: c, children: [] });
  comments.forEach(c => {
    if (c.parent_id && byId[c.parent_id]) byId[c.parent_id].children.push(byId[c.id]);
    else roots.push(byId[c.id]);
  });
  roots.sort((a, b) => b.comment.id - a.comment.id);        // newest first
  Object.values(byId).forEach(n => n.children.sort((a, b) => a.comment.id - b.comment.id)); // oldest first
  return roots;
}
```

Render each comment:

- `content` is ammonia-sanitized HTML — insert via `innerHTML`.
- `author_name`, `author_url`, `author_avatar` are plain text — escape/use as
  text/attributes, never `innerHTML`.

## Step 3: Add a top-level comment form

The widget only provides inline reply forms. You must build the top-level
comment form yourself.

```html
<form id="comment-form" action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text"  name="author_name" placeholder="Name" required>
  <input type="url"   name="author_url"  placeholder="Website (optional)">
  <textarea name="content" required></textarea>
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

For a JS-only submission that avoids a full page reload:

```js
document.getElementById('comment-form').addEventListener('submit', function (e) {
  e.preventDefault();
  var form = e.target;
  var body = new URLSearchParams(new FormData(form)).toString();
  fetch(form.action, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: body
  })
  .then(function (r) {
    if (!r.ok) throw new Error('HTTP ' + r.status);
    return r.json();
  })
  .then(function (data) {
    // Optional: store data.delete_token in localStorage for self-service deletion
    form.reset();
    alert('Comment submitted — pending approval.');
  })
  .catch(function (err) {
    alert('Submission failed: ' + err.message);
  });
});
```

## Step 4: Style it

If using the widget:

- Override `.nc-*` classes in your own stylesheet.
- Or set `data-nostyles="true"` and provide all CSS.

If building custom:

- Use any CSS framework or hand-written styles.
- Remember the security contract: `content` is sanitized HTML, author fields
  are plain text.

## Step 5: Add webmention discovery (optional)

If the `webmentions` feature is enabled on the server, add to `<head>`:

```html
<link rel="webmention" href="https://comments.your-site.example/api/webmention" />
```

## Step 6: Test end-to-end

1. Submit a comment through your form.
2. Log in to the admin API and approve it:

   ```sh
   curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     https://comments.your-site.example/api/admin/pending
   curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"id":1,"action":"approved"}' \
     https://comments.your-site.example/api/admin/moderate
   ```

3. Reload the page and confirm the comment appears.
4. Submit a reply and confirm it appears nested under the parent.

## Common pitfalls

- `data-path` must match the main site's path after `PUBLIC_TARGET_ORIGIN`.
  For `https://your-site.example/blog/hello-world` the path is
  `/blog/hello-world`.
- If the server has `TURNSTILE_ENABLED=true`, forms must include a valid
  `cf-turnstile-response`. See `.skills/configure-turnstile/SKILL.md`.
- If using the widget with replies, set `data-turnstile-sitekey` so the inline
  reply form renders the Turnstile widget.
