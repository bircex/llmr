# syntax=docker/dockerfile:1.7
#
# llmr, the product. Three stages: a Rust toolchain that builds one binary, an npm stage that
# installs the vendor command line tools, and a Node runtime that holds both, running as a
# user that is not root.
#
#   docker build -t llmr .
#   docker run -d -p 8080:8080 -v llmr-data:/var/lib/llmr \
#     -e LLMR_MASTER_KEY="$(docker run --rm llmr keygen)" llmr
#
# Providers, models and route sets are set through the management API and kept in
# /var/lib/llmr. Mount a volume there, or they are lost with the container.

# The same compiler `rust-toolchain.toml` pins, named here because that file is not copied:
# it asks rustup for clippy and rustfmt, which a build has no use for.
ARG RUST_VERSION=1.98.0
# Node runs Gemini CLI and Codex's launcher, and npm updates the tools at run time.
ARG NODE_VERSION=22

# The versions this release was tested with. The management API can install others onto the
# volume; these are what a fresh container runs, and what a reset goes back to.
ARG CLAUDE_CODE_VERSION=2.1.283
ARG CODEX_VERSION=0.157.1
ARG GEMINI_CLI_VERSION=0.61.0

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts rather than a dependency-only layer: the crate reads its model tables at
# compile time, so a stub `src/` would not build, and the cache survives a source change
# just as well.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features server --bin llmr \
 && install -D target/release/llmr /out/llmr

# One npm prefix per tool, the layout the gateway reads: /opt/llmr/cli/<tool>/bin/<program>.
FROM node:${NODE_VERSION}-bookworm-slim AS tools
ARG CLAUDE_CODE_VERSION
ARG CODEX_VERSION
ARG GEMINI_CLI_VERSION
ENV npm_config_update_notifier=false npm_config_fund=false npm_config_audit=false
RUN npm install --global --prefix /opt/llmr/cli/claude-code "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && npm install --global --prefix /opt/llmr/cli/codex "@openai/codex@${CODEX_VERSION}" \
 && npm install --global --prefix /opt/llmr/cli/gemini-cli "@google/gemini-cli@${GEMINI_CLI_VERSION}" \
 && /opt/llmr/cli/claude-code/bin/claude --version \
 && /opt/llmr/cli/codex/bin/codex --version \
 && /opt/llmr/cli/gemini-cli/bin/gemini --version \
 && npm cache clean --force

FROM node:${NODE_VERSION}-bookworm-slim
# ca-certificates for the tools that read the system's certificate store. No init such as
# tini: llmr is its own, because one started with llmr's environment could be read by the
# tools (see src/bin/llmr/init.rs). uid 65532, the uid earlier images ran as, so an existing
# volume stays writable.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 65532 llmr \
 && useradd --uid 65532 --gid 65532 --no-create-home --home-dir /var/lib/llmr --shell /usr/sbin/nologin llmr \
 && install -d -o 65532 -g 65532 /var/lib/llmr
COPY --from=tools /opt/llmr/cli /opt/llmr/cli
COPY --from=build /out/llmr /usr/local/bin/llmr
ENV LLMR_DATA_DIR=/var/lib/llmr \
    LLMR_CLI_DIR=/opt/llmr/cli
VOLUME ["/var/lib/llmr"]
EXPOSE 8080
USER 65532:65532
HEALTHCHECK --interval=15s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/llmr", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/llmr"]
CMD ["serve"]
