# discord_to_insta

Turn Discord announcement-channel posts into Instagram-ready captions + images, and auto-react to new announcements with a fixed emoji set. Web UI, delivered as a single Rust binary or a docker-compose service.

Built for the Mayo Jaune cycling group's weekly ride posts, but the transformation rules and the image-per-distance convention are easy to retune.

## Features

- **Fetch & preview.** Pull recent messages from a Discord channel, click one to load it, and see the cleaned-up Instagram caption render live in a side-by-side view.
- **Caption rules.** Strips `@everyone` / `@here`, resolves `<@user>` mentions via a user-editable ID → Instagram-handle map, rewrites `<#channel>` / `<@&role>` mentions to `voir Discord (lien en bio)`, drops the trailing `Réactions :` block and Discord's relative timestamp.
- **Per-distance image auto-pick.** Parses `Distance : Nkm` from the message, rounds to the nearest integer, finds the matching `*_{km}.png` in `images/`, and shows it inline.
- **Auto-react bot.** Opt-in poller that reacts to new announcements with ✅ 🚫 🤔 every 30 s. Bootstraps silently on first run (no retroactive reactions); persists the last-reacted message ID so restarts resume cleanly.
- **2,200-char counter.** Instagram's caption cap is enforced visibly — the counter turns red if you go over.
- **No JS build step.** Frontend is one static HTML file + htmx from CDN.

## Stack

| Layer | Choice |
|---|---|
| Language | Rust edition 2024 |
| Runtime | tokio (multi-thread) |
| HTTP server | axum 0.7 + tower-http |
| Discord REST | reqwest 0.12 (rustls) |
| Frontend | htmx 2, one static HTML file |
| Env loader | dotenvy |

No tokio-compatible desktop GUI is involved — the previous `egui` frontend was dropped when the UI moved to the browser.

## Quick start

### With docker-compose (recommended)

```bash
cp .env.example .env
# edit .env: set DISCORD_BOT_TOKEN at minimum
docker compose up -d --build
open http://localhost:9010
```

The external port is **9010** by design. Inside the container the server listens on 8080; compose maps the two. Change `docker-compose.yml` if you need a different external port.

Images are bind-mounted read-only from `./images`. The auto-react state file (`state.json`) persists in a named docker volume so `docker compose down` without `-v` won't wipe your bootstrap ID.

### Without docker

```bash
cp .env.example .env
source .env   # or: set -a; source .env; set +a
cargo run --release
open http://localhost:8080
```

## Configuration

All runtime config comes from env. A template lives in [`.env.example`](.env.example).

| Variable | Required | Purpose |
|---|---|---|
| `DISCORD_BOT_TOKEN` | yes | Bot auth. Needs `View Channel` + `Read Message History` on the channel; add `Add Reactions` if using auto-react. |
| `DISCORD_CHANNEL_ID` | no (default: Mayo Jaune) | Which channel to fetch from and react to. |
| `DISCORD_GUILD_ID` | no (default: Mayo Jaune) | Only used to build the "↗ open in Discord" hyperlink for the selected message. |
| `DISCORD_TO_INSTA_IMAGES_DIR` | no (default: `images`) | Where the `*_{km}.png` post templates live. |
| `DISCORD_TO_INSTA_STATE_PATH` | no (default: `$XDG_CONFIG_HOME/discord_to_insta/state.json`) | Auto-react state file. |
| `PORT` | no (default: 8080) | HTTP listen port. |

## Post-image convention

Drop per-distance template images into `images/`. The filename must end with `_{integer_km}.{png|jpg|jpeg}` — everything before the underscore is free-form, so you can drop a `mayo-post-ok-v7-2026-feed_20.png` redesign next to the `v6` one without touching code. The server globs `images/` for any file matching the suffix of the message's parsed distance.

Decimals round to the nearest integer (`19.7 km` → picks `_20.*`; `19.5 km` → `_20.*`; `18.4 km` → `_18.*`).

## Auto-react bot

Tick **auto-react** in the topbar to start the poller. Every 30 s it fetches the 50 most recent channel messages and compares them against the last-reacted snowflake ID stored in `state.json`:

- **First run for a channel:** silently record the newest message ID — no retroactive reactions.
- **Subsequent runs:** react with ✅ 🚫 🤔 (in that order) to anything newer, oldest-first so a mid-batch crash resumes cleanly.
- **Errors** (rate limit, transient network, permission denied) land in the log tail at the bottom of the page; the poller keeps going and retries next cycle.

The three emojis are hardcoded to match the Mayo Jaune announcement template's reaction legend. Edit `REACTION_EMOJIS` in `src/main.rs` if your template uses a different set.

## Security note

**The web UI has no authentication.** Anyone who can reach the port can trigger fetches, start/stop the poller, and view the message list. The bot token is never exposed over the UI (read from env only, never surfaced in `/api/config`), but:

- Bind the container to a trusted network. The default compose file publishes `0.0.0.0:9010` — that's any interface. If your host is internet-facing, put it behind a reverse proxy with auth, or change the publish to `127.0.0.1:9010:8080`.
- The bot token in `.env` is a secret. The repo gitignores `.env`, and the docker image is built with a `.dockerignore` that also excludes it.

## Caption transform — known limits

The current rules handle the Mayo Jaune template cleanly. Cases that **are not yet covered** and will produce imperfect output if your announcements use them:

1. **Custom Discord emojis** (`<:name:123>`, `<a:name:123>`) appear verbatim in the caption.
2. **Markdown syntax** (`**bold**`, `*italic*`, `__underline__`, `` `code` ``, `> quote`, headings) — Instagram doesn't render any of it, so the delimiters show through.
3. **Rich embeds.** If an announcement is posted with `embeds[]` instead of plain `content`, the transform has nothing to work with.

Open an issue or extend `src/transform.rs` if any of these bite.

## Deployment

A GitHub Actions workflow in [`.github/workflows/deploy.yml`](.github/workflows/deploy.yml) SSHes into a target host, runs `git pull --ff-only`, then `docker compose up -d --build`. It triggers on pushes to `master` / `main` and can also be run manually from the Actions tab.

One-time server setup:

1. Clone the repo to the path you'll use as `DEPLOY_PATH`.
2. Check out the deploy branch (`master` or `main`).
3. Install docker + docker compose plugin.
4. Create `.env` next to `docker-compose.yml` with at least `DISCORD_BOT_TOKEN`.
5. Ensure the deploy user can `docker compose` without sudo.

Repository secrets (Settings → Secrets and variables → Actions):

| Secret | Required | Notes |
|---|---|---|
| `SSH_HOST` | yes | Hostname or IP. |
| `SSH_USER` | yes | Remote user with write access to `DEPLOY_PATH` and permission to run `docker compose`. |
| `SSH_PRIVATE_KEY` | yes | Full private key including the `-----BEGIN/END-----` lines. Matching public key must be in the target's `authorized_keys`. |
| `DEPLOY_PATH` | yes | Absolute path to the cloned repo on the target. |
| `SSH_PORT` | no | Defaults to `22`. |

The workflow is `concurrency: deploy` — two pushes won't deploy in parallel. It also runs `docker image prune -f` after the rebuild to avoid disk bloat from accumulated old image layers.

## Development

```bash
cargo build       # dev build
cargo test        # 21 tests across transform, discord, images, state
cargo run         # bind to http://localhost:8080
```

`cargo clippy -- -D warnings` before committing. Every change lands with a [CHANGELOG](CHANGELOG.md) entry and its own git commit — see the "Commit Discipline" and "Changelog Discipline" sections in [CLAUDE.md](CLAUDE.md).

## Layout

```
src/
  main.rs       axum server + tokio-spawned auto-react poller
  index.html    the whole frontend (htmx, inline CSS)
  transform.rs  pure discord_to_caption(raw, user_map) — unit-tested
  discord.rs    async reqwest-backed Discord REST client
  images.rs     distance parsing + image-file lookup
  state.rs      persistent last-reacted-id map (JSON)
images/         per-distance post templates (*_{km}.png)
Dockerfile      two-stage build, runs as non-root uid 10001
docker-compose.yml   publishes 9010:8080, bind-mounts images, named state volume
.env.example    template for the env vars the app reads
```

## License

Not yet chosen. Treat as "all rights reserved" until a license file lands.
