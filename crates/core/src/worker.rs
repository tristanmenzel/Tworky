use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::accounts;
use crate::claude_config;
use crate::config::{AccountConfig, Config, Hook, ProjectConfig, RepoConfig};
use crate::git;
use crate::github::{self, PullRequest};
use crate::hierarchy::{Issue, IssueIndex};
use crate::ports;
use crate::process;
use crate::services;
use crate::stack::{self, Compose};
use crate::state::{
    BranchOrigin, IssueRef, ItemRef, ItemSource, PrRef, State, WorkerRecord, WorkerStatus,
    WorktreeRecord,
};
use crate::template;

pub type Log<'a> = &'a mut dyn FnMut(&str);

enum Subject {
    Issue(IssueRef),
    Item(ItemRef, Option<PrRef>),
}

pub struct Workspace {
    pub config: Config,
    pub state: State,
    /// Where Claude Code keeps session transcripts; `None` skips the resume check.
    pub claude_home: Option<PathBuf>,
}

const STOPPED_NOTE: &str = "Tworky: this worker was terminated, so its docker containers and any dev \
servers, watchers or test runs started in it are stopped. Before running the app, a dev server or \
tests, run tw worker setup to start the stack again, then start the dev servers you need.";

const FEEDBACK_NOTE: &str = "Tworky: if you hit a bug or friction in Tworky or tw itself (not the \
project), such as confusing tw output, a missing command or a hook or service that misbehaves, propose \
it to the user in a line or two. Only after they agree, file it with: tw issue bug|friction \"<title>\" \
--body-file -. If tw lists similar open issues, use tw issue comment <number> instead. Keep project \
details (private repo names, issue links, code) out of the report.";

/// Hooks run in bash.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A resumed conversation re-sends the system prompt recorded on its first request, so
/// `--append-system-prompt` would be ignored; a SessionStart hook's output is added as new context.
/// `session` names the activity record to report to; a worker's session reports to `$TW_WORKER_ID`.
pub(crate) fn session_settings(
    project: &str,
    memory: &Path,
    stopped: bool,
    browser: bool,
    session: Option<&str>,
) -> String {
    let target = session
        .map(|s| format!(" --worker {}", sh_quote(s)))
        .unwrap_or_default();
    let memory = sh_quote(&memory.to_string_lossy().replace('\\', "/"));
    let header = sh_quote(&format!(
        "Tworky shared memory for project {project}, kept by every Claude working on it \
         (latest: tw memory show). Propose additions rarely: only a fact that cost real time, applies to \
         most {project} work, cannot be found quickly in the repo, and is not better fixed in the repo or \
         reported as a Tworky problem."
    ));
    let mut hooks = vec![serde_json::json!({
        "type": "command",
        "command": format!("if [ -s {memory} ]; then echo {header}; echo; cat {memory}; fi"),
    })];
    hooks.push(serde_json::json!({ "type": "command", "command": format!("echo {}", sh_quote(FEEDBACK_NOTE)) }));
    if stopped {
        hooks.push(serde_json::json!({ "type": "command", "command": format!("echo {}", sh_quote(STOPPED_NOTE)) }));
    }
    // `|| true`: a PreToolUse hook exiting with 2 would block the tool call.
    let report = |activity: &str| {
        serde_json::json!([{ "hooks": [{
            "type": "command",
            "command": format!("tw session-state {activity}{target} || true"),
        }] }])
    };
    let waiting = report("waiting");
    let mut settings = serde_json::json!({
        "hooks": {
            "SessionStart": [
                { "matcher": "startup|resume", "hooks": hooks },
                { "matcher": "startup|resume", "hooks": report("idle")[0]["hooks"] },
                { "matcher": "clear", "hooks": report("idle --new-conversation")[0]["hooks"] },
            ],
            "UserPromptSubmit": report("working --new-turn"),
            "PreToolUse": [{ "matcher": "AskUserQuestion|ExitPlanMode", "hooks": waiting[0]["hooks"] }],
            "PermissionRequest": waiting,
            "Notification": [{
                "matcher": "permission_prompt|elicitation_dialog|elicitation_url_dialog",
                "hooks": waiting[0]["hooks"],
            }],
            "PostToolUse": report("working"),
            "PostToolUseFailure": report("working"),
            "Stop": report("idle"),
            "StopFailure": report("idle"),
        }
    });
    let mut allow = shell_rules(&TW_GITHUB_WRITES);
    if browser {
        allow.push(format!("mcp__{MCP_SERVER}"));
    }
    settings["permissions"] = serde_json::json!({ "ask": shell_rules(&GH_WRITES), "allow": allow });
    settings.to_string()
}

/// Auto mode's classifier blocks GitHub writes unless the user's own message names the exact action,
/// and an answer to Claude's question or a plan approval doesn't count, so they failed until the user
/// repeated the request. Permission rules are checked before the classifier: Tworky's own writes are
/// allowed outright, and `gh` writes get an `ask` rule, which shows the command in a prompt.
const TW_GITHUB_WRITES: [&str; 2] = ["tw subtask add", "tw issue"];

const GH_WRITES: [&str; 4] = [
    "gh issue create",
    "gh issue comment",
    "gh pr comment",
    "gh pr review",
];

fn shell_rules(commands: &[&str]) -> Vec<String> {
    commands
        .iter()
        .flat_map(|c| [format!("Bash({c} *)"), format!("PowerShell({c} *)")])
        .collect()
}

/// Graft reads `<root>/.claude/graft-settings.json`. A worker folder isn't a repo but holds the
/// item's repos, so Graft's plan and execute work across them from there. An existing file is kept.
fn write_graft_settings(dir: &Path) -> Result<()> {
    let file = dir.join(".claude").join("graft-settings.json");
    if file.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(
        &file,
        "{ \"plan\": { \"outsideRepoPlans\": \"here\" } }
",
    )
    .with_context(|| format!("writing {}", file.display()))
}

/// The name sessions see Tworky's browser MCP server under.
pub const MCP_SERVER: &str = "TworkyWeb";

/// Tworky's browser MCP server, with a token that only this launch's session holds.
#[derive(Debug, Clone)]
pub struct McpEndpoint {
    pub url: String,
    pub token: String,
}

fn mcp_config(endpoint: &McpEndpoint) -> String {
    serde_json::json!({ "mcpServers": { MCP_SERVER: {
        "type": "http",
        "url": endpoint.url,
        "headers": { "Authorization": format!("Bearer {}", endpoint.token) },
    } } })
    .to_string()
}

/// A job, with the file its output goes to when Claude started it as a background command.
#[derive(Debug, Clone, Serialize)]
pub struct WorkerJob {
    #[serde(flatten)]
    pub job: process::Job,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    /// The stored conversation had no transcript, so a new one is being started instead.
    pub replaced_missing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoPrs {
    pub repo: String,
    pub branch: String,
    pub name_with_owner: Option<String>,
    pub pull_requests: Vec<PullRequest>,
    pub status: Option<git::WorktreeStatus>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrSummary {
    pub repos: Vec<RepoPrs>,
    /// Every worktree with work has a merged PR, and no worktree holds unpublished changes.
    pub all_merged: bool,
}

impl Workspace {
    pub fn open() -> Result<Self> {
        let config = Config::load()?;
        let state = State::open(&config.paths.state_db())?;
        Ok(Self {
            config,
            state,
            claude_home: claude_config::claude_home(),
        })
    }

    pub fn project_for_issue(&self, issue: &Issue) -> Result<&ProjectConfig> {
        self.config
            .project_for_repo(&issue.repo)
            .with_context(|| format!("no project config matches {}", issue.repo))
    }

    pub(crate) fn project_of(&self, w: &WorkerRecord) -> Result<&ProjectConfig> {
        self.config
            .project(&w.project)
            .with_context(|| format!("project '{}' is no longer configured", w.project))
    }

    pub(crate) fn worker(&self, id: &str) -> Result<WorkerRecord> {
        self.state
            .get(id)?
            .with_context(|| format!("worker '{id}' not found"))
    }

    pub fn worker_for_issue(&self, issue_id: &str) -> Result<Option<WorkerRecord>> {
        Ok(self
            .state
            .list()?
            .into_iter()
            .find(|w| w.issue_id() == Some(issue_id)))
    }

    /// An issue item's worker is the issue's worker; any other item's worker carries the item.
    pub fn worker_for_item(&self, name: &str) -> Result<Option<WorkerRecord>> {
        if let Some(issue) = self.state.get_item(name)?.and_then(|i| i.issue().cloned()) {
            return self.worker_for_issue(&issue.id);
        }
        Ok(self
            .state
            .list()?
            .into_iter()
            .find(|w| w.item.as_ref().is_some_and(|i| i.name == name)))
    }

    /// Creates the worker record (allocating a port slot) without touching the filesystem.
    pub fn plan(&self, index: &IssueIndex, issue_id: &str) -> Result<WorkerRecord> {
        let issue = index.get(issue_id).context("issue not loaded")?;
        let project = self.project_for_issue(issue)?;
        let subject = IssueRef {
            id: issue.id.clone(),
            number: issue.number,
            repo: issue.repo.clone(),
            title: issue.title.clone(),
            url: issue.url.clone(),
        };
        self.plan_issue(project, subject, index)
    }

    fn plan_issue(
        &self,
        project: &ProjectConfig,
        issue: IssueRef,
        index: &IssueIndex,
    ) -> Result<WorkerRecord> {
        if let Some(existing) = self.worker_for_issue(&issue.id)? {
            return Ok(existing);
        }
        let mut id = format!("{}-{}", project.name, issue.number);
        if self.state.get(&id)?.is_some() {
            let repo_short = issue.repo.rsplit('/').next().unwrap_or("repo");
            id = format!(
                "{}-{}-{}",
                project.name,
                template::slugify(repo_short, 20),
                issue.number
            );
        }
        let dir = format!("{}-{}", issue.number, template::slugify(&issue.title, 24));
        self.insert_record(project, id, &dir, Subject::Issue(issue), index)
    }

    /// Like `plan`, for a work item. An issue item gets the issue's worker; `index` gives it context.
    pub fn plan_item(&self, name: &str, index: &IssueIndex) -> Result<WorkerRecord> {
        if let Some(existing) = self.worker_for_item(name)? {
            return Ok(existing);
        }
        let item = self
            .state
            .get_item(name)?
            .with_context(|| format!("item '{name}' not found"))?;
        let project = self
            .config
            .project(&item.project)
            .with_context(|| format!("project '{}' is no longer configured", item.project))?;
        let subject = ItemRef {
            name: item.name.clone(),
            title: item.title.clone(),
            description: item.description.clone(),
        };
        match &item.source {
            ItemSource::Issue(issue) => self.plan_issue(project, issue.clone(), index),
            ItemSource::PullRequest(pr) => {
                let id = format!("{}-pr{}", project.name, pr.number);
                let id = if self.state.get(&id)?.is_some() {
                    format!("{}-{}", project.name, item.name)
                } else {
                    id
                };
                let dir = format!("pr{}-{}", pr.number, template::slugify(&pr.title, 24));
                self.insert_record(
                    project,
                    id,
                    &dir,
                    Subject::Item(subject, Some(pr.clone())),
                    &IssueIndex::default(),
                )
            }
            ItemSource::Custom => self.insert_record(
                project,
                format!("{}-{}", project.name, item.name),
                &item.name,
                Subject::Item(subject, None),
                &IssueIndex::default(),
            ),
        }
    }

    fn insert_record(
        &self,
        project: &ProjectConfig,
        id: String,
        dir_name: &str,
        subject: Subject,
        index: &IssueIndex,
    ) -> Result<WorkerRecord> {
        let (issue, item, pr) = match subject {
            Subject::Issue(i) => (Some(i), None, None),
            Subject::Item(i, pr) => (None, Some(i), pr),
        };
        let existing = self.state.list()?;
        if let Some(max) = self.config.app.max_active_workers
            && existing.len() >= max
        {
            bail!("{max} workers already exist (max_active_workers); clean one up first");
        }
        let (slot, worker_ports) = ports::allocate(
            &project.ports,
            self.config.app.port_stride,
            self.config.app.max_slots,
            &existing.iter().map(|w| w.slot).collect::<Vec<_>>(),
            ports::is_free,
        )?;
        let dir = self
            .config
            .app
            .workers_root
            .join(&project.name)
            .join(dir_name);
        let mut record = WorkerRecord {
            id: id.clone(),
            project: project.name.clone(),
            issue,
            item,
            pr,
            branch: String::new(),
            slot,
            ports: worker_ports,
            dir,
            compose_project: stack::compose_project_name(&id),
            status: WorkerStatus::Creating,
            claude_session: uuid::Uuid::new_v4().to_string(),
            claude_started: false,
            error: None,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
            worktrees: vec![],
            stopped: false,
        };
        record.branch = match &record.pr {
            Some(pr) => pr.head.clone(),
            None => {
                let ctx = template_context(&record, project, index);
                template::render_str(&project.branch, &ctx)?
                    .trim()
                    .to_string()
            }
        };
        if record.branch.is_empty() {
            bail!("branch template rendered empty");
        }
        self.state.insert(&record)?;
        Ok(record)
    }

    /// Creates the worker directory and its CLAUDE.md. Worktrees, the stack and hooks are
    /// left to the session (`tw worktree add`, `tw worker setup`).
    pub fn prepare(&self, id: &str, index: &IssueIndex) -> Result<WorkerRecord> {
        let mut w = self.worker(id)?;
        let project = self.project_of(&w)?;
        std::fs::create_dir_all(&w.dir).with_context(|| format!("creating {}", w.dir.display()))?;
        std::fs::write(
            w.dir.join("CLAUDE.md"),
            worker_claude_md(
                &w,
                project,
                index,
                &self.config.paths.memory_file(&project.name),
            ),
        )?;
        write_graft_settings(&w.dir)?;
        w.status = WorkerStatus::Ready;
        w.error = None;
        self.state.save(&w)?;
        Ok(w)
    }

    fn repo<'a>(project: &'a ProjectConfig, name: &str) -> Result<&'a RepoConfig> {
        project
            .repos
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name))
            .with_context(|| {
                let names: Vec<_> = project.repos.iter().map(|r| r.name.as_str()).collect();
                format!(
                    "unknown repo '{name}'; expected one of: {}",
                    names.join(", ")
                )
            })
    }

    /// Adds a worktree for `repo` and renders its worker config. `branch` defaults to the worker's
    /// suggested branch: an existing local or remote branch is checked out, otherwise it is created
    /// from `base` (default: the repo's configured base). A worktree already at the target path is
    /// adopted as-is, so one made with plain `git worktree add` can be brought under management.
    pub fn add_worktree(
        &self,
        id: &str,
        repo_name: &str,
        branch: Option<&str>,
        base: Option<&str>,
        log: Log,
    ) -> Result<WorktreeRecord> {
        let mut w = self.worker(id)?;
        let project = self.project_of(&w)?.clone();
        let repo = Self::repo(&project, repo_name)?;
        if let Some(existing) = w.worktrees.iter().find(|t| t.repo == repo.name) {
            bail!(
                "{} already has a worktree on {} (remove it first with `tw worktree rm {}`)",
                repo.name,
                existing.branch,
                repo.name
            );
        }
        let path = w.dir.join(&repo.name);
        let record = if git::is_worktree(&path) {
            let branch = git::current_branch(&path)?;
            log(&format!(
                "▸ {}: adopting existing worktree on {branch}",
                repo.name
            ));
            WorktreeRecord {
                repo: repo.name.clone(),
                branch,
                origin: BranchOrigin::Existing,
            }
        } else {
            log(&format!("▸ {}: fetching", repo.name));
            if let Err(e) = git::fetch(&repo.source) {
                log(&format!(
                    "  fetch failed, continuing with local refs: {e:#}"
                ));
            }
            let branch = branch.unwrap_or(&w.branch).to_string();
            let base = base.unwrap_or(&repo.base);
            let origin = git::worktree_add(&repo.source, &path, &branch, base)
                .with_context(|| format!("worktree for {}", repo.name))?;
            let how = match origin {
                BranchOrigin::New => format!("new branch from {base}"),
                BranchOrigin::Tracking => "tracking origin".to_string(),
                BranchOrigin::Existing => "existing local branch".to_string(),
            };
            log(&format!(
                "▸ {}: worktree {} on {branch} ({how})",
                repo.name,
                path.display()
            ));
            WorktreeRecord {
                repo: repo.name.clone(),
                branch,
                origin,
            }
        };

        let ctx = template_context(&w, &project, &IssueIndex::default());
        let template_dir = self.config.template_dir(&project).join(&repo.name);
        for rel in template::render_dir(&template_dir, &path, &ctx)? {
            let inner = rel.to_string_lossy().replace('\\', "/");
            log(&format!("▸ config: {}/{inner}", repo.name));
            if git::is_tracked(&path, &inner) {
                git::skip_worktree(&path, &inner)?;
                log(&format!(
                    "  {inner} is tracked: marked skip-worktree so it can't be committed"
                ));
            }
        }

        w.worktrees.push(record.clone());
        self.state.save(&w)?;
        Ok(record)
    }

    pub fn remove_worktree(&self, id: &str, repo_name: &str, log: Log) -> Result<()> {
        let mut w = self.worker(id)?;
        let project = self.project_of(&w)?.clone();
        let repo = Self::repo(&project, repo_name)?;
        self.drop_worktree(&w, repo, log)?;
        w.worktrees.retain(|t| t.repo != repo.name);
        self.state.save(&w)
    }

    fn drop_worktree(&self, w: &WorkerRecord, repo: &RepoConfig, log: Log) -> Result<()> {
        let path = w.dir.join(&repo.name);
        let record = w.worktrees.iter().find(|t| t.repo == repo.name);
        if !repo.source.is_dir() {
            if path.exists() {
                log(&format!(
                    "▸ {}: the clone at {} is gone, so removing the worktree folder without git",
                    repo.name,
                    repo.source.display()
                ));
                remove_dir(&path)?;
            }
            return Ok(());
        }
        if git::is_worktree(&path) || record.is_some() {
            log(&format!("▸ {}: removing worktree", repo.name));
            git::worktree_remove(&repo.source, &path)?;
        }
        if let Some(t) = record.filter(|t| t.origin.created_by_tworky()) {
            log(&format!(
                "▸ {}: deleting local branch {}",
                repo.name, t.branch
            ));
            git::branch_delete(&repo.source, &t.branch)?;
        }
        Ok(())
    }

    fn has_worktree(w: &WorkerRecord, rel: &str) -> bool {
        let first = rel.split(['/', '\\']).next().unwrap_or_default();
        w.worktrees
            .iter()
            .any(|t| t.repo.eq_ignore_ascii_case(first))
    }

    /// Starts the stack and runs setup hooks. Re-runnable: hooks for repos without a worktree are
    /// skipped and can be picked up by running setup again later.
    pub fn setup(&self, id: &str, log: Log) -> Result<()> {
        let mut w = self.worker(id)?;
        let project = self.project_of(&w)?.clone();
        let ctx = template_context(&w, &project, &IssueIndex::default());

        if let Some(stack_cfg) = &project.stack {
            if !Self::has_worktree(&w, &stack_cfg.compose_dir) {
                bail!(
                    "the stack is defined in {}; add its worktree first (`tw worktree add {}`)",
                    stack_cfg.compose_dir,
                    stack_cfg
                        .compose_dir
                        .split(['/', '\\'])
                        .next()
                        .unwrap_or_default()
                );
            }
            log(&format!(
                "▸ stack: docker compose -p {} up",
                w.compose_project
            ));
            Compose::new(&w.compose_project, &w.dir, stack_cfg).up(log)?;
        }

        for hook in &project.hooks.setup {
            if !Self::has_worktree(&w, &hook.cwd) {
                log(&format!("▸ hook ({}): skipped, no worktree", hook.cwd));
                continue;
            }
            self.run_hook(&w, hook, &ctx, log)?;
        }

        w.stopped = false;
        self.state.save(&w)?;
        log("✓ setup complete");
        if project.services.is_empty() {
            log(&format!("note: {}", crate::services::NO_SERVICES_NOTE));
        }
        Ok(())
    }

    fn run_hook(&self, w: &WorkerRecord, hook: &Hook, ctx: &Value, log: Log) -> Result<()> {
        let Some(run) = hook.run.for_current_os() else {
            bail!(
                "the hook in '{}' has no command for {}",
                hook.cwd,
                std::env::consts::OS
            );
        };
        let run = template::render_str(run, ctx)?;
        let cwd = w.dir.join(&hook.cwd);
        log(&format!("▸ hook ({}): {run}", hook.cwd));
        let shell = &self.config.app.shell;
        let mut cmd = process::command(&shell[0]);
        cmd.args(&shell[1..])
            .arg(&run)
            .current_dir(&cwd)
            .envs(worker_env(w));
        process::stream(cmd, &run, log)
    }

    /// The project's accounts as this worker sees them: URLs rendered and moved to its ports, and
    /// services resolved to their URLs. An account naming a missing or portless service is left
    /// out, so one bad account doesn't hide the rest.
    pub fn worker_accounts(&self, w: &WorkerRecord) -> Result<Vec<AccountConfig>> {
        let project = self.project_of(w)?;
        let ctx = template_context(w, project, &IssueIndex::default());
        let render = |s: &str| template::render_str(s, &ctx);
        let url = |u: &str| accounts::worker_url(u, &project.ports, &w.ports, &render);
        let service = |name: &str| {
            project
                .service(name.trim())
                .with_context(|| format!("no service called {name}"))
        };
        let mut out = Vec::new();
        for a in &project.accounts {
            let unusable = accounts::unusable_services(project, &w.ports, a);
            if let Some(why) = unusable.first() {
                eprintln!("{}: skipping it: {why}", project.name);
                continue;
            }
            let start_url = match &a.start_service {
                Some(name) => services::service_url(w, service(name)?)
                    .with_context(|| format!("service {name} has no port"))?,
                None => url(&a.start_url)?,
            };
            let login_origins = a
                .login_origins
                .iter()
                .map(|o| worker_login_origin(w, project, &url, o))
                .collect::<Result<_>>()?;
            out.push(AccountConfig {
                start_url,
                login_origins,
                ..a.clone()
            });
        }
        Ok(out)
    }

    /// Why workers leave out the project's account called `name`, if it exists and they do.
    pub fn unusable_account(&self, w: &WorkerRecord, name: &str) -> Option<String> {
        let project = self.project_of(w).ok()?;
        let account = project
            .accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name.trim()))?;
        accounts::unusable_services(project, &w.ports, account)
            .into_iter()
            .next()
    }

    /// The `login_origins` entry that allows `origin`, an origin seen on this worker, saved so
    /// other workers map it to their own ports. Fails when this worker would read the entry back
    /// as a different origin, so saving it wouldn't allow the page.
    pub fn login_origin_for_worker(&self, w: &WorkerRecord, origin: &str) -> Result<String> {
        let project = self.project_of(w)?;
        let origin = origin.trim_end_matches('/');
        let saved = accounts::login_origin_to_save(project, &w.ports, origin);
        let ctx = template_context(w, project, &IssueIndex::default());
        let render = |s: &str| template::render_str(s, &ctx);
        let url = |u: &str| accounts::worker_url(u, &project.ports, &w.ports, &render);
        let resolved = worker_login_origin(w, project, &url, &saved)?;
        if resolved.trim_end_matches('/') != origin {
            bail!(
                "{origin} can't be saved as a login origin: it would be saved as {saved}, which \
                 this worker reads as {resolved}, because {origin} uses one of the project's base \
                 ports rather than this worker's (it's probably the main checkout or another \
                 worker)"
            );
        }
        Ok(saved)
    }

    /// Allows the account's password on `origin`, an origin seen on this worker (see
    /// [`Self::login_origin_for_worker`]). Returns the entry as saved. The workspace's project
    /// config is then stale, so re-open it to read the account back.
    pub fn add_worker_login_origin(
        &self,
        w: &WorkerRecord,
        account: &str,
        origin: &str,
    ) -> Result<String> {
        let project = self.project_of(w)?;
        let saved = self.login_origin_for_worker(w, origin)?;
        accounts::add_login_origin(project, account, &saved)?;
        Ok(saved)
    }

    /// The folder Claude runs in, which also names the folder its transcripts are kept in.
    pub fn session_cwd(&self, w: &WorkerRecord) -> Result<PathBuf> {
        let project = self.project_of(w)?;
        Ok(w.dir
            .join(&project.session_cwd)
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect())
    }

    /// Dev servers, watchers and test runs still running in each worker.
    pub fn jobs(
        &self,
        workers: &[WorkerRecord],
        tasks: &mut claude_config::BackgroundTasks,
    ) -> Result<HashMap<String, Vec<WorkerJob>>> {
        let scopes: Vec<(PathBuf, String)> = workers
            .iter()
            .map(|w| (w.dir.clone(), w.claude_session.clone()))
            .collect();
        let found = process::jobs(&scopes)?;
        let mut out = HashMap::new();
        for (w, jobs) in workers.iter().zip(found) {
            if jobs.is_empty() {
                continue;
            }
            let outputs = match (&self.claude_home, self.session_cwd(w)) {
                (Some(home), Ok(cwd)) if jobs.iter().any(|j| j.from_claude) => {
                    tasks.outputs(&claude_config::transcripts_dir(home, &cwd))
                }
                _ => Default::default(),
            };
            let jobs = jobs
                .into_iter()
                .map(|mut job| {
                    if let Some(command) = job
                        .task_output
                        .as_ref()
                        .and_then(|file| outputs.powershell.get(file))
                    {
                        job.command = command.clone();
                    }
                    WorkerJob {
                        output: job
                            .task_output
                            .clone()
                            .or_else(|| {
                                job.from_claude
                                    .then(|| outputs.by_command.get(&job.command).cloned())
                                    .flatten()
                            })
                            .or_else(|| self.service_log(&w.id, &job.roots)),
                        job,
                    }
                })
                .collect();
            out.insert(w.id.clone(), jobs);
        }
        Ok(out)
    }

    /// Stops these workers' Claude sessions and their jobs, leaving the stacks running.
    pub fn stop_sessions(&self, ids: &[String], log: Log) -> Result<()> {
        let sessions: Vec<(PathBuf, String)> = ids
            .iter()
            .filter_map(|id| match crate::project_setup::session_project(id) {
                Some(project) => {
                    let s = self.state.project_session(project).ok()??;
                    Some((
                        crate::project_setup::setup_dir(&self.config, project),
                        s.claude_session,
                    ))
                }
                None => self.worker(id).ok().map(|w| (w.dir, w.claude_session)),
            })
            .collect();
        process::stop_sessions(&sessions, log)
    }

    pub fn stop_job(&self, id: &str, pid: u32, log: Log) -> Result<()> {
        let w = self.worker(id)?;
        process::stop_job(&w.dir, &w.claude_session, pid, log)
    }

    pub fn launch(&self, id: &str, mcp: Option<&McpEndpoint>) -> Result<Launch> {
        let mut w = self.worker(id)?;
        let project = self.project_of(&w)?;
        let cwd = self.session_cwd(&w)?;
        write_graft_settings(&w.dir)?;
        let mut replaced_missing = false;
        if w.claude_started
            && let Some(home) = &self.claude_home
            && !claude_config::transcript_path(home, &cwd, &w.claude_session).exists()
        {
            // Nothing to resume: the conversation was never saved, so `--resume` would only fail.
            w.claude_session = uuid::Uuid::new_v4().to_string();
            w.claude_started = false;
            self.state.save(&w)?;
            replaced_missing = true;
        }
        let mut args: Vec<String> = self.config.app.claude[1..].to_vec();
        let memory = self.config.paths.memory_file(&project.name);
        args.extend([
            "--settings".into(),
            session_settings(&project.name, &memory, w.stopped, mcp.is_some(), None),
        ]);
        if let Some(endpoint) = mcp {
            args.extend(["--mcp-config".into(), mcp_config(endpoint)]);
        }
        if w.claude_started {
            args.extend(["--resume".into(), w.claude_session.clone()]);
        } else {
            args.extend([
                "--session-id".into(),
                w.claude_session.clone(),
                "--permission-mode".into(),
                "auto".into(),
            ]);
        }
        Ok(Launch {
            program: self.config.app.claude[0].clone(),
            args,
            cwd,
            env: worker_env(&w),
            replaced_missing,
        })
    }

    pub fn mark_claude_started(&self, id: &str) -> Result<()> {
        let mut w = self.worker(id)?;
        w.claude_started = true;
        self.state.save(&w)
    }

    /// Starts a new conversation on next launch, for when the stored one can't be resumed.
    pub fn reset_claude_session(&self, id: &str) -> Result<()> {
        let mut w = self.worker(id)?;
        w.claude_session = uuid::Uuid::new_v4().to_string();
        w.claude_started = false;
        self.state.save(&w)?;
        crate::activity::new_conversation(&self.config.paths, id)
    }

    pub fn pr_summary(&self, id: &str) -> Result<PrSummary> {
        let w = self.worker(id)?;
        let project = self.project_of(&w)?;
        let mut repos = Vec::new();
        for t in &w.worktrees {
            let Ok(repo) = Self::repo(project, &t.repo) else {
                continue;
            };
            let mut entry = RepoPrs {
                repo: t.repo.clone(),
                branch: t.branch.clone(),
                name_with_owner: None,
                pull_requests: vec![],
                status: git::status(&w.dir.join(&t.repo)).ok(),
                error: None,
            };
            match git::origin_name_with_owner(&repo.source)
                .and_then(|nwo| github::pull_requests(&nwo, &t.branch).map(|prs| (nwo, prs)))
            {
                Ok((nwo, prs)) => {
                    entry.name_with_owner = Some(nwo);
                    entry.pull_requests = prs;
                }
                Err(e) => entry.error = Some(format!("{e:#}")),
            }
            repos.push(entry);
        }
        let all_merged = repos.iter().any(|r| !r.pull_requests.is_empty())
            && repos.iter().all(|r| {
                r.error.is_none()
                    && match r.pull_requests.first() {
                        Some(pr) => pr.is_merged(),
                        None => r
                            .status
                            .as_ref()
                            .is_some_and(|s| s.dirty_files == 0 && s.unpushed_commits == 0),
                    }
            });
        Ok(PrSummary { repos, all_merged })
    }

    fn stack_to_stop(&self, w: &WorkerRecord, log: Log) -> Option<Compose> {
        let configured = self
            .config
            .project(&w.project)
            .and_then(|p| p.stack.as_ref())
            .map(|cfg| Compose::new(&w.compose_project, &w.dir, cfg));
        stack_to_stop(
            configured,
            &w.compose_project,
            |c| c.ps(STACK_PS_TIMEOUT),
            log,
        )
    }

    /// Stops the session's `claude` (pid `session_pid`) with everything it started, and the stack's
    /// containers, keeping containers and volumes so setup can start them again.
    pub fn terminate(&self, id: &str, session_pid: Option<u32>, log: Log) -> Result<()> {
        let mut w = self.worker(id)?;
        process::stop_processes(session_pid, &w.dir, log)?;
        self.stop_services(id, log)?;
        self.forget_services(id)?;
        w.stopped = true;
        self.state.save(&w)?;
        if let Some(compose) = self.stack_to_stop(&w, log) {
            log(&format!(
                "▸ stack: docker compose -p {} stop",
                w.compose_project
            ));
            compose.stop(log)?;
        }
        log("✓ stopped");
        Ok(())
    }

    /// Stops the stack's containers, keeping them and their volumes.
    pub fn stop_stack(&self, id: &str, log: Log) -> Result<()> {
        let w = self.worker(id)?;
        match self.stack_to_stop(&w, log) {
            Some(compose) => {
                log(&format!(
                    "▸ stack: docker compose -p {} stop",
                    w.compose_project
                ));
                compose.stop(log)
            }
            None => Ok(()),
        }
    }

    /// Tears a worker down. Steps continue past failures so one stuck resource doesn't strand the rest;
    /// the record is kept (as Failed) if anything went wrong so cleanup can be retried.
    pub fn cleanup(&self, id: &str, session_pid: Option<u32>, log: Log) -> Result<()> {
        let mut w = self.worker(id)?;
        w.status = WorkerStatus::Cleaning;
        self.state.save(&w)?;
        let project = self.config.project(&w.project).cloned();
        let mut errors: Vec<String> = Vec::new();
        fn attempt(errors: &mut Vec<String>, label: &str, result: Result<()>, log: Log) {
            if let Err(e) = result {
                log(&format!("  {label} failed: {e:#}"));
                errors.push(format!("{label}: {e:#}"));
            }
        }
        let errors = &mut errors;

        let r = process::stop_processes(session_pid, &w.dir, log);
        attempt(errors, "stop session processes", r, log);
        let r = self.stop_services(id, log);
        attempt(errors, "stop services", r, log);
        let r = self.forget_services(id);
        attempt(errors, "forget services", r, log);

        if let Some(project) = &project {
            let ctx = template_context(&w, project, &IssueIndex::default());
            for hook in &project.hooks.teardown {
                if !Self::has_worktree(&w, &hook.cwd) {
                    continue;
                }
                if !w.dir.join(&hook.cwd).is_dir() {
                    log(&format!(
                        "  skipping the teardown hook in {}: the folder is gone",
                        hook.cwd
                    ));
                    continue;
                }
                let r = self.run_hook(&w, hook, &ctx, log);
                attempt(errors, "teardown hook", r, log);
            }
        }

        if let Some(compose) = self.stack_to_stop(&w, log) {
            log(&format!(
                "▸ stack: docker compose -p {} down -v",
                w.compose_project
            ));
            let r = compose.down(log);
            attempt(errors, "compose down", r, log);
        }

        if let Some(project) = &project {
            for repo in &project.repos {
                let r = self.drop_worktree(&w, repo, log);
                attempt(errors, &format!("worktree {}", repo.name), r, log);
            }
        }

        if errors.is_empty() && w.dir.exists() {
            log(&format!("▸ removing {}", w.dir.display()));
            let r = remove_dir(&w.dir);
            attempt(errors, "remove worker directory", r, log);
        }

        if errors.is_empty() {
            self.state.delete(&w.id)?;
            if let Some(item) = &w.item
                && self.state.get_item(&item.name)?.is_some_and(|i| i.scratch)
            {
                self.state.delete_item(&item.name)?;
            }
            crate::activity::forget(&self.config.paths, &w.id);
            log("✓ cleaned up");
            Ok(())
        } else {
            w.status = WorkerStatus::Failed;
            w.error = Some(format!("cleanup incomplete: {}", errors.join("; ")));
            self.state.save(&w)?;
            bail!("cleanup incomplete: {}", errors.join("; "))
        }
    }

    pub fn item_for_worker(&self, w: &WorkerRecord) -> Result<Option<String>> {
        if let Some(item) = &w.item {
            return Ok(Some(item.name.clone()));
        }
        match w.issue_id() {
            Some(issue_id) => Ok(self.state.item_for_issue(issue_id)?.map(|i| i.name)),
            None => Ok(None),
        }
    }

    pub fn cleanup_and_remove_item(
        &self,
        id: &str,
        session_pid: Option<u32>,
        log: Log,
    ) -> Result<()> {
        let w = self.worker(id)?;
        let item = self.item_for_worker(&w)?;
        self.cleanup(id, session_pid, log)?;
        match item {
            Some(name) => {
                if self.state.get_item(&name)?.is_some() {
                    self.state.delete_item(&name)?;
                }
                log(&format!("✓ removed {name} from the list"));
            }
            None => log("  the worker has no item on the list"),
        }
        Ok(())
    }
}

fn remove_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

const STACK_PS_TIMEOUT: Duration = Duration::from_secs(10);

/// The compose project to stop or remove. With a `[stack]`, that's the configured stack when its
/// files exist, else the project by name, so a Docker failure still fails cleanup. Without one,
/// it's the project by name only when `ps` lists containers for it.
fn stack_to_stop(
    configured: Option<Compose>,
    project_name: &str,
    ps: impl FnOnce(&Compose) -> Result<Vec<stack::ServiceStatus>>,
    log: Log,
) -> Option<Compose> {
    if let Some(compose) = configured {
        return Some(if compose.files_exist() {
            compose
        } else {
            Compose::by_name(project_name)
        });
    }
    let compose = Compose::by_name(project_name);
    match ps(&compose) {
        Ok(containers) if containers.is_empty() => None,
        Ok(_) => Some(compose),
        Err(e) => {
            log(&format!(
                "  couldn't list docker compose project {project_name} ({e:#}), so it was left alone"
            ));
            None
        }
    }
}

/// Every `TW_*` variable also has a `WA_*` twin: worktrees, templates and notes made before the
/// rename from work-app still read the old names.
const ENV_PREFIXES: [&str; 2] = ["TW_", "WA_"];

pub fn worker_env(w: &WorkerRecord) -> BTreeMap<String, String> {
    let mut vars = vec![
        ("WORKER_ID".to_string(), w.id.clone()),
        (
            "WORKER_DIR".to_string(),
            w.dir.to_string_lossy().into_owned(),
        ),
        ("SLOT".to_string(), w.slot.to_string()),
        ("BRANCH".to_string(), w.branch.clone()),
    ];
    for (name, port) in &w.ports {
        vars.push((
            format!("PORT_{}", name.to_ascii_uppercase()),
            port.to_string(),
        ));
    }
    let mut env: BTreeMap<String, String> = vars
        .iter()
        .flat_map(|(k, v)| ENV_PREFIXES.map(|p| (format!("{p}{k}"), v.clone())))
        .collect();
    env.insert("COMPOSE_PROJECT_NAME".into(), w.compose_project.clone());
    env
}

/// The worker id from `TW_WORKER_ID`, or `WA_WORKER_ID` in sessions started before the rename.
pub fn worker_id_from_env() -> Option<String> {
    ENV_PREFIXES
        .iter()
        .find_map(|p| std::env::var(format!("{p}WORKER_ID")).ok())
        .filter(|id| !id.is_empty())
}

fn issue_json(issue: &Issue) -> Value {
    json!({
        "id": issue.id,
        "number": issue.number,
        "title": issue.title,
        "slug": template::slugify(&issue.title, 24),
        "url": issue.url,
        "repo": issue.repo,
        "type": issue.issue_type,
    })
}

/// `#123` for an issue worker, the item's name for an item worker.
fn subject_tag(w: &WorkerRecord) -> String {
    match (&w.issue, &w.item) {
        (Some(i), _) => format!("#{}", i.number),
        (None, Some(i)) => i.name.clone(),
        (None, None) => w.id.clone(),
    }
}

fn worker_login_origin(
    w: &WorkerRecord,
    project: &ProjectConfig,
    url: &dyn Fn(&str) -> Result<String>,
    entry: &str,
) -> Result<String> {
    match entry.strip_prefix(accounts::SERVICE_ORIGIN) {
        Some(name) => {
            let service = project
                .service(name.trim())
                .with_context(|| format!("no service called {name}"))?;
            services::service_origin(w, service)
                .with_context(|| format!("service {name} has no port"))
        }
        None => url(entry),
    }
}

/// Templates see an item as an `issue` whose `number` is the item's name, so branch templates
/// written for issues still give a unique branch.
pub(crate) fn template_context(
    w: &WorkerRecord,
    project: &ProjectConfig,
    index: &IssueIndex,
) -> Value {
    let issue = match (&w.issue, &w.item) {
        (Some(i), _) => index.get(&i.id).map(issue_json).unwrap_or_else(|| {
            json!({
                "id": i.id, "number": i.number, "title": i.title,
                "slug": template::slugify(&i.title, 24), "url": i.url, "repo": i.repo, "type": null,
            })
        }),
        (None, Some(i)) => json!({
            "id": i.name, "number": i.name, "title": i.title,
            "slug": template::slugify(&i.title, 24), "url": "", "repo": "", "type": "Item",
        }),
        (None, None) => Value::Null,
    };
    let ancestors: Vec<Value> = w
        .issue_id()
        .map(|id| index.ancestors(id).into_iter().map(issue_json).collect())
        .unwrap_or_default();
    json!({
        "worker": {
            "id": w.id,
            "slot": w.slot,
            "dir": w.dir.to_string_lossy(),
            "compose_project": w.compose_project,
            "branch": w.branch,
        },
        "ports": w.ports,
        "issue": issue,
        "parent": ancestors.first(),
        "ancestors": ancestors,
        "project": { "name": project.name },
    })
}

pub fn worker_claude_md(
    w: &WorkerRecord,
    project: &ProjectConfig,
    index: &IssueIndex,
    memory: &Path,
) -> String {
    let mut md = String::new();
    let push = |md: &mut String, s: &str| {
        md.push_str(s);
        md.push('\n');
    };
    push(
        &mut md,
        &format!("# Worker {} — {}\n", w.id, w.subject_label()),
    );
    push(
        &mut md,
        "This directory is an isolated Tworky worker. Tworky generated this file when the session began.\n",
    );

    push(&mut md, "## Working alongside other Claudes\n");
    push(
        &mut md,
        &format!(
            "You are one of several Claude sessions working on different {0} issues at the same time. Each \
             session has its own worker folder, worktrees, ports and docker compose project.\n\n\
             - Stay inside this worker. Don't edit the main checkouts or other workers' folders, and don't \
             stop or remove containers except with `-p {1}`.\n\
             - The {0} shared memory holds instructions and facts every Claude on {0} relies on. It was loaded \
             when this session started; `tw memory show` prints the latest. Check it before setting up, running \
             the app or tests, or starting a story. Follow it unless the user or a repo's CLAUDE.md says otherwise.\n\
             - The memory has a high bar, and most sessions should add nothing to it. Propose a fact only when \
             all of these hold:\n  \
               - It cost you real time: a failure whose cause wasn't obvious, or a detour of ten minutes or more.\n  \
               - It applies to most {0} work, not just this kind of task.\n  \
               - Another Claude couldn't find it quickly from the repo, an error message or `git status`.\n  \
               - It isn't better fixed at the source.\n\
             - Send findings to the right place instead of the memory:\n  \
               - A bug or friction in Tworky or `tw` itself goes to the user first, then `tw issue`, as the \
             Tworky note at session start explains.\n  \
               - A repo quirk goes in that repo's CLAUDE.md, `.gitignore` or config, as part of your PR.\n  \
               - Anything about this one issue stays in your summary.\n\
             - When a fact does pass, propose it to the user in one or two lines: what to do, and what goes \
             wrong otherwise. Only after the user agrees, save it with `tw memory add \"<fact>\"` (add \
             `--section <name>` to file it under a heading other than Learned).\n\
             - The memory file is `{2}`. Don't edit it directly; use `tw memory add`.\n",
            project.name,
            w.compose_project,
            memory.display()
        ),
    );

    push(&mut md, "## Work item\n");
    if let (Some(item), Some(pr)) = (&w.item, &w.pr) {
        push(
            &mut md,
            &format!(
                "### Pull request {}#{}: {}\n\n{}\n\nBranch `{}` into `{}`. The worker's branch is the PR's head, \
                 so `tw worktree add` checks it out. If the head isn't on origin (a fork), run `gh pr checkout {}` \
                 inside the worktree instead. `gh pr view {} --repo {}` shows the description and review state.\n",
                pr.repo,
                pr.number,
                pr.title,
                pr.url,
                pr.head,
                pr.base,
                pr.number,
                pr.number,
                pr.repo
            ),
        );
        if !item.description.trim().is_empty() {
            push(&mut md, &format!("{}\n", item.description.trim()));
        }
    } else if let Some(item) = &w.item {
        push(
            &mut md,
            &format!("### Ad-hoc item `{}`: {}\n", item.name, item.title),
        );
        if !item.description.trim().is_empty() {
            push(&mut md, &format!("{}\n", item.description.trim()));
        }
        push(
            &mut md,
            "There is no GitHub issue for this work yet. If one is created, the user can link this item to it \
             in Tworky; until then there is no issue to reference in commits or PRs.\n",
        );
    }
    let issue_id = w.issue_id().unwrap_or_default();
    if let Some(i) = &w.issue {
        let issue = index.get(&i.id);
        let ty = issue
            .and_then(|i| i.issue_type.as_deref())
            .unwrap_or("Issue");
        push(
            &mut md,
            &format!("### {ty} #{}: {}\n\n{}\n", i.number, i.title, i.url),
        );
        if let Some(body) = issue.map(|i| i.body.trim()).filter(|b| !b.is_empty()) {
            push(&mut md, &format!("{body}\n"));
        }
    }
    let ancestors = index.ancestors(issue_id);
    if !ancestors.is_empty() {
        push(&mut md, "## Context (parent issues, nearest first)\n");
        push(
            &mut md,
            "The work is usually described on the story; the item above is the slice to deliver in this worker.\n",
        );
        for a in ancestors {
            let include_body = project.context_types.is_empty()
                || a.issue_type.as_deref().is_some_and(|t| {
                    project
                        .context_types
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(t))
                });
            let ty = a.issue_type.as_deref().unwrap_or("Issue");
            push(
                &mut md,
                &format!("### {ty} #{}: {}\n\n{}\n", a.number, a.title, a.url),
            );
            if include_body && !a.body.trim().is_empty() {
                push(&mut md, &format!("{}\n", a.body.trim()));
            }
        }
    }

    let children = index.children(issue_id);
    if !children.is_empty() {
        push(&mut md, "## Sub-issues\n");
        for c in children {
            let ty = c.issue_type.as_deref().unwrap_or("Issue");
            let who = if c.assignees.is_empty() {
                "unassigned".to_string()
            } else {
                format!("assigned to {}", c.assignees.join(", "))
            };
            push(
                &mut md,
                &format!(
                    "### {ty} #{}: {} ({}, {who})\n\n{}\n",
                    c.number,
                    c.title,
                    c.state.to_ascii_lowercase(),
                    c.url
                ),
            );
            if !c.body.trim().is_empty() {
                push(&mut md, &format!("{}\n", c.body.trim()));
            }
        }
    }

    push(&mut md, "## Repositories\n");
    push(
        &mut md,
        "No worktrees exist until you ask for them. These repos are available:\n",
    );
    for r in &project.repos {
        push(
            &mut md,
            &format!(
                "- `{}` — main checkout `{}`, base `{}`",
                r.name,
                r.source.display(),
                r.base
            ),
        );
    }
    push(
        &mut md,
        &format!(
            "\nThe suggested branch for new work is `{}`. Each repo has its own CLAUDE.md; read it before working in that repo.\n",
            w.branch
        ),
    );

    push(&mut md, "## Setting up the worker\n");
    push(
        &mut md,
        "Use the `tw` CLI. It is on PATH and reads the worker id from `TW_WORKER_ID`.\n",
    );
    push(
        &mut md,
        &format!(
            "- `tw worktree add <repo>` creates `<repo>/` here on `{}`, branched from the repo's base.\n\
             - `tw worktree add <repo> --branch <name>` checks out an existing local or remote branch, \
             or creates it from the base if it doesn't exist. Add `--base <ref>` to branch from somewhere else.\n\
             - `tw worktree ls` lists worktrees; `tw worktree rm <repo>` removes one.\n\
             - `tw worker setup` starts the local stack and runs the setup hooks (dependency installs, migrations). \
             Run it once the worktrees you need exist; it is safe to re-run and skips hooks for repos without a worktree.\n",
            w.branch
        ),
    );
    push(
        &mut md,
        "Adding a worktree also writes its worker config (`.env` files and similar) with this worker's ports. \
         Don't edit those values back to the defaults.\n",
    );

    if !w.ports.is_empty() {
        push(&mut md, "## Ports\n");
        push(
            &mut md,
            "This worker runs on its own ports so it doesn't clash with the main checkout or other workers. \
             Always use these, never the defaults:\n",
        );
        push(&mut md, "| Name | Port | Env var |\n|---|---|---|");
        for (name, port) in &w.ports {
            push(
                &mut md,
                &format!(
                    "| {name} | {port} | `TW_PORT_{}` |",
                    name.to_ascii_uppercase()
                ),
            );
        }
        md.push('\n');
    }
    if project.stack.is_some() {
        push(&mut md, "## Local stack\n");
        push(
            &mut md,
            &format!(
                "`tw worker setup` runs the infrastructure as docker compose project `{0}`. Use \
                 `docker compose -p {0} ps` to inspect it. Don't run `docker compose down` without `-p {0}`. \
                 Terminating the session in Tworky stops these containers; if they're stopped, run \
                 `tw worker setup` again before running the app or tests.\n",
                w.compose_project
            ),
        );
    }

    push(&mut md, "## Review\n");
    push(
        &mut md,
        &format!(
            "Use the `{MCP_SERVER}` MCP tools to open the running apps in browser tabs beside this terminal, \
             signed in as the project's test accounts, take screenshots and test flows. `tw service ls` lists \
             the project's dev servers and `tw service start <name>` starts one on this worker's ports.\n"
        ),
    );

    push(&mut md, "## Delivering\n");
    push(&mut md, "- Use Conventional Commits.");
    push(
        &mut md,
        "- Push each worktree's branch and open one PR per changed repo with `gh pr create`.",
    );
    if let Some(i) = &w.issue {
        push(&mut md, &format!("- Link the PR to {} in its body.", i.url));
    }
    push(
        &mut md,
        "\n## Writing to GitHub\n\n\
         `tw subtask add` runs without a prompt, so agree subtasks with the user before adding them. Creating \
         issues and posting comments or reviews show the user a permission prompt with the exact command, even in \
         auto mode, so the user approves each one there. Use these commands, which the prompt recognises, rather \
         than `gh api`:\n\n\
         - Issue comments: `gh issue comment`\n\
         - PR comments: `gh pr comment`\n\
         - PR reviews: `gh pr review` (`--comment`, `--approve` or `--request-changes`, with `--body-file`)\n\n\
         Run each as its own command, not chained with `&&` or `;`, so each prompt shows one action. If a write is \
         denied anyway, say which command and why, and let the user decide; don't retry it unchanged.\n",
    );
    if !project.notes.trim().is_empty() {
        push(
            &mut md,
            &format!("\n## Project notes\n\n{}", project.notes.trim()),
        );
    }
    md.replace("{project}", &project.name)
        .replace("{subject}", &subject_tag(w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hierarchy::tests::issue;
    use std::time::{Duration, Instant};

    fn project() -> ProjectConfig {
        toml::from_str(
            r#"
name = "acme"
match = { owner = "o" }
context_types = ["Story"]
branch = "feat/{{ parent.slug if parent else issue.slug }}-{{ issue.number }}"
[[repos]]
name = "platform"
source = 'C:\src\platform'
[ports]
api = 4000
[stack]
compose_dir = "platform"
[[hooks.setup]]
cwd = "frontend/app"
run = "npm ci"
"#,
        )
        .unwrap()
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_dir: dir.path().join("c"),
            data_dir: dir.path().join("d"),
        };
        let mut config = Config::load_from(paths).unwrap();
        config.app.workers_root = dir.path().join("work");
        config.projects.push(project());
        let state = State::open(&config.paths.state_db()).unwrap();
        let claude_home = Some(dir.path().join("claude-home"));
        (
            dir,
            Workspace {
                config,
                state,
                claude_home,
            },
        )
    }

    fn index() -> IssueIndex {
        let mut idx = IssueIndex::new("me".into());
        let mut story = issue("S", 318, Some("Story"), Some("E"), &[]);
        story.title = "Header layout".into();
        story.body = "As a shopper I want the header to fit on my phone".into();
        idx.insert(issue("E", 116, Some("Epic"), None, &[]));
        idx.insert(story);
        let mut task = issue("T", 327, Some("Subtask"), Some("S"), &["me"]);
        task.title = "Implement - Web".into();
        task.repo = "o/tracker".into();
        idx.insert(task);
        idx
    }

    fn with_services(ws: &mut Workspace) {
        let extra: ProjectConfig = toml::from_str(
            r#"
name = "x"
repos = []
match = { owner = "o" }
[ports]
api = 4000
web = 5173
[[services]]
name = "web"
port = "web"
run = "npm run dev"
[[services]]
name = "api"
port = "api"
https = true
run = "npm start"
[[services]]
name = "queue"
run = "npm run queue"
[[accounts]]
name = "Tester"
start_service = "web"
login_origins = ["service:api", "http://localhost:4000/auth"]
[[accounts]]
name = "Missing"
start_service = "nope"
[[accounts]]
name = "Portless"
start_url = "http://localhost:5173/"
login_origins = ["service:queue"]
[[accounts]]
name = "Plain"
start_url = "http://localhost:5173/home"
"#,
        )
        .unwrap();
        let p = &mut ws.config.projects[0];
        p.ports = extra.ports;
        p.services = extra.services;
        p.accounts = extra.accounts;
    }

    #[test]
    fn service_urls_use_the_workers_port_and_scheme() {
        let (_d, mut ws) = workspace();
        with_services(&mut ws);
        let w = ws.plan(&index(), "T").unwrap();
        let p = &ws.config.projects[0];
        assert_eq!(
            services::service_url(&w, p.service("web").unwrap()),
            Some(format!("http://localhost:{}/", w.ports["web"]))
        );
        assert_eq!(
            services::service_url(&w, p.service("api").unwrap()),
            Some(format!("https://localhost:{}/", w.ports["api"]))
        );
        assert_eq!(services::service_url(&w, p.service("queue").unwrap()), None);
    }

    #[test]
    fn worker_accounts_resolve_services_and_skip_ones_naming_a_bad_service() {
        let (_d, mut ws) = workspace();
        with_services(&mut ws);
        let w = ws.plan(&index(), "T").unwrap();
        let (web, api) = (w.ports["web"], w.ports["api"]);
        assert_ne!(web, 5173, "a worker slot shifts the ports");
        let accounts = ws.worker_accounts(&w).unwrap();
        let names: Vec<_> = accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Tester", "Plain"]);
        assert_eq!(accounts[0].start_url, format!("http://localhost:{web}/"));
        assert_eq!(
            accounts[0].login_origins,
            [
                format!("https://localhost:{api}"),
                format!("http://localhost:{api}/auth")
            ]
        );
        assert_eq!(
            accounts[1].start_url,
            format!("http://localhost:{web}/home")
        );
        assert_eq!(
            ws.unusable_account(&w, "missing").as_deref(),
            Some("account Missing names service nope, which doesn't exist")
        );
        assert_eq!(
            ws.unusable_account(&w, "Portless").as_deref(),
            Some("account Portless names service queue, which has no port")
        );
        assert_eq!(ws.unusable_account(&w, "Tester"), None);
        assert_eq!(ws.unusable_account(&w, "Nobody"), None);
    }

    #[test]
    fn login_origins_are_saved_only_when_this_worker_reads_them_back() {
        let (_d, mut ws) = workspace();
        with_services(&mut ws);
        let w = ws.plan(&index(), "T").unwrap();
        let (web, api) = (w.ports["web"], w.ports["api"]);
        let save = |origin: &str| ws.login_origin_for_worker(&w, origin);
        assert_eq!(
            save(&format!("http://localhost:{web}")).unwrap(),
            "service:web"
        );
        assert_eq!(
            save(&format!("https://localhost:{api}/")).unwrap(),
            "service:api"
        );
        assert_eq!(
            save(&format!("http://127.0.0.1:{web}")).unwrap(),
            "http://127.0.0.1:5173"
        );
        assert_eq!(
            save(&format!("http://[::1]:{web}")).unwrap(),
            format!("http://[::1]:{web}")
        );
        assert_eq!(
            save("https://login.example.com").unwrap(),
            "https://login.example.com"
        );
        let refused = save("http://localhost:5173").unwrap_err().to_string();
        assert!(
            refused.starts_with(&format!(
                "http://localhost:5173 can't be saved as a login origin: it would be saved as \
                 http://localhost:5173, which this worker reads as http://localhost:{web}"
            )),
            "{refused}"
        );
    }

    #[test]
    fn plan_allocates_slot_branch_and_is_idempotent() {
        let (_d, ws) = workspace();
        let idx = index();
        let w = ws.plan(&idx, "T").unwrap();
        assert_eq!(w.id, "acme-327");
        assert_eq!(w.branch, "feat/header-layout-327");
        assert!(w.slot >= 1);
        assert_eq!(w.ports["api"], 4000 + w.slot * 100);
        assert_eq!(w.compose_project, "tw-acme-327");
        assert_eq!(ws.plan(&idx, "T").unwrap().id, w.id);
        assert_eq!(ws.state.list().unwrap().len(), 1);
    }

    #[test]
    fn item_workers_get_a_named_folder_branch_and_claude_md() {
        let (_d, ws) = workspace();
        let item = ws
            .create_item(
                "acme",
                "Fix admin-portal audit",
                "npm audit fails on CI",
                false,
            )
            .unwrap();
        assert!(ws.create_item("nope", "x", "", false).is_err());
        assert!(ws.create_item("acme", "  ", "", false).is_err());

        let w = ws.plan_item(&item.name, &IssueIndex::default()).unwrap();
        assert_eq!(w.id, format!("acme-{}", item.name));
        assert!(
            w.dir
                .ends_with(std::path::Path::new("acme").join(&item.name))
        );
        assert_eq!(
            w.branch,
            format!("feat/fix-admin-portal-audit-{}", item.name)
        );
        assert_eq!(w.issue, None);
        assert_eq!(
            ws.plan_item(&item.name, &IssueIndex::default()).unwrap().id,
            w.id,
            "idempotent"
        );

        let w = ws.prepare(&w.id, &IssueIndex::default()).unwrap();
        let md = std::fs::read_to_string(w.dir.join("CLAUDE.md")).unwrap();
        assert!(md.contains(&format!(
            "Ad-hoc item `{}`: Fix admin-portal audit",
            item.name
        )));
        assert!(md.contains("npm audit fails on CI"));
        assert!(!md.contains("Link the PR to"));
        assert!(!md.contains("## Context"));

        assert!(ws.remove_item(&item.name).is_err(), "worker still exists");
    }

    #[test]
    fn linking_an_item_moves_its_worker_to_the_issue() {
        let (_d, ws) = workspace();
        let idx = index();
        let item = ws.create_item("acme", "Fix audit", "", false).unwrap();
        let w = ws.plan_item(&item.name, &IssueIndex::default()).unwrap();
        ws.prepare(&w.id, &IssueIndex::default()).unwrap();

        let linked = ws.link_item_to(&item.name, &idx, "T").unwrap().unwrap();
        assert_eq!(linked.id, w.id, "keeps its id, folder and branch");
        assert_eq!(linked.branch, w.branch);
        assert_eq!(linked.issue_id(), Some("T"));
        assert!(linked.item.is_none());
        let relisted = ws.state.get_item(&item.name).unwrap().unwrap();
        assert_eq!(
            relisted.issue().unwrap().id,
            "T",
            "the item now stands for the issue"
        );
        assert_eq!(ws.worker_for_item(&item.name).unwrap().unwrap().id, w.id);
        assert_eq!(ws.worker_for_issue("T").unwrap().unwrap().id, w.id);
        let md = std::fs::read_to_string(w.dir.join("CLAUDE.md")).unwrap();
        assert!(md.contains("Subtask #327: Implement - Web"));

        let second = ws.create_item("acme", "Another", "", false).unwrap();
        let err = ws
            .link_item_to(&second.name, &idx, "T")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already has worker"), "{err}");
    }

    #[test]
    fn linking_refuses_an_issue_from_another_project() {
        let (_d, mut ws) = workspace();
        let mut other = project();
        other.name = "other".into();
        other.match_rule = toml::from_str(r#"repos = ["x/other"]"#).unwrap();
        ws.config.projects.push(other);
        let mut idx = index();
        let mut foreign = issue("F", 9, None, None, &[]);
        foreign.repo = "x/other".into();
        idx.insert(foreign);
        let item = ws.create_item("acme", "Fix audit", "", false).unwrap();

        let err = ws
            .link_item_to(&item.name, &idx, "F")
            .unwrap_err()
            .to_string();
        assert!(err.contains("belongs to project other"), "{err}");
        assert!(ws.state.get_item(&item.name).unwrap().is_some());
        ws.remove_item(&item.name).unwrap();
        assert!(ws.state.get_item(&item.name).unwrap().is_none());
    }

    fn insert_item(ws: &Workspace, name: &str, source: ItemSource, scratch: bool) {
        ws.state
            .insert_item(&crate::state::Item {
                name: name.into(),
                title: "t".into(),
                description: String::new(),
                project: "acme".into(),
                created_at: 0,
                source,
                scratch,
            })
            .unwrap();
    }

    #[test]
    fn an_issue_item_gets_the_issue_worker() {
        let (_d, ws) = workspace();
        let idx = index();
        let t = idx.get("T").unwrap();
        let issue = IssueRef {
            id: t.id.clone(),
            number: t.number,
            repo: t.repo.clone(),
            title: t.title.clone(),
            url: t.url.clone(),
        };
        insert_item(&ws, "keen-lynx", ItemSource::Issue(issue), false);
        let w = ws.plan_item("keen-lynx", &idx).unwrap();
        assert_eq!(w.id, "acme-327");
        assert_eq!(w.issue_id(), Some("T"));
        assert!(w.item.is_none());
        assert_eq!(ws.worker_for_item("keen-lynx").unwrap().unwrap().id, w.id);
        assert!(ws.remove_item("keen-lynx").is_err(), "worker still exists");
    }

    #[test]
    fn a_pull_request_item_works_on_the_head_branch() {
        let (_d, ws) = workspace();
        let pr = PrRef {
            number: 42,
            repo: "o/r".into(),
            title: "Add export".into(),
            url: "https://github.com/o/r/pull/42".into(),
            head: "feat/export".into(),
            base: "main".into(),
        };
        insert_item(&ws, "calm-yak", ItemSource::PullRequest(pr), false);
        let w = ws.plan_item("calm-yak", &IssueIndex::default()).unwrap();
        assert_eq!(w.id, "acme-pr42");
        assert_eq!(w.branch, "feat/export");
        assert_eq!(w.pr.as_ref().unwrap().number, 42);
        let w = ws.prepare(&w.id, &IssueIndex::default()).unwrap();
        let md = std::fs::read_to_string(w.dir.join("CLAUDE.md")).unwrap();
        assert!(md.contains("Pull request o/r#42: Add export"), "{md}");
    }

    #[test]
    fn cleanup_removes_a_scratch_item() {
        let (_d, ws) = workspace();
        insert_item(&ws, "quiet-lynx", ItemSource::Custom, true);
        insert_item(&ws, "tidy-seal", ItemSource::Custom, false);
        for name in ["quiet-lynx", "tidy-seal"] {
            let w = ws.plan_item(name, &IssueIndex::default()).unwrap();
            ws.cleanup(&w.id, None, &mut |_| {}).unwrap();
        }
        assert!(ws.state.get_item("quiet-lynx").unwrap().is_none());
        assert!(ws.state.get_item("tidy-seal").unwrap().is_some());
    }

    fn issue_ref_t() -> IssueRef {
        let idx = index();
        let t = idx.get("T").unwrap();
        IssueRef {
            id: t.id.clone(),
            number: t.number,
            repo: t.repo.clone(),
            title: t.title.clone(),
            url: t.url.clone(),
        }
    }

    #[test]
    fn cleanup_and_remove_item_removes_a_custom_item() {
        let (_d, ws) = workspace();
        insert_item(&ws, "tidy-seal", ItemSource::Custom, false);
        let w = ws.plan_item("tidy-seal", &IssueIndex::default()).unwrap();
        let mut lines = Vec::new();
        ws.cleanup_and_remove_item(&w.id, None, &mut |l| lines.push(l.to_string()))
            .unwrap();
        assert!(ws.state.get(&w.id).unwrap().is_none());
        assert!(ws.state.get_item("tidy-seal").unwrap().is_none());
        assert!(
            lines
                .iter()
                .any(|l| l == "✓ removed tidy-seal from the list")
        );
    }

    #[test]
    fn cleanup_and_remove_item_removes_an_issue_item() {
        let (_d, ws) = workspace();
        insert_item(&ws, "keen-lynx", ItemSource::Issue(issue_ref_t()), false);
        let w = ws.plan_item("keen-lynx", &index()).unwrap();
        ws.cleanup_and_remove_item(&w.id, None, &mut |_| {})
            .unwrap();
        assert!(ws.state.get(&w.id).unwrap().is_none());
        assert!(ws.state.get_item("keen-lynx").unwrap().is_none());
    }

    #[test]
    fn cleanup_and_remove_item_tolerates_a_scratch_item() {
        let (_d, ws) = workspace();
        insert_item(&ws, "quiet-lynx", ItemSource::Custom, true);
        let w = ws.plan_item("quiet-lynx", &IssueIndex::default()).unwrap();
        ws.cleanup_and_remove_item(&w.id, None, &mut |_| {})
            .unwrap();
        assert!(ws.state.get_item("quiet-lynx").unwrap().is_none());
    }

    #[test]
    fn a_failed_cleanup_keeps_the_item() {
        let (_d, mut ws, _source, id) = git_workspace();
        let project = &mut ws.config.projects[0];
        project.stack = None;
        project.hooks.teardown = toml::from_str::<crate::config::Hooks>(
            "[[teardown]]\ncwd = \"platform\"\nrun = \"exit 1\"",
        )
        .unwrap()
        .teardown;
        ws.add_worktree(&id, "platform", None, None, &mut |_| {})
            .unwrap();
        insert_item(&ws, "keen-lynx", ItemSource::Issue(issue_ref_t()), false);

        assert!(ws.cleanup_and_remove_item(&id, None, &mut |_| {}).is_err());
        assert!(ws.state.get_item("keen-lynx").unwrap().is_some());
        assert_eq!(
            ws.state.get(&id).unwrap().unwrap().status,
            WorkerStatus::Failed
        );
    }

    #[test]
    fn cleanup_and_remove_item_cleans_up_a_worker_with_no_item() {
        let (_d, ws) = workspace();
        let w = ws.plan(&index(), "T").unwrap();
        let mut lines = Vec::new();
        ws.cleanup_and_remove_item(&w.id, None, &mut |l| lines.push(l.to_string()))
            .unwrap();
        assert!(ws.state.get(&w.id).unwrap().is_none());
        assert!(lines.iter().any(|l| l.contains("no item on the list")));
    }

    #[test]
    fn prepare_writes_only_claude_md_and_graft_settings() {
        let (_d, ws) = workspace();
        let idx = index();
        let w = ws.plan(&idx, "T").unwrap();
        let w = ws.prepare(&w.id, &idx).unwrap();
        assert_eq!(w.status, WorkerStatus::Ready);
        let mut entries: Vec<_> = std::fs::read_dir(&w.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, vec![".claude", "CLAUDE.md"]);
        let graft = w.dir.join(".claude").join("graft-settings.json");
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&graft).unwrap()).unwrap();
        assert_eq!(settings["plan"]["outsideRepoPlans"], "here");

        std::fs::write(&graft, "{}").unwrap();
        ws.launch(&w.id, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&graft).unwrap(),
            "{}",
            "an edited file is kept"
        );
        std::fs::remove_file(&graft).unwrap();
        ws.launch(&w.id, None).unwrap();
        assert!(
            graft.is_file(),
            "launch restores it for workers made before"
        );
    }

    #[test]
    fn setup_requires_the_stack_worktree() {
        let (_d, ws) = workspace();
        let w = ws.plan(&index(), "T").unwrap();
        let err = ws.setup(&w.id, &mut |_| {}).unwrap_err().to_string();
        assert!(err.contains("tw worktree add platform"), "{err}");
    }

    #[test]
    fn unknown_repo_lists_choices() {
        let (_d, ws) = workspace();
        let w = ws.plan(&index(), "T").unwrap();
        let err = ws
            .add_worktree(&w.id, "nope", None, None, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected one of: platform"), "{err}");
    }

    #[test]
    fn claude_md_has_context_ports_and_setup_commands() {
        let (_d, ws) = workspace();
        let idx = index();
        let w = ws.plan(&idx, "T").unwrap();
        let memory = ws.config.paths.memory_file("acme");
        let md = worker_claude_md(&w, &project(), &idx, &memory);
        assert!(md.contains("## Working alongside other Claudes"));
        assert!(
            md.contains("Propose a fact only when all of these hold:\n  - It cost you real time")
        );
        assert!(md.contains(
            "\n  - A bug or friction in Tworky or `tw` itself goes to the user first, then `tw issue`"
        ));
        assert!(md.contains(&memory.display().to_string()));
        assert!(md.contains(&format!("except with `-p {}`", w.compose_project)));
        assert!(md.contains("Story #318: Header layout"));
        assert!(md.contains("fit on my phone"));
        assert!(md.contains("Epic #116"));
        assert!(md.contains(&format!("| api | {} |", w.ports["api"])));
        assert!(md.contains("`TworkyWeb` MCP tools"));
        assert!(md.contains("tw worktree add <repo> --branch <name>"));
        assert!(md.contains("feat/header-layout-327"));
        assert!(!md.contains("## Sub-issues"));

        let story = ws.plan(&idx, "S").unwrap();
        let md = worker_claude_md(&story, &project(), &idx, &memory);
        assert!(md.contains("## Sub-issues"));
        assert!(md.contains("Subtask #327: Implement - Web (open, assigned to me)"));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        process::run("git", args, Some(dir)).unwrap()
    }

    /// A workspace whose single repo is a real local git repository with one commit on `main`.
    fn git_workspace() -> (tempfile::TempDir, Workspace, PathBuf, String) {
        let (dir, mut ws) = workspace();
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["config", "user.email", "t@example.com"]);
        git(&source, &["config", "user.name", "t"]);
        std::fs::write(source.join("README.md"), "hi\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "init"]);
        git(&source, &["branch", "someone-elses"]);
        let project = &mut ws.config.projects[0];
        project.repos[0].source = source.clone();
        project.repos[0].base = "main".into();
        let template = dir.path().join("c/templates/acme/platform");
        std::fs::create_dir_all(&template).unwrap();
        std::fs::write(template.join(".env"), "PORT={{ ports.api }}\n").unwrap();
        let w = ws.plan(&index(), "T").unwrap();
        ws.prepare(&w.id, &index()).unwrap();
        (dir, ws, source, w.id)
    }

    #[test]
    fn worktree_lifecycle_only_deletes_branches_it_created() {
        let (_d, ws, source, id) = git_workspace();
        let mut log = |_: &str| {};
        let t = ws
            .add_worktree(&id, "platform", None, None, &mut log)
            .unwrap();
        assert_eq!(t.origin, BranchOrigin::New);
        let w = ws.state.get(&id).unwrap().unwrap();
        let wt = w.dir.join("platform");
        assert_eq!(git::current_branch(&wt).unwrap(), "feat/header-layout-327");
        let env = std::fs::read_to_string(wt.join(".env")).unwrap();
        assert_eq!(env, format!("PORT={}\n", w.ports["api"]));
        assert!(
            ws.add_worktree(&id, "platform", None, None, &mut log)
                .is_err()
        );

        ws.remove_worktree(&id, "platform", &mut log).unwrap();
        assert!(!wt.exists());
        assert!(!git::local_branch_exists(&source, "feat/header-layout-327"));

        let t = ws
            .add_worktree(&id, "platform", Some("someone-elses"), None, &mut log)
            .unwrap();
        assert_eq!(t.origin, BranchOrigin::Existing);
        ws.remove_worktree(&id, "platform", &mut log).unwrap();
        assert!(git::local_branch_exists(&source, "someone-elses"));
    }

    #[test]
    fn cleans_up_after_the_clone_is_deleted() {
        let (d, mut ws, _source, id) = git_workspace();
        ws.config.projects[0].stack = None;
        ws.add_worktree(&id, "platform", None, None, &mut |_| {})
            .unwrap();
        let dir = ws.state.get(&id).unwrap().unwrap().dir;
        ws.config.projects[0].repos[0].source = d.path().join("deleted-clone");

        ws.cleanup(&id, None, &mut |_| {}).unwrap();
        assert!(!dir.exists());
        assert!(ws.state.get(&id).unwrap().is_none());
    }

    #[test]
    fn cleans_up_after_the_worktree_folder_is_deleted() {
        let (_d, mut ws, source, id) = git_workspace();
        let project = &mut ws.config.projects[0];
        project.stack = None;
        project.hooks.teardown = toml::from_str::<crate::config::Hooks>(
            "[[teardown]]\ncwd = \"platform\"\nrun = \"exit 1\"",
        )
        .unwrap()
        .teardown;
        ws.add_worktree(&id, "platform", None, None, &mut |_| {})
            .unwrap();
        let dir = ws.state.get(&id).unwrap().unwrap().dir;
        std::fs::remove_dir_all(dir.join("platform")).unwrap();

        ws.cleanup(&id, None, &mut |_| {}).unwrap();
        assert!(!dir.exists());
        assert!(!git::local_branch_exists(&source, "feat/header-layout-327"));
    }

    #[test]
    fn adopts_a_worktree_made_with_plain_git() {
        let (_d, ws, source, id) = git_workspace();
        let w = ws.state.get(&id).unwrap().unwrap();
        let wt = w.dir.join("platform");
        git(
            &source,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "manual",
                &wt.to_string_lossy(),
                "main",
            ],
        );
        let t = ws
            .add_worktree(&id, "platform", None, None, &mut |_| {})
            .unwrap();
        assert_eq!(
            (t.branch.as_str(), t.origin),
            ("manual", BranchOrigin::Existing)
        );
        assert!(wt.join(".env").exists());
        ws.cleanup(&id, None, &mut |_| {}).ok();
        assert!(git::local_branch_exists(&source, "manual"));
    }

    #[test]
    fn starts_a_service_once_and_stops_it() {
        let (_d, mut ws) = workspace();
        #[derive(serde::Deserialize)]
        struct Services {
            services: Vec<crate::config::ServiceConfig>,
        }
        let idle = r#"
[[services]]
name = "idle"
run.windows = "Start-Sleep 60"
run.unix = "sleep 60"
"#;
        ws.config.projects[0].services = toml::from_str::<Services>(idle).unwrap().services;
        let w = ws.plan(&index(), "T").unwrap();
        let w = ws.prepare(&w.id, &IssueIndex::default()).unwrap();
        let mut log = |_: &str| {};

        let first = ws.start_service(&w.id, "idle", false, &mut log).unwrap();
        assert_eq!(
            first,
            [("idle".to_string(), crate::services::Outcome::Started)]
        );
        let again = ws.start_service(&w.id, "idle", false, &mut log).unwrap();
        assert_eq!(
            again,
            [("idle".to_string(), crate::services::Outcome::AlreadyRunning)]
        );
        let status = &ws.services(&w.id).unwrap()[0];
        assert!(status.running);
        let pid = status.pid.unwrap();

        ws.stop_services(&w.id, &mut log).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!process::process_exists(pid));
        assert!(!ws.services(&w.id).unwrap()[0].running);
        assert!(ws.stop_service(&w.id, "idle", &mut log).is_err());

        ws.start_service(&w.id, "idle", false, &mut log).unwrap();
        let running = crate::services::read(&ws.config.paths, &w.id)["idle"].clone();
        assert!(process::stop_tree(running.pid, &running.command, &mut log).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(ws.services(&w.id).unwrap()[0].pid, None);
        let err = ws.stop_service(&w.id, "idle", &mut log).unwrap_err();
        assert!(err.to_string().contains("no longer running"), "{err}");
        assert!(crate::services::read(&ws.config.paths, &w.id).is_empty());
        ws.start_service(&w.id, "idle", false, &mut log).unwrap();
        let running = crate::services::read(&ws.config.paths, &w.id)["idle"].clone();
        process::stop_tree(running.pid, &running.command, &mut log).unwrap();
        ws.stop_services(&w.id, &mut log).unwrap();
    }

    #[test]
    fn service_status_and_stop_go_by_pid_and_start_time() {
        let (_d, mut ws) = workspace();
        #[derive(serde::Deserialize)]
        struct Services {
            services: Vec<crate::config::ServiceConfig>,
        }
        let idle = r#"
[[services]]
name = "idle"
run.windows = "Start-Sleep 60"
run.unix = "sleep 60"
"#;
        ws.config.projects[0].services = toml::from_str::<Services>(idle).unwrap().services;
        let w = ws.plan(&index(), "T").unwrap();
        let w = ws.prepare(&w.id, &IssueIndex::default()).unwrap();
        let mut log = |_: &str| {};
        let paths = ws.config.paths.clone();
        let rewrite = |change: &dyn Fn(&mut crate::services::Running)| {
            let mut record = crate::services::read(&paths, &w.id);
            change(record.get_mut("idle").unwrap());
            crate::services::write(&paths, &w.id, &record).unwrap();
        };

        ws.start_service(&w.id, "idle", false, &mut log).unwrap();
        let pid = crate::services::read(&paths, &w.id)["idle"].pid;
        rewrite(&|r| r.command = "PORT=1 npm run dev".into());
        let status = &ws.services(&w.id).unwrap()[0];
        assert_eq!((status.running, status.pid), (true, Some(pid)));
        ws.stop_service(&w.id, "idle", &mut log).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!process::process_exists(pid));

        ws.start_service(&w.id, "idle", false, &mut log).unwrap();
        let running = crate::services::read(&paths, &w.id)["idle"].clone();
        let token = running.start_token.expect("a start token");
        rewrite(&|r| r.started = 1);
        let status = &ws.services(&w.id).unwrap()[0];
        assert_eq!(status.pid, Some(running.pid), "the token, not the clock");
        rewrite(&|r| r.start_token = None);
        let status = &ws.services(&w.id).unwrap()[0];
        assert_eq!(status.pid, None, "an old record goes by the clock");
        rewrite(&|r| {
            r.started = running.started;
            r.start_token = Some(token + 1);
        });
        let status = &ws.services(&w.id).unwrap()[0];
        assert_eq!((status.running, status.pid), (false, None), "pid reused");
        let err = ws.stop_service(&w.id, "idle", &mut log).unwrap_err();
        assert!(err.to_string().contains("no longer running"), "{err}");
        assert!(
            process::process_exists(running.pid),
            "a reused pid is left alone"
        );
        assert!(process::stop_tree(running.pid, &running.command, &mut log).unwrap());
    }

    /// A port held by a bound socket that isn't listening yet, so no other test takes it while
    /// connections to it are refused.
    struct ReservedPort(socket2::Socket);

    impl ReservedPort {
        fn new() -> Self {
            let socket =
                socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
            let any_port = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
            socket.bind(&any_port.into()).unwrap();
            Self(socket)
        }

        fn port(&self) -> u16 {
            self.0.local_addr().unwrap().as_socket().unwrap().port()
        }

        fn listen(self) -> std::net::TcpListener {
            self.0.listen(128).unwrap();
            self.0.into()
        }
    }

    /// `web` needs `api`, which runs `api_run`; `queue` has no port. Returns the reserved `api`
    /// and `web` ports.
    fn service_chain(
        ws: &mut Workspace,
        api_run: &str,
    ) -> (WorkerRecord, ReservedPort, ReservedPort) {
        #[derive(serde::Deserialize)]
        struct Services {
            services: Vec<crate::config::ServiceConfig>,
        }
        let sleep = sleep_command();
        let services = format!(
            r#"
[[services]]
name = "api"
port = "api"
ready_timeout_secs = 20
run = "{api_run}"
[[services]]
name = "web"
port = "web"
needs = ["api"]
ready_timeout_secs = 20
run = "{sleep}"
[[services]]
name = "queue"
run = "{sleep}"
"#
        );
        ws.config.projects[0].services = toml::from_str::<Services>(&services).unwrap().services;
        let w = ws.plan(&index(), "T").unwrap();
        let mut w = ws.prepare(&w.id, &IssueIndex::default()).unwrap();
        let (api, web) = (ReservedPort::new(), ReservedPort::new());
        w.ports = [
            ("api".to_string(), api.port()),
            ("web".to_string(), web.port()),
        ]
        .into();
        ws.state.save(&w).unwrap();
        (w, api, web)
    }

    /// Once `name` is in the services record, runs `meanwhile`, then listens on `port` and
    /// returns when it began listening, with the listener.
    fn listen_once_started(
        ws: &Workspace,
        w: &WorkerRecord,
        name: &'static str,
        port: ReservedPort,
        meanwhile: impl FnOnce(&Workspace) + Send + 'static,
    ) -> std::thread::JoinHandle<(Instant, std::net::TcpListener)> {
        let (config, id) = (ws.config.clone(), w.id.clone());
        std::thread::spawn(move || {
            let other = Workspace {
                state: State::open(&config.paths.state_db()).unwrap(),
                config,
                claude_home: None,
            };
            let deadline = Instant::now() + Duration::from_secs(30);
            while !crate::services::read(&other.config.paths, &id).contains_key(name) {
                assert!(Instant::now() < deadline, "{name} never started");
                std::thread::sleep(Duration::from_millis(50));
            }
            meanwhile(&other);
            let listener = port.listen();
            (Instant::now(), listener)
        })
    }

    fn slow_to_time_out(ws: &mut Workspace) {
        for service in &mut ws.config.projects[0].services {
            service.ready_timeout_secs = 180;
        }
    }

    fn after(delay: Duration) -> impl FnOnce(&Workspace) + Send + 'static {
        move |_| std::thread::sleep(delay)
    }

    fn sleep_command() -> &'static str {
        if cfg!(windows) {
            "Start-Sleep 60"
        } else {
            "sleep 60"
        }
    }

    /// Starts `name`, waiting, and returns when `web` was spawned.
    fn start_and_time_web(ws: &Workspace, w: &WorkerRecord, name: &str) -> Instant {
        let mut web_spawned = None;
        ws.start_service(&w.id, name, true, &mut |line| {
            if line.starts_with("▸ service web:") {
                web_spawned = Some(Instant::now());
            }
        })
        .unwrap();
        web_spawned.expect("web was started")
    }

    #[test]
    fn waits_for_a_dependency_to_listen_before_starting_what_needs_it() {
        let (_d, mut ws) = workspace();
        let (w, api_port, web_port) = service_chain(&mut ws, sleep_command());
        let api = listen_once_started(&ws, &w, "api", api_port, after(Duration::from_millis(1500)));
        let web = listen_once_started(&ws, &w, "web", web_port, after(Duration::ZERO));

        let web_spawned = start_and_time_web(&ws, &w, "web");
        let (api_listening, _api) = api.join().unwrap();
        let _web = web.join().unwrap();
        assert!(web_spawned >= api_listening);
        ws.stop_services(&w.id, &mut |_| {}).unwrap();
    }

    #[test]
    fn a_dependency_that_exits_stops_the_chain() {
        let (_d, mut ws) = workspace();
        let (w, _api, _web) = service_chain(&mut ws, "exit 1");
        let err = ws
            .start_service(&w.id, "web", true, &mut |_| {})
            .unwrap_err();
        assert!(
            err.to_string().contains("'api' exited before listening"),
            "{err}"
        );
        assert!(!crate::services::read(&ws.config.paths, &w.id).contains_key("web"));
        ws.stop_services(&w.id, &mut |_| {}).unwrap();
    }

    #[test]
    fn a_live_service_not_yet_listening_is_starting() {
        let (_d, mut ws) = workspace();
        let (w, _api, _web) = service_chain(&mut ws, sleep_command());
        ws.start_service(&w.id, "api", false, &mut |_| {}).unwrap();
        let api = &ws.services(&w.id).unwrap()[0];
        assert_eq!(
            (api.name.as_str(), api.state, api.running),
            ("api", crate::services::ServiceState::Starting, false)
        );
        assert!(api.pid.is_some());
        ws.stop_services(&w.id, &mut |_| {}).unwrap();
    }

    #[test]
    fn waits_for_a_dependency_started_earlier_that_isnt_listening_yet() {
        let (_d, mut ws) = workspace();
        let (w, api_port, web_port) = service_chain(&mut ws, sleep_command());
        ws.start_service(&w.id, "api", false, &mut |_| {}).unwrap();
        let api = listen_once_started(&ws, &w, "api", api_port, after(Duration::from_millis(1500)));
        let web = listen_once_started(&ws, &w, "web", web_port, after(Duration::ZERO));

        let web_spawned = start_and_time_web(&ws, &w, "web");
        let (api_listening, _api) = api.join().unwrap();
        let _web = web.join().unwrap();
        assert!(web_spawned >= api_listening);
        ws.stop_services(&w.id, &mut |_| {}).unwrap();
    }

    #[test]
    fn a_stop_made_during_a_wait_is_not_undone() {
        let (_d, mut ws) = workspace();
        let (w, api_port, web_port) = service_chain(&mut ws, sleep_command());
        ws.start_service(&w.id, "queue", false, &mut |_| {})
            .unwrap();
        let id = w.id.clone();
        let api = listen_once_started(&ws, &w, "api", api_port, move |other| {
            other.stop_service(&id, "queue", &mut |_| {}).unwrap()
        });
        let web = listen_once_started(&ws, &w, "web", web_port, after(Duration::ZERO));

        ws.start_service(&w.id, "web", true, &mut |_| {}).unwrap();
        let (_, _api) = api.join().unwrap();
        let _web = web.join().unwrap();
        let record = crate::services::read(&ws.config.paths, &w.id);
        assert_eq!(record.keys().collect::<Vec<_>>(), ["api", "web"]);
        ws.stop_services(&w.id, &mut |_| {}).unwrap();
    }

    #[test]
    fn a_listening_service_does_not_wait_for_a_dependency_that_is_not() {
        let (_d, mut ws) = workspace();
        let (w, _api, web_port) = service_chain(&mut ws, sleep_command());
        slow_to_time_out(&mut ws);
        ws.start_service(&w.id, "api", false, &mut |_| {}).unwrap();
        let _web = web_port.listen();

        let began = Instant::now();
        let outcomes = ws.start_service(&w.id, "web", true, &mut |_| {}).unwrap();
        assert!(began.elapsed() < Duration::from_secs(60));
        assert_eq!(
            outcomes,
            [
                ("api".to_string(), crate::services::Outcome::AlreadyRunning),
                ("web".to_string(), crate::services::Outcome::AlreadyRunning)
            ]
        );
        ws.stop_services(&w.id, &mut |_| {}).unwrap();
    }

    #[test]
    fn a_service_starting_for_longer_than_its_timeout_fails_at_once() {
        let (_d, mut ws) = workspace();
        let (w, api_port, _web) = service_chain(&mut ws, sleep_command());
        slow_to_time_out(&mut ws);
        ws.start_service(&w.id, "api", false, &mut |_| {}).unwrap();
        let mut running = crate::services::read(&ws.config.paths, &w.id)["api"].clone();
        running.started -= 240;
        let api = &ws.config.projects[0].services[0];

        let began = Instant::now();
        let err = crate::services::wait_until_listening(
            &ws.config.paths,
            &w.id,
            api,
            api_port.port(),
            running,
        )
        .unwrap_err();
        assert!(began.elapsed() < Duration::from_secs(60));
        assert!(err.to_string().contains("not listening"), "{err}");
        ws.stop_services(&w.id, &mut |_| {}).unwrap();
    }

    /// `started` is in Unix seconds.
    fn record_own_process_as_api(
        ws: &Workspace,
        w: &WorkerRecord,
        started: u64,
        start_token: Option<u64>,
    ) {
        let running = crate::services::Running {
            pid: std::process::id(),
            command: "npm run dev".into(),
            log: ws.config.paths.data_dir.join("api.log"),
            started,
            start_token,
        };
        let record = [("api".to_string(), running)].into();
        crate::services::write(&ws.config.paths, &w.id, &record).unwrap();
    }

    fn own_start_token() -> u64 {
        let pid = std::process::id();
        crate::process::start_tokens(&[pid]).unwrap()[&pid].expect("our own start token")
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn a_recorded_pid_reused_by_a_later_process_is_started_again() {
        for (started, token) in [(0, None), (now_secs(), Some(own_start_token() + 1))] {
            let (_d, mut ws) = workspace();
            let (w, _api, _web) = service_chain(&mut ws, sleep_command());
            record_own_process_as_api(&ws, &w, started, token);

            let outcomes = ws.start_service(&w.id, "api", false, &mut |_| {}).unwrap();
            assert_eq!(
                outcomes,
                [("api".to_string(), crate::services::Outcome::Started)],
                "started {started}, token {token:?}"
            );
            let running = crate::services::read(&ws.config.paths, &w.id)["api"].clone();
            assert_ne!(running.pid, std::process::id());
            assert!(running.start_token.is_some());
            ws.stop_services(&w.id, &mut |_| {}).unwrap();
        }
    }

    #[test]
    fn a_recorded_service_whose_command_line_differs_is_not_started_again() {
        for (started, token) in [(now_secs(), None), (0, Some(own_start_token()))] {
            let (_d, mut ws) = workspace();
            let (w, _api, _web) = service_chain(&mut ws, sleep_command());
            record_own_process_as_api(&ws, &w, started, token);

            let outcomes = ws.start_service(&w.id, "api", false, &mut |_| {}).unwrap();
            assert_eq!(
                outcomes,
                [("api".to_string(), crate::services::Outcome::AlreadyRunning)],
                "started {started}, token {token:?}"
            );
            assert!(ws.services(&w.id).unwrap()[0].pid.is_some());
            ws.stop_services(&w.id, &mut |_| {}).unwrap();
        }
    }

    #[test]
    fn launch_gives_the_session_the_browser_mcp_server_when_asked() {
        let (_d, ws) = workspace();
        let w = ws.plan(&index(), "T").unwrap();
        let arg = |launch: &Launch, flag: &str| -> Option<Value> {
            let at = launch.args.iter().position(|a| a == flag)?;
            Some(serde_json::from_str(&launch.args[at + 1]).unwrap())
        };

        let plain = ws.launch(&w.id, None).unwrap();
        assert!(arg(&plain, "--mcp-config").is_none());
        let plain_permissions = &arg(&plain, "--settings").unwrap()["permissions"];
        let rules = |kind: &str| -> Vec<String> {
            plain_permissions[kind]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|r| r.as_str().map(str::to_owned))
                .collect()
        };
        let (ask, allow) = (rules("ask"), rules("allow"));
        for rule in ["PowerShell(gh pr review *)", "Bash(gh pr comment *)"] {
            assert!(ask.iter().any(|r| r == rule), "gh writes prompt: {ask:?}");
        }
        for rule in ["Bash(tw subtask add *)", "PowerShell(tw issue *)"] {
            assert!(
                allow.iter().any(|r| r == rule),
                "tw writes are allowed: {allow:?}"
            );
        }
        assert!(!allow.iter().any(|r| r.starts_with("mcp__")));

        let endpoint = McpEndpoint {
            url: "http://127.0.0.1:4100/mcp".into(),
            token: "t0k".into(),
        };
        let launch = ws.launch(&w.id, Some(&endpoint)).unwrap();
        let server = &arg(&launch, "--mcp-config").unwrap()["mcpServers"]["TworkyWeb"];
        assert_eq!(server["url"], "http://127.0.0.1:4100/mcp");
        assert_eq!(server["headers"]["Authorization"], "Bearer t0k");
        let permissions = &arg(&launch, "--settings").unwrap()["permissions"];
        assert!(
            permissions["allow"]
                .as_array()
                .unwrap()
                .contains(&Value::from("mcp__TworkyWeb"))
        );
        assert!(permissions["ask"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn launch_switches_to_resume_after_start() {
        let (_d, ws) = workspace();
        let w = ws.plan(&index(), "T").unwrap();
        let first = ws.launch(&w.id, None).unwrap();
        assert!(first.args.contains(&"--session-id".to_string()));
        assert_eq!(
            first.args.last().map(String::as_str),
            Some("auto"),
            "no initial prompt"
        );
        assert_eq!(first.env["TW_PORT_API"], w.ports["api"].to_string());
        assert_eq!(first.env["WA_PORT_API"], first.env["TW_PORT_API"]);
        assert_eq!(first.env["WA_WORKER_ID"], w.id);
        assert_eq!(first.env["TW_WORKER_ID"], w.id);
        assert!(!first.replaced_missing);

        ws.mark_claude_started(&w.id).unwrap();
        let unsaved = ws.launch(&w.id, None).unwrap();
        assert!(unsaved.replaced_missing, "no transcript, so no --resume");
        assert!(unsaved.args.contains(&"--session-id".to_string()));
        let fresh = ws.state.get(&w.id).unwrap().unwrap();
        assert_ne!(fresh.claude_session, w.claude_session);

        ws.mark_claude_started(&w.id).unwrap();
        let transcript = claude_config::transcript_path(
            ws.claude_home.as_ref().unwrap(),
            &unsaved.cwd,
            &fresh.claude_session,
        );
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let again = ws.launch(&w.id, None).unwrap();
        assert!(!again.replaced_missing);
        assert_eq!(
            again.args[2..],
            ["--resume".to_string(), fresh.claude_session.clone()]
        );

        let hooks = |launch: &Launch| -> Vec<String> {
            let at = launch.args.iter().position(|a| a == "--settings").unwrap();
            let settings: Value = serde_json::from_str(&launch.args[at + 1]).unwrap();
            let clear = &settings["hooks"]["SessionStart"][2];
            assert_eq!(clear["matcher"], "clear");
            assert!(
                clear["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains("tw session-state idle --new-conversation")
            );
            let start = &settings["hooks"]["SessionStart"][0];
            assert_eq!(start["matcher"], "startup|resume");
            start["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["command"].as_str().unwrap().to_string())
                .collect()
        };
        let running = hooks(&again);
        assert_eq!(running.len(), 2, "memory and the feedback note");
        let memory = ws.config.paths.memory_file("acme");
        assert!(running[0].contains(&memory.to_string_lossy().replace('\\', "/")));
        assert!(running[1].contains("tw issue bug|friction"));

        let mut stopped = ws.state.get(&w.id).unwrap().unwrap();
        stopped.stopped = true;
        ws.state.save(&stopped).unwrap();
        let resumed = hooks(&ws.launch(&w.id, None).unwrap());
        assert_eq!(resumed.len(), 3);
        assert!(resumed[2].contains("run tw worker setup"));
    }

    #[test]
    fn quotes_hook_arguments_for_bash() {
        assert_eq!(
            sh_quote("C:/Users/o'neil/m.md"),
            r"'C:/Users/o'\''neil/m.md'"
        );
    }

    #[test]
    fn stack_to_stop_needs_a_stack_or_containers() {
        let dir = tempfile::tempdir().unwrap();
        let at = |dir: &Path| Compose {
            dir: dir.to_path_buf(),
            ..Compose::by_name("tw-acme-1")
        };
        let container = || stack::ServiceStatus {
            service: "db".into(),
            state: "exited".into(),
            health: String::new(),
            publishers: None,
        };
        let lines = std::cell::RefCell::new(Vec::<String>::new());
        let mut log = |l: &str| lines.borrow_mut().push(l.to_string());

        let mut asked = false;
        let picked = stack_to_stop(
            Some(at(dir.path())),
            "tw-acme-1",
            |_| {
                asked = true;
                Ok(vec![])
            },
            &mut log,
        );
        assert_eq!(picked.unwrap().dir, dir.path());
        assert!(!asked);

        let picked = stack_to_stop(None, "tw-acme-1", |_| Ok(vec![container()]), &mut log);
        assert_eq!(picked.unwrap().project_name, "tw-acme-1");

        let mut asked = false;
        let missing = at(&dir.path().join("gone"));
        let picked = stack_to_stop(
            Some(missing),
            "tw-acme-1",
            |_| {
                asked = true;
                Err(anyhow::anyhow!("docker isn't running"))
            },
            &mut log,
        );
        let picked = picked.unwrap();
        assert!(picked.files.is_empty());
        assert_eq!(picked.project_name, "tw-acme-1");
        assert!(!asked);

        assert!(stack_to_stop(None, "tw-acme-1", |_| Ok(vec![]), &mut log).is_none());
        assert!(lines.borrow().is_empty());

        let failed = stack_to_stop(
            None,
            "tw-acme-1",
            |_| Err(anyhow::anyhow!("docker isn't running")),
            &mut log,
        );
        assert!(failed.is_none());
        let lines = lines.into_inner();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("tw-acme-1"));
    }
}
