# syntax=docker/dockerfile:1.7
#
# Headless org3d server for a NAS (e.g. Unraid).
#
#   docker build -t org3d .
#   docker run -d -p 3000:3000 -e TZ=America/Los_Angeles \
#     -e ORG3D_AUTH='me:a-long-password' \
#     --read-only --cap-drop ALL --security-opt no-new-privileges \
#     --memory 2g --pids-limit 256 \
#     -v /mnt/user/appdata/org3d:/config \
#     -v /mnt/user/3d-prints:/library:ro \
#     org3d
#
# /config  – database, thumbnails, preview cache. Put this on SSD/cache.
# /library – your 3MF/STL files. Read-only is fine unless you use "Extract ZIPs".

FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p org3d \
 && cp target/release/org3d /org3d \
 && mkdir /config-empty

# distroless/cc: glibc + libgcc only, no shell. SQLite is bundled in the binary.
FROM gcr.io/distroless/cc-debian12
COPY --from=build /org3d /usr/local/bin/org3d
# Owned by the runtime user so a fresh named volume is writable.
COPY --from=build --chown=99:100 /config-empty /config

ENV ORG3D_LIBRARY=/library \
    ORG3D_DB=/config/org3d.db \
    ORG3D_THUMBS=/config/thumbs \
    ORG3D_CACHE=/config/cache \
    ORG3D_CONFIG_DIR=/config \
    ORG3D_PORT=3000 \
    ORG3D_BIND=0.0.0.0 \
    RUST_LOG=org3d=info

# Unraid's nobody:users. Non-root by default; override with --user if your
# bind-mounted folders are owned by someone else. The binary only writes to
# /config (and /library for "Extract ZIPs"), so --read-only works.
USER 99:100

VOLUME ["/config"]
EXPOSE 3000

# Probes localhost only; never touches the library, so it can't wake disks.
HEALTHCHECK --interval=60s --timeout=5s --start-period=10s \
    CMD ["/usr/local/bin/org3d", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/org3d"]
# `run` scans in the background only if the DB is empty; after that, scans
# happen on demand (Settings → Rescan) or at ORG3D_RESCAN_AT.
CMD ["run"]
