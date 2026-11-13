# Add Miroir to the image

An env is the product's own image with three things added: the `miroir` binary, a manifest and
a tasks directory. Miroir runs every process of the product itself, so the datastores go in the
same image as the apps.

## Dockerfile

```dockerfile
FROM ghcr.io/raphaeldkhn/miroir:0.1 AS miroir

FROM your-product-image
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-15 tini \
 && rm -rf /var/lib/apt/lists/*
ENV PATH=/usr/lib/postgresql/15/bin:$PATH
COPY --from=miroir /miroir /usr/local/bin/miroir
COPY miroir.yaml /miroir/miroir.yaml
COPY tasks/ /miroir/tasks/
RUN install -d -o postgres -g postgres /var/lib/postgresql/data \
 && su postgres -c "initdb -D /var/lib/postgresql/data --auth=trust" \
 && miroir bake
EXPOSE 8000
ENTRYPOINT ["tini", "--", "miroir"]
CMD ["run"]
```

`ghcr.io/raphaeldkhn/miroir` holds one static binary, `/miroir`, tagged `X.Y.Z` and `X.Y`.
Initialize each datastore's data directory, owned by the user it runs as, before `miroir bake`
seeds the data the [manifest](manifest.md) describes.
[`fixtures/acme-crm/Dockerfile.miroir`](https://github.com/raphaelDkhn/miroir/blob/master/fixtures/acme-crm/Dockerfile.miroir)
is a full example. Build it like any image:

```sh
docker build -f Dockerfile.miroir -t my-env .
```

## Run Miroir under an init

A reset kills every process in the container but Miroir and PID 1, so Miroir refuses to run
unless it is the entrypoint. Use `tini` (or `docker run --init`, `dumb-init`, `catatonit`). If
Miroir itself is PID 1, it runs but warns: the orphans a reset leaves are never reaped.

## Run processes as non-root users

Give every process a `user` in the manifest. A process that runs as root fails
[`check`](check.md), since it could read the tasks' checks and golden runs. Non-root users also
let Miroir find the state to restore on its own.

## Report the product's version

Set `MIROIR_ENV_VERSION` in the image, such as to the product image's tag. `/metadata` reports
it, so you know which build an episode ran on.

```dockerfile
ENV MIROIR_ENV_VERSION=ghcr.io/acme/crm:1.4.0
```
