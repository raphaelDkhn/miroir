//! The loopback SMTP and HTTP sinks: they record the email and requests the product sends, as
//! `effects.jsonl` lines (§11.1 journal shapes), and answer HTTP with canned responses. A `sink`
//! fault line is appended when the HTTP sink cannot read a request (its body is over 25 MiB)
//! and when an email over 25 MiB is recorded truncated.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use mail_parser::MessageParser;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

use crate::evidence::fault;
use crate::journal;
use crate::manifest::{CannedResponse, Sinks};

/// The largest email or request body recorded whole.
const MAX_MESSAGE: usize = 25 * 1024 * 1024;
const MAX_LINE: u64 = 64 * 1024;
const MAX_RECIPIENTS: usize = 1000;
const OK: &[u8] = b"250 OK\r\n";

/// The journals the sinks record to.
#[derive(Debug, Clone)]
pub struct Recording {
    pub effects: PathBuf,
    pub faults: PathBuf,
}

impl Recording {
    async fn effect(&self, kind: &str, data: Value) {
        append(self.effects.clone(), json!({"kind": kind, "data": data})).await;
    }

    async fn fault(&self, detail: String) {
        append(self.faults.clone(), json!({"kind": fault::SINK, "detail": detail})).await;
    }
}

/// Append a journal line off the async threads; a line that cannot be written is logged.
async fn append(path: PathBuf, line: Value) {
    let shown = path.display().to_string();
    let written = tokio::task::spawn_blocking(move || journal::append(&path, &line)).await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!("the sinks cannot record to {shown}: {e}"),
        Err(e) => tracing::error!("the sinks cannot record to {shown}: {e}"),
    }
}

/// Start the sinks the manifest lists, recording to `recording` when it is set; without it
/// they answer and record nothing.
pub async fn start(sinks: &Sinks, recording: Option<Recording>) -> std::io::Result<()> {
    let recording = recording.map(Arc::new);
    if let Some(port) = sinks.smtp {
        let listener = bind("SMTP", port).await?;
        tokio::spawn(serve_smtp(listener, recording.clone()));
    }
    if let Some(port) = sinks.http {
        let listener = bind("HTTP", port).await?;
        let sink = Arc::new(HttpSink { responses: sinks.responses.clone(), recording });
        let router = axum::Router::new()
            .fallback(record_http)
            .layer(DefaultBodyLimit::max(MAX_MESSAGE))
            .with_state(sink);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("the HTTP sink stopped: {e}");
            }
        });
    }
    Ok(())
}

async fn bind(sink: &str, port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        let message = format!(
            "the {sink} sink cannot listen on 127.0.0.1:{port}: {e}; free the port, or give \
             the sink another one in the manifest's sinks"
        );
        std::io::Error::new(e.kind(), message)
    })
}

async fn serve_smtp(listener: TcpListener, recording: Option<Arc<Recording>>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let recording = recording.clone();
                tokio::spawn(async move {
                    if let Err(e) = smtp_session(stream, recording.as_deref()).await {
                        tracing::warn!("SMTP sink: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::error!("the SMTP sink cannot accept a connection: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// The envelope of the mail transaction in progress.
#[derive(Default)]
struct Envelope {
    sender: Option<String>,
    recipients: Vec<String>,
}

impl Envelope {
    /// Add the recipient of a RCPT command, up to `MAX_RECIPIENTS`.
    fn add(&mut self, argument: &str) -> &'static [u8] {
        if self.recipients.len() >= MAX_RECIPIENTS {
            return b"452 4.5.3 Too many recipients\r\n";
        }
        self.recipients.extend(address(argument));
        OK
    }
}

/// What a command line asks of the session.
enum Next {
    Reply(&'static [u8]),
    Auth(String),
    Data,
    Quit,
}

async fn smtp_session(stream: TcpStream, recording: Option<&Recording>) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut envelope = Envelope::default();
    write.write_all(b"220 miroir ESMTP sink\r\n").await?;
    loop {
        let Some(line) = read_line(&mut reader).await? else { return Ok(()) };
        let reply = match command(&mut envelope, &line) {
            Next::Reply(reply) => reply,
            Next::Auth(rest) => {
                authenticate(&rest, &mut reader, &mut write).await?;
                b"235 2.7.0 Authentication successful\r\n"
            }
            Next::Data => {
                write.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n").await?;
                let (raw, size) = read_data(&mut reader).await?;
                let envelope = std::mem::take(&mut envelope);
                if let Some(recording) = recording {
                    record_email(recording, &raw, size, envelope).await;
                }
                OK
            }
            Next::Quit => {
                write.write_all(b"221 Bye\r\n").await?;
                return Ok(());
            }
        };
        write.write_all(reply).await?;
    }
}

fn command(envelope: &mut Envelope, line: &str) -> Next {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    match verb.to_ascii_uppercase().as_str() {
        "MAIL" => {
            *envelope = Envelope { sender: address(rest), recipients: Vec::new() };
            Next::Reply(OK)
        }
        "RCPT" => Next::Reply(envelope.add(rest)),
        "RSET" => {
            *envelope = Envelope::default();
            Next::Reply(OK)
        }
        "AUTH" => Next::Auth(rest.to_string()),
        "DATA" => Next::Data,
        "QUIT" => Next::Quit,
        other => Next::Reply(stateless_reply(other)),
    }
}

fn stateless_reply(verb: &str) -> &'static [u8] {
    match verb {
        "EHLO" => b"250-miroir\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n",
        "HELO" => b"250 miroir\r\n",
        "NOOP" => OK,
        _ => b"502 Command not implemented\r\n",
    }
}

async fn record_email(recording: &Recording, raw: &[u8], size: usize, envelope: Envelope) {
    let Envelope { sender, recipients } = envelope;
    if size > MAX_MESSAGE {
        let from = sender.as_deref().unwrap_or("no sender");
        recording
            .fault(format!(
                "an email from {from} of {size} bytes was recorded truncated to its first \
                 {MAX_MESSAGE} bytes"
            ))
            .await;
    }
    recording.effect("email", email(raw, sender, &recipients)).await;
}

async fn authenticate(
    rest: &str,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
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

/// The next line, at most `MAX_LINE` bytes of it, with its line ending; `None` at the end.
async fn read_raw(reader: &mut BufReader<OwnedReadHalf>) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = (&mut *reader).take(MAX_LINE).read_until(b'\n', &mut line).await?;
    Ok((read > 0).then_some(line))
}

async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> std::io::Result<Option<String>> {
    let line = read_raw(reader).await?;
    Ok(line.map(|line| String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string()))
}

/// The message of a DATA command, dot-unstuffed and cut at `MAX_MESSAGE` bytes, and its whole
/// size.
async fn read_data(reader: &mut BufReader<OwnedReadHalf>) -> std::io::Result<(Vec<u8>, usize)> {
    let mut raw = Vec::new();
    let mut size = 0;
    let mut line_start = true;
    let mut split_crlf = false;
    loop {
        let Some(line) = read_raw(reader).await? else {
            let message = "connection closed in DATA";
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, message));
        };
        if line_start && line == b".\r\n" {
            return Ok((raw, size));
        }
        let content = if line_start { line.strip_prefix(b".").unwrap_or(&line) } else { &line };
        size += content.len();
        if size <= MAX_MESSAGE {
            raw.extend_from_slice(content);
        }
        line_start = ends_line(&line, split_crlf);
        split_crlf = line.ends_with(b"\r");
    }
}

/// Whether `line` ends a line: with CRLF, or with the LF of a CRLF that two reads split.
fn ends_line(line: &[u8], split_crlf: bool) -> bool {
    line.ends_with(b"\r\n") || (split_crlf && line == b"\n")
}

fn address(argument: &str) -> Option<String> {
    let start = argument.find('<')? + 1;
    let end = start + argument[start..].find('>')?;
    Some(argument[start..end].to_string()).filter(|a| !a.is_empty())
}

/// The `data` of an email line: the sender, the recipients split into to, cc and bcc, the
/// subject and the bodies.
fn email(raw: &[u8], sender: Option<String>, recipients: &[String]) -> Value {
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

struct HttpSink {
    responses: BTreeMap<String, CannedResponse>,
    recording: Option<Arc<Recording>>,
}

async fn record_http(
    State(sink): State<Arc<HttpSink>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(refused) => {
            if let Some(recording) = &sink.recording {
                let path = uri.path();
                recording
                    .fault(format!("the HTTP sink cannot read {method} {path}: {refused}"))
                    .await;
            }
            return refused.into_response();
        }
    };
    if let Some(recording) = &sink.recording {
        recording.effect("http", request(&method, &uri, &headers, &body)).await;
    }
    respond(matching(&sink.responses, method.as_str(), uri.path()))
}

/// The `data` of an http line: query parameters, header names lower-case without
/// `authorization`, and the body as JSON, a JSON string of its text, or null when empty.
fn request(method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Value {
    let mut query = BTreeMap::new();
    if let Some(raw) = uri.query()
        && let Ok(url) = Url::parse(&format!("http://sink/?{raw}"))
    {
        query.extend(url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())));
    }
    let mut kept: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        if name == header::AUTHORIZATION {
            continue;
        }
        let value = String::from_utf8_lossy(value.as_bytes());
        kept.entry(name.as_str().to_string())
            .and_modify(|joined| {
                joined.push_str(", ");
                joined.push_str(&value);
            })
            .or_insert_with(|| value.into_owned());
    }
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
    };
    json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": query,
        "headers": kept,
        "body": body,
    })
}

/// The canned response for a request: exact paths beat prefixes, and longer prefixes win.
fn matching<'a>(
    responses: &'a BTreeMap<String, CannedResponse>,
    method: &str,
    path: &str,
) -> Option<&'a CannedResponse> {
    let mut best: Option<((bool, usize, bool), &CannedResponse)> = None;
    for (key, response) in responses {
        let Some(rank) = rank(key, method, path) else { continue };
        if best.is_none_or(|(top, _)| rank > top) {
            best = Some((rank, response));
        }
    }
    best.map(|(_, response)| response)
}

/// How well a `"[METHOD ]/path[*]"` key matches a request, higher being better: an exact path,
/// then a longer one, then a method; `None` when it does not match.
fn rank(key: &str, method: &str, path: &str) -> Option<(bool, usize, bool)> {
    let (key_method, pattern) = match key.split_once(' ') {
        Some((key_method, pattern)) => (Some(key_method), pattern),
        None => (None, key),
    };
    if key_method.is_some_and(|key_method| key_method != method) {
        return None;
    }
    let named = key_method.is_some();
    match pattern.strip_suffix('*') {
        Some(prefix) => path.starts_with(prefix).then_some((false, prefix.len(), named)),
        None => (path == pattern).then_some((true, pattern.len(), named)),
    }
}

/// The canned response, or 200 with an empty body.
fn respond(canned: Option<&CannedResponse>) -> Response {
    let Some(canned) = canned else { return StatusCode::OK.into_response() };
    let status = StatusCode::from_u16(canned.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = match (&canned.json, &canned.body) {
        (Some(json), _) => (status, axum::Json(json.clone())).into_response(),
        (None, Some(body)) => (status, body.clone()).into_response(),
        (None, None) => status.into_response(),
    };
    for (name, value) in &canned.headers {
        if let (Ok(name), Ok(value)) =
            (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value))
        {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::task::JoinHandle;

    use super::*;
    use crate::bake::tests::{TempRoot, temp_root};
    use crate::evidence::{self, Evidence};

    const LONG: Duration = Duration::from_secs(10);
    /// The 25 MiB of §4.4, spelled out so the tests pin `MAX_MESSAGE`.
    const LIMIT: usize = 25 * 1024 * 1024;

    fn canned(status: u16) -> CannedResponse {
        CannedResponse { status, json: None, body: None, headers: BTreeMap::new() }
    }

    /// A new directory of this test's own, and journals in it.
    fn recording(name: &str) -> (TempRoot, Recording) {
        let dir = temp_root(name);
        let recording =
            Recording { effects: dir.join("effects.jsonl"), faults: dir.join("faults.jsonl") };
        (dir, recording)
    }

    /// A loopback TCP port that nothing listens on.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn lines(path: &Path) -> Vec<Value> {
        journal::read(path).unwrap()
    }

    /// Wait until the journal at `path` holds `count` lines, for at most `LONG`.
    async fn wait_for_lines(path: &Path, count: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + LONG;
        loop {
            let read = lines(path);
            if read.len() >= count {
                return read;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} has {} lines",
                path.display(),
                read.len()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// One SMTP session on a new loopback port, recording to `recording`.
    async fn one_session(recording: Recording) -> (u16, JoinHandle<std::io::Result<()>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            smtp_session(stream, Some(&recording)).await
        });
        (port, session)
    }

    /// An SMTP client that sends a line and checks the code of the reply.
    struct Client {
        stream: BufReader<TcpStream>,
    }

    impl Client {
        async fn connect(port: u16) -> Client {
            let mut client = Client {
                stream: BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap()),
            };
            client.expect("", "220").await;
            client
        }

        async fn expect(&mut self, send: &str, code: &str) {
            let shown: String = send.chars().take(60).collect();
            self.stream.write_all(send.as_bytes()).await.unwrap();
            let mut reply = String::new();
            loop {
                reply.clear();
                tokio::time::timeout(LONG, self.stream.read_line(&mut reply))
                    .await
                    .unwrap_or_else(|_| panic!("no reply to {shown:?}"))
                    .unwrap();
                if reply.as_bytes().get(3) != Some(&b'-') {
                    break;
                }
            }
            assert!(reply.starts_with(code), "{shown:?} got {reply:?}");
        }
    }

    #[test]
    fn exact_beats_wildcard_and_longer_wildcards_win() {
        let responses: BTreeMap<String, CannedResponse> = [
            ("POST /stripe/v1/payment_intents", 201),
            ("/stripe/*", 202),
            ("/stripe/v1/*", 203),
            ("GET /stripe/v1/*", 204),
            ("/hooks/*", 205),
            ("/exact", 206),
            ("GET /exact", 207),
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
        assert_eq!(status("GET", "/exact"), Some(207), "a method breaks a tie");
        assert_eq!(status("PUT", "/exact"), Some(206));
        assert_eq!(status("PUT", "/exact/more"), None);
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
        assert_eq!(
            effect,
            json!({
                "from": "noreply@acme.test", "to": ["marcus@acme.test"], "cc": ["dana@acme.test"],
                "bcc": ["audit@acme.test"], "subject": "Deal 1042 is now yours",
                "text": "Hello\r\n", "html": null,
            })
        );
        let unparsed = email(b"", Some("bounce@acme.test".into()), &rcpts[..1]);
        assert_eq!(
            unparsed,
            json!({"from": "bounce@acme.test", "to": [], "cc": [], "bcc": ["marcus@acme.test"],
                   "subject": null, "text": null, "html": null})
        );
    }

    #[tokio::test]
    async fn smtp_session_records_what_the_client_sent() {
        let (_dir, recording) = recording("smtp");
        let effects = recording.effects.clone();
        let (port, session) = one_session(recording).await;
        let mut client = Client::connect(port).await;
        client.expect("EHLO x\r\n", "250").await;
        client.expect("AUTH PLAIN\r\n", "334").await;
        client.expect("secret\r\n", "235").await;
        client.expect("AUTH LOGIN\r\n", "334").await;
        client.expect("user\r\n", "334").await;
        client.expect("password\r\n", "235").await;
        client.expect("AUTH LOGIN dXNlcg==\r\n", "334").await;
        client.expect("password\r\n", "235").await;
        client.expect("HELO x\r\n", "250").await;
        client.expect("MAIL FROM:<dropped@x.test>\r\n", "250").await;
        client.expect("RCPT TO:<dropped@x.test>\r\n", "250").await;
        client.expect("RSET\r\n", "250").await;
        client.expect("NOOP\r\n", "250").await;
        client.expect("VRFY x\r\n", "502").await;
        client.expect("MAIL FROM:<a@x.test>\r\n", "250").await;
        client.expect("RCPT TO:<b@x.test>\r\n", "250").await;
        for i in 1..MAX_RECIPIENTS {
            client.expect(&format!("RCPT TO:<c{i}@x.test>\r\n"), "250").await;
        }
        client.expect("RCPT TO:<over@x.test>\r\n", "452").await;
        client.expect("DATA\r\n", "354").await;
        let message =
            "To: b@x.test\r\nSubject: Hi\r\n\r\n..dots\r\nbare\n.\nmore\r\n\n.\r\nend\r\n.\r\n";
        client.expect(message, "250").await;
        let at_the_read_limit = "x".repeat(64 * 1024 - 1);
        client.expect("DATA\r\n", "354").await;
        client.expect(&format!("{at_the_read_limit}\r\n.\r\n"), "250").await;
        let long = format!("{}@x.test", "l".repeat(5000));
        client.expect("MAIL FROM:<long@x.test>\r\n", "250").await;
        client.expect(&format!("RCPT TO:<{long}>\r\n"), "250").await;
        client.expect("DATA\r\n", "354").await;
        client.expect("Subject: Long\r\n\r\nx\r\n.\r\n", "250").await;
        client.expect("QUIT\r\n", "221").await;
        session.await.unwrap().unwrap();

        let recorded = lines(&effects);
        assert_eq!(recorded.len(), 3, "RSET dropped the first message");
        assert_eq!(recorded[0]["kind"], "email");
        let first = &recorded[0]["data"];
        assert_eq!(first["from"], "a@x.test");
        assert_eq!(first["to"], json!(["b@x.test"]));
        assert_eq!(first["bcc"].as_array().unwrap().len(), MAX_RECIPIENTS - 1);
        assert_eq!(first["subject"], "Hi");
        assert_eq!(
            first["text"], ".dots\r\nbare\n.\nmore\r\n\n.\r\nend\r\n",
            "only CRLF.CRLF ends the data; a dot after a bare LF is text"
        );
        let second = &recorded[1]["data"];
        assert_eq!(second["from"], Value::Null, "DATA ended the transaction");
        assert_eq!(second["bcc"], json!([]));
        assert_eq!(recorded[2]["data"]["bcc"], json!([long]), "a long line is read whole");
    }

    #[tokio::test]
    async fn an_email_is_kept_whole_up_to_25_mib_then_truncated_with_one_sink_fault() {
        let (_dir, recording) = recording("big-email");
        let (effects, faults) = (recording.effects.clone(), recording.faults.clone());
        let port = free_port();
        let sinks = Sinks { smtp: Some(port), http: None, responses: BTreeMap::new() };
        start(&sinks, Some(recording)).await.unwrap();
        let header = "Subject: Big\r\n\r\n";
        let line = format!("{}\r\n", "x".repeat(998));
        let whole = (LIMIT - header.len()) / line.len();
        let last = format!("{}\r\n", "y".repeat(LIMIT - header.len() - whole * line.len() - 2));
        let exact = format!("{header}{}{last}.\r\n", line.repeat(whole));
        let count = LIMIT / line.len() + 2;
        let over = format!("{header}{}.\r\n", line.repeat(count));
        let size = header.len() + line.len() * count;
        let mut client = Client::connect(port).await;
        for message in [&exact, &over] {
            client.expect("MAIL FROM:<big@x.test>\r\n", "250").await;
            client.expect("RCPT TO:<to@x.test>\r\n", "250").await;
            client.expect("DATA\r\n", "354").await;
            client.expect(message, "250").await;
        }
        client.expect("QUIT\r\n", "221").await;

        let recorded = wait_for_lines(&effects, 2).await;
        let kept = recorded[0]["data"]["text"].as_str().unwrap();
        assert_eq!(kept.len(), LIMIT - header.len(), "an email of 25 MiB is kept whole");
        let cut = recorded[1]["data"]["text"].as_str().unwrap();
        assert!(cut.len() <= LIMIT && cut.len() > LIMIT - line.len(), "{}", cut.len());
        assert!(cut.ends_with("x\r\n"), "cut at a line's end");
        assert_eq!(
            lines(&faults),
            [json!({"kind": "sink", "detail": format!(
                "an email from big@x.test of {size} bytes was recorded truncated to its first \
                 {LIMIT} bytes"
            )})]
        );
    }

    #[tokio::test]
    async fn http_requests_are_recorded_as_the_journal_states() {
        let (dir, recording) = recording("http");
        let effects = recording.effects.clone();
        let port = free_port();
        let sinks = Sinks { smtp: None, http: Some(port), responses: BTreeMap::new() };
        start(&sinks, Some(recording)).await.unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}");
        let paid = client
            .post(format!("{url}/stripe/v1/payment_intents?expand=charges&amount=125000"))
            .header("authorization", "Bearer sk_test")
            .header("x-trace", "a")
            .header("X-Trace", "b")
            .json(&json!({"amount": 125_000, "currency": "usd"}))
            .send()
            .await
            .unwrap();
        assert_eq!(paid.status(), 200);
        assert_eq!(paid.text().await.unwrap(), "", "no canned answer: 200, empty");
        client.post(format!("{url}/hooks/text")).body("amount=125000").send().await.unwrap();
        client.get(format!("{url}/empty")).send().await.unwrap();

        let recorded = wait_for_lines(&effects, 3).await;
        let data: Vec<&Value> = recorded.iter().map(|line| &line["data"]).collect();
        assert!(recorded.iter().all(|line| line["kind"] == "http"));
        assert_eq!(data[0]["method"], "POST");
        assert_eq!(data[0]["path"], "/stripe/v1/payment_intents");
        assert_eq!(data[0]["query"], json!({"expand": "charges", "amount": "125000"}));
        assert_eq!(data[0]["body"], json!({"amount": 125_000, "currency": "usd"}));
        let headers = data[0]["headers"].as_object().unwrap();
        assert_eq!(headers.get("authorization"), None, "credentials are dropped");
        assert_eq!(headers["x-trace"], "a, b");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(
            (&data[1]["path"], &data[1]["body"]),
            (&json!("/hooks/text"), &json!("amount=125000"))
        );
        assert_eq!((&data[2]["method"], &data[2]["body"]), (&json!("GET"), &Value::Null));
        assert_eq!(data[2]["query"], json!({}));

        let evidence = Evidence {
            miroir: "test".into(),
            tables: Vec::new(),
            changes: Vec::new(),
            effects: recorded,
            faults: Vec::new(),
        };
        let path = dir.join("evidence.db");
        evidence::write(&path, &evidence).await.unwrap();
    }

    #[tokio::test]
    async fn the_journals_of_both_sinks_are_what_evidence_reads() {
        let (dir, recording) = recording("contract");
        let effects = recording.effects.clone();
        let (smtp, http) = (free_port(), free_port());
        let sinks = Sinks { smtp: Some(smtp), http: Some(http), responses: BTreeMap::new() };
        start(&sinks, Some(recording)).await.unwrap();
        let mut client = Client::connect(smtp).await;
        client.expect("MAIL FROM:<crm@acme.test>\r\n", "250").await;
        client.expect("RCPT TO:<marcus@acme.test>\r\n", "250").await;
        client.expect("DATA\r\n", "354").await;
        client.expect("To: marcus@acme.test\r\nSubject: Hi\r\n\r\nHello\r\n.\r\n", "250").await;
        wait_for_lines(&effects, 1).await;
        reqwest::Client::new()
            .put(format!("http://127.0.0.1:{http}/s3/x"))
            .body("{")
            .send()
            .await
            .unwrap();
        let recorded = wait_for_lines(&effects, 2).await;
        assert_eq!(recorded[0]["kind"], "email");
        assert_eq!(recorded[1]["data"]["body"], "{", "a body that is not JSON is its text");
        let evidence = Evidence {
            miroir: "test".into(),
            tables: Vec::new(),
            changes: Vec::new(),
            effects: recorded,
            faults: Vec::new(),
        };
        evidence::write(&dir.join("evidence.db"), &evidence).await.unwrap();
    }

    #[tokio::test]
    async fn a_body_too_big_to_record_is_refused_with_one_sink_fault() {
        let (_dir, recording) = recording("big-body");
        let (effects, faults) = (recording.effects.clone(), recording.faults.clone());
        let port = free_port();
        let sinks = Sinks { smtp: None, http: Some(port), responses: BTreeMap::new() };
        start(&sinks, Some(recording)).await.unwrap();
        let (client, url) = (reqwest::Client::new(), format!("http://127.0.0.1:{port}/s3/dump"));
        assert_eq!(client.put(&url).body("small").send().await.unwrap().status(), 200);
        assert_eq!(lines(&faults), Vec::<Value>::new());
        let huge = client.put(&url).body(vec![b'x'; LIMIT + 1]).send().await;
        assert!(huge.is_err() || huge.is_ok_and(|r| r.status() == 413));
        let recorded = wait_for_lines(&faults, 1).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0]["kind"], "sink");
        let detail = recorded[0]["detail"].as_str().unwrap();
        assert!(detail.starts_with("the HTTP sink cannot read PUT /s3/dump: "), "{detail}");
        assert_eq!(lines(&effects).len(), 1, "the refused upload is no effect");
    }

    #[tokio::test]
    async fn canned_responses_are_answered() {
        let mut headers = BTreeMap::new();
        headers.insert("x-request-id".to_string(), "req_1".to_string());
        let responses = BTreeMap::from([
            (
                "POST /stripe/v1/payment_intents".to_string(),
                CannedResponse { json: Some(json!({"id": "pi_test"})), ..canned(201) },
            ),
            (
                "/hooks/*".to_string(),
                CannedResponse { body: Some("accepted".into()), headers, ..canned(202) },
            ),
            ("DELETE /gone".to_string(), canned(410)),
        ]);
        let port = free_port();
        let sinks = Sinks { smtp: None, http: Some(port), responses };
        start(&sinks, None).await.unwrap();
        let (client, url) = (reqwest::Client::new(), format!("http://127.0.0.1:{port}"));
        let paid = client.post(format!("{url}/stripe/v1/payment_intents")).send().await.unwrap();
        assert_eq!(paid.status(), 201);
        assert_eq!(paid.json::<Value>().await.unwrap(), json!({"id": "pi_test"}));
        let hook = client.post(format!("{url}/hooks/github")).send().await.unwrap();
        assert_eq!(hook.status(), 202);
        assert_eq!(hook.headers()["x-request-id"], "req_1");
        assert_eq!(hook.text().await.unwrap(), "accepted");
        let gone = client.delete(format!("{url}/gone")).send().await.unwrap();
        assert_eq!(gone.status(), 410);
        assert_eq!(gone.text().await.unwrap(), "");
        let other = client.get(format!("{url}/stripe/v1/payment_intents")).send().await.unwrap();
        assert_eq!(other.status(), 200, "another method gets the default");
    }

    #[tokio::test]
    async fn without_a_recording_the_sinks_answer_and_record_nothing() {
        let (smtp, http) = (free_port(), free_port());
        let sinks = Sinks { smtp: Some(smtp), http: Some(http), responses: BTreeMap::new() };
        start(&sinks, None).await.unwrap();
        let mut client = Client::connect(smtp).await;
        client.expect("MAIL FROM:<seed@acme.test>\r\n", "250").await;
        client.expect("RCPT TO:<dana@acme.test>\r\n", "250").await;
        client.expect("DATA\r\n", "354").await;
        client.expect("Subject: Seeded\r\n\r\nHi\r\n.\r\n", "250").await;
        let url = format!("http://127.0.0.1:{http}/hooks/seed");
        assert_eq!(reqwest::Client::new().post(url).send().await.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn a_session_that_ends_early_records_nothing() {
        let (_dir, recording) = recording("closed");
        let effects = recording.effects.clone();
        let (port, session) = one_session(recording.clone()).await;
        let mut client = Client::connect(port).await;
        client.expect("MAIL FROM:<a@x.test>\r\n", "250").await;
        client.expect("RCPT TO:<b@x.test>\r\n", "250").await;
        client.expect("DATA\r\n", "354").await;
        client.stream.write_all(b"Subject: Cut\r\n\r\nhalf a mes").await.unwrap();
        drop(client);
        let error = session.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "connection closed in DATA");
        let (port, session) = one_session(recording).await;
        let mut client = Client::connect(port).await;
        client.expect("EHLO x\r\n", "250").await;
        drop(client);
        session.await.unwrap().unwrap();
        assert_eq!(lines(&effects), Vec::<Value>::new());
    }

    #[tokio::test]
    async fn the_sinks_answer_when_their_journals_cannot_be_written() {
        let missing = PathBuf::from("/nonexistent/miroir");
        let recording = Recording {
            effects: missing.join("effects.jsonl"),
            faults: missing.join("faults.jsonl"),
        };
        let port = free_port();
        let sinks = Sinks { smtp: None, http: Some(port), responses: BTreeMap::new() };
        start(&sinks, Some(recording)).await.unwrap();
        let url = format!("http://127.0.0.1:{port}/hooks/lost");
        let response = reqwest::Client::new().post(url).body("{}").send().await.unwrap();
        assert_eq!(response.status(), 200, "a lost line is logged, never the product's error");
    }

    #[tokio::test]
    async fn a_port_in_use_fails_naming_the_sink() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let sinks = Sinks { smtp: Some(port), http: None, responses: BTreeMap::new() };
        let error = start(&sinks, None).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        let message = error.to_string();
        assert!(message.starts_with(&format!("the SMTP sink cannot listen on 127.0.0.1:{port}: ")));
        assert!(
            message
                .ends_with("free the port, or give the sink another one in the manifest's sinks")
        );
        let sinks = Sinks { smtp: None, http: Some(port), responses: BTreeMap::new() };
        let error = start(&sinks, None).await.unwrap_err();
        assert!(error.to_string().starts_with("the HTTP sink cannot listen"), "{error}");
    }

    #[test]
    fn smtp_addresses() {
        assert_eq!(address("FROM:<a@b.test> SIZE=10"), Some("a@b.test".into()));
        assert_eq!(address("FROM:<>"), None);
        assert_eq!(address("TO:nobody"), None);
        assert_eq!(address("TO:<unclosed"), None);
    }
}
