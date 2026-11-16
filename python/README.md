# miroir

The Python package of [Miroir](https://github.com/raphaelDkhn/miroir), which turns a product's
Docker image into a reinforcement-learning environment. It holds the client that leases
containers and runs episodes, the host CLI, and the TRL and verifiers adapters.

```sh
uv pip install miroir               # the client and the host CLI
uv pip install 'miroir[trl]'        # with the TRL adapter
uv pip install 'miroir[verifiers]'  # with the verifiers adapter
```

Start with the [quick start](https://github.com/raphaelDkhn/miroir#quick-start), then the
[client guide](https://raphaeldkhn.github.io/miroir/train/client.html).
