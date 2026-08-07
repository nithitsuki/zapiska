# Skill: build a zapiska frontend

Use this skill when a user wants to show comments or submit comments.

The server returns JSON and accepts form-encoded requests.

## Confirm these values

- zapiska origin.
- Main site origin in `ALLOWED_CORS_ORIGIN`.
- Page path, such as `/blog/hello-world`.
- Whether replies are enabled with `MAX_THREAD_DEPTH`.
- Whether Turnstile is enabled.

## Choose an integration

Choose one:

- The supplied widget.
- A custom frontend.

## Widget

```html
<div id="nc-comments"></div>
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-limit="50"
></script>
```

The widget reads approved comments and builds the reply tree.
Set `data-nostyles="true"` to supply custom CSS.

Use `embed/README.md` for the full attribute list.

## Custom frontend

Read comments:

```js
fetch('https://comments.your-site.example/api/comments?path=/blog/hello-world&limit=50')
  .then(function (response) {
    if (!response.ok) throw new Error('HTTP ' + response.status);
    return response.json();
  })
  .then(function (data) {
    renderThread(data.comments, data.total);
  });
```

The API supports `sort=newest`, `before`, `sort=oldest`, and `after`.
Use `parent_id` to build the reply tree.

The public object contains approved `reactions` counts.

Use `innerHTML` only for sanitized `content`.
Use text or attribute values for author fields.

## Top-level form

The widget supplies reply forms. Create the top-level form:

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

The response contains a delete token and status. It does not contain the new
comment ID.

## Webmention discovery

When webmentions are enabled, add:

```html
<link rel="webmention" href="https://comments.your-site.example/api/webmention">
```

## Turnstile

When Turnstile is enabled, add the widget to every native comment form:

```html
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
<div class="cf-turnstile" data-sitekey="public-sitekey"></div>
```

For inline replies, set `data-turnstile-sitekey` on the comments script.

## Test

1. Submit a top-level comment.
2. List pending comments with the admin API.
3. Approve the comment.
4. Read the public API.
5. Enable reply depth and submit a reply.

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://comments.your-site.example/api/admin/pending
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":1,"action":"approved"}' \
  https://comments.your-site.example/api/admin/moderate
```

## Common errors

- `data-path` must match the path after `PUBLIC_TARGET_ORIGIN`.
- Replies need `MAX_THREAD_DEPTH > 0`.
- Turnstile needs a valid `cf-turnstile-response` value.
- CORS needs the exact main site origin.

See `embed/README.md` and `docs/api.md`.
