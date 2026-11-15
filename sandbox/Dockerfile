# syntax=docker/dockerfile:1
ARG BASE_IMAGE=docker.io/library/debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
ARG MIROIR_IMAGE=ghcr.io/raphaeldkhn/miroir:0.0
FROM ${MIROIR_IMAGE} AS miroir

FROM ${BASE_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-requests curl jq tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home-dir /work --no-create-home --shell /usr/sbin/nologin code \
    && mkdir -m 0700 /work && chown code:code /work
COPY --from=miroir /miroir /usr/local/bin/miroir
EXPOSE 8000
ENTRYPOINT ["tini", "--", "miroir"]
CMD ["sandbox"]
