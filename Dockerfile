# syntax=docker/dockerfile:1
#
# Self-contained build: context = this dust-serve directory.
# Prerequisites on the host before building:
#   1. cd web && npm install && npm run build   → web/dist/
#   2. cargo build --release                    → target/release/dust-serve
#   3. (in dust/) cargo build --release --features clickhouse
#      then: cp dust/target/release/dust bin/dust
# Then: docker build -t dust-serve:latest .   (from inside dust-serve/)
#       docker compose up -d --build
#
# Frontend is built on the host (step 1) and COPYed in, not built in-image,
# because npm install inside the build container is unreliable in restricted
# networks (typescript version resolution fails).

# ---------- Runtime image (all artifacts built on host, copied in) ----------
FROM debian:bookworm-slim AS runtime
# Use a mirror reachable from inside the build container.
RUN sed -i 's|deb.debian.org|mirrors.tuna.tsinghua.edu.cn|g; s|security.debian.org|mirrors.tuna.tsinghua.edu.cn|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null \
    || sed -i 's|deb.debian.org|mirrors.tuna.tsinghua.edu.cn|g; s|security.debian.org|mirrors.tuna.tsinghua.edu.cn|g' /etc/apt/sources.list 2>/dev/null \
    || true
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    libc6 \
    libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Pre-built binaries from the host.
COPY bin/dust /app/dust
COPY target/release/dust-serve /app/dust-serve

# Pre-built frontend from the host (web/dist).
COPY web/dist /app/web/dist

# ClickHouse setup SQL (run by the compose ch-init service).
COPY setup.sql /app/setup.sql

# Config (overridable by env vars; see src/config.rs).
ENV LISTEN=0.0.0.0:8080 \
    WEB_DIR=/app/web/dist \
    SCAN_LOG_DIR=/app/scan_logs \
    DUST_PATH=/app/dust

EXPOSE 8080

ENTRYPOINT ["/app/dust-serve"]

