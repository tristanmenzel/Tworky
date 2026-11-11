use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::github::graphql;

const PR_FIELDS: &str = "fragment P on PullRequest { number title url state isDraft mergeable mergeStateStatus \
headRefName baseRefName updatedAt repository { nameWithOwner } \
commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 100) { nodes { __typename \
... on CheckRun { name conclusion status detailsUrl } ... on StatusContext { context state targetUrl } } } } } } } \
reviewThreads(first: 100) { nodes { isResolved isOutdated } } \
reviewRequests(first: 1) { totalCount } \
latestOpinionatedReviews(first: 20) { nodes { state author { login } } } }";

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Checks {
    /// Rollup state: SUCCESS, FAILURE, ERROR, PENDING or EXPECTED; None when the PR has no checks.
    pub state: Option<String>,
    pub failing: Vec<FailingCheck>,
    pub pending: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailingCheck {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrDetail {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub head: String,
    pub base: String,
    pub mergeable: String,
    pub merge_state: String,
    pub updated_at: String,
    pub checks: Checks,
    pub unresolved_threads: u32,
    pub approved_by: Vec<String>,
    pub changes_requested_by: Vec<String>,
    pub review_requests: u32,
}

impl PrDetail {
    pub fn is_open(&self) -> bool {
        self.state == "OPEN"
    }

    pub fn is_merged(&self) -> bool {
        self.state == "MERGED"
    }
}

pub(crate) fn parse_pr(node: &Value) -> Option<PrDetail> {
    let number = node["number"].as_u64()?;
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let rollup = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
    let mut checks = Checks {
        state: rollup["state"].as_str().map(String::from),
        ..Default::default()
    };
    for ctx in rollup["contexts"]["nodes"].as_array().into_iter().flatten() {
        let (name, url, failed, pending) = match ctx["__typename"].as_str() {
            Some("CheckRun") => {
                let conclusion = ctx["conclusion"].as_str().unwrap_or_default();
                (
                    s(&ctx["name"]),
                    &ctx["detailsUrl"],
                    matches!(
                        conclusion,
                        "FAILURE"
                            | "TIMED_OUT"
                            | "CANCELLED"
                            | "ACTION_REQUIRED"
                            | "STARTUP_FAILURE"
                    ),
                    ctx["status"].as_str().is_some_and(|st| st != "COMPLETED"),
                )
            }
            Some("StatusContext") => {
                let state = ctx["state"].as_str().unwrap_or_default();
                (
                    s(&ctx["context"]),
                    &ctx["targetUrl"],
                    matches!(state, "FAILURE" | "ERROR"),
                    matches!(state, "PENDING" | "EXPECTED"),
                )
            }
            _ => continue,
        };
        if failed {
            if !checks.failing.iter().any(|f| f.name == name) {
                checks.failing.push(FailingCheck {
                    name,
                    url: url.as_str().filter(|u| !u.is_empty()).map(String::from),
                });
            }
        } else if pending {
            checks.pending += 1;
        }
    }
    let mut approved_by = Vec::new();
    let mut changes_requested_by = Vec::new();
    for review in node["latestOpinionatedReviews"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let who = s(&review["author"]["login"]);
        match review["state"].as_str() {
            Some("APPROVED") => approved_by.push(who),
            Some("CHANGES_REQUESTED") => changes_requested_by.push(who),
            _ => {}
        }
    }
    let unresolved_threads = node["reviewThreads"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["isResolved"] == Value::Bool(false))
        .count() as u32;
    Some(PrDetail {
        repo: s(&node["repository"]["nameWithOwner"]),
        number,
        title: s(&node["title"]),
        url: s(&node["url"]),
        state: s(&node["state"]),
        is_draft: node["isDraft"].as_bool().unwrap_or(false),
        head: s(&node["headRefName"]),
        base: s(&node["baseRefName"]),
        mergeable: s(&node["mergeable"]),
        merge_state: s(&node["mergeStateStatus"]),
        updated_at: s(&node["updatedAt"]),
        checks,
        unresolved_threads,
        approved_by,
        changes_requested_by,
        review_requests: node["reviewRequests"]["totalCount"].as_u64().unwrap_or(0) as u32,
    })
}

/// Node ids are opaque base64-like strings; anything else is refused rather than spliced into a query.
fn checked_id(id: &str) -> Result<&str> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-=".contains(c))
    {
        bail!("unexpected node id '{id}'");
    }
    Ok(id)
}

#[derive(Debug, Default)]
pub struct IssueRefs {
    /// PRs that close, mention or are connected to any of the issues.
    pub pull_requests: Vec<PrDetail>,
    /// (owner/repo, branch) pairs from the issues' Development section.
    pub linked_branches: Vec<(String, String)>,
}

pub fn issue_refs(issue_ids: &[String]) -> Result<IssueRefs> {
    if issue_ids.is_empty() {
        return Ok(IssueRefs::default());
    }
    let ids = issue_ids
        .iter()
        .map(|id| checked_id(id).map(|id| format!("\"{id}\"")))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let query = format!(
        "query {{ nodes(ids: [{ids}]) {{ ... on Issue {{ number \
closedByPullRequestsReferences(first: 20, includeClosedPrs: true) {{ nodes {{ ...P }} }} \
linkedBranches(first: 20) {{ nodes {{ ref {{ name repository {{ nameWithOwner }} }} }} }} \
timelineItems(first: 100, itemTypes: [CROSS_REFERENCED_EVENT, CONNECTED_EVENT]) {{ nodes {{ \
... on CrossReferencedEvent {{ source {{ ... on PullRequest {{ ...P }} }} }} \
... on ConnectedEvent {{ subject {{ ... on PullRequest {{ ...P }} }} }} }} }} }} }} }} {PR_FIELDS}"
    );
    Ok(parse_issue_refs(&graphql(&query, &[])?))
}

pub(crate) fn parse_issue_refs(page: &Value) -> IssueRefs {
    let mut refs = IssueRefs::default();
    let mut seen = std::collections::HashSet::new();
    for issue in page["data"]["nodes"].as_array().into_iter().flatten() {
        let closing = issue["closedByPullRequestsReferences"]["nodes"]
            .as_array()
            .into_iter()
            .flatten();
        let timeline = issue["timelineItems"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|n| {
                if n["source"].is_object() {
                    &n["source"]
                } else {
                    &n["subject"]
                }
            });
        for pr in closing.chain(timeline).filter_map(parse_pr) {
            if seen.insert((pr.repo.clone(), pr.number)) {
                refs.pull_requests.push(pr);
            }
        }
        for b in issue["linkedBranches"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let r = &b["ref"];
            if let (Some(repo), Some(name)) = (
                r["repository"]["nameWithOwner"].as_str(),
                r["name"].as_str(),
            ) {
                let pair = (repo.to_string(), name.to_string());
                if !refs.linked_branches.contains(&pair) {
                    refs.linked_branches.push(pair);
                }
            }
        }
    }
    refs
}

pub fn pr_by_number(name_with_owner: &str, number: u64) -> Result<Option<PrDetail>> {
    let Some((owner, name)) = name_with_owner.split_once('/') else {
        bail!("expected owner/repo, got '{name_with_owner}'");
    };
    let query = format!(
        "query {{ repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{ ...P }} }} }} {PR_FIELDS}",
        serde_json::to_string(owner)?,
        serde_json::to_string(name)?
    );
    Ok(parse_pr(
        &graphql(&query, &[])?["data"]["repository"]["pullRequest"],
    ))
}

/// PRs whose head is one of the given branches, keyed by (owner/repo, branch).
pub fn prs_for_branches(
    branches: &[(String, String)],
) -> Result<BTreeMap<(String, String), Vec<PrDetail>>> {
    let mut by_repo: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (repo, branch) in branches {
        by_repo.entry(repo).or_default().push(branch);
    }
    let mut out = BTreeMap::new();
    if by_repo.is_empty() {
        return Ok(out);
    }
    let mut body = String::new();
    let mut aliases = Vec::new();
    for (ri, (repo, names)) in by_repo.iter().enumerate() {
        let Some((owner, name)) = repo.split_once('/') else {
            continue;
        };
        body.push_str(&format!(
            "r{ri}: repository(owner: {}, name: {}) {{ ",
            serde_json::to_string(owner)?,
            serde_json::to_string(name)?
        ));
        for (bi, branch) in names.iter().enumerate() {
            body.push_str(&format!(
                "b{bi}: pullRequests(headRefName: {}, first: 5, orderBy: {{ field: CREATED_AT, direction: DESC }}) {{ nodes {{ ...P }} }} ",
                serde_json::to_string(branch)?
            ));
            aliases.push((
                format!("r{ri}"),
                format!("b{bi}"),
                repo.to_string(),
                branch.to_string(),
            ));
        }
        body.push_str("} ");
    }
    let page = graphql(&format!("query {{ {body} }} {PR_FIELDS}"), &[])?;
    for (r, b, repo, branch) in aliases {
        let prs: Vec<PrDetail> = page["data"][&r][&b]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_pr)
            .collect();
        out.insert((repo, branch), prs);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn pr_json(number: u64, head: &str) -> Value {
        json!({
            "number": number, "title": "fix(header): keep the header on one line on mobile", "url": "u", "state": "OPEN",
            "isDraft": false, "mergeable": "MERGEABLE", "mergeStateStatus": "BLOCKED",
            "headRefName": head, "baseRefName": "main", "updatedAt": "2026-10-15T11:14:04Z",
            "repository": {"nameWithOwner": "o/platform"},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE", "contexts": {"nodes": [
                {"__typename": "CheckRun", "name": "review", "conclusion": "FAILURE", "status": "COMPLETED", "detailsUrl": "https://ci/review"},
                {"__typename": "CheckRun", "name": "review", "conclusion": "FAILURE", "status": "COMPLETED", "detailsUrl": "https://ci/review-again"},
                {"__typename": "StatusContext", "context": "deploy", "state": "ERROR", "targetUrl": null},
                {"__typename": "CheckRun", "name": "review", "conclusion": "SKIPPED", "status": "COMPLETED"},
                {"__typename": "CheckRun", "name": "build", "conclusion": null, "status": "IN_PROGRESS"},
                {"__typename": "StatusContext", "context": "snyk", "state": "SUCCESS"}
            ]}}}}]},
            "reviewThreads": {"nodes": [
                {"isResolved": false, "isOutdated": false},
                {"isResolved": true, "isOutdated": false},
                {"isResolved": false, "isOutdated": true}
            ]},
            "reviewRequests": {"totalCount": 1},
            "latestOpinionatedReviews": {"nodes": [{"state": "APPROVED", "author": {"login": "reviewer1"}}]}
        })
    }

    #[test]
    fn parses_checks_reviews_and_threads() {
        let pr = parse_pr(&pr_json(411, "feat/fix-header-318")).unwrap();
        assert_eq!(
            pr.checks.failing,
            vec![
                FailingCheck {
                    name: "review".into(),
                    url: Some("https://ci/review".into())
                },
                FailingCheck {
                    name: "deploy".into(),
                    url: None
                },
            ]
        );
        assert_eq!(pr.checks.pending, 1);
        assert_eq!(pr.unresolved_threads, 2);
        assert_eq!(pr.approved_by, vec!["reviewer1"]);
        assert_eq!(pr.review_requests, 1);
        assert!(pr.is_open());
    }

    #[test]
    fn collects_refs_once_across_issues() {
        let page = json!({"data": {"nodes": [
            {"number": 327,
             "closedByPullRequestsReferences": {"nodes": [pr_json(411, "a")]},
             "linkedBranches": {"nodes": [{"ref": {"name": "x", "repository": {"nameWithOwner": "o/r"}}}]},
             "timelineItems": {"nodes": [{"source": pr_json(411, "a")}, {"subject": pr_json(7, "b")}, {"source": {}}]}},
            {"number": 604,
             "closedByPullRequestsReferences": {"nodes": [pr_json(7, "b")]},
             "linkedBranches": {"nodes": [{"ref": {"name": "x", "repository": {"nameWithOwner": "o/r"}}}]},
             "timelineItems": {"nodes": []}}
        ]}});
        let refs = parse_issue_refs(&page);
        let numbers: Vec<_> = refs.pull_requests.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![411, 7]);
        assert_eq!(
            refs.linked_branches,
            vec![("o/r".to_string(), "x".to_string())]
        );
    }

    #[test]
    fn refuses_odd_node_ids() {
        assert!(checked_id("I_kwDOAbc-12_=").is_ok());
        assert!(checked_id("x\" } evil {").is_err());
    }
}
