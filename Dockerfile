# Cross-compiling builder: always runs on the BUILD platform's architecture
# (native speed — no QEMU emulation of the compiler) and targets
# TARGETPLATFORM's Rust triple. This is what keeps multi-arch release builds
# at native-build times; the runtime stage below only COPYs, so no target-arch
# code ever executes during the build.
#
# Explicit Debian generation ("-slim-trixie", not the floating "-slim" alias,
# which silently rolls to whatever Debian is current stable and could drift
# ahead of the distroless runtime's glibc below). Deliberately no version
# number in this comment: Renovate updates the FROM line, never the prose.
FROM --platform=$BUILDPLATFORM rust:1.98.0-slim-trixie@sha256:17d1ba895198f9934c6314ec5346a0d5115372f3243390c3d731e242f35c2f27 AS builder
ARG BUILDPLATFORM
ARG TARGETPLATFORM
WORKDIR /app

# Map the Docker platform to a Rust target triple, install the cross linker
# only when actually crossing, and register the rustup target.
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo x86_64-unknown-linux-gnu > /rust-target ;; \
      "linux/arm64") echo aarch64-unknown-linux-gnu > /rust-target ;; \
      *) echo "unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac && \
    if [ "$TARGETPLATFORM" != "$BUILDPLATFORM" ]; then \
      apt-get update && \
      case "$TARGETPLATFORM" in \
        "linux/arm64") apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu libc6-dev-arm64-cross ;; \
        "linux/amd64") apt-get install -y --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross ;; \
      esac && \
      rm -rf /var/lib/apt/lists/*; \
    fi && \
    rustup target add "$(cat /rust-target)"
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc

# Dependency-cache layer: build a throwaway binary against a stub main so
# every crate in Cargo.lock compiles and gets cached here. Only edits to
# Cargo.toml/Cargo.lock invalidate this layer; source-only edits reuse it.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release --locked --target "$(cat /rust-target)" && \
    rm -rf src

# Real source. touch main.rs so cargo's mtime fingerprinting doesn't treat
# the freshly-COPYed files as unchanged relative to the stub build above.
COPY src ./src
RUN touch src/main.rs && \
    cargo build --release --locked --target "$(cat /rust-target)" && \
    cp "target/$(cat /rust-target)/release/unifand" /unifand

# gcr.io/distroless/cc-debian13 (no version-specific tag upstream, only a
# floating default) — pinned by digest, refresh via Renovate digest PRs.
FROM gcr.io/distroless/cc-debian13@sha256:ed7c407fd64eb0af9dddb9456b94cee188a40a7f53cf38c9836e1e9ae14fca02
COPY --from=builder /unifand /usr/local/bin/unifand
ENTRYPOINT ["/usr/local/bin/unifand"]
