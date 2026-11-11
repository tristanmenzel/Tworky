use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub body: String,
    pub issue_type: Option<String>,
    pub repo: String,
    pub assignees: Vec<String>,
    pub parent_id: Option<String>,
    pub sub_issues_total: u32,
    pub sub_issues_completed: u32,
    /// The issue's Status on each open Projects board it is on.
    #[serde(default)]
    pub boards: Vec<BoardStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardStatus {
    pub item_id: String,
    pub project_id: String,
    pub number: u64,
    pub title: String,
    pub field_id: String,
    pub option_id: Option<String>,
    pub options: Vec<BoardOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardOption {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueNode {
    pub issue: Issue,
    pub assigned: bool,
    pub children: Vec<IssueNode>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IssueIndex {
    pub viewer: String,
    pub issues: BTreeMap<String, Issue>,
}

impl IssueIndex {
    pub fn new(viewer: String) -> Self {
        Self {
            viewer,
            issues: BTreeMap::new(),
        }
    }

    /// Later sightings of an issue only fill gaps: a nested `parent` selection may omit fields
    /// (body, sub-issue counts) that the top-level selection had.
    pub fn insert(&mut self, issue: Issue) {
        match self.issues.get_mut(&issue.id) {
            Some(existing) => {
                if existing.parent_id.is_none() {
                    existing.parent_id = issue.parent_id;
                }
                if existing.body.is_empty() {
                    existing.body = issue.body;
                }
                if existing.assignees.is_empty() {
                    existing.assignees = issue.assignees;
                }
                if existing.boards.is_empty() {
                    existing.boards = issue.boards;
                }
                existing.sub_issues_total = existing.sub_issues_total.max(issue.sub_issues_total);
                existing.sub_issues_completed = existing
                    .sub_issues_completed
                    .max(issue.sub_issues_completed);
            }
            None => {
                self.issues.insert(issue.id.clone(), issue);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Issue> {
        self.issues.get(id)
    }

    pub fn is_assigned(&self, issue: &Issue) -> bool {
        issue
            .assignees
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&self.viewer))
    }

    /// Nearest ancestor first.
    pub fn ancestors(&self, id: &str) -> Vec<&Issue> {
        let mut out = Vec::new();
        let mut current = self.get(id).and_then(|i| i.parent_id.as_deref());
        while let Some(pid) = current {
            let Some(parent) = self.get(pid) else { break };
            if parent.id == id || out.iter().any(|i: &&Issue| i.id == parent.id) {
                break;
            }
            out.push(parent);
            current = parent.parent_id.as_deref();
        }
        out
    }

    pub fn children(&self, id: &str) -> Vec<&Issue> {
        let mut kids: Vec<&Issue> = self
            .issues
            .values()
            .filter(|i| i.parent_id.as_deref() == Some(id))
            .collect();
        kids.sort_by_key(|i| (i.repo.clone(), i.number));
        kids
    }

    pub fn is_leaf(&self, issue: &Issue) -> bool {
        issue.sub_issues_total == 0 && self.children(&issue.id).is_empty()
    }

    pub fn forest(&self) -> Vec<IssueNode> {
        let mut by_parent: HashMap<Option<&str>, Vec<&Issue>> = HashMap::new();
        for issue in self.issues.values() {
            let parent = issue
                .parent_id
                .as_deref()
                .filter(|p| self.issues.contains_key(*p));
            by_parent.entry(parent).or_default().push(issue);
        }
        for list in by_parent.values_mut() {
            list.sort_by(|a, b| (&a.repo, a.number).cmp(&(&b.repo, b.number)));
        }
        let mut visiting = Vec::new();
        by_parent
            .get(&None)
            .map(|roots| {
                roots
                    .iter()
                    .map(|i| self.node(i, &by_parent, &mut visiting))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn node<'a>(
        &'a self,
        issue: &'a Issue,
        by_parent: &HashMap<Option<&'a str>, Vec<&'a Issue>>,
        visiting: &mut Vec<&'a str>,
    ) -> IssueNode {
        visiting.push(&issue.id);
        let mut children = Vec::new();
        for kid in by_parent
            .get(&Some(issue.id.as_str()))
            .into_iter()
            .flatten()
        {
            if !visiting.contains(&kid.id.as_str()) {
                children.push(self.node(kid, by_parent, visiting));
            }
        }
        visiting.pop();
        IssueNode {
            issue: issue.clone(),
            assigned: self.is_assigned(issue),
            children,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn issue(
        id: &str,
        number: u64,
        ty: Option<&str>,
        parent: Option<&str>,
        assignees: &[&str],
    ) -> Issue {
        Issue {
            id: id.into(),
            number,
            title: format!("Issue {number}"),
            url: format!("https://github.com/o/r/issues/{number}"),
            state: "OPEN".into(),
            state_reason: None,
            body: String::new(),
            issue_type: ty.map(String::from),
            repo: "o/r".into(),
            assignees: assignees.iter().map(|s| s.to_string()).collect(),
            parent_id: parent.map(String::from),
            sub_issues_total: 0,
            sub_issues_completed: 0,
            boards: vec![],
        }
    }

    fn index() -> IssueIndex {
        let mut idx = IssueIndex::new("me".into());
        idx.insert(issue("E", 1, Some("Epic"), None, &[]));
        idx.insert(issue("S", 2, Some("Story"), Some("E"), &["me"]));
        idx.insert(issue("T2", 4, Some("Subtask"), Some("S"), &["me"]));
        idx.insert(issue("T1", 3, Some("Subtask"), Some("S"), &["someone"]));
        idx.insert(issue("X", 9, None, Some("missing"), &["me"]));
        idx
    }

    #[test]
    fn builds_forest_with_orphans_as_roots() {
        let forest = index().forest();
        assert_eq!(forest.len(), 2);
        let epic = &forest[0];
        assert_eq!(epic.issue.id, "E");
        assert!(!epic.assigned);
        let story = &epic.children[0];
        assert!(story.assigned);
        let kids: Vec<_> = story.children.iter().map(|c| c.issue.number).collect();
        assert_eq!(kids, vec![3, 4]);
        assert_eq!(forest[1].issue.id, "X");
    }

    #[test]
    fn ancestors_nearest_first() {
        let idx = index();
        let chain: Vec<_> = idx.ancestors("T1").iter().map(|i| i.id.clone()).collect();
        assert_eq!(chain, vec!["S", "E"]);
        assert!(idx.ancestors("X").is_empty());
    }

    #[test]
    fn merge_fills_gaps_without_overwriting() {
        let mut idx = IssueIndex::new("me".into());
        let mut full = issue("S", 2, Some("Story"), Some("E"), &["me"]);
        full.body = "story body".into();
        full.sub_issues_total = 3;
        idx.insert(issue("S", 2, Some("Story"), None, &[]));
        idx.insert(full);
        let s = idx.get("S").unwrap();
        assert_eq!(s.body, "story body");
        assert_eq!(s.parent_id.as_deref(), Some("E"));
        assert_eq!(s.sub_issues_total, 3);
        assert!(!idx.is_leaf(s));
    }

    #[test]
    fn cycles_do_not_hang() {
        let mut idx = IssueIndex::new("me".into());
        idx.insert(issue("A", 1, None, Some("B"), &[]));
        idx.insert(issue("B", 2, None, Some("A"), &[]));
        assert_eq!(idx.ancestors("A").len(), 1);
        assert!(idx.forest().is_empty());
    }
}
