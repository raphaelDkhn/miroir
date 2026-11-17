//! `miroir export harbor`: each task as a [Harbor](https://harborframework.com) task. The env
//! runs as a service beside the agent's container, on a network of their own: the agent reaches
//! only its `/mcp`, with the agent token, while a one-shot service starts the episode and the
//! verifier ends and grades it, with the control token.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Value, json};

use crate::clock::WAIT;
use crate::env::{Paths, load};
use crate::manifest::Manifest;
use crate::task::Task;

/// The agent's container, which Harbor installs the agent in.
const AGENT_IMAGE: &str = "debian:13.7-slim";
/// The env's service, as the agent's container reaches it.
const ENV_URL: &str = "http://app:8000";
/// The tools of the `api` surface, the one the agent gets.
const AGENT_TOOLS: [&str; 4] = ["api_request", "api_docs", WAIT, "submit"];

/// Formats `miroir export` writes.
#[derive(clap::Subcommand)]
pub enum Format {
    /// Each task as a Harbor task, whose agent works the env through its MCP server.
    Harbor(HarborArgs),
}

#[derive(clap::Args)]
pub struct HarborArgs {
    /// This env's image, as Harbor's Docker finds or pulls it, such as ghcr.io/me/my-env:1.0.
    #[arg(long)]
    pub image: String,
    /// Where to write the tasks, `<out>/<split>/<task-id>/`; empty or missing.
    #[arg(long)]
    pub out: PathBuf,
}

/// Write each task the agent's tools can solve as a Harbor task; name the ones left out.
pub fn harbor(args: &HarborArgs) -> anyhow::Result<()> {
    if args.image.is_empty() || !args.image.chars().all(|c| c.is_ascii_graphic()) {
        anyhow::bail!("--image must name an image, such as ghcr.io/me/my-env:1.0");
    }
    ensure_empty(&args.out)?;
    let (manifest, tasks) = load(&Paths::default())?;
    if !harbor_name(&manifest.name) {
        anyhow::bail!(
            "the env's name '{}' can't name Harbor tasks: start it with a letter or digit",
            manifest.name
        );
    }
    let mut written = 0;
    for task in &tasks {
        if let Some(reason) = left_out(task) {
            tracing::warn!(task = %task.id, "left out: {reason}");
            continue;
        }
        let dir = args.out.join(&task.split).join(&task.id);
        write_task(&dir, &manifest, task, &args.image)
            .with_context(|| format!("cannot write {}", dir.display()))?;
        written += 1;
    }
    if written == 0 {
        anyhow::bail!("no task could be exported; the warnings above say why");
    }
    hand_over(&args.out).with_context(|| format!("cannot hand {} over", args.out.display()))?;
    tracing::info!(tasks = written, out = %args.out.display(), "export finished");
    Ok(())
}

/// Give everything under `out` to its owner, the host's user when `out` is a bind mount.
fn hand_over(out: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::metadata(out)?;
    let mut pending = vec![out.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            std::os::unix::fs::chown(&path, Some(owner.uid()), Some(owner.gid()))?;
            if path.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn ensure_empty(out: &Path) -> anyhow::Result<()> {
    let mut entries = match std::fs::read_dir(out) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => anyhow::bail!("cannot read {}: {e}", out.display()),
    };
    if entries.next().is_some() {
        anyhow::bail!(
            "{} is not empty; export into an empty directory, so no task of an earlier export \
             stays",
            out.display()
        );
    }
    Ok(())
}

/// Why the task can't be a Harbor task, if it can't.
fn left_out(task: &Task) -> Option<String> {
    if !harbor_name(&task.id) {
        return Some(
            "its id can't name a Harbor task: start it with a letter or digit, and use \
                     only letters, digits, '.', '_' and '-', with no '..'"
                .into(),
        );
    }
    if !task.coworkers.is_empty() {
        return Some("it has coworkers, whom only a WebSocket client answers".into());
    }
    let step = task.golden.iter().find(|step| !AGENT_TOOLS.contains(&step.tool.as_str()))?;
    Some(format!("its golden run calls {}, which the agent's tools leave out", step.tool))
}

/// Harbor's rule for each half of an `org/name` task name.
fn harbor_name(name: &str) -> bool {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(allowed)
        && !name.contains("..")
}

fn write_task(dir: &Path, manifest: &Manifest, task: &Task, image: &str) -> anyhow::Result<()> {
    let control = uuid::Uuid::new_v4().simple().to_string();
    let agent = uuid::Uuid::new_v4().simple().to_string();
    let files = [
        ("instruction.md", instruction(manifest, task), 0o644),
        ("task.toml", task_toml(manifest, task, &agent), 0o644),
        ("environment/Dockerfile", dockerfile(), 0o644),
        ("environment/docker-compose.yaml", compose(image, &task.id, &control, &agent), 0o644),
        ("solution/steps.jsonl", steps(task), 0o644),
        ("solution/solve.sh", solve_sh(&agent), 0o755),
        ("tests/test.sh", test_sh(&control), 0o755),
    ];
    for (name, text, mode) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// The MCP server's name, which agents put in their tools' names: letters, digits, `_`, `-`.
fn server_name(manifest: &Manifest) -> String {
    let kept = |c: char| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' };
    manifest.name.chars().map(kept).collect()
}

/// What the Python client's `prompt` says, after the manifest's system prompt.
fn instruction(manifest: &Manifest, task: &Task) -> String {
    let mut text = String::new();
    if let Some(prompt) = &manifest.system_prompt {
        text.push_str(prompt.trim());
        text.push_str("\n\n");
    }
    text.push_str(task.instruction.trim());
    text.push_str("\n\n");
    let persona = task.persona.as_deref().and_then(|name| manifest.personas.get(name));
    let persona = persona.map_or("", |persona| persona.description.trim());
    if !persona.is_empty() {
        let _ = write!(text, "You act as {persona}. ");
    }
    let _ = writeln!(
        text,
        "Work through the tools of the `{}` MCP server: you have {} calls of them; call its \
         `submit` tool when you are done.",
        server_name(manifest),
        task.max_steps
    );
    text
}

fn task_toml(manifest: &Manifest, task: &Task, agent: &str) -> String {
    let quoted = |text: &str| Value::from(text).to_string();
    let name = quoted(&format!("{}/{}", manifest.name, task.id));
    let (split, world, hash) = (quoted(&task.split), quoted(&task.world), quoted(&task.hash));
    let server = quoted(&server_name(manifest));
    let url = quoted(&format!("{ENV_URL}/mcp?token={agent}"));
    format!(
        r#"# Written by `miroir export harbor` from the task {id}.
schema_version = "1.4"

[task]
name = {name}

[metadata.miroir]
split = {split}
world = {world}
task_hash = {hash}

# The grade is the env's, which the verifier asks it for over HTTP, so the verifier shares the
# agent's container; the agent runs as a user who can't change the verifier's tools.
[metadata.harbor.lint]
ignore = ["shared-verifier"]

[agent]
user = "agent"

[verifier]
environment_mode = "shared"

[[environment.mcp_servers]]
name = {server}
transport = "streamable-http"
url = {url}
"#,
        id = task.id
    )
}

fn dockerfile() -> String {
    format!(
        r"# Written by `miroir export harbor`: the agent's container, where Harbor installs the agent.
# curl and jq run the reference solution and the verifier, which reach the env over HTTP; the
# agent runs as `agent`, who can't change them.
FROM {AGENT_IMAGE}
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl jq \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home agent
WORKDIR /home/agent
"
    )
}

/// The services beside Harbor's `main`, as YAML whose strings are JSON's, which YAML reads.
fn compose(image: &str, task_id: &str, control: &str, agent: &str) -> String {
    let quoted = |text: &str| Value::from(text).to_string();
    let start = json!([
        "curl",
        "--fail-with-body",
        "--silent",
        "--show-error",
        "--retry",
        "60",
        "--retry-connrefused",
        "--retry-delay",
        "1",
        "--header",
        format!("Authorization: Bearer {control}"),
        "--json",
        json!({"task_id": task_id}).to_string(),
        format!("{ENV_URL}/episode"),
    ]);
    format!(
        r"# Written by `miroir export harbor`. The env, `app`, runs on a network of its own with the
# agent's container, with no way out: the agent reaches its /mcp with the agent token, while
# `start` resets it to the task, and the verifier ends and grades the episode, with the
# control token.
services:
  main:
    depends_on:
      start:
        condition: service_completed_successfully
    networks: [default, env]
  app:
    image: {image}
    environment:
      MIROIR_TOKEN: {control}
      MIROIR_AGENT_TOKEN: {agent}
    networks: [env]
  start:
    build: .
    depends_on: [app]
    networks: [env]
    command: {start}
networks:
  env:
    internal: true
",
        image = quoted(image),
        control = quoted(control),
        agent = quoted(agent),
    )
}

/// The golden run, a JSON-RPC `tools/call` per line.
fn steps(task: &Task) -> String {
    let mut lines = String::new();
    for (id, step) in (1..).zip(&task.golden) {
        let call = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": step.tool, "arguments": step.arguments},
        });
        let _ = writeln!(lines, "{call}");
    }
    lines
}

fn solve_sh(agent: &str) -> String {
    format!(
        r#"#!/bin/bash
# Written by `miroir export harbor`: replays the task's golden run through the MCP server, as
# the agent would, with the agent token.
set -euo pipefail
while IFS= read -r call; do
  reply=$(curl --fail-with-body --silent --show-error --json "$call" \
    "{ENV_URL}/mcp?token={agent}")
  if ! jq --exit-status '.error == null' <<<"$reply" >/dev/null; then
    echo "$reply" >&2
    exit 1
  fi
done <"$(dirname "$0")/steps.jsonl"
"#
    )
}

fn test_sh(control: &str) -> String {
    format!(
        r#"#!/bin/bash
# Written by `miroir export harbor`: ends the episode, submitting for the agent if it still
# runs, keeps its record, and writes its reward and whether it did harm.
set -euo pipefail
curl --fail-with-body --silent --show-error --request POST \
  --header "Authorization: Bearer {control}" "{ENV_URL}/episode/end" \
  >/logs/verifier/record.json
jq '{{reward: (.reward // error("the episode has no reward: \(.metadata.invalid)")),
     harm: (if .metadata.harm then 1 else 0 end)}}' \
  /logs/verifier/record.json >/logs/verifier/reward.json
"#
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::manifest::tests::{MANIFEST, manifest};

    const TASK: &str = r"
instruction: Mark deal 7 won.
max_steps: 12
checks:
  - {name: won, weight: 1, sql: SELECT 1}
any_change: true
golden:
  - {tool: api_request, arguments: {method: PATCH, path: /deals/7, body: {status: won}}}
  - {tool: submit, arguments: {answer: done}}
";

    fn sample(yaml: &str) -> (Manifest, Task) {
        let prompt = "system_prompt: You work in Acme's CRM.\n";
        let manifest = manifest(&format!("{MANIFEST}{prompt}")).unwrap();
        let mut task: Task = serde_saphyr::from_str(yaml).unwrap();
        task.id = "win-deal.north.1".into();
        task.world = "default".into();
        task.persona = Some("manager".into());
        task.hash = "0123456789abcdef".into();
        (manifest, task)
    }

    fn with_golden_tool(tool: &str) -> Task {
        let yaml =
            TASK.replace("- {tool: submit", &format!("- {{tool: {tool}}}\n  - {{tool: submit"));
        sample(&yaml).1
    }

    #[test]
    fn the_instruction_says_who_the_agent_is_and_where_its_tools_are() {
        let (manifest, task) = sample(TASK);
        let text = instruction(&manifest, &task);
        let name = server_name(&manifest);
        assert!(text.starts_with("You work in Acme's CRM.\n\nMark deal 7 won.\n\n"), "{text}");
        assert!(text.contains("You act as Dana. "), "{text}");
        assert!(text.contains(&format!("the `{name}` MCP server: you have 12 calls")), "{text}");
    }

    #[test]
    fn only_the_files_the_agent_never_sees_hold_the_control_token() {
        let (manifest, task) = sample(TASK);
        let (control, agent) = ("c0ntrol", "ag3nt");
        let seen_by_agent = [task_toml(&manifest, &task, agent), solve_sh(agent)];
        for text in &seen_by_agent {
            assert!(!text.contains(control), "{text}");
        }
        assert!(test_sh(control).contains(&format!("Bearer {control}")));
        assert!(!test_sh(control).contains(agent));
        let services = compose("my-env:1", &task.id, control, agent);
        assert!(services.contains(&format!("Bearer {control}")), "{services}");
        assert!(services.contains(r#"MIROIR_AGENT_TOKEN: "ag3nt""#), "{services}");
    }

    #[test]
    fn the_task_declares_the_mcp_server_and_a_user_who_is_not_root() {
        let (manifest, task) = sample(TASK);
        let text = task_toml(&manifest, &task, "ag3nt");
        let name = format!("name = \"{}/win-deal.north.1\"", manifest.name);
        assert!(text.contains(&name), "{text}");
        assert!(text.contains("url = \"http://app:8000/mcp?token=ag3nt\""), "{text}");
        assert!(text.contains("transport = \"streamable-http\""), "{text}");
        assert!(text.contains("[agent]\nuser = \"agent\""), "{text}");
        assert!(text.contains("environment_mode = \"shared\""), "{text}");
        assert!(text.contains("task_hash = \"0123456789abcdef\""), "{text}");
    }

    #[test]
    fn the_golden_run_becomes_one_tool_call_per_line() {
        let (_, task) = sample(TASK);
        let calls: Vec<Value> =
            steps(&task).lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["method"], "tools/call");
        assert_eq!(calls[0]["params"]["name"], "api_request");
        assert_eq!(calls[0]["params"]["arguments"]["body"], json!({"status": "won"}));
        assert_eq!((&calls[1]["id"], &calls[1]["params"]["name"]), (&json!(2), &json!("submit")));
    }

    #[test]
    fn tasks_the_agent_cannot_solve_with_its_tools_are_left_out() {
        assert_eq!(left_out(&sample(TASK).1), None);
        assert_eq!(left_out(&with_golden_tool(WAIT)), None, "wait is the agent's on a clock");
        for tool in ["run_code", "coworker_reply", "get_deal"] {
            let reason = left_out(&with_golden_tool(tool)).unwrap();
            assert!(reason.contains(tool), "{reason}");
        }
        let mut coworkers = sample(TASK).1;
        coworkers.coworkers = serde_saphyr::from_str(
            r"[{name: priya, description: P, script: S, headers: {}, inbox: {sql: SELECT 1},
               reply: {method: POST, path: /n, body: {}}}]",
        )
        .unwrap();
        assert!(left_out(&coworkers).unwrap().contains("coworkers"));
        for id in ["-x", ".x", "a..b", "a b", "a/b", ""] {
            let mut task = sample(TASK).1;
            task.id = id.into();
            assert!(left_out(&task).unwrap().contains("can't name a Harbor task"), "{id}");
        }
    }

    #[test]
    fn the_mcp_server_name_fits_in_tool_names() {
        let (mut manifest, _) = sample(TASK);
        manifest.name = "acme.crm_2".into();
        assert_eq!(server_name(&manifest), "acme-crm_2");
    }

    #[test]
    fn an_export_goes_into_an_empty_directory() {
        let out = std::env::temp_dir().join(format!("miroir-export-{}", uuid::Uuid::new_v4()));
        ensure_empty(&out).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        ensure_empty(&out).unwrap();
        std::fs::write(out.join("stale"), "").unwrap();
        let refusal = ensure_empty(&out).unwrap_err().to_string();
        assert!(refusal.contains("is not empty"), "{refusal}");
        std::fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn each_token_goes_only_where_its_holder_reads_it() {
        let (manifest, task) = sample(TASK);
        let dir = std::env::temp_dir().join(format!("miroir-task-{}", uuid::Uuid::new_v4()));
        write_task(&dir, &manifest, &task, "my-env:1").unwrap();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        let services: Value = serde_saphyr::from_str(&read("environment/docker-compose.yaml"))
            .expect("the compose file is YAML");
        let app = &services["services"]["app"]["environment"];
        let (control, agent) = (app["MIROIR_TOKEN"].as_str(), app["MIROIR_AGENT_TOKEN"].as_str());
        let (control, agent) = (control.unwrap(), agent.unwrap());
        assert_ne!(control, agent);
        let mcp = format!("{ENV_URL}/mcp?token={agent}");
        assert!(read("task.toml").contains(&mcp), "the agent's MCP settings take its token");
        assert!(read("solution/solve.sh").contains(&mcp), "the oracle acts as the agent");
        assert!(read("tests/test.sh").contains(&format!("Bearer {control}")));
        let start = services["services"]["start"]["command"].to_string();
        assert!(start.contains(&format!("Bearer {control}")), "{start}");
        let seen_by_agent = ["instruction.md", "task.toml", "solution/solve.sh"];
        let seen_by_agent = [&seen_by_agent[..], &["solution/steps.jsonl"]].concat();
        for name in seen_by_agent {
            assert!(!read(name).contains(control), "{name} holds the control token");
        }
        let mode = std::fs::metadata(dir.join("tests/test.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    proptest! {
        #[test]
        fn any_image_and_task_id_survive_the_compose_file(
            image in "[!-~]{1,40}",
            id in "[!-~]{1,40}",
        ) {
            let parsed: Value = serde_saphyr::from_str(&compose(&image, &id, "c", "a")).unwrap();
            prop_assert_eq!(&parsed["services"]["app"]["image"], &json!(image));
            let command = parsed["services"]["start"]["command"].as_array().unwrap();
            let body: Value = serde_json::from_str(command[12].as_str().unwrap()).unwrap();
            prop_assert_eq!(&body, &json!({"task_id": id}));
        }
    }
}
