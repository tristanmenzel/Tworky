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
            let Some(repo) = project.repos.iter().find(|r| r.name == t.repo) else {
                continue;
            };
            let name_with_owner = git::origin_name_with_owner(&repo.source).ok();
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
                source: repo.source.clone(),
                base: repo.base.clone(),
                branch: local::checked_out(&w.dir.join(&t.repo))
                    .unwrap_or_else(|| t.branch.clone()),
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
    let mut by_head: BTreeMap<(String, String), Vec<PrDetail>> = BTreeMap::new();
    let mut failed: BTreeSet<&str> = BTreeSet::new();
    for (nwo, branches) in &by_repo {
        let lookups: Vec<(String, String)> = branches
            .iter()
            .map(|b| (nwo.to_string(), b.to_string()))
            .collect();
        match pulls::prs_for_branches(&lookups) {
            Ok(found) => by_head.extend(found),
            Err(e) => {
                eprintln!("worker stages: looking up PRs in {nwo}: {e:#}");
                failed.insert(nwo);
            }
        }
    }

    let mut by_number: BTreeMap<(&str, u64), Option<PrDetail>> = BTreeMap::new();
    for t in &targets {
        let (Some(nwo), Some(number)) = (&t.name_with_owner, t.pr_number) else {
            continue;
        };
        if by_number.contains_key(&(nwo.as_str(), number)) {
            continue;
        }
        match pulls::pr_by_number(nwo, number) {
            Ok(pr) => {
                by_number.insert((nwo, number), pr);
            }
            Err(e) => {
                eprintln!("worker stages: looking up {nwo}#{number}: {e:#}");
                failed.insert(nwo);
            }
        }
    }

    let mut stages: BTreeMap<String, Vec<Stage>> =
        workers.iter().map(|w| (w.id.clone(), Vec::new())).collect();
    for t in &targets {
        let prs: Vec<PrDetail> = match (&t.name_with_owner, t.pr_number) {
            (Some(nwo), _) if failed.contains(nwo.as_str()) => continue,
            (Some(nwo), Some(number)) => by_number
                .get(&(nwo.as_str(), number))
                .cloned()
                .flatten()
                .into_iter()
                .collect(),
            (Some(nwo), None) => by_head
                .get(&(nwo.clone(), t.branch.clone()))
                .cloned()
                .unwrap_or_default(),
            (None, _) => Vec::new(),
        };
        let has_pr = prs.iter().any(|p| p.is_open() || p.is_merged());
        let ahead = !has_pr && is_ahead(&t.source, &t.base, &t.branch);
        if let Some(stage) = repo_stage(&prs, ahead) {
            stages.entry(t.worker.clone()).or_default().push(stage);
        }
    }
    Ok(stages
        .into_iter()
        .map(|(id, s)| (id, WorkerStage { stage: combine(s) }))
        .collect())
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
