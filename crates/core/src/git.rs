use std::path::Path;

use anyhow::{Context, Result};

use crate::process;
use crate::state::BranchOrigin;

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    process::run("git", args, Some(repo))
}

fn succeeds(repo: &Path, args: &[&str]) -> bool {
    git(repo, args).is_ok()
}

pub fn fetch(repo: &Path) -> Result<()> {
    git(repo, &["fetch", "--prune", "origin"]).map(drop)
}

pub fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    succeeds(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

pub fn remote_branch_exists(repo: &Path, branch: &str) -> bool {
    succeeds(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ],
    )
}

/// Reuses an existing local or remote branch so a worker can be recreated for in-flight work.
pub fn worktree_add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<BranchOrigin> {
    let path_s = path.to_string_lossy();
    if local_branch_exists(repo, branch) {
        git(repo, &["worktree", "add", &path_s, branch])?;
        Ok(BranchOrigin::Existing)
    } else if remote_branch_exists(repo, branch) {
        git(
            repo,
            &[
                "worktree",
                "add",
                "--track",
                "-b",
                branch,
                &path_s,
                &format!("origin/{branch}"),
            ],
        )?;
        Ok(BranchOrigin::Tracking)
    } else {
        git(
            repo,
            &["worktree", "add", "--no-track", "-b", branch, &path_s, base],
        )?;
        Ok(BranchOrigin::New)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BranchRefs {
    pub local: bool,
    pub remote: bool,
}

/// Local branches and `origin/*` remote branches, by branch name.
pub fn branches(repo: &Path) -> Result<std::collections::BTreeMap<String, BranchRefs>> {
    let out = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes/origin",
        ],
    )?;
    let mut map: std::collections::BTreeMap<String, BranchRefs> = Default::default();
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("refs/heads/") {
            map.entry(name.to_string()).or_default().local = true;
        } else if let Some(name) = line.strip_prefix("refs/remotes/origin/")
            && name != "HEAD"
        {
            map.entry(name.to_string()).or_default().remote = true;
        }
    }
    Ok(map)
}

/// (worktree path, branch) for every worktree of `repo`, the main checkout included.
pub fn checked_out_branches(repo: &Path) -> Result<Vec<(String, String)>> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut list = Vec::new();
    let mut path = String::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = p.to_string();
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            list.push((path.clone(), b.to_string()));
        }
    }
    Ok(list)
}

pub fn delete_local_branch(repo: &Path, branch: &str) -> Result<()> {
    git(repo, &["branch", "-D", branch]).map(drop)
}

/// Deletes `origin/<branch>` on the server. A branch that is already gone there only has its stale
/// remote-tracking ref removed.
pub fn delete_remote_branch(repo: &Path, branch: &str) -> Result<()> {
    match git(repo, &["push", "origin", "--delete", branch]) {
        Ok(_) => Ok(()),
        Err(e) if format!("{e:#}").contains("remote ref does not exist") => {
            git(repo, &["branch", "-dr", &format!("origin/{branch}")]).map(drop)
        }
        Err(e) => Err(e),
    }
}

/// (ahead, behind) of `rev` relative to `base`.
pub fn ahead_behind(repo: &Path, base: &str, rev: &str) -> Result<(u32, u32)> {
    let out = git(
        repo,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...{rev}"),
        ],
    )?;
    let mut parts = out
        .split_whitespace()
        .map(|n| n.parse::<u32>().unwrap_or(0));
    let behind = parts.next().unwrap_or(0);
    let ahead = parts.next().unwrap_or(0);
    Ok((ahead, behind))
}

pub fn count(repo: &Path, range: &str) -> Result<u32> {
    Ok(git(repo, &["rev-list", "--count", range])?
        .trim()
        .parse()
        .unwrap_or(0))
}

/// "3 days ago · Author" for the tip of `rev`.
pub fn last_commit(repo: &Path, rev: &str) -> Result<String> {
    Ok(git(repo, &["log", "-1", "--format=%cr · %an", rev, "--"])?
        .trim()
        .to_string())
}

pub fn is_worktree(path: &Path) -> bool {
    path.join(".git").exists()
}

pub fn current_branch(worktree: &Path) -> Result<String> {
    Ok(git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

/// `git worktree remove` can stop partway on Windows when a file is held open. It deletes `.git`
/// first, so a retry finds a plain folder git no longer accepts; whatever is left is removed directly.
pub fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    let removed_by_git = if is_worktree(path) {
        git(
            repo,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        )
        .map(drop)
    } else {
        Ok(())
    };
    if path.exists() {
        std::fs::remove_dir_all(path).with_context(|| match &removed_by_git {
            Err(e) => format!(
                "removing {} after git worktree remove failed ({e:#})",
                path.display()
            ),
            Ok(()) => format!("removing {}", path.display()),
        })?;
    }
    git(repo, &["worktree", "prune"]).map(drop)
}

pub fn branch_delete(repo: &Path, branch: &str) -> Result<()> {
    if local_branch_exists(repo, branch) {
        git(repo, &["branch", "-D", branch])?;
    }
    Ok(())
}

pub fn skip_worktree(worktree: &Path, file: &str) -> Result<()> {
    git(worktree, &["update-index", "--skip-worktree", file]).map(drop)
}

pub fn is_ignored(worktree: &Path, file: &str) -> bool {
    succeeds(worktree, &["check-ignore", "-q", file])
}

pub fn is_tracked(worktree: &Path, file: &str) -> bool {
    succeeds(worktree, &["ls-files", "--error-unmatch", file])
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct WorktreeStatus {
    pub dirty_files: usize,
    pub unpushed_commits: usize,
}

pub fn status(worktree: &Path) -> Result<WorktreeStatus> {
    let dirty = git(worktree, &["--no-optional-locks", "status", "--porcelain"])?
        .lines()
        .count();
    let unpushed = git(worktree, &["rev-list", "--count", "@{upstream}..HEAD"])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| {
            git(
                worktree,
                &["rev-list", "--count", "HEAD", "--not", "--remotes=origin"],
            )
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
        });
    Ok(WorktreeStatus {
        dirty_files: dirty,
        unpushed_commits: unpushed,
    })
}

/// `owner/repo` from the origin remote, for https and ssh URLs.
pub fn origin_name_with_owner(repo: &Path) -> Result<String> {
    let url = git(repo, &["remote", "get-url", "origin"])?;
    parse_name_with_owner(url.trim())
        .with_context(|| format!("unrecognised origin URL '{}'", url.trim()))
}

/// Main clones (a `.git` folder, not a worktree's `.git` file) under `roots`, up to `max_depth`
/// levels down, keyed by their origin's `owner/repo`. Doesn't descend into clones or
/// `node_modules`.
pub fn find_clones(
    roots: &[std::path::PathBuf],
    max_depth: usize,
) -> Vec<(String, std::path::PathBuf)> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(String, std::path::PathBuf)>) {
        if dir.join(".git").is_dir() {
            if let Ok(nwo) = origin_name_with_owner(dir) {
                out.push((nwo, dir.to_path_buf()));
            }
            return;
        }
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&entry.path(), depth - 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for root in roots {
        walk(root, max_depth, &mut out);
    }
    out.sort();
    out
}

fn parse_name_with_owner(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_origin_urls() {
        for url in [
            "https://github.com/acme/acme-api.git",
            "git@github.com:acme/acme-api.git",
            "ssh://git@github.com/acme/acme-api",
        ] {
            assert_eq!(parse_name_with_owner(url).as_deref(), Some("acme/acme-api"));
        }
        assert!(parse_name_with_owner("https://gitlab.com/x/y").is_none());
    }

    #[test]
    fn finds_clones_by_origin_but_not_their_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let clone = |rel: &str, origin: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(&p).unwrap();
            git(&p, &["init", "-q"]).unwrap();
            git(&p, &["remote", "add", "origin", origin]).unwrap();
            p
        };
        let a = clone("acme/app", "https://github.com/acme/app.git");
        clone("deep/a/b/c/too-deep", "https://github.com/acme/deep.git");
        clone("other", "https://gitlab.com/x/y.git");
        let worktree = dir.path().join("acme/app-wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere").unwrap();

        let found = find_clones(&[dir.path().to_path_buf()], 3);
        assert_eq!(found, vec![("acme/app".to_string(), a)]);
    }

    #[test]
    fn removes_a_worktree_folder_git_already_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        let leftover = dir.path().join("worker").join("repo");
        std::fs::create_dir_all(leftover.join("bin")).unwrap();
        std::fs::write(leftover.join("bin").join("app.dll"), "x").unwrap();

        worktree_remove(&repo, &leftover).unwrap();

        assert!(!leftover.exists());
    }
}
