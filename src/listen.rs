//! The boot's listener audit (§3.5 step 8): only the manifest's `expose` ports may listen off
//! loopback, and each of them must.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use reqwest::Url;

use crate::manifest::{Manifest, Ready};

/// The kernel's code for a socket in the LISTEN state.
const LISTEN: &str = "0A";

/// A TCP socket in the LISTEN state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub addr: IpAddr,
    pub port: u16,
}

/// The LISTEN rows of one `/proc/net/tcp` or `/proc/net/tcp6` text. Rows in another state and
/// rows that are not the kernel's format are left out.
pub fn parse(table: &str) -> Vec<Listener> {
    let mut listeners = Vec::new();
    for row in table.lines() {
        let fields: Vec<&str> = row.split_whitespace().collect();
        let (Some(local), Some(&LISTEN)) = (fields.get(1), fields.get(3)) else { continue };
        if let Some(listener) = local_address(local) {
            listeners.push(listener);
        }
    }
    listeners
}

/// `ADDR:PORT` in hex: the address as the kernel prints its 32-bit words in host byte order.
fn local_address(field: &str) -> Option<Listener> {
    let (addr, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let mut bytes = Vec::new();
    for i in (0..addr.len()).step_by(8) {
        let word = u32::from_str_radix(addr.get(i..i + 8)?, 16).ok()?;
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    let addr = match bytes.len() {
        4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => return None,
    };
    Some(Listener { addr, port })
}

/// The listeners of this network namespace, from both files. A kernel without IPv6 has no
/// `/proc/net/tcp6`, and so no IPv6 listener.
pub fn current() -> std::io::Result<Vec<Listener>> {
    tables(Path::new("/proc/net/tcp"), Path::new("/proc/net/tcp6"))
}

/// The listeners of an IPv4 and an IPv6 table; a missing IPv6 table lists none. An error names
/// the file.
fn tables(tcp: &Path, tcp6: &Path) -> std::io::Result<Vec<Listener>> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("cannot read {}: {e}", path.display()))
        })
    };
    let mut listeners = parse(&read(tcp)?);
    match read(tcp6) {
        Ok(table) => listeners.extend(parse(&table)),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(listeners)
}

/// Whether `addr` is loopback: 127.0.0.0/8, `::1` or `::ffff:127.0.0.0/104`.
pub fn is_loopback(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// The two rules of §3.5 step 8. The error holds every violation's message, one per line, each
/// naming the manifest process whose ready probe uses the port, when one does.
pub fn audit(listeners: &[Listener], manifest: &Manifest) -> Result<(), String> {
    let owners = ready_ports(manifest);
    let owner = |port: u16| owners.get(&port).map_or(String::new(), |name| format!(" ({name})"));
    let mut violations: Vec<String> = Vec::new();
    let mut violation = |message: String| {
        if !violations.contains(&message) {
            violations.push(message);
        }
    };
    for listener in listeners {
        if !is_loopback(listener.addr) && !manifest.expose.contains(&listener.port) {
            violation(format!(
                "port {}{} listens on {}: bind it to 127.0.0.1, or list it in expose",
                listener.port,
                owner(listener.port),
                bracketed(listener.addr),
            ));
        }
    }
    for port in &manifest.expose {
        if !listeners.iter().any(|l| l.port == *port && !is_loopback(l.addr)) {
            violation(format!(
                "expose lists {port}{}, but nothing listens on it off loopback: check the \
                 product's bind address",
                owner(*port),
            ));
        }
    }
    if violations.is_empty() { Ok(()) } else { Err(violations.join("\n")) }
}

/// An address as a socket address shows it: IPv6 in brackets, so `[::]` reads apart from `:`.
fn bracketed(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// The port of each process's ready probe; the first process wins a port two probes share.
fn ready_ports(manifest: &Manifest) -> BTreeMap<u16, &str> {
    let mut owners = BTreeMap::new();
    for process in &manifest.processes {
        let port = match &process.ready {
            Some(Ready::Tcp(port)) => Some(*port),
            Some(Ready::Http(url)) => {
                Url::parse(url).ok().and_then(|url| url.port_or_known_default())
            }
            None => None,
        };
        if let Some(port) = port {
            owners.entry(port).or_insert(process.name.as_str());
        }
    }
    owners
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use proptest::prelude::*;

    use super::*;
    use crate::manifest::{Database, DbTime, Process};

    const TCP: &str = concat!(
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  ",
        "timeout inode\n",
        "   0: 0100007F:1538 00000000:0000 0A 00000000:00000000 00:00000000 00000000    70     ",
        "   0 31234 1 0000000000000000 100 0 0 10 0\n",
        "   1: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000     ",
        "   0 31240 1 0000000000000000 100 0 0 10 0\n",
        "   2: 0B00007F:A1B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0     ",
        "   0 31300 1 0000000000000000 100 0 0 10 0\n",
        "   3: 0100007F:0BB8 0100007F:D5E0 01 00000000:00000000 00:00000000 00000000  1000     ",
        "   0 31400 1 0000000000000000 20 4 30 10 -1\n",
    );

    const TCP6: &str = concat!(
        "  sl  local_address                         remote_address                        st ",
        "tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        "   0: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A ",
        "00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 ",
        "10 0\n",
        "   1: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A ",
        "00000000:00000000 00:00000000 00000000     0        0 41240 1 0000000000000000 100 0 0 ",
        "10 0\n",
        "   2: 0000000000000000FFFF00000100007F:2329 00000000000000000000000000000000:0000 0A ",
        "00000000:00000000 00:00000000 00000000     0        0 41250 1 0000000000000000 100 0 0 ",
        "10 0\n",
        "   3: 0000000000000000FFFF00000100007F:0BB8 0000000000000000FFFF00000100007F:C350 06 ",
        "00000000:00000000 03:00000F86 00000000     0        0 0 3 0000000000000000\n",
        "   4: 000080FE000000000000000001000000:0016 00000000000000000000000000000000:0000 0A ",
        "00000000:00000000 00:00000000 00000000     0        0 41260 1 0000000000000000 100 0 0 ",
        "10 0\n",
    );

    fn listener(addr: &str, port: u16) -> Listener {
        Listener { addr: addr.parse().unwrap(), port }
    }

    /// A manifest exposing `expose`, whose processes have these names and ready probes.
    fn manifest(expose: &[u16], processes: &[(&str, Option<Ready>)]) -> Manifest {
        let process = |(name, ready): &(&str, Option<Ready>)| Process {
            name: (*name).to_string(),
            command: vec!["true".into()],
            user: None,
            cwd: None,
            env: BTreeMap::new(),
            datastore: false,
            ready: ready.clone(),
        };
        Manifest {
            expose: expose.to_vec(),
            processes: processes.iter().map(process).collect(),
            env: BTreeMap::new(),
            bake: Vec::new(),
            seed: Vec::new(),
            database: Database {
                url: "postgres://postgres@127.0.0.1:5432/app".into(),
                ignore: Vec::new(),
                time: DbTime::default(),
            },
            sinks: None,
            quiet: None,
        }
    }

    /// A `/proc/net/tcp{,6}` row as the kernel prints it.
    fn row(listener: &Listener, state: &str) -> String {
        let octets = match listener.addr {
            IpAddr::V4(v4) => v4.octets().to_vec(),
            IpAddr::V6(v6) => v6.octets().to_vec(),
        };
        let mut words = String::new();
        for word in octets.chunks(4) {
            write!(words, "{:08X}", u32::from_ne_bytes(word.try_into().unwrap())).unwrap();
        }
        let zeros = "0".repeat(words.len());
        format!(
            "   0: {words}:{:04X} {zeros}:0000 {state} 00000000:00000000 00:00000000 00000000 \
             0 0 1 1 0000000000000000 100 0 0 10 0",
            listener.port
        )
    }

    #[test]
    fn ipv4_listen_rows_are_read_and_others_left_out() {
        let expected =
            [listener("127.0.0.1", 5432), listener("0.0.0.0", 3000), listener("127.0.0.11", 41394)];
        assert_eq!(parse(TCP), expected);
    }

    #[test]
    fn ipv6_listen_rows_are_read_with_their_word_order() {
        let expected = [
            listener("::", 3000),
            listener("::1", 8080),
            listener("::ffff:127.0.0.1", 9001),
            listener("fe80::1", 22),
        ];
        assert_eq!(parse(TCP6), expected);
    }

    #[test]
    fn rows_that_are_not_the_kernels_format_are_left_out() {
        let rows = [
            "",
            "garbage",
            "0: 0100007F:1538",
            "0: 0100007F:1538 00000000:0000",
            "0: 0100007:1538 00000000:0000 0A",
            "0: 0100007FF:1538 00000000:0000 0A",
            "0: 0100007X:1538 00000000:0000 0A",
            "0: 0100007F:12345 00000000:0000 0A",
            "0: 0100007F 00000000:0000 0A",
            "0: 0100007F: 00000000:0000 0A",
            "0: 01é0007F:1538 00000000:0000 0A",
            "0: 0000000000000000000000000100000:1538 00000000:0000 0A",
        ];
        for text in rows {
            assert_eq!(parse(text), [], "{text:?}");
        }
        assert_eq!(parse("0: 0100007F:1538 00000000:0000 0A"), [listener("127.0.0.1", 5432)]);
    }

    #[test]
    fn loopback_is_127_8_and_its_ipv6_forms() {
        for addr in ["127.0.0.1", "127.0.0.11", "127.255.255.254", "::1", "::ffff:127.0.0.1"] {
            assert!(is_loopback(addr.parse().unwrap()), "{addr}");
        }
        let others =
            ["0.0.0.0", "10.0.0.1", "128.0.0.1", "::", "::ffff:10.0.0.1", "::127.0.0.1", "fe80::1"];
        for addr in others {
            assert!(!is_loopback(addr.parse().unwrap()), "{addr}");
        }
    }

    #[test]
    fn a_world_on_loopback_but_its_exposed_port_passes() {
        let web = ("web", Some(Ready::Http("http://127.0.0.1:3000/healthz".into())));
        let world = manifest(&[3000], &[("db", Some(Ready::Tcp(5432))), web]);
        let listeners = [
            listener("127.0.0.1", 5432),
            listener("0.0.0.0", 3000),
            listener("127.0.0.11", 41394),
            listener("::1", 8080),
            listener("::ffff:127.0.0.1", 9000),
        ];
        assert_eq!(audit(&listeners, &world), Ok(()));
        assert_eq!(audit(&[listener("::", 3000)], &world), Ok(()), "IPv6 any is off loopback");
    }

    #[test]
    fn an_unexposed_listener_off_loopback_fails_naming_its_process() {
        let world = manifest(&[3000], &[("db", Some(Ready::Tcp(5432))), ("cache", None)]);
        let listeners = [listener("0.0.0.0", 5432), listener("0.0.0.0", 3000)];
        assert_eq!(
            audit(&listeners, &world),
            Err("port 5432 (db) listens on 0.0.0.0: bind it to 127.0.0.1, or list it in expose"
                .into())
        );
        let api = ("api", Some(Ready::Http("http://127.0.0.1/healthz".into())));
        let world = manifest(&[3000], &[api]);
        let listeners = [listener("10.0.0.5", 80), listener("::", 3000)];
        let error = audit(&listeners, &world).unwrap_err();
        assert_eq!(
            error,
            "port 80 (api) listens on 10.0.0.5: bind it to 127.0.0.1, or list it in expose"
        );
    }

    #[test]
    fn an_unexposed_listener_no_probe_uses_fails_without_a_name() {
        let world = manifest(&[3000], &[("db", Some(Ready::Tcp(5432)))]);
        let listeners = [listener("0.0.0.0", 3000), listener("::", 6379)];
        assert_eq!(
            audit(&listeners, &world),
            Err("port 6379 listens on [::]: bind it to 127.0.0.1, or list it in expose".into())
        );
    }

    #[test]
    fn an_exposed_port_nothing_listens_on_off_loopback_fails() {
        let web = ("web", Some(Ready::Http("http://127.0.0.1:3000/healthz".into())));
        let world = manifest(&[3000], &[web]);
        assert_eq!(
            audit(&[listener("127.0.0.1", 3000)], &world),
            Err("expose lists 3000 (web), but nothing listens on it off loopback: check the \
                 product's bind address"
                .into())
        );
        let world = manifest(&[4000], &[]);
        assert_eq!(
            audit(&[], &world),
            Err("expose lists 4000, but nothing listens on it off loopback: check the \
                 product's bind address"
                .into())
        );
    }

    #[test]
    fn every_violation_is_listed_once() {
        let world = manifest(&[3000, 4000], &[("db", Some(Ready::Tcp(5432)))]);
        let twice = listener("0.0.0.0", 5432);
        let listeners = [twice.clone(), twice, listener("::", 5432), listener("0.0.0.0", 3000)];
        let error = audit(&listeners, &world).unwrap_err();
        let lines: Vec<&str> = error.lines().collect();
        assert_eq!(
            lines,
            [
                "port 5432 (db) listens on 0.0.0.0: bind it to 127.0.0.1, or list it in expose",
                "port 5432 (db) listens on [::]: bind it to 127.0.0.1, or list it in expose",
                "expose lists 4000, but nothing listens on it off loopback: check the product's \
                 bind address",
            ]
        );
    }

    #[test]
    fn the_first_process_names_a_port_two_probes_share() {
        let first = ("first", Some(Ready::Tcp(7000)));
        let second = ("second", Some(Ready::Http("http://127.0.0.1:7000/".into())));
        let world = manifest(&[3000], &[first, second]);
        let error = audit(&[listener("0.0.0.0", 7000), listener("0.0.0.0", 3000)], &world);
        assert!(error.unwrap_err().starts_with("port 7000 (first) listens"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_lists_this_namespaces_listeners() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        assert!(current().unwrap().contains(&listener("127.0.0.1", port)));
    }

    #[test]
    fn both_tables_are_read_and_a_missing_ipv6_table_lists_none() {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir =
            std::env::temp_dir().join(format!("miroir-listen-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tcp, tcp6) = (dir.join("tcp"), dir.join("tcp6"));
        std::fs::write(&tcp, TCP).unwrap();
        assert_eq!(tables(&tcp, &tcp6).unwrap(), parse(TCP), "a kernel without IPv6");
        std::fs::write(&tcp6, TCP6).unwrap();
        let both: Vec<Listener> = parse(TCP).into_iter().chain(parse(TCP6)).collect();
        assert_eq!(tables(&tcp, &tcp6).unwrap(), both);
        let error = tables(&tcp, &dir).unwrap_err();
        assert_ne!(error.kind(), ErrorKind::NotFound, "an unreadable IPv6 table is an error");
        assert!(error.to_string().starts_with(&format!("cannot read {}: ", dir.display())));
        let missing = dir.join("missing");
        let error = tables(&missing, &tcp6).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.to_string().starts_with(&format!("cannot read {}: ", missing.display())));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn any_listener() -> impl Strategy<Value = Listener> {
        let addr = prop_oneof![
            any::<[u8; 4]>().prop_map(IpAddr::from),
            any::<[u8; 16]>().prop_map(IpAddr::from),
        ];
        (addr, any::<u16>()).prop_map(|(addr, port)| Listener { addr, port })
    }

    proptest! {
        #[test]
        fn parse_never_panics(text in any::<String>()) {
            parse(&text);
        }

        #[test]
        fn parse_never_panics_on_rowlike_text(text in "[ 0-9A-Fa-f:\\né]{0,120}") {
            parse(&text);
        }

        #[test]
        fn formatted_rows_round_trip(
            listeners in prop::collection::vec(any_listener(), 0..6),
            state in prop_oneof![Just("0A"), Just("01"), Just("06")],
        ) {
            let table: String = listeners.iter().map(|l| row(l, state) + "\n").collect();
            let expected = if state == LISTEN { listeners } else { Vec::new() };
            prop_assert_eq!(parse(&table), expected);
        }
    }
}
