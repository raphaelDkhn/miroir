//! The manifest, `/miroir/miroir.yaml`: how to run the product, bake its world, and which ports the
//! agent may reach. Unknown keys are errors, and every error names the field path, such as
//! `processes[2].ready.http`, and the fix.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::http::{HeaderName, HeaderValue};
use reqwest::Url;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A world's manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The only TCP ports that may listen off loopback: the agent's door.
    pub expose: Vec<u16>,
    /// Datastores first.
    pub processes: Vec<Process>,
    /// Layered over the container's environment for every process and command.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Commands run at build with only the datastores up.
    #[serde(default)]
    pub bake: Vec<Vec<String>>,
    /// Commands run at build with every process up.
    #[serde(default)]
    pub seed: Vec<Vec<String>>,
    pub database: Database,
    pub sinks: Option<Sinks>,
    /// A command that exits 0 when background work is done.
    pub quiet: Option<Vec<String>>,
}

/// A process of the world.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub name: String,
    pub command: Vec<String>,
    /// The user to run as; `None` runs as Miroir's own user (root).
    pub user: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Overrides the manifest `env` for this process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Started before the date shift and before every other process.
    #[serde(default)]
    pub datastore: bool,
    pub ready: Option<Ready>,
}

/// How to tell that a process is ready, on 127.0.0.1.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ready {
    Tcp(u16),
    Http(String),
}

/// The graded database.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `postgres://…/<db>`, reached as a superuser.
    pub url: String,
    /// `table` or `table.column` entries left out of capture.
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub time: DbTime,
}

/// How the date shift treats the database's time columns.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbTime {
    /// `table.column` entries never shifted.
    pub exclude: Vec<String>,
    /// `table.column` globs of integer columns holding Unix time.
    pub epoch: BTreeMap<String, EpochUnit>,
    /// When the data the `bake` commands restore was captured.
    #[serde(deserialize_with = "rfc3339")]
    pub captured: Option<jiff::Timestamp>,
}

/// An RFC 3339 timestamp, with an error that says so.
fn rfc3339<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<jiff::Timestamp>, D::Error> {
    let text = String::deserialize(deserializer)?;
    let timestamp = text.parse().map_err(|e| {
        let example = "such as 2026-10-26T06:00:00Z";
        D::Error::custom(format!("'{text}' is not an RFC 3339 timestamp, {example}: {e}"))
    })?;
    Ok(Some(timestamp))
}

/// The unit of a Unix-time column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EpochUnit {
    S,
    Ms,
}

impl EpochUnit {
    /// Units per second: 1 or 1000.
    pub fn per_second(self) -> i64 {
        match self {
            EpochUnit::S => 1,
            EpochUnit::Ms => 1000,
        }
    }
}

/// The loopback recorders of outbound email and HTTP.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sinks {
    pub smtp: Option<u16>,
    pub http: Option<u16>,
    /// Canned HTTP answers by `"[METHOD ]/path[*]"`.
    #[serde(default)]
    pub responses: BTreeMap<String, CannedResponse>,
}

/// A canned HTTP answer of the HTTP sink.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CannedResponse {
    /// 200 when left out.
    #[serde(default = "ok")]
    pub status: u16,
    pub json: Option<Value>,
    pub body: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn ok() -> u16 {
    200
}

/// A manifest error: the field path and what to change.
#[derive(Debug, thiserror::Error)]
#[error("{field}: {message}")]
pub struct ManifestError {
    pub field: String,
    pub message: String,
}

impl Manifest {
    /// Load and validate the manifest at `path`.
    pub fn load(path: &Path) -> Result<Manifest, ManifestError> {
        let file = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|e| {
            let fix = "a world image copies it there with COPY miroir.yaml /miroir/miroir.yaml \
                       and runs miroir as root, with USER root as its last USER";
            error(&file, format!("cannot read the manifest: {e}; {fix}"))
        })?;
        parse(&text, &file)
    }

    /// The datastore processes, in manifest order.
    pub fn datastores(&self) -> Vec<&Process> {
        self.processes.iter().filter(|process| process.datastore).collect()
    }

    /// The other processes, in manifest order.
    pub fn apps(&self) -> Vec<&Process> {
        self.processes.iter().filter(|process| !process.datastore).collect()
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.expose.is_empty() {
            return Err(error(
                "expose",
                "list at least one port: the agent reaches the product there",
            ));
        }
        for (i, port) in self.expose.iter().enumerate() {
            tcp_port(*port, &format!("expose[{i}]"))?;
        }
        self.validate_processes()?;
        env_names(&self.env, "env")?;
        commands(&self.bake, "bake")?;
        commands(&self.seed, "seed")?;
        validate_database(&self.database)?;
        if let Some(sinks) = &self.sinks {
            validate_sinks(sinks)?;
        }
        match &self.quiet {
            Some(quiet) => command(quiet, "quiet"),
            None => Ok(()),
        }
    }

    fn validate_processes(&self) -> Result<(), ManifestError> {
        if !self.processes.iter().any(|process| process.datastore) {
            let fix = "list the database first, with `datastore: true`";
            return Err(error("processes", format!("no process is a datastore; {fix}")));
        }
        let mut names: BTreeMap<&str, usize> = BTreeMap::new();
        let mut first_app: Option<&str> = None;
        for (i, process) in self.processes.iter().enumerate() {
            let at = format!("processes[{i}]");
            validate_process(process, &at)?;
            if let Some(first) = names.insert(&process.name, i) {
                let message =
                    format!("'{}' is already the name of processes[{first}]", process.name);
                return Err(error(format!("{at}.name"), format!("{message}; rename one")));
            }
            match (process.datastore, first_app) {
                (true, Some(app)) => {
                    let message =
                        format!("datastore '{}' comes after the app '{app}'", process.name);
                    let fix = "list every datastore before the other processes";
                    return Err(error(format!("{at}.datastore"), format!("{message}; {fix}")));
                }
                (false, None) => first_app = Some(&process.name),
                (true, None) | (false, Some(_)) => {}
            }
        }
        Ok(())
    }
}

fn validate_process(process: &Process, at: &str) -> Result<(), ManifestError> {
    let name = &process.name;
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        let message = format!("'{name}' must be lower-case letters, digits and '-'");
        return Err(error(format!("{at}.name"), message));
    }
    command(&process.command, &format!("{at}.command"))?;
    if let Some(cwd) = &process.cwd
        && !cwd.is_absolute()
    {
        let message = format!("'{}' must be an absolute path, such as /app", cwd.display());
        return Err(error(format!("{at}.cwd"), message));
    }
    env_names(&process.env, &format!("{at}.env"))?;
    match &process.ready {
        Some(Ready::Tcp(port)) => tcp_port(*port, &format!("{at}.ready.tcp")),
        Some(Ready::Http(url)) => loopback_http(url, &format!("{at}.ready.http")),
        None => Ok(()),
    }
}

fn validate_database(database: &Database) -> Result<(), ManifestError> {
    let url = Url::parse(&database.url).ok();
    let postgres = url.is_some_and(|url| {
        ["postgres", "postgresql"].contains(&url.scheme()) && url.path().len() > 1
    });
    if !postgres {
        let fix = "such as postgres://postgres@127.0.0.1:5432/app";
        let message =
            format!("'{}' must be a postgres:// URL naming the database, {fix}", database.url);
        return Err(error("database.url", message));
    }
    for (i, entry) in database.ignore.iter().enumerate() {
        if entry.is_empty() || entry.starts_with('.') || entry.ends_with('.') {
            let message = format!("'{entry}' must be a table or table.column");
            return Err(error(format!("database.ignore[{i}]"), message));
        }
    }
    for (i, entry) in database.time.exclude.iter().enumerate() {
        table_column(entry, &format!("database.time.exclude[{i}]"))?;
    }
    for glob in database.time.epoch.keys() {
        table_column(glob, &format!("database.time.epoch.{glob}"))?;
    }
    Ok(())
}

fn validate_sinks(sinks: &Sinks) -> Result<(), ManifestError> {
    for (port, at) in [(sinks.smtp, "sinks.smtp"), (sinks.http, "sinks.http")] {
        if let Some(port) = port {
            tcp_port(port, at)?;
        }
    }
    if let Some(port) = sinks.http
        && sinks.smtp == Some(port)
    {
        let message = format!("{port} is also the port of sinks.smtp; give each sink its own");
        return Err(error("sinks.http", message));
    }
    for (key, response) in &sinks.responses {
        let at = format!("sinks.responses.{key}");
        response_key(key, &at)?;
        canned(response, &at)?;
    }
    Ok(())
}

/// A `"[METHOD ]/path[*]"` key of `sinks.responses`.
fn response_key(key: &str, at: &str) -> Result<(), ManifestError> {
    let path = match key.split_once(' ') {
        Some((method, path)) => {
            if method.is_empty() || !method.chars().all(|c| c.is_ascii_uppercase()) {
                return Err(error(
                    at,
                    format!("'{method}' must be an upper-case method, as in POST"),
                ));
            }
            path
        }
        None => key,
    };
    if path.starts_with('/') && !path.strip_suffix('*').unwrap_or(path).contains('*') {
        return Ok(());
    }
    let message = "must be \"[METHOD ]/path\", with one optional * at the end to match a prefix";
    Err(error(at, message))
}

fn canned(response: &CannedResponse, at: &str) -> Result<(), ManifestError> {
    if !(100..=599).contains(&response.status) {
        let message = format!("{} is not an HTTP status; use 100 to 599", response.status);
        return Err(error(format!("{at}.status"), message));
    }
    if response.json.is_some() && response.body.is_some() {
        return Err(error(at, "sets both json and body; keep one"));
    }
    for (name, value) in &response.headers {
        if HeaderName::from_bytes(name.as_bytes()).is_err() || HeaderValue::from_str(value).is_err()
        {
            let message = format!("'{name}: {value}' is not a valid HTTP header");
            return Err(error(format!("{at}.headers"), message));
        }
    }
    Ok(())
}

fn tcp_port(port: u16, at: &str) -> Result<(), ManifestError> {
    if port == 0 {
        return Err(error(at, "0 is not a TCP port; use 1 to 65535"));
    }
    Ok(())
}

fn loopback_http(url: &str, at: &str) -> Result<(), ManifestError> {
    let parsed = Url::parse(url).ok();
    if parsed.is_some_and(|url| url.scheme() == "http" && url.host_str() == Some("127.0.0.1")) {
        return Ok(());
    }
    let message = format!(
        "'{url}' must be an http:// URL on 127.0.0.1, such as http://127.0.0.1:3000/healthz"
    );
    Err(error(at, message))
}

fn command(argv: &[String], at: &str) -> Result<(), ManifestError> {
    if argv.first().is_some_and(|program| !program.is_empty()) {
        return Ok(());
    }
    Err(error(at, "list the program, then its arguments, such as [node, server.js]"))
}

fn commands(list: &[Vec<String>], key: &str) -> Result<(), ManifestError> {
    for (i, argv) in list.iter().enumerate() {
        command(argv, &format!("{key}[{i}]"))?;
    }
    Ok(())
}

/// Names the OS accepts: not empty, and without '=' or NUL.
fn env_names(env: &BTreeMap<String, String>, at: &str) -> Result<(), ManifestError> {
    for name in env.keys() {
        if name.is_empty() || name.contains(['=', '\0']) {
            let message = format!("{name:?} is not a variable name; drop its '=' or NUL");
            return Err(error(at, message));
        }
    }
    Ok(())
}

/// `table.column`, its last dot splitting the two, both non-empty.
fn table_column(entry: &str, at: &str) -> Result<(), ManifestError> {
    match entry.rsplit_once('.') {
        Some((table, column)) if !table.is_empty() && !column.is_empty() => Ok(()),
        Some(_) | None => {
            let message = format!("'{entry}' must be table.column, such as contacts.birthday");
            Err(error(at, message))
        }
    }
}

/// Parse and validate a manifest; `file` names the root in errors.
fn parse(text: &str, file: &str) -> Result<Manifest, ManifestError> {
    let mut options = serde_saphyr::Options::default();
    options.with_snippet = false;
    options.strict_booleans = true;
    let root: Value = serde_saphyr::from_str_with_options(text, options).map_err(|e| {
        let message = e.render_with_formatter(&serde_saphyr::UserMessageFormatter);
        error(file, format!("not valid YAML: {}", message.lines().next().unwrap_or_default()))
    })?;
    let manifest: Manifest = serde_path_to_error::deserialize(root).map_err(|e| {
        let path = e.path().to_string();
        let field = if path == "." { file.to_string() } else { path };
        let message = e.inner().to_string();
        let message = message
            .replace("expected u16", "expected a TCP port, 1 to 65535")
            .replace("expected map with a single key", "expected {tcp: PORT} or {http: URL}");
        error(field, message)
    })?;
    manifest.validate()?;
    Ok(manifest)
}

fn error(field: impl Into<String>, message: impl Into<String>) -> ManifestError {
    ManifestError { field: field.into(), message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key, briefly.
    const MANIFEST: &str = r#"
expose: [3000]
processes:
  - {name: db, command: [postgres], user: postgres, datastore: true, ready: {tcp: 5432}}
  - name: web
    command: [node, server.js]
    cwd: /app
    env: {HOST: 0.0.0.0}
    ready: {http: "http://127.0.0.1:3000/healthz"}
env: {DATABASE_URL: "postgres://postgres@127.0.0.1:5432/crm"}
bake: [[createdb, crm]]
seed: [[node, seed.js]]
database:
  url: postgres://postgres@127.0.0.1:5432/crm
  ignore: [api_tokens.last_used_at]
  time: {exclude: [contacts.birthday], epoch: {deals.synced_unix: s}}
sinks: {smtp: 2525, http: 9000, responses: {"POST /v1/x": {json: {}}, "/hooks/*": {status: 204}}}
quiet: [sh, -c, "true"]
"#;

    /// The fixture's manifest, `fixtures/acme-crm/miroir.yaml`.
    const ACME_CRM: &str = r#"
# Acme CRM as a Miroir world: how to run it, bake its world, and what the agent may reach.
expose: [3000]
processes:
  - {name: db, command: [postgres, -D, /var/lib/postgresql/data, -c, listen_addresses=127.0.0.1],
     user: postgres, datastore: true, ready: {tcp: 5432}}
  - {name: cache, command: [redis-server, --dir, /var/lib/redis, --bind, 127.0.0.1],
     user: redis, datastore: true, ready: {tcp: 6379}}
  - name: web
    command: [node, dist/server.js]
    user: node
    cwd: /app
    env: {DATABASE_URL: "postgres://crm_app@127.0.0.1:5432/crm", HOST: 0.0.0.0}
    ready: {http: "http://127.0.0.1:3000/healthz"}
  - {name: worker, command: [node, dist/worker.js], user: node, cwd: /app,
     env: {DATABASE_URL: "postgres://crm_app@127.0.0.1:5432/crm"}}
env:
  DATABASE_URL: postgres://postgres@127.0.0.1:5432/crm
  REDIS_URL: redis://127.0.0.1:6379
  SMTP_URL: smtp://127.0.0.1:2525
  STRIPE_API_BASE: http://127.0.0.1:9000/stripe
  SCHEDULER_ENABLED: "false"
bake:
  - [createdb, -h, 127.0.0.1, -U, postgres, crm]
  - [node, dist/migrate.js]
  - [node, dist/seed.js]
  - [psql, -h, 127.0.0.1, -U, postgres, -d, crm, -v, ON_ERROR_STOP=1, -c,
     "CREATE ROLE crm_app LOGIN; GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public
      TO crm_app; GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO crm_app"]
seed:
  - [node, -e, "fetch('http://127.0.0.1:3000/deals/1100/notes', {method: 'POST',
     headers: {authorization: 'Bearer seed-token-dana', 'content-type': 'application/json'},
     body: JSON.stringify({text: 'Seeded through the API'})}).then((r) =>
     process.exit(r.ok ? 0 : 1))"]
database:
  url: postgres://postgres@127.0.0.1:5432/crm
  ignore: [api_tokens.last_used_at]
  time:
    exclude: [contacts.birthday]
    epoch: {deals.synced_unix: s}
sinks:
  smtp: 2525
  http: 9000
  responses:
    "POST /stripe/v1/payment_intents": {status: 200, json: {id: pi_test, object: payment_intent,
      status: succeeded}}
# Reads REDIS_URL from `env`, so the pipeline tests catch a collect that drops the manifest env.
quiet: [sh, -c, 'test "$(redis-cli -u "$REDIS_URL" llen bull:email:wait)$(redis-cli -u
  "$REDIS_URL" llen bull:email:active)" = 00']
"#;

    /// The Gitea example's manifest, `examples/gitea/miroir.yaml`.
    const GITEA: &str = r#"
expose: [3000]
processes:
  - name: db
    command: [postgres, -D, /var/lib/postgresql/data, -c, listen_addresses=127.0.0.1,
              -c, unix_socket_directories=, -c, fsync=off, -c, synchronous_commit=off,
              -c, full_page_writes=off]
    user: postgres
    datastore: true
    ready: {tcp: 5432}
  - name: gitea
    command: [gitea, web, --config, /etc/gitea/app.ini]
    user: git
    cwd: /var/lib/gitea
    ready: {http: "http://127.0.0.1:3000/api/healthz"}
env:
  GITEA_WORK_DIR: /var/lib/gitea
  GITEA_CUSTOM: /var/lib/gitea/custom
bake:
  - [python3, /opt/gitea-env/seed.py, configure]
  - [psql, -h, 127.0.0.1, -U, postgres, -v, ON_ERROR_STOP=1,
     -c, CREATE ROLE gitea LOGIN, -c, CREATE DATABASE gitea OWNER gitea]
  - [su-exec, git, gitea, migrate, --config, /etc/gitea/app.ini]
  - [su-exec, git, gitea, admin, user, create, --config, /etc/gitea/app.ini, --admin,
     --username, env-admin, --email, admin@env.test, --random-password,
     --must-change-password=false]
seed:
  - [python3, /opt/gitea-env/seed.py]
database:
  url: postgres://postgres@127.0.0.1:5432/gitea
  ignore: [action, notification, issue_user, hook_task, webhook.last_status,
           webhook.updated_unix, language_stat, repo_indexer_status,
           access_token.updated_unix]
  time:
    epoch: {"*.*_unix": s}
sinks:
  smtp: 2525
  http: 9000
quiet: [su-exec, git, gitea, manager, flush-queues, --config, /etc/gitea/app.ini]
"#;

    fn parsed(yaml: &str) -> Result<Manifest, ManifestError> {
        parse(yaml, "miroir.yaml")
    }

    /// The error of `MANIFEST` with its first `from` replaced by `to`.
    fn refused(from: &str, to: &str) -> ManifestError {
        assert!(MANIFEST.contains(from), "the patch does not apply: {from}");
        parsed(&MANIFEST.replacen(from, to, 1)).expect_err(to)
    }

    /// Each case: what to replace, with what, the field the error names, and a part of its
    /// message.
    fn assert_refusals(cases: &[(&str, &str, &str, &str)]) {
        for (from, to, field, message) in cases {
            let error = refused(from, to);
            assert_eq!(error.field, *field, "{to}: {error}");
            assert!(error.message.contains(message), "{to}: expected '{message}' in '{error}'");
        }
    }

    #[test]
    fn a_manifest_reads_every_key() {
        let manifest = parsed(MANIFEST).unwrap();
        assert_eq!(manifest.expose, [3000]);
        let db = &manifest.processes[0];
        assert_eq!(db.user.as_deref(), Some("postgres"));
        assert!(db.datastore);
        assert_eq!(db.cwd, None);
        assert_eq!(db.ready, Some(Ready::Tcp(5432)));
        let web = &manifest.processes[1];
        assert_eq!(web.user, None, "a process without a user runs as root");
        assert!(!web.datastore);
        assert_eq!(web.cwd, Some(PathBuf::from("/app")));
        assert_eq!(web.env["HOST"], "0.0.0.0");
        assert_eq!(web.ready, Some(Ready::Http("http://127.0.0.1:3000/healthz".into())));
        assert_eq!(manifest.env["DATABASE_URL"], "postgres://postgres@127.0.0.1:5432/crm");
        assert_eq!(manifest.bake, [["createdb", "crm"]]);
        assert_eq!(manifest.seed, [["node", "seed.js"]]);
        let time = &manifest.database.time;
        assert_eq!(time.exclude, ["contacts.birthday"]);
        assert_eq!(time.epoch["deals.synced_unix"], EpochUnit::S);
        assert_eq!(time.captured, None);
        let sinks = manifest.sinks.as_ref().unwrap();
        assert_eq!((sinks.smtp, sinks.http), (Some(2525), Some(9000)));
        let canned = &sinks.responses["POST /v1/x"];
        assert_eq!(canned.status, 200, "the status defaults to 200");
        assert_eq!(canned.json, Some(serde_json::json!({})));
        assert_eq!(sinks.responses["/hooks/*"].status, 204);
        assert_eq!(manifest.quiet, Some(vec!["sh".into(), "-c".into(), "true".into()]));
    }

    #[test]
    fn datastores_and_apps_keep_manifest_order() {
        let manifest = parsed(ACME_CRM).unwrap();
        let names = |processes: Vec<&Process>| -> Vec<String> {
            processes.into_iter().map(|process| process.name.clone()).collect()
        };
        assert_eq!(names(manifest.datastores()), ["db", "cache"]);
        assert_eq!(names(manifest.apps()), ["web", "worker"]);
    }

    #[test]
    fn the_acme_crm_and_gitea_manifests_load() {
        let acme = parsed(ACME_CRM).unwrap();
        assert_eq!(acme.processes.len(), 4);
        assert_eq!(acme.env["SCHEDULER_ENABLED"], "false");
        let quiet = concat!(
            r#"test "$(redis-cli -u "$REDIS_URL" llen bull:email:wait)"#,
            r#"$(redis-cli -u "$REDIS_URL" llen bull:email:active)" = 00"#,
        );
        assert_eq!(acme.quiet, Some(vec!["sh".into(), "-c".into(), quiet.into()]));
        let grant = &acme.bake[3][10];
        assert!(grant.contains("IN SCHEMA public TO crm_app; GRANT USAGE ON ALL"), "{grant}");
        let stripe = &acme.sinks.as_ref().unwrap().responses["POST /stripe/v1/payment_intents"];
        assert_eq!(stripe.json.as_ref().unwrap()["id"], "pi_test");
        let gitea = parsed(GITEA).unwrap();
        assert_eq!(gitea.database.ignore.len(), 9);
        assert_eq!(gitea.database.time.epoch["*.*_unix"], EpochUnit::S);
        assert_eq!(gitea.bake[0], ["python3", "/opt/gitea-env/seed.py", "configure"]);
        assert!(gitea.sinks.as_ref().unwrap().responses.is_empty());
    }

    #[test]
    fn load_reads_a_file_and_names_one_it_cannot_read() {
        let name = format!("miroir-manifest-{}.yaml", std::process::id());
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, MANIFEST).unwrap();
        assert_eq!(Manifest::load(&path).unwrap().processes.len(), 2);
        std::fs::remove_file(&path).unwrap();
        let error = Manifest::load(&path).unwrap_err();
        assert_eq!(error.field, path.display().to_string());
        assert!(error.message.starts_with("cannot read the manifest: "), "{error}");
        assert!(error.message.contains("COPY miroir.yaml /miroir/miroir.yaml"), "{error}");
        let as_root = "and runs miroir as root, with USER root as its last USER";
        assert!(error.message.ends_with(as_root), "{error}");
    }

    #[test]
    fn unknown_keys_are_refused_with_their_path() {
        assert_refusals(&[
            ("expose:", "surprise: 1\nexpose:", "surprise", "unknown field `surprise`"),
            ("    cwd: /app", "    cwd: /app\n    surprise: 1", "processes[1].surprise", "unknown"),
            ("time: {", "time: {anchor: bake, ", "database.time.anchor", "unknown field"),
            ("sinks: {", "sinks: {mcp: 1, ", "sinks.mcp", "unknown field `mcp`"),
            ("{json: {}}", "{json: {}, delay: 1}", "sinks.responses.POST /v1/x.delay", "unknown"),
            ("ignore:", "schemas: [public]\n  ignore:", "database.schemas", "unknown field"),
        ]);
    }

    #[test]
    fn values_of_the_wrong_shape_are_refused_with_their_path() {
        assert_refusals(&[
            ("{tcp: 5432}", "{tcp: 5432, timeout_s: 60}", "processes[0].ready", "{tcp: PORT} or"),
            ("{tcp: 5432}", "{udp: 5432}", "processes[0].ready", "unknown variant `udp`"),
            ("{tcp: 5432}", "{tcp: db}", "processes[0].ready.tcp", "invalid type: string"),
            ("datastore: true", "datastore: yes", "processes[0].datastore", "invalid type"),
            ("{HOST: 0.0.0.0}", "{PORT: 3000}", "processes[1].env.PORT", "expected a string"),
            ("quiet: [sh, -c, \"true\"]", "quiet: {command: [x]}", "quiet", "invalid type: map"),
            ("bake: [[createdb, crm]]", "bake: [createdb, crm]", "bake[0]", "invalid type"),
        ]);
    }

    #[test]
    fn a_missing_key_or_bad_yaml_names_the_file() {
        let error = parsed(&MANIFEST.replacen("expose: [3000]\n", "", 1)).unwrap_err();
        assert_eq!(error.field, "miroir.yaml");
        assert_eq!(error.message, "missing field `expose`");
        let error = parsed("expose: [3000\nprocesses: [").unwrap_err();
        assert_eq!(error.field, "miroir.yaml");
        assert!(error.message.starts_with("not valid YAML: "), "{error}");
        let error = parsed("").unwrap_err();
        assert_eq!(error.field, "miroir.yaml", "{error}");
    }

    #[test]
    fn processes_need_a_datastore_first_and_unique_names() {
        assert_refusals(&[
            ("datastore: true", "datastore: false", "processes", "no process is a datastore"),
            (
                "processes:\n",
                "processes:\n  - {name: early, command: [x]}\n",
                "processes[1].datastore",
                "datastore 'db' comes after the app 'early'; list every datastore before",
            ),
            ("name: web", "name: db", "processes[1].name", "already the name of processes[0]"),
            ("name: web", "name: Web", "processes[1].name", "lower-case letters, digits and '-'"),
            ("name: web", "name: web_1", "processes[1].name", "lower-case letters"),
            ("name: web", "name: ''", "processes[1].name", "lower-case letters"),
            ("command: [node, server.js]", "command: []", "processes[1].command", "the program"),
            ("command: [node, server.js]", "command: ['']", "processes[1].command", "program"),
            ("cwd: /app", "cwd: app", "processes[1].cwd", "'app' must be an absolute path"),
        ]);
        let second = "  - {name: db2, datastore: true, command: [x]}\n  - {name: db";
        let two = MANIFEST.replacen("  - {name: db", second, 1);
        assert_eq!(parsed(&two).unwrap().datastores().len(), 2, "datastores first is fine");
    }

    #[test]
    fn ready_probes_are_on_loopback() {
        let url = "\"http://127.0.0.1:3000/healthz\"";
        assert_refusals(&[
            (url, "\"http://web:3000/healthz\"", "processes[1].ready.http", "on 127.0.0.1"),
            (url, "\"http://localhost:3000/\"", "processes[1].ready.http", "on 127.0.0.1"),
            (url, "\"https://127.0.0.1:3000/\"", "processes[1].ready.http", "an http:// URL"),
            (url, "\"127.0.0.1:3000\"", "processes[1].ready.http", "an http:// URL"),
            ("{tcp: 5432}", "{tcp: 0}", "processes[0].ready.tcp", "0 is not a TCP port"),
        ]);
    }

    #[test]
    fn ports_are_tcp_ports() {
        assert_refusals(&[
            ("expose: [3000]", "expose: [0]", "expose[0]", "0 is not a TCP port; use 1 to 65535"),
            ("expose: [3000]", "expose: [3000, 70000]", "expose[1]", "a TCP port, 1 to 65535"),
            ("expose: [3000]", "expose: []", "expose", "list at least one port"),
            ("smtp: 2525", "smtp: 0", "sinks.smtp", "not a TCP port"),
            ("http: 9000", "http: 0", "sinks.http", "not a TCP port"),
            ("http: 9000", "http: 2525", "sinks.http", "give each sink its own"),
        ]);
    }

    #[test]
    fn environment_names_are_valid() {
        assert_refusals(&[
            ("{HOST: 0.0.0.0}", "{\"A=B\": x}", "processes[1].env", "\"A=B\" is not a variable"),
            ("{HOST: 0.0.0.0}", "{\"\": x}", "processes[1].env", "\"\" is not a variable name"),
            ("env: {DATABASE_URL", "env: {\"X\\0Y\": x, DATABASE_URL", "env", "\"X\\0Y\" is not"),
        ]);
    }

    #[test]
    fn commands_and_quiet_are_argvs() {
        assert_refusals(&[
            ("bake: [[createdb, crm]]", "bake: [[createdb, crm], []]", "bake[1]", "the program"),
            ("seed: [[node, seed.js]]", "seed: [[]]", "seed[0]", "the program"),
            ("quiet: [sh, -c, \"true\"]", "quiet: []", "quiet", "the program, then its arguments"),
        ]);
    }

    #[test]
    fn the_database_is_a_postgres_url_with_table_and_column_entries() {
        let url = "url: postgres://postgres@127.0.0.1:5432/crm";
        let epoch = "{deals.synced_unix: s}";
        assert_refusals(&[
            (url, "url: redis://127.0.0.1/0", "database.url", "must be a postgres:// URL"),
            (url, "url: postgres://127.0.0.1:5432", "database.url", "naming the database"),
            (url, "url: postgres://127.0.0.1:5432/", "database.url", "naming the database"),
            (url, "url: crm", "database.url", "postgres://"),
            ("[api_tokens.last_used_at]", "['']", "database.ignore[0]", "a table or table.column"),
            ("[api_tokens.last_used_at]", "[.x]", "database.ignore[0]", "table or table.column"),
            ("[contacts.birthday]", "[birthday]", "database.time.exclude[0]", "table.column"),
            ("[contacts.birthday]", "[contacts.]", "database.time.exclude[0]", "table.column"),
            (epoch, "{synced_unix: s}", "database.time.epoch.synced_unix", "table.column"),
            (
                epoch,
                "{deals.synced_unix: h}",
                "database.time.epoch.deals.synced_unix",
                "unknown variant `h`, expected `s` or `ms`",
            ),
            (
                "time: {",
                "time: {captured: last week, ",
                "database.time.captured",
                "'last week' is not an RFC 3339 timestamp, such as 2026-10-26T06:00:00Z: ",
            ),
            ("time: {", "time: {captured: 2026-10-26, ", "database.time.captured", "RFC 3339"),
            ("time: {", "time: {captured: 1792994400, ", "database.time.captured", "a string"),
        ]);
        let qualified = MANIFEST.replacen("[contacts.birthday]", "[audit.log.created_at]", 1);
        assert!(parsed(&qualified).is_ok(), "the last dot splits a schema-qualified table");
        let ms = MANIFEST.replacen(epoch, "{\"*.*_ms\": ms}", 1);
        let units = parsed(&ms).unwrap().database.time.epoch;
        assert_eq!(units["*.*_ms"].per_second(), 1000);
        assert_eq!(EpochUnit::S.per_second(), 1);
        let captured = MANIFEST.replacen("time: {", "time: {captured: 2026-10-26T06:00:00Z, ", 1);
        let captured = parsed(&captured).unwrap().database.time.captured.unwrap();
        assert_eq!(captured.as_second(), 1_792_994_400);
    }

    #[test]
    fn canned_responses_are_well_formed() {
        assert_refusals(&[
            ("\"POST /v1/x\"", "\"post /v1/x\"", "sinks.responses.post /v1/x", "upper-case"),
            ("\"POST /v1/x\"", "\" /v1/x\"", "sinks.responses. /v1/x", "upper-case method"),
            ("\"/hooks/*\"", "\"hooks/*\"", "sinks.responses.hooks/*", "[METHOD ]/path"),
            ("\"/hooks/*\"", "\"/ho*oks\"", "sinks.responses./ho*oks", "at the end"),
            ("\"/hooks/*\"", "\"/hooks/**\"", "sinks.responses./hooks/**", "one optional *"),
            ("{status: 204}", "{status: 99}", "sinks.responses./hooks/*.status", "100 to 599"),
            ("{status: 204}", "{status: 600}", "sinks.responses./hooks/*.status", "HTTP status"),
            ("{json: {}}", "{json: {}, body: x}", "sinks.responses.POST /v1/x", "keep one"),
            (
                "{status: 204}",
                "{headers: {\"bad name\": x}}",
                "sinks.responses./hooks/*.headers",
                "'bad name: x' is not a valid HTTP header",
            ),
        ]);
        let headers = MANIFEST.replacen("{status: 204}", "{headers: {X-Hook: \"1\"}, body: ok}", 1);
        let manifest = parsed(&headers).unwrap();
        let canned = &manifest.sinks.as_ref().unwrap().responses["/hooks/*"];
        assert_eq!(canned.headers["X-Hook"], "1");
        assert_eq!(canned.body.as_deref(), Some("ok"));
    }
}
