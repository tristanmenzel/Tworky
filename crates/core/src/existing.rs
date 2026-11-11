use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{ProjectConfig, RepoConfig};
use crate::git::{self, BranchRefs};
use crate::hierarchy::IssueIndex;
use crate::pulls::{self, PrDetail};
use crate::state::WorkerRecord;
use crate::worker::Workspace;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkSubject {
    Issue { id: String },
    Item { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Info,
    Warn,
    Bad,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signal {
    pub level: Level,
    pub text: String,
}

fn signal(level: Level, text: impl Into<String>) -> Signal {
    Signal {
        level,
        text: text.into(),
    }
}

/// Variant order is the order cards are sorted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    NeedsAttention,
    Draft,
    Ready,
    InReview,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BranchWork {
    /// Project repo name when the branch is in one of the project's repos, else `owner/repo`.
    pub repo: String,
    pub name_with_owner: Option<String>,
    pub branch: String,
    pub local: bool,
    pub remote: bool,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub unpushed: Option<u32>,
    pub last_commit: Option<String>,
    pub worker: Option<String>,
    /// Issue numbers that appear in the branch name.
    pub matched: Vec<u64>,
    pub pull_requests: Vec<PrDetail>,
    pub signals: Vec<Signal>,
    /// `None` unless the newest PR is open.
    pub verdict: Option<Verdict>,
}

/// Issue numbers appearing as whole digit runs in a branch name: `feat/x-318` matches 318, not 31.
pub fn numbers_in_branch(branch: &str, numbers: &BTreeSet<u64>) -> Vec<u64> {
    let mut found: Vec<u64> = branch
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|run| run.parse().ok())
        .filter(|n| numbers.contains(n))
        .collect();
    found.dedup();
    found
}

pub(crate) enum Scope {
    Issue {
        /// The issue and its sub-issues.
        ids: BTreeSet<String>,
        numbers: BTreeSet<u64>,
        /// (owner/repo, branch) named by the issues' PRs and linked branches.
        wanted: Vec<(String, String)>,
    },
    Item {
        name: String,
        /// (owner/repo, number) of a pull request item's PR.
        pr: Option<(String, u64)>,
    },
}

impl Scope {
    fn numbers(&self) -> BTreeSet<u64> {
        match self {
            Scope::Issue { numbers, .. } => numbers.clone(),
            Scope::Item { .. } => BTreeSet::new(),
        }
    }

    fn holds(&self, w: &WorkerRecord) -> bool {
        match self {
            Scope::Issue { ids, .. } => w.issue_id().is_some_and(|id| ids.contains(id)),
            Scope::Item { name, .. } => w.item.as_ref().is_some_and(|i| &i.name == name),
        }
    }
}

pub(crate) struct RepoBranches {
    pub name: String,
    pub name_with_owner: Option<String>,
    pub branches: BTreeMap<String, BranchRefs>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Lookups {
    /// (repo name, branch, issue numbers in the branch name) to show from the project's repos.
    pub show: Vec<(String, String, Vec<u64>)>,
    /// (owner/repo, branch) whose PRs are fetched by head.
    pub by_head: Vec<(String, String)>,
    /// (owner/repo, number) of PRs fetched by number.
    pub by_number: Vec<(String, u64)>,
}

/// (worker id, repo) to the branch now checked out in that worktree.
pub(crate) type CheckedOut = BTreeMap<(String, String), String>;

fn worker_branches<'a>(
    w: &'a WorkerRecord,
    checked_out: &'a CheckedOut,
) -> impl Iterator<Item = (&'a str, &'a str)> {
    w.worktrees.iter().flat_map(move |t| {
        let now = checked_out
            .get(&(w.id.clone(), t.repo.clone()))
            .filter(|b| **b != t.branch)
            .map(|b| (t.repo.as_str(), b.as_str()));
        std::iter::once((t.repo.as_str(), t.branch.as_str())).chain(now)
    })
}

/// Which branches of the project's repos to show, and which PRs to fetch for them. A branch is
/// shown when one of the subject's workers has it in a worktree, or, for an issue, when its name
/// holds one of the issues' numbers or GitHub links it to them.
pub(crate) fn plan_lookups(
    scope: &Scope,
    repos: &[RepoBranches],
    workers: &[WorkerRecord],
    checked_out: &CheckedOut,
) -> Lookups {
    let held: BTreeSet<(&str, &str)> = workers
        .iter()
        .filter(|w| scope.holds(w))
        .flat_map(|w| worker_branches(w, checked_out))
        .collect();
    let numbers = scope.numbers();
    let mut lookups = Lookups::default();
    for repo in repos {
        for branch in repo.branches.keys() {
            let matched = numbers_in_branch(branch, &numbers);
            let wanted = match scope {
                Scope::Issue { wanted, .. } => wanted.iter().any(|(r, b)| {
                    b == branch
                        && repo
                            .name_with_owner
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(r))
                }),
                Scope::Item { .. } => false,
            };
            if matched.is_empty()
                && !wanted
                && !held.contains(&(repo.name.as_str(), branch.as_str()))
            {
                continue;
            }
            if let Some(nwo) = &repo.name_with_owner {
                lookups.by_head.push((nwo.clone(), branch.clone()));
            }
            lookups
                .show
                .push((repo.name.clone(), branch.clone(), matched));
        }
    }
    if let Scope::Item { pr: Some(pr), .. } = scope {
        lookups.by_number.push(pr.clone());
    }
    lookups
}

fn branch_work(
    repo: &RepoConfig,
    name_with_owner: Option<String>,
    branch: String,
    at: BranchRefs,
    worker: Option<String>,
    matched: Vec<u64>,
) -> BranchWork {
    let remote_rev = format!("origin/{branch}");
    let rev = if at.local {
        branch.clone()
    } else {
        remote_rev.clone()
    };
    let (mut ahead, behind) = git::ahead_behind(&repo.source, &repo.base, &rev)
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((None, None));
    if at.local
        && at.remote
        && let Ok((remote_ahead, _)) = git::ahead_behind(&repo.source, &repo.base, &remote_rev)
    {
        ahead = Some(ahead.map_or(remote_ahead, |a| a.max(remote_ahead)));
    }
    let unpushed = (at.local && at.remote)
        .then(|| git::count(&repo.source, &format!("origin/{branch}..{branch}")).ok())
        .flatten();
    BranchWork {
        repo: repo.name.clone(),
        name_with_owner,
        worker,
        last_commit: git::last_commit(&repo.source, &rev).ok(),
        branch,
        local: at.local,
        remote: at.remote,
        ahead,
        behind,
        unpushed,
        matched,
        ..Default::default()
    }
}

pub fn existing_work(
    ws: &Workspace,
    index: &IssueIndex,
    subject: &WorkSubject,
    fetch: bool,
) -> Result<Vec<BranchWork>> {
    let (project, scope, mut prs) = match subject {
        WorkSubject::Issue { id } => {
            let Some(issue) = index.get(id) else {
                return Ok(vec![]);
            };
            let issues: Vec<_> = std::iter::once(issue).chain(index.children(id)).collect();
            let ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
            let refs = pulls::issue_refs(&ids)?;
            let wanted = refs
                .pull_requests
                .iter()
                .map(|pr| (pr.repo.clone(), pr.head.clone()))
                .chain(refs.linked_branches)
                .collect();
            let scope = Scope::Issue {
                ids: ids.into_iter().collect(),
                numbers: issues.iter().map(|i| i.number).collect(),
                wanted,
            };
            (
                ws.config.project_for_repo(&issue.repo),
                scope,
                refs.pull_requests,
            )
        }
        WorkSubject::Item { name } => {
            let Some(item) = ws.state.get_item(name)? else {
                return Ok(vec![]);
            };
            if let Some(issue) = item.issue() {
                let subject = WorkSubject::Issue {
                    id: issue.id.clone(),
                };
                return existing_work(ws, index, &subject, fetch);
            }
            let scope = Scope::Item {
                name: item.name.clone(),
                pr: item.pull_request().map(|p| (p.repo.clone(), p.number)),
            };
            (ws.config.project(&item.project), scope, vec![])
        }
    };

    let configs = project.map(|p| p.repos.as_slice()).unwrap_or_default();
    let repos: Vec<RepoBranches> = configs
        .iter()
        .map(|repo| {
            if fetch {
                let _ = git::fetch(&repo.source);
            }
            RepoBranches {
                name: repo.name.clone(),
                name_with_owner: git::origin_name_with_owner(&repo.source).ok(),
                branches: git::branches(&repo.source).unwrap_or_default(),
            }
        })
        .collect();
    let workers: Vec<WorkerRecord> = ws
        .state
        .list()?
        .into_iter()
        .filter(|w| project.is_some_and(|p| p.name == w.project))
        .collect();
    let checked_out: CheckedOut = workers
        .iter()
        .flat_map(|w| {
            w.worktrees.iter().filter_map(|t| {
                let branch = crate::local::checked_out(&w.dir.join(&t.repo))?;
                Some(((w.id.clone(), t.repo.clone()), branch))
            })
        })
        .collect();
    let lookups = plan_lookups(&scope, &repos, &workers, &checked_out);

    let worker_of: BTreeMap<(&str, &str), &str> = workers
        .iter()
        .flat_map(|w| worker_branches(w, &checked_out).map(|key| (key, w.id.as_str())))
        .collect();

    let mut work: BTreeMap<(String, String), BranchWork> = BTreeMap::new();
    for (repo_name, branch, matched) in lookups.show {
        let Some(i) = repos.iter().position(|r| r.name == repo_name) else {
            continue;
        };
        let worker = worker_of
            .get(&(repo_name.as_str(), branch.as_str()))
            .map(|w| w.to_string());
        let b = branch_work(
            &configs[i],
            repos[i].name_with_owner.clone(),
            branch.clone(),
            repos[i].branches[&branch],
            worker,
            matched,
        );
        work.insert((repo_name, branch), b);
    }

    let nwo_to_repo: BTreeMap<String, String> = repos
        .iter()
        .filter_map(|r| {
            Some((
                r.name_with_owner.as_ref()?.to_ascii_lowercase(),
                r.name.clone(),
            ))
        })
        .collect();
    let key_for = |nwo: &str, branch: &str| {
        let repo = nwo_to_repo
            .get(&nwo.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| nwo.to_string());
        (repo, branch.to_string())
    };

    let mut fetched: Vec<PrDetail> = pulls::prs_for_branches(&lookups.by_head)?
        .into_values()
        .flatten()
        .collect();
    for (nwo, number) in &lookups.by_number {
        match pulls::pr_by_number(nwo, *number) {
            Ok(pr) => fetched.extend(pr),
            Err(e) => eprintln!("existing work: looking up {nwo}#{number}: {e:#}"),
        }
    }
    fetched.append(&mut prs);

    let numbers = scope.numbers();
    for pr in fetched {
        let key = key_for(&pr.repo, &pr.head);
        let entry = work.entry(key.clone()).or_insert_with(|| BranchWork {
            repo: key.0.clone(),
            name_with_owner: Some(pr.repo.clone()),
            branch: pr.head.clone(),
            matched: numbers_in_branch(&pr.head, &numbers),
            ..Default::default()
        });
        if !entry.pull_requests.iter().any(|p| p.number == pr.number) {
            entry.pull_requests.push(pr);
        }
    }

    let mut list: Vec<BranchWork> = work.into_values().collect();
    for b in &mut list {
        b.pull_requests
            .sort_by_key(|p| (!p.is_open(), std::cmp::Reverse(p.number)));
        b.signals = next_steps(b);
        b.verdict = verdict(b);
    }
    list.sort_by_key(|b| (rank(b), b.repo.clone(), b.branch.clone()));
    Ok(list)
}

/// Looks up the number of an open PR whose head is (owner/repo, branch).
pub type OpenPr<'a> = &'a dyn Fn(&str, &str) -> Result<Option<u64>>;

pub fn open_pr_on_github(name_with_owner: &str, branch: &str) -> Result<Option<u64>> {
    let prs = pulls::prs_for_branches(&[(name_with_owner.to_string(), branch.to_string())])?;
    Ok(prs
        .into_values()
        .flatten()
        .find(|p| p.is_open())
        .map(|p| p.number))
}

/// Deletes `branch` from the project repo's main checkout and from origin, returning what was
/// removed. Refuses the base branch, a branch checked out in any worktree, and a branch that is
/// the head of an open PR, because deleting it would close the PR.
pub fn delete_branch(
    project: &ProjectConfig,
    repo_name: &str,
    branch: &str,
    open_pr: OpenPr,
) -> Result<Vec<String>> {
    let repo = project
        .repos
        .iter()
        .find(|r| r.name == repo_name)
        .with_context(|| format!("{repo_name} is not a repo of project {}", project.name))?;
    let base = repo.base.strip_prefix("origin/").unwrap_or(&repo.base);
    if branch == base {
        bail!("{branch} is the base branch of {repo_name}");
    }
    if let Some((path, _)) = git::checked_out_branches(&repo.source)?
        .into_iter()
        .find(|(_, b)| b == branch)
    {
        bail!("{branch} is checked out in {path}; switch that worktree away from it first");
    }
    let at = git::branches(&repo.source)?
        .get(branch)
        .copied()
        .with_context(|| format!("{branch} no longer exists in {repo_name}"))?;
    if at.remote
        && let Ok(nwo) = git::origin_name_with_owner(&repo.source)
        && let Some(number) = open_pr(&nwo, branch)?
    {
        bail!(
            "PR #{number} is open on {branch}; deleting the branch would close it. Merge or close the PR first"
        );
    }
    let mut deleted = Vec::new();
    if at.remote {
        git::delete_remote_branch(&repo.source, branch)?;
        deleted.push(format!("origin/{branch}"));
    }
    if at.local {
        git::delete_local_branch(&repo.source, branch)?;
        deleted.push(format!("local {branch}"));
    }
    Ok(deleted)
}

fn rank(b: &BranchWork) -> u8 {
    match (b.verdict, b.pull_requests.first()) {
        (Some(v), _) => v as u8,
        (None, None) => 4,
        (None, Some(pr)) if pr.is_merged() => 6,
        (None, Some(_)) => 5,
    }
}

fn has_conflicts(pr: &PrDetail) -> bool {
    pr.mergeable == "CONFLICTING" || pr.merge_state == "DIRTY"
}

pub fn is_ready(pr: &PrDetail) -> bool {
    !pr.is_draft
        && pr.merge_state == "CLEAN"
        && pr.checks.failing.is_empty()
        && pr.checks.pending == 0
        && pr.unresolved_threads == 0
        && !pr.approved_by.is_empty()
        && pr.changes_requested_by.is_empty()
        && pr.review_requests == 0
}

pub fn verdict(b: &BranchWork) -> Option<Verdict> {
    let pr = b.pull_requests.first().filter(|p| p.is_open())?;
    Some(
        if !pr.checks.failing.is_empty() || has_conflicts(pr) || !pr.changes_requested_by.is_empty()
        {
            Verdict::NeedsAttention
        } else if pr.is_draft {
            Verdict::Draft
        } else if is_ready(pr) {
            Verdict::Ready
        } else {
            Verdict::InReview
        },
    )
}

fn names(list: &[String]) -> String {
    match list.len() {
        0..=3 => list.join(", "),
        n => format!("{} +{}", list[..3].join(", "), n - 3),
    }
}

fn plural(n: u32, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn next_steps(b: &BranchWork) -> Vec<Signal> {
    let mut out = Vec::new();
    if let Some(n) = b.unpushed.filter(|n| *n > 0) {
        out.push(signal(
            Level::Warn,
            format!("{} not pushed", plural(n, "commit")),
        ));
    }
    let Some(pr) = b.pull_requests.first() else {
        match b.ahead {
            Some(0) => out.push(signal(Level::Info, "No commits yet")),
            _ if b.local && !b.remote => out.push(signal(Level::Warn, "Branch not pushed")),
            _ => out.push(signal(Level::Info, "No PR yet")),
        }
        if let Some(n) = b.behind.filter(|n| *n > 0) {
            out.push(signal(
                Level::Info,
                format!("{} behind base", plural(n, "commit")),
            ));
        }
        return out;
    };

    if pr.is_merged() {
        out.push(signal(Level::Ok, "Merged"));
        if b.local || b.remote {
            out.push(signal(Level::Info, "Branch can be deleted"));
        }
        return out;
    }
    if !pr.is_open() {
        out.push(signal(Level::Info, "Closed without merging"));
        return out;
    }

    if pr.is_draft {
        out.push(signal(
            Level::Info,
            "Draft: mark ready for review when done",
        ));
    }
    if !pr.checks.failing.is_empty() {
        out.push(signal(
            Level::Bad,
            format!(
                "Checks failing: {}",
                names(
                    &pr.checks
                        .failing
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                )
            ),
        ));
    } else if pr.checks.pending > 0 {
        out.push(signal(
            Level::Info,
            format!("{} running", plural(pr.checks.pending as u32, "check")),
        ));
    }
    if has_conflicts(pr) {
        out.push(signal(Level::Bad, "Merge conflicts"));
    } else if pr.merge_state == "BEHIND" {
        out.push(signal(Level::Warn, "Behind base: update the branch"));
    }
    if !pr.changes_requested_by.is_empty() {
        out.push(signal(
            Level::Bad,
            format!("Changes requested by {}", names(&pr.changes_requested_by)),
        ));
    }
    if pr.unresolved_threads > 0 {
        out.push(signal(
            Level::Warn,
            format!(
                "{} unresolved {}",
                pr.unresolved_threads,
                if pr.unresolved_threads == 1 {
                    "comment thread"
                } else {
                    "comment threads"
                }
            ),
        ));
    }
    if !pr.approved_by.is_empty() {
        out.push(signal(
            Level::Ok,
            format!("Approved by {}", names(&pr.approved_by)),
        ));
    } else if !pr.is_draft && pr.changes_requested_by.is_empty() {
        out.push(signal(Level::Info, "Awaiting review"));
    }
    if is_ready(pr) {
        out.push(signal(Level::Ok, "Ready to merge"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulls::{Checks, FailingCheck};

    fn pr() -> PrDetail {
        PrDetail {
            repo: "o/platform".into(),
            number: 411,
            title: "t".into(),
            url: "u".into(),
            state: "OPEN".into(),
            is_draft: false,
            head: "feat/x-318".into(),
            base: "main".into(),
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            updated_at: String::new(),
            checks: Checks {
                state: Some("SUCCESS".into()),
                failing: vec![],
                pending: 0,
            },
            unresolved_threads: 0,
            approved_by: vec![],
            changes_requested_by: vec![],
            review_requests: 0,
        }
    }

    fn texts(b: &BranchWork) -> Vec<String> {
        next_steps(b).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn matches_whole_numbers_only() {
        let numbers = BTreeSet::from([318, 327]);
        assert_eq!(
            numbers_in_branch("feat/fix-header-318", &numbers),
            vec![318]
        );
        assert_eq!(
            numbers_in_branch("feat/x-327-318", &numbers),
            vec![327, 318]
        );
        assert!(numbers_in_branch("feat/x-3180", &numbers).is_empty());
        assert!(numbers_in_branch("release/31", &numbers).is_empty());
    }

    #[test]
    fn open_pr_with_problems() {
        let mut p = pr();
        p.checks.failing = ["review", "lint"]
            .map(|name| FailingCheck {
                name: name.into(),
                url: None,
            })
            .into();
        p.unresolved_threads = 3;
        p.approved_by = vec!["reviewer1".into()];
        p.merge_state = "BEHIND".into();
        let b = BranchWork {
            pull_requests: vec![p],
            unpushed: Some(2),
            ..Default::default()
        };
        assert_eq!(
            texts(&b),
            vec![
                "2 commits not pushed",
                "Checks failing: review, lint",
                "Behind base: update the branch",
                "3 unresolved comment threads",
                "Approved by reviewer1",
            ]
        );
    }

    #[test]
    fn ready_to_merge() {
        let mut p = pr();
        p.approved_by = vec!["a".into()];
        let b = BranchWork {
            pull_requests: vec![p],
            ..Default::default()
        };
        assert_eq!(texts(&b), vec!["Approved by a", "Ready to merge"]);
    }

    #[test]
    fn draft_awaits_nothing_and_conflicts_are_bad() {
        let mut p = pr();
        p.is_draft = true;
        p.mergeable = "CONFLICTING".into();
        let b = BranchWork {
            pull_requests: vec![p],
            ..Default::default()
        };
        let s = next_steps(&b);
        assert_eq!(s[0].text, "Draft: mark ready for review when done");
        assert_eq!(
            (s[1].level, s[1].text.as_str()),
            (Level::Bad, "Merge conflicts")
        );
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn branches_without_prs() {
        let b = BranchWork {
            local: true,
            remote: false,
            ahead: Some(2),
            behind: Some(5),
            ..Default::default()
        };
        assert_eq!(
            texts(&b),
            vec!["Branch not pushed", "5 commits behind base"]
        );
        let b = BranchWork {
            local: true,
            remote: true,
            ahead: Some(0),
            ..Default::default()
        };
        assert_eq!(texts(&b), vec!["No commits yet"]);
        let b = BranchWork {
            remote: true,
            ahead: Some(3),
            behind: Some(0),
            ..Default::default()
        };
        assert_eq!(texts(&b), vec!["No PR yet"]);
    }

    #[test]
    fn finished_work() {
        let mut p = pr();
        p.state = "MERGED".into();
        let b = BranchWork {
            remote: true,
            pull_requests: vec![p.clone()],
            ..Default::default()
        };
        assert_eq!(texts(&b), vec!["Merged", "Branch can be deleted"]);
        p.state = "CLOSED".into();
        let b = BranchWork {
            pull_requests: vec![p],
            ..Default::default()
        };
        assert_eq!(texts(&b), vec!["Closed without merging"]);
    }

    fn verdict_of(edit: impl FnOnce(&mut PrDetail)) -> Option<Verdict> {
        let mut p = pr();
        p.approved_by = vec!["a".into()];
        edit(&mut p);
        verdict(&BranchWork {
            pull_requests: vec![p],
            ..Default::default()
        })
    }

    #[test]
    fn verdicts() {
        use Verdict::*;
        assert_eq!(verdict(&BranchWork::default()), None);
        assert_eq!(verdict_of(|p| p.state = "MERGED".into()), None);
        assert_eq!(verdict_of(|_| {}), Some(Ready));
        let failing = FailingCheck {
            name: "ci".into(),
            url: None,
        };
        assert_eq!(
            verdict_of(|p| {
                p.is_draft = true;
                p.checks.failing = vec![failing];
            }),
            Some(NeedsAttention)
        );
        assert_eq!(
            verdict_of(|p| p.mergeable = "CONFLICTING".into()),
            Some(NeedsAttention)
        );
        assert_eq!(
            verdict_of(|p| p.merge_state = "DIRTY".into()),
            Some(NeedsAttention)
        );
        assert_eq!(
            verdict_of(|p| p.changes_requested_by = vec!["b".into()]),
            Some(NeedsAttention)
        );
        assert_eq!(verdict_of(|p| p.is_draft = true), Some(Draft));
        assert_eq!(verdict_of(|p| p.approved_by.clear()), Some(InReview));
        assert_eq!(verdict_of(|p| p.review_requests = 1), Some(InReview));
        assert_eq!(
            verdict_of(|p| p.merge_state = "BLOCKED".into()),
            Some(InReview)
        );
        assert_eq!(
            verdict_of(|p| p.merge_state = "UNKNOWN".into()),
            Some(InReview)
        );
        assert_eq!(verdict_of(|p| p.unresolved_threads = 1), Some(InReview));
        assert_eq!(verdict_of(|p| p.checks.pending = 1), Some(InReview));
    }

    #[test]
    fn ready_signal_matches_the_verdict() {
        let mut p = pr();
        p.approved_by = vec!["a".into()];
        p.review_requests = 1;
        let b = BranchWork {
            pull_requests: vec![p],
            ..Default::default()
        };
        assert_eq!(texts(&b), vec!["Approved by a"]);
        assert_eq!(verdict(&b), Some(Verdict::InReview));
    }

    #[test]
    fn ranks_by_verdict_then_waiting_then_finished() {
        let with = |state: &str, draft: bool| {
            let mut p = pr();
            p.state = state.into();
            p.is_draft = draft;
            let mut b = BranchWork {
                pull_requests: vec![p],
                ..Default::default()
            };
            b.verdict = verdict(&b);
            b
        };
        let ranks: Vec<u8> = [
            with("MERGED", false),
            BranchWork::default(),
            with("CLOSED", false),
            with("OPEN", false),
            with("OPEN", true),
        ]
        .iter()
        .map(rank)
        .collect();
        assert_eq!(ranks, vec![6, 4, 5, 3, 1]);
    }

    #[test]
    fn subject_deserializes_from_the_ui_shapes() {
        let issue: WorkSubject = serde_json::from_str(r#"{"kind":"issue","id":"I_kw1"}"#).unwrap();
        assert_eq!(issue, WorkSubject::Issue { id: "I_kw1".into() });
        let item: WorkSubject =
            serde_json::from_str(r#"{"kind":"item","name":"brisk-otter"}"#).unwrap();
        assert_eq!(
            item,
            WorkSubject::Item {
                name: "brisk-otter".into()
            }
        );
    }

    fn worker(
        id: &str,
        issue: Option<&str>,
        item: Option<&str>,
        worktrees: &[(&str, &str)],
    ) -> WorkerRecord {
        use crate::state::{BranchOrigin, IssueRef, ItemRef, WorkerStatus, WorktreeRecord};
        WorkerRecord {
            id: id.into(),
            project: "p".into(),
            issue: issue.map(|id| IssueRef {
                id: id.into(),
                number: 0,
                repo: "o/r".into(),
                title: "t".into(),
                url: "u".into(),
            }),
            item: item.map(|name| ItemRef {
                name: name.into(),
                title: "t".into(),
                description: String::new(),
            }),
            pr: None,
            branch: String::new(),
            slot: 1,
            ports: Default::default(),
            dir: "w".into(),
            compose_project: "tw-x".into(),
            status: WorkerStatus::Ready,
            claude_session: "s".into(),
            claude_started: false,
            error: None,
            created_at: 0,
            worktrees: worktrees
                .iter()
                .map(|(repo, branch)| WorktreeRecord {
                    repo: repo.to_string(),
                    branch: branch.to_string(),
                    origin: BranchOrigin::New,
                })
                .collect(),
            stopped: false,
        }
    }

    fn repo(name: &str, nwo: Option<&str>, branches: &[&str]) -> RepoBranches {
        RepoBranches {
            name: name.into(),
            name_with_owner: nwo.map(String::from),
            branches: branches
                .iter()
                .map(|b| {
                    (
                        b.to_string(),
                        BranchRefs {
                            local: true,
                            remote: false,
                        },
                    )
                })
                .collect(),
        }
    }

    fn issue_scope(ids: &[&str], numbers: &[u64], wanted: &[(&str, &str)]) -> Scope {
        Scope::Issue {
            ids: ids.iter().map(|s| s.to_string()).collect(),
            numbers: numbers.iter().copied().collect(),
            wanted: wanted
                .iter()
                .map(|(r, b)| (r.to_string(), b.to_string()))
                .collect(),
        }
    }

    fn shown(l: &Lookups) -> Vec<(&str, &str)> {
        l.show
            .iter()
            .map(|(r, b, _)| (r.as_str(), b.as_str()))
            .collect()
    }

    #[test]
    fn issue_shows_its_workers_branches_whatever_their_names() {
        let repos = [
            repo(
                "api",
                Some("o/api"),
                &["main", "feat/x-318", "spike", "linked", "other"],
            ),
            repo("web", None, &["spike"]),
        ];
        let workers = [
            worker(
                "p-318",
                Some("I318"),
                None,
                &[("api", "spike"), ("web", "spike")],
            ),
            worker("p-9", Some("I9"), None, &[("api", "other")]),
        ];
        let scope = issue_scope(&["I318"], &[318], &[("O/API", "linked")]);
        let l = plan_lookups(&scope, &repos, &workers, &CheckedOut::new());
        assert_eq!(
            shown(&l),
            vec![
                ("api", "feat/x-318"),
                ("api", "linked"),
                ("api", "spike"),
                ("web", "spike")
            ]
        );
        assert_eq!(l.show[0].2, vec![318]);
        assert!(l.show[2].2.is_empty());
        assert_eq!(
            l.by_head,
            vec![
                ("o/api".to_string(), "feat/x-318".to_string()),
                ("o/api".to_string(), "linked".to_string()),
                ("o/api".to_string(), "spike".to_string()),
            ]
        );
        assert!(l.by_number.is_empty());
    }

    #[test]
    fn story_shows_its_subtasks_worker_branches() {
        let repos = [repo("api", Some("o/api"), &["fix-thing", "unrelated"])];
        let workers = [worker("p-12", Some("Isub"), None, &[("api", "fix-thing")])];
        let scope = issue_scope(&["Istory", "Isub"], &[10, 12], &[]);
        let l = plan_lookups(&scope, &repos, &workers, &CheckedOut::new());
        assert_eq!(shown(&l), vec![("api", "fix-thing")]);
    }

    #[test]
    fn custom_item_shows_only_its_workers_branches() {
        let repos = [repo("api", Some("o/api"), &["feat/42-x", "mine", "theirs"])];
        let workers = [
            worker("p-calm-yak", None, Some("calm-yak"), &[("api", "mine")]),
            worker("p-bold-elk", None, Some("bold-elk"), &[("api", "theirs")]),
        ];
        let scope = Scope::Item {
            name: "calm-yak".into(),
            pr: None,
        };
        let l = plan_lookups(&scope, &repos, &workers, &CheckedOut::new());
        assert_eq!(shown(&l), vec![("api", "mine")]);
        assert_eq!(l.by_head, vec![("o/api".to_string(), "mine".to_string())]);
        assert!(l.by_number.is_empty());
    }

    #[test]
    fn item_shows_the_branch_its_worktree_switched_to() {
        let repos = [repo(
            "api",
            Some("o/api"),
            &["calm-yak", "feat/foo", "theirs"],
        )];
        let workers = [
            worker("p-calm-yak", None, Some("calm-yak"), &[("api", "calm-yak")]),
            worker("p-bold-elk", None, Some("bold-elk"), &[("api", "theirs")]),
        ];
        let checked_out = CheckedOut::from([
            (("p-calm-yak".into(), "api".into()), "feat/foo".into()),
            (("p-bold-elk".into(), "api".into()), "theirs".into()),
        ]);
        let scope = Scope::Item {
            name: "calm-yak".into(),
            pr: None,
        };
        let l = plan_lookups(&scope, &repos, &workers, &checked_out);
        assert_eq!(shown(&l), vec![("api", "calm-yak"), ("api", "feat/foo")]);
        assert_eq!(
            l.by_head,
            vec![
                ("o/api".to_string(), "calm-yak".to_string()),
                ("o/api".to_string(), "feat/foo".to_string()),
            ]
        );
    }

    #[test]
    fn pr_item_without_a_worktree_requests_its_pr_by_number() {
        let repos = [repo("api", Some("o/api"), &["feat/export"])];
        let scope = Scope::Item {
            name: "calm-yak".into(),
            pr: Some(("o/api".into(), 42)),
        };
        let l = plan_lookups(&scope, &repos, &[], &CheckedOut::new());
        assert_eq!(
            l,
            Lookups {
                by_number: vec![("o/api".into(), 42)],
                ..Default::default()
            }
        );
    }

    fn project_with(source: &std::path::Path) -> ProjectConfig {
        toml::from_str(&format!(
            "name = \"p\"\nmatch = {{ owner = \"o\" }}\n[[repos]]\nname = \"r\"\nsource = '{}'\nbase = \"origin/main\"\n",
            source.display()
        ))
        .unwrap()
    }

    /// A clone of a local bare "origin" with `main` and a pushed `feat/x-318`.
    fn cloned_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let run = |cwd: &std::path::Path, args: &[&str]| {
            crate::process::run("git", args, Some(cwd)).unwrap()
        };
        let origin = dir.path().join("origin.git");
        let seed = dir.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run(
            dir.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                &origin.to_string_lossy(),
            ],
        );
        run(&seed, &["init", "-q", "-b", "main"]);
        run(&seed, &["config", "user.email", "t@example.com"]);
        run(&seed, &["config", "user.name", "t"]);
        run(&seed, &["commit", "-q", "--allow-empty", "-m", "a"]);
        run(
            &seed,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        run(&seed, &["push", "-q", "origin", "main"]);
        run(&seed, &["checkout", "-q", "-b", "feat/x-318"]);
        run(&seed, &["commit", "-q", "--allow-empty", "-m", "b"]);
        run(&seed, &["push", "-q", "origin", "feat/x-318"]);
        run(&seed, &["checkout", "-q", "main"]);
        // Reads as a GitHub repo (for the PR check) while pushes still go to the local origin.
        run(
            &seed,
            &["remote", "set-url", "origin", "https://github.com/o/r.git"],
        );
        run(
            &seed,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                &origin.to_string_lossy(),
            ],
        );
        (dir, seed)
    }

    #[test]
    fn deletes_local_and_remote_branch() {
        let (_d, repo) = cloned_repo();
        let project = project_with(&repo);
        let deleted = delete_branch(&project, "r", "feat/x-318", &|_, _| Ok(None)).unwrap();
        assert_eq!(deleted, vec!["origin/feat/x-318", "local feat/x-318"]);
        assert!(!git::branches(&repo).unwrap().contains_key("feat/x-318"));
        let origin = repo.parent().unwrap().join("origin.git");
        let remote = crate::process::run(
            "git",
            &["ls-remote", "--heads", &origin.to_string_lossy()],
            Some(&repo),
        )
        .unwrap();
        assert!(!remote.contains("feat/x-318"));
    }

    #[test]
    fn refuses_open_pr_checked_out_and_base_branches() {
        let (_d, repo) = cloned_repo();
        let project = project_with(&repo);
        let err = delete_branch(&project, "r", "feat/x-318", &|_, _| Ok(Some(42))).unwrap_err();
        assert!(err.to_string().contains("PR #42 is open"), "{err}");
        assert!(delete_branch(&project, "r", "main", &|_, _| Ok(None)).is_err());
        crate::process::run("git", &["checkout", "-q", "feat/x-318"], Some(&repo)).unwrap();
        let err = delete_branch(&project, "r", "feat/x-318", &|_, _| Ok(None)).unwrap_err();
        assert!(err.to_string().contains("checked out"), "{err}");
        assert!(
            git::branches(&repo).unwrap()["feat/x-318"].remote,
            "nothing was deleted"
        );
    }

    #[test]
    fn counts_commits_only_on_origin_as_ahead() {
        let (_d, repo) = cloned_repo();
        let project = project_with(&repo);
        crate::process::run("git", &["branch", "-f", "feat/x-318", "main"], Some(&repo)).unwrap();
        let at = BranchRefs {
            local: true,
            remote: true,
        };
        let b = branch_work(
            &project.repos[0],
            None,
            "feat/x-318".into(),
            at,
            None,
            vec![],
        );
        assert_eq!(b.ahead, Some(1));
        assert_eq!(b.unpushed, Some(0));
    }

    #[test]
    fn git_counts_against_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let run = |args: &[&str]| crate::process::run("git", args, Some(repo)).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        run(&["commit", "-q", "--allow-empty", "-m", "a"]);
        run(&["checkout", "-q", "-b", "feat/x-318"]);
        run(&["commit", "-q", "--allow-empty", "-m", "b"]);
        run(&["commit", "-q", "--allow-empty", "-m", "c"]);
        run(&["checkout", "-q", "main"]);
        run(&["commit", "-q", "--allow-empty", "-m", "d"]);
        assert_eq!(
            git::ahead_behind(repo, "main", "feat/x-318").unwrap(),
            (2, 1)
        );
        let branches = git::branches(repo).unwrap();
        assert!(branches["feat/x-318"].local && !branches["feat/x-318"].remote);
        assert!(
            git::last_commit(repo, "feat/x-318")
                .unwrap()
                .ends_with("· t")
        );
    }
}
