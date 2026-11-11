use url::{Url, form_urlencoded};

/// Accepts what a person types in an address bar: a missing scheme means http for localhost
/// and https elsewhere. Only http and https URLs come back.
pub fn normalize_url(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let candidate = if input.contains("://") {
        input.to_string()
    } else {
        let host = input.split(['/', ':', '?', '#']).next().unwrap_or("");
        let scheme = if host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{input}")
    };
    let url = Url::parse(&candidate).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

const OAUTH_COMPANIONS: [&str; 6] = [
    "code",
    "id_token",
    "access_token",
    "error",
    "session_state",
    "response_type",
];

/// An OAuth redirect or authorize URL. Restoring a tab there replays a spent `state`.
pub fn is_oauth_url(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let fragment = url.fragment().unwrap_or("");
    let names: Vec<String> = url
        .query_pairs()
        .chain(form_urlencoded::parse(fragment.as_bytes()))
        .map(|(name, _)| name.into_owned())
        .collect();
    names.iter().any(|name| name == "state")
        && names
            .iter()
            .any(|name| OAUTH_COMPANIONS.contains(&name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_a_scheme_by_host() {
        assert_eq!(
            normalize_url("google.com").as_deref(),
            Some("https://google.com/")
        );
        assert_eq!(
            normalize_url("localhost:5173/x").as_deref(),
            Some("http://localhost:5173/x")
        );
        assert_eq!(
            normalize_url(" http://a.test ").as_deref(),
            Some("http://a.test/")
        );
        assert_eq!(normalize_url("file:///c:/x"), None);
        assert_eq!(normalize_url("javascript://alert(1)"), None);
        assert_eq!(normalize_url(""), None);
    }

    #[test]
    fn recognises_oauth_urls() {
        assert!(is_oauth_url(
            "http://localhost/auth/callback#code=abc&state=xyz&session_state=s"
        ));
        assert!(is_oauth_url("https://app.test/cb?code=abc&state=xyz"));
        assert!(is_oauth_url(
            "https://app.test/cb#error=access_denied&state=xyz"
        ));
        assert!(is_oauth_url(
            "https://login.example.com/t/oauth2/v2.0/authorize?client_id=c&response_type=code&state=xyz"
        ));
        assert!(!is_oauth_url("https://app.test/checkout?code=PROMO10"));
        assert!(!is_oauth_url("https://app.test/list?state=open"));
        assert!(!is_oauth_url("https://app.test/#/settings"));
    }
}
