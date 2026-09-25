# syntax=docker/dockerfile:1.7
#
# The gateway image. Two stages: a Rust toolchain that builds one static-ish binary, and a
# distroless runtime that holds that binary and nothing else: no shell, no package
# manager, no compiler, running as a user that is not root.
#
#   docker build -t llmr .
#   docker run -p 8080:8080 -v ./llmr.toml:/etc/llmr/llmr.toml:ro \
#     -e LLMR_API_KEYS=... -e ANTHROPIC_API_KEY=... llmr

# The same compiler `rust-toolchain.toml` pins, named here because that file is not copied:
# it asks rustup for clippy and rustfmt, which a build has no use for.
ARG RUST_VERSION=1.98.0

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts rather than a dependency-only layer: the crate reads its model tables and
# README at compile time, so a stub `src/` would not build, and the cache survives a
# source change just as well.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features server --bin llmr \
 && install -D target/release/llmr /out/llmr

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/llmr /usr/local/bin/llmr
COPY llmr.example.toml /etc/llmr/llmr.toml
ENV LLMR_CONFIG=/etc/llmr/llmr.toml
EXPOSE 8080
USER nonroot
# No curl in a distroless image, so the binary asks itself.
HEALTHCHECK --interval=15s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/llmr", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/llmr"]
CMD ["serve"]
