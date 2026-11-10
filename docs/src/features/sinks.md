# Sinks

Products send emails and call outside services, such as a payment provider. In an env, the
sinks stand in for the outside world: a local SMTP server and HTTP server that record each
email and call as an effect, which checks can query.

```yaml
env:
  SMTP_URL: smtp://127.0.0.1:2525
  STRIPE_API_BASE: http://127.0.0.1:9000/stripe
sinks:
  smtp: 2525
  http: 9000
  responses:
    "POST /stripe/v1/payment_intents": {status: 200, json: {id: pi_test, status: succeeded}}
```

Point the product's outbound settings at the sinks, as the `env` above does. Both listen on
127.0.0.1.

## SMTP

The SMTP sink accepts every message, with or without login, and records it as an `email`
effect: `from`, `to`, `cc`, `bcc`, `subject`, `text` and `html`.

## HTTP

The HTTP sink records each request as an `http` effect: `method`, `path`, `query`, `headers`
(without `Authorization`) and `body`, parsed when it is JSON. It answers 200 and `{}`, unless a
canned response matches.

`responses` keys are `/path` or `METHOD /path`, and a trailing `*` matches a prefix. An exact
path beats a prefix, a longer prefix beats a shorter one, and a key with a method beats one
without. A response has a `status` (200 by default), a `json` or a `body`, and `headers`.

## Checking effects

```yaml
- name: emailed
  weight: 0.5
  db: episode
  sql: |
    SELECT count(*) = 1 FROM effects
    WHERE kind = 'email' AND json_extract(data, '$.to[0]') = 'marcus@acme.test'
      AND json_extract(data, '$.subject') = 'Deal 2001 is now yours'
```

The shapes of both effects are in the [episode database reference](../reference/episode-db.md).

## Limits

A message can be up to 25 MiB, with up to 1,000 recipients. An episode's effects can take up
to 1 GiB, past which the episode faults.

> [!WARNING]
> The sinks see only what the product sends them. Run the env on an internal network: other
> outbound calls are neither recorded nor blocked. Miroir warns at start when it can reach the
> internet.
