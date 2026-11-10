#!/usr/bin/env bash
# Run `miroir check` on IMAGE beside a sandbox, the two alone on an internal network.
set -euo pipefail

image=${1:?usage: sandbox/check.sh IMAGE [CHECK ARGS...]}
shift
name="miroir-check-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"

remove() {
	docker rm -f "$name-sandbox" >/dev/null 2>&1 || true
	docker network rm "$name" >/dev/null 2>&1 || true
}
trap remove EXIT

docker network create --internal -o com.docker.network.bridge.inhibit_ipv4=true "$name" >/dev/null
docker run -d --rm --name "$name-sandbox" --network "$name" --network-alias sandbox \
	"${SANDBOX_IMAGE:-miroir-sandbox}" >/dev/null
docker run --rm --network "$name" -e MIROIR_SANDBOX=http://sandbox:8000 "$image" check "$@"
