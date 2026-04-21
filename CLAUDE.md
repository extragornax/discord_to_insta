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

- **Language:** Rust, edition 2024 (see `Cargo.toml`). `[dependencies]` is currently empty — any crate is a deliberate choice, so justify additions in commit messages.
- **UI:** not yet chosen. Before picking one, confirm the target (desktop GUI vs. local web UI) with the user; the right crate (`egui`/`iced`/`tauri`/`axum`+browser) depends on that answer.

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

## Current State

As of this file's creation the repo contains only the Cargo scaffold and a `Hello, world!` `main.rs`. There is no existing architecture to preserve — greenfield decisions are on the table. Prefer growing the codebase in small, reviewable slices (ingest first, then compose, then publish, then UI) rather than landing the whole pipeline at once.

## Secrets & External Services

The project will need Discord bot credentials and Instagram Graph API credentials. Do not hardcode them. When you introduce config loading, read from environment variables or a gitignored config file, and document the expected variable names here.

### Discord

- Env var: `DISCORD_BOT_TOKEN` — read at startup by `App::new()`. If unset, the UI exposes a masked password field as a fallback. The token is never persisted to disk.
- Target channel defaults to `981806074233507880` (Mayo Jaune announcements, guild `981525647891525642`). It's only a UI default — the field is editable. Update `DEFAULT_CHANNEL_ID` in `src/main.rs` if the canonical channel ever changes.
- Ingestion is REST-only (`GET /channels/{id}/messages`) via `ureq`. No gateway, no tokio runtime — fetches run on a `std::thread` and stream results back through an `mpsc` channel so the egui event loop never blocks.
- The bot needs `View Channel` + `Read Message History` on the announcement channel. No privileged intents required since we're not using the gateway.
