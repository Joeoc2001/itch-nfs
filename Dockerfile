# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin itch-nfs \
    && cp target/release/itch-nfs /itch-nfs

FROM debian:bookworm-slim
# The build image already has the CA bundle; copying it avoids an apt layer.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN useradd --system --uid 10001 --home-dir /nonexistent itch-nfs \
    && install -d -o itch-nfs -g itch-nfs /archive /cache
COPY --from=build /itch-nfs /usr/local/bin/itch-nfs

# /archive: everything the scanner has downloaded. Durable; may be an object
#   store mount. The scanner (`scan`) writes it, the frontend (`serve`) only
#   reads it and should get it read-only.
# /cache: local scratch space and caches. Disposable.
ENV ITCH_NFS_ARCHIVE_DIR=/archive \
    ITCH_NFS_CACHE_DIR=/cache \
    ITCH_NFS_LISTEN=0.0.0.0:11111
VOLUME ["/archive", "/cache"]
EXPOSE 11111
USER itch-nfs
ENTRYPOINT ["itch-nfs"]
CMD ["serve"]
