use crate::common::{Container, ENV_IMAGE, MYSQL_ENV_IMAGE, Options, docker_ok, run_import};

const KEY: &[u8] = b"a key of thirty-two bytes or more, for tests";

const CRM_SPEC: &str = "apps:
  crm:
    users: {name: name, email: email, role: keep, status: keep}
    api_tokens: {token: secret}
    contacts: {name: name, email: email, company: name, phone: phone, birthday: blank}
    deals: {name: text, stage: keep, status: keep, expected_close: keep}
    notes: {text: text}
    activities: {kind: keep, summary: text}
    invoices: {number: keep, currency: keep, status: keep, due_date: keep, payment_intent: secret}
    exchange_rates: {currency: keep, day: keep, valid_until: keep}
";

const CRM_FLAGS: [&str; 8] =
    ["--dump", "crm=/in/crm.sql", "--scrub", "/in/scrub.yaml", "--key", "/in/key", "--out", "/out"];

const TRACES: &str = r#"{"id": "inc-1", "consent": "support terms 4.2, opt-in", "request": "Marcus Webb (marcus@acme.test) takes over deal 2001", "calls": [{"tool": "api_request", "arguments": {"method": "PATCH", "path": "/deals/2001", "headers": {"Authorization": "Bearer seed-token-dana"}, "body": {"owner_id": 7}}, "status": 200}]}
{"id": "inc-2", "request": "Void invoice 4013", "calls": [{"tool": "api_request", "arguments": {"method": "POST", "path": "/invoices/4013/void"}}]}
"#;

fn crm_dump() -> (Vec<u8>, Vec<String>) {
    let env = Container::start(&Options::default());
    env.wait_healthy();
    env.psql("DROP SCHEMA miroir CASCADE");
    let dump = env.exec(&[
        "pg_dump",
        "-h",
        "127.0.0.1",
        "-U",
        "postgres",
        "--no-owner",
        "--no-privileges",
        "crm",
    ]);
    assert!(dump.status.success(), "{}", String::from_utf8_lossy(&dump.stderr));
    let phones = env.psql("SELECT string_agg(phone, '|' ORDER BY id) FROM contacts WHERE id <= 20");
    let mut originals: Vec<String> = phones.split('|').map(String::from).collect();
    originals.extend(
        [
            "Elena Torres",
            "elena@northwind.test",
            "jordan@initech.test",
            "j.avery@initech.test",
            "dana@acme.test",
            "Northwind",
            "Initech",
            "Priya",
            "seed-token-dana",
            "pi_seed_",
        ]
        .map(String::from),
    );
    (dump.stdout, originals)
}

fn crm_inputs<'a>(dump: &'a [u8], spec: &'a str) -> Vec<(&'a str, &'a [u8])> {
    vec![("crm.sql", dump), ("scrub.yaml", spec.as_bytes()), ("key", KEY)]
}

#[test]
#[ignore = "needs Docker"]
fn an_import_scrubs_every_original_and_restores_as_it_was() {
    let (dump, originals) = crm_dump();
    let mut inputs = crm_inputs(&dump, CRM_SPEC);
    inputs.push(("traces.jsonl", TRACES.as_bytes()));
    let flags = [&CRM_FLAGS[..], &["--traces", "/in/traces.jsonl"]].concat();
    let imported = run_import(ENV_IMAGE, "/in", &inputs, &flags);
    assert_eq!(imported.code, 0, "{}", imported.log);
    let scrubbed = String::from_utf8_lossy(&imported.out["crm.sql"]).into_owned();
    for original in &originals {
        assert!(!scrubbed.contains(original.as_str()), "{original} is still in the dump");
    }
    let report: serde_json::Value = serde_json::from_slice(&imported.out["report.json"]).unwrap();
    let columns = &report["dumps"]["crm"]["columns"];
    assert_eq!(columns["contacts.email"], serde_json::json!({"kind": "email", "rows": 5000}));
    assert_eq!(columns["users.name"]["rows"], 20);
    assert_eq!(columns["contacts.birthday"]["kind"], "blank");
    assert!(columns.get("users.role").is_none(), "kept columns aren't rewritten: {columns}");
    assert!(report["fakes"]["words"].as_u64().unwrap() > 50, "{report}");
    for file in ["crm.sql", "report.json"] {
        let owned = imported.listing.lines().any(|l| l.contains(file) && l.contains("4242"));
        assert!(owned, "{file} belongs to /out's owner:\n{}", imported.listing);
    }

    let env = Container::start(&Options::default());
    env.wait_healthy();
    let local = std::env::temp_dir().join(format!("miroir-scrubbed-{}.sql", std::process::id()));
    std::fs::write(&local, &imported.out["crm.sql"]).unwrap();
    docker_ok(&["cp", local.to_str().unwrap(), &format!("{}:/tmp/crm.sql", env.id)]);
    std::fs::remove_file(&local).unwrap();
    env.psql_in("postgres", "CREATE DATABASE scrubbed");
    let load = ["psql", "-h", "127.0.0.1", "-U", "postgres", "-d", "scrubbed", "-v"];
    env.exec_ok(&[&load[..], &["ON_ERROR_STOP=1", "-q", "-f", "/tmp/crm.sql"]].concat());
    for table in ["users", "contacts", "deals", "notes", "activities", "invoices", "meetings"] {
        let count = format!("SELECT count(*) FROM {table}");
        assert_eq!(env.psql_in("scrubbed", &count), env.psql(&count), "{table}'s rows");
    }
    let stamps = "SELECT md5(string_agg(updated_at::text, ',' ORDER BY id)) FROM contacts";
    assert_eq!(
        env.psql_in("scrubbed", stamps),
        env.psql(stamps),
        "the scrub didn't fire contacts_touch"
    );
    let token = "SELECT count(*) FROM api_tokens WHERE token LIKE 'seed-token-%'";
    assert_eq!(env.psql_in("scrubbed", token), "0");

    let traces = String::from_utf8_lossy(&imported.out["traces.jsonl"]).into_owned();
    let kept: Vec<serde_json::Value> =
        traces.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(kept.len(), 1, "only the trace with a consent basis: {traces}");
    let marcus =
        env.psql_in("scrubbed", "SELECT name || ' (' || email || ')' FROM users WHERE id = 7");
    assert_eq!(
        kept[0]["request"],
        format!("{marcus} takes over deal 2001"),
        "the users' own fakes"
    );
    assert_eq!(kept[0]["calls"][0]["arguments"]["body"], serde_json::json!({"owner_id": 7}));
    assert!(!traces.contains("Authorization") && !traces.contains("seed-token"), "{traces}");
    assert_eq!(
        report["traces"],
        serde_json::json!({
            "kept": [{"id": "inc-1", "consent": "support terms 4.2, opt-in"}],
            "left_out": [{"line": 2, "why": "no consent basis"}],
            "headers_dropped": 1,
        })
    );
}

type Refusal<'a> = (&'a str, Vec<(&'a str, &'a [u8])>, &'a [&'a str], &'a str);

#[test]
#[ignore = "needs Docker"]
fn an_import_fails_closed_and_writes_nothing() {
    let (dump, _) = crm_dump();
    let left_out = CRM_SPEC.replace(", payment_intent: secret", "");
    let kept = CRM_SPEC
        .replace("contacts: {name: name, email: email", "contacts: {name: name, email: keep");
    let large = [&dump[..], b"SELECT pg_catalog.lo_create(4242);\n"].concat();
    let create = [&b"CREATE DATABASE elsewhere;\n"[..], &dump].concat();
    let tmp_flags = CRM_FLAGS.map(|flag| flag.replace("/in/", "/tmp/in/"));
    let tmp_flags: Vec<&str> = tmp_flags.iter().map(String::as_str).collect();
    let cases: [Refusal; 5] = [
        (
            "a column left out",
            crm_inputs(&dump, &left_out),
            &CRM_FLAGS,
            "invoices.payment_intent (text)",
        ),
        (
            "emails kept",
            crm_inputs(&dump, &kept),
            &CRM_FLAGS,
            "values hold an email address that isn't a fake",
        ),
        (
            "a large object",
            crm_inputs(&large, CRM_SPEC),
            &CRM_FLAGS,
            "the dump holds large objects",
        ),
        (
            "another database",
            crm_inputs(&create, CRM_SPEC),
            &CRM_FLAGS,
            "crm.sql:1: the dump creates a database",
        ),
        ("inputs under /tmp", crm_inputs(&dump, CRM_SPEC), &tmp_flags, "is under /tmp"),
    ];
    std::thread::scope(|scope| {
        for (case, inputs, flags, expected) in &cases {
            let dir = if flags[1].starts_with("crm=/tmp") { "/tmp/in" } else { "/in" };
            scope.spawn(move || {
                let imported = run_import(ENV_IMAGE, dir, inputs, flags);
                assert_eq!(imported.code, 1, "{case}: {}", imported.log);
                assert!(imported.log.contains(expected), "{case}: {}", imported.log);
                assert!(!imported.log.contains("elena@northwind.test"), "{case} printed a value");
                assert!(imported.out.is_empty(), "{case} wrote {:?}", imported.out.keys());
            });
        }
    });
}

#[test]
#[ignore = "needs Docker"]
fn an_import_scrubs_mysql_without_moving_its_times() {
    let env = Container::start(&Options { image: MYSQL_ENV_IMAGE, ..Options::default() });
    env.wait_healthy();
    let dump = env.exec(&["mysqldump", "-h", "127.0.0.1", "-u", "root", "--skip-triggers", "shop"]);
    assert!(dump.status.success(), "{}", String::from_utf8_lossy(&dump.stderr));
    let spec = "apps:
  shop:
    staff_keys: {key_value: secret, name: name}
    customers: {name: name, email: email}
    products: {name: name}
    orders: {status: keep, ship_by: keep}
    price_days: {day: keep}
";
    let inputs = [("shop.sql", &dump.stdout[..]), ("scrub.yaml", spec.as_bytes()), ("key", KEY)];
    let flags = ["--dump", "shop=/in/shop.sql", "--scrub", "/in/scrub.yaml", "--key", "/in/key"];
    let imported =
        run_import(MYSQL_ENV_IMAGE, "/in", &inputs, &[&flags[..], &["--out", "/out"]].concat());
    assert_eq!(imported.code, 0, "{}", imported.log);
    let scrubbed = String::from_utf8_lossy(&imported.out["shop.sql"]).into_owned();
    for original in ["@shop.test", "seed-key-ops", "Ops team", "'Ada "] {
        assert!(!scrubbed.contains(original), "{original} is still in the dump");
    }
    let local = std::env::temp_dir().join(format!("miroir-shop-{}.sql", std::process::id()));
    std::fs::write(&local, &imported.out["shop.sql"]).unwrap();
    docker_ok(&["cp", local.to_str().unwrap(), &format!("{}:/tmp/shop.sql", env.id)]);
    std::fs::remove_file(&local).unwrap();
    let mysql = |database: &str, sql: &str| {
        env.exec_ok(&["mysql", "-h", "127.0.0.1", "-u", "root", "-N", "-B", database, "-e", sql])
    };
    mysql("shop", "CREATE DATABASE scrubbed");
    env.exec_ok(&["sh", "-c", "mysql -h 127.0.0.1 -u root scrubbed < /tmp/shop.sql"]);
    for table in ["customers", "products", "orders", "order_items", "price_days", "staff_keys"] {
        let count = format!("SELECT count(*) FROM {table}");
        assert_eq!(mysql("scrubbed", &count), mysql("shop", &count), "{table}'s rows");
    }
    let stamps = "SELECT MD5(GROUP_CONCAT(UNIX_TIMESTAMP(updated_at) ORDER BY id)) FROM products";
    assert_eq!(mysql("scrubbed", stamps), mysql("shop", stamps), "ON UPDATE didn't fire");
    let renamed = "SELECT count(*) FROM products WHERE name LIKE 'Mug %'";
    assert_eq!(mysql("scrubbed", renamed), "0", "every product name was scrubbed");
}
