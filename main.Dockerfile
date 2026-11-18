# The agent's container (Harbor's main) for Miroir tasks: the tools shell agents expect, and
# nothing of Miroir's. tmux and asciinema save terminus-2 an apt-get on every trial.
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      asciinema bash ca-certificates curl git jq procps python3 tmux \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /workspace
# Harbor's keepalive ignores SIGTERM, and Harbor copies the artifacts out before it stops main.
STOPSIGNAL SIGKILL
