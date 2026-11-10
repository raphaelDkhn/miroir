# Worlds

A world is a seed snapshot. Most envs have one, `default`. List several to train on
different data with the same product:

```yaml
worlds: [north, south]
```

## Bake

Bake seeds each world in turn, each from the same unbaked state, and keeps a pristine copy of
each. The `bake` and `seed` commands get `MIROIR_WORLD`, so a seed script can branch on it:

```js
const seed = SEEDS[process.env.MIROIR_WORLD];
```

World names are lowercase letters, digits, `_` and `-`, starting with a letter or a digit.

## Tasks

With several worlds, every task names its world, and each reset restores that world:

```yaml
world: north
instruction: Products 4 and 9 are sold out. Set the stock of each to 25.
```

## Hold whole worlds out for test

With several worlds, a world can't have both `test` tasks and tasks of other splits. Keep some
worlds for test only, so a test score comes from data training never saw.

## Check one world

```sh
docker run --rm --network none my-env check --world north
```

A world can also come from a snapshot taken at a known time, such as an
[imported](../production/import.md) production dump. See
[captured worlds](dates.md#captured-worlds).
