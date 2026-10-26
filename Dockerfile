# Bump the Rust and Alpine versions together; deno comes from Alpine's packages.
FROM rust:1.99-alpine3.24 AS builder
# `dev` for a faster, unoptimized debug build.
ARG PROFILE=release
WORKDIR /src
RUN apk add --no-cache build-base cmake
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY vendor ./vendor
RUN cargo build --profile "$PROFILE" --locked && \
    cp "target/$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE")/musicart" /musicart

FROM alpine:3.24
# Deno is packaged here because its releases are glibc-only. yt-dlp needs it for YouTube.
# yt-dlp itself is downloaded into data/bin on the first start, and kept current there.
RUN apk add --no-cache ca-certificates deno && \
    adduser -u 1000 -D musicart && \
    mkdir -p /app/data && chown musicart:musicart /app/data
COPY --from=builder /musicart /app/musicart
COPY LICENSE /app/LICENSE
WORKDIR /app
USER musicart
# musicart.toml goes in /app. data/ holds yt-dlp, the Spotify login and the cookies file.
VOLUME ["/app/data"]
ENTRYPOINT ["/app/musicart"]
