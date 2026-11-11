use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Paths;
use crate::process;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    Latest,
    Behind { commits: u64, tag: String },
    Unknown { reason: String },
}

#[derive(Debug, PartialEq, Eq)]
enum Api {
    Body(String),
    NotFound,
    RateLimited,
}

/// curl's `-f` drops the error body and older Windows 10 curl lacks `--fail-with-body`, so the
/// status comes from `-w`.
fn api_get(path: &str) -> Result<Api> {
    let body_file = std::env::temp_dir().join(format!("tworky-api-{}.json", uuid::Uuid::new_v4()));
    let mut cmd = process::command("curl");
    cmd.args(["--silent", "--show-error", "--location", "--max-time", "30"])
        .args(["--header", "Accept: application/vnd.github+json"])
        .args(["--header", "User-Agent: Tworky"])
        .args([
            "--dump-header",
            "-",
            "--write-out",
            "%{http_code}",
            "--output",
        ])
        .arg(&body_file)
        .arg(format!("https://api.github.com/{path}"));
    let out = process::output(cmd, &format!("curl api.github.com/{path}"));
    let body = std::fs::read_to_string(&body_file).unwrap_or_default();
    let _ = std::fs::remove_file(&body_file);
    let out = out?;
    let (headers, code) = out.rsplit_once('\n').unwrap_or(("", out.as_str()));
    let status = code
        .trim()
        .parse()
        .with_context(|| format!("curl reported no HTTP status for {path}"))?;
    interpret(path, status, headers, body)
}

fn interpret(path: &str, status: u16, headers: &str, body: String) -> Result<Api> {
    match status {
        200 => Ok(Api::Body(body)),
        404 | 422 => Ok(Api::NotFound),
        403 | 429 if is_rate_limited(headers, &body) => Ok(Api::RateLimited),
        _ => {
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.trim().to_string());
            bail!("GitHub returned HTTP {status} for {path}: {message}")
        }
    }
}

fn is_rate_limited(headers: &str, body: &str) -> bool {
    let out_of_requests = headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("x-ratelimit-remaining") && value.trim() == "0"
        })
    });
    out_of_requests || body.to_ascii_lowercase().contains("rate limit")
}

fn rate_limited() -> Status {
    Status::Unknown {
        reason: "GitHub rate limit; try later".into(),
    }
}

/// Whether a release asset is the one this OS installs, as `scripts/install.*` picks it.
fn is_installable_here(asset: &str) -> bool {
    if cfg!(windows) {
        asset.ends_with("_x64-setup.exe")
    } else if cfg!(target_os = "macos") {
        asset == "Tworky-universal.app.tar.gz"
    } else {
        asset.ends_with("_amd64.AppImage")
    }
}

/// The newest published release with a build for this OS. The Windows build is published first,
/// while macOS and Linux still build, so the newest release can lack this OS's asset for a while.
fn newest_installable(releases: &Value, installable: impl Fn(&str) -> bool) -> Option<Release> {
    releases
        .as_array()?
        .iter()
        .filter(|r| r["draft"] != true && r["prerelease"] != true)
        .filter(|r| {
            r["assets"].as_array().is_some_and(|assets| {
                assets
                    .iter()
                    .any(|a| a["name"].as_str().is_some_and(&installable))
            })
        })
        .max_by(|a, b| {
            let at = |r: &Value| r["published_at"].as_str().unwrap_or_default().to_string();
            at(a).cmp(&at(b))
        })
        .and_then(parse_release)
}

fn parse_release(value: &Value) -> Option<Release> {
    let tag = value.get("tag_name")?.as_str()?.to_string();
    let commit = value
        .get("body")
        .and_then(Value::as_str)
        .and_then(|body| {
            body.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("commit:")
                    .map(|s| s.trim().to_string())
            })
        })
        .filter(|s| !s.is_empty());
    Some(Release { tag, commit })
}

pub fn status(repo: &str, running: &str) -> Result<Status> {
    let releases = match api_get(&format!("repos/{repo}/releases?per_page=20"))? {
        Api::Body(text) => serde_json::from_str::<Value>(&text).context("parsing releases")?,
        Api::NotFound => Value::Null,
        Api::RateLimited => return Ok(rate_limited()),
    };
    let Some(release) = newest_installable(&releases, is_installable_here) else {
        return Ok(Status::Unknown {
            reason: "no releases published yet".into(),
        });
    };
    if release.commit.as_deref() == Some(running) {
        return Ok(Status::Latest);
    }
    match api_get(&format!("repos/{repo}/compare/{running}...{}", release.tag))? {
        Api::Body(text) => {
            let value: Value = serde_json::from_str(&text).context("parsing comparison")?;
            Ok(classify(&value, &release.tag))
        }
        Api::NotFound => Ok(Status::Unknown {
            reason: "this build's commit isn't on GitHub".into(),
        }),
        Api::RateLimited => Ok(rate_limited()),
    }
}

/// `compare` is `running...release`, so `ahead_by` counts the release's commits this build lacks.
pub fn classify(compare: &Value, tag: &str) -> Status {
    match compare.get("ahead_by").and_then(Value::as_u64) {
        Some(0) => Status::Latest,
        Some(commits) => Status::Behind {
            commits,
            tag: tag.to_string(),
        },
        None => Status::Unknown {
            reason: "GitHub's comparison had no commit count".into(),
        },
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub phase: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressReport {
    pub progress: Option<Progress>,
    pub log_tail: String,
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("update")
}

pub fn read_progress(paths: &Paths) -> ProgressReport {
    let dir = dir(paths);
    let progress = std::fs::read_to_string(dir.join("status.json"))
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok());
    let log = std::fs::read_to_string(dir.join("update.log")).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    let log_tail = lines[lines.len().saturating_sub(20)..].join("\n");
    ProgressReport { progress, log_tail }
}

pub fn clear_finished(paths: &Paths) {
    let report = read_progress(paths);
    if report
        .progress
        .is_some_and(|p| p.phase == "done" || p.phase == "failed")
    {
        let _ = std::fs::remove_file(dir(paths).join("status.json"));
    }
}

fn script_name() -> &'static str {
    if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    }
}

fn fetch_script(repo: &str, dest: &Path) -> Result<()> {
    let url = format!(
        "https://raw.githubusercontent.com/{repo}/main/scripts/{}",
        script_name()
    );
    let mut cmd = process::command("curl");
    cmd.args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--max-time", "60", "--output"])
        .arg(dest)
        .arg(&url);
    process::output(cmd, &format!("curl {url}"))?;
    Ok(())
}

/// Starts the install script detached, so it outlives the app it is about to replace.
pub fn start(paths: &Paths, repo: &str) -> Result<()> {
    let dir = dir(paths);
    std::fs::create_dir_all(&dir)?;
    for stale in ["status.json", "update.log", "relaunch"] {
        let _ = std::fs::remove_file(dir.join(stale));
    }
    let script = dir.join(script_name());
    fetch_script(repo, &script)?;
    let pid = std::process::id().to_string();
    let dir_arg = dir.to_string_lossy().into_owned();
    let script_arg = script.to_string_lossy().into_owned();
    let (shells, args): (&[&str], Vec<&str>) = if cfg!(windows) {
        (
            &["pwsh", "powershell"],
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &script_arg,
                "-Repo",
                repo,
                "-WaitPid",
                &pid,
                "-StatusDir",
                &dir_arg,
            ],
        )
    } else {
        (
            &["sh"],
            vec![
                &script_arg,
                "--repo",
                repo,
                "--wait-pid",
                &pid,
                "--status-dir",
                &dir_arg,
            ],
        )
    };
    let mut last_error = None;
    for shell in shells {
        let mut cmd = std::process::Command::new(shell);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match process::spawn_detached(cmd) {
            Ok(_) => return Ok(()),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.expect("at least one shell")).context("starting the install script")
}

/// Tells the waiting install script to reopen the app once it has installed the update.
pub fn request_relaunch(paths: &Paths) -> Result<()> {
    let dir = dir(paths);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("relaunch"), "")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_commit_is_latest() {
        assert_eq!(
            classify(
                &json!({"status": "identical", "ahead_by": 0, "behind_by": 0}),
                "build-a"
            ),
            Status::Latest
        );
    }

    #[test]
    fn release_commits_missing_here_are_behind() {
        assert_eq!(
            classify(
                &json!({"status": "ahead", "ahead_by": 3, "behind_by": 0}),
                "build-b"
            ),
            Status::Behind {
                commits: 3,
                tag: "build-b".into()
            }
        );
    }

    #[test]
    fn diverged_counts_only_the_release_side() {
        assert_eq!(
            classify(
                &json!({"status": "diverged", "ahead_by": 2, "behind_by": 5}),
                "build-c"
            ),
            Status::Behind {
                commits: 2,
                tag: "build-c".into()
            }
        );
    }

    #[test]
    fn build_newer_than_release_is_latest() {
        assert_eq!(
            classify(
                &json!({"status": "behind", "ahead_by": 0, "behind_by": 4}),
                "build-d"
            ),
            Status::Latest
        );
    }

    #[test]
    fn missing_count_is_unknown() {
        assert!(matches!(classify(&json!({}), "x"), Status::Unknown { .. }));
    }

    #[test]
    fn the_newest_release_with_this_platforms_build_wins() {
        let release = |tag: &str, at: &str, assets: &[&str], draft: bool| {
            json!({
                "tag_name": tag,
                "published_at": at,
                "draft": draft,
                "prerelease": false,
                "body": format!("commit: {tag}"),
                "assets": assets.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>(),
            })
        };
        let releases = json!([
            release(
                "draft",
                "2026-11-05T00:00:00Z",
                &["a.AppImage", "s.exe"],
                true
            ),
            release(
                "windows-only",
                "2026-11-04T00:00:00Z",
                &["Tworky_x64-setup.exe"],
                false
            ),
            release(
                "all",
                "2026-11-03T00:00:00Z",
                &["Tworky_x64-setup.exe", "Tworky_amd64.AppImage"],
                false
            ),
        ]);
        let linux = newest_installable(&releases, |a| a.ends_with("_amd64.AppImage")).unwrap();
        assert_eq!(linux.tag, "all", "waits for its own build");
        let windows = newest_installable(&releases, |a| a.ends_with("_x64-setup.exe")).unwrap();
        assert_eq!(windows.tag, "windows-only");
        assert!(newest_installable(&releases, |a| a == "none").is_none());
    }

    #[test]
    fn ok_returns_the_body() {
        assert_eq!(
            interpret("p", 200, "", "[]".into()).unwrap(),
            Api::Body("[]".into())
        );
    }

    #[test]
    fn missing_repo_or_commit_is_not_found() {
        let body = r#"{"message":"Not Found"}"#;
        assert_eq!(interpret("p", 404, "", body.into()).unwrap(), Api::NotFound);
        assert_eq!(interpret("p", 422, "", "".into()).unwrap(), Api::NotFound);
    }

    #[test]
    fn exhausted_quota_is_rate_limited() {
        let headers =
            "HTTP/2 403\r\ncontent-type: application/json\r\nX-RateLimit-Remaining: 0\r\n\r";
        assert_eq!(
            interpret("p", 403, headers, "{}".into()).unwrap(),
            Api::RateLimited
        );
        let secondary = r#"{"message":"You have exceeded a secondary rate limit."}"#;
        assert_eq!(
            interpret("p", 429, "", secondary.into()).unwrap(),
            Api::RateLimited
        );
    }

    #[test]
    fn forbidden_with_quota_left_is_an_error() {
        let headers = "HTTP/2 403\r\nx-ratelimit-remaining: 42\r\n\r";
        let body = r#"{"message":"Repository access blocked"}"#;
        let e = interpret("repos/a/b", 403, headers, body.into()).unwrap_err();
        assert_eq!(
            e.to_string(),
            "GitHub returned HTTP 403 for repos/a/b: Repository access blocked"
        );
    }

    #[test]
    fn other_statuses_are_errors_with_the_status() {
        let e = interpret("p", 502, "", "Bad gateway".into()).unwrap_err();
        assert_eq!(e.to_string(), "GitHub returned HTTP 502 for p: Bad gateway");
    }

    #[test]
    fn release_commit_comes_from_the_body() {
        let r = parse_release(&json!({
            "tag_name": "build-abc1234",
            "body": "Built from main.\ncommit: abc1234def\n"
        }))
        .unwrap();
        assert_eq!(r.tag, "build-abc1234");
        assert_eq!(r.commit.as_deref(), Some("abc1234def"));
        assert_eq!(
            parse_release(&json!({"tag_name": "t", "body": null}))
                .unwrap()
                .commit,
            None
        );
    }
}
