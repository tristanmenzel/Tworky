use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::watch;
use url::Url;

use crate::cdp::{self, Cdp, evaluate};
use crate::diagnostics::{Diagnostics, Part, cut_url, redact};
use crate::interact::{self, WaitCondition};
use crate::login::{self, Account, LoginField, LoginMessage};
use crate::snapshot;
use crate::urls::normalize_url;
use crate::viewport::{self, Viewport};

pub const LOAD_WAIT: Duration = Duration::from_secs(15);
pub const SUBMIT_WAIT: Duration = Duration::from_secs(8);
pub const LOGIN_FORM_WAIT: Duration = Duration::from_secs(5);
pub const ACTION_SETTLE: Duration = Duration::from_millis(300);
pub const WAIT_POLL: Duration = Duration::from_millis(250);
const NETWORK_FALLBACK_BUFFER: u64 = 1024 * 1024;
pub const STORAGE_WAIT: Duration = Duration::from_secs(3);

/// Reads key and database names only. Errors are reported by name, since a message can quote
/// the page.
const STORAGE_NAMES_SCRIPT: &str = r#"(async () => {
  const errors = [];
  const failed = (what, e) => errors.push(`${what}: ${(e && e.name) || "error"}`);
  const keys = (what) => {
    try {
      const storage = window[what];
      const names = [];
      for (let i = 0; i < storage.length; i++) names.push(storage.key(i));
      return names;
    } catch (e) {
      failed(what, e);
      return null;
    }
  };
  const local = keys("localStorage");
  const session = keys("sessionStorage");
  let databases = null;
  try {
    databases = (await indexedDB.databases()).map((db) => db.name);
  } catch (e) {
    failed("indexedDB", e);
  }
  return { local, session, databases, errors };
})()"#;

const READ_PAGE_SCRIPT: &str = r#"(() => ({
  url: location.href,
  title: document.title,
  text: document.body ? document.body.innerText : "",
}))()"#;

#[derive(Debug, Clone, PartialEq)]
pub struct LoginForm {
    pub fields: Vec<LoginField>,
    pub origin: String,
}

#[derive(Debug, Clone, Default)]
pub struct PageState {
    pub url: String,
    pub loading: bool,
    /// Counts document loads started, so an action can tell it caused a navigation.
    pub navigations: u64,
    pub loads: u64,
    pub login: Option<LoginForm>,
    /// Tabs this page opened (popups), oldest first.
    pub opened: Vec<u64>,
}

/// What the host tells a page about itself: loads, the login detector's reports and popups,
/// which the tools wait on, and the page's console and requests.
pub struct PageEvents {
    state: watch::Sender<PageState>,
    diagnostics: Mutex<Diagnostics>,
}

impl Default for PageEvents {
    fn default() -> Self {
        PageEvents {
            state: watch::Sender::new(PageState::default()),
            diagnostics: Mutex::default(),
        }
    }
}

impl PageEvents {
    pub fn diagnostics(&self) -> MutexGuard<'_, Diagnostics> {
        self.diagnostics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn load_started(&self, url: &str) {
        self.state.send_modify(|s| {
            s.url = url.to_string();
            s.loading = true;
            s.navigations += 1;
            s.login = None;
        });
    }

    pub fn load_finished(&self, url: &str) {
        self.state.send_modify(|s| {
            s.url = url.to_string();
            s.loading = false;
            s.loads += 1;
        });
    }

    /// A same-document navigation (history.pushState, a fragment change).
    pub fn url_changed(&self, url: &str) {
        self.state.send_modify(|s| s.url = url.to_string());
    }

    /// Records a report from the login detector and returns it, so the host can act on a
    /// fill request. The form is tied to the page's origin at the time.
    pub fn login_message(&self, payload: &str) -> Option<LoginMessage> {
        let message = LoginMessage::parse(payload)?;
        self.state.send_modify(|s| match &message {
            LoginMessage::LoginForm { fields } => {
                s.login = Url::parse(&s.url).ok().map(|url| LoginForm {
                    fields: fields.clone(),
                    origin: url.origin().ascii_serialization(),
                });
            }
            LoginMessage::LoginFormGone => s.login = None,
            LoginMessage::FillRequest => {}
        });
        Some(message)
    }

    pub fn opened_tab(&self, tab: u64) {
        self.state.send_modify(|s| s.opened.push(tab));
    }

    pub fn state(&self) -> PageState {
        self.state.borrow().clone()
    }

    async fn wait(&self, timeout: Duration, ready: impl FnMut(&PageState) -> bool) -> bool {
        let mut rx = self.state.subscribe();
        tokio::time::timeout(timeout, rx.wait_for(ready))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Waits for a load that finishes after `since` loads. False if it timed out.
    pub async fn wait_for_load(&self, since: u64, timeout: Duration) -> bool {
        self.wait(timeout, |s| s.loads > since && !s.loading).await
    }
}

/// One tab as the tools see it: a CDP connection plus the host's events.
pub struct Page<'a, C: Cdp> {
    pub cdp: &'a C,
    pub events: &'a PageEvents,
    pub viewport: Viewport,
}

fn names(list: &Value, key: Option<&str>) -> Value {
    match list.as_array() {
        Some(items) => items
            .iter()
            .filter_map(|item| key.map_or(Some(item), |key| item.get(key)))
            .filter_map(Value::as_str)
            .map(Value::from)
            .collect(),
        None => Value::Null,
    }
}

fn merge(mut into: Value, from: Value) -> Value {
    if let (Value::Object(into), Value::Object(from)) = (&mut into, from) {
        into.extend(from);
    }
    into
}

impl<C: Cdp> Page<'_, C> {
    /// `{ url, title }`, plus `still_loading` when a wait gave up.
    pub async fn state(&self, still_loading: bool) -> Value {
        let title = evaluate(self.cdp, "document.title")
            .await
            .ok()
            .and_then(|t| t.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut state = json!({ "url": self.events.state().url, "title": title });
        if still_loading {
            state["still_loading"] = true.into();
        }
        state
    }

    /// Prepares a new tab before its first navigation: the login detector, diagnostics, and
    /// its viewport, which must be set first or the first page loads with the desktop user
    /// agent. `metrics` sends the viewport's metrics override (see `viewport::apply`). A fresh
    /// view has no overrides, so Fit sends nothing.
    pub async fn prepare(
        &self,
        metrics: impl Future<Output = Result<(), String>>,
    ) -> Result<(), String> {
        login::install_detector(self.cdp).await?;
        self.enable_diagnostics().await;
        if self.viewport != Viewport::Fit {
            viewport::apply(self.cdp, &self.viewport, false, metrics).await?;
        }
        Ok(())
    }

    async fn enable_diagnostics(&self) {
        let off = |part, note: String| self.events.diagnostics().turn_off(part, note);
        match self.cdp.call("Page.getFrameTree", json!({})).await {
            Ok(tree) => match tree["frameTree"]["frame"]["id"].as_str() {
                Some(id) => self.events.diagnostics().set_main_frame(id),
                None => off(
                    Part::MainFrame,
                    "loads are not numbered because Page.getFrameTree returned no main frame"
                        .into(),
                ),
            },
            Err(e) => off(
                Part::MainFrame,
                format!("loads are not numbered because {e}"),
            ),
        }
        if let Err(e) = self.cdp.call("Log.enable", json!({})).await {
            off(
                Part::Console,
                format!("browser log entries are off because {e}"),
            );
        }
        let unbuffered = json!({
            "maxTotalBufferSize": 0,
            "maxResourceBufferSize": 0,
            "maxPostDataSize": 0,
        });
        if self.cdp.call("Network.enable", unbuffered).await.is_err() {
            let buffered = json!({
                "maxTotalBufferSize": NETWORK_FALLBACK_BUFFER,
                "maxResourceBufferSize": NETWORK_FALLBACK_BUFFER,
                "maxPostDataSize": 0,
            });
            if let Err(e) = self.cdp.call("Network.enable", buffered).await {
                off(Part::Network, format!("requests are off because {e}"));
            }
        }
    }

    /// Names only, never values. Cookie names come from the browser first, so a page that
    /// doesn't answer still has them.
    pub async fn list_storage(&self) -> Value {
        let url = self.events.state().url;
        let mut notes = Vec::new();
        let params = if url.is_empty() {
            json!({})
        } else {
            json!({ "urls": [url] })
        };
        let cookies = match self.cdp.call("Network.getCookies", params).await {
            Ok(list) => names(&list["cookies"], Some("name")),
            Err(e) => {
                notes.push(format!("cookie names are missing because {e}"));
                Value::Null
            }
        };
        let mut listing = json!({ "url": cut_url(&url), "cookies": cookies });
        match tokio::time::timeout(STORAGE_WAIT, self.storage_names()).await {
            Ok(Ok(storage)) => {
                listing["local_storage"] = names(&storage["local"], None);
                listing["session_storage"] = names(&storage["session"], None);
                listing["indexed_db"] = names(&storage["databases"], None);
                for error in storage["errors"].as_array().into_iter().flatten() {
                    if let Some(error) = error.as_str() {
                        notes.push(format!("could not list {}", redact(error)));
                    }
                }
            }
            Ok(Err(e)) => notes.push(format!(
                "storage key and database names are missing because {}",
                redact(&e)
            )),
            Err(_) => notes.push(format!(
                "storage key and database names are missing because the page did not answer within {}s",
                STORAGE_WAIT.as_secs()
            )),
        }
        if !notes.is_empty() {
            listing["notes"] = notes.into();
        }
        listing
    }

    /// Runs in an isolated world, where page script can't replace what the listing calls.
    async fn storage_names(&self) -> Result<Value, String> {
        let recorded = self.events.diagnostics().main_frame().map(str::to_string);
        let frame = match recorded {
            Some(frame) => frame,
            None => {
                self.cdp.call("Page.getFrameTree", json!({})).await?["frameTree"]["frame"]["id"]
                    .as_str()
                    .ok_or("Page.getFrameTree returned no main frame")?
                    .to_string()
            }
        };
        let world = self
            .cdp
            .call(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": login::WORLD }),
            )
            .await?;
        let context = world["executionContextId"]
            .as_i64()
            .ok_or("Page.createIsolatedWorld returned no execution context")?;
        cdp::evaluate_awaiting(self.cdp, context, STORAGE_NAMES_SCRIPT).await
    }

    async fn load(&self, start: impl Future<Output = Result<(), String>>) -> Result<Value, String> {
        let since = self.events.state().loads;
        start.await?;
        let loaded = self.events.wait_for_load(since, LOAD_WAIT).await;
        Ok(self.state(!loaded).await)
    }

    pub async fn navigate(&self, url: &str) -> Result<Value, String> {
        let url =
            normalize_url(url).ok_or_else(|| format!("not a valid http or https URL: {url}"))?;
        self.load(async {
            self.cdp
                .call("Page.navigate", json!({ "url": url }))
                .await
                .map(drop)
        })
        .await
    }

    pub async fn go_back(&self) -> Result<Value, String> {
        let history = self
            .cdp
            .call("Page.getNavigationHistory", json!({}))
            .await?;
        let index = history["currentIndex"].as_i64().unwrap_or(0);
        let entry = usize::try_from(index - 1)
            .ok()
            .and_then(|i| history["entries"].get(i))
            .and_then(|e| e["id"].as_i64())
            .ok_or("there is no earlier page in this tab's history")?;
        self.load(async {
            self.cdp
                .call("Page.navigateToHistoryEntry", json!({ "entryId": entry }))
                .await
                .map(drop)
        })
        .await
    }

    pub async fn reload(&self) -> Result<Value, String> {
        self.load(async { self.cdp.call("Page.reload", json!({})).await.map(drop) })
            .await
    }

    /// Runs an input action, lets the page react, and reports whether it navigated or opened
    /// a tab. A navigation it started is waited for.
    pub async fn act(
        &self,
        action: impl Future<Output = Result<Value, String>>,
    ) -> Result<Value, String> {
        let before = self.events.state();
        let mut result = action.await?;
        tokio::time::sleep(ACTION_SETTLE).await;
        let now = self.events.state();
        let navigated = now.navigations > before.navigations;
        result["navigated"] = navigated.into();
        if let Some(tab) = now
            .opened
            .get(before.opened.len()..)
            .and_then(<[u64]>::last)
        {
            result["opened_tab"] = (*tab).into();
        }
        let loaded =
            !(navigated && now.loading) || self.events.wait_for_load(now.loads, LOAD_WAIT).await;
        Ok(merge(self.state(!loaded).await, result))
    }

    /// Fills the detected login form with the account's username and password, and submits it
    /// if asked. The password is read only here, and only for a page on one of the account's
    /// origins.
    pub async fn sign_in(
        &self,
        account: &Account,
        submit: bool,
        has_password: impl FnOnce() -> bool,
        password: impl FnOnce() -> Result<Option<String>, String>,
    ) -> Result<Value, String> {
        if self.events.state().login.is_none() {
            self.events
                .wait(LOGIN_FORM_WAIT, |s| s.login.is_some())
                .await;
        }
        let state = self.events.state();
        let url = Url::parse(&state.url).map_err(|_| "the page has no address yet".to_string())?;
        if !login::should_offer(account, &url, has_password) {
            return Err(format!(
                "this page's origin is not allowed for {}; it can sign in on {}",
                account.name,
                account
                    .allowed_origins()
                    .iter()
                    .map(|o| o.ascii_serialization())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let origin = url.origin().ascii_serialization();
        let form = state
            .login
            .filter(|form| form.origin == origin)
            .ok_or("no login form detected on this page")?;
        let username = Some(account.username.trim()).filter(|u| !u.is_empty());
        let wants_password = form.fields.contains(&LoginField::Password);
        let password = if wants_password { password()? } else { None };
        let missing_password = wants_password && password.is_none();
        let script = login::fill_script(username, password.as_deref(), submit);
        drop(password);
        let mut outcome = evaluate(self.cdp, &script).await?;
        if !outcome.is_object() {
            return Err("the fill script returned no result".into());
        }
        outcome["missing_password"] = missing_password.into();
        let loaded = outcome["submitted"] != true
            || self.events.wait_for_load(state.loads, SUBMIT_WAIT).await;
        Ok(merge(self.state(!loaded).await, outcome))
    }

    pub async fn read_page(&self, max_chars: usize) -> Result<Value, String> {
        let mut page = evaluate(self.cdp, READ_PAGE_SCRIPT)
            .await
            .map_err(|e| format!("could not read the page: {e}"))?;
        let text = page["text"].as_str().unwrap_or_default();
        let truncated = text.chars().count() > max_chars;
        page["text"] = text.chars().take(max_chars).collect::<String>().into();
        page["truncated"] = truncated.into();
        let url = self.events.state().url;
        if url.starts_with("chrome-error://")
            || page["url"]
                .as_str()
                .is_some_and(|href| href.starts_with("chrome-error://"))
        {
            page["error_page"] = true.into();
        }
        page["url"] = url.into();
        Ok(page)
    }

    /// The accessibility outline with element refs, and a caption with the page state.
    pub async fn snapshot(&self, max_chars: usize) -> Result<(Value, String), String> {
        let tree = self
            .cdp
            .call("Accessibility.getFullAXTree", json!({}))
            .await?;
        let outline = snapshot::outline(&tree, max_chars);
        let mut caption = self.state(false).await;
        caption["truncated"] = outline.truncated.into();
        let text = if outline.text.is_empty() {
            "(the page has no accessible content yet)".to_string()
        } else {
            outline.text
        };
        Ok((caption, text))
    }

    /// A base64 PNG at CSS pixel scale, on HiDPI displays and mobile pages too.
    pub async fn screenshot(&self, full_page: bool) -> Result<(Value, String), String> {
        let png = cdp::screenshot(self.cdp, full_page).await?;
        Ok((self.state(false).await, png))
    }

    pub async fn wait_for(
        &self,
        condition: &WaitCondition,
        timeout: Duration,
    ) -> Result<Value, String> {
        let started = Instant::now();
        let deadline = started + timeout;
        let matched = loop {
            if interact::check(self.cdp, condition).await {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(WAIT_POLL).await;
        };
        let mut state = self.state(false).await;
        state["matched"] = matched.into();
        state["waited_ms"] = (started.elapsed().as_millis() as u64).into();
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cdp::fake::FakeCdp;

    fn account() -> Account {
        Account {
            name: "Tester 1".into(),
            username: "tester@x".into(),
            start_url: "http://localhost:5173/".into(),
            login_origins: vec!["https://login.example".into()],
            mobile: false,
        }
    }

    fn evaluating(value: Value) -> FakeCdp {
        FakeCdp::answering(move |method, params| {
            Ok(match method {
                "Runtime.evaluate" if params["expression"] == "document.title" => {
                    json!({ "result": { "value": "Home" } })
                }
                "Runtime.evaluate" => json!({ "result": { "value": value.clone() } }),
                _ => json!({}),
            })
        })
    }

    #[tokio::test(start_paused = true)]
    async fn navigate_waits_for_the_next_load() {
        let events = Arc::new(PageEvents::default());
        let cdp = evaluating(json!(null));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let host = events.clone();
        let loader = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            host.load_started("http://localhost:5173/");
            host.load_finished("http://localhost:5173/");
        });
        let state = page.navigate("localhost:5173").await.unwrap();
        loader.await.unwrap();
        assert_eq!(state["url"], "http://localhost:5173/");
        assert_eq!(state["title"], "Home");
        assert!(state.get("still_loading").is_none());
        assert_eq!(
            cdp.params("Page.navigate")[0]["url"],
            "http://localhost:5173/"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_load_that_never_finishes_is_reported() {
        let events = PageEvents::default();
        let cdp = evaluating(json!(null));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let state = page.reload().await.unwrap();
        assert_eq!(state["still_loading"], true);
    }

    #[tokio::test]
    async fn navigate_refuses_other_schemes() {
        let events = PageEvents::default();
        let cdp = FakeCdp::default();
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        assert!(page.navigate("file:///etc/passwd").await.is_err());
        assert!(cdp.methods().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn go_back_needs_an_earlier_entry() {
        let events = PageEvents::default();
        let cdp =
            FakeCdp::answering(|_, _| Ok(json!({ "currentIndex": 0, "entries": [{ "id": 1 }] })));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        assert!(page.go_back().await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn an_action_reports_navigation_and_popups() {
        let events = PageEvents::default();
        events.load_finished("http://localhost:5173/");
        let cdp = evaluating(json!(null));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let result = page
            .act(async {
                events.load_started("http://localhost:5173/next");
                events.load_finished("http://localhost:5173/next");
                events.opened_tab(9);
                Ok(json!({ "at": { "x": 1, "y": 2 } }))
            })
            .await
            .unwrap();
        assert_eq!(result["navigated"], true);
        assert_eq!(result["opened_tab"], 9);
        assert_eq!(result["url"], "http://localhost:5173/next");
        assert_eq!(result["at"]["x"], 1);
    }

    #[tokio::test(start_paused = true)]
    async fn sign_in_fills_only_on_an_allowed_origin() {
        let events = PageEvents::default();
        events.load_finished("https://evil.example/login");
        events.login_message(r#"{"type":"login-form","fields":["username","password"]}"#);
        let cdp = evaluating(json!({ "filled": ["username", "password"], "submitted": false }));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let error = page
            .sign_in(
                &account(),
                true,
                || true,
                || panic!("the password must not be read"),
            )
            .await
            .unwrap_err();
        assert!(error.contains("not allowed"));
        assert!(cdp.methods().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn sign_in_fills_the_detected_form() {
        let events = PageEvents::default();
        events.load_finished("https://login.example/authorize");
        events.login_message(r#"{"type":"login-form","fields":["username","password"]}"#);
        let cdp = evaluating(json!({ "filled": ["username", "password"], "submitted": false }));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let outcome = page
            .sign_in(&account(), true, || true, || Ok(Some("s3cret".into())))
            .await
            .unwrap();
        assert_eq!(outcome["filled"], json!(["username", "password"]));
        assert_eq!(outcome["missing_password"], false);
        assert!(!outcome.to_string().contains("s3cret"));
        let script = cdp.params("Runtime.evaluate")[0]["expression"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(script.contains(r#""password":"s3cret""#));
        assert!(script.contains(r#""submit":true"#));
    }

    #[tokio::test(start_paused = true)]
    async fn sign_in_waits_briefly_for_a_form() {
        let events = PageEvents::default();
        events.load_finished("http://localhost:5173/");
        let cdp = evaluating(json!({}));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let error = page
            .sign_in(&account(), true, || true, || Ok(None))
            .await
            .unwrap_err();
        assert_eq!(error, "no login form detected on this page");
    }

    #[test]
    fn a_new_document_forgets_the_old_login_form() {
        let events = PageEvents::default();
        events.load_finished("https://login.example/a");
        events.login_message(r#"{"type":"login-form","fields":["password"]}"#);
        assert_eq!(
            events.state().login.unwrap().origin,
            "https://login.example"
        );
        events.load_started("https://login.example/b");
        assert!(events.state().login.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_polls_until_the_deadline() {
        let events = PageEvents::default();
        let cdp = evaluating(json!(false));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let state = page
            .wait_for(&WaitCondition::Text("Done".into()), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(state["matched"], false);
        assert!(state["waited_ms"].as_u64().unwrap() >= 1000);
    }

    #[tokio::test]
    async fn read_page_truncates_and_flags_error_pages() {
        let events = PageEvents::default();
        events.load_finished("chrome-error://chromewebdata/");
        let cdp = evaluating(json!({ "url": "x", "title": "t", "text": "abcdef" }));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let read = page.read_page(3).await.unwrap();
        assert_eq!(read["text"], "abc");
        assert_eq!(read["truncated"], true);
        assert_eq!(read["error_page"], true);
    }

    fn storage_page(
        respond: impl Fn(&str, &Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> (PageEvents, FakeCdp) {
        let events = PageEvents::default();
        events.load_finished("http://localhost:2020/auth/callback?code=abc#state=xyz");
        events.diagnostics().set_main_frame("MAIN");
        let cdp = FakeCdp::answering(move |method, params| match method {
            "Network.getCookies" => Ok(json!({ "cookies": [
                { "name": "sid", "value": "cookie-value-1", "domain": "localhost" },
                { "name": "pref", "value": "cookie-value-2", "domain": "localhost" },
            ] })),
            "Page.createIsolatedWorld" => Ok(json!({ "executionContextId": 42 })),
            _ => respond(method, params),
        });
        (events, cdp)
    }

    #[tokio::test]
    async fn list_storage_returns_names_only() {
        let (events, cdp) = storage_page(|_, _| {
            Ok(json!({ "result": { "value": {
                "local": ["msal.account.keys", "theme"],
                "session": [],
                "databases": ["app-cache"],
                "errors": [],
            } } }))
        });
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let listing = page.list_storage().await;
        assert_eq!(
            listing,
            json!({
                "url": "http://localhost:2020/auth/callback",
                "cookies": ["sid", "pref"],
                "local_storage": ["msal.account.keys", "theme"],
                "session_storage": [],
                "indexed_db": ["app-cache"],
            })
        );
        assert!(!listing.to_string().contains("cookie-value"));
        assert_eq!(
            cdp.params("Network.getCookies"),
            [json!({ "urls": ["http://localhost:2020/auth/callback?code=abc#state=xyz"] })]
        );
        let script = cdp.params("Runtime.evaluate")[0]["expression"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!script.contains("getItem"));
    }

    #[tokio::test]
    async fn list_storage_evaluates_in_an_isolated_world_on_the_main_frame() {
        let (events, cdp) =
            storage_page(|_, _| Ok(json!({ "result": { "value": { "local": [] } } })));
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        page.list_storage().await;
        assert_eq!(
            cdp.methods(),
            [
                "Network.getCookies",
                "Page.createIsolatedWorld",
                "Runtime.evaluate"
            ]
        );
        assert_eq!(
            cdp.params("Page.createIsolatedWorld"),
            [json!({ "frameId": "MAIN", "worldName": "tworky" })]
        );
        let evaluate = &cdp.params("Runtime.evaluate")[0];
        assert_eq!(evaluate["contextId"], 42);
        assert_eq!(evaluate["awaitPromise"], true);
    }

    #[tokio::test]
    async fn list_storage_reports_parts_the_page_could_not_list() {
        let (events, cdp) = storage_page(|_, _| {
            Ok(json!({ "result": { "value": {
                "local": null,
                "session": ["s"],
                "databases": null,
                "errors": ["localStorage: SecurityError", "indexedDB: UnknownError"],
            } } }))
        });
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let listing = page.list_storage().await;
        assert_eq!(listing["local_storage"], Value::Null);
        assert_eq!(listing["session_storage"], json!(["s"]));
        assert_eq!(
            listing["notes"],
            json!([
                "could not list localStorage: SecurityError",
                "could not list indexedDB: UnknownError",
            ])
        );
    }

    #[tokio::test]
    async fn list_storage_keeps_cookie_names_when_the_script_throws() {
        let (events, cdp) = storage_page(|_, _| {
            Ok(
                json!({ "result": {}, "exceptionDetails": { "text": "Uncaught",
                "exception": { "description": "TypeError: boom" } } }),
            )
        });
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let listing = page.list_storage().await;
        assert_eq!(listing["cookies"], json!(["sid", "pref"]));
        assert!(listing.get("local_storage").is_none());
        assert_eq!(
            listing["notes"],
            json!(["storage key and database names are missing because TypeError: boom"])
        );
    }

    /// Answers like `inner`, except that `method` never answers.
    struct Hanging {
        inner: FakeCdp,
        method: &'static str,
    }

    impl Cdp for Hanging {
        async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            if method == self.method {
                std::future::pending::<()>().await;
            }
            self.inner.call(method, params).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn list_storage_keeps_cookie_names_when_the_page_hangs() {
        let (events, inner) = storage_page(|_, _| Ok(json!({})));
        let cdp = Hanging {
            inner,
            method: "Runtime.evaluate",
        };
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        let started = tokio::time::Instant::now();
        let listing = page.list_storage().await;
        assert_eq!(started.elapsed(), STORAGE_WAIT);
        assert_eq!(listing["cookies"], json!(["sid", "pref"]));
        assert_eq!(
            listing["notes"],
            json!([
                "storage key and database names are missing because the page did not answer within 3s"
            ])
        );
    }

    #[tokio::test]
    async fn list_storage_finds_the_main_frame_when_none_was_recorded() {
        let events = PageEvents::default();
        let cdp = FakeCdp::answering(|method, _| {
            Ok(match method {
                "Page.getFrameTree" => json!({ "frameTree": { "frame": { "id": "LATE" } } }),
                "Page.createIsolatedWorld" => json!({ "executionContextId": 5 }),
                _ => json!({ "result": { "value": {} } }),
            })
        });
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        page.list_storage().await;
        assert_eq!(cdp.params("Network.getCookies"), [json!({})]);
        assert_eq!(cdp.params("Page.createIsolatedWorld")[0]["frameId"], "LATE");
    }

    fn notes(events: &PageEvents, part: Part) -> Vec<String> {
        events
            .diagnostics()
            .notes(part)
            .map(str::to_string)
            .collect()
    }

    async fn send_metrics(page: &Page<'_, FakeCdp>) -> Result<(), String> {
        let (method, params) = viewport::metrics_call(&page.viewport, viewport::DEFAULT_FRAME);
        page.cdp.call(method, params).await.map(drop)
    }

    #[tokio::test]
    async fn prepare_enables_diagnostics_without_buffering_bodies() {
        let events = PageEvents::default();
        let cdp = FakeCdp::answering(|method, _| {
            Ok(match method {
                "Page.getFrameTree" => json!({ "frameTree": { "frame": { "id": "MAIN" } } }),
                _ => json!({}),
            })
        });
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        page.prepare(send_metrics(&page)).await.unwrap();
        assert!(cdp.methods().contains(&"Log.enable".to_string()));
        assert_eq!(
            cdp.params("Network.enable"),
            [json!({ "maxTotalBufferSize": 0, "maxResourceBufferSize": 0, "maxPostDataSize": 0 })]
        );
        assert_eq!(events.diagnostics().main_frame(), Some("MAIN"));
        for part in [Part::Console, Part::Network, Part::MainFrame] {
            assert!(notes(&events, part).is_empty());
        }
        assert!(!cdp.methods().iter().any(|m| m.starts_with("Emulation.")));
    }

    #[tokio::test]
    async fn prepare_emulates_a_phone_before_the_first_navigation() {
        let events = PageEvents::default();
        let cdp = FakeCdp::default();
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: viewport::PHONE,
        };
        page.prepare(send_metrics(&page)).await.unwrap();
        assert_eq!(
            cdp.params("Emulation.setDeviceMetricsOverride"),
            [json!({
                "width": 393,
                "height": 852,
                "deviceScaleFactor": 3.0,
                "mobile": true,
                "screenWidth": 393,
                "screenHeight": 852,
                "screenOrientation": { "type": "portraitPrimary", "angle": 0 },
                "scale": 800.0 / 852.0,
                "dontSetVisibleSize": true,
            })]
        );
        assert_eq!(
            cdp.params("Emulation.setTouchEmulationEnabled"),
            [json!({ "enabled": true, "maxTouchPoints": 5 })]
        );
        assert_eq!(
            cdp.params("Emulation.setUserAgentOverride")[0]["platform"],
            "iPhone"
        );
        assert!(!cdp.methods().contains(&"Page.navigate".to_string()));
    }

    #[tokio::test]
    async fn prepare_retries_network_with_small_buffers() {
        let events = PageEvents::default();
        let cdp = FakeCdp::answering(|method, params| match method {
            "Network.enable" if params["maxTotalBufferSize"] == 0 => {
                Err("Network.enable failed: Invalid parameters".into())
            }
            _ => Ok(json!({})),
        });
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        page.prepare(send_metrics(&page)).await.unwrap();
        let calls = cdp.params("Network.enable");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1],
            json!({
                "maxTotalBufferSize": 1024 * 1024,
                "maxResourceBufferSize": 1024 * 1024,
                "maxPostDataSize": 0,
            })
        );
        assert!(notes(&events, Part::Network).is_empty());
    }

    #[tokio::test]
    async fn prepare_succeeds_when_every_diagnostics_call_fails() {
        let events = PageEvents::default();
        let cdp = FakeCdp::answering(|method, _| match method {
            "Page.getFrameTree" | "Log.enable" | "Network.enable" => {
                Err(format!("{method} failed: nope"))
            }
            _ => Ok(json!({})),
        });
        let page = Page {
            cdp: &cdp,
            events: &events,
            viewport: Viewport::Fit,
        };
        page.prepare(send_metrics(&page)).await.unwrap();
        assert!(
            cdp.methods()
                .contains(&"Page.addScriptToEvaluateOnNewDocument".to_string())
        );
        assert_eq!(events.diagnostics().main_frame(), None);
        assert_eq!(
            notes(&events, Part::Network),
            ["requests are off because Network.enable failed: nope"]
        );
        assert_eq!(
            notes(&events, Part::Console),
            ["browser log entries are off because Log.enable failed: nope"]
        );
        assert_eq!(
            notes(&events, Part::MainFrame),
            ["loads are not numbered because Page.getFrameTree failed: nope"]
        );
    }
}
