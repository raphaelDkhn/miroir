//! Sinks: loopback SMTP and HTTP servers that stand in for the outside world, recording
//! each email and call as an effect.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use mail_parser::MessageParser;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::edb::EpisodeDb;
use crate::manifest::{CannedResponse, Sinks};

const MAX_MESSAGE: usize = 25 * 1024 * 1024;
const MAX_LINE: u64 = 64 * 1024;
const MAX_RECIPIENTS: usize = 1000;

/// Start the sinks the manifest names; without `db` they record nothing.
pub async fn start(sinks: &Sinks, db: Option<Arc<EpisodeDb>>) -> std::io::Result<()> {
    if let Some(port) = sinks.smtp {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let db = db.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let db = db.clone();
                        tokio::spawn(async move {
                            if let Err(e) = smtp_session(stream, db).await {
                                tracing::warn!("SMTP sink: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("SMTP sink cannot accept a connection: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });
    }
    if let Some(port) = sinks.http {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let state = Arc::new((sinks.responses.clone(), db));
        let router = axum::Router::new()
            .fallback(record_http)
            .layer(axum::extract::DefaultBodyLimit::max(MAX_MESSAGE))
            .with_state(state);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("HTTP sink stopped: {e}");
            }
        });
    }
    Ok(())
}

async fn smtp_session(stream: TcpStream, db: Option<Arc<EpisodeDb>>) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut sender: Option<String> = None;
    let mut recipients: Vec<String> = Vec::new();
    write.write_all(b"220 miroir ESMTP sink\r\n").await?;
    loop {
        let Some(line) = read_line(&mut reader).await? else { return Ok(()) };
        let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let reply: &[u8] = match verb.to_ascii_uppercase().as_str() {
            "EHLO" => b"250-miroir\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n",
            "HELO" => b"250 miroir\r\n",
            "AUTH" => {
                authenticate(rest, &mut reader, &mut write).await?;
                b"235 2.7.0 Authentication successful\r\n"
            }
            "MAIL" => {
                sender = address(rest);
                recipients.clear();
                b"250 OK\r\n"
            }
            "RCPT" if recipients.len() >= MAX_RECIPIENTS => b"452 4.5.3 Too many recipients\r\n",
            "RCPT" => {
                recipients.extend(address(rest));
                b"250 OK\r\n"
            }
            "DATA" => {
                write.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n").await?;
                let raw = read_data(&mut reader).await?;
                if let Some(db) = &db {
                    db.add_effect("email", &email(&raw, sender.take(), &recipients)).await;
                }
                recipients.clear();
                b"250 OK\r\n"
            }
            "RSET" => {
                sender = None;
                recipients.clear();
                b"250 OK\r\n"
            }
            "NOOP" => b"250 OK\r\n",
            "QUIT" => {
                write.write_all(b"221 Bye\r\n").await?;
                return Ok(());
            }
            _ => b"502 Command not implemented\r\n",
        };
        write.write_all(reply).await?;
    }
}

async fn authenticate(
    rest: &str,
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: &mut tokio::net::tcp::OwnedWriteHalf,
) -> std::io::Result<()> {
    let mut parts = rest.split_whitespace();
    let mechanism = parts.next().unwrap_or_default().to_ascii_uppercase();
    let initial = parts.next().is_some();
    let prompts: &[&[u8]] = match (mechanism.as_str(), initial) {
        ("PLAIN", false) => &[b"334 \r\n"],
        ("LOGIN", false) => &[b"334 VXNlcm5hbWU6\r\n", b"334 UGFzc3dvcmQ6\r\n"],
        ("LOGIN", true) => &[b"334 UGFzc3dvcmQ6\r\n"],
        _ => &[],
    };
    for prompt in prompts {
        write.write_all(prompt).await?;
        read_line(reader).await?;
    }
    Ok(())
}

async fn read_line(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    if (&mut *reader).take(MAX_LINE).read_until(b'\n', &mut line).await? == 0 {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string()))
}

async fn read_data(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> std::io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut line_start = true;
    let mut split_crlf = false;
    loop {
        let mut line = Vec::new();
        if (&mut *reader).take(MAX_LINE).read_until(b'\n', &mut line).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed in DATA",
            ));
        }
        if line_start && line == b".\r\n" {
            return Ok(raw);
        }
        let content = if line_start { line.strip_prefix(b".").unwrap_or(&line) } else { &line };
        if raw.len() + content.len() <= MAX_MESSAGE {
            raw.extend_from_slice(content);
        }
        line_start = line.ends_with(b"\r\n") || (split_crlf && line == b"\n");
        split_crlf = line.ends_with(b"\r");
    }
}

fn address(argument: &str) -> Option<String> {
    let start = argument.find('<')? + 1;
    let end = start + argument[start..].find('>')?;
    Some(argument[start..end].to_string()).filter(|a| !a.is_empty())
}

/// An email effect: sender, recipients split into to, cc and bcc, subject and bodies.
pub fn email(raw: &[u8], sender: Option<String>, recipients: &[String]) -> Value {
    let Some(message) = MessageParser::default().parse(raw) else {
        return json!({
            "from": sender,
            "to": [],
            "cc": [],
            "bcc": recipients,
            "subject": null,
            "text": null,
            "html": null,
        });
    };
    let list = |addresses: Option<mail_parser::AddressList>| -> Vec<String> {
        addresses
            .map(|l| l.mailboxes().filter_map(|m| m.address().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let (to, cc) = (list(message.to()), list(message.cc()));
    let named: HashSet<String> = to.iter().chain(&cc).map(|a| a.to_ascii_lowercase()).collect();
    let bcc: Vec<&String> =
        recipients.iter().filter(|r| !named.contains(&r.to_ascii_lowercase())).collect();
    let from = message
        .from()
        .and_then(|f| f.first())
        .and_then(|m| m.address().map(str::to_string))
        .or(sender);
    let text = message
        .text_body()
        .position(|p| !p.is_content_type("text", "html"))
        .and_then(|i| message.body_text(i));
    let html = message
        .html_body()
        .position(|p| p.is_content_type("text", "html"))
        .and_then(|i| message.body_html(i));
    json!({
        "from": from,
        "to": to,
        "cc": cc,
        "bcc": bcc,
        "subject": message.subject(),
        "text": text,
        "html": html,
    })
}

type HttpState = Arc<(BTreeMap<String, CannedResponse>, Option<Arc<EpisodeDb>>)>;

async fn record_http(
    State(state): State<HttpState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let (responses, db) = state.as_ref();
    let body = match body {
        Ok(body) => body,
        Err(refused) => {
            if let Some(db) = db {
                db.refuse(format!("the HTTP sink cannot read {method} {}: {refused}", uri.path()));
            }
            return refused.into_response();
        }
    };
    let mut query = Map::new();
    if let Some(raw) = uri.query()
        && let Ok(url) = reqwest::Url::parse(&format!("http://sink/?{raw}"))
    {
        for (key, value) in url.query_pairs() {
            query.insert(key.into_owned(), Value::String(value.into_owned()));
        }
    }
    let mut kept = Map::new();
    for (name, value) in &headers {
        if name != "authorization" {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            let entry = kept.entry(name.as_str()).or_insert_with(|| Value::String(String::new()));
            let joined = match entry.as_str() {
                Some("") | None => value,
                Some(previous) => format!("{previous}, {value}"),
            };
            *entry = Value::String(joined);
        }
    }
    let parsed = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    if let Some(db) = db {
        let effect = json!({
            "method": method.as_str(),
            "path": uri.path(),
            "query": query,
            "headers": kept,
            "body": parsed,
        });
        db.add_effect("http", &effect).await;
    }
    respond(matching(responses, method.as_str(), uri.path()))
}

/// The canned response for a request: exact paths beat prefixes, and longer prefixes
/// win.
pub fn matching<'a>(
    responses: &'a BTreeMap<String, CannedResponse>,
    method: &str,
    path: &str,
) -> Option<&'a CannedResponse> {
    let mut best: Option<((bool, usize, bool), &CannedResponse)> = None;
    for (key, response) in responses {
        let (key_method, pattern) = match key.split_once(' ') {
            Some((m, p)) => (Some(m), p),
            None => (None, key.as_str()),
        };
        if key_method.is_some_and(|m| m != method) {
            continue;
        }
        let rank = match pattern.strip_suffix('*') {
            Some(prefix) if path.starts_with(prefix) => (false, prefix.len(), key_method.is_some()),
            None if path == pattern => (true, pattern.len(), key_method.is_some()),
            _ => continue,
        };
        if best.is_none_or(|(top, _)| rank > top) {
            best = Some((rank, response));
        }
    }
    best.map(|(_, response)| response)
}

fn respond(canned: Option<&CannedResponse>) -> Response {
    let Some(canned) = canned else {
        return (StatusCode::OK, axum::Json(json!({}))).into_response();
    };
    let status = StatusCode::from_u16(canned.status).unwrap_or(StatusCode::OK);
    let mut response = match (&canned.json, &canned.body) {
        (Some(json), _) => (status, axum::Json(json.clone())).into_response(),
        (None, Some(body)) => (status, body.clone()).into_response(),
        (None, None) => status.into_response(),
    };
    for (name, value) in &canned.headers {
        if let (Ok(name), Ok(value)) = (name.parse::<axum::http::HeaderName>(), value.parse()) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canned(status: u16) -> CannedResponse {
        CannedResponse { status, json: None, body: None, headers: BTreeMap::new() }
    }

    #[test]
    fn exact_beats_wildcard_and_longer_wildcards_win() {
        let responses: BTreeMap<String, CannedResponse> = [
            ("POST /stripe/v1/payment_intents", 201),
            ("/stripe/*", 202),
            ("/stripe/v1/*", 203),
            ("GET /stripe/v1/*", 204),
            ("/hooks/*", 205),
        ]
        .into_iter()
        .map(|(k, s)| (k.to_string(), canned(s)))
        .collect();
        let status = |m: &str, p: &str| matching(&responses, m, p).map(|r| r.status);
        assert_eq!(status("POST", "/stripe/v1/payment_intents"), Some(201));
        assert_eq!(status("GET", "/stripe/v1/payment_intents"), Some(204));
        assert_eq!(status("DELETE", "/stripe/v1/customers/1"), Some(203));
        assert_eq!(status("POST", "/stripe/other"), Some(202));
        assert_eq!(status("PUT", "/hooks/a/b"), Some(205));
        assert_eq!(status("GET", "/elsewhere"), None);
    }

    #[test]
    fn email_effect_splits_bcc_and_keeps_real_parts() {
        let raw =
            b"From: Acme <noreply@acme.test>\r\nTo: marcus@acme.test\r\nCc: dana@acme.test\r\n\
Subject: Deal 1042 is now yours\r\nContent-Type: text/plain\r\n\r\nHello\r\n";
        let rcpts =
            ["marcus@acme.test".to_string(), "dana@acme.test".into(), "audit@acme.test".into()];
        let effect = email(raw, Some("bounce@acme.test".into()), &rcpts);
        assert_eq!(effect["from"], "noreply@acme.test");
        assert_eq!(effect["to"], json!(["marcus@acme.test"]));
        assert_eq!(effect["cc"], json!(["dana@acme.test"]));
        assert_eq!(effect["bcc"], json!(["audit@acme.test"]));
        assert_eq!(effect["subject"], "Deal 1042 is now yours");
        assert_eq!(effect["text"], "Hello\r\n");
        assert_eq!(effect["html"], Value::Null);
    }

    #[tokio::test]
    async fn smtp_session_records_what_the_client_sent() {
        let path = std::env::temp_dir().join(format!("miroir-smtp-{}.db", std::process::id()));
        let db = Arc::new(EpisodeDb::create(&path).await.unwrap());
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = tokio::spawn({
            let db = db.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                smtp_session(stream, Some(db)).await
            }
        });
        let mut client = BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        let mut reply = String::new();
        let mut expect = async |client: &mut BufReader<TcpStream>, send: &str, code: &str| {
            client.write_all(send.as_bytes()).await.unwrap();
            loop {
                reply.clear();
                client.read_line(&mut reply).await.unwrap();
                if reply.as_bytes().get(3) != Some(&b'-') {
                    break;
                }
            }
            assert!(reply.starts_with(code), "{send:?} got {reply:?}");
        };
        expect(&mut client, "", "220").await;
        expect(&mut client, "EHLO x\r\n", "250").await;
        expect(&mut client, "AUTH PLAIN\r\n", "334").await;
        expect(&mut client, "secret\r\n", "235").await;
        expect(&mut client, "AUTH LOGIN\r\n", "334").await;
        expect(&mut client, "user\r\n", "334").await;
        expect(&mut client, "password\r\n", "235").await;
        expect(&mut client, "MAIL FROM:<dropped@x.test>\r\n", "250").await;
        expect(&mut client, "RCPT TO:<dropped@x.test>\r\n", "250").await;
        expect(&mut client, "RSET\r\n", "250").await;
        expect(&mut client, "NOOP\r\n", "250").await;
        expect(&mut client, "VRFY x\r\n", "502").await;
        expect(&mut client, "MAIL FROM:<a@x.test>\r\n", "250").await;
        expect(&mut client, "RCPT TO:<b@x.test>\r\n", "250").await;
        for i in 1..MAX_RECIPIENTS {
            expect(&mut client, &format!("RCPT TO:<c{i}@x.test>\r\n"), "250").await;
        }
        expect(&mut client, "RCPT TO:<over@x.test>\r\n", "452").await;
        expect(&mut client, "DATA\r\n", "354").await;
        let message = "To: b@x.test\r\nSubject: Hi\r\n\r\n..dots\r\nbare\n.\nmore\r\n.\r\n";
        expect(&mut client, message, "250").await;
        let at_the_read_limit = "x".repeat(usize::try_from(MAX_LINE).unwrap() - 1);
        expect(&mut client, "DATA\r\n", "354").await;
        expect(&mut client, &format!("{at_the_read_limit}\r\n.\r\n"), "250").await;
        expect(&mut client, "QUIT\r\n", "221").await;
        session.await.unwrap().unwrap();

        assert_eq!(db.effect_count().await.unwrap(), 2, "RSET dropped the first message");
        let sql = "SELECT json_extract(data, '$.from') || '|' || json_extract(data, '$.text') \
                   FROM effects";
        let recorded = db.check(sql).await.unwrap().unwrap();
        assert_eq!(recorded, "a@x.test|.dots\r\nbare\n.\nmore\r\n");
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn a_body_too_big_to_record_is_the_episodes_fault() {
        let path = std::env::temp_dir().join(format!("miroir-sink-{}.db", std::process::id()));
        let db = Arc::new(EpisodeDb::create(&path).await.unwrap());
        let port = TcpListener::bind(("127.0.0.1", 0)).await.unwrap().local_addr().unwrap().port();
        let sinks = Sinks { smtp: None, http: Some(port), responses: BTreeMap::new() };
        start(&sinks, Some(db.clone())).await.unwrap();
        let (client, url) = (reqwest::Client::new(), format!("http://127.0.0.1:{port}/s3/dump"));
        assert_eq!(client.put(&url).body("small").send().await.unwrap().status(), 200);
        assert_eq!(db.refused(), None);
        let huge = client.put(&url).body(vec![b'x'; MAX_MESSAGE + 1]).send().await;
        assert!(huge.is_err() || huge.is_ok_and(|r| r.status() == 413));
        let refused = db.refused().unwrap_or_default();
        assert!(refused.contains("PUT /s3/dump"), "the refused upload cannot count as never made");
        assert_eq!(db.lost(), None, "miroir lost nothing");
        assert_eq!(db.effect_count().await.unwrap(), 1);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn smtp_addresses() {
        assert_eq!(address("FROM:<a@b.test> SIZE=10"), Some("a@b.test".into()));
        assert_eq!(address("FROM:<>"), None);
        assert_eq!(address("TO:nobody"), None);
    }
}
