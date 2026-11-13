"""Push the `miroir-verifiers` environment to Prime Intellect's Environments Hub, private.

    python python/push_hub.py [PRIME ENV PUSH OPTIONS...]

The Hub installs an environment by its name and imports the module of that name, so the wheel
is named miroir-verifiers and carries the miroir client beside the adapter. Its dependencies are
this package's; its version is this package's with a post-release of the push's UTC time, since
the Hub never reuses a version number. Needs the `prime` CLI and a `prime login`.
"""

import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
MODULES = ("miroir", "miroir_verifiers")


def pyproject(project: dict, pushed: time.struct_time) -> str:
    """The Hub package's pyproject.toml, from this package's `project` table.

    Args:
        project: The `[project]` table of python/pyproject.toml.
        pushed: When the push happens, in UTC: the version's post-release.
    """
    dependencies = [*project["dependencies"], *project["optional-dependencies"]["verifiers"]]
    listed = ", ".join(f'"{dependency}"' for dependency in dependencies)
    version = f"{project['version']}.post{time.strftime('%Y%m%d%H%M%S', pushed)}"
    return (
        "[project]\n"
        'name = "miroir-verifiers"\n'
        f'version = "{version}"\n'
        f'license = "{project["license"]}"\n'
        f'requires-python = "{project["requires-python"]}"\n'
        f"dependencies = [{listed}]\n\n"
        "[build-system]\n"
        'requires = ["uv_build>=0.13.6,<0.14"]\n'
        'build-backend = "uv_build"\n\n'
        "[tool.uv.build-backend]\n"
        'module-name = ["miroir", "miroir_verifiers"]\n'
        'module-root = ""\n'
    )


def main() -> None:
    """Stage the package in a temporary directory and run `prime env push` on it."""
    project = tomllib.loads((HERE / "pyproject.toml").read_text(encoding="utf-8"))["project"]
    with tempfile.TemporaryDirectory() as stage:
        for module in MODULES:
            ignored = shutil.ignore_patterns("__pycache__")
            shutil.copytree(HERE / "src" / module, Path(stage) / module, ignore=ignored)
        staged = pyproject(project, time.gmtime())
        (Path(stage) / "pyproject.toml").write_text(staged, encoding="utf-8")
        push = ["prime", "env", "push", "--path", stage, "--visibility", "PRIVATE", *sys.argv[1:]]
        sys.exit(subprocess.run(push, check=False).returncode)


if __name__ == "__main__":
    main()
