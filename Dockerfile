# syntax=docker/dockerfile:1.6

FROM node:24-alpine AS fe-builder

ARG POSTHOG_API_KEY=""
ARG POSTHOG_API_ENDPOINT=""

WORKDIR /app

ENV PNPM_HOME=/pnpm
ENV PATH=${PNPM_HOME}:${PATH}
ENV VITE_PUBLIC_POSTHOG_KEY=${POSTHOG_API_KEY}
ENV VITE_PUBLIC_POSTHOG_HOST=${POSTHOG_API_ENDPOINT}
ENV NODE_OPTIONS=--max-old-space-size=4096

RUN corepack enable
RUN pnpm config set store-dir /pnpm/store

COPY pnpm-lock.yaml pnpm-workspace.yaml package.json ./
COPY packages/local-web/package.json packages/local-web/package.json
COPY packages/ui/package.json packages/ui/package.json
COPY packages/web-core/package.json packages/web-core/package.json
COPY patches/ patches/

RUN --mount=type=cache,id=pnpm,target=/pnpm/store \
    pnpm install --frozen-lockfile

COPY packages/local-web/ packages/local-web/
COPY packages/public/ packages/public/
COPY packages/ui/ packages/ui/
COPY packages/web-core/ packages/web-core/
COPY shared/ shared/

RUN pnpm -C packages/local-web build

FROM rust:1.93-slim-bookworm AS builder

ARG POSTHOG_API_KEY=""
ARG POSTHOG_API_ENDPOINT=""
ARG SENTRY_DSN=""
ARG VK_SHARED_API_BASE=""

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
ENV CARGO_TARGET_DIR=/app/target
ENV POSTHOG_API_KEY=${POSTHOG_API_KEY}
ENV POSTHOG_API_ENDPOINT=${POSTHOG_API_ENDPOINT}
ENV SENTRY_DSN=${SENTRY_DSN}
ENV VK_SHARED_API_BASE=${VK_SHARED_API_BASE}

WORKDIR /app

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    git \
    libclang-dev \
    libssl-dev \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

COPY rust-toolchain.toml ./
RUN cargo --version >/dev/null

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY assets/ assets/
COPY --from=fe-builder /app/packages/local-web/dist packages/local-web/dist

RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=workspace-target,target=/app/target \
    for var in POSTHOG_API_KEY POSTHOG_API_ENDPOINT SENTRY_DSN VK_SHARED_API_BASE; do \
      eval "[ -n \"\${$var}\" ]" || unset "$var"; \
    done \
 && cargo build --locked --release --bin server \
 && cp /app/target/release/server /usr/local/bin/server

# Coding agents are launched through npx, so the runtime needs Node.js.
FROM node:24-bookworm-slim AS runtime

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
    openssh-client \
    tini \
    wget \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --system --create-home --uid 10001 appuser

WORKDIR /repos

COPY --from=builder /usr/local/bin/server /usr/local/bin/server

# Pre-create the data dir so named volumes inherit appuser ownership.
RUN mkdir -p /repos /home/appuser/.local/share/vibe-kanban \
  && chown -R appuser:appuser /repos /home/appuser

USER appuser

ENV HOME=/home/appuser
ENV HOST=0.0.0.0
ENV PORT=3000
ENV PREVIEW_PROXY_PORT=3001

EXPOSE 3000 3001

VOLUME ["/home/appuser", "/repos"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/bin/sh", "-c", "wget --spider -q http://127.0.0.1:${PORT:-3000}/api/health"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/server"]
