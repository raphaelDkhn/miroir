# Security

Report vulnerabilities privately: on the repository's **Security** tab, choose **Report a
vulnerability**. Don't open a public issue.

Include the Miroir version or commit, how you ran the env, and the steps that reproduce the
problem.

## Scope

Agents act on an env with whatever it lets them do, so these are vulnerabilities:

- Reaching an env's API without `MIROIR_TOKEN`, or from a web page
- Code in the `run_code` sandbox reaching anything but the env's proxy and server
- `miroir import` leaving an original name, email or secret in what it writes

Bugs in the sample products under `fixtures/` and `examples/` are not, unless they get an agent
past one of these boundaries. A grader an agent can fool is a bug: open an issue.

## Supported versions

Miroir is pre-1.0. Fixes go into the latest release only.
