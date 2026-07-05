# Building a custom moderation engine for zapiska

zapiska is deliberately dumb about moderation — it stores comments and exposes APIs. What happens between "comment stored" and "comment approved/spam/deleted" is entirely up to you.

This guide walks through building an external moderation engine that hooks into zapiska's admin API and webhook.

## How it works

```
                 ╔══════════════════════╗
                 ║    zapiska server    ║
                 ║                      ║
                 ║  POST /api/comment ──╫──→ webhook POST (optional)
                 ║                      ║
                 ║  GET /api/admin/*  ←─╫── your engine queries context
                 ║                      ║
                 ║  POST /api/admin/  ←─╫── your engine sends decision
                 ║    moderate          ║
                 ╚══════════════════════╝
```

1. A comment arrives at `POST /api/comment`. zapiska validates, sanitizes, stores it, extracts URLs.
2. If `MODERATION_WEBHOOK_URL` is set, zapiska sends a POST to your engine with the enriched payload (includes submitter stats, parent chain, content hash, extracted URLs).
3. Your engine queries the admin API for context (parent chain, IP history, honeypot flag, author identity, URL cross-references, etc.).
4. Your engine sends a moderation decision back via `POST /api/admin/moderate` or `POST /api/admin/moderate/batch`.

The webhook mode is configurable:
- **Async** (default): fire-and-forget. Your engine calls back whenever it's ready.
- **Sync**: zapiska waits for your engine's response and applies the returned `action` immediately. The 201 response includes the final status.

## Step 1: Configure zapiska

```env
# Required: the admin token your engine will use to authenticate
ADMIN_TOKEN="your-secret-token"

# Notify your engine when new comments arrive
MODERATION_WEBHOOK_URL="http://localhost:9000/webhook"

# Enable IP storage for spam analysis
STORE_IP_ADDRESS=true

# Default status for new comments
# "pending" = manual review required (safe default)
# "approved" = auto-publish, your engine can revert if needed
DEFAULT_COMMENT_STATUS="pending"
```

## Step 2: The webhook payload

When a comment is submitted and `MODERATION_WEBHOOK_URL` is configured, zapiska POSTs this JSON to your engine:

```json
{
  "event": "comment.created",
  "id": 42,
  "target_path": "/blog/hello",
  "comment_type": "native",
  "author_name": "Alice",
  "author_url": "https://alice.blog",
  "author_avatar": "https://alice.blog/avatar.jpg",
  "honeypot": false,
  "parent_id": null,
  "depth": 0,
  "submitter_ip": "203.0.113.42",
  "delete_token": "a1b2c3d4e5f6g7h8",
  "admin_url": "/api/admin/comments/42"
}
```

The webhook is fire-and-forget — zapiska does not wait for a response. Your engine should acknowledge it (200) and process asynchronously.

## Step 3: Authenticate

All admin API calls need the `ADMIN_TOKEN`:

```
Authorization: Bearer your-secret-token
```

Or get a session cookie:

```sh
curl -c cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d '{"token":"your-secret-token"}' \
  http://localhost:3000/api/admin/login
```

## Step 4: Query context

Your engine has access to the full admin API. The most useful queries:

### Get the comment with parent chain

```sh
curl -H "Authorization: Bearer your-secret-token" \
  http://localhost:3000/api/admin/comments/42
```

Returns the comment and its full ancestor chain. Lets your engine see the whole conversation before deciding.

### Get all comments from the same IP

```sh
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/comments?ip=203.0.113.42&status=all"
```

Returns every comment from that IP across all paths and statuses. Useful for building IP reputation.

### Look up author identity across signals

```sh
# Find all activity from this IP + author name combination
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/authors/lookup?ip=203.0.113.42&author_name=Alice"

# Wider net: merge across all signals with OR
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/authors/lookup?ip=203.0.113.42&author_url=https://alice.blog&combine=true"
```

Returns aggregated stats (total, approved, spam, pending, deleted) and recent comments. No database needed on your side.

### Check for duplicate content

```sh
# Get the content_hash from the webhook payload, then query
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/comments?content_hash=h:a1b2c3d4e5f6g7h8&status=all"
```

Returns all comments with the same normalized content. Useful for detecting spam that's been copy-pasted across multiple pages.

### Look up URLs across comments

```sh
# Get URLs for a specific comment
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/comments/42/urls"

# Look up all comments referencing a URL
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/urls/lookup?url_hash=h:a1b2..."

# Look up all URLs from a spam domain
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/urls/lookup?domain=spam.example"
```

Cross-comment URL tracking without maintaining your own database.

### Bulk context (fetch everything in one call)

```sh
curl -X POST -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "comment_ids": [42, 43, 44],
    "include_parents": true,
    "include_author_stats": true,
    "include_urls": false
  }' \
  http://localhost:3000/api/admin/comments/context
```

Returns each comment with parent chain, author stats, and URLs in a single response. Replaces N individual API calls.

### Get pending comments (batch processing)

```sh
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/comments?status=pending&limit=50"
```

Your engine can poll this endpoint on a schedule if you prefer polling over webhooks.

### Filter by status and path

```sh
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:3000/api/admin/comments?status=all&path=/blog/hello"
```

## Step 5: Submit moderation decisions

### Single comment

```sh
curl -X POST -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"id":42,"action":"spam"}' \
  http://localhost:3000/api/admin/moderate
```

Valid actions: `approved`, `spam`, `deleted`, `pending`.

### Batch (many comments at once)

```sh
curl -X POST -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "actions":[
      {"id":1,"action":"approved"},
      {"id":2,"action":"spam"},
      {"id":3,"action":"deleted"}
    ]
  }' \
  http://localhost:3000/api/admin/moderate/batch
```

Each action is processed independently. Errors for individual items don't affect others.

## Example 1: Simple Python rules engine

```python
import requests

API = "http://localhost:3000"
TOKEN = "your-secret-token"
HEADERS = {"Authorization": f"Bearer {TOKEN}"}

def moderate_comment(comment):
    """Return an action for a single comment, or None to leave as-is."""
    # Rule: honeypot triggered → spam
    if comment.get("honeypot"):
        return "spam"

    # Rule: IP with history of spam → spam
    ip = comment.get("submitter_ip")
    if ip:
        resp = requests.get(
            f"{API}/api/admin/comments",
            params={"ip": ip, "status": "all"},
            headers=HEADERS,
        )
        ip_comments = resp.json().get("comments", [])
        spam_ratio = sum(
            1 for c in ip_comments if c["status"] == "spam"
        ) / max(len(ip_comments), 1)
        if spam_ratio > 0.5:
            return "spam"

    # Rule: no URL and suspicious content → pending (human review)
    if not comment.get("author_url") and any(
        word in comment.get("content", "").lower()
        for word in ["buy now", "click here", "free money"]
    ):
        return "pending"

    # Default: approve
    return "approved"

def poll():
    while True:
        resp = requests.get(
            f"{API}/api/admin/comments",
            params={"status": "pending", "limit": 20},
            headers=HEADERS,
        )
        comments = resp.json().get("comments", [])
        if not comments:
            break

        actions = [
            {"id": c["id"], "action": moderate_comment(c)}
            for c in comments
        ]
        requests.post(
            f"{API}/api/admin/moderate/batch",
            json={"actions": actions},
            headers=HEADERS,
        )

if __name__ == "__main__":
    poll()
```

## Example 2: LLM-based moderation

```python
import json
import requests
from openai import OpenAI

API = "http://localhost:3000"
TOKEN = "your-secret-token"
HEADERS = {"Authorization": f"Bearer {TOKEN}"}
LLM = OpenAI()

SYSTEM_PROMPT = """You moderate blog comments. Respond with exactly one word:
- "approved" if the comment is constructive and on-topic
- "spam" if it's promotional, irrelevant, or repetitive
- "pending" if you're unsure (send to human review)"""

def moderate_via_llm(comment):
    """Use an LLM to classify a single comment."""
    resp = LLM.chat.completions.create(
        model="gpt-4o-mini",
        messages=[
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": json.dumps({
                "author": comment["author_name"],
                "content": comment["content"],
                "has_url": bool(comment["author_url"]),
                "honeypot": comment["honeypot"],
            })},
        ],
        temperature=0,
        max_tokens=10,
    )
    return resp.choices[0].message.content.strip().lower()

def poll():
    while True:
        resp = requests.get(
            f"{API}/api/admin/comments",
            params={"status": "pending", "limit": 5},
            headers=HEADERS,
        )
        comments = resp.json().get("comments", [])
        if not comments:
            break

        actions = []
        for c in comments:
            action = moderate_via_llm(c)
            if action in ("approved", "spam", "deleted", "pending"):
                actions.append({"id": c["id"], "action": action})

        if actions:
            requests.post(
                f"{API}/api/admin/moderate/batch",
                json={"actions": actions},
                headers=HEADERS,
            )

if __name__ == "__main__":
    poll()
```

## Example 3: Webhook server (Flask)

```python
from flask import Flask, request
import requests

app = Flask(__name__)

API = "http://localhost:3000"
TOKEN = "your-secret-token"

@app.route("/webhook", methods=["POST"])
def handle_webhook():
    data = request.json
    comment_id = data["id"]

    # Fetch full context (parent chain, IP history, etc.)
    resp = requests.get(
        f"{API}/api/admin/comments/{comment_id}",
        headers={"Authorization": f"Bearer {TOKEN}"},
    )
    full = resp.json()

    ip = full["comment"].get("submitter_ip")
    if ip:
        ip_resp = requests.get(
            f"{API}/api/admin/comments",
            params={"ip": ip, "status": "all"},
            headers={"Authorization": f"Bearer {TOKEN}"},
        )
        ip_history = ip_resp.json().get("comments", [])
    else:
        ip_history = []

    # Your moderation logic here...
    action = "approved"  # or "spam", "deleted", "pending"

    requests.post(
        f"{API}/api/admin/moderate",
        json={"id": comment_id, "action": action},
        headers={"Authorization": f"Bearer {TOKEN}"},
    )

    return {"ok": True}, 200

if __name__ == "__main__":
    app.run(port=9000)
```

## Best practices

### Rate limiting

zapiska rate-limits the admin API by IP (60 req / 60s). Design your engine to batch decisions rather than making one API call per comment. Use `POST /api/admin/moderate/batch` instead of looping over individual `POST /api/admin/moderate` calls. Cache IP lookups locally.

### Error handling

If your engine crashes or the webhook fails, comments stay in `pending` status. Nothing is lost — your engine can poll for unmoderated comments on restart. The batch moderate endpoint reports per-item errors so one bad decision doesn't derail the batch.

### Security

Keep your `ADMIN_TOKEN` in environment variables, not in code. Use HTTPS between your engine and zapiska, especially if they're on different machines. If your engine exposes a webhook endpoint, restrict it to only accept requests from zapiska's IP.

### Testing

zapiska's test suite creates isolated SQLite databases. You can write integration tests that start a test instance, submit comments, and verify your engine's decisions are applied. The `DEFAULT_COMMENT_STATUS` config lets you test with both auto-approved and pending workflows.

### Allowed-by-default moderation

Set `DEFAULT_COMMENT_STATUS=approved` to auto-publish every comment immediately. Your engine still receives webhook notifications and can retroactively change a comment's status if your rules detect a problem. This is useful for low-traffic blogs where false negatives (missed spam) are preferable to false positives (legitimate comments stuck in review).

## Reference

- [Admin API reference](api.md)
- [Webhook payload format](api.md#post-apicomment)
- [Configuration reference](deployment.md)
