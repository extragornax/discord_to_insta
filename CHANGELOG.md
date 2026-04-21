# Changelog

All notable changes to this project are recorded here. Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); dates are ISO 8601.

## [Unreleased]

### Added
- `CLAUDE.md` with project purpose, stack, Cargo commands, and the caption transformation rules derived from the first worked example (Mayo Jaune ride announcement). — 2026-04-21
- `CHANGELOG.md` (this file) and a project convention that every change onward must add an entry here. — 2026-04-21
- `src/transform.rs`: pure `discord_to_caption(raw, user_map)` function implementing the caption rules (strip `@everyone`/`@here`, resolve `<@id>` via a user-supplied map, rewrite `<#id>` and `<@&id>` to `voir Discord (lien en bio)`, drop trailing relative-time suffix, drop `Réactions :` trailer). Includes a Mayo Jaune golden test plus four targeted unit tests. — 2026-04-21
- `src/main.rs`: first-pass `egui`/`eframe` desktop UI — two-pane layout (raw paste on the left, live caption preview on the right), a right side panel to edit the Discord-ID → Instagram-handle mapping, and a "Copy" button that pushes the caption to the system clipboard. — 2026-04-21
- `Cargo.toml`: added `eframe` 0.29 (glow + wayland + x11), `regex` 1, `once_cell` 1 as the minimum deps for the v1 slice. — 2026-04-21
- `src/discord.rs`: REST-only Discord client (`ureq` + `serde`) exposing `Client::fetch_messages(channel_id, limit)` with typed `Message` / `Author` / `User` / `Attachment` models. Error type maps 401/403/404/429 to specific variants so the UI can show actionable messages. Two parse tests cover the full and minimal JSON shapes. — 2026-04-21
- `src/main.rs`: wired Discord fetching into the UI — top config panel (masked token + channel ID + Fetch button + status line), left side panel listing fetched messages (date + author + preview) with click-to-load into the raw textarea. Fetching runs on a background thread via `mpsc`; egui stays responsive via `request_repaint_after`. Mentioned users from the selected message are auto-seeded into the handle table with blank handles. — 2026-04-21
- `Cargo.toml`: added `ureq` 2 (tls + json), `serde` 1, `serde_json` 1 for the Discord REST client. — 2026-04-21
- `src/main.rs`: pre-filled `DEFAULT_CHANNEL_ID = 981806074233507880` (Mayo Jaune announcements) so the channel field is usable out of the box. — 2026-04-21
- `CLAUDE.md`: documented repo URL (`github.com/extragornax/discord_to_insta`), the `DISCORD_BOT_TOKEN` env var contract, and the bot-permission requirements (View Channel + Read Message History). — 2026-04-21
- `CLAUDE.md`: added commit discipline (every logical change = one commit paired with its changelog entry, built-and-tested before commit) and documented the `images/` asset folder convention (`*_{km}.png`). — 2026-04-22
- `images/`: committed the v6 2025 post templates (distances 18–27 km) at ~5 MB each. Flagged a future migration to Git LFS in CLAUDE.md if history bloat becomes a problem. — 2026-04-22
- `src/images.rs`: distance parsing (`parse_distance_km` with tolerant regex — handles `Distance : 20km`, `distance: 19.7 km`, `Distance=18,4km`) and suffix-based image lookup (`image_for_distance` globs `images/` for any file whose stem ends in `_{km}` — PNG/JPG/JPEG). Nine unit tests covering the happy path, decimal rounding, comma decimals, and the `_20` vs `_200` false-match case. — 2026-04-22
- `src/main.rs`: auto-picks the post image from the currently-loaded announcement. Central panel shows `✓ 20 km → mayo-post-ok-v6-2025-feed_20.png` (or a yellow warning when a distance is found but no matching image exists), and a thumbnail preview on the right via `egui_extras` image loaders. — 2026-04-22
- `Cargo.toml`: added `egui_extras` 0.29 (image feature) and `image` 0.25 (png + jpeg) for inline thumbnail rendering. — 2026-04-22
