# syntax=docker/dockerfile:1.7

# --- build stage ----------------------------------------------------------
FROM rust:1.92-slim-bookworm AS builder

WORKDIR /build
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Prime the dependency cache by building a throwaway crate with our manifest.
# Any change to src/** invalidates only the final layer, not this one.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release \
 && rm -rf src

# Real build.
COPY src ./src
RUN touch src/main.rs \
 && cargo build --release \
 && strip target/release/discord_to_insta

# --- runtime stage --------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /app --create-home app

WORKDIR /app
COPY --from=builder /build/target/release/discord_to_insta /usr/local/bin/discord_to_insta

# Pre-create /app/state with the correct ownership so that when docker (or
# compose) populates the named volume on first run, it inherits these perms.
# Without this the volume mounts as root-owned and the non-root `app` user
# can't write state.json → "Permission denied (os error 13)".
RUN mkdir -p /app/state && chown app:app /app/state

# Defaults assume the compose file mounts images/ at /app/images and the state
# file at /app/state/state.json. Override via env as needed.
ENV PORT=8080 \
    DISCORD_TO_INSTA_IMAGES_DIR=/app/images \
    DISCORD_TO_INSTA_STATE_PATH=/app/state/state.json

USER app
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/discord_to_insta"]
