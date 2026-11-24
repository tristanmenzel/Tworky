use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tworky_core::config::{Os, Paths};
use tworky_core::feedback;
use tworky_core::github;
use tworky_core::hierarchy::{IssueIndex, IssueNode};
use tworky_core::items::{AddOutcome, ProjectChoice};
use tworky_core::services::ServiceState;
use tworky_core::state::ItemSource;
use tworky_core::worker::Workspace;

#[derive(Parser)]
#[command(name = "tw", about = "Tworky command line", version = tworky_core::build_info::VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show open issues assigned to you, with their hierarchy
    Issues {
        #[arg(long)]
        json: bool,
    },
    /// Show branches and PRs linked to an issue and its sub-issues, with next steps
    Work {
        url: String,
        /// Fetch the project repos first
        #[arg(long)]
        fetch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show the work list: each issue with its parents and sub-issues, then PRs and custom items
    Cards,
    /// Show config and data locations
    Paths,
    #[command(subcommand)]
    Worker(WorkerCommand),
    #[command(subcommand)]
    Worktree(WorktreeCommand),
    /// The project's services (dev servers and the like), started on demand in a worker
    #[command(subcommand)]
    Service(ServiceCommand),
    /// The project's shared memory, read by every Claude working on it
    #[command(subcommand)]
    Memory(MemoryCommand),
    /// Test accounts a session's browser can sign in as. Passwords stay in the OS credential store.
    #[command(subcommand)]
    Accounts(AccountsCommand),
    #[command(subcommand)]
    Subtask(SubtaskCommand),
    /// Report a bug or friction point with Tworky itself as a GitHub issue
    #[command(subcommand)]
    Issue(IssueCommand),
    /// The work list: issues, pull requests and custom work
    #[command(subcommand)]
    Item(ItemCommand),
    /// Configured projects, and the checks a project setup session runs
    #[command(subcommand)]
    Project(ProjectCommand),
    /// The config repo, and its sync with a remote
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Record what this worker's Claude is doing (working, idle or waiting). Run by session hooks.
    SessionState {
        activity: tworky_core::activity::Activity,
        #[arg(long = "worker", short = 'w', env = "TW_WORKER_ID")]
        id: String,
        /// The user sent a prompt, which ends any handover and account requests.
        #[arg(long)]
        new_turn: bool,
        /// The session started a new conversation (`/clear`), which ends any handover and account
        /// requests.
        #[arg(long)]
        new_conversation: bool,
        /// The turn ended. Reads the hook's `background_tasks` from stdin.
        #[arg(long)]
        stop: bool,
        /// The session started or resumed, which ends any background work listed before.
        #[arg(long)]
        session_start: bool,
    },
    /// Record the Graft phase (researching, planning or building) a hook's input implies. Run by
    /// session hooks.
    SessionPhase {
        /// Read the hook's JSON input from stdin. Input that implies no phase changes nothing.
        #[arg(long)]
        from_hook: bool,
        #[arg(long = "worker", short = 'w', env = "TW_WORKER_ID")]
        id: String,
    },
}

#[derive(Subcommand)]
enum ItemCommand {
    Ls,
    /// Create a custom item in a configured project; it gets a random name
    New {
        #[arg(long, short)]
        project: String,
        #[arg(long, short)]
        title: String,
        #[arg(long, short, default_value = "")]
        description: String,
        /// A throwaway item for testing the project's setup, removed when its worker is cleaned up
        #[arg(long)]
        scratch: bool,
    },
    /// Find issues to add: your open assigned ones, or a title search, #number, owner/repo#number or URL
    Search {
        #[arg(default_value = "")]
        query: String,
    },
    /// Open pull requests waiting on your review
    Reviews,
    /// Put an issue on the work list
    AddIssue {
        url: String,
        /// Project to add the issue's repo to, when no project covers it yet
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Put a pull request on the work list; its worker works on the PR's branch
    AddPr {
        /// A pull request URL or owner/repo#number
        url: String,
        /// Project to add the PR's repo to, when no project covers it yet
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Create the item's worker (port slot, directory, CLAUDE.md) and print its id
    Begin {
        name: String,
    },
    /// Turn a custom item into an issue item: its worker becomes that issue's worker
    Link {
        name: String,
        issue_url: String,
    },
    /// Take an item off the work list, once its worker has been cleaned up
    Rm {
        name: String,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    Ls,
    /// Show where a project's config, templates, memory and setup session live
    Show(NamedProject),
    /// Try the project's config on this machine: clones, templates, hooks and services
    Check(NamedProject),
    /// Add a repo (owner/repo) to the project, cloning it under the first source root if needed
    AddRepo {
        repo: String,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Mark a new project as set up, once its config has been tested on a scratch item
    Done(NamedProject),
    /// Record where a repo of the project is cloned on this machine, in machine.toml
    SetSource {
        /// The repo's name in [[repos]]
        #[arg(required_unless_present = "scan")]
        repo: Option<String>,
        /// Its clone on this machine
        #[arg(required_unless_present = "scan")]
        path: Option<std::path::PathBuf>,
        /// Instead, look under the source roots for clones of repos with no source, in PROJECT or
        /// every project
        #[arg(long, value_name = "PROJECT", num_args = 0..=1, conflicts_with_all = ["repo", "path"])]
        scan: Option<Option<String>>,
        #[command(flatten)]
        project: ProjectArg,
    },
}

/// A project named as an argument, or by `--project` and its defaults.
#[derive(Args)]
struct NamedProject {
    /// Project name (defaults to --project, $TW_PROJECT, or the project of $TW_WORKER_ID)
    #[arg(value_name = "PROJECT")]
    name: Option<String>,
    #[command(flatten)]
    project: ProjectArg,
}

impl NamedProject {
    fn into_arg(self) -> ProjectArg {
        ProjectArg {
            project: self.name.or(self.project.project),
            worker: self.project.worker,
        }
    }
}

#[derive(Args)]
struct ProjectArg {
    /// Project name (defaults to $TW_PROJECT, or the project of $TW_WORKER_ID)
    #[arg(long, short, env = "TW_PROJECT")]
    project: Option<String>,
    #[arg(long = "worker", short = 'w', env = "TW_WORKER_ID", hide = true)]
    worker: Option<String>,
}

#[derive(Subcommand)]
enum AccountsCommand {
    Ls(ProjectArg),
    /// Add an account, or replace the one with the same name
    Add {
        name: String,
        #[arg(long, default_value = "")]
        username: String,
        /// Written with the base ports; the worker's ports replace them when a session opens it
        #[arg(
            long,
            required_unless_present = "start_service",
            conflicts_with = "start_service"
        )]
        start_url: Option<String>,
        /// A service whose URL on the worker is the start page, in place of --start-url
        #[arg(long)]
        start_service: Option<String>,
        #[arg(long)]
        mobile: bool,
        /// Identity provider origin where the password may be filled, or service:<name>
        /// (repeatable)
        #[arg(long = "login-origin")]
        login_origins: Vec<String>,
        #[arg(long)]
        color: Option<String>,
        #[command(flatten)]
        project: ProjectArg,
    },
    Rm {
        name: String,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Store an account's password, read from stdin
    SetPassword {
        name: String,
        #[command(flatten)]
        project: ProjectArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum OsName {
    Windows,
    Macos,
    Linux,
}

impl From<OsName> for Os {
    fn from(os: OsName) -> Self {
        match os {
            OsName::Windows => Os::Windows,
            OsName::Macos => Os::Macos,
            OsName::Linux => Os::Linux,
        }
    }
}

#[derive(Subcommand)]
enum MemoryCommand {
    Show {
        /// Show that OS's memory instead of the shared one
        #[arg(long, value_enum)]
        os: Option<OsName>,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Propose a fact, attributed to this session and dated. It lands once approved in Tworky.
    Add {
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Heading to file it under
        #[arg(long, short, default_value = tworky_core::memory::DEFAULT_SECTION)]
        section: String,
        /// Target that OS's memory instead of the shared one
        #[arg(long, value_enum)]
        os: Option<OsName>,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Propose replacing text that occurs exactly once. An edited entry is restamped.
    /// It lands once approved in Tworky.
    #[command(group = clap::ArgGroup::new("replacement").required(true).args(["new", "new_file"]))]
    Edit {
        /// Text to replace; must occur exactly once in the memory
        #[arg(long, allow_hyphen_values = true)]
        old: String,
        /// Replacement text
        #[arg(long, allow_hyphen_values = true)]
        new: Option<String>,
        /// Read the replacement from a file, or from stdin with `-`
        #[arg(long)]
        new_file: Option<String>,
        /// Target that OS's memory instead of the shared one
        #[arg(long, value_enum)]
        os: Option<OsName>,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Propose deleting the line holding this text, which must occur exactly once.
    /// It lands once approved in Tworky.
    Remove {
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Target that OS's memory instead of the shared one
        #[arg(long, value_enum)]
        os: Option<OsName>,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// List the changes waiting for approval in Tworky
    Changes(ProjectArg),
    Path(ProjectArg),
}

#[derive(Subcommand)]
enum SubtaskCommand {
    /// Create a sub-issue of the project's subtask type under the worker's issue (or --parent)
    Add {
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Assign the new subtask to you
        #[arg(long)]
        assign_me: bool,
        /// Parent issue URL (defaults to the worker's issue)
        #[arg(long)]
        parent: Option<String>,
        /// Show what would be created without creating it
        #[arg(long)]
        dry_run: bool,
        #[arg(long = "worker", short = 'w', env = "TW_WORKER_ID", hide = true)]
        worker: Option<String>,
    },
}

#[derive(Args)]
struct BodyArgs {
    /// Issue body text
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,
    /// Read the body from a file, or from stdin with `-`
    #[arg(long)]
    body_file: Option<String>,
}

#[derive(Args)]
struct NewIssueArgs {
    title: String,
    #[command(flatten)]
    body: BodyArgs,
    /// Show what would be filed without calling GitHub
    #[arg(long)]
    dry_run: bool,
    /// File even if similar open issues exist
    #[arg(long)]
    force: bool,
}

#[derive(Subcommand)]
enum IssueCommand {
    /// Report a bug in Tworky or `tw`. Only after the user agrees.
    Bug(NewIssueArgs),
    /// Report friction in Tworky or `tw`. Only after the user agrees.
    Friction(NewIssueArgs),
    /// Add a comment to an existing Tworky issue
    Comment {
        number: u64,
        #[command(flatten)]
        body: BodyArgs,
        /// Show what would be posted without calling GitHub
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args)]
struct WorkerArg {
    /// Worker id (defaults to $TW_WORKER_ID, set inside Tworky sessions)
    #[arg(long = "worker", short = 'w', env = "TW_WORKER_ID")]
    id: String,
}

#[derive(Subcommand)]
enum WorkerCommand {
    /// Create a worker (port slot, directory, CLAUDE.md) for an issue URL
    Create {
        url: String,
    },
    Ls,
    /// Show one worker: ports, stack, worktrees
    Show(WorkerArg),
    /// Start the stack and run setup hooks (re-runnable)
    Setup(WorkerArg),
    /// Stop everything running from the worker folder and its containers, keeping them for the next setup
    Stop(WorkerArg),
    /// Tear down a worker: stack, worktrees, branches Tworky created, directory
    Rm(WorkerArg),
    Prs(WorkerArg),
    /// List dev servers, watchers and test runs still running in workers
    Jobs {
        /// Only this worker (defaults to every worker)
        #[arg(long = "worker", short = 'w')]
        id: Option<String>,
    },
    /// Stop one job and everything under it
    StopJob {
        #[command(flatten)]
        worker: WorkerArg,
        pid: u32,
    },
    /// Print the claude command a session would run
    Launch(WorkerArg),
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// List the worker's services and whether each is running
    Ls(WorkerArg),
    /// Start a service, and the services it needs, unless already running; waits for each to listen
    Start {
        name: String,
        #[command(flatten)]
        worker: WorkerArg,
        /// Start them all without waiting for any port
        #[arg(long)]
        no_wait: bool,
    },
    /// Stop a service Tworky started
    Stop {
        name: String,
        #[command(flatten)]
        worker: WorkerArg,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Show the config repo's state and its sync with the remote
    Status,
    /// Sync with the remote now, resuming a sync that stopped once it's fixed in the sync worktree
    Sync,
    /// Abort the rebase or merge a stopped sync left in the sync worktree. Background syncs then
    /// wait for the next `tw config sync`, which starts over
    Abort,
    #[command(subcommand)]
    Remote(RemoteCommand),
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Sync with this remote. A config repo with no history of its own takes the remote's.
    Set { url: String },
    /// Stop syncing and forget the remote
    Rm,
}

#[derive(Subcommand)]
enum WorktreeCommand {
    /// Add a worktree for a project repo and render its worker config
    Add {
        repo: String,
        /// Branch to check out (existing local/remote) or create; defaults to the worker's suggested branch
        #[arg(long, short)]
        branch: Option<String>,
        /// Ref to create a new branch from; defaults to the repo's configured base
        #[arg(long)]
        base: Option<String>,
        #[command(flatten)]
        worker: WorkerArg,
    },
    Ls(WorkerArg),
    /// Remove a worktree (and its branch, if Tworky created it)
    Rm {
        repo: String,
        #[command(flatten)]
        worker: WorkerArg,
    },
}

fn main() -> Result<()> {
    if std::env::var_os("TW_WORKER_ID").is_none()
        && let Some(id) = tworky_core::worker::worker_id_from_env()
    {
        // SAFETY: no other threads exist yet.
        unsafe { std::env::set_var("TW_WORKER_ID", id) };
    }
    let paths = Paths::resolve()?;
    for line in paths.migrate_legacy()? {
        eprintln!("{line}");
    }
    let command = Cli::parse().command;
    if !matches!(
        command,
        Command::SessionState { .. } | Command::SessionPhase { .. }
    ) {
        match paths.migrate_to_repo() {
            Ok(lines) => lines.iter().for_each(|line| eprintln!("{line}")),
            Err(e) => eprintln!("{e:#}"),
        }
    }
    match command {
        Command::Issues { json } => issues(json),
        Command::Paths => {
            let p = Paths::resolve()?;
            println!("config:    {}", p.app_config().display());
            println!("repo:      {}", p.repo_dir().display());
            println!("machine:   {}", p.machine_file().display());
            println!("projects:  {}", p.project_dir("<project>").display());
            println!("templates: {}", p.templates_dir().display());
            println!("state:     {}", p.state_db().display());
            println!("memory:    {}", p.memory_file("<project>", None).display());
            Ok(())
        }
        Command::Work { url, fetch, json } => work(&url, fetch, json),
        Command::Cards => cards(),
        Command::SessionState {
            activity,
            id,
            new_turn,
            new_conversation,
            stop,
            session_start,
        } => {
            let flags = SessionFlags {
                new_turn,
                new_conversation,
                stop,
                session_start,
            };
            let change = session_change(activity, flags, hook_input().as_ref());
            apply_session_change(&Paths::resolve()?, &id, change)
        }
        Command::SessionPhase { from_hook, id } => {
            anyhow::ensure!(from_hook, "pass --from-hook with the hook's input on stdin");
            let Ok(input) = std::io::read_to_string(std::io::stdin()) else {
                return Ok(());
            };
            let Ok(input) = serde_json::from_str(&input) else {
                return Ok(());
            };
            match tworky_core::activity::phase_from_hook(&input) {
                Some(phase) => tworky_core::activity::set_phase(&Paths::resolve()?, &id, phase),
                None => Ok(()),
            }
        }
        Command::Worker(cmd) => worker(cmd),
        Command::Worktree(cmd) => worktree(cmd),
        Command::Service(cmd) => service(cmd),
        Command::Memory(cmd) => memory(cmd),
        Command::Accounts(cmd) => accounts(cmd),
        Command::Subtask(cmd) => subtask(cmd),
        Command::Issue(cmd) => issue(cmd),
        Command::Item(cmd) => item(cmd),
        Command::Project(cmd) => project(cmd),
        Command::Config(cmd) => config(&paths, cmd),
    }
}

/// A terminal on stdin means a person ran the command, and reading it would block.
fn hook_input() -> Option<serde_json::Value> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut text = String::new();
    stdin.read_to_string(&mut text).ok()?;
    parse_hook_input(&text)
}

fn parse_hook_input(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok()
}

#[derive(Clone, Copy, Default)]
struct SessionFlags {
    new_turn: bool,
    new_conversation: bool,
    stop: bool,
    session_start: bool,
}

#[derive(Debug, PartialEq)]
enum SessionChange {
    Record(tworky_core::activity::Activity),
    NewTurn,
    BackgroundReport,
    NewConversation(tworky_core::activity::Activity),
    Stop(Option<Vec<tworky_core::activity::BackgroundTask>>),
    SessionStart,
    Touch,
}

fn session_change(
    activity: tworky_core::activity::Activity,
    flags: SessionFlags,
    input: Option<&serde_json::Value>,
) -> SessionChange {
    use tworky_core::activity::{self, Activity};
    let field = |name: &str| input.and_then(|i| i.get(name)).and_then(|v| v.as_str());
    if flags.new_turn {
        return if field("prompt").is_some_and(|p| p.trim_start().starts_with("<task-notification>"))
        {
            SessionChange::BackgroundReport
        } else {
            SessionChange::NewTurn
        };
    }
    if flags.stop {
        return SessionChange::Stop(input.and_then(activity::background_from_hook));
    }
    if flags.session_start {
        return SessionChange::SessionStart;
    }
    if flags.new_conversation {
        return SessionChange::NewConversation(activity);
    }
    if activity == Activity::Working && field("agent_id").is_some_and(|id| !id.is_empty()) {
        return SessionChange::Touch;
    }
    SessionChange::Record(activity)
}

fn apply_session_change(paths: &Paths, id: &str, change: SessionChange) -> Result<()> {
    use tworky_core::activity;
    match change {
        SessionChange::Record(a) => activity::record(paths, id, a),
        SessionChange::NewTurn => activity::new_turn(paths, id),
        SessionChange::BackgroundReport => activity::background_report(paths, id),
        SessionChange::NewConversation(a) => {
            activity::record(paths, id, a)?;
            activity::new_conversation(paths, id)
        }
        SessionChange::Stop(background) => activity::record_stop(paths, id, background),
        SessionChange::SessionStart => activity::session_start(paths, id),
        SessionChange::Touch => activity::touch(paths, id),
    }
}

fn config(paths: &Paths, cmd: ConfigCommand) -> Result<()> {
    use tworky_core::config_repo::{self, Trigger};
    let status = matches!(cmd, ConfigCommand::Status);
    let record = match cmd {
        ConfigCommand::Status => {
            let status = config_repo::status(paths)?;
            println!("repo:    {}", paths.repo_dir().display());
            println!(
                "remote:  {}",
                config_repo::remote_url(paths).unwrap_or_else(|| "none".into())
            );
            if let Some((ahead, behind)) = status.ahead_behind {
                println!("main:    {ahead} ahead, {behind} behind the remote");
            }
            if !status.changes.is_empty() {
                println!("uncommitted: {}", status.changes.join(", "));
            }
            if status.stale_index_lock {
                println!(
                    "warning: a git command that was stopped left an index.lock behind in the config repo or its sync worktree; delete it if no git is running there"
                );
            }
            status.sync
        }
        ConfigCommand::Sync => config_repo::sync(paths, Trigger::User)?,
        ConfigCommand::Abort => config_repo::abort(paths)?,
        ConfigCommand::Remote(RemoteCommand::Set { url }) => config_repo::set_remote(paths, &url)?,
        ConfigCommand::Remote(RemoteCommand::Rm) => config_repo::remove_remote(paths)?,
    };
    let stopped = print_sync(paths, &record);
    anyhow::ensure!(status || !stopped, "the config sync is stopped");
    Ok(())
}

/// Prints the sync state and what came in. Returns whether the sync is stopped.
fn print_sync(paths: &Paths, record: &tworky_core::config_repo::SyncRecord) -> bool {
    use tworky_core::config_repo::{HeldBackCause, SyncState};
    let files = |files: &[String]| files.join(", ");
    let worktree = paths.repo_sync_dir();
    let (line, stopped) = match &record.state {
        SyncState::Idle => ("idle".to_string(), false),
        SyncState::Syncing => ("syncing".to_string(), false),
        SyncState::NoRemote => ("no remote".to_string(), false),
        SyncState::Conflict { files: f } if record.paused => (
            format!(
                "paused after a conflict in {}; `tw config sync` starts over",
                files(f)
            ),
            true,
        ),
        SyncState::Conflict { files: f } => (
            format!(
                "conflict in {}; resolve it in {} and run `tw config sync`",
                files(f),
                worktree.display()
            ),
            true,
        ),
        SyncState::HeldBack {
            files: f,
            cause: HeldBackCause::Unrecognised,
        } => (
            format!(
                "held back: the remote's {} are valid TOML but not project config this tw understands, most likely written by a newer Tworky; update Tworky, or fix and commit them in {} if they're wrong, then run `tw config sync`",
                files(f),
                worktree.display()
            ),
            true,
        ),
        SyncState::HeldBack { files: f, .. } => (
            format!(
                "held back: the remote's {} don't parse; fix and commit them in {}, then run `tw config sync`",
                files(f),
                worktree.display()
            ),
            true,
        ),
        SyncState::BlockedByLocal { files: f } => (
            format!(
                "blocked: uncommitted local {} would be overwritten by incoming changes; fix or remove them",
                files(f)
            ),
            true,
        ),
        SyncState::HeldInSyncWorktree { files: f } => (
            format!(
                "held: {} in {} have uncommitted changes; commit them to carry on, or run `tw config abort` and `tw config sync` to discard them",
                files(f),
                worktree.display()
            ),
            true,
        ),
        SyncState::Error { message } => (format!("failed: {message}"), true),
    };
    println!("sync:    {line}");
    if let Some(secs) = record.last_sync {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(secs);
        println!("last:    {} min ago", now.saturating_sub(secs) / 60);
    }
    if !record.incoming.hooks_or_services.is_empty() {
        println!(
            "incoming hooks or services in: {}",
            record.incoming.hooks_or_services.join(", ")
        );
    }
    if !record.incoming.guidance.is_empty() {
        println!(
            "incoming guidance or memory: {}",
            record.incoming.guidance.join(", ")
        );
    }
    if !record.incoming.dropped.is_empty() {
        println!(
            "warning: resolving a conflict left out this machine's commits: {}",
            record.incoming.dropped.join("; ")
        );
    }
    stopped
}

fn project(cmd: ProjectCommand) -> Result<()> {
    use tworky_core::project_setup::{self, Level};
    let ws = Workspace::open()?;
    let named = |arg: &ProjectArg| -> Result<tworky_core::config::ProjectConfig> {
        let name = project_name(&ws, arg)?;
        if let Some(error) = ws.config.project_errors.get(&name) {
            anyhow::bail!("{name} doesn't load: {error}");
        }
        ws.config
            .project(&name)
            .cloned()
            .with_context(|| format!("no project called {name}"))
    };
    if matches!(cmd, ProjectCommand::Ls | ProjectCommand::Check(_)) {
        report_config_problems(&ws.config);
    }
    match cmd {
        ProjectCommand::Ls => {
            for p in &ws.config.projects {
                let repos: Vec<&str> = p.repos.iter().map(|r| r.name.as_str()).collect();
                let state = if p.onboarding {
                    " (not set up yet)"
                } else {
                    ""
                };
                println!("{:<16} {}{state}", p.name, repos.join(", "));
            }
            for (folder, error) in &ws.config.project_errors {
                println!("{folder:<16} doesn't load: {error}");
            }
        }
        ProjectCommand::Show(arg) => {
            let arg = arg.into_arg();
            let p = named(&arg)?;
            for (label, value) in project_setup::describe(&ws.config, &p) {
                println!("{label:<14} {value}");
            }
            for r in &p.repos {
                let at = match &r.source {
                    Some(source) => format!("at {}", source.display()),
                    None => "not cloned".into(),
                };
                let github = r.repo.as_deref().unwrap_or("no repo");
                println!(
                    "repo           {} ({github}) {at} (base {})",
                    r.name, r.base
                );
            }
        }
        ProjectCommand::Check(arg) => {
            let arg = arg.into_arg();
            let p = named(&arg)?;
            let reloaded;
            let ws = if report_found_sources(&ws, Some(&p.name))? {
                reloaded = Workspace::open()?;
                &reloaded
            } else {
                &ws
            };
            let findings = ws.check_project(&p.name)?;
            for f in &findings {
                let tag = match f.level {
                    Level::Error => "error",
                    Level::Warning => "warning",
                    Level::Info => "note",
                };
                println!("{tag}: {}", f.text);
            }
            if project_setup::has_errors(&findings) {
                anyhow::bail!("{} has config errors", p.name);
            }
            println!("✓ {} loads, and its templates and hooks render", p.name);
        }
        ProjectCommand::AddRepo { repo, project } => {
            let p = named(&project)?;
            ws.add_repo_to_project(&p.name, &repo)?;
            println!("added {repo} to {}", p.name);
        }
        ProjectCommand::SetSource {
            repo,
            path,
            scan,
            project,
        } => match (scan, repo, path) {
            (Some(only), _, _) => {
                if let Some(name) = &only
                    && ws.config.project(name).is_none()
                {
                    anyhow::bail!("no project called {name}");
                }
                if !report_found_sources(&ws, only.as_deref())? {
                    println!("found no new clones under the source roots");
                }
            }
            (None, Some(repo), Some(path)) => {
                let p = named(&project)?;
                let entry = p
                    .repos
                    .iter()
                    .find(|r| r.name.eq_ignore_ascii_case(&repo))
                    .with_context(|| format!("{} has no repo called {repo}", p.name))?;
                let path = std::path::absolute(&path)?;
                if !path.is_dir() {
                    anyhow::bail!("{} isn't a folder", path.display());
                }
                if let Some(nwo) = &entry.repo {
                    match tworky_core::git::origin_name_with_owner(&path) {
                        Ok(origin) if origin.eq_ignore_ascii_case(nwo) => {}
                        Ok(origin) => {
                            eprintln!(
                                "warning: {} is a clone of {origin}, not {nwo}",
                                path.display()
                            )
                        }
                        Err(e) => eprintln!("warning: {e:#}"),
                    }
                }
                tworky_core::config::set_source(&ws.config.paths, &p.name, &entry.name, &path)?;
                println!("{} of {} is at {}", entry.name, p.name, path.display());
            }
            _ => anyhow::bail!("give a repo and its path, or --scan"),
        },
        ProjectCommand::Done(arg) => {
            let arg = arg.into_arg();
            let p = named(&arg)?;
            ws.finish_onboarding(&p.name)?;
            println!("{} is set up", p.name);
        }
    }
    Ok(())
}

/// Prints what's wrong with the config as a whole: load errors, and a config repo whose commits are
/// stuck or behind.
fn report_config_problems(config: &tworky_core::config::Config) {
    for error in &config.load_errors {
        println!("error: {error}");
    }
    if !config.paths.has_repo() {
        return;
    }
    match tworky_core::config_repo::status(&config.paths) {
        Ok(status) => {
            if status.stale_index_lock {
                println!(
                    "error: {} is left from a git command that was stopped, and stops every commit to the config repo; delete it if no git is running there",
                    config
                        .paths
                        .repo_dir()
                        .join(".git")
                        .join("index.lock")
                        .display()
                );
            }
            if !status.changes.is_empty() {
                println!(
                    "warning: uncommitted changes in the config repo: {} (Tworky commits them on its next sweep, unless a project.toml doesn't parse)",
                    status.changes.join(", ")
                );
            }
        }
        Err(e) => println!("error: couldn't read the config repo's status: {e:#}"),
    }
}

/// Looks for clones of repos with no source, prints each one found and records it. Returns whether
/// it found any.
fn report_found_sources(ws: &Workspace, project: Option<&str>) -> Result<bool> {
    let found = tworky_core::config::find_sources(&ws.config, project)?;
    for f in &found {
        println!(
            "found {} of {} at {}",
            f.repo,
            f.project,
            f.source.display()
        );
    }
    Ok(!found.is_empty())
}

fn item(cmd: ItemCommand) -> Result<()> {
    let ws = Workspace::open()?;
    match cmd {
        ItemCommand::Ls => {
            for i in ws.state.list_items()? {
                let worker = ws
                    .worker_for_item(&i.name)?
                    .map(|w| format!(" [{}]", w.id))
                    .unwrap_or_default();
                let subject = match &i.source {
                    ItemSource::Custom if i.scratch => "scratch".to_string(),
                    ItemSource::Custom => "custom".to_string(),
                    ItemSource::Issue(r) => format!("{}#{}", r.repo, r.number),
                    ItemSource::PullRequest(p) => format!("{} PR #{}", p.repo, p.number),
                };
                println!(
                    "{:<22} {:<10} {:<28} {}{worker}",
                    i.name, i.project, subject, i.title
                );
            }
        }
        ItemCommand::New {
            project,
            title,
            description,
            scratch,
        } => {
            let i = ws.create_item(&project, &title, &description, scratch)?;
            println!("{}", i.name);
            eprintln!("item {} in {}", i.name, i.project);
        }
        ItemCommand::Search { query } => {
            for l in ws
                .search_issues(&query)?
                .into_iter()
                .filter(|l| l.hit.matched)
            {
                let listed = l
                    .item
                    .map(|i| format!(" [listed as {i}]"))
                    .unwrap_or_default();
                println!(
                    "{}#{:<6} {:<8} {:<7} {}{listed}
    {}",
                    l.hit.repo,
                    l.hit.number,
                    l.hit.issue_type.as_deref().unwrap_or("Issue"),
                    l.hit.state.to_lowercase(),
                    l.hit.title,
                    l.hit.url
                );
            }
        }
        ItemCommand::Reviews => {
            for l in ws.review_requests()? {
                let listed = l
                    .item
                    .map(|i| format!(" [listed as {i}]"))
                    .unwrap_or_default();
                println!(
                    "{} PR #{} by {} ({}){listed}: {}
    {}",
                    l.hit.repo, l.hit.number, l.hit.author, l.hit.head, l.hit.title, l.hit.url
                );
            }
        }
        ItemCommand::AddIssue { url, project } => {
            print_added(ws.add_issue_item(&url, &project_choice(project))?)?
        }
        ItemCommand::AddPr { url, project } => {
            print_added(ws.add_pull_request_item(&url, &project_choice(project))?)?
        }
        ItemCommand::Begin { name } => {
            let index = match ws.state.get_item(&name)?.and_then(|i| i.issue().cloned()) {
                Some(issue) => github::issue_by_url(&issue.url, &|i| is_subtask(Some(&ws), i))?.0,
                None => IssueIndex::default(),
            };
            let planned = ws.plan_item(&name, &index)?;
            let w = ws.prepare(&planned.id, &index)?;
            println!("{}", w.id);
            eprintln!("worker {} (slot {}) at {}", w.id, w.slot, w.dir.display());
        }
        ItemCommand::Link { name, issue_url } => {
            match ws.link_item(&name, &issue_url, &|i| is_subtask(Some(&ws), i))? {
                Some(w) => println!(
                    "{name} now stands for {}; worker {} moved with it",
                    issue_url, w.id
                ),
                None => println!("{name} now stands for {issue_url}"),
            }
        }
        ItemCommand::Rm { name } => {
            ws.remove_item(&name)?;
            println!("removed {name}");
        }
    }
    Ok(())
}

fn project_choice(project: Option<String>) -> ProjectChoice {
    match project {
        Some(name) => ProjectChoice::Existing { name },
        None => ProjectChoice::Infer,
    }
}

fn print_added(outcome: AddOutcome) -> Result<()> {
    match outcome {
        AddOutcome::Added { item, .. } => {
            println!("{}", item.name);
            eprintln!("added {} to {}", item.name, item.project);
            Ok(())
        }
        AddOutcome::NeedsProject { repo } => anyhow::bail!(
            "no project covers {repo}; pass --project <name> to add it to one (tw project ls)"
        ),
    }
}

fn project_name(ws: &Workspace, arg: &ProjectArg) -> Result<String> {
    if let Some(p) = &arg.project {
        return Ok(p.clone());
    }
    let id = arg
        .worker
        .as_deref()
        .context("pass --project, or run inside a Tworky session")?;
    Ok(ws.state.get(id)?.context("worker not found")?.project)
}

fn accounts(cmd: AccountsCommand) -> Result<()> {
    use tworky_core::accounts;
    use tworky_core::config::AccountConfig;
    let ws = Workspace::open()?;
    let project = |arg: &ProjectArg| -> Result<&tworky_core::config::ProjectConfig> {
        let name = project_name(&ws, arg)?;
        ws.config
            .project(&name)
            .with_context(|| format!("no project called {name}"))
    };
    match cmd {
        AccountsCommand::Ls(arg) => {
            let p = project(&arg)?;
            for a in &p.accounts {
                let stored = if accounts::has_password(&p.name, &a.name) {
                    "password stored"
                } else {
                    "no password"
                };
                let mobile = if a.mobile { " (mobile)" } else { "" };
                let start = match &a.start_service {
                    Some(service) => format!("service {service}"),
                    None => a.start_url.clone(),
                };
                println!(
                    "{:<16} {:<40} {start}{mobile}  [{stored}]",
                    a.name, a.username
                );
            }
        }
        AccountsCommand::Add {
            name,
            username,
            start_url,
            start_service,
            mobile,
            login_origins,
            color,
            project: arg,
        } => {
            let p = project(&arg)?;
            let previous = p
                .accounts
                .iter()
                .any(|a| a.name == name)
                .then_some(name.as_str());
            let account = AccountConfig {
                name: name.clone(),
                username,
                start_url: start_url.unwrap_or_default(),
                start_service,
                mobile,
                login_origins,
                color,
            };
            accounts::save_account(&ws.config.paths, p, previous, &account)?;
            println!("saved {name} in {}", p.file.display());
        }
        AccountsCommand::Rm { name, project: arg } => {
            let p = project(&arg)?;
            if !p.accounts.iter().any(|a| a.name == name) {
                anyhow::bail!("{} has no account called {name}", p.name);
            }
            accounts::remove_account(&ws.config.paths, p, &name)?;
            println!("removed {name}");
        }
        AccountsCommand::SetPassword { name, project: arg } => {
            let p = project(&arg)?;
            if !p.accounts.iter().any(|a| a.name == name) {
                anyhow::bail!("{} has no account called {name}", p.name);
            }
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let password = line.trim_end_matches(['\r', '\n']);
            if password.is_empty() {
                anyhow::bail!("no password on stdin");
            }
            accounts::set_password(&p.name, &name, password)?;
            println!("stored the password for {name}");
        }
    }
    Ok(())
}

fn memory(cmd: MemoryCommand) -> Result<()> {
    use tworky_core::memory;
    let ws = Workspace::open()?;
    match cmd {
        MemoryCommand::Show { os, project } => {
            let path = ws
                .config
                .paths
                .memory_file(&project_name(&ws, &project)?, os.map(Os::from));
            let text = memory::read(&path)?;
            if text.is_empty() {
                eprintln!("{} is empty", path.display());
            }
            print!("{text}");
            if text.len() > memory::SIZE_WARNING {
                eprintln!(
                    "warning: {} is over {} KB; sessions may see it truncated. Trim it in Tworky's Memory tab.",
                    path.display(),
                    memory::SIZE_WARNING / 1024
                );
            }
        }
        MemoryCommand::Add {
            text,
            section,
            os,
            project,
        } => propose(&ws, &project, os, memory::ChangeKind::Add { section, text })?,
        MemoryCommand::Edit {
            old,
            new,
            new_file,
            os,
            project,
        } => {
            let new = match (new, new_file) {
                (Some(new), _) => new,
                (None, Some(path)) => read_text_file(&path)?,
                (None, None) => unreachable!("clap requires --new or --new-file"),
            };
            propose(&ws, &project, os, memory::ChangeKind::Edit { old, new })?
        }
        MemoryCommand::Remove { text, os, project } => {
            propose(&ws, &project, os, memory::ChangeKind::Remove { text })?
        }
        MemoryCommand::Changes(arg) => {
            let name = project_name(&ws, &arg)?;
            let changes = memory::pending(&ws.config.paths, &name)?;
            if changes.is_empty() {
                eprintln!("no pending changes for {name}");
            }
            for c in changes {
                let text = match &c.change.kind {
                    memory::ChangeKind::Add { text, .. } | memory::ChangeKind::Remove { text } => {
                        text
                    }
                    memory::ChangeKind::Edit { new, .. } => new,
                };
                let first = text.lines().next().unwrap_or("");
                let kind = kind_name(&c.change.kind);
                let target = c.change.os.map_or("shared", Os::name);
                println!(
                    "{}  {kind:<6}  {target:<7}  {}  {first}",
                    c.id, c.change.author
                );
                if let memory::PreviewResult::Stale { reason } = &c.preview {
                    println!("    stale: {reason}");
                }
            }
        }
        MemoryCommand::Path(arg) => {
            println!(
                "{}",
                ws.config
                    .paths
                    .memory_file(&project_name(&ws, &arg)?, None)
                    .display()
            );
        }
    }
    Ok(())
}

fn kind_name(kind: &tworky_core::memory::ChangeKind) -> &'static str {
    use tworky_core::memory::ChangeKind;
    match kind {
        ChangeKind::Add { .. } => "add",
        ChangeKind::Edit { .. } => "edit",
        ChangeKind::Remove { .. } => "remove",
    }
}

fn propose(
    ws: &Workspace,
    arg: &ProjectArg,
    os: Option<OsName>,
    kind: tworky_core::memory::ChangeKind,
) -> Result<()> {
    let name = project_name(ws, arg)?;
    ws.config
        .project(&name)
        .with_context(|| format!("no project called {name}"))?;
    let author = match &arg.worker {
        Some(worker) => worker.clone(),
        None if std::env::var_os("TW_SETUP_SESSION").is_some() => format!("setup@{name}"),
        None => "you".to_string(),
    };
    let kind_name = kind_name(&kind);
    let os = os.map(Os::from);
    let id = tworky_core::memory::propose(&ws.config.paths, &name, os, kind, &author)?;
    let target = os.map_or(String::new(), |os| format!(" ({} memory)", os.label()));
    println!(
        "proposed {kind_name} {id} for {name}{target}; it applies once approved in Tworky (Projects › {name} › Changes)"
    );
    Ok(())
}

fn read_body(args: &BodyArgs) -> Result<String> {
    match (&args.body, args.body_file.as_deref()) {
        (Some(body), _) => Ok(body.clone()),
        (None, Some(path)) => read_text_file(path),
        (None, None) => Ok(String::new()),
    }
}

fn read_text_file(path: &str) -> Result<String> {
    if path == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("reading stdin")?;
        return Ok(text);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
}

fn issue(cmd: IssueCommand) -> Result<()> {
    let repo = Workspace::open()?.config.app.tworky_repo;
    match cmd {
        IssueCommand::Bug(args) => new_issue(&repo, feedback::Kind::Bug, args),
        IssueCommand::Friction(args) => new_issue(&repo, feedback::Kind::Friction, args),
        IssueCommand::Comment {
            number,
            body,
            dry_run,
        } => {
            let body = feedback::with_footer(&read_body(&body)?);
            if dry_run {
                println!("{}", feedback::dry_run_comment(&repo, number, &body));
                return Ok(());
            }
            println!("{}", feedback::comment_issue(&repo, number, &body)?);
            Ok(())
        }
    }
}

fn new_issue(repo: &str, kind: feedback::Kind, args: NewIssueArgs) -> Result<()> {
    let body = feedback::with_footer(&read_body(&args.body)?);
    if args.dry_run {
        println!(
            "{}",
            feedback::dry_run_create(repo, kind, &args.title, &body)
        );
        return Ok(());
    }
    if !args.force {
        let similar = feedback::find_similar(repo, kind, &args.title)?;
        if !similar.is_empty() {
            eprintln!("{}", feedback::similar_hint(&similar));
            std::process::exit(1);
        }
    }
    println!(
        "{}",
        feedback::create_issue(repo, kind, &args.title, &body)?
    );
    Ok(())
}

fn subtask(cmd: SubtaskCommand) -> Result<()> {
    let SubtaskCommand::Add {
        title,
        body,
        assign_me,
        parent,
        dry_run,
        worker,
    } = cmd;
    let ws = Workspace::open()?;
    let (parent_id, repo, label) = match parent {
        Some(url) => {
            let (index, id) = github::issue_by_url(&url, &|i| is_subtask(Some(&ws), i))?;
            let issue = index.get(&id).context("parent not loaded")?;
            (
                id.clone(),
                issue.repo.clone(),
                format!("{}#{}", issue.repo, issue.number),
            )
        }
        None => {
            let id = worker.context("pass --parent <issue-url>, or run inside a Tworky session")?;
            let w = ws.state.get(&id)?.context("worker not found")?;
            let issue = w.issue.with_context(|| {
                format!("{id} is an ad-hoc item with no issue; pass --parent <issue-url>")
            })?;
            let label = format!("{}#{}", issue.repo, issue.number);
            (issue.id, issue.repo, label)
        }
    };
    let project = ws
        .config
        .project_for_repo(&repo)
        .with_context(|| format!("no project config matches {repo}"))?;
    let type_name = project
        .subtask_types
        .first()
        .map(String::as_str)
        .unwrap_or("Subtask");
    let board = project.status_board;
    let assignee = if assign_me { ", assigned to you" } else { "" };
    let on_board = board.map(|b| format!(", on board {b}")).unwrap_or_default();
    if dry_run {
        println!("would create {type_name} \"{title}\" under {label}{assignee}{on_board}");
        return Ok(());
    }
    let created = github::create_subtask(
        &repo, &parent_id, &title, &body, type_name, assign_me, board,
    )?;
    println!("{}", created.url);
    if let Some(e) = created.board_error {
        eprintln!(
            "warning: created, but not added to board {}: {e}",
            board.unwrap_or_default()
        );
    }
    Ok(())
}

fn issues(json: bool) -> Result<()> {
    let ws = Workspace::open().ok();
    let index = github::assigned_issues(&Default::default(), &|i| is_subtask(ws.as_ref(), i))?;
    let forest = index.forest();
    if json {
        println!("{}", serde_json::to_string_pretty(&forest)?);
        return Ok(());
    }
    for node in &forest {
        print_node(node, 0, &index, ws.as_ref());
    }
    Ok(())
}

fn cards() -> Result<()> {
    let ws = Workspace::open()?;
    let items = ws.state.list_items()?;
    let ids: Vec<String> = items
        .iter()
        .filter_map(|i| i.issue().map(|r| r.id.clone()))
        .collect();
    let mut index = github::issues_by_id(&ids, &|i| is_subtask(Some(&ws), i))?;
    tworky_core::cards::fill_missing(&mut index, &items);
    let label = |id: &str| {
        index
            .get(id)
            .map(|i| {
                format!(
                    "[{}] #{} {}",
                    i.issue_type.as_deref().unwrap_or("-"),
                    i.number,
                    i.title
                )
            })
            .unwrap_or_default()
    };
    for card in tworky_core::cards::cards(&items, &index) {
        for a in card.ancestors.iter().rev() {
            println!("  {}", label(a));
        }
        println!("● {}  ({})", label(&card.id), card.item);
        for t in index.children(&card.id) {
            println!("    - {}", label(&t.id));
        }
        println!();
    }
    for i in items.iter().filter(|i| i.issue().is_none()) {
        let kind = if i.pull_request().is_some() {
            "PR"
        } else {
            "item"
        };
        println!("● [{kind}] {}  ({})", i.title, i.name);
    }
    Ok(())
}

fn work(url: &str, fetch: bool, json: bool) -> Result<()> {
    let ws = Workspace::open()?;
    let (index, id) = github::issue_by_url(url, &|i| is_subtask(Some(&ws), i))?;
    let subject = tworky_core::existing::WorkSubject::Issue { id };
    let list = tworky_core::existing::existing_work(&ws, &index, &subject, fetch)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    for b in &list {
        let counts = match (b.ahead, b.behind) {
            (Some(a), Some(z)) => format!(" ↑{a} ↓{z}"),
            _ => String::new(),
        };
        let worker = b
            .worker
            .as_deref()
            .map(|w| format!(" [{w}]"))
            .unwrap_or_default();
        println!("{} {}{counts}{worker}", b.repo, b.branch);
        if let Some(c) = &b.last_commit {
            println!("    {c}");
        }
        for pr in &b.pull_requests {
            let state = if pr.is_draft {
                "DRAFT"
            } else {
                pr.state.as_str()
            };
            println!("    #{} {} ({state})", pr.number, pr.title);
        }
        for s in &b.signals {
            println!("    {:?}: {}", s.level, s.text);
        }
    }
    Ok(())
}

fn is_subtask(ws: Option<&Workspace>, issue: &tworky_core::hierarchy::Issue) -> bool {
    match ws {
        Some(ws) => ws.config.is_subtask(issue),
        None => tworky_core::config::is_subtask_type(None, issue.issue_type.as_deref()),
    }
}

fn print_node(node: &IssueNode, depth: usize, index: &IssueIndex, ws: Option<&Workspace>) {
    let i = &node.issue;
    let marker = if node.assigned { "●" } else { "○" };
    let ty = i.issue_type.as_deref().unwrap_or("-");
    let project = ws
        .and_then(|w| w.config.project_for_repo(&i.repo))
        .map(|p| format!(" [{}]", p.name))
        .unwrap_or_default();
    let leaf = if index.is_leaf(i) { "" } else { " ▾" };
    println!(
        "{}{marker} {ty:<8} {}#{} {}{project}{leaf}",
        "  ".repeat(depth),
        i.repo,
        i.number,
        i.title
    );
    for c in &node.children {
        print_node(c, depth + 1, index, ws);
    }
}

fn worker(cmd: WorkerCommand) -> Result<()> {
    let ws = Workspace::open()?;
    let mut log = |line: &str| println!("{line}");
    match cmd {
        WorkerCommand::Create { url } => {
            let (index, id) = github::issue_by_url(&url, &|i| is_subtask(Some(&ws), i))?;
            let planned = ws.plan(&index, &id)?;
            let w = ws.prepare(&planned.id, &index)?;
            println!("worker {} (slot {}) at {}", w.id, w.slot, w.dir.display());
        }
        WorkerCommand::Ls => {
            for w in ws.state.list()? {
                println!(
                    "{:<20} {:<9?} slot {:<3} {} {}",
                    w.id,
                    w.status,
                    w.slot,
                    w.subject_label(),
                    w.dir.display()
                );
                if let Some(e) = &w.error {
                    println!("    error: {e}");
                }
            }
        }
        WorkerCommand::Show(WorkerArg { id }) => {
            let w = ws.state.get(&id)?.context("worker not found")?;
            println!("{} — {}", w.id, w.subject_label());
            println!("dir:     {}", w.dir.display());
            println!("branch:  {} (suggested)", w.branch);
            println!("stack:   docker compose -p {}", w.compose_project);
            println!("ports:");
            for (name, port) in &w.ports {
                println!("  {name:<14} {port}");
            }
            print_worktrees(&w);
        }
        WorkerCommand::Setup(WorkerArg { id }) => ws.setup(&id, &mut log)?,
        WorkerCommand::Stop(WorkerArg { id }) => ws.terminate(&id, None, &mut log)?,
        WorkerCommand::Rm(WorkerArg { id }) => ws.cleanup(&id, None, &mut log)?,
        WorkerCommand::Prs(WorkerArg { id }) => {
            println!("{}", serde_json::to_string_pretty(&ws.pr_summary(&id)?)?);
        }
        WorkerCommand::Jobs { id } => {
            let workers: Vec<_> = ws
                .state
                .list()?
                .into_iter()
                .filter(|w| id.as_ref().is_none_or(|id| *id == w.id))
                .collect();
            let jobs = ws.jobs(&workers, &mut Default::default())?;
            for w in &workers {
                for j in jobs.get(&w.id).into_iter().flatten() {
                    println!(
                        "{:<20} pid {:<7} {:>6} MB {:>3} procs  up {:>6}s  {}",
                        w.id,
                        j.job.pid,
                        j.job.memory_bytes >> 20,
                        j.job.process_count,
                        j.job.age_secs,
                        j.job.command
                    );
                    if let Some(out) = &j.output {
                        println!("{:<20} output {}", "", out.display());
                    }
                }
            }
        }
        WorkerCommand::StopJob {
            worker: WorkerArg { id },
            pid,
        } => ws.stop_job(&id, pid, &mut log)?,
        WorkerCommand::Launch(WorkerArg { id }) => {
            let l = ws.launch(&id, None)?;
            println!("cd {}", l.cwd.display());
            println!("{} {}", l.program, l.args.join(" "));
        }
    }
    Ok(())
}

fn service(cmd: ServiceCommand) -> Result<()> {
    let ws = Workspace::open()?;
    let mut log = |line: &str| println!("{line}");
    match cmd {
        ServiceCommand::Ls(WorkerArg { id }) => {
            let services = ws.services(&id)?;
            if services.is_empty() {
                let w = ws.state.get(&id)?.context("worker not found")?;
                let project = ws
                    .config
                    .project(&w.project)
                    .context("project not configured")?;
                if tworky_core::services::suggest_services(project) {
                    println!("{}", tworky_core::services::NO_SERVICES_NOTE);
                } else {
                    println!("no services defined");
                }
            }
            for s in services {
                let state = match s.state {
                    ServiceState::Running => "running",
                    ServiceState::Starting => "starting",
                    ServiceState::Stopped => "stopped",
                };
                println!("{:<16} {state:<8} {}", s.name, s.url.unwrap_or_default());
                if let Some(log) = s.log.filter(|_| s.state != ServiceState::Stopped) {
                    println!("{:<16} log {}", "", log.display());
                }
            }
        }
        ServiceCommand::Start {
            name,
            worker: WorkerArg { id },
            no_wait,
        } => {
            for (service, outcome) in ws.start_service(&id, &name, !no_wait, &mut log)? {
                if outcome == tworky_core::services::Outcome::AlreadyRunning {
                    println!("{service} is already running");
                }
            }
        }
        ServiceCommand::Stop {
            name,
            worker: WorkerArg { id },
        } => ws.stop_service(&id, &name, &mut log)?,
    }
    Ok(())
}

fn print_worktrees(w: &tworky_core::state::WorkerRecord) {
    if w.worktrees.is_empty() {
        println!("worktrees: none (add one with `tw worktree add <repo>`)");
        return;
    }
    println!("worktrees:");
    for t in &w.worktrees {
        println!("  {:<28} {} ({:?})", t.repo, t.branch, t.origin);
    }
}

fn worktree(cmd: WorktreeCommand) -> Result<()> {
    let ws = Workspace::open()?;
    let mut log = |line: &str| println!("{line}");
    match cmd {
        WorktreeCommand::Add {
            repo,
            branch,
            base,
            worker,
        } => {
            ws.add_worktree(
                &worker.id,
                &repo,
                branch.as_deref(),
                base.as_deref(),
                &mut log,
            )?;
        }
        WorktreeCommand::Ls(WorkerArg { id }) => {
            let w = ws.state.get(&id)?.context("worker not found")?;
            print_worktrees(&w);
        }
        WorktreeCommand::Rm { repo, worker } => ws.remove_worktree(&worker.id, &repo, &mut log)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tworky_core::activity::{self, Activity, BackgroundTask};

    fn change(activity: Activity, flags: SessionFlags, stdin: &str) -> SessionChange {
        session_change(activity, flags, parse_hook_input(stdin).as_ref())
    }

    const STOP: SessionFlags = SessionFlags {
        new_turn: false,
        new_conversation: false,
        stop: true,
        session_start: false,
    };

    const NEW_TURN: SessionFlags = SessionFlags {
        new_turn: true,
        new_conversation: false,
        stop: false,
        session_start: false,
    };

    fn temp_paths(home: &std::path::Path) -> Paths {
        Paths {
            config_dir: home.join("config"),
            data_dir: home.join("data"),
        }
    }

    #[test]
    fn a_stop_reads_running_background_tasks_from_stdin() {
        let stdin = r#"{"hook_event_name":"Stop","background_tasks":[
            {"id":"a1","type":"subagent","status":"running"},
            {"id":"a2","type":"subagent","status":"completed"},
            {"id":"s1","type":"shell","status":"running"}]}"#;
        let stop = change(Activity::Idle, STOP, stdin);
        assert_eq!(
            stop,
            SessionChange::Stop(Some(vec![BackgroundTask {
                id: "a1".into(),
                kind: "subagent".into()
            }]))
        );

        let home = tempfile::tempdir().unwrap();
        let paths = temp_paths(home.path());
        apply_session_change(&paths, "acme-1", stop).unwrap();
        let session = activity::read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.activity, session.recorded, session.background),
            (Activity::Working, Activity::Idle, true)
        );
    }

    #[test]
    fn a_stop_without_a_task_list_or_input_keeps_the_list() {
        let stdin = r#"{"hook_event_name":"StopFailure","error":"rate_limit"}"#;
        assert_eq!(
            change(Activity::Idle, STOP, stdin),
            SessionChange::Stop(None)
        );
        assert_eq!(change(Activity::Idle, STOP, ""), SessionChange::Stop(None));
        assert_eq!(
            change(Activity::Idle, STOP, "not json"),
            SessionChange::Stop(None)
        );
        assert_eq!(
            change(Activity::Idle, SessionFlags::default(), ""),
            SessionChange::Record(Activity::Idle)
        );
    }

    #[test]
    fn a_task_notification_prompt_is_a_background_report() {
        let stdin = r#"{"hook_event_name":"UserPromptSubmit",
            "prompt":"<task-notification>\n<task-id>a1</task-id>"}"#;
        assert_eq!(
            change(Activity::Working, NEW_TURN, stdin),
            SessionChange::BackgroundReport
        );
        let user =
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"fix the <task-notification> bug"}"#;
        assert_eq!(
            change(Activity::Working, NEW_TURN, user),
            SessionChange::NewTurn
        );
        assert_eq!(
            change(Activity::Working, NEW_TURN, ""),
            SessionChange::NewTurn
        );
    }

    #[test]
    fn a_subagent_tool_call_leaves_an_idle_record_idle() {
        let stdin = r#"{"hook_event_name":"PostToolUse","agent_id":"a1","agent_type":"Explore"}"#;
        let touch = change(Activity::Working, SessionFlags::default(), stdin);
        assert_eq!(touch, SessionChange::Touch);

        let home = tempfile::tempdir().unwrap();
        let paths = temp_paths(home.path());
        activity::record(&paths, "acme-1", Activity::Idle).unwrap();
        apply_session_change(&paths, "acme-1", touch).unwrap();
        assert_eq!(
            activity::read(&paths, "acme-1").unwrap().activity,
            Activity::Idle
        );

        let main = r#"{"hook_event_name":"PostToolUse","tool_name":"Bash"}"#;
        assert_eq!(
            change(Activity::Working, SessionFlags::default(), main),
            SessionChange::Record(Activity::Working)
        );
    }

    #[test]
    fn session_start_and_clear_map_to_their_changes() {
        let start = SessionFlags {
            session_start: true,
            ..SessionFlags::default()
        };
        assert_eq!(
            change(Activity::Idle, start, ""),
            SessionChange::SessionStart
        );
        let clear = SessionFlags {
            new_conversation: true,
            ..SessionFlags::default()
        };
        assert_eq!(
            change(Activity::Idle, clear, ""),
            SessionChange::NewConversation(Activity::Idle)
        );
    }

    /// `tw` commands quoted in a guide, with placeholders filled in.
    fn quoted_commands(guide: &str) -> Vec<String> {
        let mut commands = Vec::new();
        for line in guide.lines() {
            let trimmed = line.trim_start();
            let spans: Vec<&str> = if trimmed.starts_with("tw ") {
                vec![trimmed.split(" # ").next().unwrap_or(trimmed)]
            } else {
                line.split('`').skip(1).step_by(2).collect()
            };
            for span in spans.into_iter().filter(|s| s.starts_with("tw ")) {
                let filled = span
                    .replace("{{ project.name }}", "demo")
                    .replace("bug|friction", "bug");
                let mut out = String::new();
                let mut in_placeholder = false;
                for c in filled.chars() {
                    match c {
                        '<' => {
                            in_placeholder = true;
                            out.push('x');
                        }
                        '>' => in_placeholder = false,
                        _ if !in_placeholder => out.push(c),
                        _ => {}
                    }
                }
                commands.push(out.trim().to_string());
            }
        }
        commands
    }

    fn words(command: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for c in command.chars() {
            match c {
                '"' => quoted = !quoted,
                ' ' if !quoted => {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(c),
            }
        }
        if !current.is_empty() {
            words.push(current);
        }
        words
    }

    #[test]
    fn the_project_setup_guide_only_uses_commands_tw_accepts() {
        let guide = include_str!("../../core/src/project_setup.md");
        let commands = quoted_commands(guide);
        assert!(commands.len() > 10, "{commands:?}");
        for command in commands {
            let parsed = Cli::try_parse_from(words(&command)).or_else(|e| {
                // The guide describes worker commands as a worker session runs them, with
                // $TW_WORKER_ID supplying --worker.
                if e.kind() == clap::error::ErrorKind::MissingRequiredArgument
                    && !command.contains("--worker")
                {
                    Cli::try_parse_from(words(&format!("{command} --worker w")))
                } else {
                    Err(e)
                }
            });
            if let Err(e) = parsed {
                panic!("`{command}` doesn't parse:\n{e}");
            }
        }
    }

    #[test]
    fn an_account_starts_at_a_url_or_a_service() {
        let add = |extra: &[&str]| {
            Cli::try_parse_from(
                ["tw", "accounts", "add", "x", "--project", "p"]
                    .iter()
                    .chain(extra),
            )
        };
        let Command::Accounts(AccountsCommand::Add {
            start_url,
            start_service,
            ..
        }) = add(&["--start-service", "web"]).unwrap().command
        else {
            panic!("not an add");
        };
        assert_eq!((start_url, start_service.as_deref()), (None, Some("web")));
        assert!(add(&["--start-url", "http://localhost:5173/"]).is_ok());
        assert!(add(&[]).is_err());
        assert!(
            add(&[
                "--start-url",
                "http://localhost:5173/",
                "--start-service",
                "web"
            ])
            .is_err()
        );
    }

    #[test]
    fn a_project_can_be_named_as_an_argument_or_an_option() {
        for args in [
            &["tw", "project", "check", "acme"][..],
            &["tw", "project", "check", "--project", "acme"],
        ] {
            let Command::Project(ProjectCommand::Check(named)) =
                Cli::try_parse_from(args).unwrap().command
            else {
                panic!("not a check");
            };
            assert_eq!(named.into_arg().project.as_deref(), Some("acme"));
        }
    }
}
