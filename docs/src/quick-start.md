# Quick start

Build the example env, check it, serve it and run one episode by hand. You need Docker with
buildx, and [uv](https://docs.astral.sh/uv/) for the Python part.

## Build and check

From a clone of the repository:

```sh
docker buildx bake acme-crm-env
docker run --rm --network none acme-crm-env check
```

`acme-crm-env` is Acme CRM, a small sales CRM in
[`fixtures/acme-crm`](https://github.com/raphaelDkhn/miroir/tree/master/fixtures/acme-crm),
with Postgres, Redis, a web server, a worker and an MCP server. `check` replays every task's
reference run and ends with `PASSED`.

## Serve

```sh
docker run --rm -d -p 127.0.0.1:8000:8000 -e MIROIR_TOKEN=secret acme-crm-env
curl http://127.0.0.1:8000/health
```

`/health` answers 200 once the env is ready. Every other route needs the token:

```sh
curl -H 'Authorization: Bearer secret' http://127.0.0.1:8000/acme-crm/splits
curl -H 'Authorization: Bearer secret' -H 'Content-Type: application/json' \
  -d '{"split": "train"}' http://127.0.0.1:8000/acme-crm/tasks
```

## Run an episode

Install the client:

```sh
uv venv && source .venv/bin/activate
uv pip install ./python
```

Then play the `pay-invoice` task in Python:

```python
from miroir import Session

session = Session.lease(["http://127.0.0.1:8000"], token="secret", wait_busy=False)
start = session.reset("pay-invoice")
print(start["instruction"])

step = session.step("api_request", {"method": "POST", "path": "/invoices/4001/pay"})
print(step["observation"]["result"]["status"])

outcome = session.finish()  # submits, then fetches the episode's record
print(outcome["reward"], outcome["metadata"]["checks"])
session.close()
```

The reward is 1: the invoice is paid, the payment provider was called once, and nothing else
changed.

## Next

- [Concepts](concepts.md) explains the words used in these docs.
- [Add Miroir to the image](make/image.md) starts the guide to making an env for your product.
- [TRL](train/trl.md) and [verifiers](train/verifiers.md) train and evaluate on an env.
