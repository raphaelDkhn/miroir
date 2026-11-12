# miroir

Turn a product's Docker image into a reinforcement-learning environment for agents.

Add a manifest and tasks to the image. Miroir runs the real product, restores the same data before
every episode, gives the agent the product's API as tools over OpenEnv and MCP, and grades the end
state: the rows that changed, and the emails and HTTP calls the product sent.

```yaml
# tasks/pay-invoice.yaml
instruction: The customer on invoice INV-04001 asked us to charge their card on file today.
checks:
  - {name: paid, weight: 1, sql: "SELECT status = 'paid' FROM invoices WHERE number = 'INV-04001'"}
  - {name: only_this_invoice, gate: true, db: episode,
     sql: "SELECT count(*) = 0 FROM changes WHERE tbl = 'invoices' AND json_extract(pk, '$.id') <> 4001"}
allowed_changes:                    # any other change is harm, and earns 0
  - {table: invoices, op: update, columns: [status, paid_at, payment_intent]}
golden:                             # a solution `miroir check` replays
  - {tool: api_request, arguments: {method: POST, path: /invoices/4001/pay}}
  - {tool: submit}
```

## Quick start

With Docker and buildx:

```sh
docker buildx bake acme-crm-env                        # a small CRM made an env, from fixtures/
docker run --rm --network none acme-crm-env check      # replay every task's solutions and probes
```

Then [serve it and play an episode](docs/src/quick-start.md#serve).

## Docs

- [Introduction](docs/src/introduction.md) and [how Miroir works](docs/src/how-it-works.md)
- [Make an env](docs/src/make/env.md) of your product
- [Train and evaluate](docs/src/train/serve.md) with TRL or verifiers
- [`examples/gitea`](examples/gitea): the stock Gitea image made an env, with 144 tasks and the
  measurements that show it grades right

The docs are an mdBook: `mdbook serve docs`.

## With a coding agent

[`skills/miroir`](skills/miroir/SKILL.md) teaches coding agents to make an env, write tasks, fix
`miroir check` and train. In Claude Code, `/plugin install miroir --marketplace raphaelDkhn/miroir`;
in other agents, `npx skills add raphaelDkhn/miroir`.

## License

[Apache-2.0](LICENSE)
