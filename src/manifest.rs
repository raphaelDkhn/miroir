//! The env manifest, `/miroir/miroir.yaml`: its types, defaults and validation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use reqwest::Url;
use serde::Deserialize;
use serde_json::{Map, Value};

const REQUEST_KEYS: [&str; 4] = ["app", "method", "path", "body"];
/// Longest timeout any field takes: a day.
pub const MAX_TIMEOUT_S: u64 = 86_400;

/// A manifest or task file that fails to read, parse or validate.
#[derive(Debug, thiserror::Error)]
#[error("{file}: {message}")]
pub struct ManifestError {
    pub file: String,
    pub message: String,
}

/// The env manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Format version; must be 1.
    pub miroir: u32,
    pub name: String,
    pub description: String,
    /// What miroir starts, datastores first.
    pub processes: Vec<Process>,
    /// Variables for every process and command miroir runs.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Commands that create the seed data, run with only the datastores up and
    /// `MIROIR_WORLD` naming the world.
    #[serde(default)]
    pub bake: Vec<Vec<String>>,
    /// Commands run at bake once the apps are up, such as seeding through the API.
    #[serde(default)]
    pub seed: Vec<Vec<String>>,
    /// Seed snapshots, each baked apart; each task resets to one.
    #[serde(default = "default_worlds")]
    pub worlds: Vec<String>,
    /// Paths each reset restores; when unset, the paths the processes' users can write.
    pub state: Option<Vec<PathBuf>>,
    /// Apps by name, each with an API, an MCP server or a database.
    pub apps: BTreeMap<String, App>,
    /// System prompt the task listings carry, for the adapters.
    pub system_prompt: Option<String>,
    /// How the data's dates follow the real clock.
    #[serde(default)]
    pub time: Time,
    /// Users the agent can act as, by name.
    pub personas: BTreeMap<String, Persona>,
    /// Servers that stand in for the outside world and record what reaches them.
    #[serde(default)]
    pub sinks: Sinks,
    /// Command that tells when the apps' background work has settled.
    pub quiet: Option<Quiet>,
}

/// A process miroir supervises.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub name: String,
    pub command: Vec<String>,
    /// User it runs as; unset means root, which `check` fails.
    pub user: Option<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Started first, and alone while a reset restores and shifts the data.
    #[serde(default)]
    pub datastore: bool,
    /// How miroir tells it's ready.
    pub ready: Option<Ready>,
}

/// A readiness probe: a loopback TCP port, or an `http://` URL that answers 2xx or 3xx.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub tcp: Option<u16>,
    pub http: Option<String>,
    /// Seconds to wait; 60 by default.
    #[serde(default = "default_ready_timeout")]
    pub timeout_s: u64,
}

/// An app: an HTTP API, an MCP server, a database, or several.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    /// What it is, which the tools show the agent when several apps have an API.
    pub description: Option<String>,
    pub api: Option<Api>,
    pub mcp: Option<Mcp>,
    pub database: Option<Database>,
}

/// The app's MCP server, at a loopback `http://` URL.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mcp {
    pub url: String,
}

/// The app's Postgres or `MySQL` database, whose changes grading reads.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// URL miroir connects with, as a user that can install triggers.
    pub url: String,
    /// Schemas to read; all of them when unset.
    pub schemas: Option<Vec<String>>,
    /// Tables, or `table.column`s, whose changes never count.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// How its time columns move.
    #[serde(default)]
    pub time: DbTime,
}

/// How a database's time columns move with the clock.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbTime {
    /// Columns, as `table.column`, that never move, such as birthdays.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Integer columns that hold Unix time, as `table.column` globs, with their unit.
    #[serde(default)]
    pub epoch: BTreeMap<String, EpochUnit>,
}

/// How the data's dates follow the real clock.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Time {
    /// When the seed data is dated: `bake`, the default, or an RFC 3339 instant.
    #[serde(default = "default_anchor")]
    pub anchor: String,
    /// Command that moves the dates in place of miroir, given `MIROIR_SHIFT_DAYS` and
    /// `MIROIR_SHIFT_DATE_DAYS`.
    pub hook: Option<Vec<String>>,
    /// When each captured world's snapshot was taken; bake moves its data to the anchor.
    #[serde(default)]
    pub captured: BTreeMap<String, jiff::Timestamp>,
}

/// Unit of an integer Unix-time column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum EpochUnit {
    #[serde(rename = "s")]
    Seconds,
    #[serde(rename = "ms")]
    Milliseconds,
}

impl EpochUnit {
    /// Units in a second.
    pub fn per_second(self) -> i64 {
        match self {
            EpochUnit::Seconds => 1,
            EpochUnit::Milliseconds => 1000,
        }
    }
}

/// The app's HTTP API.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    /// Loopback `http://` base URL the tools call.
    pub base_url: String,
    /// Path of its `OpenAPI` spec, which `api_docs` serves.
    pub openapi: Option<PathBuf>,
    /// Keys dropped from its JSON answers at any depth: `key` or `parent.key` globs.
    #[serde(default)]
    pub omit: Vec<String>,
}

/// A user the agent acts as.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Persona {
    pub description: String,
    /// Headers pinned on its requests, by app, such as a bearer token.
    #[serde(default)]
    pub headers: BTreeMap<String, BTreeMap<String, String>>,
}

/// Loopback servers that stand in for the outside world and record what reaches them.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sinks {
    /// Port of the SMTP sink.
    pub smtp: Option<u16>,
    /// Port of the HTTP sink.
    pub http: Option<u16>,
    /// Canned answers by `[METHOD ]/path`, a trailing `*` matching a prefix; others get
    /// 200 and `{}`.
    #[serde(default)]
    pub responses: BTreeMap<String, CannedResponse>,
}

/// What the HTTP sink answers a matching request.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CannedResponse {
    #[serde(default = "default_status")]
    pub status: u16,
    pub json: Option<Value>,
    pub body: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// A command that exits 0 once the apps' background work is done; it must pass twice
/// in a row.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quiet {
    pub command: Vec<String>,
    /// Seconds to wait for it; 30 by default.
    #[serde(default = "default_quiet_timeout")]
    pub timeout_s: u64,
}

fn default_ready_timeout() -> u64 {
    60
}
fn default_quiet_timeout() -> u64 {
    30
}
fn default_status() -> u16 {
    200
}
fn default_anchor() -> String {
    "bake".into()
}
fn default_worlds() -> Vec<String> {
    vec!["default".into()]
}

impl Default for Time {
    fn default() -> Self {
        Time { anchor: default_anchor(), hook: None, captured: BTreeMap::new() }
    }
}

impl Manifest {
    /// Read and validate a manifest.
    pub fn load(path: &Path) -> Result<Manifest, ManifestError> {
        let manifest: Manifest = parse_file(path)?;
        manifest.validate().map_err(|message| error(path, message))?;
        Ok(manifest)
    }

    /// The fixed anchor, or `None` when it is `bake`.
    pub fn anchor(&self) -> Option<jiff::Timestamp> {
        self.time.anchor.parse().ok()
    }

    pub fn datastores(&self) -> impl Iterator<Item = &Process> {
        self.processes.iter().filter(|p| p.datastore)
    }

    pub fn app_processes(&self) -> impl Iterator<Item = &Process> {
        self.processes.iter().filter(|p| !p.datastore)
    }

    pub fn apis(&self) -> impl Iterator<Item = (&str, &Api)> {
        self.apps.iter().filter_map(|(name, app)| Some((name.as_str(), app.api.as_ref()?)))
    }

    /// The one app with an MCP server, if any.
    pub fn mcp(&self) -> Option<(&str, &Mcp)> {
        self.apps.iter().find_map(|(name, app)| Some((name.as_str(), app.mcp.as_ref()?)))
    }

    pub fn databases(&self) -> impl Iterator<Item = (&str, &Database)> {
        self.apps.iter().filter_map(|(name, app)| Some((name.as_str(), app.database.as_ref()?)))
    }

    /// The app with a database, when only one has one.
    pub fn only_database(&self) -> Option<&str> {
        let mut names = self.databases().map(|(name, _)| name);
        let first = names.next()?;
        names.next().is_none().then_some(first)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        ensure(self.miroir == 1, format!("miroir must be 1, got {}", self.miroir))?;
        ensure(is_slug(&self.name), "name must be letters, digits, '.', '_' or '-'")?;
        self.validate_processes()?;
        ensure_env_names(self.env.keys())?;
        for command in &self.bake {
            ensure(!command.is_empty(), "bake commands must not be empty")?;
        }
        for command in &self.seed {
            ensure(!command.is_empty(), "seed commands must not be empty")?;
        }
        self.validate_worlds()?;
        match &self.state {
            Some(dirs) => validate_state(dirs)?,
            None => self.validate_derivable()?,
        }
        self.validate_apps()?;
        if let Some(prompt) = &self.system_prompt {
            ensure(!prompt.trim().is_empty(), "system_prompt must not be empty")?;
        }
        self.validate_time()?;
        self.validate_personas()?;
        validate_sinks(&self.sinks)?;
        if let Some(quiet) = &self.quiet {
            ensure(!quiet.command.is_empty(), "quiet.command must not be empty")?;
            ensure(
                (1..=MAX_TIMEOUT_S).contains(&quiet.timeout_s),
                format!("quiet.timeout_s must be 1 to {MAX_TIMEOUT_S}"),
            )?;
        }
        Ok(())
    }

    fn validate_processes(&self) -> Result<(), String> {
        ensure(!self.processes.is_empty(), "processes must list at least one process")?;
        let mut names = BTreeSet::new();
        let mut apps_started = false;
        for p in &self.processes {
            let at = format!("process '{}'", p.name);
            ensure(!p.name.is_empty(), "every process needs a name")?;
            ensure(names.insert(&p.name), format!("{at} is listed twice"))?;
            ensure(!p.command.is_empty(), format!("{at} has an empty command"))?;
            ensure(!apps_started || !p.datastore, format!("{at}: datastores come first"))?;
            apps_started |= !p.datastore;
            if let Some(cwd) = &p.cwd {
                ensure(cwd.is_absolute(), format!("{at}: cwd must be absolute"))?;
            }
            ensure_env_names(p.env.keys())?;
            if let Some(ready) = &p.ready {
                validate_ready(ready).map_err(|e| format!("{at}: {e}"))?;
            }
        }
        ensure(self.datastores().next().is_some(), "at least one process must be a datastore")
    }

    fn validate_worlds(&self) -> Result<(), String> {
        ensure(!self.worlds.is_empty(), "worlds must name at least one world, or be left out")?;
        let mut names = BTreeSet::new();
        for world in &self.worlds {
            ensure(
                is_world_name(world),
                format!(
                    "world '{world}' must be lowercase letters, digits, '_' or '-', starting with \
                     a letter or digit"
                ),
            )?;
            ensure(names.insert(world), format!("world '{world}' is listed twice"))?;
        }
        Ok(())
    }

    fn validate_derivable(&self) -> Result<(), String> {
        for p in &self.processes {
            let root = p.user.as_deref().is_none_or(|user| user == "root");
            ensure(
                !root,
                format!(
                    "process '{}' runs as root, and miroir derives state only from what \
                     non-root users can write: give it a user, or list state",
                    p.name
                ),
            )?;
        }
        Ok(())
    }

    fn validate_apps(&self) -> Result<(), String> {
        ensure(!self.apps.is_empty(), "apps must name at least one app")?;
        let several_apis = self.apis().nth(1).is_some();
        for (name, app) in &self.apps {
            ensure(
                is_world_name(name),
                format!(
                    "app '{name}' must be lowercase letters, digits, '_' or '-', starting with a \
                     letter or digit"
                ),
            )?;
            let at = |e: String| format!("app '{name}': {e}");
            ensure(
                app.api.is_some() || app.mcp.is_some() || app.database.is_some(),
                at("needs an api, an mcp server or a database".into()),
            )?;
            if let Some(mcp) = &app.mcp {
                loopback_http(&mcp.url, "mcp.url").map_err(at)?;
            }
            if let Some(api) = &app.api {
                validate_api(api).map_err(at)?;
                let described = app.description.as_deref().is_some_and(|d| !d.trim().is_empty());
                ensure(
                    described || !several_apis,
                    at("needs a description, since several apps have an api".into()),
                )?;
            }
            if let Some(database) = &app.database {
                validate_database(database).map_err(at)?;
            }
        }
        ensure(self.apis().next().is_some(), "at least one app must have an api")?;
        let servers = self.apps.values().filter(|app| app.mcp.is_some()).count();
        ensure(servers <= 1, "only one app may name an mcp server")?;
        let mut variables = BTreeMap::new();
        for (name, _) in self.apis() {
            let variable = api_variable(name);
            if let Some(other) = variables.insert(variable.clone(), name) {
                return Err(format!(
                    "apps '{other}' and '{name}' would share the code tool's variable {variable}; \
                     rename one"
                ));
            }
        }
        ensure(self.databases().next().is_some(), "at least one app must have a database")?;
        self.validate_distinct_databases()
    }

    fn validate_distinct_databases(&self) -> Result<(), String> {
        let mut seen: Vec<(&str, Url)> = Vec::new();
        for (name, database) in self.databases() {
            let url = Url::parse(&database.url).map_err(|e| e.to_string())?;
            for (other, other_url) in &seen {
                if server(&url) != server(other_url) {
                    continue;
                }
                ensure(
                    url.path() != other_url.path(),
                    format!(
                        "apps '{other}' and '{name}' name the same database; give the second \
                         only an api, and grading counts its changes under '{other}'"
                    ),
                )?;
                ensure(
                    url.scheme() != "mysql",
                    format!(
                        "apps '{other}' and '{name}' have MySQL databases on one server, whose \
                         changes miroir cannot tell apart: give each its own server"
                    ),
                )?;
            }
            seen.push((name, url));
        }
        Ok(())
    }

    fn validate_time(&self) -> Result<(), String> {
        let anchor_ok = self.time.anchor == "bake" || self.anchor().is_some();
        ensure(anchor_ok, "time.anchor must be 'bake' or an RFC 3339 instant")?;
        if let Some(hook) = &self.time.hook {
            ensure(!hook.is_empty(), "time.hook must not be empty when set")?;
            ensure(
                self.time.captured.is_empty(),
                "time.captured can't go with time.hook: bake moves a captured world's databases, \
                 not what the hook moves",
            )?;
        }
        for world in self.time.captured.keys() {
            ensure(
                self.worlds.contains(world),
                format!("time.captured names world '{world}', which worlds doesn't list"),
            )?;
        }
        Ok(())
    }

    fn validate_personas(&self) -> Result<(), String> {
        ensure(!self.personas.is_empty(), "personas must name at least one persona")?;
        for (name, persona) in &self.personas {
            for app in persona.headers.keys() {
                let called = self.apps.get(app).is_some_and(|a| a.api.is_some() || a.mcp.is_some());
                ensure(
                    called,
                    format!(
                        "persona '{name}' has headers for '{app}', which is no app with an api or \
                         an mcp server"
                    ),
                )?;
            }
        }
        Ok(())
    }

    /// Check an entry's `app` against the apps that have what it needs; with `needed`, it
    /// is required when several do.
    pub(crate) fn check_app(
        &self,
        at: &str,
        app: Option<&str>,
        pick: Pick,
        needed: bool,
    ) -> Result<(), String> {
        let (kind, names): (&str, Vec<&str>) = match pick {
            Pick::Api | Pick::Docs => ("an api", self.apis().map(|(name, _)| name).collect()),
            Pick::Database => ("a database", self.databases().map(|(name, _)| name).collect()),
        };
        let several = names.len() > 1;
        let Some(name) = app else {
            return ensure(
                !(needed && several),
                format!(
                    "{at}: app is required, since several apps have {kind}: {}",
                    names.join(", ")
                ),
            );
        };
        ensure(several, format!("{at}: app applies only when several apps have {kind}"))?;
        let app = self.apps.get(name).ok_or_else(|| format!("{at}: unknown app '{name}'"))?;
        let (has, what) = match pick {
            Pick::Api => (app.api.is_some(), "api"),
            Pick::Docs => {
                (app.api.as_ref().is_some_and(|api| api.openapi.is_some()), "api.openapi")
            }
            Pick::Database => (app.database.is_some(), "database"),
        };
        ensure(has, format!("{at}: app '{name}' has no {what}"))
    }

    /// Check that headers name apps with an API.
    pub(crate) fn validate_headers(
        &self,
        at: &str,
        headers: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<(), String> {
        for app in headers.keys() {
            ensure(
                self.apps.get(app).is_some_and(|a| a.api.is_some()),
                format!("{at} has headers for '{app}', which is no app with an api"),
            )?;
        }
        Ok(())
    }

    /// Check a request template: its keys, app, method and path.
    pub(crate) fn validate_request(
        &self,
        at: &str,
        request: &Map<String, Value>,
    ) -> Result<(), String> {
        if let Some(key) = request.keys().find(|key| !REQUEST_KEYS.contains(&key.as_str())) {
            return Err(format!(
                "{at}: '{key}' is not one of {}; the sender's headers go with every request",
                REQUEST_KEYS.join(", ")
            ));
        }
        let app = match request.get("app") {
            None => None,
            Some(Value::String(name)) => Some(name.as_str()),
            Some(_) => return Err(format!("{at}: app must be a string")),
        };
        self.check_app(at, app, Pick::Api, true)?;
        let method = request.get("method").and_then(Value::as_str);
        ensure(
            method.is_some_and(|m| ["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&m)),
            format!("{at}: method must be GET, POST, PUT, PATCH or DELETE"),
        )?;
        let path = request.get("path").and_then(Value::as_str);
        ensure(path.is_some_and(|p| p.starts_with('/')), format!("{at}: path must start with /"))
    }
}

/// What an entry needs from its app.
#[derive(Clone, Copy)]
pub(crate) enum Pick {
    Api,
    Docs,
    Database,
}

fn validate_ready(ready: &Ready) -> Result<(), String> {
    ensure(ready.tcp.is_some() != ready.http.is_some(), "ready needs exactly one of tcp or http")?;
    ensure(
        (1..=MAX_TIMEOUT_S).contains(&ready.timeout_s),
        format!("ready.timeout_s must be 1 to {MAX_TIMEOUT_S}"),
    )?;
    if let Some(http) = &ready.http {
        let url = Url::parse(http).map_err(|e| format!("ready.http is not a URL: {e}"))?;
        ensure(url.scheme() == "http", "ready.http must be an http:// URL")?;
    }
    Ok(())
}

fn validate_state(state: &[PathBuf]) -> Result<(), String> {
    ensure(!state.is_empty(), "state must list at least one directory")?;
    for dir in state {
        let shown = dir.display();
        ensure(dir.is_absolute(), format!("state directory {shown} must be absolute"))?;
        let normal =
            dir.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
        ensure(
            normal && dir.parent().is_some(),
            format!("state directory {shown} is not allowed"),
        )?;
        ensure(!dir.starts_with("/miroir"), format!("state directory {shown} is inside /miroir"))?;
    }
    for a in state {
        for b in state {
            if !std::ptr::eq(a, b) && b.starts_with(a) {
                return Err(format!(
                    "state directories must not nest: {} contains {}",
                    a.display(),
                    b.display()
                ));
            }
        }
    }
    Ok(())
}

fn validate_api(api: &Api) -> Result<(), String> {
    loopback_http(&api.base_url, "api.base_url")?;
    if let Some(openapi) = &api.openapi {
        ensure(openapi.is_absolute(), "api.openapi must be an absolute path")?;
    }
    for pattern in &api.omit {
        let parts: Vec<&str> = pattern.split('.').collect();
        let named = parts.len() <= 2 && parts.iter().all(|part| !part.is_empty());
        ensure(named, format!("api.omit '{pattern}' must be a key or parent.key"))?;
    }
    Ok(())
}

fn validate_database(database: &Database) -> Result<(), String> {
    let url = Url::parse(&database.url).map_err(|e| format!("database.url is not a URL: {e}"))?;
    let scheme_ok = matches!(url.scheme(), "postgres" | "postgresql" | "mysql");
    ensure(scheme_ok, "database.url must start with postgres://, postgresql:// or mysql://")?;
    ensure(url.path().len() > 1, "database.url must name a database")?;
    if let Some(schemas) = &database.schemas {
        ensure(!schemas.is_empty(), "database.schemas must not be empty when set")?;
    }
    for entry in &database.ignore {
        ensure(!entry.is_empty(), "database.ignore entries must not be empty")?;
    }
    for entry in &database.time.exclude {
        ensure(
            entry.contains('.'),
            format!("database.time.exclude entry '{entry}' must be table.column"),
        )?;
    }
    for pattern in database.time.epoch.keys() {
        let parts = pattern.rsplit_once('.');
        let ok = parts.is_some_and(|(table, column)| !table.is_empty() && !column.is_empty());
        ensure(ok, format!("database.time.epoch key '{pattern}' must be table.column"))?;
    }
    Ok(())
}

fn server(url: &Url) -> (String, u16) {
    let host = match url.host_str().unwrap_or_default() {
        "localhost" | "127.0.0.1" | "[::1]" => "loopback",
        other => other,
    };
    let default = if url.scheme() == "mysql" { 3306 } else { 5432 };
    (host.to_string(), url.port().unwrap_or(default))
}

fn validate_sinks(sinks: &Sinks) -> Result<(), String> {
    if let (Some(smtp), Some(http)) = (sinks.smtp, sinks.http) {
        ensure(smtp != http, "sinks.smtp and sinks.http must use different ports")?;
    }
    for (key, response) in &sinks.responses {
        let path = match key.split_once(' ') {
            Some((method, path)) => {
                let method_ok =
                    !method.is_empty() && method.chars().all(|c| c.is_ascii_uppercase());
                ensure(method_ok, format!("sinks.responses key '{key}' has a bad method"))?;
                path
            }
            None => key.as_str(),
        };
        ensure(path.starts_with('/'), format!("sinks.responses key '{key}' must have a /path"))?;
        let star_ok = !path.trim_end_matches('*').contains('*');
        ensure(star_ok, format!("sinks.responses key '{key}': '*' only at the end"))?;
        ensure(
            (100..=599).contains(&response.status),
            format!("sinks.responses '{key}': bad status"),
        )?;
        let one_body = response.json.is_none() || response.body.is_none();
        ensure(one_body, format!("sinks.responses '{key}' sets both json and body"))?;
    }
    Ok(())
}

/// 1 to 64 lowercase letters, digits, `_` or `-`.
pub(crate) fn is_short_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

fn loopback_http(url: &str, key: &str) -> Result<(), String> {
    let url = Url::parse(url).map_err(|e| format!("{key} is not a URL: {e}"))?;
    ensure(url.scheme() == "http", format!("{key} must be plain http://"))?;
    let host = url.host_str().unwrap_or_default();
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
    ensure(loopback, format!("{key} must be on the loopback interface"))
}

fn ensure_env_names<'a>(names: impl Iterator<Item = &'a String>) -> Result<(), String> {
    for name in names {
        let ok = !name.is_empty() && !name.contains(['=', '\0']);
        ensure(ok, format!("'{name}' is not a valid environment variable name"))?;
    }
    Ok(())
}

/// The sandbox variable that holds an app's API URL, such as `API_WEST_OPS`.
pub fn api_variable(app: &str) -> String {
    format!("API_{}", app.to_ascii_uppercase().replace('-', "_"))
}

fn is_world_name(s: &str) -> bool {
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    s.chars().next().is_some_and(allowed) && s.chars().all(|c| allowed(c) || matches!(c, '_' | '-'))
}

fn is_slug(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `Err(message)` unless `condition` holds.
pub(crate) fn ensure(condition: bool, message: impl Into<String>) -> Result<(), String> {
    if condition { Ok(()) } else { Err(message.into()) }
}

/// 64-bit FNV-1a as 16 hex digits; Python's `miroir.derive.content_hash` matches it.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Read and parse a YAML file.
pub(crate) fn parse_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ManifestError> {
    let text = std::fs::read_to_string(path).map_err(|e| error(path, e.to_string()))?;
    serde_saphyr::from_str(&text).map_err(|e| error(path, e.to_string()))
}

pub(crate) fn error(path: &Path, message: impl Into<String>) -> ManifestError {
    ManifestError { file: path.display().to_string(), message: message.into() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const MANIFEST: &str = r#"
miroir: 1
name: acme-crm
description: Acme CRM
processes:
  - {name: db, command: [postgres], user: postgres, datastore: true, ready: {tcp: 5432}}
  - name: web
    command: [node, server.js]
    cwd: /app
    ready: {http: "http://127.0.0.1:3000/healthz"}
state: [/var/lib/postgresql/data, /app/uploads]
apps:
  crm:
    database: {url: "postgres://postgres@127.0.0.1:5432/crm"}
    api: {base_url: "http://127.0.0.1:3000"}
personas:
  manager: {description: Dana, headers: {crm: {Authorization: Bearer x}}}
sinks: {smtp: 2525, http: 9000, responses: {"POST /v1/x": {json: {}}, "/hooks/*": {status: 204}}}
"#;

    pub(crate) fn manifest(yaml: &str) -> Result<Manifest, String> {
        let manifest: Manifest = serde_saphyr::from_str(yaml).map_err(|e| e.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn manifest_err(from: &str, to: &str) -> String {
        assert!(MANIFEST.contains(from), "test patch does not apply: {from}");
        manifest(&MANIFEST.replacen(from, to, 1)).expect_err("manifest should be rejected")
    }

    fn with_db_time(time: &str) -> String {
        let from = "5432/crm\"}";
        assert!(MANIFEST.contains(from), "test patch does not apply: {from}");
        MANIFEST.replacen(from, &format!("5432/crm\", time: {time}}}"), 1)
    }

    #[test]
    fn worlds_are_names_a_path_can_hold() {
        let worlds = |list: &str| manifest(&format!("{MANIFEST}worlds: {list}\n"));
        assert_eq!(worlds("[north, south-2, a_b, 9lives]").unwrap().worlds.len(), 4);
        let refused = [
            ("[]", "at least one world"),
            ("[North]", "lowercase letters"),
            ("['..']", "lowercase letters"),
            ("[a.b]", "lowercase letters"),
            ("[a/b]", "lowercase letters"),
            ("[-a]", "starting with a letter or digit"),
            ("['']", "lowercase letters"),
            ("[a, a]", "listed twice"),
        ];
        for (list, expected) in refused {
            let err = worlds(list).unwrap_err();
            assert!(err.contains(expected), "{list}: expected '{expected}', got '{err}'");
        }
    }

    #[test]
    fn epoch_columns_name_their_unit() {
        let m = manifest(&with_db_time("{epoch: {\"*.*_unix\": s, post.createat: ms}}")).unwrap();
        let time = &m.apps["crm"].database.as_ref().unwrap().time;
        let units: Vec<i64> = time.epoch.values().map(|u| u.per_second()).collect();
        assert_eq!(units, [1, 1000]);
    }

    #[test]
    fn apps_are_named_described_and_on_databases_of_their_own() {
        let with_app = |app: &str| MANIFEST.replacen("apps:\n", &format!("apps:\n  {app}\n"), 1);
        let two_apis = with_app("east: {api: {base_url: \"http://127.0.0.1:3001\"}}");
        let err = manifest(&two_apis).unwrap_err();
        assert!(err.contains("app 'crm': needs a description, since several"), "{err}");
        let described = two_apis
            .replacen("  crm:\n", "  crm:\n    description: The CRM\n", 1)
            .replacen("{api: {base_url", "{description: East, api: {base_url", 1);
        assert_eq!(manifest(&described).unwrap().apis().count(), 2, "an api-only app is fine");
        let clash = MANIFEST.replacen(
            "apps:\n",
            "apps:\n  west-ops: {description: Ops, api: {base_url: \"http://127.0.0.1:3001\"}}\n  \
             west_ops: {description: Ops, api: {base_url: \"http://127.0.0.1:3002\"}}\n",
            1,
        );
        let clash = clash.replacen("  crm:\n", "  crm:\n    description: The CRM\n", 1);
        let err = manifest(&clash).unwrap_err();
        assert!(err.contains("'west-ops' and 'west_ops' would share"), "{err}");
        let db_only =
            with_app("ledger: {database: {url: \"postgres://postgres@localhost/ledger\"}}");
        let m = manifest(&db_only).unwrap();
        assert_eq!((m.apis().count(), m.databases().count()), (1, 2), "a database-only app too");
        let err = manifest(&db_only.replacen("{crm: {Author", "{ledger: {Author", 1)).unwrap_err();
        assert!(err.contains("headers for 'ledger', which is no app with an api"), "{err}");

        let crm_db = "    database: {url: \"postgres://postgres@127.0.0.1:5432/crm\"}\n";
        let refused = [
            ("  crm:\n", "  Crm:\n", "lowercase letters"),
            ("  crm:\n", "  crm.x:\n", "lowercase letters"),
            ("apps:\n", "apps:\n  bare: {}\n", "app 'bare': needs an api, an mcp server or a"),
            ("apps:\n", "apps: {}\nx:\n", "unknown field"),
            (crm_db, "", "at least one app must have a database"),
            ("    api: {base_url", "    unused: {base_url", "unknown field"),
            ("{crm: {Authorization", "{ghost: {Authorization", "headers for 'ghost'"),
            ("3000\"}\n", "3000\", omit: [a.b.c]}\n", "api.omit 'a.b.c' must be a key or"),
            ("3000\"}\n", "3000\", omit: [.url]}\n", "api.omit '.url' must be a key or"),
        ];
        for (from, to, expected) in refused {
            let err = manifest_err(from, to);
            assert!(err.contains(expected), "{to}: expected '{expected}', got '{err}'");
        }
        let no_api = MANIFEST.replacen("    api: {base_url: \"http://127.0.0.1:3000\"}\n", "", 1);
        let err = manifest(&no_api).unwrap_err();
        assert!(err.contains("at least one app must have an api"), "{err}");
        let block_end = MANIFEST.find("personas:").unwrap();
        let block_start = MANIFEST.find("apps:").unwrap();
        let empty = format!("{}apps: {{}}\n{}", &MANIFEST[..block_start], &MANIFEST[block_end..]);
        assert!(manifest(&empty).unwrap_err().contains("apps must name at least one app"));

        let other =
            |url: &str| manifest(&with_app(&format!("other: {{database: {{url: \"{url}\"}}}}")));
        for url in ["postgres://postgres@127.0.0.1:5432/crm", "postgresql://app@localhost/crm"] {
            let err = other(url).unwrap_err();
            assert!(err.contains("name the same database; give the second only an api"), "{err}");
        }
        assert!(other("postgres://postgres@127.0.0.1:5433/crm").is_ok(), "another server");
        assert!(other("postgres://postgres@127.0.0.1/ledger").is_ok(), "another database");
        let mysql = with_app("books: {database: {url: \"mysql://root@localhost:3306/books\"}}")
            .replacen("postgres://postgres@127.0.0.1:5432/crm", "mysql://root@127.0.0.1/shop", 1);
        let err = manifest(&mysql).unwrap_err();
        assert!(err.contains("apps 'books' and 'crm' have MySQL databases on one server"), "{err}");
    }

    #[test]
    fn one_app_may_name_an_mcp_server_on_loopback() {
        let crm_api = "    api: {base_url: \"http://127.0.0.1:3000\"}\n";
        let mcp = |url: &str| format!("{crm_api}    mcp: {{url: \"{url}\"}}\n");
        let with_mcp = MANIFEST.replacen(crm_api, &mcp("http://127.0.0.1:8080/mcp"), 1);
        let m = manifest(&with_mcp).unwrap();
        assert_eq!(
            m.mcp().map(|(name, mcp)| (name, mcp.url.as_str())),
            Some(("crm", "http://127.0.0.1:8080/mcp"))
        );
        assert!(manifest(MANIFEST).unwrap().mcp().is_none());

        let ops = "apps:\n  ops: {mcp: {url: \"http://127.0.0.1:8081/mcp\"}}\n";
        let mcp_only = MANIFEST.replacen("apps:\n", ops, 1);
        let m = manifest(&mcp_only.replacen("{crm: {Author", "{ops: {Author", 1)).unwrap();
        assert_eq!(m.mcp().map(|(name, _)| name), Some("ops"), "an mcp-only app takes headers");

        let refused = [
            (mcp("https://127.0.0.1:8080/mcp"), "mcp.url must be plain http://"),
            (mcp("http://gitea.example:8080/mcp"), "mcp.url must be on the loopback interface"),
            (mcp("not a url"), "mcp.url is not a URL"),
            (format!("{crm_api}    mcp: {{url: x, token: y}}\n"), "unknown field"),
        ];
        for (to, expected) in refused {
            let err = manifest(&MANIFEST.replacen(crm_api, &to, 1)).unwrap_err();
            assert!(err.contains(expected), "{to}: expected '{expected}', got '{err}'");
        }
        let err = manifest(&with_mcp.replacen("apps:\n", ops, 1)).unwrap_err();
        assert!(err.contains("only one app may name an mcp server"), "{err}");
    }

    #[test]
    fn a_system_prompt_is_text() {
        let with = |prompt: &str| {
            MANIFEST.replacen(
                "description: Acme CRM\n",
                &format!("description: Acme CRM\nsystem_prompt: {prompt}\n"),
                1,
            )
        };
        let m = manifest(&with("Work only through the tools.")).unwrap();
        assert_eq!(m.system_prompt.as_deref(), Some("Work only through the tools."));
        assert!(manifest(MANIFEST).unwrap().system_prompt.is_none());
        let err = manifest(&with("\" \"")).unwrap_err();
        assert!(err.contains("system_prompt must not be empty"), "{err}");
    }

    #[test]
    fn state_left_out_needs_every_process_to_run_as_a_user() {
        let derived = MANIFEST.replacen("state: [/var/lib/postgresql/data, /app/uploads]\n", "", 1);
        let err = manifest(&derived).unwrap_err();
        assert!(err.contains("process 'web' runs as root"), "{err}");
        let as_root = derived.replacen("cwd: /app", "cwd: /app\n    user: root", 1);
        assert!(manifest(&as_root).unwrap_err().contains("runs as root"));
        let as_node = derived.replacen("cwd: /app", "cwd: /app\n    user: node", 1);
        assert!(manifest(&as_node).unwrap().state.is_none());
    }

    #[test]
    fn manifest_rejects_nested_state() {
        let err = manifest_err("/app/uploads]", "/var/lib/postgresql/data/pg_wal]");
        assert!(err.contains("must not nest"), "{err}");
    }

    #[test]
    fn manifest_rules() {
        let cases = [
            ("miroir: 1", "miroir: 2", "miroir must be 1"),
            ("name: acme-crm", "name: acme crm", "name must be"),
            (
                "ready: {tcp: 5432}",
                "ready: {tcp: 5432, http: \"http://x\"}",
                "exactly one of tcp or http",
            ),
            (
                "ready: {tcp: 5432}",
                "ready: {tcp: 5432, timeout_s: 0}",
                "ready.timeout_s must be 1 to",
            ),
            ("command: [postgres]", "command: []", "empty command"),
            ("name: web", "name: db", "listed twice"),
            ("datastore: true", "datastore: false", "at least one process must be a datastore"),
            ("cwd: /app", "cwd: app", "cwd must be absolute"),
            (
                "ready: {tcp: 5432}",
                "ready: {tcp: 5432, timeout_s: 18446744073709551615}",
                "ready.timeout_s must be 1 to",
            ),
            (
                "state: [/var/lib/postgresql/data, /app/uploads]",
                "state: [var/lib]",
                "must be absolute",
            ),
            ("state: [/var/lib/postgresql/data, /app/uploads]", "state: [/]", "is not allowed"),
            (
                "state: [/var/lib/postgresql/data, /app/uploads]",
                "state: [/a/../b]",
                "is not allowed",
            ),
            (
                "state: [/var/lib/postgresql/data, /app/uploads]",
                "state: [/miroir/x]",
                "inside /miroir",
            ),
            (
                "state: [/var/lib/postgresql/data, /app/uploads]",
                "state: []",
                "at least one directory",
            ),
            ("postgres://postgres@127.0.0.1:5432/crm", "redis://x/0", "must start with"),
            (
                "postgres://postgres@127.0.0.1:5432/crm",
                "postgres://127.0.0.1:5432",
                "name a database",
            ),
            ("http://127.0.0.1:3000\"}", "https://127.0.0.1:3000\"}", "plain http"),
            ("http://127.0.0.1:3000\"}", "http://example.com\"}", "loopback"),
            (
                "personas:\n  manager: {description: Dana, \
                 headers: {crm: {Authorization: Bearer x}}}",
                "personas: {}",
                "at least one persona",
            ),
            ("http: 9000", "http: 2525", "different ports"),
            ("\"POST /v1/x\"", "\"post /v1/x\"", "bad method"),
            ("\"/hooks/*\"", "\"hooks/*\"", "must have a /path"),
            ("\"/hooks/*\"", "\"/ho*oks\"", "only at the end"),
            ("{status: 204}", "{status: 99}", "bad status"),
            ("{json: {}}", "{json: {}, body: x}", "both json and body"),
            ("description: Acme CRM", "description: Acme CRM\nsurprise: 1", "unknown field"),
            ("command: [postgres]", "command: [postgres], env: {'A=B': x}", "not a valid"),
            ("command: [postgres]", "command: [postgres], env: {'': x}", "not a valid"),
            (
                "database: {url:",
                "database: {schemas: [], url:",
                "database.schemas must not be empty",
            ),
            (
                "database: {url:",
                "database: {ignore: [''], url:",
                "database.ignore entries must not be empty",
            ),
            ("personas:", "bake: [[]]\npersonas:", "bake commands must not be empty"),
            ("personas:", "seed: [[]]\npersonas:", "seed commands must not be empty"),
        ];
        for (from, to, expected) in cases {
            let err = manifest_err(from, to);
            assert!(err.contains(expected), "{to}: expected '{expected}', got '{err}'");
        }
        let anchor = manifest(&format!("{MANIFEST}time: {{anchor: yesterday}}\n")).unwrap_err();
        assert!(anchor.contains("time.anchor"), "{anchor}");
        let fixed =
            manifest(&format!("{MANIFEST}time: {{anchor: \"2026-01-01T00:00:00Z\"}}\n")).unwrap();
        assert_eq!(fixed.anchor().unwrap().as_second(), 1_767_225_600);
        let exclude = manifest(&with_db_time("{exclude: [birthday]}")).unwrap_err();
        assert!(exclude.contains("app 'crm': database.time.exclude"), "{exclude}");
        let moved = manifest(&format!("{MANIFEST}time: {{exclude: [contacts.birthday]}}\n"));
        assert!(moved.unwrap_err().contains("unknown field"), "exclude moved to each database");
        let quiet = manifest(&format!("{MANIFEST}quiet: {{command: []}}\n")).unwrap_err();
        assert!(quiet.contains("quiet.command"), "{quiet}");
        let quiet =
            manifest(&format!("{MANIFEST}quiet: {{command: [x], timeout_s: 0}}\n")).unwrap_err();
        assert!(quiet.contains("quiet.timeout_s"), "{quiet}");
        let hook = manifest(&format!("{MANIFEST}time: {{hook: []}}\n")).unwrap_err();
        assert!(hook.contains("time.hook"), "{hook}");
        let time = |yaml: &str| manifest(&format!("{MANIFEST}time: {yaml}\n"));
        let captured = time("{captured: {default: 2026-10-26T06:00:00Z}}").unwrap();
        assert_eq!(captured.time.captured["default"].as_second(), 1_792_994_400);
        let unknown = time("{captured: {elsewhere: 2026-10-26T06:00:00Z}}").unwrap_err();
        assert!(unknown.contains("world 'elsewhere', which worlds doesn't list"), "{unknown}");
        let hooked = time("{hook: [x], captured: {default: 2026-10-26T06:00:00Z}}").unwrap_err();
        assert!(hooked.contains("time.captured can't go with time.hook"), "{hooked}");
        let vague = time("{captured: {default: last week}}").unwrap_err();
        assert!(vague.contains("captured"), "{vague}");
        assert!(manifest(MANIFEST).unwrap().time.captured.is_empty());
        for key in ["created_unix", ".created_unix", "issue."] {
            let epoch = manifest(&with_db_time(&format!("{{epoch: {{\"{key}\": s}}}}")));
            assert!(epoch.unwrap_err().contains("must be table.column"), "{key}");
        }
        let unit = manifest(&with_db_time("{epoch: {issue.created_unix: h}}"));
        assert!(unit.unwrap_err().contains('h'), "only s and ms");
        let early_app =
            MANIFEST.replacen("processes:\n", "processes:\n  - {name: a, command: [x]}\n", 1);
        assert!(manifest(&early_app).unwrap_err().contains("datastores come first"));
    }
}
