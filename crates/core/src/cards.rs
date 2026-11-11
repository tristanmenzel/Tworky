use serde::Serialize;

use crate::hierarchy::{Issue, IssueIndex};
use crate::state::{IssueRef, Item};

/// An issue on the work list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Card {
    /// The issue's id.
    pub id: String,
    /// The work item that put it on the list.
    pub item: String,
    /// The issue's parents, nearest first. Context only.
    pub ancestors: Vec<String>,
}

/// Adds a placeholder for each listed issue GitHub no longer returns (deleted, transferred or out of
/// reach), so its card stays until the item is removed.
pub fn fill_missing(index: &mut IssueIndex, items: &[Item]) {
    for issue in items.iter().filter_map(Item::issue) {
        if index.get(&issue.id).is_none() {
            index.insert(placeholder(issue));
        }
    }
}

fn placeholder(r: &IssueRef) -> Issue {
    Issue {
        id: r.id.clone(),
        number: r.number,
        title: r.title.clone(),
        url: r.url.clone(),
        state: "UNKNOWN".into(),
        state_reason: None,
        body: String::new(),
        issue_type: None,
        repo: r.repo.clone(),
        assignees: vec![],
        parent_id: None,
        sub_issues_total: 0,
        sub_issues_completed: 0,
        boards: vec![],
    }
}

/// One card per issue item, by repo, newest issue first.
pub fn cards(items: &[Item], index: &IssueIndex) -> Vec<Card> {
    let mut cards: Vec<(String, u64, Card)> = items
        .iter()
        .filter_map(|item| {
            let issue = item.issue()?;
            Some((
                issue.repo.clone(),
                issue.number,
                Card {
                    id: issue.id.clone(),
                    item: item.name.clone(),
                    ancestors: index
                        .ancestors(&issue.id)
                        .iter()
                        .map(|a| a.id.clone())
                        .collect(),
                },
            ))
        })
        .collect();
    cards.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    cards.into_iter().map(|(_, _, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hierarchy::tests::issue;
    use crate::state::ItemSource;

    fn item(name: &str, issue: Option<&Issue>) -> Item {
        Item {
            name: name.into(),
            title: "t".into(),
            description: String::new(),
            project: "acme".into(),
            created_at: 0,
            source: match issue {
                Some(i) => ItemSource::Issue(IssueRef {
                    id: i.id.clone(),
                    number: i.number,
                    repo: i.repo.clone(),
                    title: i.title.clone(),
                    url: i.url.clone(),
                }),
                None => ItemSource::Custom,
            },
            scratch: false,
        }
    }

    #[test]
    fn one_card_per_issue_item_with_its_parents() {
        let mut idx = IssueIndex::new("me".into());
        idx.insert(issue("E", 116, Some("Epic"), None, &["me"]));
        idx.insert(issue("S", 318, Some("Story"), Some("E"), &["other"]));
        idx.insert(issue("T", 604, Some("Subtask"), Some("S"), &["me"]));
        let mut other_repo = issue("X", 3, None, None, &[]);
        other_repo.repo = "a/first".into();
        idx.insert(other_repo);
        let items = vec![
            item("one", idx.get("T")),
            item("two", idx.get("S")),
            item("three", None),
            item("four", idx.get("X")),
        ];
        let cards = cards(&items, &idx);
        let ids: Vec<_> = cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["X", "T", "S"], "by repo, then newest first");
        assert_eq!(cards[1].item, "one");
        assert_eq!(cards[1].ancestors, ["S", "E"]);
        assert_eq!(cards[2].ancestors, ["E"]);
    }

    #[test]
    fn a_missing_issue_keeps_its_card() {
        let gone = issue("G", 9, None, None, &[]);
        let items = vec![item("lost", Some(&gone))];
        let mut idx = IssueIndex::new("me".into());
        fill_missing(&mut idx, &items);
        assert_eq!(idx.get("G").unwrap().title, gone.title);
        assert_eq!(cards(&items, &idx)[0].id, "G");
    }
}
