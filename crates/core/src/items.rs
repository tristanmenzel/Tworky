use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{self, NewRepo};
use crate::git;
use crate::github::{self, IsSubtask};
use crate::hierarchy::IssueIndex;
use crate::state::{IssueRef, Item, ItemSource, PrRef, WorkerRecord};
use crate::worker::{Workspace, worker_claude_md};

const CLONE_SEARCH_DEPTH: usize = 3;

#[derive(Debug, Clone, Serialize)]
pub struct RepoCandidate {
    pub name_with_owner: String,
    pub description: String,
    /// The existing clone, or `None` if picking it means cloning it first.
    pub local: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoSearch {
    pub candidates: Vec<RepoCandidate>,
    /// GitHub search failed; `candidates` holds only local clones.
    pub search_error: Option<String>,
}

/// Which project a new item goes in when its repo belongs to none yet.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectChoice {
    /// The project the repo already belongs to; otherwise the user is asked.
    #[default]
    Infer,
    /// Add the repo to this project.
    Existing { name: String },
    /// Make a project for the repo, to be set up by its project setup session.
    New { name: Option<String> },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AddOutcome {
    Added {
        item: Box<Item>,
        /// Set when a project was made for the item, which still needs setting up.
        new_project: Option<String>,
    },
    /// No project covers the repo; ask the user which to use.
    NeedsProject { repo: String },
}

/// A search result, with the work item it's already on the list as.
#[derive(Debug, Clone, Serialize)]
pub struct Listed<T> {
    #[serde(flatten)]
    pub hit: T,
    pub item: Option<String>,
}

struct Placed {
    project: String,
    created: bool,
}

impl Placed {
    fn added(self, item: Item) -> AddOutcome {
        AddOutcome::Added {
            item: Box::new(item),
            new_project: self.created.then_some(self.project),
        }
    }
}

const ADJECTIVES: [&str; 32] = [
    "amber", "brave", "brisk", "calm", "clever", "crisp", "dapper", "eager", "fair", "fleet",
    "gentle", "glad", "hardy", "humble", "jolly", "keen", "lively", "lucky", "merry", "mellow",
    "nimble", "plucky", "quick", "quiet", "rapid", "sly", "snug", "spry", "sunny", "swift", "tidy",
    "witty",
];

const ANIMALS: [&str; 32] = [
    "badger", "bison", "crane", "dingo", "egret", "elk", "falcon", "ferret", "gecko", "heron",
    "ibis", "jackal", "koala", "lemur", "lynx", "marten", "moose", "newt", "numbat", "ocelot",
    "otter", "panda", "quokka", "raven", "seal", "stoat", "tapir", "tern", "vole", "walrus",
    "wombat", "yak",
];

/// An adjective-animal pair not in `taken`, falling back to a numbered one when most are used.
pub fn random_name(taken: &HashSet<String>) -> String {
    for _ in 0..64 {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        let name = format!(
            "{}-{}",
            ADJECTIVES[bytes[0] as usize % ADJECTIVES.len()],
            ANIMALS[bytes[1] as usize % ANIMALS.len()]
        );
        if !taken.contains(&name) {
            return name;
        }
    }
    (2..)
        .map(|n| format!("brisk-otter-{n}"))
        .find(|n| !taken.contains(n))
        .expect("an unused number exists")
}

/// Keeps matches and their ancestors. With `hide_listed`, a listed match stays only as the ancestor
/// of an unlisted one.
fn offered_tree(
    mut hits: Vec<Listed<github::IssueHit>>,
    hide_listed: bool,
) -> Vec<Listed<github::IssueHit>> {
    if hide_listed {
        for l in hits.iter_mut().filter(|l| l.item.is_some()) {
            l.hit.matched = false;
        }
    }
    let parents: HashMap<&str, &str> = hits
        .iter()
        .filter_map(|l| Some((l.hit.id.as_str(), l.hit.parent_id.as_deref()?)))
        .collect();
    let mut keep: HashSet<String> = HashSet::new();
    for l in hits.iter().filter(|l| l.hit.matched) {
        let mut id = Some(l.hit.id.as_str());
        while let Some(i) = id {
            if !keep.insert(i.to_string()) {
                break;
            }
            id = parents.get(i).copied();
        }
    }
    hits.retain(|l| keep.contains(&l.hit.id));
    hits
}

impl Workspace {
    fn is_covered(&self, name_with_owner: &str) -> bool {
        self.config.project_for_repo(name_with_owner).is_some()
    }

    /// Repos no project covers yet: local clones matching `query` first, then GitHub results.
    pub fn repo_candidates(&self, query: &str) -> RepoSearch {
        let needle = query.trim().to_ascii_lowercase();
        let clones = git::find_clones(&self.config.app.source_roots, CLONE_SEARCH_DEPTH);
        let mut candidates: Vec<RepoCandidate> = clones
            .iter()
            .filter(|(nwo, _)| !self.is_covered(nwo))
            .filter(|(nwo, _)| nwo.to_ascii_lowercase().contains(&needle))
            .map(|(nwo, path)| RepoCandidate {
                name_with_owner: nwo.clone(),
                description: String::new(),
                local: Some(path.clone()),
            })
            .collect();
        let mut search_error = None;
        match github::search_repos(query) {
            Ok(hits) => {
                for hit in hits {
                    if self.is_covered(&hit.full_name) {
                        continue;
                    }
                    match candidates
                        .iter_mut()
                        .find(|c| c.name_with_owner.eq_ignore_ascii_case(&hit.full_name))
                    {
                        Some(local) => local.description = hit.description,
                        None => candidates.push(RepoCandidate {
                            local: clones
                                .iter()
                                .find(|(nwo, _)| nwo.eq_ignore_ascii_case(&hit.full_name))
                                .map(|(_, p)| p.clone()),
                            name_with_owner: hit.full_name,
                            description: hit.description,
                        }),
                    }
                }
            }
            Err(e) => search_error = Some(format!("{e:#}")),
        }
        RepoSearch {
            candidates,
            search_error,
        }
    }

    /// Where `nwo` is cloned under a source root, cloning it into the first root if it isn't yet.
    fn new_repo(&self, nwo: &str, clones: &[(String, PathBuf)]) -> Result<NewRepo> {
        if let Some(p) = self.config.project_for_repo(nwo) {
            bail!("{nwo} already belongs to project {}", p.name);
        }
        let source = match clones.iter().find(|(c, _)| c.eq_ignore_ascii_case(nwo)) {
            Some((_, path)) => path.clone(),
            None => {
                let root = self
                    .config
                    .app
                    .source_roots
                    .first()
                    .context("no source_roots configured to clone into")?;
                let dest = nwo.split('/').fold(root.clone(), |p, part| p.join(part));
                if dest.exists() {
                    bail!("{} exists but isn't a clone of {nwo}", dest.display());
                }
                github::clone_repo(nwo, &dest)?;
                dest
            }
        };
        Ok(NewRepo {
            name_with_owner: nwo.to_string(),
            source,
            base: github::default_base(nwo)?,
        })
    }

    /// Makes a project for repos with no project config, cloning any that aren't under a source
    /// root yet. Returns its name; reopen the workspace to see it.
    pub fn create_repo_project(
        &self,
        names_with_owner: &[String],
        name: Option<&str>,
    ) -> Result<String> {
        let clones = git::find_clones(&self.config.app.source_roots, CLONE_SEARCH_DEPTH);
        let repos = names_with_owner
            .iter()
            .map(|nwo| self.new_repo(nwo, &clones))
            .collect::<Result<Vec<_>>>()?;
        let taken: Vec<String> = self
            .config
            .projects
            .iter()
            .map(|p| p.name.clone())
            .collect();
        config::create_repo_project(&self.config.paths, &taken, &repos, name)
    }

    /// Adds a repo no project covers yet to `project`, cloning it first if needed.
    pub fn add_repo_to_project(&self, project: &str, name_with_owner: &str) -> Result<()> {
        let project = self
            .config
            .project(project)
            .with_context(|| format!("no project named '{project}'"))?;
        let clones = git::find_clones(&self.config.app.source_roots, CLONE_SEARCH_DEPTH);
        let repo = self.new_repo(name_with_owner, &clones)?;
        config::add_repo_to_project(&project.file, &repo)
    }

    /// The project a new item for `repo` goes in, adding the repo to it or making it as `choice`
    /// says. `None` when the repo is unknown and `choice` is `Infer`, so the user has to pick.
    fn project_for_new_item(&self, repo: &str, choice: &ProjectChoice) -> Result<Option<Placed>> {
        let known = self.config.project_for_repo(repo);
        Ok(match (choice, known) {
            (_, Some(p)) => Some(Placed {
                project: p.name.clone(),
                created: false,
            }),
            (ProjectChoice::Infer, None) => None,
            (ProjectChoice::Existing { name }, None) => {
                self.add_repo_to_project(name, repo)?;
                Some(Placed {
                    project: name.clone(),
                    created: false,
                })
            }
            (ProjectChoice::New { name }, None) => Some(Placed {
                project: self.create_repo_project(&[repo.to_string()], name.as_deref())?,
                created: true,
            }),
        })
    }

    /// `owner/repo` of every repo a project names or holds a clone of, plus the repos of issues and
    /// PRs on the list or held by workers (an org's issue tracker is often a repo nobody clones).
    fn known_repos(&self) -> Vec<String> {
        let mut repos: Vec<String> = Vec::new();
        let mut add = |nwo: String| {
            if !repos.iter().any(|r| r.eq_ignore_ascii_case(&nwo)) {
                repos.push(nwo);
            }
        };
        for p in &self.config.projects {
            p.match_rule.repos.iter().cloned().for_each(&mut add);
            p.repos
                .iter()
                .filter_map(|r| git::origin_name_with_owner(&r.source).ok())
                .for_each(&mut add);
        }
        for item in self.state.list_items().unwrap_or_default() {
            match item.source {
                ItemSource::Issue(i) => add(i.repo),
                ItemSource::PullRequest(p) => add(p.repo),
                ItemSource::Custom => {}
            }
        }
        for w in self.state.list().unwrap_or_default() {
            if let Some(i) = w.issue {
                add(i.repo);
            }
        }
        repos
    }

    /// Issues to offer for the work list. Text is searched in repos owned by the viewer, their
    /// organisations and the owners projects know about. With no query, the viewer's open assigned
    /// issues that aren't listed yet.
    pub fn search_issues(&self, query: &str) -> Result<Vec<Listed<github::IssueHit>>> {
        let repos = self.known_repos();
        let mut owners: Vec<String> = github::viewer_owners().unwrap_or_default();
        let project_owners = self
            .config
            .projects
            .iter()
            .filter_map(|p| p.match_rule.owner.clone());
        let repo_owners = repos
            .iter()
            .filter_map(|r| r.split('/').next().map(String::from));
        for o in project_owners.chain(repo_owners) {
            if !owners.iter().any(|x| x.eq_ignore_ascii_case(&o)) {
                owners.push(o);
            }
        }
        let hits = github::search_issues(query, &repos, &owners)?;
        let assigned = matches!(
            github::parse_issue_query(query),
            github::IssueQuery::Assigned
        );
        let listed = hits
            .into_iter()
            .map(|hit| {
                let item = self.state.item_for_issue(&hit.id)?.map(|i| i.name);
                Ok(Listed { hit, item })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(offered_tree(listed, assigned))
    }

    /// Open pull requests waiting on the viewer's review, marking those already listed.
    pub fn review_requests(&self) -> Result<Vec<Listed<github::PrHit>>> {
        github::review_requests()?
            .into_iter()
            .map(|hit| {
                let item = self.state.item_for_pull_request(&hit.url)?.map(|i| i.name);
                Ok(Listed { hit, item })
            })
            .collect()
    }

    /// Puts an issue on the work list, in the project its repo belongs to.
    pub fn add_issue_item(&self, url: &str, choice: &ProjectChoice) -> Result<AddOutcome> {
        let hit = github::search_issues(url, &[], &[])?
            .into_iter()
            .find(|h| h.matched)
            .with_context(|| format!("{url} is not an issue"))?;
        if let Some(existing) = self.state.item_for_issue(&hit.id)? {
            bail!(
                "{}#{} is already on the list as {}",
                hit.repo,
                hit.number,
                existing.name
            );
        }
        let Some(placed) = self.project_for_new_item(&hit.repo, choice)? else {
            return Ok(AddOutcome::NeedsProject { repo: hit.repo });
        };
        let source = ItemSource::Issue(IssueRef {
            id: hit.id,
            number: hit.number,
            repo: hit.repo,
            title: hit.title.clone(),
            url: hit.url,
        });
        let item = self.insert_new_item(&placed.project, &hit.title, "", source, false)?;
        Ok(placed.added(item))
    }

    /// Puts a pull request on the work list, to review or finish it in a worker on its branch.
    pub fn add_pull_request_item(&self, url: &str, choice: &ProjectChoice) -> Result<AddOutcome> {
        let pr = github::pull_request(url)?;
        if let Some(existing) = self.state.item_for_pull_request(&pr.url)? {
            bail!(
                "{} PR #{} is already on the list as {}",
                pr.repo,
                pr.number,
                existing.name
            );
        }
        let Some(placed) = self.project_for_new_item(&pr.repo, choice)? else {
            return Ok(AddOutcome::NeedsProject { repo: pr.repo });
        };
        let source = ItemSource::PullRequest(PrRef {
            number: pr.number,
            repo: pr.repo,
            title: pr.title.clone(),
            url: pr.url,
            head: pr.head,
            base: pr.base,
        });
        let item = self.insert_new_item(&placed.project, &pr.title, "", source, false)?;
        Ok(placed.added(item))
    }

    pub fn create_item(
        &self,
        project: &str,
        title: &str,
        description: &str,
        scratch: bool,
    ) -> Result<Item> {
        let title = title.trim();
        if title.is_empty() {
            bail!("an item needs a title");
        }
        let project = self
            .config
            .project(project)
            .with_context(|| format!("no project named '{project}'"))?;
        self.insert_new_item(
            &project.name,
            title,
            description.trim(),
            ItemSource::Custom,
            scratch,
        )
    }

    fn insert_new_item(
        &self,
        project: &str,
        title: &str,
        description: &str,
        source: ItemSource,
        scratch: bool,
    ) -> Result<Item> {
        let taken: HashSet<String> = self
            .state
            .list_items()?
            .into_iter()
            .map(|i| i.name)
            .chain(
                self.state
                    .list()?
                    .into_iter()
                    .filter_map(|w| w.item.map(|i| i.name)),
            )
            .collect();
        let item = Item {
            name: random_name(&taken),
            title: title.to_string(),
            description: description.to_string(),
            project: project.to_string(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
            source,
            scratch,
        };
        self.state.insert_item(&item)?;
        Ok(item)
    }

    /// Only once the item's worker has been cleaned up, so nothing is left orphaned.
    pub fn remove_item(&self, name: &str) -> Result<()> {
        if let Some(w) = self.worker_for_item(name)? {
            bail!("{name} still has worker {}; clean it up first", w.id);
        }
        self.state.delete_item(name)
    }

    /// Merges the item into the issue at `issue_url`: its worker, if any, becomes that issue's
    /// worker (same folder, branch and stack), and the item now stands for the issue.
    pub fn link_item(
        &self,
        name: &str,
        issue_url: &str,
        is_subtask: IsSubtask,
    ) -> Result<Option<WorkerRecord>> {
        let (index, issue_id) = github::issue_by_url(issue_url, is_subtask)?;
        self.link_item_to(name, &index, &issue_id)
    }

    pub(crate) fn link_item_to(
        &self,
        name: &str,
        index: &IssueIndex,
        issue_id: &str,
    ) -> Result<Option<WorkerRecord>> {
        let item = self
            .state
            .get_item(name)?
            .with_context(|| format!("item '{name}' not found"))?;
        let issue = index.get(issue_id).context("issue not loaded")?;
        let project = self.project_for_issue(issue)?;
        if project.name != item.project {
            bail!(
                "{}#{} belongs to project {}, but {name} is in {}",
                issue.repo,
                issue.number,
                project.name,
                item.project
            );
        }
        if item.source != ItemSource::Custom {
            bail!("only a custom item can be linked to an issue");
        }
        if let Some(w) = self.worker_for_issue(issue_id)? {
            bail!(
                "{}#{} already has worker {}",
                issue.repo,
                issue.number,
                w.id
            );
        }
        if let Some(other) = self.state.item_for_issue(issue_id)? {
            bail!(
                "{}#{} is already on the list as {}",
                issue.repo,
                issue.number,
                other.name
            );
        }
        let issue_ref = IssueRef {
            id: issue.id.clone(),
            number: issue.number,
            repo: issue.repo.clone(),
            title: issue.title.clone(),
            url: issue.url.clone(),
        };
        let worker = match self.worker_for_item(name)? {
            Some(mut w) => {
                w.issue = Some(issue_ref.clone());
                w.item = None;
                w.pr = None;
                self.state.save(&w)?;
                if w.dir.is_dir() {
                    let memory = self.config.paths.memory_file(&project.name);
                    std::fs::write(
                        w.dir.join("CLAUDE.md"),
                        worker_claude_md(&w, project, index, &memory),
                    )?;
                }
                Some(w)
            }
            None => None,
        };
        self.state.save_item(&Item {
            title: issue.title.clone(),
            source: ItemSource::Issue(issue_ref),
            ..item
        })?;
        Ok(worker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_names_avoid_taken_ones() {
        let mut taken = HashSet::new();
        for _ in 0..200 {
            let name = random_name(&taken);
            assert!(!taken.contains(&name));
            assert!(name.contains('-'));
            taken.insert(name);
        }
    }

    fn hit(
        id: &str,
        parent: Option<&str>,
        matched: bool,
        item: Option<&str>,
    ) -> Listed<github::IssueHit> {
        Listed {
            hit: github::IssueHit {
                id: id.into(),
                number: 1,
                title: id.into(),
                repo: "o/r".into(),
                url: String::new(),
                state: "OPEN".into(),
                assignees: vec![],
                issue_type: None,
                parent_id: parent.map(String::from),
                matched,
            },
            item: item.map(String::from),
        }
    }

    fn shown(hits: &[Listed<github::IssueHit>]) -> Vec<(&str, bool)> {
        hits.iter()
            .map(|l| (l.hit.id.as_str(), l.hit.matched))
            .collect()
    }

    #[test]
    fn offered_tree_prunes_ancestors_with_no_match_under_them() {
        let hits = vec![
            hit("T", Some("S"), true, None),
            hit("S", Some("E"), false, None),
            hit("E", None, false, Some("listed-epic")),
            hit("ORPHAN", None, false, None),
        ];
        assert_eq!(
            shown(&offered_tree(hits, false)),
            vec![("T", true), ("S", false), ("E", false)]
        );
    }

    #[test]
    fn offered_tree_keeps_listed_matches_when_searching() {
        let hits = vec![
            hit("A", None, true, Some("brisk-otter")),
            hit("B", None, true, None),
        ];
        assert_eq!(
            shown(&offered_tree(hits, false)),
            vec![("A", true), ("B", true)]
        );
    }

    #[test]
    fn offered_tree_hides_listed_matches_unless_they_parent_a_kept_one() {
        let hits = vec![
            hit("T", Some("S"), true, None),
            hit("S", Some("E"), true, Some("brisk-otter")),
            hit("DONE", Some("E2"), true, Some("calm-heron")),
            hit("E", None, false, None),
            hit("E2", None, false, None),
        ];
        assert_eq!(
            shown(&offered_tree(hits, true)),
            vec![("T", true), ("S", false), ("E", false)]
        );
    }
}
