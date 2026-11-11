use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};
use tworky_browser::interact::{self, Button, KeyPress, Target, WaitCondition};
use tworky_browser::snapshot::parse_ref;
use tworky_core::activity::{self, AccountRequest, Handover};
use tworky_core::config::{AccountConfig, Paths, ServiceConfig};
use tworky_core::ports::PortMap;
use tworky_core::worker::{McpEndpoint, Workspace};

use super::{Browser, Tab, host, page};

const DEFAULT_WAIT: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_secs(25);
const DEFAULT_MAX_CHARS: usize = 20_000;

/// Each session's token maps to its worker. Tokens live only in memory: sessions end with the
/// app, and nothing on disk registers the server, so Claudes started elsewhere never see it.
#[derive(Clone, Default)]
struct Tokens(Arc<Mutex<HashMap<String, String>>>);

impl Tokens {
    fn worker(&self, token: &str) -> Option<String> {
        self.0.lock().unwrap().get(token).cloned()
    }
}

pub struct Mcp {
    port: u16,
    tokens: Tokens,
}

impl Mcp {
    pub fn start(app: &AppHandle) -> Result<Mcp, String> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let tokens = Tokens::default();
        tauri::async_runtime::spawn(serve(listener, app.clone(), tokens.clone()));
        Ok(Mcp { port, tokens })
    }

    /// A fresh endpoint for a session about to launch, replacing the worker's earlier tokens.
    pub fn endpoint(&self, worker: &str) -> McpEndpoint {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut tokens = self.tokens.0.lock().unwrap();
        tokens.retain(|_, w| w != worker);
        tokens.insert(token.clone(), worker.to_string());
        McpEndpoint {
            url: format!("http://127.0.0.1:{}/mcp", self.port),
            token,
        }
    }

    pub fn revoke(&self, worker: &str) {
        self.tokens.0.lock().unwrap().retain(|_, w| w != worker);
    }
}

async fn serve(listener: std::net::TcpListener, app: AppHandle, tokens: Tokens) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(e) => return eprintln!("browser MCP server: {e}"),
    };
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = None;
    let handler_tokens = tokens.clone();
    let service = StreamableHttpService::new(
        move || Ok(TworkyMcp::new(app.clone(), handler_tokens.clone())),
        Arc::new(sessions),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(tokens, require_token));
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("browser MCP server: {e}");
    }
}

fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn require_token(
    State(tokens): State<Tokens>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if bearer(request.headers()).is_some_and(|token| tokens.worker(token).is_some()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

enum Reply {
    Json(Value),
    Image { caption: Value, png_base64: String },
    Text { caption: Value, text: String },
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn respond(result: Result<Reply, String>) -> Result<CallToolResult, McpError> {
    Ok(match result {
        Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        Ok(Reply::Json(value)) => CallToolResult::success(vec![ContentBlock::text(pretty(&value))]),
        Ok(Reply::Image {
            caption,
            png_base64,
        }) => CallToolResult::success(vec![
            ContentBlock::text(pretty(&caption)),
            ContentBlock::image(png_base64, "image/png"),
        ]),
        Ok(Reply::Text { caption, text }) => CallToolResult::success(vec![
            ContentBlock::text(pretty(&caption)),
            ContentBlock::text(text),
        ]),
    })
}

#[derive(Clone)]
struct TworkyMcp {
    app: AppHandle,
    tokens: Tokens,
    tool_router: ToolRouter<Self>,
}

impl TworkyMcp {
    fn new(app: AppHandle, tokens: Tokens) -> Self {
        Self {
            app,
            tokens,
            tool_router: Self::tool_router(),
        }
    }

    fn worker(&self, ctx: &RequestContext<RoleServer>) -> Result<String, String> {
        ctx.extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| bearer(&parts.headers))
            .and_then(|token| self.tokens.worker(token))
            .ok_or_else(|| {
                "this session's Tworky token is no longer valid; restart the session from Tworky"
                    .to_string()
            })
    }

    /// One of this session's worker's tabs. Tabs of other workers are reported as missing.
    fn tab(
        &self,
        ctx: &RequestContext<RoleServer>,
        id: u64,
    ) -> Result<(Arc<Tab>, host::TabCdp), String> {
        let worker = self.worker(ctx)?;
        let tab = self
            .app
            .state::<Browser>()
            .tab(id)
            .ok()
            .filter(|tab| tab.worker == worker)
            .ok_or_else(|| format!("tab {id} not found; call list_tabs"))?;
        let cdp = host::TabCdp::new(&self.app, tab.id);
        Ok((tab, cdp))
    }

    async fn act(
        &self,
        ctx: &RequestContext<RoleServer>,
        id: u64,
        action: impl AsyncFnOnce(&host::TabCdp, bool) -> Result<Value, String>,
    ) -> Result<Reply, String> {
        let (tab, cdp) = self.tab(ctx, id)?;
        let page = page(&cdp, &tab);
        page.act(action(&cdp, tab.mobile())).await.map(Reply::Json)
    }
}

#[derive(Deserialize, JsonSchema)]
struct OpenTabArgs {
    /// Account name, as shown by list_accounts. Omit for an anonymous tab, which then needs url.
    account: Option<String>,
    /// Page to open. Defaults to the account's start URL on this worker's ports. Required without account.
    url: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TabArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
}

#[derive(Deserialize, JsonSchema)]
struct NavigateArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Absolute URL to load.
    url: String,
}

#[derive(Deserialize, JsonSchema)]
struct ReadPageArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Maximum number of characters to return. Defaults to 20000.
    max_chars: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ScreenshotArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Capture the whole scrollable page instead of just the visible viewport. Defaults to false.
    full_page: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ClickArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Element ref from snapshot, e.g. "e412". Give either ref, or x and y.
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Viewport x in CSS pixels, the same as screenshot pixels. Use with y instead of ref.
    x: Option<f64>,
    /// Viewport y in CSS pixels, the same as screenshot pixels. Use with x instead of ref.
    y: Option<f64>,
    /// "left" (default), "right" or "middle". Mobile tabs take left only (a tap).
    button: Option<String>,
    /// Double-click. Defaults to false.
    double: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ServiceArgs {
    /// Service name from list_services.
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct DragArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Ref of the element to press on, e.g. "e412". Give either from_ref, or from_x and from_y.
    from_ref: Option<String>,
    /// Viewport x in CSS pixels where the drag starts. Use with from_y instead of from_ref.
    from_x: Option<f64>,
    /// Viewport y in CSS pixels where the drag starts. Use with from_x instead of from_ref.
    from_y: Option<f64>,
    /// Ref of the element to release over. Give either to_ref, or to_x and to_y.
    to_ref: Option<String>,
    /// Viewport x in CSS pixels where the drag ends. Use with to_y instead of to_ref.
    to_x: Option<f64>,
    /// Viewport y in CSS pixels where the drag ends. Use with to_x instead of to_ref.
    to_y: Option<f64>,
    /// Moves between press and release. Defaults to 10; more suit apps that track the path, such as drawing.
    steps: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct HoverArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Element ref from snapshot, e.g. "e412". Give either ref, or x and y.
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Viewport x in CSS pixels. Use with y instead of ref.
    x: Option<f64>,
    /// Viewport y in CSS pixels. Use with x instead of ref.
    y: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
struct TypeArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Text to type.
    text: String,
    /// Field to click first, as a ref from snapshot. Omit to type into the focused element.
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Viewport x of the field to click first. Use with y instead of ref.
    x: Option<f64>,
    /// Viewport y of the field to click first. Use with x instead of ref.
    y: Option<f64>,
    /// Select and delete the field's existing text first. Defaults to false.
    clear: Option<bool>,
    /// Press Enter after typing. Defaults to false.
    submit: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct PressKeyArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Key to press, optionally with modifiers joined by "+": "Enter", "Tab", "Escape", "ArrowDown", "PageDown", "a", "Shift+Tab", "Control+a".
    key: String,
}

#[derive(Deserialize, JsonSchema)]
struct ScrollArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Element ref from snapshot. With no deltas, scrolls it into view; with deltas, scrolls the area under it.
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Viewport x to scroll at. Use with y instead of ref. Defaults to the viewport centre.
    x: Option<f64>,
    /// Viewport y to scroll at. Use with x instead of ref.
    y: Option<f64>,
    /// Pixels to scroll right (negative scrolls left).
    delta_x: Option<f64>,
    /// Pixels to scroll down (negative scrolls up).
    delta_y: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
struct SelectOptionArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Ref of the <select> element (a combobox in the snapshot).
    #[serde(rename = "ref")]
    reference: String,
    /// Options to select, matched by value or by visible label. Give several only for a multi-select.
    values: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
struct HandOverArgs {
    /// One or two sentences: what changed and what the user should look at.
    summary: String,
    /// The checks to make, in order, each one action and its expected result, e.g. "As Tester 1, open Clients: the new Status column shows each client's plan".
    steps: Vec<String>,
    /// Tabs left open on the pages to check, in the order the steps use them.
    tabs: Option<Vec<u64>>,
}

#[derive(Deserialize, JsonSchema)]
struct RequestAccountArgs {
    /// A name for the account, e.g. "Admin 1". It must not be one list_accounts already shows.
    name: String,
    /// The email (username) to suggest for it, e.g. "admin1@example.com".
    email: String,
    /// The service whose page the account starts on, as list_services names it.
    service: String,
    /// One sentence for the user: what the account is for and what role or data it needs, e.g. "An admin with two team members, to check the audit page".
    reason: String,
}

fn account_request(
    args: RequestAccountArgs,
    project: &str,
    services: &[ServiceConfig],
    worker_ports: &PortMap,
    accounts: &[AccountConfig],
) -> Result<AccountRequest, String> {
    let with_port: Vec<_> = services
        .iter()
        .filter(|s| {
            s.port
                .as_ref()
                .is_some_and(|p| worker_ports.contains_key(p))
        })
        .collect();
    if with_port.is_empty() {
        if services.iter().any(|s| s.port.is_some()) {
            return Err(format!(
                "none of project {project}'s services has a port on this worker, whose ports \
                 were fixed when it was created, so a new account has no page to start on here"
            ));
        }
        return Err(format!(
            "project {project} defines no service with a port, so a new account has no page to \
             start on; ask the user to add [[services]] in the project's setup session \
             (Projects › Setup in Tworky)"
        ));
    }
    let (name, email) = (args.name.trim(), args.email.trim());
    if name.is_empty() || email.is_empty() {
        return Err("give the account a name and an email".into());
    }
    if let Some(existing) = accounts.iter().find(|a| a.name.eq_ignore_ascii_case(name)) {
        return Err(format!(
            "account {} already exists; open_tab with it",
            existing.name
        ));
    }
    let service = with_port
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(args.service.trim()))
        .ok_or_else(|| {
            let names: Vec<_> = with_port.iter().map(|s| s.name.as_str()).collect();
            format!(
                "no service with a port on this worker called {}; use one of: {}",
                args.service,
                names.join(", ")
            )
        })?;
    Ok(AccountRequest {
        name: name.to_string(),
        username: email.to_string(),
        service: service.name.clone(),
        reason: args.reason.trim().to_string(),
    })
}

#[derive(Deserialize, JsonSchema)]
struct WaitForArgs {
    /// Tab id returned by open_tab or list_tabs.
    tab: u64,
    /// Wait until the page's visible text contains this.
    text: Option<String>,
    /// Wait until the page's visible text no longer contains this.
    text_gone: Option<String>,
    /// Wait until the element with this ref from snapshot is rendered with a size on screen.
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// How long to wait in milliseconds. Defaults to 5000, at most 25000.
    timeout_ms: Option<u64>,
}

fn wait_condition(args: &WaitForArgs) -> Result<WaitCondition, String> {
    match (&args.text, &args.text_gone, &args.reference) {
        (Some(text), None, None) if !text.is_empty() => Ok(WaitCondition::Text(text.clone())),
        (None, Some(text), None) if !text.is_empty() => Ok(WaitCondition::TextGone(text.clone())),
        (None, None, Some(reference)) => parse_ref(reference)
            .map(WaitCondition::Visible)
            .ok_or_else(|| format!("\"{reference}\" is not a ref like \"e412\"")),
        _ => Err("give exactly one of text, text_gone or ref".into()),
    }
}

#[tool_router]
impl TworkyMcp {
    #[tool(
        description = "List the test accounts of this worker's project: name, username, start URL (on this worker's ports), whether it is a mobile account, and whether it has a stored password. Each account gets its own isolated, persistent browser session for this worker. Call this first to find the account to pass to open_tab."
    )]
    async fn list_accounts(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let worker = self.worker(&ctx)?;
            let accounts = tauri::async_runtime::spawn_blocking(move || {
                let ws = Workspace::open().map_err(|e| format!("{e:#}"))?;
                let record = ws
                    .state
                    .get(&worker)
                    .map_err(|e| format!("{e:#}"))?
                    .ok_or_else(|| format!("worker {worker} not found"))?;
                let accounts = ws.worker_accounts(&record).map_err(|e| format!("{e:#}"))?;
                Ok::<_, String>(
                    accounts
                        .into_iter()
                        .map(|a| {
                            let stored =
                                tworky_core::accounts::has_password(&record.project, &a.name);
                            json!({
                                "name": a.name,
                                "username": a.username,
                                "start_url": a.start_url,
                                "mobile": a.mobile,
                                "has_password": stored,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok(Reply::Json(json!({ "accounts": accounts })))
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Open a new browser tab signed in as one of this worker's accounts, load the page and return its tab id with the page state. The tab appears beside this session's terminal in the user's Tworky window without taking focus, and reuses the account's existing session (cookies, logins) for this worker. Opens the account's start URL unless url is given. Omit account for an anonymous tab, signed in as nobody, on a fresh browser profile that is discarded when the tab closes; it needs url (list_services gives the URLs). If the project has a service serving that URL's port (a dev server) and it isn't running, it is started first and waited for, so don't start it yourself."
    )]
    async fn open_tab(
        &self,
        Parameters(args): Parameters<OpenTabArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let worker = self.worker(&ctx)?;
            super::check_open(args.account.as_deref(), args.url.as_deref())?;
            let tab = super::open_tab(&self.app, &worker, args.account.as_deref()).await?;
            let cdp = host::TabCdp::new(&self.app, tab.id);
            let target = super::target_url(&tab, args.url);
            let (services, service_error) =
                match super::start_service_for(&self.app, &tab, &target).await {
                    Ok(services) => (services, None),
                    Err(e) => {
                        if super::pause_if_stopped(&self.app, &tab, &target).await {
                            return Err(format!(
                                "tab {} is open, but its page's service did not start: {e}",
                                tab.id
                            ));
                        }
                        super::set_status(&self.app, &tab, None);
                        (Vec::new(), Some(e))
                    }
                };
            let mut state = page(&cdp, &tab).navigate(&target).await?;
            state["tab"] = tab.id.into();
            state["account"] = tab.account().map(|a| a.name).into();
            if let Some(e) = service_error {
                state["service_error"] = e.into();
            }
            if !services.is_empty() {
                state["services"] = json!(
                    services
                        .into_iter()
                        .map(|(name, outcome)| json!({ "name": name, "outcome": outcome }))
                        .collect::<Vec<_>>()
                );
            }
            Ok(Reply::Json(state))
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "List the project's services for this worker: dev servers and the like, started on demand. Each has a name, the port it listens on (this worker's) and its URL, what it needs, its state (running; starting: its process is up but nothing answers on its port yet; or stopped), and its log file. open_tab starts the service its page needs; use start_service for others. listeners are processes already listening on this worker's other ports, such as a dev server started by hand, with the port's name, port, pid and command."
    )]
    async fn list_services(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let worker = self.worker(&ctx)?;
            let (services, listeners) = tauri::async_runtime::spawn_blocking(move || {
                Workspace::open()
                    .and_then(|ws| {
                        let listeners = ws.port_listeners(&worker).unwrap_or_default();
                        Ok((ws.services(&worker)?, listeners))
                    })
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .map_err(|e| e.to_string())??;
            let mut reply = json!({ "services": services, "listeners": listeners });
            if services.is_empty() {
                reply["note"] = json!(tworky_core::services::NO_SERVICES_NOTE);
            }
            Ok(Reply::Json(reply))
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Start one of the project's services, and the services it needs, unless already running, waiting for each to listen on its port before starting the next. Prefer this to running the dev server yourself: Tworky tracks it, shows its output to the user as a job tab, and stops it on terminate. Returns what was started; on failure, the end of its log."
    )]
    async fn start_service(
        &self,
        Parameters(args): Parameters<ServiceArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let worker = self.worker(&ctx)?;
            let outcomes = tauri::async_runtime::spawn_blocking(move || {
                Workspace::open()
                    .and_then(|ws| ws.start_service(&worker, &args.name, true, &mut |_| {}))
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok(Reply::Json(json!({
                "services": outcomes
                    .into_iter()
                    .map(|(name, outcome)| json!({ "name": name, "outcome": outcome }))
                    .collect::<Vec<_>>(),
            })))
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Stop a service Tworky started for this worker, with everything under it. A dev server you started yourself with a shell command isn't a service, so the user can't start or stop it as one; stop it as you started it."
    )]
    async fn stop_service(
        &self,
        Parameters(args): Parameters<ServiceArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let worker = self.worker(&ctx)?;
            tauri::async_runtime::spawn_blocking(move || {
                Workspace::open()
                    .and_then(|ws| ws.stop_service(&worker, &args.name, &mut |_| {}))
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok(Reply::Json(json!({ "stopped": true })))
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "List this worker's open browser tabs, including ones the user opened, with their tab id, account (null for an anonymous tab), current URL and whether a login form for the account is showing. Tab ids are required by every other tab tool."
    )]
    async fn list_tabs(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let result = self.worker(&ctx).map(|worker| {
            let tabs: Vec<Value> = self
                .app
                .state::<Browser>()
                .worker_tabs(&worker)
                .iter()
                .map(|tab| {
                    let view = super::view(tab);
                    json!({
                        "tab": view.id,
                        "account": view.account,
                        "url": view.url,
                        "loading": view.loading,
                        "login_form": view.can_sign_in,
                    })
                })
                .collect();
            Reply::Json(json!({ "tabs": tabs }))
        });
        respond(result)
    }

    #[tool(
        description = "Navigate an open tab to an absolute URL and wait for the page to finish loading. The tab keeps its account's session. If the tab is held back because the service serving its page isn't running, this fails until that service answers: start it with start_service, then navigate again."
    )]
    async fn navigate(
        &self,
        Parameters(args): Parameters<NavigateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, _) = self.tab(&ctx, args.tab)?;
            super::load_page(&self.app, &tab, Some(&args.url))
                .await
                .map(Reply::Json)
        }
        .await;
        respond(result)
    }

    #[tool(description = "Go back one step in the tab's history.")]
    async fn go_back(
        &self,
        Parameters(args): Parameters<TabArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, cdp) = self.tab(&ctx, args.tab)?;
            page(&cdp, &tab).go_back().await.map(Reply::Json)
        }
        .await;
        respond(result)
    }

    #[tool(description = "Reload the tab's current page.")]
    async fn reload(
        &self,
        Parameters(args): Parameters<TabArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, _) = self.tab(&ctx, args.tab)?;
            super::load_page(&self.app, &tab, None)
                .await
                .map(Reply::Json)
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Close a tab. The account's cookies and session for this worker are kept, so a later open_tab resumes it. An anonymous tab's are discarded."
    )]
    async fn close_tab(
        &self,
        Parameters(args): Parameters<TabArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, _) = self.tab(&ctx, args.tab)?;
            super::close(&self.app, tab.id).await;
            Ok(Reply::Json(json!({ "closed": tab.id })))
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Read the visible text content of the tab's current page, along with its URL and title. Output is truncated to max_chars (default 20000). Prefer this over screenshot when you only need the text."
    )]
    async fn read_page(
        &self,
        Parameters(args): Parameters<ReadPageArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, cdp) = self.tab(&ctx, args.tab)?;
            page(&cdp, &tab)
                .read_page(args.max_chars.unwrap_or(DEFAULT_MAX_CHARS))
                .await
                .map(Reply::Json)
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Take a PNG screenshot of the tab's page. Captures the visible viewport by default, or the whole scrollable page when full_page is true. Mobile accounts are captured at phone size."
    )]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ScreenshotArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, cdp) = self.tab(&ctx, args.tab)?;
            let (caption, png_base64) = page(&cdp, &tab)
                .screenshot(args.full_page.unwrap_or(false))
                .await?;
            Ok(Reply::Image {
                caption,
                png_base64,
            })
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Sign in on the tab's current login page using the account's stored credentials, then submit the form. Tworky fills the credentials; they are never revealed to you. Fails on an anonymous tab, or if the page has no recognisable login form, the account has no stored password, or the page is not on an origin the account may sign in to. Check the result with read_page or screenshot afterwards."
    )]
    async fn sign_in(
        &self,
        Parameters(args): Parameters<TabArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, _) = self.tab(&ctx, args.tab)?;
            super::sign_in(&self.app, &tab, true).await.map(Reply::Json)
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Get an outline of the tab's page from its accessibility tree: headings, links, buttons, form fields with their values and states, and text. Every line ends with a ref like [ref=e412] that click, hover, drag, type, scroll, select_option and wait_for take. Refs go stale when the page navigates or re-renders that element; call snapshot again then. Password field values are never shown. Output is truncated to max_chars (default 20000)."
    )]
    async fn snapshot(
        &self,
        Parameters(args): Parameters<ReadPageArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let (tab, cdp) = self.tab(&ctx, args.tab)?;
            let (caption, text) = page(&cdp, &tab)
                .snapshot(args.max_chars.unwrap_or(DEFAULT_MAX_CHARS))
                .await?;
            Ok(Reply::Text { caption, text })
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Click an element, given its ref from snapshot or x,y viewport coordinates from a screenshot. The element is scrolled into view first. Sends real mouse events (taps on mobile tabs). Returns the page state after the click settles, with navigated: true if it started a page load (waited for, up to 15s) and opened_tab if it opened a popup, which becomes another tab of this worker."
    )]
    async fn click(
        &self,
        Parameters(args): Parameters<ClickArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let target = Target::required(args.reference.as_deref(), args.x, args.y)?;
            let button = Button::parse(args.button.as_deref())?;
            let double = args.double.unwrap_or(false);
            self.act(&ctx, args.tab, async |cdp, mobile| {
                interact::click(cdp, target, button, double, mobile).await
            })
            .await
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Drag from one point to another with the left button held: press at from_ref or from_x,from_y, move there in steps, release at to_ref or to_x,to_y. Coordinates are viewport CSS pixels, the same as screenshot pixels. Works for pointer and mouse event handlers, such as moving items on a canvas, sliders, resizing and sortable lists; mobile tabs get a touch drag. Native HTML5 drag and drop (draggable elements with dragstart and drop handlers) does not respond to it. Take a screenshot afterwards to check the result."
    )]
    async fn drag(
        &self,
        Parameters(args): Parameters<DragArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let from = Target::required(args.from_ref.as_deref(), args.from_x, args.from_y)
                .map_err(|e| format!("from: {e}"))?;
            let to = Target::required(args.to_ref.as_deref(), args.to_x, args.to_y)
                .map_err(|e| format!("to: {e}"))?;
            let steps = args.steps.unwrap_or(10).clamp(1, 200);
            self.act(&ctx, args.tab, async |cdp, mobile| {
                interact::drag(cdp, from, to, steps, mobile).await
            })
            .await
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Move the mouse over an element, given its ref from snapshot or x,y viewport coordinates, to trigger hover effects such as menus and tooltips. Call snapshot afterwards to see what appeared."
    )]
    async fn hover(
        &self,
        Parameters(args): Parameters<HoverArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let target = Target::required(args.reference.as_deref(), args.x, args.y)?;
            self.act(&ctx, args.tab, async |cdp, _| {
                interact::hover(cdp, target).await
            })
            .await
        }
        .await;
        respond(result)
    }

    #[tool(
        name = "type",
        description = "Type text as real keyboard input. Clicks the field given by ref or x,y first, or types into the focused element. clear deletes the existing text first; submit presses Enter afterwards. For sign-in forms use sign_in instead, which fills the account's stored credentials."
    )]
    async fn type_text(
        &self,
        Parameters(args): Parameters<TypeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let target = Target::from_args(args.reference.as_deref(), args.x, args.y)?;
            let (clear, submit) = (args.clear.unwrap_or(false), args.submit.unwrap_or(false));
            self.act(&ctx, args.tab, async |cdp, mobile| {
                interact::type_text(cdp, target, &args.text, clear, submit, mobile).await
            })
            .await
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Press a key in the tab, optionally with modifiers, e.g. \"Enter\", \"Escape\", \"Tab\", \"ArrowDown\", \"Shift+Tab\", \"Control+a\". The key goes to the focused element."
    )]
    async fn press_key(
        &self,
        Parameters(args): Parameters<PressKeyArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let key = KeyPress::parse(&args.key)?;
            self.act(&ctx, args.tab, async |cdp, _| {
                interact::press_key(cdp, &key).await
            })
            .await
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Scroll the page with the mouse wheel by delta_x/delta_y pixels, at an element's ref, at x,y, or at the viewport centre. With a ref and no deltas, scrolls that element into view."
    )]
    async fn scroll(
        &self,
        Parameters(args): Parameters<ScrollArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let delta_x = args.delta_x.unwrap_or(0.0);
            let delta_y = args.delta_y.unwrap_or(0.0);
            let target = Target::from_args(args.reference.as_deref(), args.x, args.y)?;
            if delta_x == 0.0 && delta_y == 0.0 && !matches!(target, Some(Target::Ref(_))) {
                return Err("give delta_x or delta_y, or a ref to scroll into view".to_string());
            }
            self.act(&ctx, args.tab, async |cdp, _| {
                interact::scroll(cdp, target, delta_x, delta_y).await
            })
            .await
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Choose options in a <select> element (shown as a combobox in snapshot), matched by option value or visible label. Fires input and change events like a user's choice. Returns the selected values."
    )]
    async fn select_option(
        &self,
        Parameters(args): Parameters<SelectOptionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let reference = parse_ref(&args.reference)
                .ok_or_else(|| format!("\"{}\" is not a ref like \"e412\"", args.reference))?;
            if args.values.is_empty() {
                return Err("give at least one value".to_string());
            }
            self.act(&ctx, args.tab, async |cdp, _| {
                interact::select_option(cdp, reference, args.values).await
            })
            .await
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Wait until the page's visible text contains text, no longer contains text_gone, or the element with ref is rendered. Checks every 250ms for up to timeout_ms (default 5000, max 25000). Returns matched: true or false and the page state."
    )]
    async fn wait_for(
        &self,
        Parameters(args): Parameters<WaitForArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let condition = wait_condition(&args)?;
            let timeout = args
                .timeout_ms
                .map_or(DEFAULT_WAIT, Duration::from_millis)
                .min(MAX_WAIT);
            let (tab, cdp) = self.tab(&ctx, args.tab)?;
            page(&cdp, &tab)
                .wait_for(&condition, timeout)
                .await
                .map(Reply::Json)
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Hand the change over to the user for acceptance testing, once it is built and you have checked it yourself in the browser. Tworky marks this worker Ready for UAT, notifies the user, and shows your summary, the steps as a checklist, and buttons for the tabs. Leave those tabs open on the pages to check, signed in as the right accounts. Call it as the last thing in your turn, then end the turn with a short message; the handover stays until the user dismisses it or sends you a new prompt."
    )]
    async fn hand_over(
        &self,
        Parameters(args): Parameters<HandOverArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let worker = self.worker(&ctx)?;
            if args.summary.trim().is_empty() {
                return Err("give a summary of what to test".to_string());
            }
            let tabs = args.tabs.unwrap_or_default();
            for &tab in &tabs {
                self.tab(&ctx, tab)?;
            }
            let handover = Handover {
                summary: args.summary.trim().to_string(),
                steps: args.steps,
                tabs,
            };
            let paths = Paths::resolve().map_err(|e| format!("{e:#}"))?;
            activity::hand_over(&paths, &worker, handover).map_err(|e| format!("{e:#}"))?;
            Ok(Reply::Json(json!({
                "handed_over": true,
                "note": "The user sees Ready for UAT once this turn ends.",
            })))
        }
        .await;
        respond(result)
    }

    #[tool(
        description = "Ask the user to create a test account the task needs and the project lacks: a role, permission or data set no account in list_accounts covers. Tworky marks this worker Needs account, notifies the user, and shows a card whose Create button opens Tworky's account dialog filled with your name, email and service. Returns at once: tell the user which account you need and why, then end your turn. Once they have created it they will prompt you, and it shows in list_accounts. Several requests can be pending; a request with the same name as a pending one replaces it."
    )]
    async fn request_account(
        &self,
        Parameters(args): Parameters<RequestAccountArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result = async {
            let worker = self.worker(&ctx)?;
            tauri::async_runtime::spawn_blocking(move || {
                let ws = Workspace::open().map_err(|e| format!("{e:#}"))?;
                let record = ws
                    .state
                    .get(&worker)
                    .map_err(|e| format!("{e:#}"))?
                    .ok_or_else(|| format!("worker {worker} not found"))?;
                let project = ws
                    .config
                    .project(&record.project)
                    .ok_or_else(|| format!("project {} is not configured", record.project))?;
                let request = account_request(
                    args,
                    &project.name,
                    &project.services,
                    &record.ports,
                    &project.accounts,
                )?;
                activity::request_account(&ws.config.paths, &worker, request)
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok(Reply::Json(json!({
                "requested": true,
                "note": "The user sees Needs account once this turn ends. Tell them which account you need and why, then end your turn.",
            })))
        }
        .await;
        respond(result)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TworkyMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Drives browser tabs in Tworky, beside this session's terminal, signed in as this worker's project test accounts. Each account has its own isolated browser session per worker, and start URLs point at this worker's dev server ports. Call list_accounts, then open_tab to get a tab id, then use navigate, read_page, screenshot and sign_in with that id. To interact, call snapshot to get element refs, then click, hover, drag, type, press_key, scroll or select_option with a ref, and wait_for to wait for the result. open_tab with no account gives an anonymous tab, signed in as nobody, for checking what a visitor sees. The user sees the same tabs and may open their own; list_tabs shows them all.\n\nTo test a UI change: open_tab as the account the change is for, which starts the dev server its page needs if the project defines it as a service (list_services; start others with start_service). When the project defines no service for a dev server you need, you can run it yourself, but tell the user they can't start or see it as a service, and suggest adding [[services]] in the project's setup session, sign_in if the page asks, then check the change with snapshot and screenshot, fixing what you find. When it works, leave tabs open on the pages to check and call hand_over with the steps for the user to accept it.

When a task needs an account no account covers (a role, permission or data set), call request_account with a suggested name, email, service and the reason, tell the user, and end your turn. Never work around a missing account: don't sign up through the app, edit the database, reuse an account with the wrong role or change the project's accounts yourself.",
        )
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_launch_replaces_the_workers_token() {
        let mcp = Mcp {
            port: 4100,
            tokens: Tokens::default(),
        };
        let first = mcp.endpoint("acme-1");
        let other = mcp.endpoint("acme-2");
        assert_eq!(first.url, "http://127.0.0.1:4100/mcp");
        assert_eq!(first.token.len(), 64);
        assert_eq!(mcp.tokens.worker(&first.token).as_deref(), Some("acme-1"));

        let second = mcp.endpoint("acme-1");
        assert_ne!(first.token, second.token);
        assert_eq!(mcp.tokens.worker(&first.token), None);
        assert_eq!(mcp.tokens.worker(&second.token).as_deref(), Some("acme-1"));

        mcp.revoke("acme-1");
        assert_eq!(mcp.tokens.worker(&second.token), None);
        assert_eq!(mcp.tokens.worker(&other.token).as_deref(), Some("acme-2"));
    }

    #[test]
    fn wait_condition_takes_exactly_one_condition() {
        let args = |text: Option<&str>, gone: Option<&str>, reference: Option<&str>| WaitForArgs {
            tab: 1,
            text: text.map(Into::into),
            text_gone: gone.map(Into::into),
            reference: reference.map(Into::into),
            timeout_ms: None,
        };
        assert!(matches!(
            wait_condition(&args(Some("Saved"), None, None)),
            Ok(WaitCondition::Text(t)) if t == "Saved"
        ));
        assert!(matches!(
            wait_condition(&args(None, None, Some("e7"))),
            Ok(WaitCondition::Visible(7))
        ));
        assert!(wait_condition(&args(Some("a"), Some("b"), None)).is_err());
        assert!(wait_condition(&args(None, None, Some("x"))).is_err());
        assert!(wait_condition(&args(None, None, None)).is_err());
    }

    #[test]
    fn open_tab_takes_an_account_or_a_url() {
        let parse = |json: Value| serde_json::from_value::<OpenTabArgs>(json).unwrap();
        let signed_in = parse(json!({ "account": "Tester 1" }));
        assert_eq!(signed_in.account.as_deref(), Some("Tester 1"));
        assert_eq!(signed_in.url, None);
        assert!(crate::browser::check_open(signed_in.account.as_deref(), None).is_ok());

        let anonymous = parse(json!({ "url": "http://localhost:5273/" }));
        assert_eq!(anonymous.account, None);
        assert!(
            crate::browser::check_open(anonymous.account.as_deref(), anonymous.url.as_deref())
                .is_ok()
        );

        let neither = parse(json!({}));
        let error = crate::browser::check_open(neither.account.as_deref(), neither.url.as_deref())
            .unwrap_err();
        assert!(error.contains("url"), "{error}");
    }
    fn request_args(name: &str, service: &str) -> RequestAccountArgs {
        serde_json::from_value(json!({
            "name": name,
            "email": " admin1@example.com ",
            "service": service,
            "reason": "An admin, to check the audit page",
        }))
        .unwrap()
    }

    fn services() -> Vec<ServiceConfig> {
        serde_json::from_value(json!([
            { "name": "api", "run": "npm run dev", "port": "api" },
            { "name": "admin-portal", "run": "npx vite", "port": "admin_portal" },
            { "name": "worker", "run": "npm run worker" },
        ]))
        .unwrap()
    }

    fn worker_ports() -> PortMap {
        [
            ("api".to_string(), 3101),
            ("admin_portal".to_string(), 5274),
        ]
        .into()
    }

    fn accounts() -> Vec<AccountConfig> {
        serde_json::from_value(json!([{ "name": "Tester 1", "username": "tester1@example.com" }]))
            .unwrap()
    }

    #[test]
    fn an_account_request_names_a_new_account_on_a_service_with_a_port() {
        let request = account_request(
            request_args(" Admin 1 ", "Admin-Portal"),
            "acme",
            &services(),
            &worker_ports(),
            &accounts(),
        )
        .unwrap();
        assert_eq!(
            request,
            AccountRequest {
                name: "Admin 1".into(),
                username: "admin1@example.com".into(),
                service: "admin-portal".into(),
                reason: "An admin, to check the audit page".into(),
            }
        );

        let taken = account_request(
            request_args("tester 1", "api"),
            "acme",
            &services(),
            &worker_ports(),
            &accounts(),
        )
        .unwrap_err();
        assert!(taken.contains("Tester 1 already exists"), "{taken}");

        let portless = account_request(
            request_args("Admin 1", "worker"),
            "acme",
            &services(),
            &worker_ports(),
            &accounts(),
        )
        .unwrap_err();
        assert!(portless.contains("api, admin-portal"), "{portless}");

        let unknown = account_request(
            request_args("Admin 1", "web"),
            "acme",
            &services(),
            &worker_ports(),
            &accounts(),
        )
        .unwrap_err();
        assert!(
            unknown.contains("no service with a port on this worker called web"),
            "{unknown}"
        );

        let unnamed = account_request(
            request_args(" ", "api"),
            "acme",
            &services(),
            &worker_ports(),
            &accounts(),
        );
        assert!(unnamed.is_err());
    }

    #[test]
    fn an_account_request_needs_a_service_with_a_port() {
        let error = account_request(
            request_args("Admin 1", "worker"),
            "acme",
            &services()[2..],
            &worker_ports(),
            &[],
        )
        .unwrap_err();
        assert!(error.contains("[[services]]"), "{error}");
    }

    #[test]
    fn an_account_request_needs_a_port_on_this_worker() {
        let ports: PortMap = [("api".to_string(), 3101)].into();
        let missing = account_request(
            request_args("Admin 1", "admin-portal"),
            "acme",
            &services(),
            &ports,
            &accounts(),
        )
        .unwrap_err();
        assert!(missing.contains("use one of: api"), "{missing}");

        let none = account_request(
            request_args("Admin 1", "api"),
            "acme",
            &services(),
            &PortMap::new(),
            &accounts(),
        )
        .unwrap_err();
        assert!(none.contains("has a port on this worker"), "{none}");
    }
}
