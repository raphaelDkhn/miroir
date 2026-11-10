# syntax=docker/dockerfile:1
FROM rust:1.99.0-alpine3.22 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    sh -c 'touch src/main.rs && cargo build --release --locked && cp target/release/miroir /miroir'

FROM scratch
COPY --from=build /miroir /miroir
ENTRYPOINT ["/miroir"]
