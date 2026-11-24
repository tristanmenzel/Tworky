use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tworky_browser::cookies::CookieKey;
use tworky_browser::urls::is_oauth_url;
use tworky_browser::viewport::Viewport;

/// What the browser keeps across restarts: the open tabs, and each profile's worker, so cleanup
/// can delete it, with the cookies kept alive for it.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub profiles: BTreeMap<String, SavedProfile>,
    /// The highest tab id handed out, so ids of tabs that aren't saved are never reused.
    #[serde(default)]
    pub next_id: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub id: u64,
    pub worker: String,
    pub account: String,
    pub url: String,
    /// A file that fails to parse loses every profile, so a viewport that can't be read is
    /// dropped on its own.
    #[serde(default, deserialize_with = "lenient_viewport")]
    pub viewport: Option<Viewport>,
}

fn lenient_viewport<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Viewport>, D::Error> {
    Ok(Value::deserialize(d)
        .ok()
        .and_then(|value| serde_json::from_value::<Viewport>(value).ok())
        .filter(|viewport| viewport.check().is_ok()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedProfile {
    pub worker: String,
    #[serde(default)]
    pub kept: Vec<CookieKey>,
    #[serde(default)]
    pub anonymous: bool,
}

pub fn path() -> Option<PathBuf> {
    tworky_core::config::Paths::resolve()
        .ok()
        .map(|p| p.data_dir.join("browser").join("state.json"))
}

pub fn load(path: &Path) -> Saved {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, saved: &Saved) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(saved)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reopening an OAuth redirect would replay a spent `state`, so such a tab starts over.
pub fn restore_url<'a>(last: &'a str, start_url: &'a str) -> &'a str {
    if last.is_empty() || last == "about:blank" || is_oauth_url(last) {
        start_url
    } else {
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_a_missing_or_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("browser").join("state.json");
        assert_eq!(load(&file), Saved::default());

        let mut saved = Saved::default();
        saved.tabs.push(SavedTab {
            id: 3,
            worker: "w".into(),
            account: "Tester 1".into(),
            url: "http://localhost:5273/clients".into(),
            viewport: Some(tworky_browser::viewport::DESKTOP),
        });
        saved.profiles.insert(
            "tw-w-Tester_1".into(),
            SavedProfile {
                worker: "w".into(),
                kept: vec![CookieKey {
                    name: "sid".into(),
                    domain: "localhost".into(),
                    path: "/".into(),
                }],
                anonymous: false,
            },
        );
        saved.profiles.insert(
            "ta-w-7-0123abcd".into(),
            SavedProfile {
                worker: "w".into(),
                kept: Vec::new(),
                anonymous: true,
            },
        );
        saved.next_id = 7;
        save(&file, &saved).unwrap();
        assert_eq!(load(&file), saved);

        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load(&file), Saved::default());
    }

    #[test]
    fn reads_state_saved_before_anonymous_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        std::fs::write(
            &file,
            r#"{"tabs":[{"id":2,"worker":"w","account":"a","url":""}],"profiles":{"tw-w-a":{"worker":"w","kept":[]}}}"#,
        )
        .unwrap();
        let saved = load(&file);
        assert_eq!(saved.next_id, 0);
        assert!(!saved.profiles["tw-w-a"].anonymous);
        assert_eq!(saved.tabs[0].viewport, None);
    }

    #[test]
    fn a_viewport_that_cannot_be_read_is_dropped_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let tab = |id: u64, viewport: &str| {
            format!(r#"{{"id":{id},"worker":"w","account":"a","url":"","viewport":{viewport}}}"#)
        };
        let tabs = [
            tab(1, r#"{"kind":"watch"}"#),
            tab(
                2,
                r#"{"kind":"fixed","width":"wide","height":800,"mobile":false}"#,
            ),
            tab(
                3,
                r#"{"kind":"fixed","width":10,"height":800,"mobile":false}"#,
            ),
            tab(4, r#""phone""#),
            tab(5, "null"),
            tab(
                6,
                r#"{"kind":"fixed","width":390,"height":844,"mobile":true}"#,
            ),
            tab(7, r#"{"kind":"fit"}"#),
        ];
        std::fs::write(
            &file,
            format!(
                r#"{{"tabs":[{}],"profiles":{{"tw-w-a":{{"worker":"w","kept":[]}}}},"next_id":9}}"#,
                tabs.join(",")
            ),
        )
        .unwrap();
        let saved = load(&file);
        assert_eq!(saved.next_id, 9);
        assert!(saved.profiles.contains_key("tw-w-a"));
        let viewports: Vec<_> = saved.tabs.iter().map(|t| (t.id, t.viewport)).collect();
        assert_eq!(
            viewports,
            [
                (1, None),
                (2, None),
                (3, None),
                (4, None),
                (5, None),
                (6, Some(Viewport::fixed(390, 844, true).unwrap())),
                (7, Some(Viewport::Fit)),
            ]
        );
    }

    #[test]
    fn restores_at_the_start_url_instead_of_an_oauth_redirect() {
        let start = "http://localhost:5273/";
        assert_eq!(
            restore_url("http://localhost:5273/clients", start),
            "http://localhost:5273/clients"
        );
        assert_eq!(restore_url("", start), start);
        assert_eq!(restore_url("about:blank", start), start);
        assert_eq!(
            restore_url("http://localhost:5273/cb?code=abc&state=xyz", start),
            start
        );
    }
}
