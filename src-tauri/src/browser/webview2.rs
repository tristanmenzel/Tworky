use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager};
use tokio::sync::oneshot;
use tworky_browser::cdp::Cdp;
use tworky_browser::login::{BINDING, LoginMessage};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2_13, ICoreWebView2Profile2, ICoreWebView2Profile8,
    ICoreWebView2Settings4,
};
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, ClearBrowsingDataCompletedHandler,
    DevToolsProtocolEventReceivedEventHandler, SourceChangedEventHandler, take_pwstr,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BOTTOM, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
};
use windows::core::{HSTRING, Interface, PWSTR, w};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{
    NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows, WebViewExtWindows,
};

use super::{Bounds, Shown, Tab};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Each tab is a WebView2 child of the main window. A hidden tab is never made invisible,
/// because a WebView2 created hidden never loads and screenshots of one hang. It is stacked
/// beneath the Tauri webview instead.
#[derive(Default)]
struct Host {
    context: Option<WebContext>,
    views: HashMap<u64, WebView>,
    last_bounds: Option<Bounds>,
    shown: Shown,
}

thread_local! {
    static HOST: RefCell<Host> = RefCell::new(Host::default());
}

fn rect(b: Bounds) -> Rect {
    Rect {
        position: LogicalPosition::new(b.x, b.y).into(),
        size: LogicalSize::new(b.width.max(1.0), b.height.max(1.0)).into(),
    }
}

async fn on_main<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce(&AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.send(f(&handle));
    })
    .map_err(|e| e.to_string())?;
    rx.await
        .map_err(|_| "the main thread dropped the call".to_string())?
}

#[derive(Clone)]
pub struct TabCdp {
    app: AppHandle,
    tab: u64,
}

impl TabCdp {
    pub fn new(app: &AppHandle, tab: u64) -> Self {
        TabCdp {
            app: app.clone(),
            tab,
        }
    }
}

impl Cdp for TabCdp {
    fn call(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, String>> + Send {
        let (tx, rx) = oneshot::channel();
        let name = method.to_string();
        let method = method.to_string();
        let tab = self.tab;
        let sent = self.app.run_on_main_thread(move || {
            HOST.with(|h| match h.borrow().views.get(&tab) {
                Some(view) => call_raw(&view.webview(), &method, &params, move |result| {
                    let _ = tx.send(result);
                }),
                None => {
                    let _ = tx.send(Err(format!("tab {tab} is closed")));
                }
            })
        });
        async move {
            sent.map_err(|e| e.to_string())?;
            match tokio::time::timeout(CALL_TIMEOUT, rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(format!("tab {tab} closed during {name}")),
                Err(_) => Err(format!("{name} got no answer in {CALL_TIMEOUT:?}")),
            }
        }
    }
}

fn call_raw(
    webview: &ICoreWebView2,
    method: &str,
    params: &Value,
    on_done: impl FnOnce(Result<Value, String>) + 'static,
) {
    let on_done = Rc::new(RefCell::new(Some(on_done)));
    let fallback = on_done.clone();
    let name = method.to_string();
    let handler =
        CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |result, json| {
            let outcome = result
                .map_err(|error| {
                    let detail = serde_json::from_str::<Value>(&json)
                        .ok()
                        .and_then(|body| body.get("message")?.as_str().map(str::to_string))
                        .unwrap_or_else(|| error.to_string());
                    format!("{name} failed: {detail}")
                })
                .and_then(|()| {
                    serde_json::from_str::<Value>(&json)
                        .map_err(|error| format!("{name} returned invalid JSON: {error}"))
                });
            if let Some(on_done) = on_done.borrow_mut().take() {
                on_done(outcome);
            }
            Ok(())
        }));
    let requested = unsafe {
        webview.CallDevToolsProtocolMethod(
            &HSTRING::from(method),
            &HSTRING::from(params.to_string()),
            &handler,
        )
    };
    if let Err(error) = requested
        && let Some(on_done) = fallback.borrow_mut().take()
    {
        on_done(Err(format!("{method} could not be sent: {error}")));
    }
}

fn stack(view: &WebView, top: bool) {
    unsafe {
        let _ = SetWindowPos(
            view.hwnd(),
            Some(if top { HWND_TOP } else { HWND_BOTTOM }),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

fn data_dir() -> Result<std::path::PathBuf, String> {
    Ok(tworky_core::config::Paths::resolve()
        .map_err(|e| format!("{e:#}"))?
        .data_dir
        .join("browser")
        .join("webview"))
}

/// A child webview of the main window on `profile`, stacked beneath the UI.
fn build(
    app: &AppHandle,
    dir: std::path::PathBuf,
    profile: String,
    configure: impl for<'a> FnOnce(WebViewBuilder<'a>) -> WebViewBuilder<'a>,
) -> Result<WebView, String> {
    let window = app.get_webview_window("main").ok_or("no main window")?;
    // Building pumps messages, so other main-thread work can run meanwhile; the host isn't
    // borrowed across it.
    let (context, bounds) = HOST.with(|h| {
        let mut h = h.borrow_mut();
        (h.context.take(), h.last_bounds)
    });
    let mut context = context.unwrap_or_else(|| WebContext::new(Some(dir)));
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_profile_name(profile)
        .with_bounds(rect(bounds.unwrap_or(Bounds {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 800.0,
        })));
    let built = configure(builder).build_as_child(&window);
    HOST.with(|h| {
        h.borrow_mut().context.get_or_insert(context);
    });
    let view = built.map_err(|e| e.to_string())?;
    stack(&view, false);
    Ok(view)
}

pub async fn create(app: &AppHandle, tab: &Arc<Tab>, profile: &str) -> Result<(), String> {
    let dir = data_dir()?;
    let tab = tab.clone();
    let profile = profile.to_string();
    on_main(app, move |app| {
        let id = tab.id;
        let load_app = app.clone();
        let load_events = tab.events.clone();
        let popup_app = app.clone();
        let view = build(app, dir, profile, |builder| {
            builder
                .with_general_autofill_enabled(false)
                .with_devtools(cfg!(debug_assertions))
                .with_on_page_load_handler(move |event, url| {
                    match event {
                        PageLoadEvent::Started => load_events.load_started(&url),
                        PageLoadEvent::Finished => {
                            load_events.load_finished(&url);
                            super::page_loaded(&load_app, id);
                        }
                    }
                    super::changed(&load_app);
                })
                .with_new_window_req_handler(move |url, _| {
                    super::open_popup(&popup_app, id, url);
                    NewWindowResponse::Deny
                })
        })?;
        let core = view.webview();
        disable_password_autosave(&core);
        watch_source(&core, &view, &tab);
        watch_login(app, &core, &tab)?;
        HOST.with(|h| {
            let mut h = h.borrow_mut();
            stack(&view, h.shown.tab == Some(id));
            h.views.insert(id, view);
        });
        Ok(())
    })
    .await
}

fn disable_password_autosave(core: &ICoreWebView2) {
    unsafe {
        if let Ok(settings) = core.Settings()
            && let Ok(settings4) = settings.cast::<ICoreWebView2Settings4>()
        {
            let _ = settings4.SetIsPasswordAutosaveEnabled(false);
        }
    }
}

/// Same-document navigations change the URL without a page load.
fn watch_source(core: &ICoreWebView2, view: &WebView, tab: &Arc<Tab>) {
    let events = tab.events.clone();
    let source = view.webview();
    let handler = SourceChangedEventHandler::create(Box::new(move |_, _| {
        let mut uri = PWSTR::null();
        if unsafe { source.Source(&mut uri) }.is_ok() {
            events.url_changed(&take_pwstr(uri));
        }
        Ok(())
    }));
    let mut token = 0;
    unsafe {
        let _ = core.add_SourceChanged(&handler, &mut token);
    }
}

/// The login detector reports through a CDP binding, which arrives as `Runtime.bindingCalled`.
fn watch_login(app: &AppHandle, core: &ICoreWebView2, tab: &Arc<Tab>) -> Result<(), String> {
    let app = app.clone();
    let events = tab.events.clone();
    let id = tab.id;
    let handler = DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut json = PWSTR::null();
        unsafe { args.ParameterObjectAsJson(&mut json)? };
        let Ok(call) = serde_json::from_str::<Value>(&take_pwstr(json)) else {
            return Ok(());
        };
        if call["name"] != BINDING {
            return Ok(());
        }
        let message = events.login_message(call["payload"].as_str().unwrap_or_default());
        if message == Some(LoginMessage::FillRequest) {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = super::browser_sign_in(app, id).await;
            });
        }
        super::changed(&app);
        Ok(())
    }));
    let mut token = 0;
    unsafe {
        core.GetDevToolsProtocolEventReceiver(w!("Runtime.bindingCalled"))
            .and_then(|receiver| receiver.add_DevToolsProtocolEventReceived(&handler, &mut token))
            .map_err(|e| format!("could not watch for login forms: {e}"))
    }
}

pub async fn destroy(app: &AppHandle, id: u64) {
    let _ = on_main(app, move |_| {
        let view = HOST.with(|h| h.borrow_mut().views.remove(&id));
        drop(view);
        Ok(())
    })
    .await;
}

/// Clears and deletes a profile's data through a webview of its own, as WebView2 offers no
/// other way. Its tabs must be closed first.
pub async fn delete_profile(app: &AppHandle, profile: &str) -> Result<(), String> {
    let dir = data_dir()?;
    let profile = profile.to_string();
    on_main(app, move |app| {
        let view = build(app, dir, profile, |builder| builder)?;
        let deleted = unsafe {
            view.webview()
                .cast::<ICoreWebView2_13>()
                .and_then(|core| core.Profile())
                .map_err(webview2_com::Error::from)
                .and_then(|profile| {
                    let clearable = profile.cast::<ICoreWebView2Profile2>()?;
                    ClearBrowsingDataCompletedHandler::wait_for_async_operation(
                        Box::new(move |handler| {
                            clearable.ClearBrowsingDataAll(&handler).map_err(Into::into)
                        }),
                        Box::new(|_| Ok(())),
                    )?;
                    if let Ok(deletable) = profile.cast::<ICoreWebView2Profile8>() {
                        deletable.Delete()?;
                    }
                    Ok(())
                })
        };
        drop(view);
        deleted.map_err(|e| e.to_string())
    })
    .await
}

pub async fn set_bounds(app: &AppHandle, id: u64, bounds: Bounds) -> Result<(), String> {
    on_main(app, move |_| {
        HOST.with(|h| {
            let mut h = h.borrow_mut();
            h.last_bounds = Some(bounds);
            match h.views.get(&id) {
                Some(view) => view.set_bounds(rect(bounds)).map_err(|e| e.to_string()),
                None => Ok(()),
            }
        })
    })
    .await
}

pub async fn show(app: &AppHandle, shown: Option<u64>, load: u64, seq: u64) -> Result<(), String> {
    on_main(app, move |app| {
        let Some(focused) = HOST.with(|h| {
            let mut h = h.borrow_mut();
            if !h.shown.update(load, seq, shown) {
                return None;
            }
            for (id, view) in &h.views {
                stack(view, Some(*id) == shown);
            }
            Some(
                shown
                    .and_then(|id| h.views.get(&id))
                    .map(|view| view.focus().is_ok()),
            )
        }) else {
            return Ok(());
        };
        if focused.is_none()
            && let Some(window) = app.get_webview_window("main")
        {
            let _ = AsRef::<tauri::Webview>::as_ref(&window).set_focus();
        }
        Ok(())
    })
    .await
}

pub async fn history(app: &AppHandle, id: u64, action: &str) -> Result<(), String> {
    let action = action.to_string();
    on_main(app, move |_| {
        HOST.with(|h| {
            let h = h.borrow();
            let view = h.views.get(&id).ok_or("the tab is closed")?;
            let core = view.webview();
            unsafe {
                match action.as_str() {
                    "back" => core.GoBack(),
                    "forward" => core.GoForward(),
                    "reload" => core.Reload(),
                    other => return Err(format!("unknown history action {other}")),
                }
            }
            .map_err(|e| e.to_string())
        })
    })
    .await
}
