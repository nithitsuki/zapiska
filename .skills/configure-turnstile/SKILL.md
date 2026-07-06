# Skill: Configure Cloudflare Turnstile for zapiska

Use this skill when the user wants bot protection on their comment forms.
Turnstile is optional in zapiska and can be enabled server-wide; once enabled,
every `POST /api/comment` must include a valid `cf-turnstile-response` token.

## Before you start

Confirm:

- The zapiska server is already deployed
- The user has or can create a Cloudflare account
- The user knows whether they want Turnstile on both the top-level form and
  the widget's inline reply form

## Step 1: Create the Turnstile widget in Cloudflare

1. Go to https://dash.cloudflare.com → Turnstile → Add site.
2. Give it a name, e.g. `your-site comments`.
3. Add the allowed hostnames:
   - Your main site, e.g. `your-site.example`
   - `localhost` and `127.0.0.1` for local development
4. Choose the widget mode (Managed is the safest default).
5. Save and note:
   - **Site Key** (public, starts with `0x4AAAAAA...`)
   - **Secret Key** (private, starts with `0x4AAAAAA...`)

## Step 2: Configure the zapiska server

Add to the server's environment:

```env
TURNSTILE_ENABLED=true
TURNSTILE_SECRET_KEY=0x4AAAAAAA...your-secret...
```

`TURNSTILE_VERIFY_URL` defaults to Cloudflare's public endpoint and rarely
needs changing.

Restart zapiska. The server will fail fast if `TURNSTILE_ENABLED=true` and the
secret is missing.

Verify from the server host:

```sh
curl http://127.0.0.1:3000/healthz
# -> ok
```

## Step 3: Add Turnstile to the top-level comment form

Load the Turnstile API once per page:

```html
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
```

Add the widget container inside the form. Do **not** add a hidden input
manually — the widget creates `cf-turnstile-response` automatically:

```html
<form action="https://comments.your-site.example/api/comment" method="POST">
  <input type="hidden" name="target_path" value="/blog/hello-world">
  <input type="text"  name="author_name" placeholder="Name" required>
  <input type="url"   name="author_url"  placeholder="Website (optional)">
  <textarea name="content" required></textarea>
  <div class="cf-turnstile" data-sitekey="0x4AAAAAAA-your-sitekey"></div>
  <input type="text" name="website" style="display:none">
  <button type="submit">Send</button>
</form>
```

If submitting via JavaScript, include the token in the body:

```js
var token = turnstile.getResponse(widgetId);
var body = 'target_path=' + encodeURIComponent('/blog/hello-world') +
           '&author_name=' + encodeURIComponent(name) +
           '&content=' + encodeURIComponent(content) +
           '&cf-turnstile-response=' + encodeURIComponent(token);
```

## Step 4: Add Turnstile to the drop-in widget's reply form

If using `embed/comments.js`, set the sitekey on the script tag:

```html
<script
  id="nc-comments"
  src="https://comments.your-site.example/embed/comments.js"
  data-path="/blog/hello-world"
  data-turnstile-sitekey="0x4AAAAAAA-your-sitekey"
></script>
```

The widget will:

- Load the Turnstile API on demand
- Render a widget in each inline reply form
- Include `cf-turnstile-response` in the POST
- Clean up the widget when the form is cancelled or submitted

## Step 5: Test fail-closed behavior

1. Submit a comment **without** solving the Turnstile widget.
2. Expect `400 Bad Request` with body `{ "code": "turnstile_failed" }` and no
   comment stored.
3. Submit a comment **with** a solved widget.
4. Expect `201 Created` and a `delete_token` in the response.

## Step 6: Troubleshooting

- **400 with `turnstile_failed`**: the token is missing, expired, or was not
  generated for the origin making the request. Check that the site's origin is
  in the widget's allowed hostnames.
- **503 on comment submit**: zapiska could not reach Cloudflare's siteverify
  endpoint. It fails closed — no comment is stored. Check outbound HTTPS from
  the server.
- **Widget does not render**: ensure the Turnstile script loaded and the
  `data-sitekey` is the public sitekey, not the secret.
- **Secret leaked in logs**: verify the server startup logs show
  `turnstile_secret_key: ***`. If not, rotate the secret immediately.

## Security notes

- The **sitekey** is public and safe in HTML.
- The **secret** lives only in the zapiska env / Cloudflare Worker secret store.
  Never commit it.
- zapiska verifies tokens server-side. Do **not** call siteverify from the
  browser.
