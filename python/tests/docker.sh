#!/usr/bin/env bash
# The command `miroir pool` runs: start an env container, with its sandbox when
# SANDBOX_IMAGE is set, or stop one.
set -euo pipefail

usage="usage: docker.sh IMAGE start | docker.sh IMAGE stop ID"
image=${1:?$usage}
label="miroir.pool=${MIROIR_POOL:-}"

remove() {
	local container
	for container in "$1" "$1-sandbox"; do
		if docker inspect --type container "$container" >/dev/null 2>&1; then
			docker rm -f "$container" >/dev/null
		fi
	done
	if docker network inspect "$1" >/dev/null 2>&1; then
		docker network rm "$1" >/dev/null
	fi
}

start_alone() {
	id=$(docker run -d --rm --label "$label" -p 127.0.0.1::8000 -e MIROIR_TOKEN "$image")
}

start_pair() {
	id="miroir-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
	if ! docker network create --internal -o com.docker.network.bridge.inhibit_ipv4=true \
		--label "$label" "$id" >/dev/null; then
		echo "cannot create network $id: Docker's default address pools hold about 30" \
			"networks, and each pair takes one; set default-address-pools in daemon.json" >&2
		exit 1
	fi
	if ! {
		docker run -d --rm --name "$id-sandbox" --label "$label" --network "$id" \
			--network-alias sandbox --read-only --tmpfs /work:exec,size=256m \
			--tmpfs /tmp:exec,size=256m --pids-limit 256 --memory 1g \
			--security-opt no-new-privileges "$SANDBOX_IMAGE" >/dev/null &&
			docker create --rm --name "$id" --label "$label" -p 127.0.0.1::8000 \
				-e MIROIR_TOKEN -e MIROIR_SANDBOX=http://sandbox:8000 "$image" >/dev/null &&
			docker network connect "$id" "$id" &&
			docker start "$id" >/dev/null
	}; then
		remove "$id"
		exit 1
	fi
}

case ${2:-} in
start)
	if [ -n "${SANDBOX_IMAGE:-}" ]; then
		start_pair
	else
		start_alone
	fi
	port=""
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		port=$(docker port "$id" 8000/tcp 2>/dev/null || true)
		[ -n "$port" ] && break
		sleep 0.5
	done
	if [ -z "$port" ]; then
		echo "container $id published no port for 8000" >&2
		remove "$id"
		exit 1
	fi
	echo "$id http://${port%%$'\n'*}"
	;;
stop)
	id=${3:?$usage}
	remove "$id"
	;;
*)
	echo "$usage" >&2
	exit 2
	;;
esac
