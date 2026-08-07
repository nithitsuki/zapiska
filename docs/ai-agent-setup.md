# Set up zapiska with an AI agent

Use the prompt below with an AI coding assistant.
The assistant must read the repository skills and documentation before it acts.

## Prompt

```text
Set up zapiska from start to finish.

Read these files before you make changes:

1. .skills/setup-server/SKILL.md
2. .skills/build-frontend/SKILL.md
3. .skills/configure-turnstile/SKILL.md, when Turnstile is requested
4. docs/getting-started.md
5. docs/deployment.md
6. docs/api.md

Ask a question only when you need information that is not in this prompt.
Do not write secrets to files or chat messages.

Main site origin: https://your-site.example
zapiska origin: https://comments.your-site.example
Deployment method: Docker, source build, or release binary
Turnstile: enabled or disabled
Site type: static HTML, Astro, Next.js, SvelteKit, WordPress, or other

Do these tasks:

1. Deploy the server.
2. Set the required environment variables.
3. Check /healthz.
4. Add the widget or a custom frontend.
5. Add a native comment form.
6. Add webmention discovery when webmentions are enabled.
7. Test one comment and one moderation decision.
8. Report the exact files and commands that you used.
```

## Use the prompt

1. Replace the placeholder values.
2. Paste the prompt into the assistant.
3. Answer questions about the deployment and site.
4. Check the reported files and commands.

## Skill files

| Skill | File | Purpose |
|---|---|---|
| Server setup | `.skills/setup-server/SKILL.md` | Deployment and service setup. |
| Frontend | `.skills/build-frontend/SKILL.md` | Widget, form, and custom frontend. |
| Turnstile | `.skills/configure-turnstile/SKILL.md` | Cloudflare bot protection. |

## Reference files

- [Getting started](getting-started.md)
- [Deployment](deployment.md)
- [API](api.md)
- [Embed](../embed/README.md)
- [Configuration](../.env.example)
