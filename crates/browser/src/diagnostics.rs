//! What a tab's page logs and requests, kept for agents to read. Every string is redacted as it
//! is captured, and raw CDP payloads are never kept.

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::{Captures, Regex};
use serde::Deserialize;
use serde::de::IgnoredAny;
use url::Url;

pub const REDACTED: &str = "[redacted]";
pub const WITHHELD: &str = "entry withheld: it contained the account's password";
pub const PENDING_AFTER: Duration = Duration::from_secs(5);

const MAX_TEXT: usize = 500;
/// Bounds the work redaction does on the UI thread for a huge message.
const MAX_INPUT: usize = 16_000;
const CONSOLE_CAP: usize = 200;
const OPEN_CAP: usize = 100;
const ORDINARY_CAP: usize = 200;
const FAILED_CAP: usize = 50;

const SENSITIVE_NAMES: [&str; 9] = [
    "password",
    "passwd",
    "pwd",
    "secret",
    "client_secret",
    "cookie",
    "authorization",
    "apikey",
    "api_key",
];

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

static URL_IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"\b[A-Za-z][A-Za-z0-9+.\-]*://[^\s"'`<>()\[\]{}]+"#));
static LINE_COL: LazyLock<Regex> = LazyLock::new(|| regex(r"(?::\d+){1,2}$"));
static JWT: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]*"));
static NAME_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    let names = format!(
        r"{}|[A-Za-z0-9_\-]*(?:token|secret)",
        SENSITIVE_NAMES.join("|")
    );
    regex(&format!(
        r#"(?i)(^|[^A-Za-z0-9_$\-])(["']?)({names})(["']?)([ \t]*[:=][ \t]*)("(?:[^"\\\r\n]|\\.)*"?|'(?:[^'\\\r\n]|\\.)*'?|`[^`\r\n]*`?|[^,}};&\s][^,}};&\r\n]*)"#
    ))
});
static AUTH_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\b(bearer|basic)([ \t]+)[A-Za-z0-9+/=._~\-]+"));
static RUN: LazyLock<Regex> = LazyLock::new(|| regex(r"[A-Za-z0-9+=_\-/]+"));

pub fn is_sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SENSITIVE_NAMES.contains(&name.as_str()) || name.ends_with("token") || name.ends_with("secret")
}

/// Origin and path only: no userinfo, query or fragment.
pub fn cut_url(raw: &str) -> String {
    if let Ok(url) = Url::parse(raw) {
        if let Some(host) = url.host_str() {
            let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
            return format!("{}://{host}{port}{}", url.scheme(), url.path());
        }
        if url.scheme() == "data" {
            return raw.split(',').next().unwrap_or(raw).to_string();
        }
    }
    let base = raw.split(['?', '#']).next().unwrap_or(raw);
    match base.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            format!("{scheme}://{host}{path}")
        }
        None => base.to_string(),
    }
}

/// A URL found in text keeps a `:line:col` that followed its query, as in a Vite dependency's
/// stack frame (`deps/x.js?v=1a2b:10:5`).
fn cut_url_in_text(found: &str) -> String {
    let url = found.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    let trailing = &found[url.len()..];
    let (url, location) = match LINE_COL.find(url) {
        Some(at) if url[..at.start()].contains(['?', '#']) => url.split_at(at.start()),
        _ => (url, ""),
    };
    format!("{}{location}{trailing}", cut_url(url))
}

fn token_shaped(run: &str) -> bool {
    if run.contains('/') {
        return false;
    }
    let hex = run.len() >= 32 && run.chars().all(|c| c.is_ascii_hexdigit());
    let mixed = run.len() >= 24
        && run.chars().any(|c| c.is_ascii_uppercase())
        && run.chars().any(|c| c.is_ascii_lowercase())
        && run.chars().any(|c| c.is_ascii_digit());
    hex || mixed
}

fn redact_prose(text: &str) -> String {
    let text = JWT.replace_all(text, REDACTED);
    let text = NAME_VALUE.replace_all(&text, |c: &Captures| {
        let value = match c[6].chars().next() {
            Some(quote @ ('"' | '\'' | '`')) => format!("{quote}{REDACTED}{quote}"),
            _ => REDACTED.to_string(),
        };
        format!("{}{}{}{}{}{value}", &c[1], &c[2], &c[3], &c[4], &c[5])
    });
    let text = AUTH_SCHEME.replace_all(&text, format!("${{1}}${{2}}{REDACTED}"));
    RUN.replace_all(&text, |c: &Captures| {
        if token_shaped(&c[0]) {
            REDACTED.to_string()
        } else {
            c[0].to_string()
        }
    })
    .into_owned()
}

/// Pattern redaction of free text. URLs are cut to origin and path, and the token-shaped rule
/// never looks inside them.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in URL_IN_TEXT.find_iter(text) {
        out.push_str(&redact_prose(&text[last..found.start()]));
        out.push_str(&JWT.replace_all(&cut_url_in_text(found.as_str()), REDACTED));
        last = found.end();
    }
    out.push_str(&redact_prose(&text[last..]));
    out
}

fn truncate(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        text
    } else {
        text.chars().take(max - 1).chain(['…']).collect()
    }
}

/// Redacts, then truncates to 500 characters.
pub fn clean(text: &str) -> String {
    let input: String;
    let text = if text.len() > MAX_INPUT {
        input = text.chars().take(MAX_INPUT).collect();
        &input
    } else {
        text
    };
    truncate(redact(text), MAX_TEXT)
}

/// Implemented by what the diagnostics keep, so a reader can withhold an entry.
pub trait Entry {
    fn contains(&self, needle: &str) -> bool;
}

/// Whether an entry must be withheld because it contains the account's password.
pub fn withheld(password: &str, entry: &impl Entry) -> bool {
    !password.trim().is_empty() && entry.contains(password)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Primitive {
    Bool(bool),
    Number(f64),
    Text(String),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObject {
    #[serde(rename = "type", default)]
    kind: String,
    subtype: Option<String>,
    value: Option<Primitive>,
    unserializable_value: Option<String>,
    description: Option<String>,
    preview: Option<ObjectPreview>,
}

#[derive(Deserialize)]
struct ObjectPreview {
    subtype: Option<String>,
    #[serde(default)]
    overflow: bool,
    #[serde(default)]
    properties: Vec<PropertyPreview>,
}

#[derive(Deserialize)]
struct PropertyPreview {
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    value: Option<String>,
}

fn render_property(property: &PropertyPreview) -> String {
    if is_sensitive_name(&property.name) {
        return REDACTED.to_string();
    }
    match (property.kind.as_str(), &property.value) {
        ("string", Some(value)) => format!("'{value}'"),
        (_, Some(value)) => value.clone(),
        (kind, None) => kind.to_string(),
    }
}

fn render_preview(preview: &ObjectPreview) -> String {
    let array = preview.subtype.as_deref() == Some("array");
    let mut items: Vec<String> = preview
        .properties
        .iter()
        .map(|property| {
            let value = render_property(property);
            if array {
                value
            } else {
                format!("{}: {value}", property.name)
            }
        })
        .collect();
    if preview.overflow {
        items.push("…".into());
    }
    if array {
        format!("[{}]", items.join(", "))
    } else {
        format!("{{{}}}", items.join(", "))
    }
}

fn render_arg(arg: &RemoteObject) -> String {
    match &arg.value {
        Some(Primitive::Text(text)) => return text.clone(),
        Some(Primitive::Bool(value)) => return value.to_string(),
        Some(Primitive::Number(value)) => return value.to_string(),
        Some(Primitive::Other(_)) | None => {}
    }
    match (arg.kind.as_str(), arg.subtype.as_deref(), &arg.preview) {
        ("undefined", _, _) => "undefined".into(),
        ("object", Some("null"), _) => "null".into(),
        ("object", None | Some("array"), Some(preview)) => render_preview(preview),
        _ => arg
            .description
            .clone()
            .or_else(|| arg.unserializable_value.clone())
            .unwrap_or_else(|| arg.kind.clone()),
    }
}

/// Console arguments as one line, before redaction.
pub fn render_args(args: &[RemoteObject]) -> String {
    args.iter().map(render_arg).collect::<Vec<_>>().join(" ")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StackTrace {
    #[serde(default)]
    call_frames: Vec<CallFrame>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallFrame {
    #[serde(default)]
    url: String,
    #[serde(default)]
    line_number: i64,
    #[serde(default)]
    column_number: i64,
}

fn location(url: &str, line: Option<i64>, column: Option<i64>) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let mut at = cut_url(url);
    for number in [line, column].into_iter().map_while(|n| n) {
        at.push_str(&format!(":{}", number.max(0) + 1));
    }
    Some(clean(&at))
}

fn frame_location(stack: Option<&StackTrace>) -> Option<String> {
    let frame = stack?.call_frames.first()?;
    location(
        &frame.url,
        Some(frame.line_number),
        Some(frame.column_number),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleApiCalled {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    args: Vec<RemoteObject>,
    #[serde(default)]
    timestamp: f64,
    stack_trace: Option<StackTrace>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceptionThrown {
    #[serde(default)]
    timestamp: f64,
    exception_details: ExceptionDetails,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExceptionDetails {
    #[serde(default)]
    text: String,
    url: Option<String>,
    #[serde(default)]
    line_number: i64,
    #[serde(default)]
    column_number: i64,
    stack_trace: Option<StackTrace>,
    exception: Option<RemoteObject>,
}

#[derive(Deserialize)]
pub struct LogEntryAdded {
    entry: LogEntry,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    #[serde(default)]
    source: String,
    #[serde(default)]
    level: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    timestamp: f64,
    url: Option<String>,
    line_number: Option<i64>,
    stack_trace: Option<StackTrace>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
}

impl Level {
    fn from_cdp(name: &str) -> Level {
        match name {
            "error" | "assert" => Level::Error,
            "warning" | "warn" => Level::Warning,
            "debug" | "verbose" => Level::Debug,
            _ => Level::Info,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleEntry {
    pub level: Level,
    /// `console`, `exception`, or the browser log's source (`network`, `security`, …).
    pub source: String,
    pub text: String,
    /// `url:line:col`.
    pub location: Option<String>,
    /// Milliseconds since the epoch.
    pub timestamp: f64,
    pub load: Option<u32>,
}

impl Entry for ConsoleEntry {
    fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
            || self.source.contains(needle)
            || self.location.as_deref().is_some_and(|l| l.contains(needle))
    }
}

#[derive(Default)]
pub struct ConsoleLog {
    entries: VecDeque<ConsoleEntry>,
}

impl ConsoleLog {
    fn push(&mut self, entry: ConsoleEntry) {
        if self.entries.len() == CONSOLE_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn entries(&self) -> impl Iterator<Item = &ConsoleEntry> {
        self.entries.iter()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestWillBeSent {
    request_id: String,
    #[serde(default)]
    loader_id: String,
    frame_id: Option<String>,
    request: RequestLine,
    #[serde(default)]
    timestamp: f64,
    #[serde(rename = "type")]
    resource_type: Option<String>,
    redirect_response: Option<Status>,
}

#[derive(Deserialize)]
struct RequestLine {
    url: String,
    method: String,
}

#[derive(Deserialize)]
struct Status {
    status: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseReceived {
    request_id: String,
    response: Status,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadingFinished {
    request_id: String,
    #[serde(default)]
    timestamp: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadingFailed {
    request_id: String,
    #[serde(default)]
    timestamp: f64,
    #[serde(default)]
    error_text: String,
    #[serde(default)]
    canceled: bool,
    blocked_reason: Option<String>,
    cors_error_status: Option<CorsErrorStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CorsErrorStatus {
    cors_error: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Open,
    /// Open for longer than [`PENDING_AFTER`]; only reads report it.
    Pending,
    Finished {
        status: Option<u16>,
    },
    Redirected {
        status: u16,
    },
    Failed {
        error: String,
        canceled: bool,
        blocked: Option<String>,
        status: Option<u16>,
    },
    /// Pushed out of the open requests by newer ones.
    Abandoned,
}

impl Outcome {
    pub fn failed(&self) -> bool {
        match self {
            Outcome::Finished {
                status: Some(status),
            }
            | Outcome::Redirected { status } => *status >= 400,
            Outcome::Pending | Outcome::Failed { .. } | Outcome::Abandoned => true,
            Outcome::Open | Outcome::Finished { status: None } => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    /// Origin and path.
    pub url: String,
    pub resource_type: String,
    pub outcome: Outcome,
    /// For an open request, how long it has been open.
    pub duration_ms: Option<u64>,
    pub load: Option<u32>,
    seq: u64,
}

impl Entry for Request {
    fn contains(&self, needle: &str) -> bool {
        let outcome = match &self.outcome {
            Outcome::Failed { error, blocked, .. } => {
                error.contains(needle) || blocked.as_deref().is_some_and(|b| b.contains(needle))
            }
            _ => false,
        };
        outcome
            || self.method.contains(needle)
            || self.url.contains(needle)
            || self.resource_type.contains(needle)
    }
}

struct OpenRequest {
    request: Request,
    status: Option<u16>,
    started: f64,
    opened_at: Instant,
}

fn streams(resource_type: &str) -> bool {
    matches!(resource_type, "EventSource" | "WebSocket")
}

/// Requests in flight, plus two pools of closed ones so a burst of ordinary requests can't push
/// out a failure.
#[derive(Default)]
pub struct RequestLog {
    main_frame: Option<String>,
    /// Main-frame `loaderId`s, in the order first seen; load N is `loads[N - 1]`.
    loads: Vec<String>,
    open: HashMap<String, OpenRequest>,
    ordinary: VecDeque<Request>,
    failed: VecDeque<Request>,
    next_seq: u64,
}

impl RequestLog {
    pub fn current_load(&self) -> Option<u32> {
        (!self.loads.is_empty()).then_some(self.loads.len() as u32)
    }

    fn load_of(&mut self, loader: &str, frame: Option<&str>) -> Option<u32> {
        if let Some(at) = self.loads.iter().rposition(|l| l == loader) {
            return Some(at as u32 + 1);
        }
        if !loader.is_empty() && frame.is_some() && frame == self.main_frame.as_deref() {
            self.loads.push(loader.to_string());
            return self.current_load();
        }
        self.current_load()
    }

    fn retire(&mut self, request: Request) {
        let (pool, cap) = if request.outcome.failed() {
            (&mut self.failed, FAILED_CAP)
        } else {
            (&mut self.ordinary, ORDINARY_CAP)
        };
        if pool.len() == cap {
            pool.pop_front();
        }
        pool.push_back(request);
    }

    fn close(&mut self, id: &str, timestamp: f64, outcome: impl FnOnce(Option<u16>) -> Outcome) {
        let Some(open) = self.open.remove(id) else {
            return;
        };
        let mut request = open.request;
        request.outcome = outcome(open.status);
        request.duration_ms = Some(((timestamp - open.started).max(0.0) * 1000.0).round() as u64);
        self.retire(request);
    }

    fn abandon_oldest(&mut self) {
        let oldest = self
            .open
            .iter()
            .min_by_key(|(_, open)| open.request.seq)
            .map(|(id, _)| id.clone());
        if let Some(open) = oldest.and_then(|id| self.open.remove(&id)) {
            let mut request = open.request;
            request.outcome = Outcome::Abandoned;
            self.retire(request);
        }
    }

    /// A redirect closes the previous hop under the same request id before opening the next.
    pub fn will_be_sent(&mut self, event: RequestWillBeSent, now: Instant) {
        if let Some(redirect) = &event.redirect_response {
            let status = redirect.status;
            self.close(&event.request_id, event.timestamp, |_| {
                Outcome::Redirected { status }
            });
        }
        let load = self.load_of(&event.loader_id, event.frame_id.as_deref());
        if !self.open.contains_key(&event.request_id) && self.open.len() >= OPEN_CAP {
            self.abandon_oldest();
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let request = Request {
            method: event.request.method,
            url: clean(&cut_url(&event.request.url)),
            resource_type: event.resource_type.unwrap_or_else(|| "Other".into()),
            outcome: Outcome::Open,
            duration_ms: None,
            load,
            seq,
        };
        self.open.insert(
            event.request_id,
            OpenRequest {
                request,
                status: None,
                started: event.timestamp,
                opened_at: now,
            },
        );
    }

    pub fn response_received(&mut self, event: ResponseReceived) {
        if let Some(open) = self.open.get_mut(&event.request_id) {
            open.status = Some(event.response.status);
        }
    }

    pub fn loading_finished(&mut self, event: LoadingFinished) {
        self.close(&event.request_id, event.timestamp, |status| {
            Outcome::Finished { status }
        });
    }

    pub fn loading_failed(&mut self, event: LoadingFailed) {
        let blocked = event.blocked_reason.or_else(|| {
            event
                .cors_error_status
                .map(|cors| format!("cors: {}", cors.cors_error))
        });
        let error = clean(&event.error_text);
        let blocked = blocked.map(|b| clean(&b));
        let canceled = event.canceled;
        self.close(&event.request_id, event.timestamp, |status| {
            Outcome::Failed {
                error,
                canceled,
                blocked,
                status,
            }
        });
    }

    /// Oldest first. An open request older than [`PENDING_AFTER`] is `Pending`, except an
    /// event stream or WebSocket, which stays open by design.
    pub fn entries(&self, now: Instant, failed_only: bool) -> Vec<Request> {
        let open = self.open.values().map(|open| {
            let mut request = open.request.clone();
            let age = now.saturating_duration_since(open.opened_at);
            request.duration_ms = Some(age.as_millis() as u64);
            if age > PENDING_AFTER && !streams(&request.resource_type) {
                request.outcome = Outcome::Pending;
            }
            request
        });
        let ordinary = (!failed_only).then_some(&self.ordinary);
        let mut entries: Vec<Request> = self
            .failed
            .iter()
            .chain(ordinary.into_iter().flatten())
            .cloned()
            .chain(open)
            .filter(|request| !failed_only || request.outcome.failed())
            .collect();
        entries.sort_by_key(|request| request.seq);
        entries
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Console,
    Network,
    /// The main frame's id, which numbers loads.
    MainFrame,
}

/// A tab's console and requests. Both outlive navigations.
#[derive(Default)]
pub struct Diagnostics {
    pub console: ConsoleLog,
    pub requests: RequestLog,
    off: Vec<(Part, String)>,
}

impl Diagnostics {
    pub fn set_main_frame(&mut self, id: &str) {
        self.requests.main_frame = Some(id.to_string());
    }

    pub fn main_frame(&self) -> Option<&str> {
        self.requests.main_frame.as_deref()
    }

    /// Records why part of the diagnostics is off, for the matching tool to report.
    pub fn turn_off(&mut self, part: Part, note: String) {
        self.off.push((part, note));
    }

    pub fn notes(&self, part: Part) -> impl Iterator<Item = &str> {
        self.off
            .iter()
            .filter(move |(p, _)| *p == part)
            .map(|(_, note)| note.as_str())
    }

    fn push_console(
        &mut self,
        level: Level,
        source: &str,
        text: &str,
        location: Option<String>,
        timestamp: f64,
    ) {
        let entry = ConsoleEntry {
            level,
            source: source.to_string(),
            text: clean(text),
            location,
            timestamp,
            load: self.requests.current_load(),
        };
        self.console.push(entry);
    }

    pub fn console_api_called(&mut self, event: ConsoleApiCalled) {
        let text = render_args(&event.args);
        let at = frame_location(event.stack_trace.as_ref());
        self.push_console(
            Level::from_cdp(&event.kind),
            "console",
            &text,
            at,
            event.timestamp,
        );
    }

    pub fn exception_thrown(&mut self, event: ExceptionThrown) {
        let details = event.exception_details;
        let at = details
            .url
            .as_deref()
            .and_then(|url| location(url, Some(details.line_number), Some(details.column_number)))
            .or_else(|| frame_location(details.stack_trace.as_ref()));
        let text = details
            .exception
            .and_then(|exception| exception.description)
            .unwrap_or(details.text);
        self.push_console(Level::Error, "exception", &text, at, event.timestamp);
    }

    pub fn log_entry_added(&mut self, event: LogEntryAdded) {
        let entry = event.entry;
        let at = frame_location(entry.stack_trace.as_ref()).or_else(|| {
            location(
                entry.url.as_deref().unwrap_or_default(),
                entry.line_number,
                None,
            )
        });
        self.push_console(
            Level::from_cdp(&entry.level),
            &entry.source,
            &entry.text,
            at,
            entry.timestamp,
        );
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn parse<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn redacts_name_value_forms() {
        for (input, expected) in [
            ("password=hunter2", "password=[redacted]"),
            ("password: hunter2", "password: [redacted]"),
            ("'pwd': hunter2", "'pwd': [redacted]"),
            (
                r#""client_secret": "abc""#,
                r#""client_secret": "[redacted]""#,
            ),
            (
                r#"{"accessToken":"abc","user":"bob"}"#,
                r#"{"accessToken":"[redacted]","user":"bob"}"#,
            ),
            ("refresh_token=abc&x=1", "refresh_token=[redacted]&x=1"),
            (
                "ClientSecret = abc; next",
                "ClientSecret = [redacted]; next",
            ),
            ("Cookie: a; b", "Cookie: [redacted]; b"),
            (
                "apikey=k1, api_key=k2",
                "apikey=[redacted], api_key=[redacted]",
            ),
            ("passwd=x}", "passwd=[redacted]}"),
            ("authorization: anything here", "authorization: [redacted]"),
            ("X-Csrf-Token: v", "X-Csrf-Token: [redacted]"),
        ] {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn redacts_an_unquoted_key_with_a_quoted_value() {
        assert_eq!(
            redact("{password: 'x', user: 'bob'}"),
            "{password: '[redacted]', user: 'bob'}"
        );
    }

    #[test]
    fn a_quoted_value_ends_at_its_quote() {
        assert_eq!(
            redact(r#"token: "a, b; c" then more"#),
            r#"token: "[redacted]" then more"#
        );
    }

    #[test]
    fn an_unquoted_value_with_spaces_is_redacted_to_the_delimiter() {
        assert_eq!(
            redact("password: correct horse battery staple, user: bob"),
            "password: [redacted], user: bob"
        );
        assert_eq!(
            redact("secret=correct horse battery staple\nnext line"),
            "secret=[redacted]\nnext line"
        );
    }

    #[test]
    fn redacts_auth_schemes() {
        assert_eq!(redact("Bearer abc.def-ghi"), "Bearer [redacted]");
        assert_eq!(
            redact("sent Basic dXNlcjpwYXNz= to the api"),
            "sent Basic [redacted] to the api"
        );
        assert_eq!(
            redact("Authorization: Bearer x"),
            "Authorization: [redacted]"
        );
    }

    #[test]
    fn redacts_jwts() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl";
        assert_eq!(redact(&format!("got {jwt} back")), "got [redacted] back");
    }

    #[test]
    fn redacts_token_shaped_runs() {
        assert_eq!(
            redact("refresh with aB3dE5fG7hI9-jK1_lM3nO5pQ7 now"),
            "refresh with [redacted] now"
        );
        assert_eq!(
            redact("key 0123456789abcdef0123456789abcdef"),
            "key [redacted]"
        );
    }

    #[test]
    fn cuts_urls_to_origin_and_path() {
        assert_eq!(
            redact("GET https://user:pw@api.example.com/v1/items?access_token=abc#frag failed."),
            "GET https://api.example.com/v1/items failed."
        );
        assert_eq!(
            redact("redirected to http://localhost:2020/auth/callback#code=abc&state=xyz"),
            "redirected to http://localhost:2020/auth/callback"
        );
        assert_eq!(
            cut_url("data:text/plain;base64,c2VjcmV0"),
            "data:text/plain;base64"
        );
    }

    #[test]
    fn the_token_rule_never_matches_url_paths() {
        assert_eq!(
            redact("see http://localhost:2020/assets/index.aB3dE5fG7hI9jK1lM3nO5pQ7.js"),
            "see http://localhost:2020/assets/index.aB3dE5fG7hI9jK1lM3nO5pQ7.js"
        );
    }

    #[test]
    fn redacts_a_password_property_in_an_object_preview() {
        let arg: RemoteObject = parse(json!({
            "type": "object",
            "className": "Object",
            "description": "Object",
            "preview": {
                "type": "object",
                "description": "Object",
                "overflow": false,
                "properties": [
                    { "name": "username", "type": "string", "value": "bob" },
                    { "name": "password", "type": "string", "value": "hunter2" },
                    { "name": "attempts", "type": "number", "value": "3" },
                ],
            },
        }));
        assert_eq!(
            render_args(&[arg]),
            "{username: 'bob', password: [redacted], attempts: 3}"
        );
    }

    #[test]
    fn renders_primitives_and_falls_back_to_the_description() {
        let args: Vec<RemoteObject> = parse(json!([
            { "type": "string", "value": "count" },
            { "type": "number", "value": 3, "description": "3" },
            { "type": "boolean", "value": true },
            { "type": "undefined" },
            { "type": "object", "subtype": "null", "value": null },
            { "type": "function", "description": "() => {}" },
            { "type": "object", "subtype": "array", "preview": {
                "subtype": "array", "overflow": true,
                "properties": [{ "name": "0", "type": "number", "value": "1" }] } },
        ]));
        assert_eq!(
            render_args(&args),
            "count 3 true undefined null () => {} [1, …]"
        );
    }

    #[test]
    fn readable_values_survive() {
        for text in [
            "errorCode: interaction_in_progress",
            "uninitialized_public_client_application",
            "request 3f2504e0-4f89-11d3-9a0c-0305e82c3301 failed",
            "statusCode=500",
            "isAuthenticated=false",
            "authority: https://login.example.com/common",
            "sessionId=42",
            "error_code: 7",
            "at AuthenticationProvider (http://localhost:2020/src/components/AuthenticationProvider.tsx:12:5)",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn a_vite_dependency_frame_keeps_its_location() {
        assert_eq!(
            redact(
                "at acquireToken (http://localhost:2020/node_modules/.vite/deps/@azure_msal-browser.js?v=4f9c2a1b:5123:17)"
            ),
            "at acquireToken (http://localhost:2020/node_modules/.vite/deps/@azure_msal-browser.js:5123:17)"
        );
    }

    #[test]
    fn truncates_after_redacting() {
        let long = format!("password=x, {}", "word ".repeat(200));
        let cleaned = clean(&long);
        assert!(cleaned.starts_with("password=[redacted]"));
        assert_eq!(cleaned.chars().count(), 500);
        assert!(cleaned.ends_with('…'));
    }

    fn console_entry(text: &str) -> ConsoleEntry {
        ConsoleEntry {
            level: Level::Error,
            source: "console".into(),
            text: text.into(),
            location: None,
            timestamp: 0.0,
            load: None,
        }
    }

    #[test]
    fn an_entry_containing_a_short_password_is_withheld() {
        let entry = console_entry("signing in with ab12 for bob");
        assert!(withheld("ab12", &entry));
        assert!(!withheld("zz99", &entry));
    }

    #[test]
    fn an_empty_password_withholds_nothing() {
        let entry = console_entry("anything at all");
        assert!(!withheld("", &entry));
        assert!(!withheld("   ", &entry));
    }

    #[test]
    fn requests_are_withheld_too() {
        let mut log = RequestLog::default();
        log.will_be_sent(
            sent("1", "L1", "MAIN", "http://a.test/u/ab12", None),
            Instant::now(),
        );
        let request = &log.entries(Instant::now(), false)[0];
        assert!(withheld("ab12", request));
    }

    fn sent(
        id: &str,
        loader: &str,
        frame: &str,
        url: &str,
        redirect: Option<u16>,
    ) -> RequestWillBeSent {
        let mut event = json!({
            "requestId": id,
            "loaderId": loader,
            "frameId": frame,
            "documentURL": url,
            "request": { "url": url, "method": "GET", "headers": { "Authorization": "Bearer x" } },
            "timestamp": 1.0,
            "wallTime": 1.0,
            "type": "Document",
        });
        if let Some(status) = redirect {
            event["redirectResponse"] = json!({ "status": status, "url": "x", "headers": {} });
        }
        parse(event)
    }

    fn typed(id: &str, kind: &str) -> RequestWillBeSent {
        parse(json!({
            "requestId": id, "loaderId": "L1", "frameId": "MAIN",
            "request": { "url": format!("http://a.test/{id}"), "method": "GET" },
            "timestamp": 1.0, "type": kind,
        }))
    }

    fn finished(id: &str, status: Option<u16>, log: &mut RequestLog) {
        if let Some(status) = status {
            log.response_received(parse(json!({
                "requestId": id, "timestamp": 1.2, "type": "Fetch",
                "response": { "status": status, "url": "x", "headers": {} },
            })));
        }
        log.loading_finished(parse(json!({ "requestId": id, "timestamp": 1.5 })));
    }

    fn failed(id: &str, log: &mut RequestLog) {
        log.loading_failed(parse(json!({
            "requestId": id, "timestamp": 2.0, "type": "Fetch",
            "errorText": "net::ERR_CONNECTION_REFUSED",
        })));
    }

    fn main_log() -> RequestLog {
        RequestLog {
            main_frame: Some("MAIN".into()),
            ..RequestLog::default()
        }
    }

    #[test]
    fn records_every_redirect_hop() {
        let now = Instant::now();
        let mut log = main_log();
        log.will_be_sent(
            sent("1", "L1", "MAIN", "http://localhost:2020/login?x=1", None),
            now,
        );
        log.will_be_sent(
            sent(
                "1",
                "L1",
                "MAIN",
                "https://login.example/authorize?client_id=c",
                Some(302),
            ),
            now,
        );
        log.will_be_sent(
            sent(
                "1",
                "L1",
                "MAIN",
                "http://localhost:2020/auth/callback#code=abc",
                Some(302),
            ),
            now,
        );
        finished("1", Some(200), &mut log);
        let entries = log.entries(now, false);
        let hops: Vec<(&str, &Outcome)> = entries
            .iter()
            .map(|r| (r.url.as_str(), &r.outcome))
            .collect();
        assert_eq!(
            hops,
            [
                (
                    "http://localhost:2020/login",
                    &Outcome::Redirected { status: 302 }
                ),
                (
                    "https://login.example/authorize",
                    &Outcome::Redirected { status: 302 }
                ),
                (
                    "http://localhost:2020/auth/callback",
                    &Outcome::Finished { status: Some(200) }
                ),
            ]
        );
    }

    #[test]
    fn a_failure_with_no_response_is_recorded() {
        let now = Instant::now();
        let mut log = main_log();
        log.will_be_sent(typed("1", "Fetch"), now);
        failed("1", &mut log);
        let entries = log.entries(now, true);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].outcome,
            Outcome::Failed {
                error: "net::ERR_CONNECTION_REFUSED".into(),
                canceled: false,
                blocked: None,
                status: None,
            }
        );
        assert_eq!(entries[0].duration_ms, Some(1000));
    }

    #[test]
    fn an_error_status_counts_as_failed() {
        let now = Instant::now();
        let mut log = main_log();
        log.will_be_sent(typed("1", "Fetch"), now);
        log.will_be_sent(typed("2", "Fetch"), now);
        finished("1", Some(404), &mut log);
        finished("2", Some(200), &mut log);
        let failed: Vec<String> = log.entries(now, true).into_iter().map(|r| r.url).collect();
        assert_eq!(failed, ["http://a.test/1"]);
        assert_eq!(log.entries(now, false).len(), 2);
    }

    #[test]
    fn an_old_open_request_is_pending_unless_it_is_a_stream() {
        let now = Instant::now();
        let mut log = main_log();
        log.will_be_sent(typed("1", "Fetch"), now);
        log.will_be_sent(typed("2", "EventSource"), now);
        assert_eq!(
            log.entries(now + Duration::from_secs(1), false)[0].outcome,
            Outcome::Open
        );
        let later = log.entries(now + Duration::from_secs(6), false);
        assert_eq!(later[0].outcome, Outcome::Pending);
        assert_eq!(later[0].duration_ms, Some(6000));
        assert_eq!(later[1].outcome, Outcome::Open);
        let failed_only = log.entries(now + Duration::from_secs(6), true);
        assert_eq!(failed_only.len(), 1);
        assert_eq!(failed_only[0].url, "http://a.test/1");
    }

    #[test]
    fn a_request_failing_after_300_others_lands_in_the_failed_pool() {
        let now = Instant::now();
        let mut log = main_log();
        log.will_be_sent(typed("slow", "Fetch"), now);
        for i in 0..300 {
            let id = format!("r{i}");
            log.will_be_sent(typed(&id, "Script"), now);
            finished(&id, Some(200), &mut log);
        }
        failed("slow", &mut log);
        let failed: Vec<String> = log.entries(now, true).into_iter().map(|r| r.url).collect();
        assert_eq!(failed, ["http://a.test/slow"]);
    }

    #[test]
    fn failures_survive_300_ordinary_requests() {
        let now = Instant::now();
        let mut log = main_log();
        log.will_be_sent(typed("bad", "Fetch"), now);
        finished("bad", Some(500), &mut log);
        for i in 0..300 {
            let id = format!("r{i}");
            log.will_be_sent(typed(&id, "Script"), now);
            finished(&id, Some(200), &mut log);
        }
        let all = log.entries(now, false);
        assert_eq!(all.len(), 201);
        assert_eq!(all[0].url, "http://a.test/bad");
        assert_eq!(all[1].url, "http://a.test/r100");
    }

    #[test]
    fn a_full_open_map_abandons_its_oldest_request() {
        let now = Instant::now();
        let mut log = main_log();
        for i in 0..=100 {
            log.will_be_sent(typed(&format!("r{i}"), "Fetch"), now);
        }
        let failed = log.entries(now, true);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].url, "http://a.test/r0");
        assert_eq!(failed[0].outcome, Outcome::Abandoned);
        assert_eq!(log.entries(now, false).len(), 101);
    }

    #[test]
    fn a_redirected_document_belongs_to_its_own_load() {
        let now = Instant::now();
        let mut log = main_log();
        log.will_be_sent(sent("1", "L1", "MAIN", "http://localhost:2020/", None), now);
        finished("1", Some(200), &mut log);
        log.will_be_sent(
            sent("2", "L2", "MAIN", "http://localhost:2020/login", None),
            now,
        );
        log.will_be_sent(
            sent("3", "L1", "MAIN", "http://localhost:2020/late.js", None),
            now,
        );
        log.will_be_sent(
            sent(
                "2",
                "L2",
                "MAIN",
                "https://login.example/authorize",
                Some(302),
            ),
            now,
        );
        let loads: Vec<(String, Option<u32>)> = log
            .entries(now, false)
            .into_iter()
            .map(|r| (r.url, r.load))
            .collect();
        assert_eq!(
            loads,
            [
                ("http://localhost:2020/".to_string(), Some(1)),
                ("http://localhost:2020/login".to_string(), Some(2)),
                ("http://localhost:2020/late.js".to_string(), Some(1)),
                ("https://login.example/authorize".to_string(), Some(2)),
            ]
        );
    }

    #[test]
    fn an_iframe_loader_starts_no_load() {
        let now = Instant::now();
        let mut diagnostics = Diagnostics::default();
        diagnostics.set_main_frame("MAIN");
        diagnostics
            .requests
            .will_be_sent(sent("1", "L1", "MAIN", "http://localhost:2020/", None), now);
        diagnostics.requests.will_be_sent(
            sent(
                "2",
                "IFRAME",
                "FRAME2",
                "https://login.example/silent-renew",
                None,
            ),
            now,
        );
        assert_eq!(diagnostics.requests.current_load(), Some(1));
        diagnostics.console_api_called(parse(json!({
            "type": "error",
            "args": [{ "type": "string", "value": "renew failed" }],
            "timestamp": 1.0,
        })));
        assert_eq!(diagnostics.console.entries().last().unwrap().load, Some(1));
    }

    #[test]
    fn console_entries_are_redacted_with_their_location() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.console_api_called(parse(json!({
            "type": "warning",
            "args": [{ "type": "string", "value": "token=abc" }],
            "timestamp": 5.0,
            "stackTrace": { "callFrames": [{
                "functionName": "f", "scriptId": "1",
                "url": "http://localhost:2020/src/a.ts?t=1", "lineNumber": 9, "columnNumber": 4 }] },
        })));
        diagnostics.exception_thrown(parse(json!({
            "timestamp": 6.0,
            "exceptionDetails": {
                "exceptionId": 1, "text": "Uncaught", "lineNumber": 0, "columnNumber": 0,
                "exception": { "type": "object", "subtype": "error",
                    "description": "Error: boom\n    at http://localhost:2020/src/b.ts?t=2:3:7" },
            },
        })));
        diagnostics.log_entry_added(parse(json!({ "entry": {
            "source": "network", "level": "error", "timestamp": 7.0,
            "text": "Failed to load resource: the server responded with a status of 404",
            "url": "http://localhost:2020/missing?code=abc",
        }})));
        let entries: Vec<&ConsoleEntry> = diagnostics.console.entries().collect();
        assert_eq!(entries[0].level, Level::Warning);
        assert_eq!(entries[0].text, "token=[redacted]");
        assert_eq!(
            entries[0].location.as_deref(),
            Some("http://localhost:2020/src/a.ts:10:5")
        );
        assert_eq!(entries[1].source, "exception");
        assert_eq!(
            entries[1].text,
            "Error: boom\n    at http://localhost:2020/src/b.ts:3:7"
        );
        assert_eq!(entries[2].source, "network");
        assert_eq!(
            entries[2].location.as_deref(),
            Some("http://localhost:2020/missing")
        );
    }

    #[test]
    fn the_console_keeps_the_last_200_entries() {
        let mut diagnostics = Diagnostics::default();
        for i in 0..250 {
            diagnostics.console_api_called(parse(json!({
                "type": "log", "args": [{ "type": "string", "value": format!("m{i}") }],
            })));
        }
        let entries: Vec<&ConsoleEntry> = diagnostics.console.entries().collect();
        assert_eq!(entries.len(), 200);
        assert_eq!(entries[0].text, "m50");
    }
}
