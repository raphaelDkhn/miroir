# syntax=docker/dockerfile:1
FROM rust:1.99.1-alpine3.22@sha256:9a744dc47d1ac2a7d17fc3cedfc8b4928b1696dfcba3274ab25bfb40988a9ff0 AS build
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
FROM alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf AS miroir
RUN apk add --no-cache bash
COPY --from=build /miroir /usr/local/bin/miroir
COPY --chmod=0755 <<'EOF' /tests/test.sh
#!/bin/bash
exec miroir grade
EOF
WORKDIR /tests
# Harbor's keepalive ignores SIGTERM; nothing in this container needs a graceful stop.
STOPSIGNAL SIGKILL
