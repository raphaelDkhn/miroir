# Contributing

Bug reports, questions and pull requests are welcome.

- **Bugs:** open an issue with the Miroir version (`miroir --version`), the manifest or task
  involved, and what the build, the boot, the grader or `miroir check` printed.
- **Larger changes:** open an issue first, to agree on the approach before you write it.
- **Security issues:** report them privately, as [SECURITY.md](SECURITY.md) describes.

Contributions are licensed under [Apache-2.0](LICENSE), like the rest of Miroir.

## Layout

| Path | What it is |
|---|---|
| `src/main.rs` | The command line: six subcommands over the library, and their exit statuses |
| `src/bake.rs`, `src/run.rs`, `src/collect.rs` | The world image's build, boot and collect hook |
| `src/grade.rs`, `src/spec.rs`, `src/evidence.rs` | The grader, the grading spec and the evidence file |
| `src/check.rs`, `src/draft.rs` | The host commands: grader validation and calibration, and spec drafts |
| `src/manifest.rs`, `src/supervisor.rs`, `src/sinks.rs`, `src/journal.rs`, `src/listen.rs` | The manifest, the process supervisor, the SMTP and HTTP sinks, the effects and faults journals, and the boot's listener audit |
| `src/pg.rs`, `src/fold.rs`, `src/paths.rs` | Postgres capture and date shift, the fold of the change log into net changes, and Miroir's file paths |
| `Dockerfile`, `Dockerfile.release` | The `miroir` image (the grader, and the binary world images copy), from source or from release binaries |
| `main.Dockerfile` | The `miroir-main` image, the agent's container |
| `docker-bake.hcl` | The image targets: `miroir`, `main`, `acme-crm`, `acme-crm-env` and `gitea` |
| `fixtures/acme-crm/` | The test fixture: a Node.js CRM, its world image, 7 tasks and `check.yaml` |
| `examples/gitea/` | Stock Gitea as three worlds, and the generator of their tasks |
| `tests/integration/` | Docker integration tests: boot, the collect and grade pipeline, change capture |
| `tests/e2e.sh` | The Harbor end-to-end gate on the fixture |
| `docs/` | The Mintlify site; its `.md` pages are also the skill's references |
| `skills/miroir/`, `.claude-plugin/marketplace.json` | The skill for coding agents, and its Claude Code plugin |

## Build

```bash
docker buildx bake
```

That builds `miroir:dev`, `miroir-main:dev` and `acme-crm-env:dev`; the first build compiles
Miroir in release mode, which took 6 min 44 s on a 12-CPU Docker Desktop host. The `miroir`
binary is a static Linux build, so on macOS it runs from `miroir:dev`. The Gitea worlds build as
the [Gitea example's README](examples/gitea/README.md#build) describes.

## Checks

Run these before you push; CI runs them on every pull request. `prek run --all-files` runs the
repository's pre-commit hooks, which cover the formatters, the linters and the skill's
validation.

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The unit tests need no Docker. The integration tests and the end-to-end gate need the images
above; the gate also needs Harbor 0.26.0 (`uv tool install harbor==0.26.0`) and jq:

```bash
cargo test --locked --test integration -- --ignored
tests/e2e.sh
```

`tests/e2e.sh` checks the fixture's tasks, runs its `check.yaml` through Harbor and validates the
job with `miroir check`, regrades the job with a changed spec, and checks that a trial without
evidence voids. Its work stays under `jobs/` for inspection. `harbor lint` needs Docker Compose
2.35.0 or later (`config --no-env-resolution`): with an older Compose the script skips lint with
a message, or fails when `CI` is set.

The shell scripts and the Gitea example's Python, which runs on the world image's Python 3.12:

```bash
shellcheck tests/*.sh fixtures/acme-crm/tasks/*/solution/*.sh
shfmt -i 2 -d tests/*.sh fixtures/acme-crm/tasks/*/solution/*.sh
uvx ruff==0.17.3 check examples
uvx ruff==0.17.3 format --check examples
uv venv -q --allow-existing --python 3.12 .venv-examples
uv pip install -q --python .venv-examples pyyaml==6.0.3 pytest==9.2.0
uvx ty==0.0.96 check --error-on-warning --python .venv-examples \
  --extra-search-path examples/gitea/seed examples/gitea
.venv-examples/bin/pytest -q examples/gitea/seed
```

The Gitea gate, which validates the 120 generated tasks against the three worlds, is in the
[Gitea example's README](examples/gitea/README.md#validate-the-grader).

The modules that decide rewards and verdicts (`spec.rs`, `fold.rs`, `grade.rs`, `check.rs`,
`draft.rs`) are mutation-tested with cargo-mutants: a change to them comes with tests that leave
no mutant alive.

## Docs and the skill

The site is a [Mintlify](https://mintlify.com) project in `docs/`, published at
https://miroir.mintlify.site; `docs/docs.json` holds its navigation. Its `.md` pages (environment,
manifest, lifecycle, tasks, spec, evidence) are also the skill's references:
`skills/miroir/references/` holds symlinks to them, so a fact has one source. Keep them plain
Markdown, without Mintlify components, so agents read them as they are; the `.mdx` pages are for
the site only.

```bash
cd docs
npx mint@4.3.49 dev                                              # preview on localhost:3000
npx mint@4.3.49 validate
npx mint@4.3.49 broken-links --check-anchors --check-snippets
cd ..
uvx --from skills-ref==0.1.1 agentskills validate skills/miroir
claude plugin validate --strict .
```

A change to the manifest, the task format, the spec, the evidence or a command's output updates
the docs in the same pull request. The docs show only commands that were run, with their real
output; a command that pushes, publishes or spends money is marked as not run.

## Release

Set the version in `Cargo.toml`, merge, then push a tag `vX.Y.Z` on master. The release workflow,
`.github/workflows/release.yml`, runs CI, builds the static Linux binaries for amd64 and arm64
with their `SHA256SUMS`, and stops if the binary's version is not the tag's. It then pushes the
`ghcr.io/raphaeldkhn/miroir` and `ghcr.io/raphaeldkhn/miroir-main` images tagged `X.Y.Z` and
`X.Y`, and creates the GitHub release with the binaries.
