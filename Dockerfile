# syntax=docker/dockerfile:1
# Static musl binary on distroless/static (non-root, no shell).

FROM rust:1.99.1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Cache dependencies separately from sources.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# `docker build --target check .` runs formatting, lints and unit tests.
FROM build AS check
RUN rustup component add clippy rustfmt \
 && cargo fmt --check \
 && cargo clippy --release --locked --all-targets -- -D warnings \
 && cargo test --release --locked

FROM gcr.io/distroless/static-debian12:nonroot AS runtime
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/colaH16/tiered-snapshots" \
      org.opencontainers.image.description="CSI VolumeSnapshot CronJob with tiered retention" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /src/target/release/tiered-snapshots /tiered-snapshots
USER 65532:65532
ENTRYPOINT ["/tiered-snapshots"]
