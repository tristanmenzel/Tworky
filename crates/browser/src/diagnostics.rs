//! What a tab's page logs and requests, kept for agents to read. Every string is redacted as it
//! is captured, and raw CDP payloads are never kept.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use regex::{Captures, Regex};
use serde::de::{self, IgnoredAny, Visitor};
use serde::{Deserialize, Deserializer};
use url::Url;

pub const REDACTED: &str = "[redacted]";
pub const WITHHELD: &str = "entry withheld: it contained the account's password";
pub const PENDING_AFTER: Duration = Duration::from_secs(5);

const MAX_TEXT: usize = 500;
/// The fewest characters of a password beside a `[redacted]` or `…` that withhold an entry.
const SHORTEST_PART: usize = 2;
/// Bounds the work redaction does on the UI thread for a huge message.
const MAX_INPUT: usize = 16_000;
const CONSOLE_CAP: usize = 200;
const CONSOLE_ERROR_CAP: usize = 50;
const OPEN_CAP: usize = 100;
const ORDINARY_CAP: usize = 200;
const FAILED_CAP: usize = 50;

const SENSITIVE_NAMES: [&str; 2] = ["pwd", "authorization"];
const SENSITIVE_SUFFIXES: [&str; 8] = [
    "token", "secret", "password", "passwd", "apikey", "api_key", "api-key", "cookie",
];
const EMPTY_VALUES: [&str; 9] = [
    "null",
    "undefined",
    "true",
    "false",
    "''",
    "\"\"",
    "``",
    r#"\"\""#,
    r"\'\'",
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
        r"{}|[A-Za-z0-9_\-]*(?:{})",
        SENSITIVE_NAMES.join("|"),
        SENSITIVE_SUFFIXES.join("|")
    );
    regex(&format!(
        r#"(?i)(^|[^A-Za-z0-9_$\-])(\\?["']?)({names})(\\?["']?)([ \t]*[:=][ \t]*)(\\"(?:[^\\\r\n]|\\\\\\"|\\[^"\r\n])*(?:\\")?|"(?:[^"\\\r\n]|\\.)*"?|'(?:[^'\\\r\n]|\\.)*'?|`[^`\r\n]*`?|[^,}};&\s][^,}};&\r\n]*)"#
    ))
});
static AUTH_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\b(Bearer|Basic)([ \t]+)([A-Za-z0-9+/=._~\-]+)"));
static RUN: LazyLock<Regex> = LazyLock::new(|| regex(r"[A-Za-z0-9+_\-/]+=*"));
static PATH_WORD: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:[A-Z]?[a-z0-9_\-]+)+$"));

/// The set `Url` percent-encodes in a path.
const PATH_ENCODED: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

pub fn is_sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SENSITIVE_NAMES.contains(&name.as_str())
        || SENSITIVE_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Origin and path only: no userinfo, query or fragment.
pub fn cut_url(raw: &str) -> String {
    if raw
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
    {
        return raw.split(',').next().unwrap_or(raw).to_string();
    }
    if let Ok(url) = Url::parse(raw)
        && let Some(host) = url.host_str()
    {
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        return format!("{}://{host}{port}{}", url.scheme(), url.path());
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

fn mixed_case(run: &str) -> bool {
    run.len() >= 24
        && run.chars().any(|c| c.is_ascii_uppercase())
        && run.chars().any(|c| c.is_ascii_lowercase())
}

fn token_shaped(run: &str) -> bool {
    let hex = run.len() >= 32 && run.chars().all(|c| c.is_ascii_hexdigit());
    hex || (mixed_case(run) && run.chars().any(|c| c.is_ascii_digit()))
}

/// A run with a `/` is a path when one of its segments reads as a word of 3 or more
/// characters, and then only its token-shaped segments go. Otherwise it is tested whole as
/// standard base64, where the `/` stands in for a digit.
fn redact_run(run: &str) -> String {
    if !run.contains('/') {
        return if token_shaped(run) {
            REDACTED.to_string()
        } else {
            run.to_string()
        };
    }
    let path = run
        .split('/')
        .any(|segment| segment.len() >= 3 && PATH_WORD.is_match(segment));
    let whole = run.replace('/', "");
    if !path && (token_shaped(&whole) || mixed_case(&whole)) {
        return REDACTED.to_string();
    }
    run.split('/')
        .map(|segment| {
            if token_shaped(segment) {
                REDACTED
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn redact_value(value: &str) -> String {
    if EMPTY_VALUES.contains(&value.trim_end()) {
        return value.to_string();
    }
    if value.starts_with("\\\"") {
        return format!("\\\"{REDACTED}\\\"");
    }
    match value.chars().next() {
        Some(quote @ ('"' | '\'' | '`')) => format!("{quote}{REDACTED}{quote}"),
        _ => REDACTED.to_string(),
    }
}

fn redact_prose(text: &str) -> String {
    let text = JWT.replace_all(text, REDACTED);
    let text = NAME_VALUE.replace_all(&text, |c: &Captures| {
        let value = redact_value(&c[6]);
        format!("{}{}{}{}{}{value}", &c[1], &c[2], &c[3], &c[4], &c[5])
    });
    let text = AUTH_SCHEME.replace_all(&text, |c: &Captures| {
        let value = &c[3];
        if value.len() >= 8 && !value.chars().all(|c| c.is_ascii_lowercase()) {
            format!("{}{}{REDACTED}", &c[1], &c[2])
        } else {
            c[0].to_string()
        }
    });
    RUN.replace_all(&text, |c: &Captures| redact_run(&c[0]))
        .into_owned()
}

/// URLs are cut to origin and path, and the token-shaped rule never looks inside them.
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

fn truncate(text: &mut String, max: usize) {
    if text.chars().nth(max).is_some() {
        let keep = text.char_indices().nth(max - 1).map_or(0, |(at, _)| at);
        text.truncate(keep);
        text.push('…');
    }
}

/// Redacts the first `MAX_INPUT` characters, and says whether there were more.
pub fn clean(text: &str) -> (String, bool) {
    match text.char_indices().nth(MAX_INPUT) {
        Some((at, _)) => (redact(&text[..at]), true),
        None => (redact(text), false),
    }
}

pub trait Entry: Clone {
    fn texts(&self) -> Vec<&str>;
    /// Whether any text was cut at `MAX_INPUT`.
    fn cut(&self) -> bool;
    fn truncate_texts(&mut self, max: usize);
}

fn json_escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted[1..quoted.len() - 1].to_string()
}

/// Whether a password prefix of `shortest` characters or more ends right before `marker`, or a
/// suffix starts right after it: redaction or the browser shortened the text and left part of
/// the password showing.
fn beside_marker(text: &str, marker: &str, form: &str, shortest: usize) -> bool {
    text.match_indices(marker).any(|(at, _)| {
        let before = &text[..at];
        let after = &text[at + marker.len()..];
        form.char_indices().skip(1).any(|(cut, _)| {
            let (prefix, suffix) = form.split_at(cut);
            (prefix.chars().count() >= shortest && before.ends_with(prefix))
                || (suffix.chars().count() >= shortest && after.starts_with(suffix))
        })
    })
}

/// Whether an entry must be withheld because it contains the account's password, raw, JSON
/// escaped or percent-encoded, or part of it beside a `[redacted]` or `…`. An entry cut at the
/// input cap is also withheld when it ends partway into the password.
pub fn withheld(password: &str, entry: &impl Entry) -> bool {
    if password.trim().is_empty() {
        return false;
    }
    let escaped = json_escaped(password);
    let encoded = utf8_percent_encode(password, PATH_ENCODED).to_string();
    let forms = [password, escaped.as_str(), encoded.as_str()];
    let shortest = password.chars().count().min(SHORTEST_PART);
    let texts = entry.texts();
    if texts.iter().any(|text| {
        forms.iter().any(|form| {
            text.contains(form)
                || [REDACTED, "…"]
                    .iter()
                    .any(|marker| beside_marker(text, marker, form, shortest))
        })
    }) {
        return true;
    }
    entry.cut()
        && texts.iter().any(|text| {
            forms.iter().any(|form| {
                form.char_indices()
                    .skip(1)
                    .any(|(at, _)| text.ends_with(&form[..at]))
            })
        })
}

/// What a reader may show of an entry: `None` when it is withheld, else the entry with each
/// text truncated to 500 characters.
pub fn readable<T: Entry>(password: &str, entry: &T) -> Option<T> {
    if withheld(password, entry) {
        return None;
    }
    let mut entry = entry.clone();
    entry.truncate_texts(MAX_TEXT);
    Some(entry)
}

/// Keeps one character more than [`MAX_INPUT`], so [`clean`] can tell the text was cut.
struct Capped(String);

impl<'de> Deserialize<'de> for Capped {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl Visitor<'_> for Visit {
            type Value = Capped;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Capped, E> {
                let end = text
                    .char_indices()
                    .nth(MAX_INPUT + 1)
                    .map_or(text.len(), |(at, _)| at);
                Ok(Capped(text[..end].to_string()))
            }
        }
        deserializer.deserialize_str(Visit)
    }
}

fn capped<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Capped::deserialize(deserializer).map(|capped| capped.0)
}

fn capped_option<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<Capped>::deserialize(deserializer).map(|capped| capped.map(|capped| capped.0))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Primitive {
    Bool(bool),
    Number(f64),
    Text(Capped),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObject {
    #[serde(rename = "type", default)]
    kind: String,
    subtype: Option<String>,
    value: Option<Primitive>,
    #[serde(default, deserialize_with = "capped_option")]
    unserializable_value: Option<String>,
    #[serde(default, deserialize_with = "capped_option")]
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
    #[serde(default, deserialize_with = "capped_option")]
    value: Option<String>,
}

fn empty_value(property: &PropertyPreview) -> bool {
    matches!(
        (property.kind.as_str(), property.value.as_deref()),
        ("undefined" | "boolean", _) | ("object", Some("null")) | ("string", Some(""))
    )
}

fn render_property(property: &PropertyPreview) -> String {
    if is_sensitive_name(&property.name) && !empty_value(property) {
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
        Some(Primitive::Text(text)) => return text.0.clone(),
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

/// Console arguments as one line, before redaction, stopping once it passes the input cap.
pub fn render_args(args: &[RemoteObject]) -> String {
    let mut line = String::new();
    let mut length = 0;
    for arg in args {
        if length > MAX_INPUT {
            break;
        }
        if !line.is_empty() {
            line.push(' ');
            length += 1;
        }
        let text = render_arg(arg);
        length += text.chars().count();
        line.push_str(&text);
    }
    line
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
    #[serde(default, deserialize_with = "capped")]
    url: String,
    #[serde(default)]
    line_number: i64,
    #[serde(default)]
    column_number: i64,
}

fn location(url: &str, line: Option<i64>, column: Option<i64>) -> Option<(String, bool)> {
    if url.is_empty() {
        return None;
    }
    let mut at = cut_url(url);
    for number in [line, column].into_iter().map_while(|n| n) {
        at.push_str(&format!(":{}", number.max(0) + 1));
    }
    Some(clean(&at))
}

fn frame_location(stack: Option<&StackTrace>) -> Option<(String, bool)> {
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
    #[serde(default, deserialize_with = "capped")]
    text: String,
    #[serde(default, deserialize_with = "capped_option")]
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
    #[serde(default, deserialize_with = "capped")]
    text: String,
    #[serde(default)]
    timestamp: f64,
    #[serde(default, deserialize_with = "capped_option")]
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
    /// Kept whole; [`readable`] truncates it.
    pub text: String,
    /// `url:line:col`.
    pub location: Option<String>,
    /// Milliseconds since the epoch.
    pub timestamp: f64,
    pub load: Option<u32>,
    cut: bool,
}

impl Entry for ConsoleEntry {
    fn texts(&self) -> Vec<&str> {
        let mut texts = vec![self.text.as_str(), self.source.as_str()];
        texts.extend(self.location.as_deref());
        texts
    }

    fn cut(&self) -> bool {
        self.cut
    }

    fn truncate_texts(&mut self, max: usize) {
        truncate(&mut self.text, max);
        truncate(&mut self.source, max);
        if let Some(location) = &mut self.location {
            truncate(location, max);
        }
    }
}

/// Errors are kept apart from other entries, so verbose logging can't push one out.
#[derive(Default)]
pub struct ConsoleLog {
    errors: VecDeque<(u64, ConsoleEntry)>,
    others: VecDeque<(u64, ConsoleEntry)>,
    next_seq: u64,
}

impl ConsoleLog {
    fn push(&mut self, entry: ConsoleEntry) {
        let (pool, cap) = if entry.level == Level::Error {
            (&mut self.errors, CONSOLE_ERROR_CAP)
        } else {
            (&mut self.others, CONSOLE_CAP)
        };
        if pool.len() == cap {
            pool.pop_front();
        }
        pool.push_back((self.next_seq, entry));
        self.next_seq += 1;
    }

    /// Oldest first.
    pub fn entries(&self) -> impl Iterator<Item = &ConsoleEntry> {
        let mut entries: Vec<&(u64, ConsoleEntry)> =
            self.errors.iter().chain(&self.others).collect();
        entries.sort_by_key(|(seq, _)| *seq);
        entries.into_iter().map(|(_, entry)| entry)
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
    #[serde(deserialize_with = "capped")]
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
    #[serde(default, deserialize_with = "capped")]
    error_text: String,
    #[serde(default)]
    canceled: bool,
    #[serde(default, deserialize_with = "capped_option")]
    blocked_reason: Option<String>,
    cors_error_status: Option<CorsErrorStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CorsErrorStatus {
    #[serde(deserialize_with = "capped")]
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
    /// Origin and path, kept whole; [`readable`] truncates it.
    pub url: String,
    pub resource_type: String,
    pub outcome: Outcome,
    /// For an open request, how long it has been open.
    pub duration_ms: Option<u64>,
    pub load: Option<u32>,
    seq: u64,
    cut: bool,
}

impl Entry for Request {
    fn texts(&self) -> Vec<&str> {
        let mut texts = vec![
            self.method.as_str(),
            self.url.as_str(),
            self.resource_type.as_str(),
        ];
        if let Outcome::Failed { error, blocked, .. } = &self.outcome {
            texts.push(error);
            texts.extend(blocked.as_deref());
        }
        texts
    }

    fn cut(&self) -> bool {
        self.cut
    }

    fn truncate_texts(&mut self, max: usize) {
        truncate(&mut self.method, max);
        truncate(&mut self.url, max);
        truncate(&mut self.resource_type, max);
        if let Outcome::Failed { error, blocked, .. } = &mut self.outcome {
            truncate(error, max);
            if let Some(blocked) = blocked {
                truncate(blocked, max);
            }
        }
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
    /// Main-frame `loaderId`s, numbered from 1 in the order first seen.
    loads: HashMap<String, u32>,
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
        if let Some(load) = self.loads.get(loader) {
            return Some(*load);
        }
        if !loader.is_empty() && frame.is_some() && frame == self.main_frame.as_deref() {
            let load = self.loads.len() as u32 + 1;
            self.loads.insert(loader.to_string(), load);
            return Some(load);
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

    fn close(
        &mut self,
        id: &str,
        timestamp: f64,
        cut: bool,
        outcome: impl FnOnce(Option<u16>) -> Outcome,
    ) {
        let Some(open) = self.open.remove(id) else {
            return;
        };
        let mut request = open.request;
        request.cut |= cut;
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
            self.close(&event.request_id, event.timestamp, false, |_| {
                Outcome::Redirected { status }
            });
        }
        let load = self.load_of(&event.loader_id, event.frame_id.as_deref());
        if !self.open.contains_key(&event.request_id) && self.open.len() >= OPEN_CAP {
            self.abandon_oldest();
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let (url, cut) = clean(&cut_url(&event.request.url));
        let request = Request {
            method: event.request.method,
            url,
            resource_type: event.resource_type.unwrap_or_else(|| "Other".into()),
            outcome: Outcome::Open,
            duration_ms: None,
            load,
            seq,
            cut,
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
        self.close(&event.request_id, event.timestamp, false, |status| {
            Outcome::Finished { status }
        });
    }

    pub fn loading_failed(&mut self, event: LoadingFailed) {
        let blocked = event.blocked_reason.or_else(|| {
            event
                .cors_error_status
                .map(|cors| format!("cors: {}", cors.cors_error))
        });
        let (error, error_cut) = clean(&event.error_text);
        let blocked = blocked.map(|b| clean(&b));
        let cut = error_cut || blocked.as_ref().is_some_and(|(_, cut)| *cut);
        let blocked = blocked.map(|(b, _)| b);
        let canceled = event.canceled;
        self.close(&event.request_id, event.timestamp, cut, |status| {
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
        location: Option<(String, bool)>,
        timestamp: f64,
    ) {
        let (text, text_cut) = clean(text);
        let location_cut = location.as_ref().is_some_and(|(_, cut)| *cut);
        let entry = ConsoleEntry {
            level,
            source: source.to_string(),
            text,
            location: location.map(|(at, _)| at),
            timestamp,
            load: self.requests.current_load(),
            cut: text_cut || location_cut,
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
        let shown = readable("hunter2", &console_entry(&clean(&long).0)).unwrap();
        assert!(shown.text.starts_with("password=[redacted]"));
        assert_eq!(shown.text.chars().count(), 500);
        assert!(shown.text.ends_with('…'));
    }

    fn console_entry(text: &str) -> ConsoleEntry {
        ConsoleEntry {
            level: Level::Error,
            source: "console".into(),
            text: text.into(),
            location: None,
            timestamp: 0.0,
            load: None,
            cut: false,
        }
    }

    fn logged(diagnostics: &mut Diagnostics, text: &str) -> ConsoleEntry {
        diagnostics.console_api_called(parse(json!({
            "type": "log", "args": [{ "type": "string", "value": text }],
        })));
        diagnostics.console.entries().last().unwrap().clone()
    }

    #[test]
    fn a_password_straddling_the_display_cut_is_withheld() {
        let mut diagnostics = Diagnostics::default();
        let entry = logged(
            &mut diagnostics,
            &format!("{}hunter2pass", "x ".repeat(247)),
        );
        assert_eq!(readable("hunter2pass", &entry), None);
        assert!(readable("other-pass", &entry).is_some());
    }

    #[test]
    fn an_entry_cut_at_the_input_cap_ending_in_a_password_prefix_is_withheld() {
        let mut diagnostics = Diagnostics::default();
        let filler = format!("{} ", "x".repeat(MAX_INPUT - 4));
        let entry = logged(&mut diagnostics, &format!("{filler}hunter2pass and more"));
        assert!(entry.text.ends_with("hun"));
        assert_eq!(readable("hunter2pass", &entry), None);
        assert!(readable("zz", &entry).is_some());

        let whole = logged(&mut diagnostics, &format!("{}hun", "x ".repeat(10)));
        assert!(readable("hunter2pass", &whole).is_some());
    }

    #[test]
    fn readable_truncates_to_500_characters() {
        let entry = console_entry(&"word ".repeat(200));
        let shown = readable("hunter2", &entry).unwrap();
        assert_eq!(shown.text.chars().count(), 500);
        assert_eq!(entry.text.chars().count(), 1000);
    }

    #[test]
    fn redacts_double_stringified_json() {
        assert_eq!(
            redact(r#"{\"password\":\"hunter2\",\"user\":\"bob\"}"#),
            r#"{\"password\":\"[redacted]\",\"user\":\"bob\"}"#
        );
        assert_eq!(
            redact(r#"body: "{\"accessToken\":\"a \\\" b\"}""#),
            r#"body: "{\"accessToken\":\"[redacted]\"}""#
        );
    }

    #[test]
    fn the_json_escaped_password_is_withheld() {
        let entry = console_entry(r#"{"user":"bob","pw":"a\"b\\c"}"#);
        assert!(withheld(r#"a"b\c"#, &entry));
    }

    #[test]
    fn standard_base64_with_slashes_is_redacted() {
        assert_eq!(
            redact("AccountKey=Ab3+xYz/9QwErTy12UiOpAsDfGhJkLzXcVbNm0987654321abcdEFGH==;"),
            "AccountKey=[redacted];"
        );
        assert_eq!(
            redact("key wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY set"),
            "key [redacted] set"
        );
    }

    #[test]
    fn a_path_keeps_its_readable_segments() {
        for text in [
            "src/components/AuthenticationProvider.tsx",
            "at src/components/AuthenticationProvider.tsx:12:5",
            "node_modules/.vite/deps/chunk-ABC123.js",
        ] {
            assert_eq!(redact(text), text);
        }
        assert_eq!(
            redact("GET api/sessions/aB3dE5fG7hI9jK1lM3nO5pQ7xx"),
            "GET api/sessions/[redacted]"
        );
    }

    #[test]
    fn an_id_after_equals_stays_readable() {
        for name in ["sessionId", "correlationId", "clientId"] {
            let text = format!("{name}=3f2504e0-4f89-11d3-9a0c-0305e82c3301");
            assert_eq!(redact(&text), text);
        }
    }

    #[test]
    fn auth_schemes_need_a_capital_and_a_credential() {
        for text in [
            "This is a basic example",
            "send a bearer token with it",
            "Basic authentication failed",
            "Bearer token missing",
        ] {
            assert_eq!(redact(text), text);
        }
        assert_eq!(redact("Basic dXNlcjpwYXNz"), "Basic [redacted]");
        assert_eq!(
            redact("Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln"),
            "Bearer [redacted]"
        );
        assert_eq!(redact("Bearer 8f14e45fceea167a"), "Bearer [redacted]");
    }

    #[test]
    fn empty_values_stay_readable() {
        for text in [
            "{account: null, accessToken: undefined, idToken: ''}",
            r#"{"refreshToken": "", "password": null}"#,
            "hasSecret: true, cookie: false",
        ] {
            assert_eq!(redact(text), text);
        }
        assert_eq!(
            redact("{accessToken: abc, idToken: ''}"),
            "{accessToken: [redacted], idToken: ''}"
        );
    }

    #[test]
    fn empty_properties_stay_readable_in_an_object_preview() {
        let arg: RemoteObject = parse(json!({
            "type": "object",
            "preview": {
                "type": "object",
                "properties": [
                    { "name": "account", "type": "object", "subtype": "null", "value": "null" },
                    { "name": "accessToken", "type": "undefined" },
                    { "name": "idToken", "type": "string", "value": "" },
                    { "name": "hasPassword", "type": "boolean", "value": "false" },
                    { "name": "refreshToken", "type": "string", "value": "abc" },
                ],
            },
        }));
        assert_eq!(
            render_args(&[arg]),
            "{account: null, accessToken: undefined, idToken: '', hasPassword: false, refreshToken: [redacted]}"
        );
    }

    #[test]
    fn compound_sensitive_names_are_redacted() {
        for (input, expected) in [
            ("newPassword=hunter2", "newPassword=[redacted]"),
            ("confirmPassword: hunter2", "confirmPassword: [redacted]"),
            ("x-api-key: k1", "x-api-key: [redacted]"),
            (
                "set-cookie: sid=1; Path=/",
                "set-cookie: [redacted]; Path=/",
            ),
            ("old_passwd=x", "old_passwd=[redacted]"),
            ("stripeApiKey=k", "stripeApiKey=[redacted]"),
        ] {
            assert_eq!(redact(input), expected, "{input}");
        }
        for name in ["newPassword", "x-api-key", "set-cookie", "STRIPE_API_KEY"] {
            assert!(is_sensitive_name(name), "{name}");
        }
        for name in [
            "errorCode",
            "statusCode",
            "error_code",
            "isAuthenticated",
            "authority",
            "sessionId",
        ] {
            assert!(!is_sensitive_name(name), "{name}");
        }
    }

    #[test]
    fn huge_strings_are_capped_as_they_are_parsed() {
        let huge = "a".repeat(100_000);
        let arg: RemoteObject = parse(json!({ "type": "string", "value": huge }));
        assert_eq!(render_args(&[arg]).chars().count(), MAX_INPUT + 1);
        let arg: RemoteObject = parse(json!({ "type": "function", "description": huge }));
        assert_eq!(render_args(&[arg]).chars().count(), MAX_INPUT + 1);
        let args: Vec<RemoteObject> = parse(json!([
            { "type": "string", "value": huge },
            { "type": "string", "value": huge },
        ]));
        assert_eq!(render_args(&args).chars().count(), MAX_INPUT + 1);
        let short: RemoteObject = parse(json!({ "type": "string", "value": "é".repeat(10) }));
        assert_eq!(render_args(&[short]), "é".repeat(10));
    }

    #[test]
    fn a_data_url_is_cut_before_parsing() {
        let url = format!("data:image/png;base64,{}", "iVBOR".repeat(10_000));
        assert_eq!(cut_url(&url), "data:image/png;base64");
        assert_eq!(cut_url("DATA:text/html,<p>hi</p>"), "DATA:text/html");
        let mut log = main_log();
        log.will_be_sent(typed_url("1", &url), Instant::now());
        assert_eq!(
            log.entries(Instant::now(), false)[0].url,
            "data:image/png;base64"
        );
    }

    #[test]
    fn a_load_keeps_its_number() {
        let now = Instant::now();
        let mut log = main_log();
        for loader in ["L1", "L2", "L3"] {
            log.will_be_sent(sent(loader, loader, "MAIN", "http://a.test/", None), now);
        }
        log.will_be_sent(sent("4", "L2", "MAIN", "http://a.test/x.js", None), now);
        assert_eq!(log.current_load(), Some(3));
        let loads: Vec<Option<u32>> = log.entries(now, false).iter().map(|r| r.load).collect();
        assert_eq!(loads, [Some(1), Some(2), Some(3), Some(2)]);
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

    fn typed_url(id: &str, url: &str) -> RequestWillBeSent {
        parse(json!({
            "requestId": id, "loaderId": "L1", "frameId": "MAIN",
            "request": { "url": url, "method": "GET" },
            "timestamp": 1.0, "type": "Image",
        }))
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
    fn a_password_suffix_after_a_redaction_is_withheld() {
        for (password, logged, stored) in [
            ("Tr0ub4dor&3", "pwd=Tr0ub4dor&3", "pwd=[redacted]&3"),
            (
                "correct;horse;battery",
                "password: correct;horse;battery",
                "password: [redacted];horse;battery",
            ),
        ] {
            let entry = console_entry(&clean(logged).0);
            assert_eq!(entry.text, stored);
            assert!(withheld(password, &entry), "{stored}");
        }
    }

    #[test]
    fn a_password_prefix_before_a_redaction_is_withheld() {
        let entry = console_entry(&clean("Summer!aB3dE5fG7hI9jK1lM3nO5pQ7").0);
        assert_eq!(entry.text, "Summer![redacted]");
        assert!(withheld("Summer!aB3dE5fG7hI9jK1lM3nO5pQ7", &entry));
    }

    #[test]
    fn a_password_shortened_by_an_ellipsis_is_withheld() {
        let entry = console_entry("{note: '…xxxxhunt…'}");
        assert!(withheld("hunter2pass", &entry));
        let entry = console_entry("{note: '…ter2pass'}");
        assert!(withheld("hunter2pass", &entry));
        assert!(!withheld("zebra-crossing", &entry));
    }

    #[test]
    fn an_unrelated_redaction_is_not_withheld() {
        for text in ["token=[redacted]", "pwd=[redacted]&x=1", "[1, 2, …]"] {
            assert!(!withheld("Tr0ub4dor&3", &console_entry(text)), "{text}");
        }
    }

    #[test]
    fn a_percent_encoded_password_is_withheld() {
        let mut log = RequestLog::default();
        log.will_be_sent(
            sent("1", "L1", "MAIN", "http://a.test/u/my pass", None),
            Instant::now(),
        );
        let request = &log.entries(Instant::now(), false)[0];
        assert_eq!(request.url, "http://a.test/u/my%20pass");
        assert!(withheld("my pass", request));
    }

    #[test]
    fn a_base64_run_with_a_short_segment_is_redacted() {
        assert_eq!(
            redact("key wJalrXUtnFEMIabc/k/MDENGbPxRfiCYEXAMPLE set"),
            "key [redacted] set"
        );
        for text in [
            "src/components/AuthenticationProvider.tsx",
            "a/b/c",
            "I/O error",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn an_error_survives_300_other_console_entries() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.console_api_called(parse(json!({
            "type": "error", "args": [{ "type": "string", "value": "startup failed" }],
        })));
        for i in 0..300 {
            let kind = if i % 2 == 0 { "info" } else { "warning" };
            diagnostics.console_api_called(parse(json!({
                "type": kind, "args": [{ "type": "string", "value": format!("m{i}") }],
            })));
        }
        let entries: Vec<&ConsoleEntry> = diagnostics.console.entries().collect();
        assert_eq!(entries.len(), 201);
        assert_eq!(entries[0].text, "startup failed");
        assert_eq!(entries[1].text, "m100");
        assert_eq!(entries[200].text, "m299");
    }

    #[test]
    fn errors_keep_the_last_50_in_order_with_other_entries() {
        let mut diagnostics = Diagnostics::default();
        for i in 0..60 {
            diagnostics.console_api_called(parse(json!({
                "type": "error", "args": [{ "type": "string", "value": format!("e{i}") }],
            })));
            diagnostics.console_api_called(parse(json!({
                "type": "log", "args": [{ "type": "string", "value": format!("m{i}") }],
            })));
        }
        let texts: Vec<&str> = diagnostics
            .console
            .entries()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts.len(), 110);
        assert_eq!(&texts[..3], ["m0", "m1", "m2"]);
        assert_eq!(&texts[10..13], ["e10", "m10", "e11"]);
        assert_eq!(texts.last(), Some(&"m59"));
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
