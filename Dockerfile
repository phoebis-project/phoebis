# syntax=docker/dockerfile:1

# The builder cooks the dependency tree with cargo-chef so BoringSSL and the crates stay a cached layer until
# Cargo.lock or a manifest changes. The runtime is distroless, so the health check is the binary's own subcommand.

# node:26.11.1-trixie-slim, resolved 2026-10-22
FROM node:26.11.1-trixie-slim@sha256:193fe51b64e77981119c98c2002c9e32a70e2f006fb4d25068ce0558998917f0 AS node

FROM rust:1.96.1-trixie AS chef
# clang, cmake, libclang-dev, and pkg-config build BoringSSL and its bindings.
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang cmake libclang-dev pkg-config \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add clippy rustfmt \
 && cargo install cargo-chef --locked --version 0.1.78
# The node stage's Node and npm build the console. npm and npx are linked here because a COPY follows a link and
# lands npm-cli.js as a plain file, whose relative require then fails.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
# The recipe carries dummy sources and an empty build script, so no npm runs here.
RUN cargo chef cook --release --locked --recipe-path recipe.json --package phoebis-router
COPY . .
# The context leaves `.git` out, so the commit reaches the build only as this argument, in the environment of both
# steps below. It sits after the cook, since every RUN after an argument misses the cache when its value changes,
# and a new commit would otherwise cook the dependencies again.
ARG PHOEBIS_BUILD_COMMIT=unknown
# The console is its own step so a failed npm command fails the image: the crate's build script would catch
# the failure and embed a placeholder page in a binary that builds green.
RUN cd web && npm ci --no-audit --no-fund && npm run build
# PHOEBIS_SKIP_UI_BUILD=1 embeds the bundle the step above produced as it is, with no mtime judgement against the
# sources the context just copied. The data directory made here gives the runtime stage an empty source to copy,
# and that copy sets the owner and the mode.
RUN PHOEBIS_SKIP_UI_BUILD=1 cargo build --release --locked --package phoebis-router \
 && install -d -o 65532 -g 65532 -m 0700 /var/lib/phoebis

# gcr.io/distroless/cc-debian13:nonroot, resolved 2026-11-27
FROM gcr.io/distroless/cc-debian13@sha256:29ee70762214fe8190cea04ef1c0a84a8365879e047ce8f911ae5a5ab2355fa2
COPY --from=builder /src/target/release/phoebis /usr/local/bin/phoebis
COPY --from=builder /src/LICENSE /usr/share/doc/phoebis/LICENSE
COPY --from=builder --chown=65532:65532 --chmod=0700 /var/lib/phoebis /var/lib/phoebis
WORKDIR /var/lib/phoebis
ENV PHOEBIS_CONFIG=/etc/phoebis/config.yaml
USER 65532
EXPOSE 8081 8317 9464
ENTRYPOINT ["/usr/local/bin/phoebis"]
# The start interval probes every two seconds for the first ten, so a container is healthy seconds after it
# binds rather than a whole interval later; failures inside the start period do not count.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --start-interval=2s --retries=3 \
  CMD ["/usr/local/bin/phoebis", "health"]
