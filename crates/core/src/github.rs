use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hierarchy::{BoardOption, BoardStatus, Issue, IssueIndex};
use crate::process;

const BOARDS: &str = "projectItems(first: 10) { nodes { id \
project { id number title closed field(name: \"Status\") { ... on ProjectV2SingleSelectField { id options { id name } } } } \
fieldValueByName(name: \"Status\") { ... on ProjectV2ItemFieldSingleSelectValue { optionId } } } }";

fn fields(boards: bool) -> String {
    format!(
        "fragment F on Issue {{ id number title url state stateReason body issueType {{ name }} \
repository {{ nameWithOwner }} assignees(first: 10) {{ nodes {{ login }} }} subIssuesSummary {{ total completed }} {} }}",
        if boards { BOARDS } else { "" }
    )
}

/// The direct parent also brings its sub-issues, so a story shows every subtask, not just mine.
const PARENTS: &str = "parent { ...F subIssues(first: 50) { nodes { ...F } } \
parent { ...F parent { ...F parent { ...F } } } }";

fn search_query(boards: bool) -> String {
    format!(
        "query($cursor: String) {{ viewer {{ login }} \
search(type: ISSUE, query: \"assignee:@me is:open is:issue archived:false\", first: 50, after: $cursor) {{ \
pageInfo {{ hasNextPage endCursor }} \
nodes {{ ... on Issue {{ ...F {PARENTS} subIssues(first: 50) {{ nodes {{ ...F }} }} }} }} }} }} {}",
        fields(boards)
    )
}

fn resource_query(boards: bool) -> String {
    format!(
        "query($url: URI!) {{ viewer {{ login }} resource(url: $url) {{ ... on Issue {{ ...F {PARENTS} \
subIssues(first: 50) {{ nodes {{ ...F }} }} }} }} }} {}",
        fields(boards)
    )
}

fn nodes_query(ids: &[String], boards: bool) -> String {
    format!(
        "query {{ viewer {{ login }} nodes(ids: {}) {{ ... on Issue {{ ...F {PARENTS} \
subIssues(first: 50) {{ nodes {{ ...F }} }} }} }} }} {}",
        serde_json::to_string(ids).unwrap_or_default(),
        fields(boards)
    )
}

/// Board fields need the `read:project` scope, which `gh auth login` doesn't grant by default.
fn with_boards_if_allowed<T>(query: impl Fn(bool) -> Result<T>) -> Result<T> {
    match query(true) {
        Err(e) if format!("{e:#}").contains("read:project") => query(false),
        result => result,
    }
}

pub(crate) fn graphql(query: &str, vars: &[(&str, &str)]) -> Result<Value> {
    let query_arg = format!("query={query}");
    let mut args = vec!["api", "graphql", "-f", &query_arg];
    let var_args: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
    for v in &var_args {
        args.push("-f");
        args.push(v);
    }
    // A mutation may have applied before a 5xx came back, so only reads are retried.
    let attempts = if query.starts_with("mutation") { 1 } else { 3 };
    let mut attempt = 1;
    let text = loop {
        let mut cmd = process::command("gh");
        cmd.args(&args);
        match process::output(cmd, "gh api graphql") {
            Err(e) if attempt < attempts && format!("{e:#}").contains("HTTP 5") => {
                std::thread::sleep(std::time::Duration::from_secs(attempt));
                attempt += 1;
            }
            result => break result?,
        }
    };
    let value: Value = serde_json::from_str(&text).context("parsing gh api graphql output")?;
    if let Some(errors) = value.get("errors") {
        bail!("GitHub GraphQL errors: {errors}");
    }
    Ok(value)
}

/// Open issues assigned to the viewer, plus the `also` issues whatever their state or assignees,
/// each with its ancestors and direct sub-issues.
/// `is_subtask` decides when a parent's other children are kept: only for an assigned subtask, so its
/// story lists every sibling, not for an assigned story, whose parent epic would bring every story.
pub fn assigned_issues(also: &HashSet<String>, is_subtask: IsSubtask) -> Result<IssueIndex> {
    with_boards_if_allowed(|boards| {
        let mut index = assigned_issues_query(&search_query(boards), is_subtask)?;
        let missing: Vec<String> = also
            .iter()
            .filter(|id| index.get(id).is_none())
            .cloned()
            .collect();
        ingest_ids(&missing, boards, &mut index, is_subtask)?;
        Ok(index)
    })
}

/// The work list's issues, whatever their state or assignees, each with its ancestors and direct
/// sub-issues. An issue that was deleted or transferred is left out.
pub fn issues_by_id(ids: &[String], is_subtask: IsSubtask) -> Result<IssueIndex> {
    with_boards_if_allowed(|boards| {
        let mut index = IssueIndex::new(String::new());
        ingest_ids(ids, boards, &mut index, is_subtask)?;
        if index.viewer.is_empty() {
            index.viewer = viewer(&graphql("query { viewer { login } }", &[])?);
        }
        Ok(index)
    })
}

fn ingest_ids(
    ids: &[String],
    boards: bool,
    index: &mut IssueIndex,
    is_subtask: IsSubtask,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let pages = match graphql(&nodes_query(ids, boards), &[]) {
        Ok(page) => vec![page],
        // One deleted or transferred issue fails the whole batch, so skip just the ones that fail.
        Err(e) if format!("{e:#}").contains("read:project") => return Err(e),
        Err(_) => ids
            .iter()
            .filter_map(|id| graphql(&nodes_query(std::slice::from_ref(id), boards), &[]).ok())
            .collect(),
    };
    for page in &pages {
        if index.viewer.is_empty() {
            index.viewer = viewer(page);
        }
        for node in page["data"]["nodes"].as_array().into_iter().flatten() {
            if node.get("id").is_some() {
                ingest_node(node, None, true, index, is_subtask)?;
            }
        }
    }
    Ok(())
}

/// An issue as the add-from-issue search lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IssueHit {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub repo: String,
    pub url: String,
    pub state: String,
    pub assignees: Vec<String>,
    pub issue_type: Option<String>,
    pub parent_id: Option<String>,
    /// Found by the search itself, rather than only as an ancestor of something that was.
    pub matched: bool,
}

const HIT_FIELDS: &str = "id number title url state issueType { name } \
repository { nameWithOwner } assignees(first: 10) { nodes { login } }";

/// Four levels of parents, as `PARENTS` fetches, each with the hit fields written out because a
/// fragment can't include itself. The deepest asks only for its parent's id.
fn hit_fragment() -> String {
    format!(
        "fragment H on Issue {{ {HIT_FIELDS} parent {{ {HIT_FIELDS} parent {{ {HIT_FIELDS} \
parent {{ {HIT_FIELDS} parent {{ {HIT_FIELDS} parent {{ id }} }} }} }} }} }}"
    )
}

fn hit(node: &Value) -> Option<IssueHit> {
    Some(IssueHit {
        id: node["id"].as_str()?.to_string(),
        number: node["number"].as_u64()?,
        title: str_field(node, "title"),
        repo: node["repository"]["nameWithOwner"].as_str()?.to_string(),
        url: str_field(node, "url"),
        state: str_field(node, "state"),
        assignees: logins(&node["assignees"]),
        issue_type: node["issueType"]["name"].as_str().map(String::from),
        parent_id: node["parent"]["id"].as_str().map(String::from),
        matched: false,
    })
}

pub fn flatten_with_ancestors<'a>(nodes: impl IntoIterator<Item = &'a Value>) -> Vec<IssueHit> {
    let nodes: Vec<&Value> = nodes.into_iter().collect();
    let mut out: Vec<IssueHit> = Vec::new();
    let mut add = |hit: IssueHit| match out.iter_mut().find(|h| h.id == hit.id) {
        Some(existing) => {
            existing.matched |= hit.matched;
            if existing.parent_id.is_none() {
                existing.parent_id = hit.parent_id;
            }
        }
        None => out.push(hit),
    };
    for node in &nodes {
        if let Some(h) = hit(node) {
            add(IssueHit { matched: true, ..h });
        }
    }
    for node in &nodes {
        if hit(node).is_none() {
            continue;
        }
        let mut parent = &node["parent"];
        while let Some(h) = hit(parent) {
            add(h);
            parent = &parent["parent"];
        }
    }
    out
}

fn str_field(node: &Value, key: &str) -> String {
    node[key].as_str().unwrap_or_default().to_string()
}

fn logins(connection: &Value) -> Vec<String> {
    connection["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["login"].as_str().map(String::from))
        .collect()
}

/// What the add-from-issue search box holds.
#[derive(Debug, Clone, PartialEq)]
pub enum IssueQuery {
    /// Nothing typed: the viewer's open assigned issues.
    Assigned,
    Url(String),
    /// `owner/repo#12`
    InRepo(String, u64),
    /// `12` or `#12`, looked up in each known repo.
    Number(u64),
    Text(String),
}

pub fn parse_issue_query(query: &str) -> IssueQuery {
    let q = query.trim();
    if q.is_empty() {
        return IssueQuery::Assigned;
    }
    if q.starts_with("https://") || q.starts_with("http://") {
        return IssueQuery::Url(q.to_string());
    }
    if let Ok(n) = q.trim_start_matches('#').parse::<u64>() {
        return IssueQuery::Number(n);
    }
    if let Some((repo, n)) = q.split_once('#')
        && repo.split('/').filter(|p| !p.is_empty()).count() == 2
        && let Ok(n) = n.parse::<u64>()
    {
        return IssueQuery::InRepo(repo.to_string(), n);
    }
    IssueQuery::Text(q.to_string())
}

/// Searches GitHub for issues to add to the work list. Plain GitHub search spans every public repo,
/// so text is searched only in repos `owners` own, and a bare number is looked up in each of `repos`.
/// Matches come first, in search order, then their ancestors (`matched: false`).
pub fn search_issues(query: &str, repos: &[String], owners: &[String]) -> Result<Vec<IssueHit>> {
    let fragment = hit_fragment();
    let search = |q: &str| -> Result<Vec<Value>> {
        let page = graphql(
            &format!(
                "query($q: String!) {{ search(type: ISSUE, query: $q, first: 50) {{ nodes {{ ... on Issue {{ ...H }} }} }} }} {fragment}"
            ),
            &[("q", q)],
        )?;
        Ok(page["data"]["search"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    };
    let in_repo = |repo: &str, n: u64| -> Result<Value> {
        let (owner, name) = repo.split_once('/').context("expected owner/repo")?;
        let page = graphql(
            &format!(
                "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) \
                 {{ issue(number: {n}) {{ ...H }} }} }} {fragment}"
            ),
            &[("owner", owner), ("name", name)],
        )?;
        Ok(page["data"]["repository"]["issue"].clone())
    };
    let nodes = match parse_issue_query(query) {
        IssueQuery::Assigned => search("assignee:@me is:open is:issue archived:false")?,
        IssueQuery::Url(url) => {
            let page = graphql(
                &format!(
                    "query($url: URI!) {{ resource(url: $url) {{ ... on Issue {{ ...H }} }} }} {fragment}"
                ),
                &[("url", &url)],
            )?;
            vec![page["data"]["resource"].clone()]
        }
        IssueQuery::InRepo(repo, n) => vec![in_repo(&repo, n)?],
        IssueQuery::Number(n) => std::thread::scope(|s| {
            let lookups: Vec<_> = repos
                .iter()
                .map(|r| s.spawn(move || in_repo(r, n).ok()))
                .collect();
            lookups
                .into_iter()
                .filter_map(|h| h.join().ok().flatten())
                .collect()
        }),
        IssueQuery::Text(text) => {
            let scope: Vec<String> = owners.iter().map(|o| format!("user:{o}")).collect();
            search(&format!(
                "{text} in:title is:issue archived:false {}",
                scope.join(" ")
            ))?
        }
    };
    Ok(flatten_with_ancestors(&nodes))
}

/// The viewer's login and organisations, which bound issue text search. Fetched once per process.
pub fn viewer_owners() -> Result<Vec<String>> {
    static OWNERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    if let Some(owners) = OWNERS.get() {
        return Ok(owners.clone());
    }
    let page = graphql(
        "query { viewer { login organizations(first: 100) { nodes { login } } } }",
        &[],
    )?;
    let mut owners = vec![viewer(&page)];
    owners.extend(logins(&page["data"]["viewer"]["organizations"]));
    Ok(OWNERS.get_or_init(|| owners).clone())
}

/// A pull request as the add-from-PR dialog lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrHit {
    pub number: u64,
    pub title: String,
    pub repo: String,
    pub url: String,
    pub author: String,
    pub head: String,
    pub base: String,
    pub is_draft: bool,
    pub updated_at: String,
}

const PR_HIT: &str = "fragment P on PullRequest { number title url isDraft updatedAt headRefName \
baseRefName author { login } repository { nameWithOwner } }";

fn pr_hit(node: &Value) -> Option<PrHit> {
    Some(PrHit {
        number: node["number"].as_u64()?,
        title: str_field(node, "title"),
        repo: node["repository"]["nameWithOwner"].as_str()?.to_string(),
        url: str_field(node, "url"),
        author: node["author"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        head: str_field(node, "headRefName"),
        base: str_field(node, "baseRefName"),
        is_draft: node["isDraft"].as_bool().unwrap_or_default(),
        updated_at: str_field(node, "updatedAt"),
    })
}

/// Open pull requests waiting on the viewer's review.
pub fn review_requests() -> Result<Vec<PrHit>> {
    let page = graphql(
        &format!(
            "query {{ search(type: ISSUE, query: \"is:pr is:open review-requested:@me archived:false\", first: 50) \
             {{ nodes {{ ... on PullRequest {{ ...P }} }} }} }} {PR_HIT}"
        ),
        &[],
    )?;
    Ok(page["data"]["search"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(pr_hit)
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrReference {
    Url(String),
    InRepo {
        owner: String,
        name: String,
        number: u64,
    },
}

pub fn parse_pr_reference(reference: &str) -> Option<PrReference> {
    match parse_issue_query(reference) {
        IssueQuery::Url(url) => Some(PrReference::Url(url)),
        IssueQuery::Text(text) if text.contains("github.com/") => Some(PrReference::Url(text)),
        IssueQuery::InRepo(repo, number) => {
            let (owner, name) = repo.split_once('/')?;
            let valid = |part: &str| !part.is_empty() && !part.contains('/');
            (valid(owner) && valid(name)).then(|| PrReference::InRepo {
                owner: owner.to_string(),
                name: name.to_string(),
                number,
            })
        }
        _ => None,
    }
}

pub fn pull_request(reference: &str) -> Result<PrHit> {
    let reference = reference.trim();
    let node = match parse_pr_reference(reference) {
        Some(PrReference::Url(url)) => {
            let page = graphql(
                &format!(
                    "query($url: URI!) {{ resource(url: $url) {{ ... on PullRequest {{ ...P }} }} }} {PR_HIT}"
                ),
                &[("url", &url)],
            )?;
            page["data"]["resource"].clone()
        }
        Some(PrReference::InRepo {
            owner,
            name,
            number,
        }) => {
            let page = graphql(
                &format!(
                    "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) \
                     {{ issueOrPullRequest(number: {number}) {{ ... on PullRequest {{ ...P }} }} }} }} {PR_HIT}"
                ),
                &[("owner", &owner), ("name", &name)],
            )?;
            page["data"]["repository"]["issueOrPullRequest"].clone()
        }
        None => bail!("expected a pull request URL or owner/repo#number, got '{reference}'"),
    };
    pr_hit(&node).with_context(|| format!("{reference} is not a pull request"))
}

fn assigned_issues_query(query: &str, is_subtask: IsSubtask) -> Result<IssueIndex> {
    let mut cursor: Option<String> = None;
    let mut index: Option<IssueIndex> = None;
    loop {
        let vars: Vec<(&str, &str)> = cursor
            .as_deref()
            .map(|c| vec![("cursor", c)])
            .unwrap_or_default();
        let page = graphql(query, &vars)?;
        let idx = index.get_or_insert_with(|| IssueIndex::new(viewer(&page)));
        let next = ingest_search(&page, idx, is_subtask)?;
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(index.unwrap_or_default())
}

pub fn issue_by_url(url: &str, is_subtask: IsSubtask) -> Result<(IssueIndex, String)> {
    let page = with_boards_if_allowed(|boards| graphql(&resource_query(boards), &[("url", url)]))?;
    let mut index = IssueIndex::new(viewer(&page));
    let node = &page["data"]["resource"];
    if node.get("id").is_none() {
        bail!("{url} is not an issue");
    }
    let id = ingest_node(node, None, true, &mut index, is_subtask)?;
    Ok((index, id))
}

fn viewer(page: &Value) -> String {
    page["data"]["viewer"]["login"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn ingest_search(
    page: &Value,
    index: &mut IssueIndex,
    is_subtask: IsSubtask,
) -> Result<Option<String>> {
    let search = &page["data"]["search"];
    for node in search["nodes"].as_array().into_iter().flatten() {
        if node.get("id").is_some() {
            ingest_node(node, None, true, index, is_subtask)?;
        }
    }
    let info = &search["pageInfo"];
    Ok(if info["hasNextPage"].as_bool() == Some(true) {
        info["endCursor"].as_str().map(String::from)
    } else {
        None
    })
}

pub type IsSubtask<'a> = &'a dyn Fn(&Issue) -> bool;

fn ingest_node(
    node: &Value,
    child_of: Option<&str>,
    with_children: bool,
    index: &mut IssueIndex,
    is_subtask: IsSubtask,
) -> Result<String> {
    let mut issue = parse_issue(node)?;
    issue.parent_id = match node.get("parent") {
        Some(p) if p.is_object() => {
            let siblings = is_subtask(&issue);
            Some(ingest_node(p, None, siblings, index, is_subtask)?)
        }
        _ => child_of.map(String::from),
    };
    let id = issue.id.clone();
    index.insert(issue);
    if with_children {
        for child in node["subIssues"]["nodes"].as_array().into_iter().flatten() {
            if child.get("id").is_some() {
                ingest_node(child, Some(&id), false, index, is_subtask)?;
            }
        }
    }
    Ok(id)
}

fn parse_issue(node: &Value) -> Result<Issue> {
    let str_of = |key: &str| node[key].as_str().unwrap_or_default().to_string();
    Ok(Issue {
        id: node["id"].as_str().context("issue without id")?.to_string(),
        number: node["number"].as_u64().unwrap_or_default(),
        title: str_of("title"),
        url: str_of("url"),
        state: str_of("state"),
        state_reason: node["stateReason"].as_str().map(String::from),
        body: str_of("body"),
        issue_type: node["issueType"]["name"].as_str().map(String::from),
        repo: node["repository"]["nameWithOwner"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        assignees: node["assignees"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["login"].as_str().map(String::from))
            .collect(),
        parent_id: None,
        sub_issues_total: node["subIssuesSummary"]["total"]
            .as_u64()
            .unwrap_or_default() as u32,
        sub_issues_completed: node["subIssuesSummary"]["completed"]
            .as_u64()
            .unwrap_or_default() as u32,
        boards: node["projectItems"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_board)
            .collect(),
    })
}

fn parse_board(item: &Value) -> Option<BoardStatus> {
    let project = &item["project"];
    if project["closed"].as_bool() == Some(true) {
        return None;
    }
    let field = &project["field"];
    Some(BoardStatus {
        item_id: item["id"].as_str()?.into(),
        project_id: project["id"].as_str()?.into(),
        number: project["number"].as_u64()?,
        title: project["title"].as_str().unwrap_or_default().into(),
        field_id: field["id"].as_str()?.into(),
        option_id: item["fieldValueByName"]["optionId"]
            .as_str()
            .map(String::from),
        options: field["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| {
                Some(BoardOption {
                    id: o["id"].as_str()?.into(),
                    name: o["name"].as_str()?.into(),
                })
            })
            .collect(),
    })
}

/// Sets the issue's Status on board `project_id` and mirrors it onto `issue`.
pub fn set_board_status(issue: &mut Issue, project_id: &str, option_id: &str) -> Result<()> {
    let board = issue
        .boards
        .iter_mut()
        .find(|b| b.project_id == project_id)
        .context("the issue is not on that board")?;
    if !board.options.iter().any(|o| o.id == option_id) {
        bail!("{option_id} is not a Status option on {}", board.title);
    }
    graphql(
        "mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) { \
updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, \
value: {singleSelectOptionId: $option}}) { projectV2Item { id } } }",
        &[
            ("project", &board.project_id),
            ("item", &board.item_id),
            ("field", &board.field_id),
            ("option", option_id),
        ],
    )?;
    board.option_id = Some(option_id.into());
    Ok(())
}

pub fn assign_to_me(repo: &str, number: u64) -> Result<()> {
    process::run(
        "gh",
        &[
            "issue",
            "edit",
            &number.to_string(),
            "--repo",
            repo,
            "--add-assignee",
            "@me",
        ],
        None,
    )
    .map(drop)
}

/// Images attached to private issues only load with GitHub auth, which the webview doesn't have.
pub fn is_github_asset(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.contains(['@', ':'])
        && (host.eq_ignore_ascii_case("github.com") || host.ends_with(".githubusercontent.com"))
}

pub struct Asset {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// Fetches a GitHub-hosted image with the `gh` token. The token goes to curl on stdin so it never
/// appears in a process list, and curl drops it when a redirect leaves the host (the signed
/// `private-user-images` URL needs no auth).
pub fn fetch_asset(url: &str) -> Result<Asset> {
    use std::io::Write;
    if !is_github_asset(url) {
        bail!("{url} is not a GitHub asset");
    }
    let token = process::run("gh", &["auth", "token"], None)?;
    let mut cmd = process::command("curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--max-time",
        "30",
        "--config",
        "-",
        "--write-out",
        "%{stderr}%{content_type}",
        url,
    ])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("starting curl")?;
    child
        .stdin
        .take()
        .context("curl stdin")?
        .write_all(format!("header = \"Authorization: token {}\"\n", token.trim()).as_bytes())?;
    let out = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        bail!("fetching {url}: {stderr}");
    }
    Ok(Asset {
        bytes: out.stdout,
        content_type: if stderr.is_empty() {
            "application/octet-stream".into()
        } else {
            stderr
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Completed,
    NotPlanned,
}

impl IssueState {
    fn close_reason(self) -> Option<&'static str> {
        match self {
            IssueState::Open => None,
            IssueState::Completed => Some("COMPLETED"),
            IssueState::NotPlanned => Some("NOT_PLANNED"),
        }
    }
}

#[derive(Debug, PartialEq)]
enum StateStep {
    Reopen,
    Close(&'static str),
}

/// A new close reason is applied by reopening and closing again, rather than relying on `closeIssue`
/// to update an issue that is already closed.
fn state_steps(issue: &Issue, target: IssueState) -> Vec<StateStep> {
    let open = issue.state == "OPEN";
    match target.close_reason() {
        None if open => vec![],
        None => vec![StateStep::Reopen],
        Some(reason) if open => vec![StateStep::Close(reason)],
        Some(reason) if issue.state_reason.as_deref() == Some(reason) => vec![],
        Some(reason) => vec![StateStep::Reopen, StateStep::Close(reason)],
    }
}

/// Applies `target` on GitHub and mirrors it onto `issue`.
pub fn set_issue_state(issue: &mut Issue, target: IssueState) -> Result<()> {
    for step in state_steps(issue, target) {
        match step {
            StateStep::Reopen => graphql(
                "mutation($id: ID!) { reopenIssue(input: {issueId: $id}) { issue { id } } }",
                &[("id", &issue.id)],
            )?,
            StateStep::Close(reason) => graphql(
                "mutation($id: ID!, $reason: IssueClosedStateReason!) { \
closeIssue(input: {issueId: $id, stateReason: $reason}) { issue { id } } }",
                &[("id", &issue.id), ("reason", reason)],
            )?,
        };
    }
    issue.state = if target == IssueState::Open {
        "OPEN"
    } else {
        "CLOSED"
    }
    .into();
    issue.state_reason = target.close_reason().map(String::from);
    Ok(())
}

fn pick_issue_type(types: &Value, name: &str) -> Result<String> {
    let nodes = types.as_array().map(Vec::as_slice).unwrap_or_default();
    nodes
        .iter()
        .find(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|t| t["id"].as_str())
        .map(String::from)
        .with_context(|| {
            let names: Vec<&str> = nodes.iter().filter_map(|t| t["name"].as_str()).collect();
            format!(
                "no issue type named {name}; this repo has: {}",
                names.join(", ")
            )
        })
}

/// Node ids are inlined into the mutation because `gh api -f` can only pass string variables.
/// Older accounts have base64 ids such as `MDQ6VXNlcjY4MzYzMTU=`.
fn is_node_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='))
}

pub struct CreatedSubtask {
    pub url: String,
    /// The issue exists, but adding it to the status board failed.
    pub board_error: Option<String>,
}

/// Creates an issue of type `type_name` in `repo` as a sub-issue of `parent_id`, optionally assigned
/// to the viewer and added to Projects board number `board` of the repo's owner.
pub fn create_subtask(
    repo: &str,
    parent_id: &str,
    title: &str,
    body: &str,
    type_name: &str,
    assign_me: bool,
    board: Option<u64>,
) -> Result<CreatedSubtask> {
    let (owner, name) = repo.split_once('/').context("repo must be owner/name")?;
    let info = graphql(
        "query($owner: String!, $name: String!) { viewer { id } \
repository(owner: $owner, name: $name) { id issueTypes(first: 25) { nodes { id name } } } }",
        &[("owner", owner), ("name", name)],
    )?;
    let repo_node = &info["data"]["repository"];
    let repo_id = repo_node["id"].as_str().context("repository not found")?;
    let type_id = pick_issue_type(&repo_node["issueTypes"]["nodes"], type_name)?;
    let assignees = match info["data"]["viewer"]["id"].as_str() {
        _ if !assign_me => String::new(),
        Some(id) if is_node_id(id) => format!(", assigneeIds: [\"{id}\"]"),
        Some(id) => bail!("unexpected GitHub user id {id:?}"),
        None => bail!("could not resolve your GitHub user id"),
    };
    let created = graphql(
        &format!(
            "mutation($repo: ID!, $title: String!, $body: String!, $type: ID!, $parent: ID!) {{ \
createIssue(input: {{repositoryId: $repo, title: $title, body: $body, issueTypeId: $type, \
parentIssueId: $parent{assignees}}}) {{ issue {{ id url }} }} }}"
        ),
        &[
            ("repo", repo_id),
            ("title", title),
            ("body", body),
            ("type", &type_id),
            ("parent", parent_id),
        ],
    )?;
    let issue = &created["data"]["createIssue"]["issue"];
    let url = issue["url"]
        .as_str()
        .context("createIssue returned no url")?;
    let board_error = board.and_then(|number| {
        add_to_board(owner, number, issue["id"].as_str().unwrap_or_default())
            .err()
            .map(|e| format!("{e:#}"))
    });
    Ok(CreatedSubtask {
        url: url.into(),
        board_error,
    })
}

fn add_to_board(owner: &str, number: u64, content_id: &str) -> Result<()> {
    let board = graphql(
        &format!(
            "query($owner: String!) {{ repositoryOwner(login: $owner) {{ \
... on ProjectV2Owner {{ projectV2(number: {number}) {{ id }} }} }} }}"
        ),
        &[("owner", owner)],
    )?;
    let project_id = board["data"]["repositoryOwner"]["projectV2"]["id"]
        .as_str()
        .with_context(|| format!("board {number} not found on {owner}"))?;
    let add = || {
        graphql(
            "mutation($project: ID!, $content: ID!) { \
addProjectV2ItemById(input: {projectId: $project, contentId: $content}) { item { id } } }",
            &[("project", project_id), ("content", content_id)],
        )
    };
    // A just-created issue isn't always visible to the Projects API yet; it shows up within seconds.
    for _ in 0..3 {
        if add().is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    add().map(drop)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RepoHit {
    pub full_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_private: bool,
}

pub fn search_repos(query: &str) -> Result<Vec<RepoHit>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(vec![]);
    }
    let text = process::run(
        "gh",
        &[
            "search",
            "repos",
            query,
            "--limit",
            "20",
            "--json",
            "fullName,description,isPrivate",
        ],
        None,
    )?;
    Ok(serde_json::from_str(&text)?)
}

/// `origin/<default branch>`, the base new worktree branches start from.
pub fn default_base(name_with_owner: &str) -> Result<String> {
    let branch = process::run(
        "gh",
        &[
            "repo",
            "view",
            name_with_owner,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
        None,
    )?;
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("{name_with_owner} has no default branch");
    }
    Ok(format!("origin/{branch}"))
}

pub fn clone_repo(name_with_owner: &str, dest: &std::path::Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    process::run(
        "gh",
        &["repo", "clone", name_with_owner, &dest.to_string_lossy()],
        None,
    )
    .map(drop)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
}

impl PullRequest {
    pub fn is_merged(&self) -> bool {
        self.state.eq_ignore_ascii_case("MERGED")
    }
}

pub fn pull_requests(repo: &str, branch: &str) -> Result<Vec<PullRequest>> {
    let text = process::run(
        "gh",
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,title,url,state,isDraft",
        ],
        None,
    )?;
    Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_issue_queries() {
        assert_eq!(parse_issue_query("  "), IssueQuery::Assigned);
        assert_eq!(
            parse_issue_query("https://github.com/o/r/issues/5"),
            IssueQuery::Url("https://github.com/o/r/issues/5".into())
        );
        assert_eq!(
            parse_issue_query("o/r#12"),
            IssueQuery::InRepo("o/r".into(), 12)
        );
        assert_eq!(parse_issue_query("412"), IssueQuery::Number(412));
        assert_eq!(parse_issue_query("#412"), IssueQuery::Number(412));
        assert_eq!(
            parse_issue_query("fix #12 login"),
            IssueQuery::Text("fix #12 login".into())
        );
        assert_eq!(parse_issue_query("o#12"), IssueQuery::Text("o#12".into()));
    }

    #[test]
    fn parses_pr_references() {
        assert_eq!(
            parse_pr_reference("https://github.com/o/r/pull/5"),
            Some(PrReference::Url("https://github.com/o/r/pull/5".into()))
        );
        assert_eq!(
            parse_pr_reference("github.com/o/r/pull/5"),
            Some(PrReference::Url("github.com/o/r/pull/5".into()))
        );
        assert_eq!(
            parse_pr_reference(" o/r#12 "),
            Some(PrReference::InRepo {
                owner: "o".into(),
                name: "r".into(),
                number: 12
            })
        );
        for rejected in [
            "/o/r#12",
            "o/r#abc",
            "o/r/x#12",
            "#12",
            "12",
            "fix login",
            "",
            "   ",
        ] {
            assert_eq!(parse_pr_reference(rejected), None, "{rejected:?}");
        }
    }

    fn f(id: &str, number: u64, ty: &str, assignees: &[&str]) -> Value {
        json!({
            "id": id, "number": number, "title": format!("T{number}"), "url": "u", "state": "OPEN",
            "body": format!("body {number}"), "issueType": {"name": ty},
            "repository": {"nameWithOwner": "acme/acme-web"},
            "assignees": {"nodes": assignees.iter().map(|a| json!({"login": a})).collect::<Vec<_>>()},
            "subIssuesSummary": {"total": 0, "completed": 0}
        })
    }

    #[test]
    fn ingests_search_with_parents_and_sub_issues() {
        let mut subtask = f("T", 327, "Subtask", &["me"]);
        let mut story = f("S", 318, "Story", &[]);
        let mut epic = f("E", 116, "Epic", &[]);
        epic["subIssues"] = json!({"nodes": [f("OTHER", 99, "Story", &[])]});
        story["parent"] = epic;
        story["subIssues"] = json!({"nodes": [f("SIB", 604, "Subtask", &[])]});
        subtask["parent"] = story;
        subtask["subIssues"] = json!({"nodes": []});
        let mut assigned_story = f("S2", 681, "Story", &["me"]);
        assigned_story["parent"] = Value::Null;
        assigned_story["subIssues"] = json!({"nodes": [f("C", 709, "Subtask", &["other"])]});
        let page = json!({"data": {"viewer": {"login": "me"}, "search": {
            "pageInfo": {"hasNextPage": true, "endCursor": "abc"},
            "nodes": [subtask, assigned_story, {}]
        }}});
        let mut idx = IssueIndex::new("me".into());
        assert_eq!(
            ingest_search(&page, &mut idx, &|i| i.issue_type.as_deref()
                == Some("Subtask"))
            .unwrap()
            .as_deref(),
            Some("abc")
        );
        assert_eq!(idx.issues.len(), 6);
        assert_eq!(idx.get("SIB").unwrap().parent_id.as_deref(), Some("S"));
        assert!(
            idx.get("OTHER").is_none(),
            "an epic's other stories are not pulled in"
        );
        let chain: Vec<_> = idx.ancestors("T").iter().map(|i| i.number).collect();
        assert_eq!(chain, vec![318, 116]);
        assert_eq!(idx.get("C").unwrap().parent_id.as_deref(), Some("S2"));
        assert!(idx.is_assigned(idx.get("T").unwrap()));
        assert!(!idx.is_assigned(idx.get("C").unwrap()));
    }

    fn h(id: &str, number: u64, parent: Value) -> Value {
        json!({
            "id": id, "number": number, "title": format!("T{number}"), "url": "u", "state": "OPEN",
            "issueType": {"name": "Story"}, "repository": {"nameWithOwner": "o/r"},
            "assignees": {"nodes": []}, "parent": parent
        })
    }

    fn ids(hits: &[IssueHit]) -> Vec<(&str, bool)> {
        hits.iter().map(|h| (h.id.as_str(), h.matched)).collect()
    }

    #[test]
    fn flattens_matches_then_ancestors_once_each() {
        let epic = h("E", 1, Value::Null);
        let story = h("S", 2, epic);
        let a = h("A", 3, story.clone());
        let b = h("B", 4, story);
        let hits = flatten_with_ancestors(&[a, Value::Null, json!({}), b]);
        assert_eq!(
            ids(&hits),
            vec![("A", true), ("B", true), ("S", false), ("E", false)]
        );
        assert_eq!(hits[0].parent_id.as_deref(), Some("S"));
        assert_eq!(hits[2].parent_id.as_deref(), Some("E"));
        assert_eq!(hits[3].parent_id, None);
        assert_eq!(hits[0].issue_type.as_deref(), Some("Story"));
    }

    #[test]
    fn a_match_that_is_also_an_ancestor_stays_matched_in_search_order() {
        let epic = h("E", 1, Value::Null);
        let story = h("S", 2, epic.clone());
        let task = h("T", 3, story);
        let hits = flatten_with_ancestors(&[task, epic]);
        assert_eq!(ids(&hits), vec![("T", true), ("E", true), ("S", false)]);
    }

    #[test]
    fn keeps_a_parent_id_any_copy_knows() {
        let deepest = json!({"id": "TOP"});
        let e = h("E", 1, deepest);
        let d = h("D", 2, e);
        let c = h("C", 3, d);
        let b = h("B", 4, c);
        let a = h("A", 5, b);
        let e_alone = h("E", 1, Value::Null);
        let hits = flatten_with_ancestors(&[e_alone, a]);
        assert_eq!(
            ids(&hits),
            vec![
                ("E", true),
                ("A", true),
                ("B", false),
                ("C", false),
                ("D", false)
            ]
        );
        assert_eq!(hits[0].parent_id.as_deref(), Some("TOP"));
        assert!(
            !hits.iter().any(|h| h.id == "TOP"),
            "an id alone isn't a hit"
        );
    }

    #[test]
    fn parses_board_status_skipping_closed_boards() {
        let mut node = f("I", 1, "Story", &[]);
        let board = |id: &str, closed: bool, option: Value| {
            json!({"id": format!("ITEM_{id}"), "fieldValueByName": option, "project": {
                "id": id, "number": 5, "title": "Roadmap", "closed": closed,
                "field": {"id": "FIELD", "options": [{"id": "a", "name": "Todo"}, {"id": "b", "name": "Done"}]}
            }})
        };
        node["projectItems"] = json!({"nodes": [
            board("P1", false, json!({"optionId": "b"})),
            board("P2", false, Value::Null),
            board("P3", true, json!({"optionId": "a"})),
        ]});
        let issue = parse_issue(&node).unwrap();
        assert_eq!(issue.boards.len(), 2);
        assert_eq!(issue.boards[0].option_id.as_deref(), Some("b"));
        assert_eq!(issue.boards[0].item_id, "ITEM_P1");
        assert_eq!(issue.boards[0].options[0].name, "Todo");
        assert_eq!(issue.boards[1].option_id, None, "no Status set yet");
    }

    #[test]
    fn picks_issue_types_by_name() {
        let types = json!([{"id": "IT_1", "name": "Story"}, {"id": "IT_2", "name": "Subtask"}]);
        assert_eq!(pick_issue_type(&types, "subtask").unwrap(), "IT_2");
        let err = pick_issue_type(&types, "Task").unwrap_err().to_string();
        assert!(err.contains("Story, Subtask"), "{err}");
        assert!(is_node_id("U_kgDOAAA-1_x"));
        assert!(is_node_id("MDQ6VXNlcjY4MzYzMTU="));
        assert!(!is_node_id("x\"]) { evil"));
    }

    #[test]
    fn only_github_hosts_get_the_token() {
        assert!(is_github_asset(
            "https://github.com/user-attachments/assets/ee4c76ec"
        ));
        assert!(is_github_asset(
            "https://private-user-images.githubusercontent.com/1/x.png?jwt=a"
        ));
        assert!(!is_github_asset("http://github.com/x.png"));
        assert!(!is_github_asset("https://github.com.evil.example/x.png"));
        assert!(!is_github_asset("https://github.com@evil.example/x.png"));
        assert!(!is_github_asset(
            "https://evil.githubusercontent.com.example/x"
        ));
        assert!(!is_github_asset("https://imgur.com/x.png"));
    }

    #[test]
    fn state_steps_reopen_before_changing_close_reason() {
        use IssueState::*;
        use StateStep::*;
        let mut issue = crate::hierarchy::tests::issue("I", 1, None, None, &[]);
        assert_eq!(state_steps(&issue, Open), vec![]);
        assert_eq!(state_steps(&issue, Completed), vec![Close("COMPLETED")]);

        issue.state = "CLOSED".into();
        issue.state_reason = Some("COMPLETED".into());
        assert_eq!(state_steps(&issue, Completed), vec![]);
        assert_eq!(state_steps(&issue, Open), vec![Reopen]);
        assert_eq!(
            state_steps(&issue, NotPlanned),
            vec![Reopen, Close("NOT_PLANNED")]
        );
    }
}
