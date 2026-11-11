use serde::{Deserialize, Serialize};
use serde_json::json;
use url::{Origin, Url};

use crate::cdp::Cdp;

/// The detector runs in this isolated world, so page scripts can't see or replace it.
pub const WORLD: &str = "tworky";
/// The binding the detector reports through; `login_detect.js` calls it by this name.
pub const BINDING: &str = "__tworkyLogin";

const FINDER_SOURCE: &str = include_str!("js/login_detect.js");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginField {
    Username,
    Password,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LoginMessage {
    LoginForm {
        fields: Vec<LoginField>,
    },
    LoginFormGone,
    /// The user pressed Ctrl+Shift+L in the page.
    FillRequest,
}

impl LoginMessage {
    pub fn parse(payload: &str) -> Option<LoginMessage> {
        serde_json::from_str(payload).ok()
    }
}

/// A test account as one worker sees it: URLs already moved to the worker's ports.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub name: String,
    pub username: String,
    pub start_url: String,
    pub login_origins: Vec<String>,
    pub mobile: bool,
}

impl Account {
    pub fn allowed_origins(&self) -> Vec<Origin> {
        std::iter::once(&self.start_url)
            .chain(&self.login_origins)
            .filter_map(|s| Url::parse(s.trim()).ok())
            .map(|url| url.origin())
            .filter(Origin::is_tuple)
            .collect()
    }

    pub fn allows(&self, url: &Url) -> bool {
        let origin = url.origin();
        origin.is_tuple() && self.allowed_origins().contains(&origin)
    }
}

pub fn should_offer(account: &Account, url: &Url, has_password: impl FnOnce() -> bool) -> bool {
    account.allows(url) && (!account.username.trim().is_empty() || has_password())
}

/// Only for an account with no login origins, which in practice is a profile made on the fly.
pub fn can_fill_email(account: &Account, url: &Url, fields: &[LoginField]) -> bool {
    account.login_origins.is_empty()
        && !account.username.trim().is_empty()
        && fields.contains(&LoginField::Username)
        && url.origin().is_tuple()
        && !account.allows(url)
}

pub fn detect_script() -> String {
    format!("({FINDER_SOURCE}\n)(\"detect\")")
}

pub fn fill_script(username: Option<&str>, password: Option<&str>, submit: bool) -> String {
    let payload = json!({ "username": username, "password": password, "submit": submit });
    format!("({FINDER_SOURCE}\n)(\"fill\", {payload})")
}

/// Runs the detector in every new top-level document from now on, and in the current one.
/// The host then gets `Runtime.bindingCalled` events named [`BINDING`].
pub async fn install_detector(cdp: &impl Cdp) -> Result<(), String> {
    cdp.call("Runtime.enable", json!({})).await?;
    // Without it, WebView2 runs the script added below only in the current document, not in the
    // documents later navigations create (an OAuth redirect to the sign-in page, say).
    cdp.call("Page.enable", json!({})).await?;
    cdp.call(
        "Runtime.addBinding",
        json!({ "name": BINDING, "executionContextName": WORLD }),
    )
    .await?;
    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": detect_script(), "worldName": WORLD, "runImmediately": true }),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(username: &str) -> Account {
        Account {
            name: "Admin".into(),
            username: username.into(),
            start_url: "https://app.example.com/login".into(),
            login_origins: vec!["https://sso.example.com".into()],
            mobile: false,
        }
    }

    fn offered(account: &Account, url: &str, has_password: bool) -> bool {
        should_offer(account, &Url::parse(url).unwrap(), || has_password)
    }

    fn payload(script: &str) -> serde_json::Value {
        let start = script.rfind("(\"fill\", ").unwrap() + "(\"fill\", ".len();
        let json = script[start..].strip_suffix(')').unwrap();
        serde_json::from_str(json).unwrap()
    }

    #[tokio::test]
    async fn the_detector_is_installed_for_later_documents_too() {
        let cdp = crate::cdp::fake::FakeCdp::default();
        install_detector(&cdp).await.unwrap();
        let methods = cdp.methods();
        let at = |m: &str| methods.iter().position(|x| x == m).unwrap();
        assert!(
            at("Page.enable") < at("Page.addScriptToEvaluateOnNewDocument"),
            "{methods:?}"
        );
    }

    #[test]
    fn the_detector_reports_through_the_binding() {
        assert!(FINDER_SOURCE.contains(&format!("window.{BINDING}")));
    }

    #[test]
    fn parses_detector_messages() {
        assert_eq!(
            LoginMessage::parse(r#"{"type":"login-form","fields":["username","password"]}"#),
            Some(LoginMessage::LoginForm {
                fields: vec![LoginField::Username, LoginField::Password]
            })
        );
        assert_eq!(
            LoginMessage::parse(r#"{"type":"login-form-gone"}"#),
            Some(LoginMessage::LoginFormGone)
        );
        assert_eq!(LoginMessage::parse("nonsense"), None);
    }

    #[test]
    fn fill_script_round_trips_awkward_values() {
        let nasty = "a\"b\\c</script>\nd\u{2028}e\u{2029}f\u{1F511}'`${x}";
        let script = fill_script(Some(nasty), Some(nasty), false);
        let value = payload(&script);
        assert_eq!(value["username"], nasty);
        assert_eq!(value["password"], nasty);
        assert!(!script.contains(nasty));
    }

    #[test]
    fn fill_script_keeps_values_out_of_the_source_text() {
        let script = fill_script(Some("x\ny"), None, false);
        assert!(script.contains(r#""username":"x\ny""#));
        assert!(!script.contains("x\ny"));
    }

    #[test]
    fn fill_script_handles_partial_credentials() {
        let username_only = payload(&fill_script(Some("tom"), None, false));
        assert_eq!(username_only["username"], "tom");
        assert!(username_only["password"].is_null());
        let password_only = payload(&fill_script(None, Some("pw"), false));
        assert!(password_only["username"].is_null());
        assert_eq!(password_only["password"], "pw");
    }

    #[test]
    fn fill_script_round_trips_submit() {
        assert_eq!(payload(&fill_script(None, None, true))["submit"], true);
        assert_eq!(payload(&fill_script(None, None, false))["submit"], false);
    }

    #[test]
    fn offers_on_allowed_origins_only() {
        let a = account("tom");
        assert!(offered(&a, "https://app.example.com/other", false));
        assert!(offered(&a, "https://sso.example.com/auth", false));
        assert!(!offered(&a, "https://evil.example.com/login", true));
        assert!(!offered(&a, "https://app.example.com:8443/login", true));
        assert!(!offered(&a, "about:blank", true));
    }

    #[test]
    fn needs_a_username_or_a_saved_password() {
        let url = "https://app.example.com/login";
        assert!(!offered(&account(""), url, false));
        assert!(offered(&account(""), url, true));
        assert!(offered(&account(" tom "), url, false));
    }

    #[test]
    fn allows_requires_an_exact_port() {
        let mut a = account("tom");
        a.start_url = "http://localhost:3000/".into();
        a.login_origins = vec!["not a url".into(), "data:text/plain,hi".into()];
        assert!(a.allows(&Url::parse("http://localhost:3000/x").unwrap()));
        assert!(!a.allows(&Url::parse("http://localhost:3001/x").unwrap()));
        assert_eq!(a.allowed_origins().len(), 1);
    }

    #[test]
    fn fill_email_is_offered_only_off_the_allowed_origins_of_a_new_account() {
        let mut a = account("tom@example.com");
        a.login_origins.clear();
        let sso = Url::parse("https://sso.example.com/login").unwrap();
        let app = Url::parse("https://app.example.com/login").unwrap();
        let username = [LoginField::Username, LoginField::Password];
        assert!(can_fill_email(&a, &sso, &username));
        assert!(!can_fill_email(&a, &app, &username), "already allowed");
        assert!(!can_fill_email(&a, &sso, &[LoginField::Password]));
        assert!(!can_fill_email(
            &a,
            &Url::parse("about:blank").unwrap(),
            &username
        ));
        assert!(
            !can_fill_email(&account("tom"), &sso, &username),
            "has origins"
        );
        a.username = " ".into();
        assert!(!can_fill_email(&a, &sso, &username));
    }
}
