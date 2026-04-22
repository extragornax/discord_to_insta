# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Purpose

Repository: https://github.com/extragornax/discord_to_insta

`discord_to_insta` turns posts from a Discord announcement channel into an Instagram post, driven from a local UI. The tooling chain is therefore roughly:

1. **Ingest** — pull messages (and their attachments) from a specific Discord announcement channel.
2. **Compose** — render those messages into an Instagram-ready artifact (image/carousel + caption).
3. **Publish** — push the artifact to Instagram.
4. **Control** — a UI that lets the user pick which announcement to convert, preview the result, and trigger publishing.

When implementing features, keep these four stages as the mental model and avoid collapsing them — the Discord side and the Instagram side have very different rate limits, auth flows, and failure modes, so they should stay loosely coupled.

## Caption Transformation Rules

Derived from a worked example (a Mayo Jaune cycling-ride announcement). These rules are the contract the compose stage must satisfy:

| Discord input | Instagram output |
|---|---|
| `@everyone` | *(removed)* |
| `<@USER_ID>` | `@instagram_handle` — resolved via a user-maintained ID → handle map |
| `<#CHANNEL_ID>` | `voir Discord (lien en bio)` *(generic — treat all channel links the same way unless a future case contradicts this)* |
| Discord's relative-time suffix (`1d`, `2h`, `5m`, …) | *(removed)* |
| Trailing `Réactions :` block (from the literal line `Réactions :` to end of message) | *(removed)* |
| Everything else (body text, line breaks, emojis, typographic apostrophes) | Preserved verbatim |

Images attached to the Instagram post are **supplied by the user per post**, not extracted from the Discord message. The UI must accept an ordered image list (Instagram carousel order) alongside the captured announcement.

The user → handle map is the only piece of durable configuration the compose stage needs. Start with a plain file (TOML/JSON) keyed by Discord user ID; the UI can grow an editor later.

## Stack

- **Language:** Rust, edition 2024. Keep the dep list lean; every addition should be justifiable in the commit message.
- **Runtime:** `tokio` (multi-thread).
- **HTTP server:** `axum` 0.7 + `tower-http` (for serving `images/` via `ServeDir`).
- **Discord REST client:** `reqwest` 0.12 with `rustls-tls`. No gateway/WebSocket — pure REST polling.
- **Frontend:** single `src/index.html` served via `include_str!`, htmx 2 loaded from CDN, no JS build step, no framework. Handlers return HTML fragments (not JSON) that htmx swaps into the page.
- **No desktop GUI anymore.** Earlier commits used `eframe`/`egui`; those were removed when the UI moved to the browser.

## Web UI

- One binary, one process: the axum server and the auto-react poller share the tokio runtime. The poller is a `tokio::spawn`'d task, not a separate binary.
- **No authentication.** Anyone who can reach the port can trigger fetches and start/stop the poller. Bind to localhost or a trusted docker network; don't expose publicly without a reverse proxy that enforces auth.
- Bot token is **only** read from env — there is no UI field to set or view it. This is deliberate: the web UI never handles the secret.
- Port: `PORT` env var (default 8080).
- Endpoints:
  - `GET /` — the single-page app (htmx-driven).
  - `GET /api/config` — JSON with the configured `channel_id` + `guild_id` (never token).
  - `POST /api/fetch` — HTML fragment: the recent messages list.
  - `POST /api/preview` — HTML fragments (caption textarea + OOB image preview) from a form post containing `raw` + `handle_id_N`/`handle_user_N` pairs.
  - `POST /api/poller/toggle` (`enabled=1|0`), `GET /api/poller/status`, `GET /api/poller/log` — auto-react control + polling.
  - `GET /images/*` — static file serving from the configured images dir.

## Commands

Standard Cargo workflow — nothing project-specific yet:

```
cargo build              # compile
cargo run                # run the binary
cargo test               # run all tests
cargo test <name>        # run a single test by name substring
cargo clippy -- -D warnings   # lint (treat warnings as errors)
cargo fmt                # format
```

There is no custom build script, no workspace, and no CI configured at the moment. If you add any of those, update this file.

## Changelog Discipline

Every change to this repository — code, docs, config — must add an entry to `CHANGELOG.md` under `## [Unreleased]`, grouped by `Added` / `Changed` / `Fixed` / `Removed`, with an ISO date. Do not skip this, even for one-line fixes. When cutting a release, rename `[Unreleased]` to the version and start a fresh `[Unreleased]` block above it.

## Commit Discipline

Every logical change produces a git commit — there are no uncommitted slices at the end of a task. Pair each commit with its CHANGELOG entry (same scope, same wording). Before committing, run `cargo build` and `cargo test` — a red tree doesn't get a commit. Subject line is imperative mood, ≤ 70 chars (`add image auto-pick`, not `added image auto-pick`).

## Assets

`images/` holds the per-distance post templates (naming pattern `*_{km}.png`, where `{km}` is the integer kilometer distance, e.g. `mayo-post-ok-v6-2025-feed_20.png`). These are ~5 MB each and are currently committed to regular git history. If that ever becomes a pain (slow clones, bloated `.git`), migrate them to Git LFS rather than leaving them where they are.

The lookup is done by **suffix**, not by full filename — `src/images.rs::image_for_distance` globs the folder for any file whose stem ends in `_{km}`. So you can drop a `v7` redesign into `images/` without touching code; the only contract is the `_{integer_km}.{png|jpg|jpeg}` suffix.

Distance is parsed out of the Discord message via the regex in `src/images.rs`: it expects a literal `distance` (case-insensitive) followed by `:` or `=`, a number (`.` or `,` as decimal separator), and `km`. Non-integer distances are rounded to the nearest integer.

## Current State

As of this file's creation the repo contains only the Cargo scaffold and a `Hello, world!` `main.rs`. There is no existing architecture to preserve — greenfield decisions are on the table. Prefer growing the codebase in small, reviewable slices (ingest first, then compose, then publish, then UI) rather than landing the whole pipeline at once.

## Secrets & External Services

The project will need Discord bot credentials and Instagram Graph API credentials. Do not hardcode them. When you introduce config loading, read from environment variables or a gitignored config file, and document the expected variable names here.

### Discord

- Env vars (all read in `main()` before the server starts):
  - `DISCORD_BOT_TOKEN` — bot auth. **Env-only.** The web UI never exposes it.
  - `DISCORD_CHANNEL_ID` — the target announcement channel. Falls back to `DEFAULT_CHANNEL_ID`.
  - `DISCORD_GUILD_ID` — the guild the channel lives in. Surfaced in the topbar chip and used to build the "open in Discord" hyperlink for a selected message.
  - `DISCORD_TO_INSTA_IMAGES_DIR` (optional) — overrides the default `images/` path. Useful when running in a container with a volume mount.
  - `DISCORD_TO_INSTA_STATE_PATH` (optional) — overrides the default XDG state-file path.
  - `PORT` (optional) — HTTP listen port, default 8080.
- `.env` loading: `dotenvy::dotenv()` runs once at `main()` start. Missing `.env` is not an error (vars may come from docker-compose `env_file:` or the ambient environment). Ambient env wins over `.env`.
- Template lives at `.env.example`; `.env` is gitignored.
- Target channel defaults to `981806074233507880` (Mayo Jaune announcements, guild `981525647891525642`). It's only a UI default — the field is editable. Update `DEFAULT_CHANNEL_ID` in `src/main.rs` if the canonical channel ever changes.
- Ingestion is REST-only (`GET /channels/{id}/messages`) via `ureq`. No gateway, no tokio runtime — fetches run on a `std::thread` and stream results back through an `mpsc` channel so the egui event loop never blocks.
- The bot needs `View Channel` + `Read Message History` on the announcement channel. For the auto-react poller, it additionally needs `Add Reactions`. No privileged intents required since we're not using the gateway.

### Auto-react poller

- Lives in `run_poller` (`src/main.rs`) as a `tokio::spawn`'d task. Polls the channel every `POLL_INTERVAL` (30 s) via the same reqwest client used for web-triggered fetches. Interruptible: the `AtomicBool` stop flag is checked every 200 ms during sleep.
- On first run for a channel it **bootstraps** — it records the current newest message ID into `state.json` without reacting. Historical messages are never reacted to retroactively.
- "New" is determined by numeric comparison of Discord snowflake IDs (`state::is_newer_snowflake`). String comparison would misorder IDs of different lengths.
- The three emojis are hardcoded in `REACTION_EMOJIS` (`✅ 🚫 🤔`) to match the announcement template's reaction legend. If the template's emojis change, update this const.
- Persistent state: `$XDG_CONFIG_HOME/discord_to_insta/state.json` (falls back to `~/.config/…`). Schema: `{ last_reacted_by_channel: { channel_id: message_id } }`. Small by design; if it grows, split it.
- The poller **does not auto-start** on launch — the user must opt in via the `auto-react` checkbox in the topbar. This avoids accidentally reacting to a backlog after config changes. Because the web UI has no auth, be deliberate about who can reach the port.
