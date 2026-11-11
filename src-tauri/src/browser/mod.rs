use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tworky_browser::cookies;
use tworky_browser::login::{self, Account, LoginField};
use tworky_browser::page::{Page, PageEvents};
use tworky_core::config::AccountConfig;
use tworky_core::services::Outcome;
use tworky_core::worker::Workspace;

mod mcp;
mod store;
pub use mcp::Mcp;
use store::{Saved, SavedProfile, SavedTab};

#[cfg(windows)]
mod webview2;
#[cfg(windows)]
use webview2 as host;

#[cfg(not(windows))]
mod unsupported;
#[cfg(not(windows))]
use unsupported as host;

type CmdResult<T> = Result<T, String>;

pub struct Tab {
    pub id: u64,
    pub worker: String,
    pub project: String,
    /// `None` for an anonymous tab.
    pub account: RwLock<Option<Account>>,
    pub profile: String,
    pub color: Option<String>,
    pub events: Arc<PageEvents>,
    /// Shown in the tab's bar while its page's service starts, or why it didn't.
    pub status: Mutex<Option<String>>,
    /// Set instead of loading a page whose service isn't running, until the user starts it.
    pub paused: Mutex<Option<Paused>>,
}

impl Tab {
    pub fn account(&self) -> Option<Account> {
        self.account.read().unwrap().clone()
    }

    pub fn anonymous(&self) -> bool {
        self.account.read().unwrap().is_none()
    }

    pub fn mobile(&self) -> bool {
        self.account
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|a| a.mobile)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Paused {
    service: String,
    port: u16,
    url: String,
}

#[derive(Default)]
pub struct Browser {
    tabs: Mutex<HashMap<u64, Arc<Tab>>>,
    next_id: AtomicU64,
    saved: Mutex<Saved>,
    /// Until the saved tabs are reopened, the tab list on disk is theirs and isn't overwritten.
    restored: AtomicBool,
}

#[derive(Clone, Serialize)]
pub struct TabView {
    id: u64,
    worker: String,
    project: String,
    account: Option<String>,
    username: Option<String>,
    color: Option<String>,
    mobile: bool,
    url: String,
    loading: bool,
    /// A login form for this account is on the page.
    can_sign_in: bool,
    login_fields: Vec<LoginField>,
    /// Only looked up while the page's login form has a password field.
    has_password: bool,
    can_fill_email: bool,
    status: Option<String>,
    paused: Option<Paused>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Browser {
    pub fn tab(&self, id: u64) -> CmdResult<Arc<Tab>> {
        self.tabs
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("tab {id} not found; it may have been closed"))
    }

    pub fn views(&self) -> Vec<TabView> {
        let mut tabs: Vec<_> = self.tabs.lock().unwrap().values().cloned().collect();
        tabs.sort_by_key(|t| t.id);
        tabs.iter().map(|t| view(t)).collect()
    }

    fn all_tabs(&self) -> Vec<Arc<Tab>> {
        let mut tabs: Vec<_> = self.tabs.lock().unwrap().values().cloned().collect();
        tabs.sort_by_key(|t| t.id);
        tabs
    }

    pub fn worker_tabs(&self, worker: &str) -> Vec<Arc<Tab>> {
        let mut tabs: Vec<_> = self
            .tabs
            .lock()
            .unwrap()
            .values()
            .filter(|t| t.worker == worker)
            .cloned()
            .collect();
        tabs.sort_by_key(|t| t.id);
        tabs
    }

    fn insert(&self, tab: Tab) -> Arc<Tab> {
        let tab = Arc::new(tab);
        self.tabs.lock().unwrap().insert(tab.id, tab.clone());
        tab
    }

    fn remove(&self, id: u64) -> Option<Arc<Tab>> {
        self.tabs.lock().unwrap().remove(&id)
    }

    fn update_saved(&self, change: impl FnOnce(&mut Saved)) {
        let mut saved = self.saved.lock().unwrap();
        let before = saved.clone();
        change(&mut saved);
        if *saved != before
            && let Some(path) = store::path()
            && let Err(e) = store::save(&path, &saved)
        {
            eprintln!("could not save the browser state: {e:#}");
        }
    }

    fn save_tabs(&self) {
        if !self.restored.load(Ordering::Acquire) {
            return;
        }
        let tabs = saved_tabs(&self.all_tabs());
        self.update_saved(|s| s.tabs = tabs);
    }

    fn kept_cookies(&self, profile: &str) -> Vec<cookies::CookieKey> {
        self.saved
            .lock()
            .unwrap()
            .profiles
            .get(profile)
            .map(|p| p.kept.clone())
            .unwrap_or_default()
    }

    fn set_kept_cookies(&self, profile: &str, kept: Vec<cookies::CookieKey>) {
        self.update_saved(|s| {
            if let Some(p) = s.profiles.get_mut(profile) {
                p.kept = kept;
            }
        });
    }
}

/// Anonymous tabs aren't reopened after a restart.
fn saved_tabs(tabs: &[Arc<Tab>]) -> Vec<SavedTab> {
    tabs.iter()
        .filter_map(|t| {
            Some(SavedTab {
                id: t.id,
                worker: t.worker.clone(),
                account: t.account.read().unwrap().as_ref()?.name.clone(),
                url: t
                    .paused
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map_or_else(|| t.events.state().url, |p| p.url.clone()),
            })
        })
        .collect()
}

fn view(tab: &Tab) -> TabView {
    let state = tab.events.state();
    let account = tab.account();
    let url = url::Url::parse(&state.url).ok();
    let login_fields = match (&account, &url, &state.login) {
        (Some(_), Some(url), Some(form)) if url.origin().ascii_serialization() == form.origin => {
            form.fields.clone()
        }
        _ => Vec::new(),
    };
    let has_password = account.as_ref().is_some_and(|account| {
        login_fields.contains(&LoginField::Password) && has_password(tab, account)
    });
    let (can_sign_in, can_fill_email) = match (&account, &url) {
        (Some(account), Some(url)) if !login_fields.is_empty() => (
            login::should_offer(account, url, || has_password),
            login::can_fill_email(account, url, &login_fields),
        ),
        _ => (false, false),
    };
    TabView {
        id: tab.id,
        worker: tab.worker.clone(),
        project: tab.project.clone(),
        mobile: account.as_ref().is_some_and(|a| a.mobile),
        username: account.as_ref().map(|a| a.username.clone()),
        account: account.map(|a| a.name),
        color: tab.color.clone(),
        url: state.url,
        loading: state.loading,
        can_sign_in,
        login_fields,
        has_password,
        can_fill_email,
        status: tab.status.lock().unwrap().clone(),
        paused: tab.paused.lock().unwrap().clone(),
    }
}

fn has_password(tab: &Tab, account: &Account) -> bool {
    tworky_core::accounts::has_password(&tab.project, &account.name)
}

pub fn changed(app: &AppHandle) {
    let browser = app.state::<Browser>();
    browser.save_tabs();
    let _ = app.emit("browser-tabs-changed", browser.views());
}

fn page<'a>(cdp: &'a host::TabCdp, tab: &'a Tab) -> Page<'a, host::TabCdp> {
    Page {
        cdp,
        events: &tab.events,
        mobile: tab.mobile(),
    }
}

/// A profile name per worker and account, so each signs in on its own.
fn profile_name(worker: &str, account: &str) -> String {
    valid_profile_name(format!("tw-{worker}-{account}"))
}

/// A fresh profile for one anonymous tab. The random part keeps a reused tab id from finding
/// an old profile's cookies.
fn anonymous_profile_name(worker: &str, id: u64) -> String {
    let random = uuid::Uuid::new_v4().simple().to_string();
    valid_profile_name(format!("ta-{worker}-{id}-{}", &random[..8]))
}

/// WebView2 allows up to 64 characters from a small set, so long names keep a stable hash of
/// the full name.
fn valid_profile_name(full: String) -> String {
    let clean: String = full
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.len() <= 64 {
        return clean;
    }
    let hash = full.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{}-{hash:016x}", &clean[..47])
}

fn browser_account(account: AccountConfig) -> Account {
    Account {
        name: account.name,
        username: account.username,
        start_url: account.start_url,
        login_origins: account.login_origins,
        mobile: account.mobile,
    }
}

pub fn check_open(account: Option<&str>, url: Option<&str>) -> CmdResult<()> {
    if account.is_none() && url.is_none_or(|u| u.trim().is_empty()) {
        return Err("an anonymous tab (no account) needs a url to open".into());
    }
    Ok(())
}

/// Opens a tab for one of the worker's accounts, or an anonymous one on a profile of its own
/// when `account` is `None`, prepared (login detector, mobile emulation) but not yet navigated.
pub async fn open_tab(app: &AppHandle, worker: &str, account: Option<&str>) -> CmdResult<Arc<Tab>> {
    open_tab_as(app, worker, account, None, None).await
}

async fn open_tab_as(
    app: &AppHandle,
    worker: &str,
    account: Option<&str>,
    id: Option<u64>,
    profile: Option<String>,
) -> CmdResult<Arc<Tab>> {
    let (project, found) = {
        let worker = worker.to_string();
        let wanted = account.map(str::to_string);
        tauri::async_runtime::spawn_blocking(move || {
            let ws = Workspace::open().map_err(|e| format!("{e:#}"))?;
            let record = ws
                .state
                .get(&worker)
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| format!("worker {worker} not found"))?;
            let Some(wanted) = wanted else {
                return Ok((record.project, None));
            };
            let accounts = ws.worker_accounts(&record).map_err(|e| format!("{e:#}"))?;
            let names: Vec<_> = accounts.iter().map(|a| a.name.clone()).collect();
            let found = accounts
                .into_iter()
                .find(|a| a.name.eq_ignore_ascii_case(wanted.trim()))
                .ok_or_else(|| {
                    if let Some(why) = ws.unusable_account(&record, &wanted) {
                        format!("{why}; fix it in Tworky's Projects › Accounts")
                    } else if names.is_empty() {
                        format!(
                            "project {} has no accounts; add them in Tworky's Projects › Accounts",
                            record.project
                        )
                    } else {
                        format!(
                            "no account called {wanted}; use one of: {}, or ask the user \
                             for it with request_account",
                            names.join(", ")
                        )
                    }
                })?;
            let color = found.color.clone();
            Ok::<_, String>((record.project, Some((browser_account(found), color))))
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let (account, color) = found.unzip();
    let browser = app.state::<Browser>();
    let id = id.unwrap_or_else(|| browser.next_id.fetch_add(1, Ordering::Relaxed) + 1);
    let anonymous = account.is_none();
    let profile = profile.unwrap_or_else(|| match &account {
        Some(account) => profile_name(worker, &account.name),
        None => anonymous_profile_name(worker, id),
    });
    let tab = browser.insert(Tab {
        id,
        worker: worker.to_string(),
        project,
        account: RwLock::new(account),
        profile: profile.clone(),
        color: color.flatten(),
        events: Arc::new(PageEvents::default()),
        status: Mutex::new(None),
        paused: Mutex::new(None),
    });
    browser.update_saved(|s| {
        s.next_id = s.next_id.max(id);
        s.profiles
            .entry(profile.clone())
            .or_insert_with(|| SavedProfile {
                worker: tab.worker.clone(),
                kept: Vec::new(),
                anonymous,
            });
    });
    let created = host::create(app, &tab, &profile).await;
    let prepared = match created {
        Ok(()) => page(&host::TabCdp::new(app, tab.id), &tab).prepare().await,
        Err(e) => Err(e),
    };
    if let Err(e) = prepared {
        close(app, tab.id).await;
        return Err(format!("could not open the tab: {e}"));
    }
    changed(app);
    Ok(tab)
}

/// Closing the last tab on an anonymous profile deletes the profile.
pub async fn close(app: &AppHandle, id: u64) {
    let browser = app.state::<Browser>();
    let Some(tab) = browser.remove(id) else {
        return;
    };
    host::destroy(app, id).await;
    changed(app);
    if tab.anonymous() && !browser.all_tabs().iter().any(|t| t.profile == tab.profile) {
        delete_profile(app, &tab.profile).await;
    }
}

/// The profile's tabs must be closed first.
async fn delete_profile(app: &AppHandle, profile: &str) {
    match host::delete_profile(app, profile).await {
        Ok(()) => app.state::<Browser>().update_saved(|s| {
            s.profiles.remove(profile);
        }),
        Err(e) => eprintln!("could not delete browser profile {profile}: {e}"),
    }
}

/// Closes the worker's tabs and deletes the browser profiles it used, with their sign-ins.
pub async fn forget_worker(app: &AppHandle, worker: &str) {
    for tab in app.state::<Browser>().worker_tabs(worker) {
        close(app, tab.id).await;
    }
    let profiles: Vec<String> = app
        .state::<Browser>()
        .saved
        .lock()
        .unwrap()
        .profiles
        .iter()
        .filter(|(_, p)| p.worker == worker)
        .map(|(name, _)| name.clone())
        .collect();
    for profile in profiles {
        delete_profile(app, &profile).await;
    }
}

/// Reads the saved state before anything can open a tab, so new tabs never reuse a saved id.
pub fn load(app: &AppHandle) {
    let saved = store::path().map(|p| store::load(&p)).unwrap_or_default();
    let browser = app.state::<Browser>();
    let last = saved.tabs.iter().map(|t| t.id).max().unwrap_or(0);
    browser
        .next_id
        .fetch_max(last.max(saved.next_id), Ordering::Relaxed);
    *browser.saved.lock().unwrap() = saved;
}

/// Reopens the tabs open when the app last closed, with their ids, so a handover's tab buttons
/// and Claude's tab ids still work.
pub async fn restore(app: AppHandle) {
    let browser = app.state::<Browser>();
    let tabs = browser.saved.lock().unwrap().tabs.clone();
    for saved in tabs {
        match open_tab_as(
            &app,
            &saved.worker,
            Some(&saved.account),
            Some(saved.id),
            None,
        )
        .await
        {
            Ok(tab) => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let cdp = host::TabCdp::new(&app, tab.id);
                    let start = target_url(&tab, None);
                    let url = store::restore_url(&saved.url, &start).to_string();
                    if pause_if_stopped(&app, &tab, &url).await {
                        return;
                    }
                    if let Err(e) = page(&cdp, &tab).navigate(&url).await {
                        eprintln!("tab {}: {e}", tab.id);
                    }
                });
            }
            Err(e) => eprintln!(
                "could not reopen {}'s {} tab: {e}",
                saved.worker, saved.account
            ),
        }
    }
    browser.restored.store(true, Ordering::Release);
    changed(&app);
    delete_anonymous_profiles(&app).await;
    delete_orphaned_profiles(&app).await;
}

/// Anonymous tabs aren't reopened, so their profiles are left over from the last run, or from a
/// close whose delete failed.
async fn delete_anonymous_profiles(app: &AppHandle) {
    let browser = app.state::<Browser>();
    let anonymous: Vec<String> = browser
        .saved
        .lock()
        .unwrap()
        .profiles
        .iter()
        .filter(|(_, p)| p.anonymous)
        .map(|(name, _)| name.clone())
        .collect();
    let in_use: Vec<String> = browser
        .all_tabs()
        .iter()
        .map(|t| t.profile.clone())
        .collect();
    for profile in anonymous.iter().filter(|p| !in_use.contains(p)) {
        delete_profile(app, profile).await;
    }
}

/// `tw worker rm` cleans up without the app, so its workers' profiles are deleted here.
async fn delete_orphaned_profiles(app: &AppHandle) {
    let workers = tauri::async_runtime::spawn_blocking(|| {
        Workspace::open()
            .and_then(|ws| ws.state.list())
            .map(|records| records.into_iter().map(|w| w.id).collect::<Vec<_>>())
    })
    .await;
    let Ok(Ok(workers)) = workers else {
        return;
    };
    let mut orphaned: Vec<String> = app
        .state::<Browser>()
        .saved
        .lock()
        .unwrap()
        .profiles
        .values()
        .filter(|p| !workers.contains(&p.worker))
        .map(|p| p.worker.clone())
        .collect();
    orphaned.dedup();
    for worker in orphaned {
        forget_worker(app, &worker).await;
    }
}

fn set_status(app: &AppHandle, tab: &Tab, status: Option<String>) {
    *tab.status.lock().unwrap() = status;
    changed(app);
}

/// Holds the tab back from `url` when the service serving it isn't running, so it offers to
/// start it rather than showing a connection error.
pub async fn pause_if_stopped(app: &AppHandle, tab: &Tab, url: &str) -> bool {
    let worker = tab.worker.clone();
    let target = url.to_string();
    let stopped = tauri::async_runtime::spawn_blocking(move || {
        Workspace::open().and_then(|ws| ws.stopped_service_for_url(&worker, &target))
    })
    .await;
    let Ok(Ok(Some((service, port)))) = stopped else {
        return false;
    };
    *tab.paused.lock().unwrap() = Some(Paused {
        service,
        port,
        url: url.to_string(),
    });
    changed(app);
    true
}

fn unpause(app: &AppHandle, tab: &Tab) {
    *tab.paused.lock().unwrap() = None;
    set_status(app, tab, None);
}

#[derive(Debug, PartialEq)]
enum Load {
    Page,
    Resume(String),
    Held(Paused),
}

/// A paused tab's document is blank, so reloading one navigates to the page it held back.
fn load_decision(
    paused: Option<Paused>,
    url: Option<&str>,
    stopped: impl FnOnce(&str) -> Option<(String, u16)>,
) -> Load {
    let Some(paused) = paused else {
        return Load::Page;
    };
    let url = url.map_or(paused.url, str::to_string);
    match stopped(&url) {
        Some((service, port)) => Load::Held(Paused { service, port, url }),
        None => Load::Resume(url),
    }
}

/// Navigates to `url`, or reloads without one. A paused tab loads only once the service serving
/// the page answers, and otherwise stays paused on it.
pub async fn load_page(app: &AppHandle, tab: &Tab, url: Option<&str>) -> CmdResult<Value> {
    let paused = tab.paused.lock().unwrap().clone();
    let worker = tab.worker.clone();
    let target = url.map(str::to_string);
    let decision = tauri::async_runtime::spawn_blocking(move || {
        load_decision(paused, target.as_deref(), |target| {
            Workspace::open()
                .ok()?
                .stopped_service_for_url(&worker, target)
                .ok()
                .flatten()
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    let cdp = host::TabCdp::new(app, tab.id);
    let page = page(&cdp, tab);
    match decision {
        Load::Page => match url {
            Some(url) => page.navigate(url).await,
            None => page.reload().await,
        },
        Load::Resume(url) => {
            unpause(app, tab);
            page.navigate(&url).await
        }
        Load::Held(paused) => {
            let message = format!(
                "{} isn't running (nothing is listening on port {}); start it, then load the page again",
                paused.service, paused.port
            );
            *tab.paused.lock().unwrap() = Some(paused);
            changed(app);
            Err(message)
        }
    }
}

/// Starts the project service that serves `url`, if any and not already running, with the
/// services it needs, and waits for each to listen. The tab shows progress, and the error if it
/// fails.
pub async fn start_service_for(
    app: &AppHandle,
    tab: &Arc<Tab>,
    url: &str,
) -> CmdResult<Vec<(String, Outcome)>> {
    let (app, tab) = (app.clone(), tab.clone());
    let url = url.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut shown = false;
        let started = (|| {
            let ws = Workspace::open()?;
            let record = ws
                .state
                .get(&tab.worker)?
                .ok_or_else(|| anyhow::anyhow!("worker {} not found", tab.worker))?;
            let Some(name) = ws.service_for_url(&record, &url) else {
                return Ok(Vec::new());
            };
            if ws.stopped_service_for_url(&tab.worker, &url)?.is_some() {
                set_status(&app, &tab, Some(format!("Starting {name}…")));
                shown = true;
            }
            ws.start_service(&tab.worker, &name, true, &mut |line| {
                set_status(&app, &tab, Some(line.to_string()));
                shown = true;
            })
        })();
        match started {
            Ok(outcomes) => {
                if shown {
                    set_status(&app, &tab, None);
                }
                Ok(outcomes)
            }
            Err(e) => {
                let e = format!("{e:#}");
                set_status(&app, &tab, Some(e.clone()));
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r)
}

/// Session cookies would end with the app, signing every account out, so each profile's are
/// given a long expiry and renewed while it's in use.
async fn keep_cookies(app: &AppHandle, tab: &Tab) {
    if tab.anonymous() {
        return;
    }
    let browser = app.state::<Browser>();
    let kept = browser.kept_cookies(&tab.profile);
    match cookies::persist_session_cookies(&host::TabCdp::new(app, tab.id), &kept).await {
        Ok(kept) => browser.set_kept_cookies(&tab.profile, kept),
        Err(e) => eprintln!("tab {}: could not keep its cookies: {e}", tab.id),
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn page_loaded(app: &AppHandle, id: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Ok(tab) = app.state::<Browser>().tab(id) {
            keep_cookies(&app, &tab).await;
        }
    });
}

/// Cookies set by script after a load, such as a token exchange, are kept within a minute.
pub async fn keep_cookies_periodically(app: AppHandle) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut seen = Vec::new();
        let tabs: Vec<_> = app
            .state::<Browser>()
            .all_tabs()
            .into_iter()
            .filter(|t| {
                let first = !seen.contains(&t.profile);
                seen.push(t.profile.clone());
                first
            })
            .collect();
        for tab in tabs {
            keep_cookies(&app, &tab).await;
        }
    }
}

/// Opens a popup the page asked for as another tab of the same worker, account and profile.
#[cfg_attr(not(windows), allow(dead_code))]
fn open_popup(app: &AppHandle, opener: u64, url: String) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Ok(from) = app.state::<Browser>().tab(opener) else {
            return;
        };
        let account = from.account().map(|a| a.name);
        match open_tab_as(
            &app,
            &from.worker,
            account.as_deref(),
            None,
            Some(from.profile.clone()),
        )
        .await
        {
            Ok(tab) => {
                from.events.opened_tab(tab.id);
                let cdp = host::TabCdp::new(&app, tab.id);
                let _ = page(&cdp, &tab).navigate(&url).await;
            }
            Err(e) => eprintln!("could not open a popup tab: {e}"),
        }
    });
}

#[tauri::command]
pub fn browser_tabs(app: AppHandle) -> Vec<TabView> {
    app.state::<Browser>().views()
}

#[tauri::command]
pub async fn browser_open(
    app: AppHandle,
    worker: String,
    account: Option<String>,
    url: Option<String>,
) -> CmdResult<u64> {
    check_open(account.as_deref(), url.as_deref())?;
    let tab = open_tab(&app, &worker, account.as_deref()).await?;
    let id = tab.id;
    tauri::async_runtime::spawn(async move {
        let target = target_url(&tab, url);
        if start_service_for(&app, &tab, &target).await.is_err() {
            if pause_if_stopped(&app, &tab, &target).await {
                return;
            }
            set_status(&app, &tab, None);
        }
        let cdp = host::TabCdp::new(&app, tab.id);
        if let Err(e) = page(&cdp, &tab).navigate(&target).await {
            eprintln!("tab {}: {e}", tab.id);
        }
    });
    Ok(id)
}

#[tauri::command]
pub async fn browser_start_service(app: AppHandle, tab: u64) -> CmdResult<()> {
    let tab = app.state::<Browser>().tab(tab)?;
    let Some(url) = paused_url(&tab) else {
        return Ok(());
    };
    if start_service_for(&app, &tab, &url).await.is_ok() && paused_url(&tab).is_some() {
        load_page(&app, &tab, None).await?;
    }
    Ok(())
}

fn paused_url(tab: &Tab) -> Option<String> {
    tab.paused.lock().unwrap().as_ref().map(|p| p.url.clone())
}

/// Loads a held-back page once its service is up, however it was started.
#[tauri::command]
pub async fn browser_retry(app: AppHandle, tab: u64) -> CmdResult<()> {
    let tab = app.state::<Browser>().tab(tab)?;
    if paused_url(&tab).is_none() {
        return Ok(());
    }
    load_page(&app, &tab, None).await.map(|_| ())
}

#[tauri::command]
pub async fn browser_navigate(app: AppHandle, tab: u64, url: String) -> CmdResult<Value> {
    let tab = app.state::<Browser>().tab(tab)?;
    load_page(&app, &tab, Some(&url)).await
}

#[tauri::command]
pub async fn browser_history(app: AppHandle, tab: u64, action: String) -> CmdResult<()> {
    app.state::<Browser>().tab(tab)?;
    host::history(&app, tab, &action).await
}

#[tauri::command]
pub async fn browser_bounds(app: AppHandle, tab: u64, bounds: Bounds) -> CmdResult<()> {
    host::set_bounds(&app, tab, bounds).await
}

/// Raises `tab` above the UI, or lowers every tab when none is shown. `load` names the UI load,
/// and `seq` rises with each call within it.
#[tauri::command]
pub async fn browser_show(app: AppHandle, tab: Option<u64>, load: u64, seq: u64) -> CmdResult<()> {
    host::show(&app, tab, load, seq).await
}

/// The tab the UI last asked to show. Each show posts to the main thread on its own, so they
/// can arrive out of order, and a tab can be asked for before its view exists.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Shown {
    load: u64,
    seq: u64,
    pub tab: Option<u64>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Shown {
    /// False for a call older than one already applied from the same UI load. A call from another
    /// load always applies, so a reloaded UI is never ignored.
    pub fn update(&mut self, load: u64, seq: u64, tab: Option<u64>) -> bool {
        if load == self.load && seq < self.seq {
            return false;
        }
        self.load = load;
        self.seq = seq;
        self.tab = tab;
        true
    }
}

#[tauri::command]
pub async fn browser_close(app: AppHandle, tab: u64) -> CmdResult<()> {
    close(&app, tab).await;
    Ok(())
}

pub fn target_url(tab: &Tab, url: Option<String>) -> String {
    url.or_else(|| tab.account().map(|a| a.start_url))
        .unwrap_or_default()
}

async fn sign_in(app: &AppHandle, tab: &Tab, submit: bool) -> CmdResult<Value> {
    let Some(account) = tab.account() else {
        return Err(format!(
            "tab {} is anonymous, so it has no account to sign in as; open a tab as one of the project's accounts to sign in",
            tab.id
        ));
    };
    let result = page(&host::TabCdp::new(app, tab.id), tab)
        .sign_in(
            &account,
            submit,
            || has_password(tab, &account),
            || {
                tworky_core::accounts::password(&tab.project, &account.name)
                    .map_err(|e| format!("{e:#}"))
            },
        )
        .await;
    changed(app);
    result
}

/// Fills the login form without submitting it, so the user can check it first.
#[tauri::command]
pub async fn browser_sign_in(app: AppHandle, tab: u64) -> CmdResult<Value> {
    let tab = app.state::<Browser>().tab(tab)?;
    sign_in(&app, &tab, false).await
}

#[derive(Clone, Serialize)]
struct LoginOriginAdded {
    project: String,
    account: String,
    origin: String,
}

/// With `persist`, also saves the page's origin as a login origin and refreshes the account in
/// every tab using it, across the project's workers. An origin that can't be saved so it allows
/// this page is refused before anything is filled or written. `origin` is the one the user
/// confirmed; a page that has left it is refused.
#[tauri::command]
pub async fn browser_fill_email(
    app: AppHandle,
    tab: u64,
    persist: bool,
    origin: Option<String>,
) -> CmdResult<String> {
    let tab = app.state::<Browser>().tab(tab)?;
    let Some(account) = tab.account() else {
        return Err("an anonymous tab has no account email to fill".into());
    };
    let state = tab.events.state();
    let url = url::Url::parse(&state.url).map_err(|_| "the page has no address yet".to_string())?;
    let page_origin = url.origin().ascii_serialization();
    let fields = state
        .login
        .filter(|form| form.origin == page_origin)
        .map(|form| form.fields)
        .unwrap_or_default();
    if !login::can_fill_email(&account, &url, &fields) {
        return Err(format!(
            "there's no username field to fill with {}'s email on this page",
            account.name
        ));
    }
    if origin.as_ref().is_some_and(|o| *o != page_origin) {
        return Err(format!(
            "the page moved to {page_origin}, so its origin wasn't saved"
        ));
    }
    let record = if persist {
        let worker = tab.worker.clone();
        let origin = page_origin.clone();
        let record = tauri::async_runtime::spawn_blocking(move || {
            let ws = Workspace::open().map_err(|e| format!("{e:#}"))?;
            let record = ws
                .state
                .get(&worker)
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| format!("worker {worker} not found"))?;
            ws.login_origin_for_worker(&record, &origin)
                .map_err(|e| format!("{e:#}"))?;
            Ok::<_, String>(record)
        })
        .await
        .map_err(|e| e.to_string())??;
        Some(record)
    } else {
        None
    };
    let cdp = host::TabCdp::new(&app, tab.id);
    tworky_browser::cdp::evaluate(
        &cdp,
        &login::fill_script(Some(account.username.trim()), None, false),
    )
    .await?;
    if let Some(record) = record {
        let held_as = |t: &Tab| {
            t.project == tab.project
                && t.account
                    .read()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|a| a.name == account.name)
        };
        let tabs: Vec<_> = app
            .state::<Browser>()
            .all_tabs()
            .into_iter()
            .filter(|t| held_as(t))
            .collect();
        let mut workers: Vec<String> = tabs.iter().map(|t| t.worker.clone()).collect();
        workers.sort();
        workers.dedup();
        let name = account.name.clone();
        let origin = page_origin.clone();
        let (saved, refreshed) = tauri::async_runtime::spawn_blocking(move || {
            let saved = Workspace::open()
                .and_then(|ws| ws.add_worker_login_origin(&record, &name, &origin))
                .map_err(|e| format!("{e:#}"))?;
            let ws = Workspace::open().map_err(|e| format!("{e:#}"))?;
            let refreshed: HashMap<String, Account> = workers
                .into_iter()
                .filter_map(|worker| {
                    let record = ws.state.get(&worker).ok()??;
                    let account = ws
                        .worker_accounts(&record)
                        .ok()?
                        .into_iter()
                        .find(|a| a.name == name)?;
                    Some((worker, browser_account(account)))
                })
                .collect();
            Ok::<_, String>((saved, refreshed))
        })
        .await
        .map_err(|e| e.to_string())??;
        for t in tabs {
            if let Some(refreshed) = refreshed.get(&t.worker) {
                let mut held = t.account.write().unwrap();
                if held.as_ref().is_some_and(|a| a.name == account.name) {
                    *held = Some(refreshed.clone());
                }
            }
        }
        let _ = app.emit(
            "accounts-changed",
            LoginOriginAdded {
                project: tab.project.clone(),
                account: account.name.clone(),
                origin: saved,
            },
        );
    }
    changed(&app);
    Ok(page_origin)
}

#[tauri::command]
pub async fn browser_end_session(app: AppHandle, tab: u64) -> CmdResult<Value> {
    let browser = app.state::<Browser>();
    let tab = browser.tab(tab)?;
    if tab.anonymous() {
        return Err("an anonymous tab has no session to end; close the tab to discard it".into());
    }
    let cdp = host::TabCdp::new(&app, tab.id);
    cookies::end_session(&cdp, &browser.kept_cookies(&tab.profile)).await?;
    browser.set_kept_cookies(&tab.profile, Vec::new());
    page(&cdp, &tab).reload().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: u64, account: Option<&str>) -> Arc<Tab> {
        let account = account.map(|name| Account {
            name: name.into(),
            username: String::new(),
            start_url: "http://localhost:5273/".into(),
            login_origins: Vec::new(),
            mobile: false,
        });
        let events = PageEvents::default();
        events.load_finished("http://localhost:5273/clients");
        Arc::new(Tab {
            id,
            worker: "w".into(),
            project: "p".into(),
            profile: match &account {
                Some(a) => profile_name("w", &a.name),
                None => anonymous_profile_name("w", id),
            },
            account: RwLock::new(account),
            color: None,
            events: Arc::new(events),
            status: Mutex::new(None),
            paused: Mutex::new(None),
        })
    }

    #[test]
    fn anonymous_tabs_are_not_saved() {
        let saved = saved_tabs(&[tab(1, Some("Tester 1")), tab(2, None)]);
        assert_eq!(
            saved,
            vec![SavedTab {
                id: 1,
                worker: "w".into(),
                account: "Tester 1".into(),
                url: "http://localhost:5273/clients".into(),
            }]
        );
    }

    #[test]
    fn anonymous_profiles_are_fresh_and_valid() {
        let worker = "acme-123-a-long-issue-title-slug-for-this-particular-worker";
        let first = anonymous_profile_name(worker, 41);
        assert_ne!(first, anonymous_profile_name(worker, 41));
        assert!(first.starts_with("ta-acme-123-"));
        assert_eq!(first.len(), 64);
        let short = anonymous_profile_name("w 1", 3);
        assert!(short.starts_with("ta-w_1-3-"), "{short}");
        assert_eq!(short.len(), "ta-w_1-3-".len() + 8);
        assert!(profile_name("w", "a").starts_with("tw-"));
    }

    #[test]
    fn an_anonymous_tab_needs_a_url() {
        assert!(check_open(Some("Tester 1"), None).is_ok());
        assert!(check_open(None, Some("http://localhost:5273/")).is_ok());
        assert!(check_open(None, None).is_err());
        assert!(check_open(None, Some(" ")).is_err());
    }

    fn paused(url: &str) -> Paused {
        Paused {
            service: "web".into(),
            port: 5273,
            url: url.into(),
        }
    }

    const HELD: &str = "http://localhost:5273/clients";
    const TARGET: &str = "http://localhost:5273/settings";

    #[test]
    fn navigating_a_paused_tab_resumes_it_once_its_service_answers() {
        let decision = load_decision(Some(paused(HELD)), Some(TARGET), |url| {
            assert_eq!(url, TARGET);
            None
        });
        assert_eq!(decision, Load::Resume(TARGET.into()));
    }

    #[test]
    fn navigating_a_paused_tab_while_its_service_is_down_holds_the_new_url() {
        let decision = load_decision(Some(paused(HELD)), Some(TARGET), |_| {
            Some(("web".into(), 5273))
        });
        assert_eq!(decision, Load::Held(paused(TARGET)));
    }

    #[test]
    fn reloading_a_paused_tab_loads_the_page_it_held_back() {
        let decision = load_decision(Some(paused(HELD)), None, |url| {
            assert_eq!(url, HELD);
            None
        });
        assert_eq!(decision, Load::Resume(HELD.into()));
    }

    #[test]
    fn reloading_a_paused_tab_while_its_service_is_down_stays_paused() {
        let decision = load_decision(Some(paused(HELD)), None, |_| Some(("web".into(), 5273)));
        assert_eq!(decision, Load::Held(paused(HELD)));
    }

    #[test]
    fn a_tab_that_is_not_paused_loads_without_checking_services() {
        let decision = load_decision(None, Some(TARGET), |_| panic!("checked a service"));
        assert_eq!(decision, Load::Page);
    }

    #[test]
    fn a_stale_show_is_ignored() {
        let mut shown = Shown::default();
        assert!(shown.update(7, 10, Some(1)));
        assert!(shown.update(7, 12, Some(2)));
        assert!(!shown.update(7, 11, None));
        assert_eq!(shown.tab, Some(2));
        assert!(shown.update(7, 13, None));
        assert_eq!(shown.tab, None);
    }

    #[test]
    fn a_reloaded_ui_is_shown_whatever_its_first_seq() {
        let mut shown = Shown::default();
        assert!(shown.update(7, 500, Some(1)));
        assert!(shown.update(8, 2, Some(2)));
        assert_eq!(shown.tab, Some(2));
        assert!(!shown.update(8, 1, None));
        assert!(shown.update(8, 3, Some(3)));
        assert_eq!(shown.tab, Some(3));
    }
}
