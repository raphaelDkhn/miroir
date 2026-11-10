# Develop Miroir

## Layout

| Path | What |
|---|---|
| `src/` | the `miroir` binary |
| `python/` | the client, the TRL and verifiers adapters, the host-side CLI |
| `sandbox/` | the image that runs `run_code` commands |
| `fixtures/` | example products and their envs |
| `tests/` | Docker integration tests, and OpenEnv conformance |
| `tools/` | reset timing and pool soak benchmarks |
| `docs/` | this book |

## Checks

```sh
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                                    # unit tests
cargo test --locked --test integration -- --ignored    # builds the fixture images; needs Docker
(cd python && uv run ruff check . && uv run ty check && uv run pytest -q)   # -m integration: Docker
(cd tests/openenv && uv run pytest -q)                 # OpenEnv conformance, on acme-crm-env
```

`docker buildx bake` builds the binary's image, the sandbox and every fixture env.

## The docs

The docs are an [mdBook](https://rust-lang.github.io/mdBook/) in `docs/`:

```sh
mdbook serve docs    # http://localhost:3000, rebuilt on save
mdbook build docs    # into docs/book
```
