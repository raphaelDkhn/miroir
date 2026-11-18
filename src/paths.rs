//! Where Miroir's files live: in the world image under `/miroir`, and in the verifier.

use std::path::PathBuf;

/// The answer file the agent writes in `main`, which Harbor restores in the verifier.
pub const ANSWER: &str = "/logs/artifacts/answer.txt";
/// Where the grader writes `reward.json` and `reward-details.json`.
pub const VERIFIER_DIR: &str = "/logs/verifier";
/// The verifier environment variable that holds the grading spec.
pub const SPEC_VAR: &str = "MIROIR_SPEC";

/// The files under a world image's Miroir root, `/miroir` by default.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Paths under `root` in place of `/miroir`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Paths { root: root.into() }
    }

    /// The manifest, `miroir.yaml`.
    pub fn manifest(&self) -> PathBuf {
        self.root.join("miroir.yaml")
    }

    /// `bake.json`, which `miroir bake` writes last.
    pub fn bake_info(&self) -> PathBuf {
        self.root.join("bake.json")
    }

    /// The runtime directory, made afresh at every boot.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// The readiness marker the compose healthcheck tests.
    pub fn ready(&self) -> PathBuf {
        self.run_dir().join("ready")
    }

    /// The tables captured at the start of the episode.
    pub fn catalog(&self) -> PathBuf {
        self.run_dir().join("catalog.json")
    }

    /// The effects journal: the emails and HTTP requests the product sent.
    pub fn effects(&self) -> PathBuf {
        self.run_dir().join("effects.jsonl")
    }

    /// The faults journal.
    pub fn faults(&self) -> PathBuf {
        self.run_dir().join("faults.jsonl")
    }

    /// The sidecar artifact directory Harbor downloads.
    pub fn evidence_dir(&self) -> PathBuf {
        self.root.join("evidence")
    }

    /// The evidence file.
    pub fn evidence(&self) -> PathBuf {
        self.evidence_dir().join("evidence.db")
    }
}

impl Default for Paths {
    fn default() -> Self {
        Paths::new("/miroir")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_lies_under_the_root() {
        let paths = Paths::new("/r");
        let all = [
            (paths.manifest(), "/r/miroir.yaml"),
            (paths.bake_info(), "/r/bake.json"),
            (paths.run_dir(), "/r/run"),
            (paths.ready(), "/r/run/ready"),
            (paths.catalog(), "/r/run/catalog.json"),
            (paths.effects(), "/r/run/effects.jsonl"),
            (paths.faults(), "/r/run/faults.jsonl"),
            (paths.evidence_dir(), "/r/evidence"),
            (paths.evidence(), "/r/evidence/evidence.db"),
        ];
        for (path, expected) in all {
            assert_eq!(path, PathBuf::from(expected));
        }
    }

    #[test]
    fn the_default_root_is_miroir() {
        assert_eq!(Paths::default().evidence(), PathBuf::from("/miroir/evidence/evidence.db"));
    }
}
