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
| `<:name:id>` / `<a:name:id>` (custom Discord emojis) | `:name:` |
| `**bold**`, `*italic*`, `__underline__`, `~~strike~~`, `||spoiler||`, `` `code` ``, ``` ```fenced``` ``` | inner text preserved, delimiters dropped |
| Line-start `# / ## / ### / -# / > / >>>` (headings, subtext, blockquotes) | prefix stripped, line content preserved |
| `<https://…>` (bracketed URLs, Discord's no-embed syntax) | unwrapped |
| Everything else (body text, line breaks, emojis, typographic apostrophes) | Preserved verbatim |

**Intentional non-rules:**
- Single-underscore italic `_foo_` is NOT stripped — it breaks `snake_case` file names and other literal underscores with no Discord-side benefit.
- Embed-only messages (empty `content`, data in `embeds[]`) are handled at the list-rendering stage, not in `discord_to_caption`. See `Message::synthesized_body()` in `src/discord.rs`.

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
- **Optional authentication.** Set `APP_PASSWORD` to gate the UI behind a login page. Sessions are random tokens stored in-memory (`AppCtx.sessions: HashSet<String>`); restart invalidates all sessions. The middleware (`auth_middleware`) passes through `/login` and `/images/*` (Meta must fetch images for publishing). When `APP_PASSWORD` is empty, all routes are open — bind to localhost or a trusted network in that case.
- Bot token is **only** read from env — there is no UI field to set or view it. This is deliberate: the web UI never handles the secret.
- Port: `PORT` env var (default 8080).
- Endpoints:
  - `GET /` — the single-page app (htmx-driven). Protected by auth when `APP_PASSWORD` is set.
  - `GET /login` — login page (public). `POST /login` — verifies password, sets `dti_session` cookie. `GET /logout` — clears session.
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
  - `APP_PASSWORD` (optional) — when set, the web UI requires this password to log in. Empty or unset = no auth.

## Docker

- `Dockerfile` is a two-stage build on `rust:1.92-slim-bookworm` → `debian:bookworm-slim`. A dummy-src layer primes the dep cache so `COPY src` only rebuilds the final binary (real app layer recompiles in ~7 s locally).
- `docker-compose.yml` publishes **`9010:8080`** — the external port is 9010, the container listens on 8080. The external port is a user-facing contract; change it in one place.
- `images/` is bind-mounted read-only from the host. The `state` named volume persists `/app/state/state.json` across restarts.
- `.env` is loaded via `env_file: { path: .env, required: false }` so it's optional.
- The container runs as a non-root user (`app`, uid 10001).
- Build: `docker compose build`. Run: `docker compose up -d`. Logs: `docker compose logs -f`.
- `.env` loading: `dotenvy::dotenv()` runs once at `main()` start. Missing `.env` is not an error (vars may come from docker-compose `env_file:` or the ambient environment). Ambient env wins over `.env`.
- Template lives at `.env.example`; `.env` is gitignored.
- Target channel defaults to `981806074233507880` (Mayo Jaune announcements, guild `981525647891525642`). It's only a UI default — the field is editable. Update `DEFAULT_CHANNEL_ID` in `src/main.rs` if the canonical channel ever changes.
- Ingestion is REST-only (`GET /channels/{id}/messages`) via `ureq`. No gateway, no tokio runtime — fetches run on a `std::thread` and stream results back through an `mpsc` channel so the egui event loop never blocks.
- The bot needs `View Channel` + `Read Message History` on the announcement channel. For the auto-react poller, it additionally needs `Add Reactions`. No privileged intents required since we're not using the gateway.

### Gateway (bot online presence + fast-path trigger)

- `src/gateway.rs` holds a Discord Gateway v10 WebSocket open for the lifetime of the process so the bot appears **online** in Discord.
- Subscribes to `GUILD_MESSAGES` (`IDENTIFY_INTENTS = 1 << 9`, not privileged) so `MESSAGE_CREATE` events arrive in real time. No `MESSAGE_CONTENT` intent — the event's channel_id + message id are enough to fire a trigger; we still fetch the body via REST.
- On `MESSAGE_CREATE` for `ctx.channel_id`, calls `ctx.poll_trigger.notify_one()`. The poller races that notification against its 30 s timer via `tokio::select!`, so reactions land within a few seconds of a new announcement. If the gateway drops, the timer is the safety net — reactions still happen within 30 s even without WebSocket events.
- Hand-rolled minimal client (`tokio-tungstenite` + `futures-util`). Handles HELLO → IDENTIFY → heartbeat → reconnect with exponential backoff capped at 60 s.
- Fatal close codes (4004 invalid token, 4010–4014 invalid shard/intents/api) stop the reconnect loop so a misconfigured deployment doesn't loop forever — the log shows `gateway: fatal, not reconnecting — …` and the status chip stays offline.
- Status surfaces via `GET /api/gateway/status` (HTML badge, polled every 5 s by the topbar chip).
- Skipped silently when `DISCORD_BOT_TOKEN` is empty, same principle as the poller.

### Auto-react poller

- Lives in `run_poller` (`src/main.rs`) as a `tokio::spawn`'d task. Polls the channel every `POLL_INTERVAL` (30 s) via the same reqwest client used for web-triggered fetches. Interruptible: the `AtomicBool` stop flag is checked every 200 ms during sleep.
- On first run for a channel it **bootstraps** — it records the current newest message ID into `state.json` without reacting. Historical messages are never reacted to retroactively.
- "New" is determined by numeric comparison of Discord snowflake IDs (`state::is_newer_snowflake`). String comparison would misorder IDs of different lengths.
- The three emojis are hardcoded in `REACTION_EMOJIS` (`✅ 🚫 🤔`) to match the announcement template's reaction legend. If the template's emojis change, update this const.
- **Per-emoji idempotence.** `state.reactions_done_by_channel` records, for each in-flight message, which emojis have already been placed. Checked before every `add_reaction` call so a crash mid-batch resumes without re-hitting the API. Entries are cleared once the full emoji set is in place (at which point `last_reacted_by_channel` advances past the message), so the map only holds currently-in-flight messages.
- **Paced calls.** `REACT_DELAY` (1 s) is slept between every `add_reaction` to stay well under Discord's message-reaction rate-limit bucket.
- Persistent state: `$XDG_CONFIG_HOME/discord_to_insta/state.json` (falls back to `~/.config/…`). Schema: `{ last_reacted_by_channel: { channel_id: message_id }, reactions_done_by_channel: { channel_id: { message_id: [emoji, …] } }, handles: { discord_user_id: instagram_handle } }`.
- The UI's Répertoire panel and the poller both write state.json. `AppCtx.state_write_lock` (a `tokio::sync::Mutex<()>`) serializes any load-mutate-save cycle so their changes can't clobber each other. Every caller must acquire it before `AppState::load` + `.save()`.
- `DEFAULT_HANDLES` in `src/main.rs` seeds three known mappings (bertrandbernager / extragornax / mithiriath) on first launch when `state.handles` is empty. Operator deletes are preserved — re-seed only fires when the whole map is empty again.
- The poller **auto-starts on launch** when `DISCORD_BOT_TOKEN` is set — the app is meant to keep reacting unattended (this is a docker-compose service, not a desktop tool). The UI checkbox still toggles it at runtime. Token empty → poller skipped silently to avoid a 401 log spam.

### Telegram approval gate

- `src/telegram.rs` is a minimal transport-only Bot API client (sendPhoto, sendMessage with inline keyboard, getUpdates long-polling, answerCallbackQuery, editMessageText). No bot framework, no event loop beyond a single tokio task.
- Flow: when the poller finishes its reactions for a new announcement, it spawns `run_approval()`, which posts the image + caption + `✅ Publier / ❌ Annuler` buttons to the configured group and waits on a `tokio::sync::oneshot` with a 2-hour timeout.
- The gate is **optional**: if `TELEGRAM_BOT_TOKEN` or `TELEGRAM_APPROVAL_CHAT_ID` is empty, `Config::telegram_enabled()` is false and approvals are skipped entirely. Partial configuration logs a warning at startup.
- Long-polling (`getUpdates` with a 25 s timeout) avoids the need for a public HTTPS URL for webhooks. One background task, one backoff-on-error loop, `allowed_updates: ["callback_query"]` so unrelated chat traffic is ignored.
- Pending approvals are in-memory (`AppCtx.pending_approvals: HashMap<discord_msg_id, oneshot::Sender<ApprovalOutcome>>`). Restart means pending approvals are lost; a subsequent button click is acked with "session expired" and the message is edited to reflect that.
- The UI has a manual **Envoyer pour approbation** button in § II Laboratoire that posts the current editor state to `/api/telegram/request`, letting operators test the flow without waiting for a new Discord announcement.
- **Instagram publishing is wired** (see "Instagram" section below): on `Approved + Publish` mode, `perform_publish_action` calls `instagram::Client::publish_photo(image_url, caption)`, which does the two-step container-then-publish Graph API dance and returns the `ig-media-id`. The id is persisted into `state.published_to_instagram` under the write-lock so a concurrent poller save can't clobber it. On `Approved + EditCaption` mode, the helper looks up the media id and calls `update_caption`. Instagram not configured → returns `Ok("Instagram non-configuré — aucune action réelle")` so the approval UX still works end-to-end for testing.

**Two approval modes.** `ApprovalMode::Publish` (new announcement → post to Instagram) and `ApprovalMode::EditCaption` (Discord message was edited → update Instagram caption in place, without deleting the post). Both travel through the same `run_approval()`; the mode parameterizes the Telegram intro text, button labels, and callback prefix (`approve:publish:{id}` / `approve:edit:{id}`). The callback router in `run_telegram_updates` parses the `verb:mode:id` format and keys `pending_approvals` as `{mode}:{id}` so publish and edit approvals for the same message can't collide.

**Edit detection.** `run_edit_watcher` consumes message IDs from an `UnboundedReceiver<String>` fed by (a) the gateway's `MESSAGE_UPDATE` handler and (b) the manual `POST /api/telegram/request_edit` endpoint. For each event it **first checks `state.published_to_instagram`** and skips silently if the message was never published (nothing to update), then fetches the updated message via `discord::Client::fetch_message` (we don't have the privileged MESSAGE_CONTENT intent, so REST is the source of truth), recomputes caption + image, and spawns an edit-mode approval. No filtering on what changed within the content — Discord also fires MESSAGE_UPDATE for embed resolves etc., so the approver may occasionally see spurious "update?" prompts; filter by content hash if this gets noisy.

### Instagram (publish + caption edit)

- `src/instagram.rs` is a transport-only Graph API client on `v21.0`. Three operations: `create_container(image_url, caption)`, `wait_for_container(creation_id)` (polls `status_code` up to ~60 s waiting for `FINISHED`, bailing on `ERROR`/`EXPIRED`), `publish_container(creation_id)` (returns the ig-media-id), plus a one-shot `update_caption(media_id, caption)`.
- Env vars (all three required; missing any one disables publishing but leaves the rest of the pipeline running):
  - `INSTAGRAM_ACCESS_TOKEN` — long-lived User or System User token with `instagram_business_content_publish`.
  - `INSTAGRAM_BUSINESS_ACCOUNT_ID` — the IG Business Account numeric ID (fetched via `/me/accounts` → page → `instagram_business_account`).
  - `PUBLIC_BASE_URL` — base URL where this service's `/images/*` is reachable from Meta's servers. Required because Meta fetches the image itself. Trailing slash is stripped.
- `/api/instagram/status` mirrors the Telegram status pattern, with tooltips naming the specific missing var.
- State: `state.published_to_instagram[discord_msg_id] = ig_media_id`. Written under `state_write_lock` immediately after a successful publish. Consulted by `run_edit_watcher` (skip-if-unpublished) and by `perform_publish_action`'s EditCaption branch (look up the media id to call `update_caption`).
- No retry logic beyond the container-ingest polling. Graph API errors (rate limit, bad token, caption too long, image unreachable) surface as a single log line + a `⚠️ Approuvé mais échec: …` edit on the Telegram approval message. Operators can fix and re-trigger via the manual "Envoyer pour approbation" button.
