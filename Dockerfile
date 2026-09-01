# conminer — multi-stage Alpine/musl build (§9)
#
# Stages:
#   base     shared toolchain layer (musl cross-compile deps for SQLite bundled)
#   dev      full dev/test image: cargo test, clippy, fmt, fuzz corpus, pty tools
#   builder  release build producing a single static musl `conminer` binary
#   runtime  final ~15 MB Alpine image with ser2net + eudev + the binary
#
# One image, four entrypoints (`conminer discoveryd|minerd|mcpd|ingest`), per §9.

# ---------------------------------------------------------------- base --------
FROM rust:1-trixie AS base

# rusqlite's bundled SQLite is compiled from C, so a C toolchain is required.
# libssl-dev/pkg-config are needed by hook transports (HTTP power/flash hooks).
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        gcc \
        make \
        pkg-config \
        libssl-dev \
        libsqlite3-dev \
        git \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy

ENV CARGO_HOME=/usr/local/cargo \
    CARGO_TERM_COLOR=always \
    RUSTFLAGS="-C target-feature=-crt-static" \
    OPENSSL_STATIC=1

WORKDIR /work

# ----------------------------------------------------------------- dev --------
# Test/development image. Source is bind-mounted by docker-compose.dev.yaml and
# the cargo registry/target dirs are named volumes, so rebuilds are incremental.
FROM base AS dev

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        socat \
        ser2net \
        udev \
        bash \
        curl \
        jq \
        # A REAL BROWSER for the dashboard tests. Server-side tests can prove a
        # route answers and that a control exists in the served HTML, but not
        # that the page's JavaScript actually builds it, wires the click, and
        # sends the request -- and the dashboard is where a human presses things.
        # Chromium headless is the smallest thing that executes the page for real.
        chromium \
        gzip \
        util-linux \
        strace \
        procps \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-nextest --locked --version 0.9.* 2>/dev/null || true

ENV CARGO_TARGET_DIR=/work/target-docker \
    CONMINER_TEST_MODE=1

CMD ["cargo", "test", "--workspace"]

# ------------------------------------------------------------- builder --------
FROM base AS builder

ARG PROFILE=release

# Dependency pre-build layer: copy only manifests so the (slow) dependency
# compile is cached independently of source changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/conminer-core/Cargo.toml       crates/conminer-core/Cargo.toml
COPY crates/conminer-testkit/Cargo.toml    crates/conminer-testkit/Cargo.toml
COPY crates/conminer-mcp/Cargo.toml        crates/conminer-mcp/Cargo.toml
COPY crates/conminer/Cargo.toml            crates/conminer/Cargo.toml
RUN mkdir -p crates/conminer-core/src crates/conminer-testkit/src \
             crates/conminer-mcp/src crates/conminer/src \
    && echo "" > crates/conminer-core/src/lib.rs \
    && echo "" > crates/conminer-testkit/src/lib.rs \
    && echo "" > crates/conminer-mcp/src/lib.rs \
    && echo "" > crates/conminer/src/lib.rs \
    && echo "fn main() {}" > crates/conminer/src/main.rs \
    && cargo build --profile "$PROFILE" --workspace \
    && rm -rf crates/*/src

COPY crates ./crates
COPY profiles.d ./profiles.d

# WHICH SOURCE THIS BINARY IS.
#
# The Cargo version cannot answer that: three nodes reported `0.2.0` while
# running three genuinely different builds, which is the exact shape of a lying
# surface -- a field that always agrees can never disagree when it matters. A
# fleet where one node proxies calls to another needs the real answer, because a
# behaviour difference between builds arrives looking like a misbehaving board.
#
# Computed by the deployer (`./cm build-id`) and passed in, so it is the same
# string on every architecture for the same source -- a binary hash would differ
# between the x86_64 lab hosts and the arm64 dev box for identical code.
ARG CONMINER_BUILD=unknown
ENV CONMINER_BUILD=$CONMINER_BUILD

# Force a rebuild of our own crates (their mtimes changed above).
RUN find crates -name '*.rs' -exec touch {} + \
    && cargo build --profile "$PROFILE" --bin conminer \
    && install -Dm0755 "target/$PROFILE/conminer" /out/conminer \
    && /out/conminer --version

# ------------------------------------------------------------- runtime --------
# Debian trixie for ser2net 4.6.4.
#
# Alpine stable has only ser2net 3.5.1, which cannot read the 4.x YAML conminer
# generates and serves exactly ONE client per port — it answers the second with
# "Port already in use", making the multi-consumer premise false and blocking
# both the dashboard consoles and `run_command`. The builder is Debian too, so
# there is one libc across the image: shipping the Alpine musl binary onto
# glibc failed to exec at all, reported as "No such file or directory".
FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ser2net \
        udev \
        ca-certificates \
        tini \
        # §F10: the host side of a zmodem pull. `pull_file` moves small files by
        # base64 through the shell; a 200 KB proc dump needs the protocol that
        # was designed for a serial line, and lrzsz is the implementation every
        # board's `sz` expects to be talking to.
        lrzsz \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r conminer \
    && useradd -r -g conminer -M -s /usr/sbin/nologin conminer \
    && usermod -aG dialout conminer

COPY --from=builder /out/conminer /usr/local/bin/conminer
# Power/boot-mode hooks. Installed on PATH so a controller profile can name them
# directly.
COPY tools/bantam-power /usr/local/bin/
RUN chmod +x /usr/local/bin/bantam-power
COPY profiles.d /etc/conminer/profiles.d
COPY conminer.toml /etc/conminer/conminer.toml

ENV CONMINER_CONFIG=/etc/conminer/conminer.toml \
    CONMINER_DATA=/var/lib/conminer \
    CONMINER_PROFILES=/etc/conminer/profiles.d \
    RUST_LOG=info

RUN mkdir -p /var/lib/conminer /run/conminer \
    && chown -R conminer:conminer /var/lib/conminer /run/conminer

VOLUME ["/var/lib/conminer"]
EXPOSE 8090 9090

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/conminer"]
CMD ["--help"]
