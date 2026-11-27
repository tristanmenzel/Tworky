use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::github::{PrReference, graphql};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    /// The current line, else the original line of an outdated thread; None for a file-level thread.
    pub line: Option<u64>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub viewer_can_reply: bool,
    pub viewer_can_resolve: bool,
    pub comments: Vec<ReviewComment>,
}

impl ReviewThread {
    pub fn location(&self) -> String {
        match self.line {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewComment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PostedReply {
    pub url: String,
    /// The reply joined the viewer's unsubmitted review, so only they see it until they submit it.
    pub pending: bool,
}

const THREADS: &str = "fragment T on PullRequestReviewThreadConnection { \
pageInfo { hasNextPage endCursor } nodes { id path line originalLine isResolved isOutdated \
viewerCanReply viewerCanResolve comments(first: 100) { nodes { fullDatabaseId author { login } body url } } } }";

/// GitHub's `resource(url:)` returns `null` for Files-tab links (`.../pull/N/files#r1`, `.../pull/N/changes?w=1`).
fn normalise_pr_url(url: &str) -> &str {
    let url = url.split(['#', '?']).next().unwrap_or_default();
    let url = url.trim_end_matches('/');
    url.strip_suffix("/files")
        .or_else(|| url.strip_suffix("/changes"))
        .unwrap_or(url)
}

fn reference_text(pr: &PrReference) -> String {
    match pr {
        PrReference::Url(url) => url.clone(),
        PrReference::InRepo {
            owner,
            name,
            number,
        } => format!("{owner}/{name}#{number}"),
    }
}

pub fn threads(pr: &PrReference) -> Result<Vec<ReviewThread>> {
    let reference = reference_text(pr);
    // The number is inlined because `graphql` passes variables as strings, which `Int!` rejects.
    let (query, mut vars) = match pr {
        PrReference::Url(url) => (
            format!(
                "query($url: URI!, $cursor: String) {{ resource(url: $url) {{ ... on PullRequest {{ \
reviewThreads(first: 100, after: $cursor) {{ ...T }} }} }} }} {THREADS}"
            ),
            vec![("url", normalise_pr_url(url).to_string())],
        ),
        PrReference::InRepo {
            owner,
            name,
            number,
        } => (
            format!(
                "query($owner: String!, $name: String!, $cursor: String) {{ \
repository(owner: $owner, name: $name) {{ issueOrPullRequest(number: {number}) {{ ... on PullRequest {{ \
reviewThreads(first: 100, after: $cursor) {{ ...T }} }} }} }} }} {THREADS}"
            ),
            vec![("owner", owner.clone()), ("name", name.clone())],
        ),
    };
    let mut threads = Vec::new();
    loop {
        let args: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let page = graphql(&query, &args)?;
        let (mut found, cursor) = parse_threads_page(&page, &reference)?;
        threads.append(&mut found);
        match cursor {
            Some(cursor) => {
                vars.retain(|(k, _)| *k != "cursor");
                vars.push(("cursor", cursor));
            }
            None => return Ok(threads),
        }
    }
}

pub fn parse_threads_page(
    page: &Value,
    reference: &str,
) -> Result<(Vec<ReviewThread>, Option<String>)> {
    let data = &page["data"];
    let node = match data.get("resource") {
        Some(resource) => resource,
        None => &data["repository"]["issueOrPullRequest"],
    };
    let Some(connection) = node.get("reviewThreads").filter(|c| c.is_object()) else {
        bail!("{reference} is not a pull request");
    };
    let threads = connection["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(parse_thread)
        .collect::<Result<Vec<_>>>()?;
    let info = &connection["pageInfo"];
    let cursor = if info["hasNextPage"].as_bool() == Some(true) {
        info["endCursor"].as_str().map(String::from)
    } else {
        None
    };
    Ok((threads, cursor))
}

fn parse_thread(node: &Value) -> Result<ReviewThread> {
    let flag = |key: &str| node[key].as_bool().unwrap_or_default();
    Ok(ReviewThread {
        id: node["id"]
            .as_str()
            .context("review thread without id")?
            .to_string(),
        path: node["path"].as_str().unwrap_or_default().to_string(),
        line: node["line"]
            .as_u64()
            .or_else(|| node["originalLine"].as_u64()),
        is_resolved: flag("isResolved"),
        is_outdated: flag("isOutdated"),
        viewer_can_reply: flag("viewerCanReply"),
        viewer_can_resolve: flag("viewerCanResolve"),
        comments: node["comments"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(parse_comment)
            .collect::<Result<_>>()?,
    })
}

fn parse_comment(node: &Value) -> Result<ReviewComment> {
    let id = &node["fullDatabaseId"];
    let id = id
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| id.as_u64())
        .with_context(|| format!("review comment with fullDatabaseId {id}"))?;
    Ok(ReviewComment {
        id,
        author: node["author"]["login"]
            .as_str()
            .unwrap_or("ghost")
            .to_string(),
        body: node["body"].as_str().unwrap_or_default().to_string(),
        url: node["url"].as_str().unwrap_or_default().to_string(),
    })
}

pub fn find_thread<'a>(
    threads: &'a [ReviewThread],
    comment_id: u64,
    pr: &PrReference,
) -> Result<&'a ReviewThread> {
    threads
        .iter()
        .find(|t| t.comments.iter().any(|c| c.id == comment_id))
        .with_context(|| {
            format!(
                "comment {comment_id} is not a review comment on {}; top-level PR comments have no thread to reply in",
                reference_text(pr)
            )
        })
}

/// GitHub reports `viewerCanResolve: false` on every resolved thread, so callers pass
/// `resolve: false` for a thread that is already resolved.
pub fn check_can(thread: &ReviewThread, reply: bool, resolve: bool) -> Result<()> {
    let mut missing = Vec::new();
    if reply && !thread.viewer_can_reply {
        missing.push("reply to");
    }
    if resolve && !thread.viewer_can_resolve {
        missing.push("resolve");
    }
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "you don't have permission to {} the review thread at {}",
        missing.join(" or "),
        thread.location()
    )
}

pub fn reply(thread_id: &str, body: &str) -> Result<PostedReply> {
    // Starts with `mutation` so `graphql` never retries it and posts the reply twice.
    let page = graphql(
        "mutation($thread: ID!, $body: String!) { \
addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $thread, body: $body}) { comment { url state } } }",
        &[("thread", thread_id), ("body", body)],
    )?;
    let comment = &page["data"]["addPullRequestReviewThreadReply"]["comment"];
    Ok(PostedReply {
        url: comment["url"]
            .as_str()
            .context("addPullRequestReviewThreadReply returned no comment url")?
            .to_string(),
        pending: comment["state"].as_str() == Some("PENDING"),
    })
}

pub fn resolve(thread_id: &str) -> Result<()> {
    graphql(
        "mutation($thread: ID!) { resolveReviewThread(input: {threadId: $thread}) { thread { id } } }",
        &[("thread", thread_id)],
    )
    .map(drop)
}

pub fn format_threads(threads: &[ReviewThread], all: bool) -> String {
    let shown: Vec<&ReviewThread> = threads.iter().filter(|t| all || !t.is_resolved).collect();
    if shown.is_empty() {
        return if all {
            "no review threads".into()
        } else {
            "no unresolved review threads".into()
        };
    }
    let blocks: Vec<String> = shown
        .iter()
        .map(|thread| {
            let mut out = thread.location();
            if thread.is_outdated {
                out.push_str(" (outdated)");
            }
            if thread.is_resolved {
                out.push_str(" (resolved)");
            }
            for comment in &thread.comments {
                let mut lines = comment.body.trim_end().lines();
                let first = lines.next().unwrap_or_default();
                out.push_str(&format!("\n  {} {}: {first}", comment.id, comment.author));
                for line in lines {
                    out.push('\n');
                    out.push_str(format!("    {line}").trim_end());
                }
            }
            out
        })
        .collect();
    blocks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread_node(id: &str, extra: Value, comments: Value) -> Value {
        let mut node = json!({
            "id": id, "path": "src/lib.rs", "line": 12, "originalLine": 10,
            "isResolved": false, "isOutdated": false,
            "viewerCanReply": true, "viewerCanResolve": true,
            "comments": {"nodes": comments}
        });
        for (k, v) in extra.as_object().unwrap() {
            node[k] = v.clone();
        }
        node
    }

    fn comment_node(id: &str, author: Value, body: &str) -> Value {
        json!({"fullDatabaseId": id, "author": author, "body": body, "url": format!("https://github.com/o/r/pull/1#discussion_r{id}")})
    }

    fn url_page(connection: Value) -> Value {
        json!({"data": {"resource": {"reviewThreads": connection}}})
    }

    fn pr() -> PrReference {
        PrReference::InRepo {
            owner: "o".into(),
            name: "r".into(),
            number: 1,
        }
    }

    fn fixture() -> Value {
        json!({
            "pageInfo": {"hasNextPage": false, "endCursor": "END"},
            "nodes": [
                thread_node("PRRT_open", json!({}), json!([
                    comment_node("2000000000001", json!({"login": "alice"}), "Rename this?"),
                    comment_node("2000000000002", json!({"login": "bob"}), "Done"),
                ])),
                thread_node("PRRT_resolved", json!({"isResolved": true, "viewerCanResolve": false}), json!([
                    comment_node("3", json!({"login": "alice"}), "nit"),
                ])),
                thread_node("PRRT_outdated", json!({"line": null, "isOutdated": true, "viewerCanReply": false}), json!([
                    comment_node("4", json!(null), "from a deleted user"),
                ])),
                thread_node("PRRT_file", json!({"path": "README.md", "line": null, "originalLine": null}), json!([
                    comment_node("5", json!({"login": "carol"}), "first\nsecond\n\nfourth\n"),
                ])),
            ]
        })
    }

    #[test]
    fn parse_a_page() {
        let (threads, cursor) = parse_threads_page(&url_page(fixture()), "o/r#1").unwrap();
        assert_eq!(cursor, None);
        assert_eq!(threads.len(), 4);

        let open = &threads[0];
        assert_eq!(open.id, "PRRT_open");
        assert_eq!(open.location(), "src/lib.rs:12");
        assert!(!open.is_resolved && !open.is_outdated);
        assert!(open.viewer_can_reply && open.viewer_can_resolve);
        assert_eq!(open.comments[0].id, 2_000_000_000_001);
        assert_eq!(open.comments[0].author, "alice");
        assert_eq!(open.comments[1].body, "Done");
        assert!(open.comments[1].url.ends_with("#discussion_r2000000000002"));

        let resolved = &threads[1];
        assert!(resolved.is_resolved);
        assert!(!resolved.viewer_can_resolve);

        let outdated = &threads[2];
        assert!(outdated.is_outdated);
        assert_eq!(outdated.line, Some(10), "falls back to originalLine");
        assert!(!outdated.viewer_can_reply);
        assert_eq!(outdated.comments[0].author, "ghost");

        let file = &threads[3];
        assert_eq!(file.line, None);
        assert_eq!(file.location(), "README.md");
    }

    #[test]
    fn return_the_next_cursor() {
        let mut connection = fixture();
        connection["pageInfo"] = json!({"hasNextPage": true, "endCursor": "Y3Vyc29y"});
        let page =
            json!({"data": {"repository": {"issueOrPullRequest": {"reviewThreads": connection}}}});
        let (threads, cursor) = parse_threads_page(&page, "o/r#1").unwrap();
        assert_eq!(threads.len(), 4);
        assert_eq!(cursor.as_deref(), Some("Y3Vyc29y"));
    }

    #[test]
    fn reject_nodes_that_are_not_pull_requests() {
        for page in [
            json!({"data": {"resource": {}}}),
            json!({"data": {"resource": null}}),
            json!({"data": {"repository": {"issueOrPullRequest": {}}}}),
            json!({"data": {"repository": {"issueOrPullRequest": null}}}),
        ] {
            let err = parse_threads_page(&page, "o/r#7").unwrap_err().to_string();
            assert_eq!(err, "o/r#7 is not a pull request", "{page}");
        }
    }

    #[test]
    fn normalise_files_tab_urls() {
        assert_eq!(
            normalise_pr_url("https://github.com/o/r/pull/5/files#r1"),
            "https://github.com/o/r/pull/5"
        );
        assert_eq!(
            normalise_pr_url("https://github.com/o/r/pull/5/changes?w=1"),
            "https://github.com/o/r/pull/5"
        );
        assert_eq!(
            normalise_pr_url("https://github.com/o/r/pull/5#discussion_r9"),
            "https://github.com/o/r/pull/5"
        );
        assert_eq!(
            normalise_pr_url("https://github.com/o/r/pull/5/"),
            "https://github.com/o/r/pull/5"
        );
    }

    #[test]
    fn find_the_thread_holding_a_comment() {
        let (threads, _) = parse_threads_page(&url_page(fixture()), "o/r#1").unwrap();
        assert_eq!(
            find_thread(&threads, 2_000_000_000_002, &pr()).unwrap().id,
            "PRRT_open"
        );
        let err = find_thread(&threads, 99, &pr()).unwrap_err().to_string();
        assert!(
            err.contains("comment 99 is not a review comment on o/r#1")
                && err.contains("top-level PR comments"),
            "{err}"
        );
    }

    #[test]
    fn check_permissions_before_writing() {
        let (threads, _) = parse_threads_page(&url_page(fixture()), "o/r#1").unwrap();
        let (open, resolved, outdated) = (&threads[0], &threads[1], &threads[2]);
        check_can(open, true, true).unwrap();

        let err = check_can(outdated, true, false).unwrap_err().to_string();
        assert!(
            err.contains("reply to") && err.contains("src/lib.rs:10"),
            "{err}"
        );
        check_can(outdated, false, true).unwrap();

        let err = check_can(resolved, false, true).unwrap_err().to_string();
        assert!(
            err.contains("resolve") && err.contains("src/lib.rs:12"),
            "{err}"
        );
        check_can(resolved, true, false).unwrap();
        check_can(resolved, false, false).unwrap();
    }

    #[test]
    fn format_unresolved_by_default() {
        let (threads, _) = parse_threads_page(&url_page(fixture()), "o/r#1").unwrap();
        assert_eq!(
            format_threads(&threads, false),
            "src/lib.rs:12\n  2000000000001 alice: Rename this?\n  2000000000002 bob: Done\n\n\
src/lib.rs:10 (outdated)\n  4 ghost: from a deleted user\n\n\
README.md\n  5 carol: first\n    second\n\n    fourth"
        );
    }

    #[test]
    fn format_all_marks_resolved() {
        let (threads, _) = parse_threads_page(&url_page(fixture()), "o/r#1").unwrap();
        let text = format_threads(&threads, true);
        assert!(
            text.contains("src/lib.rs:12 (resolved)\n  3 alice: nit"),
            "{text}"
        );
        assert_eq!(text.matches("\n\n").count(), 4, "{text}");
        assert_eq!(
            format_threads(&threads[1..2], false),
            "no unresolved review threads"
        );
    }
}
