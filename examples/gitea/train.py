"""Train an agent on the Gitea env with TRL's GRPO, each rollout on an env container of its own.

Start the containers, then train on a GPU machine (see README.md):

    export MIROIR_TOKEN=secret
    miroir pool --size 20 --urls urls.txt -- python/tests/docker.sh gitea-env
    python examples/gitea/train.py --urls urls.txt
"""

import argparse
import os

from miroir import CoworkerModel, read_urls
from miroir.trl import dataset, environment_factory, reward
from trl import GRPOConfig, GRPOTrainer


def main() -> None:
    """Parse the options, then train on the train split and evaluate on the validation split:
    the training worlds' other tasks, and the families training never sees."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--model", default="Qwen/Qwen3-4B-Instruct-2507")
    parser.add_argument("--urls", default="urls.txt", help="the URL file `miroir pool` writes")
    parser.add_argument("--calibration", help="a `miroir calibrate` file: train on learnable tasks")
    parser.add_argument("--traces", default="traces", help="where to save every episode's record")
    parser.add_argument("--output-dir", default="out/gitea-grpo")
    parser.add_argument(
        "--coworker-model", help="a chat model that plays the colleagues of the ask-coworker tasks"
    )
    parser.add_argument("--coworker-base-url", help="its OpenAI-compatible endpoint")
    args = parser.parse_args()

    token = os.environ["MIROIR_TOKEN"]
    url = read_urls(args.urls)[0]
    coworker = None
    if args.coworker_model:
        api_key = os.environ.get("COWORKER_API_KEY")
        coworker = CoworkerModel(args.coworker_base_url, args.coworker_model, api_key)
    trainer = GRPOTrainer(
        model=args.model,
        train_dataset=dataset(url, "train", token, calibration=args.calibration),
        eval_dataset=dataset(url, "validation", token, coworkers=coworker is not None),
        # Harm (a change the task doesn't allow, or a failed gate) earns -0.5: worse than doing
        # nothing, which earns 0, so the policy learns to stop short of a damaging action.
        environment_factory=environment_factory(
            urls_file=args.urls,
            token=token,
            traces=args.traces,
            harm_reward=-0.5,
            max_seconds=900,
            coworker=coworker,
        ),
        reward_funcs=reward,
        args=GRPOConfig(
            output_dir=args.output_dir,
            # Eight rollouts of one task per step, each in its own container: the pool needs
            # these eight, eight more for evaluation, and a few spare.
            num_generations=8,
            per_device_train_batch_size=8,
            per_device_eval_batch_size=8,
            # A whole episode: every turn and every API response, which Gitea makes long.
            max_completion_length=32_768,
            use_vllm=True,
            vllm_mode="colocate",
            learning_rate=1e-6,
            num_train_epochs=30,
            eval_strategy="steps",
            eval_steps=20,
            logging_steps=1,
            log_completions=True,
            bf16=True,
        ),
    )
    trainer.train()


if __name__ == "__main__":
    main()
