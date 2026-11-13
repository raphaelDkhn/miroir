//! `miroir import`: restores production dumps into scratch databases, scrubs them and the
//! traces, scans for originals left, then writes the results.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

use crate::db::{Db, Dialect};
use crate::env::{BAKE_LIMIT, Dbs, Env, Paths, STOP_LIMIT};
use crate::manifest::Manifest;
use crate::scrub::{self, Counts, Kind, Leak, Originals, Plan, Scrubber, Spec, Trace};
use crate::state::TEMP_DIRS;
use crate::task::GoldenStep;

const RESTRICT_KEY: &str = "miroir";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ImportError(pub String);

impl From<crate::env::EnvError> for ImportError {
    fn from(e: crate::env::EnvError) -> Self {
        ImportError(e.to_string())
    }
}

impl From<crate::db::DbError> for ImportError {
    fn from(e: crate::db::DbError) -> Self {
        ImportError(e.0)
    }
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// A dump to import, as APP=FILE; give one per app.
    #[arg(long = "dump", value_name = "APP=FILE", required = true, value_parser = parse_dump)]
    pub dumps: Vec<(String, PathBuf)>,
    /// The scrub spec, YAML: each column's kind, by app and table.
    #[arg(long, value_name = "FILE")]
    pub scrub: PathBuf,
    /// The scrub key, a file of at least 32 bytes.
    #[arg(long, value_name = "FILE")]
    pub key: PathBuf,
    /// Production agent traces, as JSONL, to scrub too.
    #[arg(long, value_name = "FILE")]
    pub traces: Option<PathBuf>,
    /// Where to write the scrubbed dumps, traces and report.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
}

fn parse_dump(text: &str) -> Result<(String, PathBuf), String> {
    let (app, file) = text.split_once('=').ok_or("give it as APP=FILE")?;
    if app.is_empty() || file.is_empty() {
        return Err("give it as APP=FILE".into());
    }
    Ok((app.to_string(), PathBuf::from(file)))
}

/// What `report.json` records: rows scrubbed per column, fakes drawn, traces kept.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub dumps: BTreeMap<String, Dumped>,
    pub fakes: Option<Counts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traces: Option<Traces>,
}

/// Traces kept and left out, and credential headers dropped.
#[derive(Debug, Default, Serialize)]
pub struct Traces {
    pub kept: Vec<Kept>,
    pub left_out: Vec<LeftOut>,
    pub headers_dropped: usize,
}

#[derive(Debug, Serialize)]
pub struct Kept {
    pub id: String,
    pub consent: String,
}

#[derive(Debug, Serialize)]
pub struct LeftOut {
    pub line: usize,
    pub why: &'static str,
}

#[derive(Debug, Default, Serialize)]
pub struct Dumped {
    pub columns: BTreeMap<String, Scrubbed>,
}

#[derive(Debug, Serialize)]
pub struct Scrubbed {
    pub kind: Kind,
    pub rows: u64,
}

/// Import, writing nothing unless the scan finds no original left.
pub async fn run(args: &Args) -> Result<(), ImportError> {
    let env = Env::open(Paths::default(), None).await?;
    let spec = Spec::parse(&read_text(&args.scrub)?).map_err(|e| at(&args.scrub, e))?;
    let key = std::fs::read(&args.key).map_err(|e| at(&args.key, e))?;
    check_apps(args, &spec, &env.dbs)?;
    let traces = match &args.traces {
        Some(file) => load_traces(file, &env.manifest)?,
        None => Vec::new(),
    };
    check_paths(args, &env.state)?;
    let imported = match env.start_datastores_alone().await {
        Ok(()) => import(args, &spec, &key, &env.dbs, &traces).await,
        Err(e) => Err(e.into()),
    };
    let stopped = env.supervisor.stop(STOP_LIMIT).await;
    let report = imported?;
    stopped.map_err(|e| ImportError(format!("stopping the datastores: {}", e.0)))?;
    let text = serde_json::to_string_pretty(&report).map_err(|e| ImportError(e.to_string()))?;
    let file = args.out.join("report.json");
    std::fs::write(&file, text + "\n").map_err(|e| at(&file, e))?;
    hand_over(&args.out)
}

fn load_traces(file: &Path, manifest: &Manifest) -> Result<Vec<(usize, Trace)>, ImportError> {
    let traces = scrub::read_traces(&read_text(file)?).map_err(|e| at(file, e))?;
    for (line, trace) in &traces {
        let steps: Vec<GoldenStep> = trace
            .calls
            .iter()
            .map(|call| GoldenStep { tool: call.tool.clone(), arguments: call.arguments.clone() })
            .collect();
        let what = format!("line {line}'s calls");
        manifest.validate_step_apps(&what, &steps).map_err(|e| at(file, e))?;
    }
    Ok(traces)
}

fn check_apps(args: &Args, spec: &Spec, dbs: &Dbs) -> Result<(), ImportError> {
    let mut seen = Vec::new();
    for (app, _) in &args.dumps {
        if !dbs.contains_key(app) {
            let known: Vec<&str> = dbs.keys().map(String::as_str).collect();
            let message = format!(
                "--dump {app}: no app of that name has a database here: {}",
                known.join(", ")
            );
            return Err(ImportError(message));
        }
        if seen.contains(&app) {
            return Err(ImportError(format!("--dump {app}: given twice")));
        }
        seen.push(app);
    }
    for app in spec.apps.keys() {
        if !seen.contains(&app) {
            return Err(ImportError(format!(
                "the scrub spec lists app '{app}', which no --dump names"
            )));
        }
    }
    if let Some(app) = seen.iter().find(|app| !spec.apps.contains_key(**app)) {
        return Err(ImportError(format!("the scrub spec has no section for app '{app}'")));
    }
    Ok(())
}

fn check_paths(args: &Args, roots: &[PathBuf]) -> Result<(), ImportError> {
    let temps = TEMP_DIRS.iter().map(PathBuf::from);
    let kept: Vec<PathBuf> = roots.iter().cloned().chain(temps).collect();
    let inputs = args.dumps.iter().map(|(_, file)| file).chain([&args.scrub, &args.key]);
    let inputs = inputs.chain(args.traces.as_ref());
    for path in inputs.chain([&args.out]) {
        let real = path.canonicalize().map_err(|e| at(path, e))?;
        if let Some(root) = kept.iter().find(|root| real.starts_with(root)) {
            return Err(ImportError(format!(
                "{} is under {}, which the import restores or empties: mount it elsewhere, such as \
                 /in or /out",
                path.display(),
                root.display()
            )));
        }
    }
    if !args.out.is_dir() {
        return Err(ImportError(format!("--out {}: not a directory", args.out.display())));
    }
    Ok(())
}

async fn import(
    args: &Args,
    spec: &Spec,
    key: &[u8],
    dbs: &Dbs,
    traces: &[(usize, Trace)],
) -> Result<Report, ImportError> {
    let mut scratches = BTreeMap::new();
    for (app, dump) in &args.dumps {
        scratches.insert(app.clone(), restored(app, dump, &dbs[app]).await?);
    }
    let mut plans = BTreeMap::new();
    for (app, scratch) in &scratches {
        let tables = scratch.tables().await?;
        plans.insert(app.clone(), scrub::plan(app, &tables, &spec.apps[app]).map_err(ImportError)?);
    }
    let (kept, left_out): (Vec<_>, Vec<_>) =
        traces.iter().cloned().partition(|(_, trace)| scrub::consented(trace));
    let mut originals = originals(&scratches, &plans).await?;
    scrub::add_fields(&mut originals, &kept, &spec.fields);
    let scrubber = Scrubber::new(key, &originals).map_err(ImportError)?;
    let mut report = Report::default();
    for (app, scratch) in &scratches {
        let columns = scrub_columns(&scrubber, scratch, &plans[app]).await?;
        report.dumps.insert(app.clone(), Dumped { columns });
    }
    let (scrubbed, summary) = scrub_traces(&scrubber, spec, &kept, &left_out)?;
    let mut found = scan(&scrubber, &scratches, &plans).await?;
    for (line, trace) in &scrubbed {
        for (field, leak) in scrubber.trace_leaks(trace) {
            found.push(format!("trace on line {line}: {field} holds {}", leak.as_str()));
        }
    }
    if !found.is_empty() {
        return Err(ImportError(format!(
            "the scan found originals left, so nothing was written; give these columns the kind \
             of what they hold, or the traces' fields theirs: {}",
            found.join("; ")
        )));
    }
    for (app, scratch) in &scratches {
        dump(scratch, &args.out.join(format!("{app}.sql"))).await?;
    }
    if args.traces.is_some() {
        write_traces(&args.out.join("traces.jsonl"), &scrubbed)?;
        report.traces = Some(summary);
    }
    report.fakes = Some(scrubber.counts());
    Ok(report)
}

fn scrub_traces(
    scrubber: &Scrubber,
    spec: &Spec,
    kept: &[(usize, Trace)],
    left_out: &[(usize, Trace)],
) -> Result<(Vec<(usize, Trace)>, Traces), ImportError> {
    let mut summary = Traces::default();
    let mut scrubbed: Vec<(usize, Trace)> = Vec::new();
    for (line, trace) in kept {
        let (clean, dropped) = scrubber.trace(trace, &spec.fields);
        if scrubbed.iter().any(|(_, other)| other.id == clean.id) {
            return Err(ImportError(format!("trace on line {line}: its id repeats another's")));
        }
        summary.headers_dropped += dropped;
        let consent = clean.consent.clone().unwrap_or_default();
        summary.kept.push(Kept { id: clean.id.clone(), consent });
        scrubbed.push((*line, clean));
    }
    for (line, _) in left_out {
        summary.left_out.push(LeftOut { line: *line, why: "no consent basis" });
    }
    Ok((scrubbed, summary))
}

fn write_traces(file: &Path, traces: &[(usize, Trace)]) -> Result<(), ImportError> {
    let mut text = String::new();
    for (_, trace) in traces {
        text += &serde_json::to_string(trace).map_err(|e| ImportError(e.to_string()))?;
        text.push('\n');
    }
    std::fs::write(file, text).map_err(|e| at(file, e))
}

async fn restored(app: &str, dump: &Path, db: &Db) -> Result<Db, ImportError> {
    if let Some((line, what)) = switches(&std::fs::read(dump).map_err(|e| at(dump, e))?, db.dialect)
    {
        return Err(ImportError(format!(
            "{}:{line}: the dump {what}, so it would restore elsewhere than the scratch database: \
             dump one database without --create (pg_dump) or --databases (mysqldump)",
            dump.display()
        )));
    }
    let scratch = db.scratch(&format!("miroir_import_{}", app.replace('-', "_"))).await?;
    if db.dialect == Dialect::Postgres {
        scratch.execute("ALTER SYSTEM SET autovacuum = off").await?;
        scratch.execute("SELECT pg_reload_conf()").await?;
    }
    tracing::info!(app, dump = %dump.display(), "restoring the dump");
    let file = dump.to_string_lossy().into_owned();
    match db.dialect {
        Dialect::Postgres => {
            let flags = ["-X", "-q", "-v", "ON_ERROR_STOP=1", "-v", "VERBOSITY=terse"];
            let mut argv = words(&["psql"]);
            argv.extend(words(&flags));
            argv.extend([s("--single-transaction"), s("-d"), s(scratch.url()), s("-f"), file]);
            tool(&argv, None, None, "restoring the dump").await?;
        }
        Dialect::MySql => {
            let mut argv = words(&["mysql"]);
            argv.extend(client_args(scratch.url())?);
            tool(&argv, Some(Path::new(&file)), None, "restoring the dump").await?;
        }
    }
    refuse_miroirs(app, &scratch).await?;
    Ok(scratch)
}

fn s(text: &str) -> String {
    text.to_string()
}

fn words(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| s(text)).collect()
}

async fn refuse_miroirs(app: &str, scratch: &Db) -> Result<(), ImportError> {
    let env = "it is a Miroir env's database, not production's";
    let checks = match scratch.dialect {
        Dialect::Postgres => vec![
            ("SELECT count(*) FROM pg_namespace WHERE nspname = 'miroir'", "a miroir schema"),
            ("SELECT count(*) FROM pg_largeobject_metadata", "large objects"),
        ],
        Dialect::MySql => vec![(
            "SELECT count(*) FROM information_schema.TRIGGERS \
             WHERE TRIGGER_SCHEMA = DATABASE() AND TRIGGER_NAME LIKE 'miroir%'",
            "miroir's capture triggers",
        )],
    };
    for (sql, what) in checks {
        if scratch.value(sql).await?.as_deref().is_some_and(|n| n != "0") {
            let why = if what == "large objects" {
                "which the scrub can't read: export without them"
            } else {
                env
            };
            return Err(ImportError(format!("app '{app}': the dump holds {what}: {why}")));
        }
    }
    Ok(())
}

fn switches(dump: &[u8], dialect: Dialect) -> Option<(usize, &'static str)> {
    for (n, line) in dump.split(|b| *b == b'\n').enumerate() {
        let start = line.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(line.len());
        let line = line[start..].to_ascii_lowercase();
        let word = |w: &[u8]| {
            line.starts_with(w) && line.get(w.len()).is_none_or(|b| !b.is_ascii_alphanumeric())
        };
        if word(b"create database") {
            return Some((n + 1, "creates a database"));
        }
        let switch = match dialect {
            Dialect::Postgres => word(b"\\connect") || word(b"\\c"),
            Dialect::MySql => word(b"use"),
        };
        if switch {
            return Some((n + 1, "switches to another database"));
        }
    }
    None
}

async fn originals(
    scratches: &BTreeMap<String, Db>,
    plans: &BTreeMap<String, Plan>,
) -> Result<Originals, ImportError> {
    let mut originals = Originals::default();
    for (app, plan) in plans {
        for planned in &plan.scrub {
            match planned.kind {
                Kind::Name | Kind::Phone | Kind::Secret => {
                    for value in scratches[app].distinct(&planned.table, &planned.column).await? {
                        originals.add(planned.kind, &value);
                    }
                }
                Kind::Email | Kind::Text | Kind::Blank | Kind::Keep => {}
            }
        }
    }
    Ok(originals)
}

async fn scrub_columns(
    scrubber: &Scrubber,
    scratch: &Db,
    plan: &Plan,
) -> Result<BTreeMap<String, Scrubbed>, ImportError> {
    let mut columns = BTreeMap::new();
    for planned in &plan.scrub {
        let (table, column, kind) = (&planned.table, &planned.column, planned.kind);
        let rows = if kind == Kind::Blank {
            scratch.rewrite(table, column, None).await?
        } else {
            let mut pairs = Vec::new();
            for old in scratch.distinct(table, column).await? {
                match scrubber.scrub(kind, &old) {
                    Some(new) if new != old => pairs.push((old, new)),
                    Some(_) | None => {}
                }
            }
            if pairs.is_empty() { 0 } else { scratch.rewrite(table, column, Some(&pairs)).await? }
        };
        columns.insert(planned.label(), Scrubbed { kind, rows });
    }
    Ok(columns)
}

async fn scan(
    scrubber: &Scrubber,
    scratches: &BTreeMap<String, Db>,
    plans: &BTreeMap<String, Plan>,
) -> Result<Vec<String>, ImportError> {
    let mut found = Vec::new();
    for (app, plan) in plans {
        for planned in &plan.scan {
            let words = match planned.kind {
                Kind::Name | Kind::Text => true,
                Kind::Email | Kind::Phone | Kind::Secret | Kind::Blank | Kind::Keep => false,
            };
            let mut counts: BTreeMap<Leak, usize> = BTreeMap::new();
            for value in scratches[app].distinct(&planned.table, &planned.column).await? {
                for leak in scrubber.leaks(&value, words) {
                    *counts.entry(leak).or_default() += 1;
                }
            }
            for (leak, n) in counts {
                found.push(format!(
                    "{app}: {}: {n} values hold {}",
                    planned.label(),
                    leak.as_str()
                ));
            }
        }
    }
    Ok(found)
}

async fn dump(scratch: &Db, file: &Path) -> Result<(), ImportError> {
    tracing::info!(file = %file.display(), "writing the scrubbed dump");
    match scratch.dialect {
        Dialect::Postgres => {
            let mut argv = words(&["pg_dump", "--no-owner", "--no-privileges", "--no-comments"]);
            if tool_help("pg_dump").await?.contains("--restrict-key") {
                argv.push(format!("--restrict-key={RESTRICT_KEY}"));
            }
            argv.extend([s("--file"), file.to_string_lossy().into_owned(), s(scratch.url())]);
            tool(&argv, None, None, "dumping").await
        }
        Dialect::MySql => {
            let flags =
                ["--skip-comments", "--single-transaction", "--no-tablespaces", "--routines"];
            let mut argv = words(&["mysqldump"]);
            argv.extend(words(&flags));
            argv.extend(client_args(scratch.url())?);
            tool(&argv, None, Some(file), "dumping").await
        }
    }
}

fn client_args(url: &str) -> Result<Vec<String>, ImportError> {
    let url = reqwest::Url::parse(url).map_err(|e| ImportError(e.to_string()))?;
    let mut args = vec![
        format!("--host={}", url.host_str().unwrap_or("127.0.0.1")),
        format!("--port={}", url.port().unwrap_or(3306)),
        format!("--user={}", url.username()),
    ];
    if let Some(password) = url.password() {
        args.push(format!("--password={password}"));
    }
    args.push(url.path().trim_start_matches('/').to_string());
    Ok(args)
}

async fn tool(
    argv: &[String],
    stdin: Option<&Path>,
    stdout: Option<&Path>,
    what: &str,
) -> Result<(), ImportError> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(ImportError(format!("{what}: no command")));
    };
    let mut command = Command::new(program);
    command.args(rest).stderr(std::io::stderr()).kill_on_drop(true);
    command.stdin(match stdin {
        Some(file) => Stdio::from(std::fs::File::open(file).map_err(|e| at(file, e))?),
        None => Stdio::null(),
    });
    command.stdout(match stdout {
        Some(file) => Stdio::from(std::fs::File::create(file).map_err(|e| at(file, e))?),
        None => Stdio::from(std::io::stderr()),
    });
    let run = tokio::time::timeout(BAKE_LIMIT, command.status()).await;
    let status = run
        .map_err(|_| ImportError(format!("{what}: {program} ran past {} s", BAKE_LIMIT.as_secs())))?
        .map_err(|e| ImportError(format!("{what}: cannot run {program}: {e}")))?;
    if status.success() {
        return Ok(());
    }
    Err(ImportError(format!("{what}: {program} failed ({status}); its messages are above")))
}

async fn tool_help(program: &str) -> Result<String, ImportError> {
    let output = Command::new(program).arg("--help").stdin(Stdio::null()).output().await;
    let output = output.map_err(|e| ImportError(format!("cannot run {program}: {e}")))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn hand_over(out: &Path) -> Result<(), ImportError> {
    let owner = std::fs::metadata(out).map_err(|e| at(out, e))?;
    for entry in std::fs::read_dir(out).map_err(|e| at(out, e))? {
        let path = entry.map_err(|e| at(out, e))?.path();
        std::os::unix::fs::chown(&path, Some(owner.uid()), Some(owner.gid()))
            .map_err(|e| at(&path, e))?;
    }
    Ok(())
}

fn read_text(path: &Path) -> Result<String, ImportError> {
    std::fs::read_to_string(path).map_err(|e| at(path, e))
}

fn at(path: &Path, e: impl std::fmt::Display) -> ImportError {
    ImportError(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dump_that_moves_the_restore_elsewhere_is_found() {
        let pg = b"SET x = 1;\nCOPY t (a) FROM stdin;\n\\\\connect inside data\n\\.\n";
        assert_eq!(switches(pg, Dialect::Postgres), None, "COPY doubles a data line's backslash");
        assert_eq!(switches(b"\\restrict abc\n", Dialect::Postgres), None);
        let connect = b"SET x = 1;\n  \\connect crm\n";
        assert_eq!(switches(connect, Dialect::Postgres), Some((2, "switches to another database")));
        assert_eq!(switches(b"\\c crm\n", Dialect::Postgres).map(|(n, _)| n), Some(1));
        assert_eq!(switches(b"\\copy x\n", Dialect::Postgres), None, "a word, not a prefix");
        let create = b"-- x\nCREATE DATABASE shop;\n";
        assert_eq!(switches(create, Dialect::MySql), Some((2, "creates a database")));
        assert_eq!(switches(b"USE `shop`;\n", Dialect::MySql).map(|(n, _)| n), Some(1));
        assert_eq!(switches(b"INSERT INTO t VALUES ('use me');\n", Dialect::MySql), None);
        assert_eq!(switches(b"user\n", Dialect::MySql), None, "a word, not a prefix");
    }

    #[test]
    fn a_dump_is_named_by_its_app() {
        let (app, file) = parse_dump("crm=/in/crm.sql").unwrap();
        assert_eq!((app.as_str(), file.to_str().unwrap()), ("crm", "/in/crm.sql"));
        assert!(parse_dump("/in/crm.sql").is_err());
        assert!(parse_dump("crm=").is_err());
    }

    #[test]
    fn the_mysql_clients_reach_the_url() {
        let args = client_args("mysql://root@127.0.0.1:3306/miroir_import_shop").unwrap();
        assert_eq!(args, ["--host=127.0.0.1", "--port=3306", "--user=root", "miroir_import_shop"]);
        let with = client_args("mysql://app:pw@db:3307/x").unwrap();
        assert!(
            with.contains(&"--password=pw".to_string())
                && with.contains(&"--port=3307".to_string())
        );
    }
}
