# Set up zapiska with an AI agent

You can have an AI coding assistant set up zapiska end-to-end by pasting the
prompt below. The prompt instructs the agent to read the skills in `.skills/`
and the docs in `docs/` and follow them step by step.

## Copy-paste prompt

```text
Set up zapiska for me end to end.

zapiska is a self-hosted comment engine in this repo. Follow the skills in
`.skills/` and the docs in `docs/` to:

1. Set up the zapiska server (read `.skills/setup-server/SKILL.md`).
2. Build the frontend integration on my site (read `.skills/build-frontend/SKILL.md`).
3. Optionally configure Cloudflare Turnstile (read `.skills/configure-turnstile/SKILL.md`).

Work through the skills in that order. Ask me only when required information is
missing (e.g. domain, deployment preference, Cloudflare credentials).

Details:
- My main site origin: https://your-site.example
- I want to host zapiska at: https://comments.your-site.example
- Deployment preference: Docker / build from source / pre-built binary
- Add Turnstile bot protection: yes / no
- Framework or site type: (e.g. static HTML, Next.js, Astro, SvelteKit, WordPress, etc.)

When you need secrets, tell me the exact environment variable name and where to
set it, but do not write secrets into any files or chat logs.
```

## How to use it

1. Replace the placeholder values in the prompt with your actual details.
2. Paste the prompt into your AI coding assistant (OpenCode, Claude Code,
   Cursor, Copilot Chat, etc.).
3. The agent will read the skills and docs from the repo and perform the setup.
4. If you already have some parts done, tell the agent which step to start from.

## What each skill covers

| Skill | File | Purpose |
|---|---|---|
| Set up server | `.skills/setup-server/SKILL.md` | Deploy zapiska, configure env vars, reverse proxy, TLS, systemd, health checks. |
| Build frontend | `.skills/build-frontend/SKILL.md` | Add the widget or a custom frontend, comment form, styling, webmention link. |
| Configure Turnstile | `.skills/configure-turnstile/SKILL.md` | Enable bot protection on the server and in the comment forms. |

## Docs reference

The skills point to these docs when more detail is needed:

- [`docs/getting-started.md`](getting-started.md) — end-to-end walkthrough
- [`docs/deployment.md`](deployment.md) — env vars, Docker, systemd, updates
- [`docs/api.md`](api.md) — full API reference
- [`embed/README.md`](../embed/README.md) — widget attributes and custom frontend example
- [`.env.example`](../.env.example) — annotated configuration file

## Tips

- Make sure the assistant has access to the repo (it needs to read `.skills/`
  and `docs/`).
- If you only want the server, tell the assistant to skip the frontend and
  Turnstile skills.
- If you already deployed the server, tell the assistant to start from the
  frontend skill.
