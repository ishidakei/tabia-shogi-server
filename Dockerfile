# tabia-shogi-server, built to a slim non-root runtime image.
#
# Nothing about a deployment is baked in: no configuration, no position
# collection, no certificate, no key. The image is the same for every
# deployment, and the files mounted over /etc/tabia are the deployment.
#
#   docker build -t tabia-shogi-server .
#   docker run --rm -p 4081:4081 -v "$PWD/config:/etc/tabia:ro" tabia-shogi-server

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
# rust-toolchain.toml is this project's single source of truth for the
# toolchain, and this tag must stay equal to the channel it pins. The file is
# copied into the build as well, so cargo honours the pin
# rather than whatever the base image happens to carry; keeping the tag in step
# is what makes that a no-op instead of a download.
FROM rust:1.98.0-bookworm AS builder

WORKDIR /build

# Only what a release build needs. .dockerignore is written as an allow-list so
# that this stays a short list as the tree grows, and these are its entries.
#
# migrations/ and templates/ are build inputs, not runtime ones: `sqlx::migrate!`
# embeds the first and askama compiles the second, so both travel inside the
# executable and the runtime stage below needs neither.
#
# tests/ is here for a different reason, and the reason is worth writing down
# because the line looks gratuitous otherwise: this stage runs `cargo build
# --release`, which compiles no test. But Cargo.toml declares one test target
# explicitly —
#
#     [[test]]
#     name = "panic_containment"
#     required-features = ["fault-injection"]
#
# — and cargo refuses to *parse* a manifest whose declared target file it cannot
# find, before it has decided which targets to build. Without this line the
# build fails at manifest parsing with "can't find `panic_containment` test at
# `tests/panic_containment.rs`", not at compilation.
#
# Copying the directory is the fix rather than dropping the declaration and
# gating that file from the inside with a crate-level `cfg`. The declaration is
# what keeps a default `cargo test` from compiling a file whose every line would
# sit behind one `cfg` — the reasoning is written beside it in Cargo.toml — and
# it is also the shape that survives a second declared target, where an
# inside-gated file works only for as long as nobody declares anything. Nothing
# from tests/ reaches the runtime image: the stage below copies one binary.
#
# `tests/docker_context.rs` fails when a target Cargo.toml declares is not
# admitted by .dockerignore or not covered by one of these COPY lines, so a
# declaration added without a COPY beside it is caught before the build is.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY tests ./tests

# --locked: build the dependency versions Cargo.lock records, and fail rather
# than quietly resolve a different set. An image is supposed to be an artifact
# of a revision, not of the day it was built.
RUN cargo build --release --locked

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
# Debian bookworm on both sides: the binary is linked against the builder's
# glibc, and a runtime base of another vintage is how that goes wrong.
FROM debian:bookworm-slim

# ca-certificates and nothing else. The CSA listener presents the operator's
# mounted certificate and verifies nobody, so this is not for the listener; it
# is that a slim base carries no trust store at all, and an image with none
# cannot make an outbound TLS connection should anything here ever need one.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# A dedicated unprivileged account, ids fixed rather than assigned, so that the
# host side of a mount can be made readable to a uid known before the image is
# built.
RUN groupadd --system --gid 10001 tabia \
    && useradd --system --uid 10001 --gid 10001 --no-create-home \
        --shell /usr/sbin/nologin tabia

COPY --from=builder \
    /build/target/release/tabia-shogi-server \
    /usr/local/bin/tabia-shogi-server

# The conventional mount point, present and empty in the image. A deployment
# that forgot its mount then fails on the missing configuration file, naming it,
# rather than on a missing directory.
RUN install -d -o root -g root -m 0755 /etc/tabia

# Numeric on purpose: an orchestrator that has to decide whether this image runs
# as root can read the id, and does not have to resolve a name inside it.
USER 10001:10001

# The CSA listener's conventional port, and the web half's. The configuration
# decides the real ones — these document the convention and give `docker run -P`
# something to map. 8080 is only bound when the configuration writes `[web]`; an
# instance without one simply has nothing listening behind it.
EXPOSE 4081
EXPOSE 8080

# The binary takes exactly one argument, the configuration path
# (`usage: tabia-shogi-server <config.toml>`), so the default command names the
# conventional mount point. An operator who mounts elsewhere passes that path as
# the command instead.
ENTRYPOINT ["/usr/local/bin/tabia-shogi-server"]
CMD ["/etc/tabia/config.toml"]
