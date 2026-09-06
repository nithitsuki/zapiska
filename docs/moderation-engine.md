# Build a moderation engine

zapiska stores comments and exposes moderation routes. It does not decide
whether a comment is spam.

An external service can receive a webhook, query context, and send a status.
It can also poll the admin API when webhooks are not suitable.

## Flow

```text
native comment
    |
    +-- zapiska stores the comment
    +-- optional comment.created webhook
    +-- moderation service reads context
    +-- moderation service sends a status

reaction
    |
    +-- zapiska stores a pending reaction
    +-- optional reaction.created webhook
    +-- moderation service sends a reaction status
```

Webmentions use the webmention worker. The worker sends admin notifications for
new mentions, but it does not send the moderation webhook.

## Configure zapiska

Set these values:

```env
ADMIN_TOKEN=replace-this-value
MODERATION_WEBHOOK_URL=http://localhost:9000/webhook
MODERATION_WEBHOOK_MODE=async
STORE_IP_ADDRESS=true
IP_HASH_SECRET=replace-this-value
DEFAULT_COMMENT_STATUS=pending
```

The webhook URL is optional. The default mode is `async`.

In asynchronous mode, zapiska sends the event and does not wait for a decision.
The comment stays in its configured initial status until the service calls the
admin API.

In synchronous mode, the service must return JSON with an `action` value.
Valid actions are `approved`, `spam`, `deleted`, and `pending`.

```json
{
  "action": "approved"
}
```

If the webhook fails, zapiska keeps the current status.

## Comment event

The native comment event uses `event: comment.created`.

```json
{
  "event": "comment.created",
  "id": 42,
  "target_path": "/blog/hello",
  "comment_type": "native",
  "author_name": "Alice",
  "author_url": "https://alice.blog",
  "author_avatar": "https://alice.blog/avatar.jpg",
  "content": "<p>Great post.</p>",
  "honeypot": false,
  "parent_id": null,
  "depth": 0,
  "submitter_ip": "203.0.113.42",
  "delete_token": "0123456789abcdef",
  "content_hash": "h:a1b2c3d4",
  "is_reply": false,
  "parents": null,
  "submitter": {
    "ip": "203.0.113.42",
    "total_comments": 4,
    "approved_comments": 2,
    "spam_comments": 1,
    "pending_comments": 1,
    "deleted_comments": 0,
    "first_seen": "2026-08-01 10:00:00"
  },
  "admin_url": "/api/admin/comments/42"
}
```

The `submitter_ip` value is absent when `STORE_IP_ADDRESS=false`. When IP
storage is enabled, it is the raw stored peer IP. The event does not include
`submitter_ip_hash`.

The `parents` value is an array for a reply and `null` for a top-level comment.
The event does not contain extracted URL rows. Query those rows with the URL
route.

## Reaction events

New reactions use `event: reaction.created`.

```json
{
  "event": "reaction.created",
  "id": 7,
  "comment_id": 42,
  "reaction": "👍",
  "status": "pending",
  "target_path": "/blog/hello",
  "is_admin": true,
  "admin_url": "/api/admin/reactions"
}
```

Reaction status changes use `event: reaction.status_changed`.

```json
{
  "event": "reaction.status_changed",
  "id": 7,
  "comment_id": 42,
  "reaction": "👍",
  "old_status": "pending",
  "new_status": "approved",
  "changed_by": "admin"
}
```

## Authenticate

Send the admin token with every protected request:

```text
Authorization: Bearer replace-this-value
```

You can use a session cookie instead:

```sh
curl -c cookies.txt -X POST \
  -H "Content-Type: application/json" \
  -d '{"token":"replace-this-value"}' \
  http://localhost:3000/api/admin/login
```

## Query comment context

Get a comment and its parent chain:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://localhost:3000/api/admin/comments/42
```

List comments by status:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://localhost:3000/api/admin/comments?status=pending&limit=50'
```

Use `status=all` to include approved, spam, and deleted rows.

Find comments by the raw stored IP when `STORE_IP_ADDRESS=true`:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://localhost:3000/api/admin/comments?ip=203.0.113.42&status=all'
```

Find repeated content:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://localhost:3000/api/admin/comments?content_hash=h:a1b2c3d4&status=all'
```

Find author activity:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://localhost:3000/api/admin/authors/lookup?author_name=Alice'
```

Find URLs from one comment:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://localhost:3000/api/admin/comments/42/urls
```

Find comments that contain one URL:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://localhost:3000/api/admin/urls/lookup?url_hash=h:a1b2c3d4'
```

Find URLs from one domain:

```sh
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://localhost:3000/api/admin/urls/lookup?domain=spam.example'
```

## Query bulk context

Use one request for several comments:

```sh
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "comment_ids": [42, 43],
    "include_parents": true,
    "include_author_stats": true,
    "include_urls": true
  }' \
  http://localhost:3000/api/admin/comments/context
```

The response can contain parent chains, author status counts, and URL rows.

## Send decisions

Change one comment:

```sh
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":42,"action":"spam"}' \
  http://localhost:3000/api/admin/moderate
```

Change several comments:

```sh
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "actions": [
      {"id":42,"action":"approved"},
      {"id":43,"action":"spam"}
    ]
  }' \
  http://localhost:3000/api/admin/moderate/batch
```

The batch route processes each item independently. Use it for polling jobs.
Over the throttle budget the batch routes answer `429` with a `Retry-After`
header but a plain-text body (not the JSON error shape): polling jobs must
read the headers, not parse the body.

Moderate reactions with the same route shape:

```sh
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":7,"action":"approved"}' \
  http://localhost:3000/api/admin/reactions/moderate
```

## Python rules engine

This small example flags honeypot comments and repeated spam.

```python
import requests

API = "http://localhost:3000"
TOKEN = "replace-this-value"
HEADERS = {"Authorization": f"Bearer {TOKEN}"}


def action_for(comment):
    if comment.get("honeypot"):
        return "spam"

    ip = comment.get("submitter_ip")
    if ip:
        response = requests.get(
            f"{API}/api/admin/comments",
            params={"ip": ip, "status": "all"},
            headers=HEADERS,
            timeout=10,
        )
        history = response.json().get("comments", [])
        spam_count = sum(item["status"] == "spam" for item in history)
        if spam_count > len(history) / 2:
            return "spam"

    return "approved"


def moderate_pending():
    response = requests.get(
        f"{API}/api/admin/comments",
        params={"status": "pending", "limit": 50},
        headers=HEADERS,
        timeout=10,
    )
    comments = response.json().get("comments", [])
    actions = [
        {"id": item["id"], "action": action_for(item)}
        for item in comments
    ]
    if actions:
        requests.post(
            f"{API}/api/admin/moderate/batch",
            json={"actions": actions},
            headers=HEADERS,
            timeout=10,
        )


if __name__ == "__main__":
    moderate_pending()
```

## LLM moderation

An LLM can return one valid action. Validate the result before sending it to
zapiska. Keep uncertain results as `pending`.

Do not send the admin token or raw IP data to an external model unless the
privacy policy allows it.

## Webhook receiver

Return `200` as soon as the receiver stores the event. Process the event in a
separate task when the moderation work takes time.

```python
from flask import Flask, request
import requests

app = Flask(__name__)
API = "http://localhost:3000"
TOKEN = "replace-this-value"


@app.post("/webhook")
def receive():
    event = request.get_json()
    if event["event"] == "comment.created":
        action = "approved"
        requests.post(
            f"{API}/api/admin/moderate",
            json={"id": event["id"], "action": action},
            headers={"Authorization": f"Bearer {TOKEN}"},
            timeout=10,
        )
    return {"ok": True}, 200
```

## Service rules

The single moderation route has a default limit of 10 requests per 60 seconds.
The batch route is the better choice for polling.

Keep the admin token in an environment variable. Use HTTPS between the service
and zapiska when they use different hosts.

If the service stops, pending comments remain pending. Poll them after restart.

## Reference

- [API reference](api.md)
- [Deployment reference](deployment.md)
- [Security notes](security.md)
