//! Client of an app's MCP server, whose tools the agent calls on the `mcp` surface.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Response, Url};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, OnceCell};

use crate::env::Env;
use crate::manifest::Persona;
use crate::task::{TOOLS, is_tool_name};
use crate::tools::{BODY_LIMIT, Failure, ToolError, cut, http_client};

/// The MCP version miroir asks for.
pub const MCP_VERSION: &str = "2025-06-18";
/// The MCP versions miroir speaks.
pub const MCP_VERSIONS: [&str; 2] = [MCP_VERSION, "2025-03-26"];
const DEADLINE: Duration = Duration::from_secs(30);
const READ_LIMIT: usize = 8 * 1024 * 1024;
const PAGE_LIMIT: usize = 100;
const SESSION: &str = "mcp-session-id";
const PROTOCOL: &str = "mcp-protocol-version";

/// An app's MCP server: tools listed once, one session per restore.
pub struct Server {
    pub app: String,
    url: Url,
    client: reqwest::Client,
    deadline: Duration,
    session: Mutex<Option<Session>>,
    tools: OnceCell<Vec<Value>>,
    ids: AtomicU64,
}

#[derive(Clone)]
struct Session {
    id: Option<String>,
    version: String,
}

enum Reply {
    Result(Value),
    Error { code: i64, message: String },
}

impl Server {
    pub fn new(app: &str, url: &str) -> Result<Server, String> {
        let url = Url::parse(url).map_err(|e| format!("app '{app}': mcp.url: {e}"))?;
        Ok(Server {
            app: app.to_string(),
            url,
            client: http_client()?,
            deadline: DEADLINE,
            session: Mutex::new(None),
            tools: OnceCell::new(),
            ids: AtomicU64::new(1),
        })
    }

    /// The server's tools, listed once; a failed listing voids the grade.
    pub async fn tools(
        &self,
        headers: Option<&BTreeMap<String, String>>,
    ) -> Result<&[Value], Failure> {
        let listed = self
            .tools
            .get_or_try_init(|| async {
                let listing = tokio::time::timeout(self.deadline, self.list(headers)).await;
                let listed = listing.unwrap_or_else(|_| Err(self.late("tools/list")));
                listed.map_err(|failure| Failure { void: true, ..failure })
            })
            .await?;
        Ok(listed)
    }

    /// Whether the listed tools include `name`.
    pub fn lists(&self, name: &str) -> bool {
        self.tools.get().is_some_and(|tools| tools.iter().any(|tool| tool["name"] == name))
    }

    /// Call a tool with the persona's headers.
    pub async fn call(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
        headers: Option<&BTreeMap<String, String>>,
    ) -> Result<Value, Failure> {
        let session = self.session(headers).await?;
        let body = self.request("tools/call", &json!({"name": name, "arguments": arguments}));
        let exchange = self.exchange(&body, headers, Some(&session));
        let seconds = self.deadline.as_secs_f64();
        let late = |_| Failure::agent("timeout", format!("the call took longer than {seconds} s"));
        let (reply, _) = tokio::time::timeout(self.deadline, exchange).await.map_err(late)??;
        match reply {
            Reply::Error { code: -32602, message } => Err(Failure::agent("invalid_args", message)),
            Reply::Error { message, .. } => Err(Failure::agent("execution_error", message)),
            Reply::Result(result) => self.content(&result),
        }
    }

    /// Drop the session, so the next call opens one on the restored app.
    pub async fn forget(&self) {
        *self.session.lock().await = None;
    }

    async fn list(
        &self,
        headers: Option<&BTreeMap<String, String>>,
    ) -> Result<Vec<Value>, Failure> {
        let session = self.session(headers).await?;
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        for _ in 0..PAGE_LIMIT {
            let params = cursor.take().map_or_else(|| json!({}), |next| json!({"cursor": next}));
            let body = self.request("tools/list", &params);
            let page = match self.exchange(&body, headers, Some(&session)).await?.0 {
                Reply::Result(page) => page,
                Reply::Error { message, .. } => {
                    return Err(self.fault(&format!("refused tools/list: {message}")));
                }
            };
            for tool in page["tools"].as_array().into_iter().flatten() {
                tools.push(self.shown(tool)?);
            }
            match page.get("nextCursor") {
                Some(next) if !next.is_null() => cursor = Some(next.clone()),
                _ => return Ok(tools),
            }
        }
        Err(self.fault(&format!("lists more than {PAGE_LIMIT} pages of tools")))
    }

    fn shown(&self, tool: &Value) -> Result<Value, Failure> {
        let name = tool["name"].as_str().unwrap_or_default();
        if !is_tool_name(name) {
            return Err(self.fault(&format!(
                "lists a tool named '{name}', which model APIs refuse: use 1 to 64 letters, \
                 digits, '_' or '-'"
            )));
        }
        if TOOLS.contains(&name) {
            return Err(self.fault(&format!("lists a tool named '{name}', one of Miroir's own")));
        }
        let mut shown = Map::new();
        for key in ["name", "title", "description", "inputSchema", "annotations"] {
            if let Some(value) = tool.get(key) {
                shown.insert(key.to_string(), value.clone());
            }
        }
        Ok(Value::Object(shown))
    }

    async fn session(
        &self,
        headers: Option<&BTreeMap<String, String>>,
    ) -> Result<Session, Failure> {
        let mut held = self.session.lock().await;
        if let Some(session) = held.as_ref() {
            return Ok(session.clone());
        }
        let opening = tokio::time::timeout(self.deadline, self.initialize(headers)).await;
        let session = opening.unwrap_or_else(|_| Err(self.late("initialize")))?;
        *held = Some(session.clone());
        Ok(session)
    }

    async fn initialize(
        &self,
        headers: Option<&BTreeMap<String, String>>,
    ) -> Result<Session, Failure> {
        let client = json!({"name": "miroir", "version": env!("CARGO_PKG_VERSION")});
        let params =
            json!({"protocolVersion": MCP_VERSION, "capabilities": {}, "clientInfo": client});
        let (reply, id) =
            self.exchange(&self.request("initialize", &params), headers, None).await?;
        let result = match reply {
            Reply::Result(result) => result,
            Reply::Error { message, .. } => {
                return Err(self.fault(&format!("refused initialize: {message}")));
            }
        };
        let version = result["protocolVersion"].as_str().unwrap_or_default();
        if !MCP_VERSIONS.contains(&version) {
            let spoken = MCP_VERSIONS.join(" and ");
            return Err(self.fault(&format!("speaks MCP '{version}', and miroir speaks {spoken}")));
        }
        let session = Session { id, version: version.to_string() };
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        self.post(&note, headers, Some(&session)).await?;
        Ok(session)
    }

    fn request(&self, method: &str, params: &Value) -> Value {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    async fn exchange(
        &self,
        body: &Value,
        headers: Option<&BTreeMap<String, String>>,
        session: Option<&Session>,
    ) -> Result<(Reply, Option<String>), Failure> {
        let response = self.post(body, headers, session).await?;
        let header = |name| response.headers().get(name).and_then(|v| v.to_str().ok());
        let id = header(SESSION).map(str::to_string);
        let kind = header(CONTENT_TYPE.as_str()).unwrap_or_default().to_ascii_lowercase();
        let wanted = body["id"].as_u64();
        let message = if kind.starts_with("text/event-stream") {
            self.answer_in_events(response, wanted).await?
        } else if kind.contains("json") {
            let text = self.read(response).await?;
            serde_json::from_slice(&text).map_err(|e| self.fault(&format!("sent bad JSON: {e}")))?
        } else {
            return Err(self.fault(&format!("answered with content type '{kind}'")));
        };
        Ok((self.reply(&message, wanted)?, id))
    }

    async fn post(
        &self,
        body: &Value,
        headers: Option<&BTreeMap<String, String>>,
        session: Option<&Session>,
    ) -> Result<Response, Failure> {
        let mut sent = HeaderMap::new();
        for (name, value) in headers.into_iter().flatten() {
            let parsed = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value));
            let (Ok(name), Ok(value)) = parsed else {
                return Err(self.fault(&format!("can't be sent the persona's header '{name}'")));
            };
            sent.insert(name, value);
        }
        sent.insert(ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));
        sent.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(session) = session {
            let values = (Some(session.version.as_str()), session.id.as_deref());
            for (name, value) in [PROTOCOL, SESSION].into_iter().zip([values.0, values.1]) {
                if let Some(value) = value.and_then(|v| HeaderValue::from_str(v).ok()) {
                    sent.insert(HeaderName::from_static(name), value);
                }
            }
        }
        let request = self.client.post(self.url.clone()).headers(sent).body(body.to_string());
        let unreachable = |e| self.cut_off(&format!("can't be reached: {e}"));
        let response = request.send().await.map_err(unreachable)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let shown = cut(&text, 500);
            return Err(self.cut_off(&format!("answered HTTP {}: {shown}", status.as_u16())));
        }
        Ok(response)
    }

    async fn read(&self, mut response: Response) -> Result<Vec<u8>, Failure> {
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| self.cut_off(&e.to_string()))? {
            body.extend_from_slice(&chunk);
            if body.len() > READ_LIMIT {
                return Err(too_big());
            }
        }
        Ok(body)
    }

    async fn answer_in_events(
        &self,
        mut response: Response,
        wanted: Option<u64>,
    ) -> Result<Value, Failure> {
        let (mut buffer, mut read) = (Vec::new(), 0);
        loop {
            while let Some((length, blank)) = event_end(&buffer) {
                let event: Vec<u8> = buffer.drain(..length + blank).collect();
                if let Some(message) = event_data(&event[..length])
                    && message.get("id").and_then(Value::as_u64) == wanted
                    && (message.get("result").is_some() || message.get("error").is_some())
                {
                    return Ok(message);
                }
            }
            let next = response.chunk().await.map_err(|e| self.cut_off(&e.to_string()))?;
            let Some(chunk) = next else {
                return Err(self.fault("closed its event stream without answering"));
            };
            read += chunk.len();
            if read > READ_LIMIT {
                return Err(too_big());
            }
            buffer.extend_from_slice(&chunk);
        }
    }

    fn reply(&self, message: &Value, wanted: Option<u64>) -> Result<Reply, Failure> {
        if message.get("id").and_then(Value::as_u64) != wanted {
            return Err(self.fault("answered another request"));
        }
        if let Some(error) = message.get("error") {
            let code = error["code"].as_i64().unwrap_or_default();
            let text = error["message"].as_str().unwrap_or("no message given");
            return Ok(Reply::Error { code, message: text.to_string() });
        }
        let result = message.get("result").cloned();
        result.map(Reply::Result).ok_or_else(|| self.fault("answered with no result"))
    }

    fn content(&self, result: &Value) -> Result<Value, Failure> {
        let Some(content) = result.get("content").and_then(Value::as_array) else {
            return Err(self.fault("returned a tool result with no content"));
        };
        if result.get("isError") == Some(&Value::Bool(true)) {
            let text = joined(content);
            return Err(Failure::agent("execution_error", cut(&text, BODY_LIMIT).to_string()));
        }
        Ok(capped(content))
    }

    fn fault(&self, what: &str) -> Failure {
        Failure::env("execution_error", format!("app '{}' MCP server {what}", self.app))
    }

    fn cut_off(&self, what: &str) -> Failure {
        Failure::env("transport_error", format!("app '{}' MCP server {what}", self.app))
    }

    fn late(&self, what: &str) -> Failure {
        let seconds = self.deadline.as_secs_f64();
        let message = format!("app '{}' MCP server did not answer {what} in {seconds} s", self.app);
        Failure::env("timeout", message)
    }
}

fn too_big() -> Failure {
    let mib = READ_LIMIT / (1024 * 1024);
    Failure::agent("execution_error", format!("the reply passed {mib} MiB; ask for less"))
}

fn event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let find = |blank: &[u8]| {
        let at = buffer.windows(blank.len()).position(|window| window == blank);
        at.map(|at| (at, blank.len()))
    };
    match (find(b"\n\n"), find(b"\r\n\r\n")) {
        (Some(lf), Some(crlf)) => Some(if lf.0 <= crlf.0 { lf } else { crlf }),
        (lf, crlf) => lf.or(crlf),
    }
}

fn event_data(event: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(event).ok()?;
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect();
    if lines.is_empty() {
        return None;
    }
    serde_json::from_str(&lines.join("\n")).ok()
}

fn capped(content: &[Value]) -> Value {
    let whole = Value::Array(content.to_vec());
    if whole.to_string().len() <= BODY_LIMIT {
        return json!({"content": whole});
    }
    let text = joined(content);
    json!({"content": [{"type": "text", "text": cut(&text, BODY_LIMIT)}], "truncated": true})
}

fn joined(content: &[Value]) -> String {
    let texts: Vec<&str> =
        content.iter().filter_map(|block| block.get("text").and_then(Value::as_str)).collect();
    texts.join("\n")
}

impl Env {
    /// Call a tool of the app's MCP server, or say there is no such tool.
    pub(crate) async fn call_mcp(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
        persona: Option<&Persona>,
    ) -> Result<Value, ToolError> {
        let (Some(server), Some(listing)) = (&self.mcp, self.mcp_tools().await) else {
            return Err(ToolError::unknown(name));
        };
        if let Err(failure) = listing {
            return Err(self.fail(failure));
        }
        if !server.lists(name) {
            return Err(ToolError::unknown(name));
        }
        let headers = persona.and_then(|persona| persona.headers.get(&server.app));
        server.call(name, arguments, headers).await.map_err(|failure| self.fail(failure))
    }

    /// The MCP server's tools if the env has one, listed with a persona's headers.
    pub async fn mcp_tools(&self) -> Option<Result<&[Value], Failure>> {
        let server = self.mcp.as_ref()?;
        let personas = self.manifest.personas.values();
        let headers = personas.into_iter().find_map(|persona| persona.headers.get(&server.app));
        Some(server.tools(headers).await)
    }

    /// The app to record an MCP call under, when several apps can be called.
    pub(crate) fn mcp_app(&self, name: &str) -> Option<&str> {
        let server = self.mcp.as_ref().filter(|server| server.lists(name))?;
        let callable = self.manifest.apps.values().filter(|a| a.api.is_some() || a.mcp.is_some());
        (callable.count() > 1).then_some(server.app.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response as AxumResponse};
    use axum::routing::post;

    use super::*;

    #[derive(Default)]
    struct Fake {
        sse: bool,
        session: Option<&'static str>,
        version: Option<&'static str>,
        broken: bool,
        tools: Vec<&'static str>,
        seen: StdMutex<Vec<(String, axum::http::HeaderMap)>>,
    }

    impl Fake {
        fn seen(&self, method: &str) -> Vec<axum::http::HeaderMap> {
            let seen = self.seen.lock().unwrap();
            seen.iter().filter(|(m, _)| m == method).map(|(_, h)| h.clone()).collect()
        }
    }

    async fn answer(
        State(fake): State<Arc<Fake>>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> AxumResponse {
        let request: Value = serde_json::from_str(&body).unwrap();
        let method = request["method"].as_str().unwrap().to_string();
        fake.seen.lock().unwrap().push((method.clone(), headers.clone()));
        if fake.broken {
            return (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response();
        }
        if method.starts_with("notifications/") {
            return StatusCode::ACCEPTED.into_response();
        }
        let id = request["id"].clone();
        let outcome = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": fake.version.unwrap_or(MCP_VERSION),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "1"},
            })),
            "tools/list" => Ok(page(&fake.tools, &request["params"])),
            _ => call(&request["params"], &headers).await,
        };
        let message = match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, text)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}})
            }
        };
        let mut response = if fake.sse {
            let note = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
            let text = format!("event: message\r\ndata: {note}\r\n\r\ndata: {message}\n\n");
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], text).into_response()
        } else {
            axum::Json(message).into_response()
        };
        if let (Some(session), "initialize") = (fake.session, method.as_str()) {
            response.headers_mut().insert(SESSION, session.parse().unwrap());
        }
        response
    }

    fn page(tools: &[&str], params: &Value) -> Value {
        let at: usize = params["cursor"].as_str().map_or(0, |c| c.parse().unwrap());
        let tool =
            json!({"name": tools[at], "inputSchema": {"type": "object"}, "outputSchema": {}});
        let next = (at + 1 < tools.len()).then(|| (at + 1).to_string());
        json!({"tools": [tool], "nextCursor": next})
    }

    async fn call(params: &Value, headers: &axum::http::HeaderMap) -> Result<Value, (i64, String)> {
        let text = |text: String| json!({"content": [{"type": "text", "text": text}]});
        match params["name"].as_str().unwrap() {
            "whoami" => Ok(text(headers["authorization"].to_str().unwrap().to_string())),
            "fail" => {
                Ok(json!({"content": [{"type": "text", "text": "no such deal"}], "isError": true}))
            }
            "bad" => Err((-32602, "deal_id is required".into())),
            "boom" => Err((-32603, "internal error".into())),
            "big" => Ok(text("é".repeat(BODY_LIMIT))),
            "bare" => Ok(json!({"structuredContent": {}})),
            _ => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(text(String::new()))
            }
        }
    }

    async fn serve(fake: Fake) -> (Server, Arc<Fake>) {
        let fake = Arc::new(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let router = axum::Router::new().route("/mcp", post(answer)).with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let server = Server::new("crm", &url).unwrap();
        (Server { deadline: Duration::from_millis(500), ..server }, fake)
    }

    fn persona() -> BTreeMap<String, String> {
        [("Authorization".to_string(), "Bearer dana".to_string())].into()
    }

    #[tokio::test]
    async fn it_lists_every_page_and_calls_as_the_persona_in_one_session() {
        let fake =
            Fake { session: Some("s1"), tools: vec!["list_deals", "whoami"], ..Fake::default() };
        let (server, fake) = serve(fake).await;
        let tools = server.tools(Some(&persona())).await.unwrap();
        assert_eq!(
            tools,
            [
                json!({"name": "list_deals", "inputSchema": {"type": "object"}}),
                json!({"name": "whoami", "inputSchema": {"type": "object"}}),
            ]
        );
        assert!(server.lists("whoami") && !server.lists("submit"));
        let shown = server.call("whoami", &Map::new(), Some(&persona())).await.unwrap();
        assert_eq!(shown, json!({"content": [{"type": "text", "text": "Bearer dana"}]}));

        assert_eq!(fake.seen("initialize").len(), 1, "one session for the listing and the call");
        assert_eq!(fake.seen("notifications/initialized").len(), 1);
        let call = &fake.seen("tools/call")[0];
        assert_eq!(call["mcp-session-id"], "s1");
        assert_eq!(call[PROTOCOL], MCP_VERSION);
        assert_eq!(call["accept"], "application/json, text/event-stream");
        assert!(fake.seen("initialize")[0].get(SESSION).is_none());

        server.forget().await;
        server.call("whoami", &Map::new(), Some(&persona())).await.unwrap();
        assert_eq!(fake.seen("initialize").len(), 2, "a restore opens a new session");
        assert_eq!(fake.seen("tools/list").len(), 2, "but the tools stay listed");
    }

    #[tokio::test]
    async fn it_reads_the_answer_from_an_event_stream() {
        let (server, _) = serve(Fake { sse: true, tools: vec!["whoami"], ..Fake::default() }).await;
        let shown = server.call("whoami", &Map::new(), Some(&persona())).await.unwrap();
        assert_eq!(shown["content"][0]["text"], "Bearer dana");
        assert_eq!(server.tools(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_servers_own_errors_are_the_agents() {
        let (server, _) = serve(Fake { sse: true, ..Fake::default() }).await;
        let failed = |name: &'static str| {
            let server = &server;
            async move { server.call(name, &Map::new(), None).await.unwrap_err() }
        };
        for (name, error_type, message) in [
            ("fail", "execution_error", "no such deal"),
            ("bad", "invalid_args", "deal_id is required"),
            ("boom", "execution_error", "internal error"),
            ("slow", "timeout", "the call took longer than 0.5 s"),
        ] {
            let failure = failed(name).await;
            assert_eq!((failure.error.error_type, failure.void), (error_type, false), "{name}");
            assert_eq!(failure.error.message, message, "{name}");
        }
        let big = server.call("big", &Map::new(), None).await.unwrap();
        assert_eq!(big["truncated"], true);
        let text = big["content"][0]["text"].as_str().unwrap();
        assert!(text.len() <= BODY_LIMIT && text.len() > BODY_LIMIT - 2, "cut on a char boundary");
        assert!(failed("bare").await.void, "a result with no content breaks the protocol");
    }

    #[tokio::test]
    async fn a_server_that_cant_be_spoken_to_voids_the_grade() {
        let (old, _) = serve(Fake { version: Some("2024-11-05"), ..Fake::default() }).await;
        let failure = old.call("whoami", &Map::new(), None).await.unwrap_err();
        assert!(
            failure.void && failure.error.message.contains("speaks MCP '2024-11-05'"),
            "{failure:?}"
        );

        let (down, _) = serve(Fake { broken: true, ..Fake::default() }).await;
        let failure = down.call("whoami", &Map::new(), None).await.unwrap_err();
        assert_eq!((failure.error.error_type, failure.void), ("transport_error", true));
        assert!(failure.error.message.contains("answered HTTP 500: down"), "{failure:?}");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        drop(listener);
        let gone = Server::new("crm", &url).unwrap();
        let failure = gone.call("whoami", &Map::new(), None).await.unwrap_err();
        assert_eq!((failure.error.error_type, failure.void), ("transport_error", true));
        assert!(gone.tools(None).await.unwrap_err().void, "and so does a failed listing");
    }

    #[tokio::test]
    async fn a_listing_refuses_names_models_cant_call_or_miroir_keeps() {
        for (name, expected) in [
            ("submit", "one of Miroir's own"),
            ("coworker_reply", "one of Miroir's own"),
            ("crm.list", "model APIs refuse"),
        ] {
            let (server, _) = serve(Fake { tools: vec!["ok", name], ..Fake::default() }).await;
            let failure = server.tools(None).await.unwrap_err();
            assert!(failure.void && failure.error.message.contains(expected), "{failure:?}");
        }
    }

    #[test]
    fn events_end_at_the_first_blank_line_of_either_kind() {
        assert_eq!(event_end(b"data: 1\n\ndata: 2\r\n\r\n"), Some((7, 2)));
        assert_eq!(event_end(b"data: 1\r\n\r\ndata: 2\n\n"), Some((7, 4)));
        assert_eq!(event_end(b"data: 1\n"), None);
        assert_eq!(event_data(b"event: message\ndata: {\"a\":\ndata: 1}"), Some(json!({"a": 1})));
        assert_eq!(event_data(b": a comment"), None);
    }
}
