use std::process::Command;

use crate::common::{ENV_IMAGE, build_images, docker, docker_ok, pid_label};

const ASK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/acme-crm/coworker-tasks/ask-priya.yaml"
));

const FILES: [&str; 7] = [
    "instruction.md",
    "task.toml",
    "environment/Dockerfile",
    "environment/docker-compose.yaml",
    "solution/steps.jsonl",
    "solution/solve.sh",
    "tests/test.sh",
];

#[test]
#[ignore = "needs Docker"]
fn an_export_writes_a_harbor_task_for_each_task_the_agent_can_solve() {
    build_images();
    let scratch = std::env::temp_dir().join(format!("miroir-export-{}", std::process::id()));
    std::fs::create_dir_all(scratch.join("out")).unwrap();
    std::fs::write(scratch.join("ask-priya.yaml"), ASK).unwrap();
    let run = "chown 4242:4242 /out && exec tini -- miroir export harbor \
               --image ghcr.io/acme/crm-env:1.2 --out /out";
    let create = ["create", "--label", &pid_label(), "--network", "none", "--entrypoint", "sh"];
    let id = docker_ok(&[&create[..], &[ENV_IMAGE, "-c", run]].concat());
    docker_ok(&["cp", scratch.join("out").to_str().unwrap(), &format!("{id}:/")]);
    let task = scratch.join("ask-priya.yaml");
    docker_ok(&["cp", task.to_str().unwrap(), &format!("{id}:/miroir/tasks/")]);
    docker_ok(&["start", &id]);
    let code = docker_ok(&["wait", &id]);
    let logs = String::from_utf8_lossy(&docker(&["logs", &id]).stderr).into_owned();
    let back = scratch.join("back");
    docker_ok(&["cp", &format!("{id}:/out"), back.to_str().unwrap()]);
    let tar = docker(&["cp", &format!("{id}:/out"), "-"]).stdout;
    let _ = docker(&["rm", "-f", &id]);
    let mut list = Command::new("tar")
        .args(["-tvf", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(list.stdin.as_mut().unwrap(), &tar).unwrap();
    let listing = String::from_utf8_lossy(&list.wait_with_output().unwrap().stdout).into_owned();

    assert_eq!(code, "0", "{logs}");
    assert!(logs.contains("ask-priya") && logs.contains("it has coworkers"), "{logs}");
    let mut exported = Vec::new();
    for split in std::fs::read_dir(&back).unwrap() {
        for task in std::fs::read_dir(split.unwrap().path()).unwrap() {
            exported.push(task.unwrap().path());
        }
    }
    assert_eq!(exported.len(), 8, "every fixture task but ask-priya: {exported:?}");
    for task in &exported {
        for file in FILES {
            assert!(task.join(file).is_file(), "{} has no {file}", task.display());
        }
    }
    let pay = back.join("train/pay-invoice");
    let compose = std::fs::read_to_string(pay.join("environment/docker-compose.yaml")).unwrap();
    assert!(compose.contains(r#"image: "ghcr.io/acme/crm-env:1.2""#), "{compose}");
    let toml = std::fs::read_to_string(pay.join("task.toml")).unwrap();
    assert!(toml.contains(r#"name = "acme-crm/pay-invoice""#), "{toml}");
    let steps = std::fs::read_to_string(pay.join("solution/steps.jsonl")).unwrap();
    assert_eq!(steps.lines().count(), 3, "the golden run's three calls: {steps}");
    for line in listing.lines().filter(|line| line.contains("pay-invoice")) {
        assert!(line.contains("4242"), "everything belongs to /out's owner: {line}");
    }
    let script = listing.lines().find(|line| line.ends_with("pay-invoice/tests/test.sh")).unwrap();
    assert!(script.starts_with("-rwxr-xr-x"), "{script}");
    let _ = std::fs::remove_dir_all(&scratch);
}
