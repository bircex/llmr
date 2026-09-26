# syntax=docker/dockerfile:1.7
#
# llmr, the product. Two stages: a Rust toolchain that builds one binary, and a distroless
# runtime that holds that binary and nothing else: no shell, no package manager, no
# compiler, running as a user that is not root.
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

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts rather than a dependency-only layer: the crate reads its model tables at
# compile time, so a stub `src/` would not build, and the cache survives a source change
# just as well.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features server --bin llmr \
 && install -D target/release/llmr /out/llmr \
 && mkdir -p /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/llmr /usr/local/bin/llmr
# Owned by the runtime user, so a named volume mounted here starts out writable.
COPY --from=build --chown=65532:65532 /out/data /var/lib/llmr
ENV LLMR_DATA_DIR=/var/lib/llmr
VOLUME ["/var/lib/llmr"]
EXPOSE 8080
USER nonroot
# No curl in a distroless image, so the binary asks itself.
HEALTHCHECK --interval=15s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/llmr", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/llmr"]
CMD ["serve"]
