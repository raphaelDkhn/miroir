# syntax=docker/dockerfile:1
FROM rust:1.99.0-alpine3.22@sha256:c3a5ad77ff2e5ec99fffaf62cb518e61a9dcbd9e1d7526d281a380db0be3bb3b AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The cached target outlives this checkout, and COPY keeps the sources' mtimes, which cargo
# trusts: touching both crate roots rebuilds miroir's own code, while dependencies stay cached.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    touch src/lib.rs src/main.rs \
 && cargo build --release --locked \
 && cp target/release/miroir /miroir

# The grader and the binary donor: world images copy /usr/local/bin/miroir, and every task names
# this image as its separate verifier, whose /tests/test.sh grades.
FROM alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f AS miroir
RUN apk add --no-cache bash
COPY --from=build /miroir /usr/local/bin/miroir
COPY --chmod=0755 <<'EOF' /tests/test.sh
#!/bin/bash
exec miroir grade
EOF
WORKDIR /tests
# Harbor's keepalive ignores SIGTERM; nothing in this container needs a graceful stop.
STOPSIGNAL SIGKILL
