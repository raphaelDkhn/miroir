# Code sandbox

`run_code` lets the agent run bash, for work that would take many API calls, such as updating
two hundred rows. The code runs in a container of its own, the sandbox, and reaches the apps
only through Miroir, as the persona.

## Start a sandbox

The sandbox image is `ghcr.io/raphaeldkhn/miroir-sandbox`: Debian with `python3` (and
`requests`), `curl` and `jq`, running `miroir sandbox`. Put it and the env on a network of
their own, and point the env at it with `MIROIR_SANDBOX`:

```sh
docker network create --internal -o com.docker.network.bridge.inhibit_ipv4=true ep1
docker run -d --rm --name ep1-sandbox --network ep1 --network-alias sandbox \
  --read-only --tmpfs /work:exec,size=256m --tmpfs /tmp:exec,size=256m \
  --pids-limit 256 --memory 1g --security-opt no-new-privileges \
  ghcr.io/raphaeldkhn/miroir-sandbox:0.1
docker create --rm --name ep1 -p 127.0.0.1::8000 -e MIROIR_TOKEN \
  -e MIROIR_SANDBOX=http://sandbox:8000 my-env
docker network connect ep1 ep1
docker start ep1
```

[`python/tests/docker.sh`](https://github.com/raphaelDkhn/miroir/blob/master/python/tests/docker.sh)
does this for [pools](../run/pool.md). The env serves the sandbox's requests on port 8001.

Before it reports healthy, the env probes the sandbox. The sandbox must reach no port of the
env's but the proxy and the env's server, and must not share the env's network namespace.
Miroir warns if the sandbox can reach the internet.

## What the agent gets

Clients pick the `code` surface to get `run_code` in place of `api_request`. Inside the
sandbox, each app's API is at `$API_<APP>`, such as `$API_CRM` for the app `crm`:

```sh
curl -s "$API_CRM/deals?status=open" | jq length
```

Requests there act as the persona and take the paths `api_docs` lists. Each URL works only
during the call it was given to. The code's requests are recorded in `actions`, with `via` set
to `run_code`, and they count in grading like any other.

`run_code` returns `exit_code` and `output` (stdout and stderr together).

## Limits

| Limit | Value |
|---|---|
| one call | 60 s |
| output returned | 64 KiB |
| command | 1 MiB |
| a request or answer body | 8 MiB |
| all requests of an episode's code | 64 MiB, past which the episode faults |

Files the code writes in its workspace last until the episode ends. Processes stop when the
call returns.

## Check with a sandbox

Golden runs and probes that call `run_code` need a sandbox. Run the check with
[`sandbox/check.sh`](../make/check.md#with-a-sandbox).
