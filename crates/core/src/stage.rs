use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::existing::{Verdict, pr_verdict};
use crate::git;
use crate::local;
use crate::pulls::{self, PrDetail};
use crate::state::WorkerStatus;
use crate::worker::Workspace;

/// Variant order is how far the work has got; `NeedsAttention` ranks with `InReview` but beats
/// every stage in `combine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Building,
    PrDraft,
    InReview,
    NeedsAttention,
    Ready,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkerStage {
    pub stage: Option<Stage>,
}

pub fn repo_stage(prs: &[PrDetail], ahead: bool) -> Option<Stage> {
    if let Some(pr) = prs.iter().filter(|p| p.is_open()).max_by_key(|p| p.number) {
        return pr_verdict(pr).map(|v| match v {
            Verdict::Draft => Stage::PrDraft,
            Verdict::InReview => Stage::InReview,
            Verdict::NeedsAttention => Stage::NeedsAttention,
            Verdict::Ready => Stage::Ready,
        });
    }
    if prs.iter().any(|p| p.is_merged()) {
        return Some(Stage::Merged);
    }
    ahead.then_some(Stage::Building)
}

pub fn combine(stages: impl IntoIterator<Item = Stage>) -> Option<Stage> {
    let stages: Vec<Stage> = stages.into_iter().collect();
    if stages.contains(&Stage::NeedsAttention) {
        return Some(Stage::NeedsAttention);
    }
    stages.into_iter().min()
}

fn is_ahead(source: &Path, base: &str, branch: &str) -> bool {
    git::ahead_behind(source, base, branch).is_ok_and(|(ahead, _)| ahead > 0)
}

struct Target {
    worker: String,
    source: PathBuf,
    base: String,
    name_with_owner: Option<String>,
    branch: String,
    pr_number: Option<u64>,
}

fn lookup_branch(checked_out: Option<String>, recorded: &str, base: &str) -> String {
    let base_branch = base.split_once('/').map_or(base, |(_, b)| b);
    checked_out
        .filter(|b| b != base_branch)
        .unwrap_or_else(|| recorded.to_string())
}

#[derive(Default)]
struct Lookups {
    by_head: BTreeMap<(String, String), Vec<PrDetail>>,
    by_number: BTreeMap<(String, u64), Option<PrDetail>>,
    failed_repos: BTreeSet<String>,
    failed_numbers: BTreeSet<(String, u64)>,
}

impl Lookups {
    /// None when a lookup this target needs failed.
    fn prs(&self, t: &Target) -> Option<Vec<PrDetail>> {
        let Some(nwo) = &t.name_with_owner else {
            return Some(Vec::new());
        };
        match t.pr_number {
            Some(number) => {
                let key = (nwo.clone(), number);
                if self.failed_numbers.contains(&key) {
                    return None;
                }
                Some(
                    self.by_number
                        .get(&key)
                        .cloned()
                        .flatten()
                        .into_iter()
                        .collect(),
                )
            }
            None => {
                if self.failed_repos.contains(nwo) {
                    return None;
                }
                Some(
                    self.by_head
                        .get(&(nwo.clone(), t.branch.clone()))
                        .cloned()
                        .unwrap_or_default(),
                )
            }
        }
    }
}

/// `repos` pairs a worker with one repo's stage, or with None when that stage is unknown. A worker
/// with any unknown repo is left out.
fn by_worker(
    workers: impl IntoIterator<Item = String>,
    repos: impl IntoIterator<Item = (String, Option<Option<Stage>>)>,
) -> BTreeMap<String, WorkerStage> {
    let mut stages: BTreeMap<String, Option<Vec<Stage>>> =
        workers.into_iter().map(|w| (w, Some(Vec::new()))).collect();
    for (worker, stage) in repos {
        let entry = stages.entry(worker).or_insert_with(|| Some(Vec::new()));
        match stage {
            None => *entry = None,
            Some(stage) => {
                if let Some(known) = entry {
                    known.extend(stage);
                }
            }
        }
    }
    stages
        .into_iter()
        .filter_map(|(id, s)| s.map(|s| (id, WorkerStage { stage: combine(s) })))
        .collect()
}

/// Leaves out workers whose stage is unknown because a GitHub lookup failed.
pub fn worker_stages(ws: &Workspace) -> Result<BTreeMap<String, WorkerStage>> {
    let workers: Vec<_> = ws
        .state
        .list()?
        .into_iter()
        .filter(|w| w.status == WorkerStatus::Ready)
        .collect();

    let mut targets = Vec::new();
    for w in &workers {
        let Some(project) = ws.config.project(&w.project) else {
            continue;
        };
        for t in &w.worktrees {
            let Some((repo, source)) = project
                .repos
                .iter()
                .find(|r| r.name == t.repo)
                .and_then(|r| Some((r, r.source.clone()?)))
            else {
                continue;
            };
            let name_with_owner = git::origin_name_with_owner(&source).ok();
            let pr_number =
                w.pr.as_ref()
                    .filter(|pr| {
                        name_with_owner
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(&pr.repo))
                    })
                    .map(|pr| pr.number);
            targets.push(Target {
                worker: w.id.clone(),
                source,
                base: repo.base.clone(),
                branch: lookup_branch(
                    local::checked_out(&w.dir.join(&t.repo)),
                    &t.branch,
                    &repo.base,
                ),
                name_with_owner,
                pr_number,
            });
        }
    }

    let mut by_repo: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for t in &targets {
        if let (Some(nwo), None) = (&t.name_with_owner, t.pr_number) {
            by_repo.entry(nwo).or_default().insert(&t.branch);
        }
    }
    let mut lookups = Lookups::default();
    for (nwo, branches) in &by_repo {
        let batch: Vec<(String, String)> = branches
            .iter()
            .map(|b| (nwo.to_string(), b.to_string()))
            .collect();
        match pulls::prs_for_branches(&batch) {
            Ok(found) => lookups.by_head.extend(found),
            Err(e) => {
                eprintln!("worker stages: looking up PRs in {nwo}: {e:#}");
                lookups.failed_repos.insert(nwo.to_string());
            }
        }
    }

    for t in &targets {
        let (Some(nwo), Some(number)) = (&t.name_with_owner, t.pr_number) else {
            continue;
        };
        let key = (nwo.clone(), number);
        if lookups.by_number.contains_key(&key) || lookups.failed_numbers.contains(&key) {
            continue;
        }
        match pulls::pr_by_number(nwo, number) {
            Ok(pr) => {
                lookups.by_number.insert(key, pr);
            }
            Err(e) => {
                eprintln!("worker stages: looking up {nwo}#{number}: {e:#}");
                lookups.failed_numbers.insert(key);
            }
        }
    }

    let repos = targets.iter().map(|t| {
        let stage = lookups.prs(t).map(|prs| {
            let has_pr = prs.iter().any(|p| p.is_open() || p.is_merged());
            let ahead = !has_pr && is_ahead(&t.source, &t.base, &t.branch);
            repo_stage(&prs, ahead)
        });
        (t.worker.clone(), stage)
    });
    Ok(by_worker(workers.iter().map(|w| w.id.clone()), repos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process;
    use crate::pulls::Checks;

    fn pr(number: u64, state: &str) -> PrDetail {
        PrDetail {
            repo: "o/r".into(),
            number,
            title: "t".into(),
            url: "u".into(),
            state: state.into(),
            is_draft: false,
            head: "feat/x".into(),
            base: "main".into(),
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            updated_at: String::new(),
            checks: Checks::default(),
            unresolved_threads: 0,
            approved_by: vec!["a".into()],
            changes_requested_by: vec![],
            review_requests: 0,
        }
    }

    fn open(edit: impl FnOnce(&mut PrDetail)) -> PrDetail {
        let mut p = pr(2, "OPEN");
        edit(&mut p);
        p
    }

    #[test]
    fn open_prs_map_through_their_verdict() {
        use Stage::*;
        assert_eq!(repo_stage(&[open(|_| {})], false), Some(Ready));
        assert_eq!(
            repo_stage(&[open(|p| p.is_draft = true)], false),
            Some(PrDraft)
        );
        assert_eq!(
            repo_stage(&[open(|p| p.approved_by.clear())], true),
            Some(InReview)
        );
        assert_eq!(
            repo_stage(&[open(|p| p.mergeable = "CONFLICTING".into())], false),
            Some(NeedsAttention)
        );
    }

    #[test]
    fn an_open_pr_beats_a_merged_one() {
        assert_eq!(
            repo_stage(&[pr(1, "MERGED"), open(|p| p.is_draft = true)], false),
            Some(Stage::PrDraft)
        );
    }

    #[test]
    fn merged_closed_and_branch_only_stages() {
        assert_eq!(repo_stage(&[pr(1, "MERGED")], false), Some(Stage::Merged));
        assert_eq!(repo_stage(&[pr(1, "CLOSED")], false), None);
        assert_eq!(repo_stage(&[pr(1, "CLOSED")], true), Some(Stage::Building));
        assert_eq!(repo_stage(&[], true), Some(Stage::Building));
        assert_eq!(repo_stage(&[], false), None);
    }

    #[test]
    fn combine_takes_the_least_advanced_unless_one_needs_attention() {
        use Stage::*;
        assert_eq!(combine([Merged, Building]), Some(Building));
        assert_eq!(combine([Ready, InReview]), Some(InReview));
        assert_eq!(
            combine([Building, NeedsAttention, Merged]),
            Some(NeedsAttention)
        );
        assert_eq!(combine([]), None);
    }

    fn target(worker: &str, branch: &str, pr_number: Option<u64>) -> Target {
        Target {
            worker: worker.into(),
            source: PathBuf::new(),
            base: "origin/main".into(),
            name_with_owner: Some("o/r".into()),
            branch: branch.into(),
            pr_number,
        }
    }

    #[test]
    fn a_failed_lookup_fails_only_the_targets_that_needed_it() {
        let mut lookups = Lookups::default();
        lookups
            .by_head
            .insert(("o/r".into(), "feat/x".into()), vec![pr(1, "MERGED")]);
        lookups.failed_numbers.insert(("o/r".into(), 5));
        lookups
            .by_number
            .insert(("o/r".into(), 6), Some(pr(6, "MERGED")));

        assert_eq!(lookups.prs(&target("a", "feat/x", None)).unwrap().len(), 1);
        assert!(lookups.prs(&target("b", "pr-5", Some(5))).is_none());
        assert_eq!(lookups.prs(&target("c", "pr-6", Some(6))).unwrap().len(), 1);

        lookups.failed_repos.insert("o/r".into());
        assert!(lookups.prs(&target("a", "feat/x", None)).is_none());
        assert_eq!(lookups.prs(&target("c", "pr-6", Some(6))).unwrap().len(), 1);

        let mut no_origin = target("d", "feat/x", None);
        no_origin.name_with_owner = None;
        assert!(lookups.prs(&no_origin).unwrap().is_empty());
    }

    #[test]
    fn a_worker_with_an_unknown_repo_is_left_out() {
        use Stage::*;
        let stages = by_worker(
            ["a", "b", "c", "d"].map(String::from),
            [
                ("a".into(), Some(Some(Merged))),
                ("a".into(), None),
                ("a".into(), Some(Some(Building))),
                ("b".into(), Some(Some(Merged))),
                ("b".into(), Some(Some(Building))),
                ("c".into(), Some(None)),
            ],
        );
        assert_eq!(
            stages.keys().map(String::as_str).collect::<Vec<_>>(),
            ["b", "c", "d"]
        );
        assert_eq!(stages["b"].stage, Some(Building));
        assert_eq!(stages["c"].stage, None);
        assert_eq!(stages["d"].stage, None);
    }

    #[test]
    fn a_worktree_back_on_its_base_uses_the_recorded_branch() {
        assert_eq!(
            lookup_branch(Some("main".into()), "feat/x", "origin/main"),
            "feat/x"
        );
        assert_eq!(
            lookup_branch(Some("feat/y".into()), "feat/x", "origin/main"),
            "feat/y"
        );
        assert_eq!(lookup_branch(None, "feat/x", "origin/main"), "feat/x");
        assert_eq!(
            lookup_branch(Some("release/1".into()), "feat/x", "origin/release/1"),
            "feat/x"
        );
    }

    #[test]
    fn ahead_counts_commits_past_the_remote_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
            all.extend_from_slice(args);
            process::run("git", &all, Some(repo)).unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&["branch", "fresh"]);
        git(&["commit", "-q", "--allow-empty", "-m", "work"]);

        assert!(is_ahead(repo, "origin/main", "main"));
        assert!(!is_ahead(repo, "main", "main"));
        assert!(!is_ahead(repo, "origin/main", "fresh"));
        assert!(!is_ahead(repo, "origin/main", "missing"));
    }
}
