# syntax=docker/dockerfile:1
# Static musl binary on distroless/static (non-root, no shell).
#
# All compilation runs on the build host's architecture ($BUILDPLATFORM);
# arm64 is cross-compiled with cargo-zigbuild (zig as the C compiler/linker for
# ring), so multi-arch builds need no QEMU emulation.

FROM --platform=$BUILDPLATFORM rust:1.99.1-alpine AS toolchain
ARG ZIG_VERSION=0.15.2
ARG ZIG_SHA256=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239
ARG CARGO_ZIGBUILD_VERSION=0.24.0
RUN apk add --no-cache musl-dev curl xz
RUN curl -fsSL -o /tmp/zig.tar.xz \
      "https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz" \
 && echo "${ZIG_SHA256}  /tmp/zig.tar.xz" | sha256sum -c - \
 && mkdir /opt/zig && tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1 \
 && rm /tmp/zig.tar.xz
ENV PATH="/opt/zig:${PATH}"
RUN cargo install cargo-zigbuild --version "${CARGO_ZIGBUILD_VERSION}" --locked \
 && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
WORKDIR /src

# `docker build --target check .` runs formatting, lints and unit tests (native).
FROM toolchain AS check
RUN rustup component add clippy rustfmt
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo fmt --check \
 && cargo clippy --release --locked --all-targets -- -D warnings \
 && cargo test --release --locked

FROM toolchain AS build
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
      amd64) echo x86_64-unknown-linux-musl ;; \
      arm64) echo aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac > /rust-target
COPY Cargo.toml Cargo.lock ./
# Cache dependencies separately from sources.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo zigbuild --release --locked --target "$(cat /rust-target)" \
 && rm -rf src
COPY src ./src
RUN touch src/main.rs \
 && cargo zigbuild --release --locked --target "$(cat /rust-target)" \
 && cp "target/$(cat /rust-target)/release/tiered-snapshots" /tiered-snapshots
# Fail the build if the binary's ELF e_machine does not match the image platform.
RUN machine="$(od -An -t u2 -j 18 -N 2 /tiered-snapshots | tr -d ' ')" \
 && case "${TARGETARCH}:${machine}" in \
      amd64:62|arm64:183) echo "ELF machine ${machine} ok for ${TARGETARCH}" ;; \
      *) echo "wrong ELF machine ${machine} for ${TARGETARCH}" >&2; exit 1 ;; \
    esac

FROM gcr.io/distroless/static-debian12:nonroot AS runtime
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/colaH16/tiered-snapshots" \
      org.opencontainers.image.description="CSI VolumeSnapshot CronJob with tiered retention" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /tiered-snapshots /tiered-snapshots
USER 65532:65532
ENTRYPOINT ["/tiered-snapshots"]
