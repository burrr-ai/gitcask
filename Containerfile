# gitcask container image — one authenticated server in front of an object store.
#
#   podman build -t gitcask -f Containerfile .
#   podman run --rm -p 8080:8080 \
#       -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
#       -v ./gitcask.toml:/etc/gitcask/gitcask.toml:ro \
#       -v gitcask-cache:/var/lib/gitcask \
#       gitcask
#
# The image carries git (upload-pack, repack, index-pack run as subprocesses),
# git-lfs, CA certificates and tini. Config comes from /etc/gitcask/gitcask.toml or
# GITCASK__SECTION__KEY environment overrides; the local cache (materialized repositories,
# a self-signed TLS cert) lives under /var/lib/gitcask and can be wiped at any time — the
# bucket is the only durable state.

# ---- 1. rust build ------------------------------------------------------------------------
FROM docker.io/library/rust:1.97-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev pkg-config cmake perl python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
ARG GITCASK_BUILD_SHA=dev
ENV GITCASK_BUILD_SHA=${GITCASK_BUILD_SHA}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p gitcask-cli \
    && install -D target/release/gitcask /out/bin/gitcask \
    && install -D target/release/gitcask-server /out/bin/gitcask-server

# ---- 2. runtime -----------------------------------------------------------------------------
# trixie ships git 2.47+: gitcask wants >= 2.47 on the server
# (`pack.writeReverseIndex`, `index-pack --rev-index`); clients need >= 2.46.
FROM docker.io/library/debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends git git-lfs ca-certificates tini curl \
    && rm -rf /var/lib/apt/lists/* \
    && git --version
RUN useradd --uid 1000 --create-home --shell /bin/sh gitcask \
    && mkdir -p /etc/gitcask /var/lib/gitcask && chown gitcask:gitcask /var/lib/gitcask
COPY --from=build /out/bin/gitcask /out/bin/gitcask-server /usr/local/bin/
COPY gitcask.example.toml /etc/gitcask/gitcask.toml
COPY gitcask.standalone.toml /etc/gitcask/gitcask.standalone.toml
ENV RUST_LOG=info,gitcask=debug \
    GITCASK_CONFIG=/etc/gitcask/gitcask.toml \
    GITCASK__CACHE__DIR=/var/lib/gitcask \
    GITCASK__SERVER__LISTEN=0.0.0.0:8080
USER gitcask
WORKDIR /home/gitcask
EXPOSE 8080
VOLUME ["/var/lib/gitcask"]
HEALTHCHECK --interval=30s --timeout=5s CMD curl -fsS http://127.0.0.1:8080/readyz || exit 1
ENTRYPOINT ["tini", "--", "gitcask"]
CMD ["serve"]
