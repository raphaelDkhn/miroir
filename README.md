# Miroir 🪞

**Turn a product into an agentic RL environment.**

Miroir packages a real application as a reproducible environment for training and evaluating agents.

Add a manifest and tasks to your product's Docker image. Miroir handles the rest: seeding data, resetting state between episodes, exposing APIs as tools through OpenEnv and MCP, and grading what the agent actually changed—including database rows, emails, and HTTP requests.

## Quick start

Requirements: Docker with buildx.

```bash
docker buildx bake acme-crm-env
docker run --rm --network none acme-crm-env check
```

Build an environment from your application and verify its tasks, golden runs, and probes.

[Serve the environment and run your first episode](docs/src/quick-start.md#serve).

## Documentation

- [Introduction](docs/src/introduction.md)
- [How Miroir works](docs/src/introduction.md#how-it-works)
- [Build an environment](docs/src/make/image.md)
- [Train and evaluate](docs/src/run/serve.md)
- [Gitea example](examples/gitea): unmodified Gitea, 144 tasks, 12 task families.

Documentation is built with mdBook: `mdbook serve docs`.

## Coding agents

Use the [Miroir skill](skills/miroir/SKILL.md) to let coding agents build environments, write tasks, debug checks, and run training.

**Claude Code**

```bash
/plugin install miroir --marketplace raphaelDkhn/miroir
```

**Other agents**

```bash
npx skills add raphaelDkhn/miroir
```

## License

[Apache-2.0](LICENSE)
