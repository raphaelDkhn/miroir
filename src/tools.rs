//! The agent's API tools, `api_request` and `api_docs`, the tool list, and the forwarding
//! behind the code proxy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Bytes;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use reqwest::header::{
    CONNECTION, CONTENT_LENGTH, COOKIE, HOST, HeaderMap, HeaderName, HeaderValue,
};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Map, Value, json};

use crate::manifest::{Manifest, Persona};

/// Most bytes of a body or output a tool returns.
pub const BODY_LIMIT: usize = 64 * 1024;
const HEADER_LIMIT: usize = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REF_DEPTH: usize = 5;
const REF_BUDGET: usize = 64 * 1024;
/// Most bytes of a body the code proxy forwards.
pub const FORWARD_LIMIT: usize = 8 * 1024 * 1024;
const HOP_BY_HOP: [&str; 7] =
    ["connection", "keep-alive", "te", "trailer", "transfer-encoding", "upgrade", "expect"];

const TOOLS: &str = include_str!("tools.json");

/// A tool call's error, as its observation carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub error_type: &'static str,
    pub message: String,
}

impl ToolError {
    pub fn invalid(message: impl Into<String>) -> ToolError {
        ToolError { error_type: "invalid_args", message: message.into() }
    }

    pub fn unknown(name: &str) -> ToolError {
        let message = format!("unknown tool '{name}'; call list_tools to see the tools");
        ToolError { error_type: "tool_not_found", message }
    }

    pub fn to_json(&self) -> Value {
        json!({"error_type": self.error_type, "message": self.message})
    }
}

/// A tool error, and whether it voids the grade: the env's fault, not the agent's.
#[derive(Debug)]
pub struct Failure {
    pub error: ToolError,
    pub void: bool,
}

impl Failure {
    pub fn agent(error_type: &'static str, message: String) -> Failure {
        Failure { error: ToolError { error_type, message }, void: false }
    }

    pub fn env(error_type: &'static str, message: String) -> Failure {
        Failure { error: ToolError { error_type, message }, void: true }
    }
}

/// An HTTP client that ignores proxies and follows no redirects.
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("cannot build the HTTP client: {e}"))
}

/// Percent-encode all but unreserved characters.
pub fn percent_encode(text: &str) -> String {
    const KEPT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
    utf8_percent_encode(text, KEPT).to_string()
}

/// Compare byte strings in time that doesn't depend on where they differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The longest prefix of at most `limit` bytes that ends on a char boundary.
pub fn cut(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Each app's API and docs.
pub struct Tools {
    client: reqwest::Client,
    apps: BTreeMap<String, Endpoint>,
}

struct Endpoint {
    base: Url,
    description: Option<String>,
    docs: Option<Vec<Operation>>,
}

struct Operation {
    id: Option<String>,
    method: String,
    path: String,
    summary: Option<String>,
    detail: Value,
}

impl Tools {
    /// Load each app's API, reading its spec where `spec` maps the path.
    pub fn new(manifest: &Manifest, spec: impl Fn(&Path) -> PathBuf) -> Result<Tools, String> {
        let mut apps = BTreeMap::new();
        for (name, api) in manifest.apis() {
            let at = |e: String| format!("app '{name}': {e}");
            let base =
                Url::parse(api.base_url.trim_end_matches('/')).map_err(|e| at(e.to_string()))?;
            let docs = match &api.openapi {
                Some(path) => Some(load_docs(&spec(path)).map_err(at)?),
                None => None,
            };
            let description = manifest.apps.get(name).and_then(|app| app.description.clone());
            apps.insert(name.to_string(), Endpoint { base, description, docs });
        }
        Ok(Tools { client: http_client()?, apps })
    }

    pub fn apis(&self) -> impl Iterator<Item = &str> {
        self.apps.keys().map(String::as_str)
    }

    /// The tools to list: with `code`, `run_code` replaces `api_request`; `wait` needs
    /// `clock`.
    pub fn list(&self, code: bool, clock: bool) -> Vec<Value> {
        let tools: Vec<Value> = serde_json::from_str(TOOLS).unwrap_or_default();
        let left_out = if code { "api_request" } else { "run_code" };
        tools
            .into_iter()
            .filter(|t| self.has_docs() || t["name"] != "api_docs")
            .filter(|t| clock || t["name"] != "wait")
            .filter(|t| t["name"] != left_out)
            .map(|t| self.with_app(t))
            .collect()
    }

    fn has_docs(&self) -> bool {
        self.apps.values().any(|endpoint| endpoint.docs.is_some())
    }

    fn with_app(&self, mut tool: Value) -> Value {
        let docs = match tool["name"].as_str() {
            _ if self.apps.len() < 2 => return tool,
            Some("api_request") => false,
            Some("api_docs") => true,
            _ => return tool,
        };
        let apps: Vec<(&String, &Endpoint)> =
            self.apps.iter().filter(|(_, endpoint)| !docs || endpoint.docs.is_some()).collect();
        let named: Vec<String> = apps
            .iter()
            .map(|(name, endpoint)| match &endpoint.description {
                Some(description) => format!("{name}, {description}"),
                None => (*name).clone(),
            })
            .collect();
        let schema = &mut tool["input_schema"];
        schema["properties"]["app"] = json!({
            "type": "string",
            "enum": apps.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "description": format!("The app to call: {}", named.join("; ")),
        });
        match schema["required"].as_array_mut() {
            Some(required) => required.insert(0, json!("app")),
            None => schema["required"] = json!(["app"]),
        }
        tool
    }

    fn endpoint(&self, args: &Map<String, Value>) -> Result<(&str, &Endpoint), ToolError> {
        let names = || self.apps.keys().map(String::as_str).collect::<Vec<_>>().join(", ");
        let several = self.apps.len() > 1;
        let name = match (args.get("app"), several) {
            (None, false) => self.apps.keys().next().map_or("", String::as_str),
            (Some(_), false) => return Err(ToolError::invalid("unexpected argument 'app'")),
            (None, true) => {
                return Err(ToolError::invalid(format!("app is required: one of {}", names())));
            }
            (Some(Value::String(name)), true) => name.as_str(),
            (Some(_), true) => return Err(ToolError::invalid("app must be a string")),
        };
        match self.apps.get_key_value(name) {
            Some((name, endpoint)) => Ok((name.as_str(), endpoint)),
            None => Err(ToolError::invalid(format!("unknown app '{name}': one of {}", names()))),
        }
    }
}

impl Endpoint {
    /// The URL for a path, refusing any that would leave the API.
    pub fn url_for(&self, path: &str) -> Result<Url, ToolError> {
        if !path.starts_with('/') {
            return Err(ToolError::invalid("path must start with '/'"));
        }
        if path.chars().any(|c| c.is_ascii_control()) {
            return Err(ToolError::invalid("path must not contain control characters"));
        }
        let raw = path.split(['?', '#']).next().unwrap_or_default();
        let decoded: Vec<u8> = percent_decode_str(raw).collect();
        if has_overlong_utf8(&decoded) {
            return Err(ToolError::invalid("path must not hold overlong UTF-8"));
        }
        if decoded.split(|b| *b == b'/' || *b == b'\\').any(|segment| segment == b"..") {
            return Err(ToolError::invalid("path must not contain '..' segments"));
        }
        let joined = format!("{}{path}", self.base.as_str().trim_end_matches('/'));
        let url = Url::parse(&joined).map_err(|e| ToolError::invalid(format!("bad path: {e}")))?;
        let same = url.scheme() == self.base.scheme()
            && url.host_str() == self.base.host_str()
            && url.port_or_known_default() == self.base.port_or_known_default()
            && url.path().starts_with(self.base.path().trim_end_matches('/'));
        if !same {
            return Err(ToolError::invalid("path must stay on the product's API"));
        }
        Ok(url)
    }
}

impl Tools {
    /// Send a request to an app as the persona, whose pinned headers win.
    pub async fn api_request(
        &self,
        args: &Map<String, Value>,
        persona: &Persona,
    ) -> Result<Value, ToolError> {
        allow_keys(args, &["app", "method", "path", "body", "headers"])?;
        let (app, endpoint) = self.endpoint(args)?;
        let method = match args.get("method").and_then(Value::as_str) {
            Some(m @ ("GET" | "POST" | "PUT" | "PATCH" | "DELETE")) => {
                Method::from_bytes(m.as_bytes())
            }
            _ => return Err(ToolError::invalid("method must be GET, POST, PUT, PATCH or DELETE")),
        }
        .map_err(|e| ToolError::invalid(e.to_string()))?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::invalid("path is required"))?;
        let url = endpoint.url_for(path)?;
        let body = args.get("body");
        let pinned = persona.headers.get(app);
        let headers = request_headers(args.get("headers"), pinned, body.is_some())?;
        let encoded = body.map(|body| encode_body(body, &headers)).transpose()?;
        let mut request = self.client.request(method, url).headers(headers);
        if let Some(bytes) = encoded {
            request = request.body(bytes);
        }
        let (status, answered, body) = exchange(request, BODY_LIMIT).await?;
        let mut headers = Map::new();
        for name in ["content-type", "location", "link"] {
            if let Some(value) = answered.get(name).and_then(|v| v.to_str().ok()) {
                headers.insert(name.into(), value.into());
            }
        }
        let is_json =
            headers.get("content-type").and_then(Value::as_str).is_some_and(|t| t.contains("json"));
        let mut result = json!({"status": status.as_u16(), "headers": headers});
        if body.len() > BODY_LIMIT {
            result["body"] =
                Value::String(String::from_utf8_lossy(&body[..BODY_LIMIT]).into_owned());
            result["truncated"] = Value::Bool(true);
        } else if body.is_empty() {
            result["body"] = Value::Null;
        } else {
            let parsed = if is_json { serde_json::from_slice(&body).ok() } else { None };
            result["body"] = parsed
                .unwrap_or_else(|| Value::String(String::from_utf8_lossy(&body).into_owned()));
        }
        Ok(result)
    }

    /// List an app's operations, or show one by id or `METHOD /path`.
    pub fn api_docs(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        if !self.has_docs() {
            return Err(ToolError::unknown("api_docs"));
        }
        allow_keys(args, &["app", "operation"])?;
        let (app, endpoint) = self.endpoint(args)?;
        let docs = endpoint.docs.as_ref().ok_or_else(|| ToolError {
            error_type: "tool_not_found",
            message: format!("api_docs is not available for app '{app}': it sets no api.openapi"),
        })?;
        let result = match args.get("operation") {
            None => {
                let list: Vec<Value> = docs
                    .iter()
                    .map(|op| {
                        json!({
                            "operation": op.id,
                            "method": op.method,
                            "path": op.path,
                            "summary": op.summary,
                        })
                    })
                    .collect();
                json!({"operations": list})
            }
            Some(Value::String(wanted)) => {
                let by_route = wanted.split_once(' ').and_then(|(method, path)| {
                    docs.iter()
                        .filter(|op| method.eq_ignore_ascii_case(&op.method))
                        .filter_map(|op| fits(&op.path, path.trim()).map(|literal| (literal, op)))
                        .max_by_key(|(literal, _)| *literal)
                        .map(|(_, op)| op)
                });
                let found =
                    docs.iter().find(|op| op.id.as_deref() == Some(wanted.as_str())).or(by_route);
                let op = found.ok_or_else(|| {
                    ToolError::invalid(format!(
                        "unknown operation '{wanted}'; call api_docs with no arguments to list them"
                    ))
                })?;
                op.detail.clone()
            }
            Some(_) => return Err(ToolError::invalid("operation must be a string")),
        };
        Ok(cap(result))
    }

    /// Forward a request the sandbox's code made, as the persona.
    pub async fn forward(&self, request: Forward<'_>) -> Result<Forwarded, ToolError> {
        let endpoint = self.apps.get(request.app).ok_or_else(|| ToolError {
            error_type: "tool_not_found",
            message: format!("no app named '{}' has an API", request.app),
        })?;
        let url = endpoint.url_for(request.path)?;
        let pinned = request.persona.and_then(|persona| persona.headers.get(request.app));
        let headers = forwarded_headers(request.headers, pinned)?;
        let mut sent = self.client.request(request.method, url).headers(headers);
        if !request.body.is_empty() {
            sent = sent.body(request.body);
        }
        let (status, headers, body) = exchange(sent, FORWARD_LIMIT).await?;
        if body.len() > FORWARD_LIMIT {
            let message = "the app's answer passed 8 MiB".to_string();
            return Err(ToolError { error_type: "execution_error", message });
        }
        let base = endpoint.base.as_str().trim_end_matches('/');
        let headers = answered_headers(&headers, base, request.base);
        Ok(Forwarded { status, headers, body: Bytes::from(body) })
    }
}

async fn exchange(
    request: reqwest::RequestBuilder,
    limit: usize,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), ToolError> {
    let answer = async {
        let mut response = request.send().await?;
        let (status, headers) = (response.status(), response.headers().clone());
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            body.extend_from_slice(&chunk);
            if body.len() > limit {
                break;
            }
        }
        Ok::<_, reqwest::Error>((status, headers, body))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, answer)
        .await
        .map_err(|_| ToolError {
            error_type: "timeout",
            message: "the request took longer than 30 s".into(),
        })?
        .map_err(|e| ToolError {
            error_type: "execution_error",
            message: format!("request failed: {e}"),
        })
}

/// A request the code proxy forwards to an app.
pub struct Forward<'a> {
    pub app: &'a str,
    pub method: Method,
    pub path: &'a str,
    pub headers: &'a HeaderMap,
    pub body: Bytes,
    pub persona: Option<&'a Persona>,
    pub base: &'a str,
}

/// The app's answer, with links and redirects pointing back at the proxy.
pub struct Forwarded {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

fn forwarded_headers(
    sent: &HeaderMap,
    pinned: Option<&BTreeMap<String, String>>,
) -> Result<HeaderMap, ToolError> {
    let mut headers = without_hop_by_hop(sent);
    headers.remove(HOST);
    headers.remove(CONTENT_LENGTH);
    if pins(pinned) {
        headers.remove(COOKIE);
    }
    if headers.len() > HEADER_LIMIT {
        return Err(ToolError::invalid(format!("at most {HEADER_LIMIT} headers are allowed")));
    }
    for (name, value) in pinned.into_iter().flatten() {
        insert_header(&mut headers, name, value)?;
    }
    Ok(headers)
}

fn answered_headers(sent: &HeaderMap, base: &str, proxied: &str) -> HeaderMap {
    let mut headers = without_hop_by_hop(sent);
    headers.remove(CONTENT_LENGTH);
    for name in [reqwest::header::LINK, reqwest::header::LOCATION] {
        let values: Vec<HeaderValue> = headers
            .get_all(&name)
            .iter()
            .map(|value| match value.to_str() {
                Ok(text) if text.contains(base) => {
                    HeaderValue::from_str(&text.replace(base, proxied)).unwrap_or(value.clone())
                }
                _ => value.clone(),
            })
            .collect();
        headers.remove(&name);
        for value in values {
            headers.append(&name, value);
        }
    }
    headers
}

fn without_hop_by_hop(headers: &HeaderMap) -> HeaderMap {
    let named: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    let mut kept = HeaderMap::new();
    for (name, value) in headers {
        let name_text = name.as_str();
        let hop = HOP_BY_HOP.contains(&name_text)
            || name_text.starts_with("proxy-")
            || named.iter().any(|named| named == name_text);
        if !hop {
            kept.append(name.clone(), value.clone());
        }
    }
    kept
}

fn fits(template: &str, path: &str) -> Option<usize> {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let (template, path): (Vec<&str>, Vec<&str>) =
        (template.split('/').collect(), path.split('/').collect());
    if template.len() != path.len() {
        return None;
    }
    let mut literal = 0;
    for (want, got) in template.iter().zip(&path) {
        if want.starts_with('{') && want.ends_with('}') && !got.is_empty() {
            continue;
        }
        if want != got {
            return None;
        }
        literal += 1;
    }
    Some(literal)
}

fn cap(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    if text.len() <= BODY_LIMIT {
        return value;
    }
    let mut end = BODY_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"text": &text[..end], "truncated": true})
}

fn encode_body(body: &Value, headers: &HeaderMap) -> Result<Vec<u8>, ToolError> {
    let header = headers.get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let kind = header.unwrap_or_default().split(';').next().unwrap_or_default();
    let kind = kind.trim().to_ascii_lowercase();
    let form = kind == "application/x-www-form-urlencoded";
    match body {
        _ if kind.is_empty() || kind.contains("json") => to_json(body),
        Value::Object(fields) if form => encode_form(fields),
        _ if form => Err(ToolError::invalid("a form body must be an object")),
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        _ => to_json(body),
    }
}

fn to_json(body: &Value) -> Result<Vec<u8>, ToolError> {
    serde_json::to_vec(body).map_err(|e| ToolError::invalid(e.to_string()))
}

fn encode_form(fields: &Map<String, Value>) -> Result<Vec<u8>, ToolError> {
    let mut url =
        Url::parse("http://form.invalid/").map_err(|e| ToolError::invalid(e.to_string()))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in fields {
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => {
                    let message =
                        format!("form field '{name}' must be a string, number or boolean");
                    return Err(ToolError::invalid(message));
                }
            };
            pairs.append_pair(name, &value);
        }
    }
    Ok(url.query().unwrap_or_default().as_bytes().to_vec())
}

/// Refuse any argument but `allowed`.
pub(crate) fn allow_keys(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), ToolError> {
    match args.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(ToolError::invalid(format!("unexpected argument '{key}'"))),
        None => Ok(()),
    }
}

fn request_headers(
    sent: Option<&Value>,
    pinned: Option<&BTreeMap<String, String>>,
    json_body: bool,
) -> Result<HeaderMap, ToolError> {
    let mut headers = HeaderMap::new();
    let sent = match sent {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err(ToolError::invalid("headers must be an object of strings")),
    };
    if sent.len() > HEADER_LIMIT {
        return Err(ToolError::invalid(format!("at most {HEADER_LIMIT} headers are allowed")));
    }
    for (name, value) in &sent {
        let value = value
            .as_str()
            .ok_or_else(|| ToolError::invalid(format!("header '{name}' must be a string")))?;
        let lower = name.to_ascii_lowercase();
        let dropped = matches!(
            lower.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) || (lower == "cookie" && pins(pinned));
        if dropped {
            continue;
        }
        insert_header(&mut headers, name, value)?;
    }
    if json_body && !headers.contains_key(reqwest::header::CONTENT_TYPE) {
        insert_header(&mut headers, "content-type", "application/json")?;
    }
    for (name, value) in pinned.into_iter().flatten() {
        insert_header(&mut headers, name, value)?;
    }
    Ok(headers)
}

fn pins(pinned: Option<&BTreeMap<String, String>>) -> bool {
    pinned.is_some_and(|headers| !headers.is_empty())
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), ToolError> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| ToolError::invalid(format!("bad header name '{name}'")))?;
    let value = HeaderValue::from_str(value)
        .map_err(|_| ToolError::invalid(format!("bad value for header '{name}'")))?;
    headers.insert(name, value);
    Ok(())
}

fn has_overlong_utf8(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|pair| {
        let [lead, next] = pair else { return false };
        match lead {
            0xC0 | 0xC1 => true,
            0xE0 => (0x80..0xA0).contains(next),
            0xF0 => (0x80..0x90).contains(next),
            _ => false,
        }
    })
}

fn load_docs(path: &Path) -> Result<Vec<Operation>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: Value = match serde_json::from_str(&text) {
        Ok(json) => json,
        Err(_) => serde_saphyr::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
    };
    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{}: no paths", path.display()))?;
    let mut operations = Vec::new();
    for (route, item) in paths {
        let mut budget = REF_BUDGET;
        let shared =
            item.get("parameters").map_or_else(|| json!([]), |p| resolve(p, &root, 0, &mut budget));
        for method in ["get", "put", "post", "delete", "patch", "head", "options"] {
            let Some(op) = item.get(method) else { continue };
            let mut budget = REF_BUDGET;
            let op = resolve(op, &root, 0, &mut budget);
            let mut parameters = shared.as_array().cloned().unwrap_or_default();
            parameters.extend(
                op.get("parameters").and_then(Value::as_array).cloned().unwrap_or_default(),
            );
            let text = |key: &str| op.get(key).and_then(Value::as_str).map(str::to_string);
            let detail = json!({
                "operation": text("operationId"),
                "method": method.to_uppercase(),
                "path": route,
                "summary": text("summary"),
                "description": text("description"),
                "parameters": parameters,
                "request_body": op.get("requestBody"),
                "responses": op.get("responses"),
            });
            operations.push(Operation {
                id: text("operationId"),
                method: method.to_uppercase(),
                path: route.clone(),
                summary: text("summary"),
                detail,
            });
        }
    }
    Ok(operations)
}

fn resolve(value: &Value, root: &Value, depth: usize, budget: &mut usize) -> Value {
    *budget = budget.saturating_sub(1);
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && depth < REF_DEPTH
                && *budget > 0
                && let Some(target) =
                    reference.strip_prefix('#').and_then(|pointer| root.pointer(pointer))
            {
                return resolve(target, root, depth + 1, budget);
            }
            let resolved: BTreeMap<String, Value> =
                map.iter().map(|(k, v)| (k.clone(), resolve(v, root, depth, budget))).collect();
            Value::Object(resolved.into_iter().collect())
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| resolve(v, root, depth, budget)).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(base: &str, docs: Option<Vec<Operation>>) -> Endpoint {
        Endpoint { base: Url::parse(base).unwrap(), description: None, docs }
    }

    fn tools(openapi: Option<&Path>) -> Tools {
        let docs = openapi.map(|p| load_docs(p).unwrap());
        let apps = [("crm".to_string(), endpoint("http://127.0.0.1:3000", docs))].into();
        Tools { client: reqwest::Client::new(), apps }
    }

    #[test]
    fn the_codes_headers_lose_those_about_its_connection_and_take_the_personas() {
        let mut sent = HeaderMap::new();
        for (name, value) in [
            ("host", "10.0.3.5:8001"),
            ("connection", "keep-alive, x-trace"),
            ("x-trace", "1"),
            ("keep-alive", "timeout=5"),
            ("expect", "100-continue"),
            ("proxy-authorization", "Basic eA=="),
            ("content-length", "12"),
            ("authorization", "token someone-elses"),
        ] {
            sent.insert(name, value.parse().unwrap());
        }
        sent.append("accept", "application/json".parse().unwrap());
        sent.append("accept", "text/plain".parse().unwrap());
        let pinned = BTreeMap::from([("Authorization".to_string(), "token maya".to_string())]);
        sent.insert("cookie", "MMAUTHTOKEN=someone-elses".parse().unwrap());
        let headers = forwarded_headers(&sent, Some(&pinned)).unwrap();
        assert_eq!(headers["authorization"], "token maya", "the persona's win");
        assert!(!headers.contains_key("cookie"), "a cookie could act as someone else");
        let unpinned = forwarded_headers(&sent, None).unwrap();
        assert_eq!(
            unpinned["cookie"], "MMAUTHTOKEN=someone-elses",
            "nothing pinned, nothing to guard"
        );
        assert_eq!(headers.get_all("accept").iter().count(), 2, "a repeated header stays whole");
        let gone = ["host", "connection", "x-trace", "keep-alive", "expect", "proxy-authorization"];
        for name in gone.into_iter().chain(["content-length"]) {
            assert!(!headers.contains_key(name), "{name}");
        }
    }

    #[test]
    fn an_apps_links_and_redirects_point_back_at_the_proxy() {
        let api = "http://127.0.0.1:3000/api/v1";
        let mut sent = HeaderMap::new();
        let link = format!(
            "<{api}/repos/a/b/issues?page=2>; rel=\"next\", <{api}/repos/a/b/issues?page=5>; rel=\"last\""
        );
        sent.insert("link", link.parse().unwrap());
        sent.insert("location", format!("{api}/repos/a/b").parse().unwrap());
        sent.insert("x-total-count", "93".parse().unwrap());
        sent.insert("transfer-encoding", "chunked".parse().unwrap());
        let proxied = "http://10.0.3.5:8001/tok/gitea";
        let headers = answered_headers(&sent, api, proxied);
        let expected = format!(
            "<{proxied}/repos/a/b/issues?page=2>; rel=\"next\", <{proxied}/repos/a/b/issues?page=5>; rel=\"last\""
        );
        assert_eq!(headers["link"], expected.as_str());
        assert_eq!(headers["location"], format!("{proxied}/repos/a/b").as_str());
        assert_eq!(headers["x-total-count"], "93", "the app's own headers stay");
        assert!(!headers.contains_key("transfer-encoding"));
    }

    #[test]
    fn a_body_takes_the_form_its_content_type_asks_for() {
        let encoded = |kind: Option<&str>, body: Value| {
            let mut headers = HeaderMap::new();
            if let Some(kind) = kind {
                headers.insert("Content-Type", HeaderValue::from_str(kind).unwrap());
            }
            encode_body(&body, &headers).map(|bytes| String::from_utf8(bytes).unwrap())
        };
        let form = Some("application/x-www-form-urlencoded");
        let cases = [
            (None, json!({"a": 1}), r#"{"a":1}"#),
            (Some("application/json; charset=utf-8"), json!("x"), r#""x""#),
            (Some("application/vnd.api+json"), json!({"a": [1]}), r#"{"a":[1]}"#),
            (form, json!({"q": "a b&c", "n": 2, "ok": true}), "n=2&ok=true&q=a+b%26c"),
            (Some("Application/X-WWW-Form-Urlencoded; charset=utf-8"), json!({"a": "1"}), "a=1"),
            (Some("text/plain"), json!("raw <b>text</b>"), "raw <b>text</b>"),
            (Some("application/xml"), json!("<a/>"), "<a/>"),
            (Some("text/plain"), json!({"a": 1}), r#"{"a":1}"#),
        ];
        for (kind, body, sent) in cases {
            assert_eq!(encoded(kind, body.clone()).unwrap(), sent, "{kind:?} {body}");
        }
        let err = encoded(form, json!({"a": {"b": 1}})).unwrap_err();
        assert_eq!(err.message, "form field 'a' must be a string, number or boolean");
        let err = encoded(form, json!("a=1")).unwrap_err();
        assert_eq!(
            (err.error_type, err.message.as_str()),
            ("invalid_args", "a form body must be an object")
        );
    }

    #[test]
    fn the_tools_listed_are_the_tools_known() {
        let without_docs = tools(None);
        let names = |tools: &Tools| -> Vec<String> {
            tools
                .list(false, false)
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(names(&without_docs), ["api_request", "submit"]);
        let unlisted = without_docs.api_docs(&Map::new()).unwrap_err();
        assert_eq!(unlisted, ToolError::unknown("api_docs"), "not listed, so no tool");
        let apps = [("crm".to_string(), endpoint("http://127.0.0.1:3000", Some(Vec::new())))];
        let with_docs = Tools { apps: apps.into(), ..without_docs };
        assert_eq!(names(&with_docs), ["api_request", "api_docs", "submit"]);
        assert!(with_docs.api_docs(&Map::new()).is_ok());
    }

    #[test]
    fn the_code_surface_lists_the_code_tool_in_place_of_api_request() {
        let docs = [("crm".to_string(), endpoint("http://127.0.0.1:3000", Some(Vec::new())))];
        let with = Tools { apps: docs.into(), ..tools(None) };
        let names = |code: bool, clock: bool| -> Vec<String> {
            let listed = with.list(code, clock);
            listed.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(names(true, false), ["api_docs", "run_code", "submit"]);
        assert_eq!(names(false, false), ["api_request", "api_docs", "submit"], "as before");
        assert_eq!(names(false, true), ["api_request", "api_docs", "wait", "submit"]);
        assert_eq!(names(true, true), ["api_docs", "run_code", "wait", "submit"]);
        let wait = with.list(false, true).into_iter().find(|t| t["name"] == "wait").unwrap();
        assert_eq!(wait["input_schema"]["required"], json!(["minutes"]));
        assert_eq!(wait["input_schema"]["properties"]["minutes"]["type"], "integer");
    }

    proptest::proptest! {
        #[test]
        fn a_path_never_leaves_the_api(path in r"/[ -~]{0,40}") {
            let api = endpoint("http://127.0.0.1:3000/api", None);
            if let Ok(url) = api.url_for(&path) {
                proptest::prop_assert_eq!(url.host_str(), Some("127.0.0.1"));
                proptest::prop_assert_eq!(url.port(), Some(3000));
                proptest::prop_assert!(url.path().starts_with("/api/"), "{url}");
                proptest::prop_assert!(!url.path().split('/').any(|s| s == ".."), "{url}");
            }
        }
    }

    #[test]
    fn paths_must_stay_on_the_api() {
        let tools = endpoint("http://127.0.0.1:3000", None);
        assert_eq!(
            tools.url_for("/deals?owner_id=12").unwrap().as_str(),
            "http://127.0.0.1:3000/deals?owner_id=12"
        );
        assert!(
            tools.url_for("//evil.test/x").unwrap().as_str().starts_with("http://127.0.0.1:3000/")
        );
        let escapes = [
            "deals",
            "/../etc",
            "/a/%2e%2e/b",
            "/a/.%2E/b",
            "/a%2f..%2fb",
            "/a/..\\b",
            "/%c0%ae%c0%ae/x",
            "/..%c0%af/x",
            "/%e0%80%ae%e0%80%ae/x",
            "/%f0%80%80%ae/x",
            "/%e0%81%9c/x",
        ];
        for bad in escapes.into_iter().chain(["http://x/", "/a/.\t./b", "/.\n./x", "/x\r"]) {
            let err = tools.url_for(bad).unwrap_err();
            assert_eq!(err.error_type, "invalid_args", "{bad:?}");
        }
        assert!(tools.url_for("/files/..hidden").is_ok());
        assert!(tools.url_for("/files/caf%E9.txt").is_ok(), "Latin-1 is not an attack");
        assert!(tools.url_for("/blobs/%FF%00a").is_ok(), "nor is a binary id");
        assert!(tools.url_for("/caf%C3%A9").is_ok(), "valid two-byte UTF-8 is fine");
        assert!(tools.url_for("/%E0%A4%85").is_ok(), "so is valid three-byte UTF-8");
        assert!(tools.url_for("/l%E0x").is_ok(), "and Latin-1 before a letter");
        let api = endpoint("http://127.0.0.1:3000/api", None);
        assert_eq!(api.url_for("/deals").unwrap().as_str(), "http://127.0.0.1:3000/api/deals");
        assert_eq!(api.url_for("/.\t./admin").unwrap_err().error_type, "invalid_args");
    }

    fn two_apps() -> Tools {
        let list = Operation {
            id: Some("listDeals".into()),
            method: "GET".into(),
            path: "/deals".into(),
            summary: None,
            detail: json!({"summary": "deals"}),
        };
        let mut east = endpoint("http://127.0.0.1:3000", Some(vec![list]));
        east.description = Some("the east CRM".into());
        let mut west = endpoint("http://127.0.0.1:3001/api", None);
        west.description = Some("the west CRM".into());
        let apps = [("east".to_string(), east), ("west".to_string(), west)];
        Tools { client: reqwest::Client::new(), apps: apps.into() }
    }

    #[test]
    fn several_apps_add_a_required_app_to_the_api_tools() {
        let listed = two_apps().list(false, false);
        let schema =
            |name: &str| listed.iter().find(|t| t["name"] == name).unwrap()["input_schema"].clone();
        let request = schema("api_request");
        assert_eq!(request["properties"]["app"]["enum"], json!(["east", "west"]));
        assert_eq!(
            request["properties"]["app"]["description"],
            "The app to call: east, the east CRM; west, the west CRM"
        );
        assert_eq!(request["required"], json!(["app", "method", "path"]));
        let docs = schema("api_docs");
        assert_eq!(docs["properties"]["app"]["enum"], json!(["east"]), "only apps with a spec");
        assert_eq!(docs["required"], json!(["app"]));
        assert!(schema("submit")["properties"].get("app").is_none());
        let raw: Vec<Value> = serde_json::from_str(TOOLS).unwrap();
        let one = tools(None).list(false, false);
        assert_eq!(one[0], raw[0], "one app keeps the schema tools.json has");
    }

    #[test]
    fn the_app_argument_picks_the_api() {
        let args = |app: Value| -> Map<String, Value> {
            let mut args = Map::new();
            if !app.is_null() {
                args.insert("app".into(), app);
            }
            args
        };
        let one = tools(None);
        assert_eq!(one.endpoint(&args(Value::Null)).unwrap().0, "crm");
        let err = one.endpoint(&args(json!("crm"))).err().unwrap();
        assert_eq!(err.message, "unexpected argument 'app'", "one api takes no app");
        let two = two_apps();
        let (app, west) = two.endpoint(&args(json!("west"))).unwrap();
        assert_eq!(app, "west");
        assert_eq!(west.url_for("/deals").unwrap().as_str(), "http://127.0.0.1:3001/api/deals");
        let refused = [
            (Value::Null, "app is required: one of east, west"),
            (json!("ghost"), "unknown app 'ghost': one of east, west"),
            (json!(["east"]), "app must be a string"),
        ];
        for (app, expected) in refused {
            let err = two.endpoint(&args(app)).err().unwrap();
            assert_eq!((err.error_type, err.message.as_str()), ("invalid_args", expected));
        }
    }

    #[test]
    fn docs_are_each_apps_own() {
        let two = two_apps();
        let ask = |app: &str| {
            let args: Map<String, Value> = [("app".to_string(), json!(app))].into_iter().collect();
            two.api_docs(&args)
        };
        assert_eq!(ask("east").unwrap()["operations"][0]["operation"], "listDeals");
        let err = ask("west").unwrap_err();
        assert_eq!(err.error_type, "tool_not_found");
        assert!(err.message.contains("for app 'west'"), "{}", err.message);
    }

    #[test]
    fn persona_headers_win_and_hop_headers_drop() {
        let pinned: BTreeMap<String, String> =
            [("Authorization".into(), "Bearer dana".into())].into();
        let sent = json!({
            "authorization": "Bearer priya",
            "Host": "evil",
            "X-Trace": "1",
            "Connection": "close",
            "Cookie": "MMAUTHTOKEN=priya",
        });
        let headers = request_headers(Some(&sent), Some(&pinned), true).unwrap();
        assert_eq!(headers.get_all("content-type").iter().count(), 1);
        assert_eq!(headers["authorization"], "Bearer dana");
        assert!(headers.get("cookie").is_none(), "a cookie could act as priya");
        let unpinned = request_headers(Some(&sent), None, false).unwrap();
        assert_eq!(unpinned["cookie"], "MMAUTHTOKEN=priya");
        assert_eq!(headers["x-trace"], "1");
        assert!(headers.get("host").is_none() && headers.get("connection").is_none());
        assert_eq!(
            request_headers(Some(&json!({"x": 1})), Some(&pinned), false).unwrap_err().error_type,
            "invalid_args"
        );
    }

    #[test]
    fn a_flood_of_headers_is_refused_rather_than_a_panic() {
        let flood: Map<String, Value> =
            (0..30_000).map(|i| (format!("x-h{i}"), json!("1"))).collect();
        let error = request_headers(Some(&Value::Object(flood)), None, false).unwrap_err();
        assert_eq!(error.error_type, "invalid_args");
    }

    #[test]
    fn docs_list_and_resolve_refs() {
        let path = std::env::temp_dir().join(format!("miroir-openapi-{}.yaml", std::process::id()));
        let spec = r"
paths:
  /deals/{id}:
    parameters: [{$ref: '#/components/parameters/Id'}]
    patch:
      operationId: updateDeal
      requestBody: {content: {application/json: {schema: {$ref: '#/components/schemas/Deal'}}}}
components:
  parameters:
    Id: {name: id, in: path}
  schemas:
    Deal: {type: object, properties: {owner: {$ref: '#/components/schemas/Deal'}}}
";
        std::fs::write(&path, spec).unwrap();
        let tools = tools(Some(&path));
        let list = tools.api_docs(&Map::new()).unwrap();
        assert_eq!(
            list["operations"][0],
            json!({
                "operation": "updateDeal",
                "method": "PATCH",
                "path": "/deals/{id}",
                "summary": null,
            })
        );
        let by_id =
            tools.api_docs(json!({"operation": "updateDeal"}).as_object().unwrap()).unwrap();
        let by_route =
            tools.api_docs(json!({"operation": "patch /deals/{id}"}).as_object().unwrap()).unwrap();
        assert_eq!(by_id, by_route);
        assert_eq!(by_id["parameters"][0]["name"], "id");
        let schema = &by_id["request_body"]["content"]["application/json"]["schema"];
        let mut level = schema;
        for _ in 0..4 {
            level = &level["properties"]["owner"];
        }
        assert_eq!(level["properties"]["owner"]["$ref"], "#/components/schemas/Deal");
        let unknown =
            tools.api_docs(json!({"operation": "nope"}).as_object().unwrap()).unwrap_err();
        assert_eq!(unknown.error_type, "invalid_args");
        let extra = tools.api_docs(json!({"op": "x"}).as_object().unwrap()).unwrap_err();
        assert_eq!(extra.message, "unexpected argument 'op'");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn docs_find_the_operation_a_concrete_route_fits() {
        let path =
            std::env::temp_dir().join(format!("miroir-openapi-routes-{}.yaml", std::process::id()));
        let spec = r"
paths:
  /json/2/{model}/search_read: {post: {operationId: search_read}}
  /json/2/{model}/{method}: {post: {operationId: call_method}}
";
        std::fs::write(&path, spec).unwrap();
        let tools = tools(Some(&path));
        let named = |route: &str| {
            let args = json!({"operation": route});
            tools.api_docs(args.as_object().unwrap()).map(|op| op["operation"].clone())
        };
        assert_eq!(named("POST /json/2/sale.order/search_read").unwrap(), "search_read");
        assert_eq!(named("POST /json/2/sale.order/action_confirm?x=1").unwrap(), "call_method");
        assert_eq!(named("post /json/2/{model}/search_read").unwrap(), "search_read");
        assert_eq!(
            named("GET /json/2/sale.order/search_read").unwrap_err().error_type,
            "invalid_args"
        );
        assert_eq!(named("POST /json/2/sale.order").unwrap_err().error_type, "invalid_args");
        assert_eq!(named("POST /json/2//search_read").unwrap_err().error_type, "invalid_args");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn schemas_that_reference_each_other_stay_within_the_budget() {
        let references: Vec<Value> =
            (0..20).map(|i| json!({"$ref": format!("#/components/schemas/S{i}")})).collect();
        let schemas: Map<String, Value> =
            (0..20).map(|i| (format!("S{i}"), json!({"anyOf": references}))).collect();
        let root = json!({"components": {"schemas": schemas}});
        let started = std::time::Instant::now();
        let mut budget = REF_BUDGET;
        let resolved = resolve(&references[0], &root, 0, &mut budget);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "20^5 expansions");
        assert!(resolved.to_string().contains("$ref"), "the rest stays as references");
    }

    #[test]
    fn big_docs_are_cut() {
        let big = json!({"text": "x".repeat(BODY_LIMIT * 2)});
        let capped = cap(big);
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["text"].as_str().unwrap().len(), BODY_LIMIT);
    }
}
