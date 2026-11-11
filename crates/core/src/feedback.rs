use crate::process;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::io::Write;
use std::process::Stdio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bug,
    Friction,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Bug => "bug",
            Kind::Friction => "friction",
        }
    }

    fn colour(self) -> &'static str {
        match self {
            Kind::Bug => "d73a4a",
            Kind::Friction => "fbca04",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Similar {
    pub number: u64,
    pub title: String,
    pub url: String,
}

pub fn is_dev_build() -> bool {
    cfg!(debug_assertions)
        || ["TWORKY_HOME", "WORK_APP_HOME"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .any(|v| v.contains("Tworky-dev"))
}

/// Worker ids and project names are left out on purpose: an id holds the issue number and a slug
/// of its title, and both can name a private client or repo.
pub fn footer(version: &str, os: &str, dev: bool) -> String {
    let build = if dev { ", dev build" } else { "" };
    format!(
        "---
tw {version} on {os}{build}"
    )
}

pub fn with_footer(body: &str) -> String {
    let footer = footer(
        &format!(
            "{} ({})",
            env!("CARGO_PKG_VERSION"),
            crate::build_info::SHORT
        ),
        std::env::consts::OS,
        is_dev_build(),
    );
    let body = body.trim_end();
    if body.is_empty() {
        footer
    } else {
        format!(
            "{body}

{footer}"
        )
    }
}

pub fn format_similar(similar: &[Similar]) -> String {
    similar
        .iter()
        .map(|s| format!("#{} {} {}", s.number, s.title, s.url))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn similar_hint(similar: &[Similar]) -> String {
    format!(
        "Similar open issues:\n{}\nAdd to one with `tw issue comment <number>`, or file a new issue anyway with --force.",
        format_similar(similar)
    )
}

pub fn dry_run_create(repo: &str, kind: Kind, title: &str, body: &str) -> String {
    format!(
        "repo: {repo}\ntitle: {title}\nlabels: {}\n\n{body}",
        kind.label()
    )
}

pub fn dry_run_comment(repo: &str, number: u64, body: &str) -> String {
    format!("repo: {repo}\ncomment on: #{number}\n\n{body}")
}

fn gh_with_stdin(args: &[&str], stdin: &str) -> Result<String> {
    let mut cmd = process::command("gh");
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("starting `gh`")?;
    // gh may exit before reading stdin (bad auth, unknown repo); its stderr says why, not the pipe.
    let _ = child
        .stdin
        .take()
        .context("gh stdin")?
        .write_all(stdin.as_bytes());
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "gh {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn ensure_label(repo: &str, kind: Kind) {
    let mut cmd = process::command("gh");
    cmd.args([
        "label",
        "create",
        kind.label(),
        "--repo",
        repo,
        "--color",
        kind.colour(),
        "--force",
    ]);
    let _ = process::output(cmd, "gh label create");
}

pub fn find_similar(repo: &str, kind: Kind, title: &str) -> Result<Vec<Similar>> {
    let mut cmd = process::command("gh");
    cmd.args([
        "issue",
        "list",
        "--repo",
        repo,
        "--label",
        kind.label(),
        "--state",
        "open",
        "--search",
        title,
        "--json",
        "number,title,url",
        "--limit",
        "5",
    ]);
    let text = process::output(cmd, "gh issue list")?;
    serde_json::from_str(&text).context("parsing gh issue list output")
}

/// Creates the label only when gh reports it missing, so an existing label's colour is kept.
pub fn create_issue(repo: &str, kind: Kind, title: &str, body: &str) -> Result<String> {
    match try_create_issue(repo, kind, title, body) {
        Err(e) if is_missing_label(&format!("{e:#}"), kind) => {
            ensure_label(repo, kind);
            try_create_issue(repo, kind, title, body)
        }
        result => result,
    }
}

fn is_missing_label(error: &str, kind: Kind) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("not found") && error.contains(&format!("'{}'", kind.label()))
}

fn try_create_issue(repo: &str, kind: Kind, title: &str, body: &str) -> Result<String> {
    gh_with_stdin(
        &[
            "issue",
            "create",
            "--repo",
            repo,
            "--title",
            title,
            "--label",
            kind.label(),
            "--body-file",
            "-",
        ],
        body,
    )
}

pub fn comment_issue(repo: &str, number: u64, body: &str) -> Result<String> {
    let number = number.to_string();
    gh_with_stdin(
        &[
            "issue",
            "comment",
            &number,
            "--repo",
            repo,
            "--body-file",
            "-",
        ],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_names_only_the_build() {
        assert_eq!(
            footer("0.3.0", "windows", true),
            "---
tw 0.3.0 on windows, dev build"
        );
        assert_eq!(
            footer("0.3.0", "linux", false),
            "---
tw 0.3.0 on linux"
        );
    }

    #[test]
    fn body_gets_footer_after_blank_line() {
        assert!(
            with_footer(
                "It broke.

"
            )
            .starts_with(
                "It broke.

---
tw "
            )
        );
        assert!(with_footer("").starts_with(
            "---
"
        ));
    }

    #[test]
    fn recognises_gh_missing_label_error() {
        let err = "gh issue failed: could not add label: 'friction' not found";
        assert!(is_missing_label(err, Kind::Friction));
        assert!(!is_missing_label(err, Kind::Bug));
        assert!(!is_missing_label(
            "gh issue failed: HTTP 401",
            Kind::Friction
        ));
    }

    #[test]
    fn similar_issues_are_listed_with_hint() {
        let similar = vec![
            Similar {
                number: 4,
                title: "Setup hangs".into(),
                url: "https://github.com/o/r/issues/4".into(),
            },
            Similar {
                number: 9,
                title: "Setup slow".into(),
                url: "https://github.com/o/r/issues/9".into(),
            },
        ];
        assert_eq!(
            format_similar(&similar),
            "#4 Setup hangs https://github.com/o/r/issues/4\n#9 Setup slow https://github.com/o/r/issues/9"
        );
        let hint = similar_hint(&similar);
        assert!(hint.contains("tw issue comment <number>"));
        assert!(hint.contains("--force"));
    }
}
