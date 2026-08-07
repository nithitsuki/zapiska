# Skill: configure Cloudflare Turnstile

Use this skill when a user wants bot protection on native comment forms.

Turnstile is optional. When enabled, every `POST /api/comment` request needs a
valid `cf-turnstile-response` value.

## Confirm these values

- The zapiska server is running.
- The main site host.
- Whether the widget reply form also needs Turnstile.

Do not ask the user to send the secret key in chat.

## Create the widget

1. Open the Cloudflare dashboard.
2. Open Turnstile.
3. Add the main site host.
4. Add `localhost` and `127.0.0.1` for local tests.
5. Select the widget mode.
6. Copy the public sitekey and secret key.

## Configure the server

Set these values on the server:

```env
TURNSTILE_ENABLED=true
TURNSTILE_SECRET_KEY=replace-this-value
```

`TURNSTILE_VERIFY_URL` uses the Cloudflare siteverify URL by default.
The server fails at startup when the secret is missing.

Check the server:

```sh
curl http://127.0.0.1:3000/healthz
```

## Add a top-level form

Load the Turnstile script once:

```html
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
```

Add the widget container to the form:

```html
<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text" name="author_name" required>
  <textarea name="content" required></textarea>
  <div class="cf-turnstile" data-sitekey="public-sitekey"></div>
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

The widget creates `cf-turnstile-response`.
Do not add that field by hand.

## Add the widget reply form

Set the public sitekey on the comments script:

```html
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-turnstile-sitekey="public-sitekey"
></script>
```

The widget loads the Turnstile script and sends the token with the reply.

## Test

1. Submit without a valid token.
2. Confirm `400` and `turnstile_failed`.
3. Confirm that no comment was stored.
4. Submit with a valid token.
5. Confirm `201`.

If siteverify is not reachable, zapiska returns `503` and does not store the
comment.

## Security

- The sitekey is public and can be in HTML.
- The secret stays in the server environment.
- The server sends the token to siteverify.
- The browser never receives the secret.

See `docs/deployment.md` and `embed/README.md`.
