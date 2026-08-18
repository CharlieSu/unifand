# rust:1.97.1-slim-trixie — explicit Debian generation (not the floating
# "-slim" alias, which silently rolls to whatever Debian is current stable
# and could drift ahead of the distroless runtime's glibc below).
FROM rust:1.97.1-slim-trixie@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS builder
WORKDIR /app

# Dependency-cache layer: build a throwaway binary against a stub main so
# every crate in Cargo.lock compiles and gets cached here. Only edits to
# Cargo.toml/Cargo.lock invalidate this layer; source-only edits reuse it.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release --locked && \
    rm -rf src

# Real source. touch main.rs so cargo's mtime fingerprinting doesn't treat
# the freshly-COPYed files as unchanged relative to the stub build above.
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# gcr.io/distroless/cc-debian13 (no version-specific tag upstream, only a
# floating default) — pinned by digest, refresh via Renovate digest PRs.
FROM gcr.io/distroless/cc-debian13@sha256:ed7c407fd64eb0af9dddb9456b94cee188a40a7f53cf38c9836e1e9ae14fca02
COPY --from=builder /app/target/release/unifand /usr/local/bin/unifand
ENTRYPOINT ["/usr/local/bin/unifand"]
