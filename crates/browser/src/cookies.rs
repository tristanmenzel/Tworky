use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cdp::Cdp;

const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CookieKey {
    pub name: String,
    pub domain: String,
    pub path: String,
}

impl CookieKey {
    fn of(cookie: &Value) -> Option<CookieKey> {
        Some(CookieKey {
            name: cookie["name"].as_str()?.to_string(),
            domain: cookie["domain"].as_str()?.to_string(),
            path: cookie["path"].as_str()?.to_string(),
        })
    }
}

async fn all_cookies(cdp: &impl Cdp) -> Result<Vec<Value>, String> {
    let list = cdp.call("Network.getAllCookies", json!({})).await?;
    Ok(list["cookies"].as_array().cloned().unwrap_or_default())
}

fn is_session(cookie: &Value) -> bool {
    cookie["session"].as_bool().unwrap_or(false)
}

/// `Network.setCookie` params that recreate `cookie` with a new expiry. A host-only cookie is
/// set by URL, because naming its domain would widen it to subdomains.
fn reissue(cookie: &Value, expires: f64) -> Value {
    let mut params = json!({
        "name": cookie["name"],
        "value": cookie["value"],
        "path": cookie["path"],
        "secure": cookie["secure"],
        "httpOnly": cookie["httpOnly"],
        "expires": expires,
    });
    let domain = cookie["domain"].as_str().unwrap_or_default();
    if domain.starts_with('.') {
        params["domain"] = domain.into();
    } else {
        let scheme = if cookie["secure"].as_bool().unwrap_or(false) {
            "https"
        } else {
            "http"
        };
        let path = cookie["path"].as_str().unwrap_or("/");
        params["url"] = format!("{scheme}://{domain}{path}").into();
    }
    for key in ["sameSite", "priority", "partitionKey"] {
        if let Some(v) = cookie.get(key) {
            params[key] = v.clone();
        }
    }
    params
}

/// Gives session cookies, and ones kept earlier, a 30-day expiry so a sign-in survives a
/// restart. Returns the cookies now kept, for `end_session` to find later.
pub async fn persist_session_cookies(
    cdp: &impl Cdp,
    kept: &[CookieKey],
) -> Result<Vec<CookieKey>, String> {
    let expires = (SystemTime::now() + KEEP_FOR)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let mut still_kept = Vec::new();
    for cookie in all_cookies(cdp).await? {
        let Some(key) = CookieKey::of(&cookie) else {
            continue;
        };
        if (is_session(&cookie) || kept.contains(&key))
            && cdp
                .call("Network.setCookie", reissue(&cookie, expires))
                .await
                .is_ok()
        {
            still_kept.push(key);
        }
    }
    Ok(still_kept)
}

/// Signs the account out: deletes session cookies and the ones `persist_session_cookies` kept.
pub async fn end_session(cdp: &impl Cdp, kept: &[CookieKey]) -> Result<(), String> {
    for cookie in all_cookies(cdp).await? {
        let Some(key) = CookieKey::of(&cookie) else {
            continue;
        };
        if is_session(&cookie) || kept.contains(&key) {
            cdp.call(
                "Network.deleteCookies",
                json!({ "name": key.name, "domain": key.domain, "path": key.path }),
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::fake::FakeCdp;

    fn jar() -> FakeCdp {
        FakeCdp::answering(|method, _| {
            Ok(match method {
                "Network.getAllCookies" => json!({ "cookies": [
                    { "name": "sid", "value": "1", "domain": "localhost", "path": "/",
                      "secure": false, "httpOnly": true, "session": true, "sameSite": "Lax" },
                    { "name": "pref", "value": "2", "domain": ".example.com", "path": "/",
                      "secure": true, "httpOnly": false, "session": false },
                    { "name": "old", "value": "3", "domain": "app.test", "path": "/a",
                      "secure": true, "httpOnly": false, "session": false },
                ] }),
                _ => json!({}),
            })
        })
    }

    fn key(name: &str, domain: &str, path: &str) -> CookieKey {
        CookieKey {
            name: name.into(),
            domain: domain.into(),
            path: path.into(),
        }
    }

    #[tokio::test]
    async fn persists_session_and_kept_cookies_only() {
        let cdp = jar();
        let kept = persist_session_cookies(&cdp, &[key("old", "app.test", "/a")])
            .await
            .unwrap();
        assert_eq!(
            kept,
            [key("sid", "localhost", "/"), key("old", "app.test", "/a")]
        );
        let set = cdp.params("Network.setCookie");
        assert_eq!(set[0]["url"], "http://localhost/");
        assert!(set[0].get("domain").is_none());
        assert_eq!(set[0]["sameSite"], "Lax");
        assert_eq!(set[1]["url"], "https://app.test/a");
        assert!(set[0]["expires"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn domain_cookies_keep_their_domain() {
        let cookie = json!({ "name": "pref", "value": "2", "domain": ".example.com", "path": "/",
            "secure": true, "httpOnly": false });
        let params = reissue(&cookie, 1.0);
        assert_eq!(params["domain"], ".example.com");
        assert!(params.get("url").is_none());
    }

    #[tokio::test]
    async fn ending_a_session_deletes_session_and_kept_cookies() {
        let cdp = jar();
        end_session(&cdp, &[key("old", "app.test", "/a")])
            .await
            .unwrap();
        let deleted: Vec<_> = cdp
            .params("Network.deleteCookies")
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(deleted, ["sid", "old"]);
    }
}
