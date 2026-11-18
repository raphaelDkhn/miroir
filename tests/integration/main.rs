//! Docker integration tests: the fixture's world image booted, driven, collected and
//! graded in real containers, with the images `docker buildx bake` builds. They are ignored by
//! default: `cargo test --locked --test integration -- --ignored`.
#![cfg(test)]

mod boot;
mod capture;
mod common;
mod pipeline;
