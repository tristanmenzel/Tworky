#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod keep_awake;
mod pty;
mod title;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};
use tworky_core::activity::{self, AccountRequest, Activity, Handover, Session};
use tworky_core::config::Paths;
use tworky_core::existing::WorkSubject;
use tworky_core::github;
use tworky_core::hierarchy::IssueIndex;
use tworky_core::items::{AddOutcome, Listed, ProjectChoice};
use tworky_core::project_setup;
use tworky_core::state::WorkerRecord;
use tworky_core::worker::{PrSummary, Workspace};
use tworky_core::{build_info, feedback, update};

const LOG_LIMIT: usize = 2000;

#[derive(Default)]
struct AppState {
    index: Mutex<IssueIndex>,
    logs: Mutex<HashMap<String, Vec<String>>>,
    busy: Mutex<HashSet<String>>,
    sessions: pty::Sessions,
    /// When each project's repos were last fetched for the existing-work view.
    fetched: Mutex<HashMap<String, std::time::Instant>>,
    assets: Mutex<HashMap<String, std::sync::Arc<github::Asset>>>,
    background_tasks: Mutex<tworky_core::claude_config::BackgroundTasks>,
    activity: Mutex<HashMap<String, Session>>,
    keep_awake: Mutex<keep_awake::Control>,
    keep_awake_changed: std::sync::Condvar,
}

const FETCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    format!("{e:#}")
}

fn workspace() -> CmdResult<Workspace> {
    Workspace::open().map_err(|e| format!("{e:#}"))
}

#[derive(Serialize)]
struct IssuesPayload {
    viewer: String,
    issues: Vec<tworky_core::hierarchy::Issue>,
    cards: Vec<tworky_core::cards::Card>,
    /// issue id → project name, for issues a project config matches
    projects: BTreeMap<String, String>,
    /// Subtask-type issues whose parent is loaded: listed under their parent card.
    subtasks: Vec<String>,
    /// project name → the Projects board number whose Status is shown
    status_boards: BTreeMap<String, u64>,
    config_error: Option<String>,
}

fn is_subtask(ws: Option<&Workspace>, issue: &tworky_core::hierarchy::Issue) -> bool {
    match ws {
        Some(ws) => ws.config.is_subtask(issue),
        None => tworky_core::config::is_subtask_type(None, issue.issue_type.as_deref()),
    }
}

fn listed_issue_ids(ws: Option<&Workspace>) -> Vec<String> {
    ws.and_then(|w| w.state.list_items().ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i.issue().map(|r| r.id.clone()))
        .collect()
}

fn issues_payload(index: &IssueIndex) -> IssuesPayload {
    let (ws, config_error) = match Workspace::open() {
        Ok(ws) => (Some(ws), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let items = ws
        .as_ref()
        .and_then(|w| w.state.list_items().ok())
        .unwrap_or_default();
    let mut index = index.clone();
    tworky_core::cards::fill_missing(&mut index, &items);
    let index = &index;
    let mut projects = BTreeMap::new();
    let mut subtasks = Vec::new();
    for issue in index.issues.values() {
        if let Some(project) = ws
            .as_ref()
            .and_then(|w| w.config.project_for_repo(&issue.repo))
        {
            projects.insert(issue.id.clone(), project.name.clone());
        }
        let has_parent = issue
            .parent_id
            .as_deref()
            .is_some_and(|p| index.get(p).is_some());
        if has_parent && is_subtask(ws.as_ref(), issue) {
            subtasks.push(issue.id.clone());
        }
    }
    for item in &items {
        if let Some(issue) = item.issue() {
            projects.insert(issue.id.clone(), item.project.clone());
        }
    }
    IssuesPayload {
        viewer: index.viewer.clone(),
        issues: index.issues.values().cloned().collect(),
        cards: tworky_core::cards::cards(&items, index),
        projects,
        subtasks,
        status_boards: ws
            .iter()
            .flat_map(|w| &w.config.projects)
            .filter_map(|p| Some((p.name.clone(), p.status_board?)))
            .collect(),
        config_error,
    }
}

#[derive(Serialize)]
struct WorkerView {
    #[serde(flatten)]
    record: WorkerRecord,
    session_alive: bool,
    busy: bool,
    /// What the live session is doing; `None` when no session runs.
    activity: Option<Activity>,
    handover: Option<Handover>,
    account_requests: Vec<AccountRequest>,
}

#[derive(Clone, Serialize)]
struct LogEvent {
    id: String,
    line: String,
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> CmdResult<T> + Send + 'static,
) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(err)?
}

#[tauri::command]
async fn get_issues(app: AppHandle, refresh: bool) -> CmdResult<IssuesPayload> {
    blocking(move || {
        let state = app.state::<AppState>();
        if refresh || state.index.lock().unwrap().viewer.is_empty() {
            let ws = Workspace::open().ok();
            let fresh = github::issues_by_id(&listed_issue_ids(ws.as_ref()), &|i| {
                is_subtask(ws.as_ref(), i)
            })
            .map_err(err)?;
            *state.index.lock().unwrap() = fresh;
        }
        let index = state.index.lock().unwrap().clone();
        Ok(issues_payload(&index))
    })
    .await
}

fn subject_project(
    ws: &Workspace,
    index: &IssueIndex,
    subject: &WorkSubject,
) -> CmdResult<Option<String>> {
    Ok(match subject {
        WorkSubject::Issue { id } => index
            .get(id)
            .and_then(|i| ws.config.project_for_repo(&i.repo))
            .map(|p| p.name.clone()),
        WorkSubject::Item { name } => ws.state.get_item(name).map_err(err)?.map(|i| i.project),
    })
}

#[tauri::command]
async fn existing_work(
    app: AppHandle,
    subject: WorkSubject,
    refresh: bool,
) -> CmdResult<Vec<tworky_core::existing::BranchWork>> {
    blocking(move || {
        let state = app.state::<AppState>();
        let index = state.index.lock().unwrap().clone();
        let ws = workspace()?;
        let project = subject_project(&ws, &index, &subject)?;
        let fetch = project.as_ref().is_some_and(|p| {
            let mut fetched = state.fetched.lock().unwrap();
            let stale = refresh || fetched.get(p).is_none_or(|t| t.elapsed() > FETCH_INTERVAL);
            if stale {
                fetched.insert(p.clone(), std::time::Instant::now());
            }
            stale
        });
        tworky_core::existing::existing_work(&ws, &index, &subject, fetch).map_err(err)
    })
    .await
}

#[tauri::command]
async fn delete_branch(
    app: AppHandle,
    subject: WorkSubject,
    repo: String,
    branch: String,
) -> CmdResult<Vec<String>> {
    blocking(move || {
        let ws = workspace()?;
        let index = app.state::<AppState>().index.lock().unwrap().clone();
        let name = subject_project(&ws, &index, &subject)?
            .ok_or("no project config matches this work item")?;
        let project = ws
            .config
            .project(&name)
            .ok_or_else(|| format!("project '{name}' is no longer configured"))?;
        tworky_core::existing::delete_branch(
            project,
            &repo,
            &branch,
            &tworky_core::existing::open_pr_on_github,
        )
        .map_err(err)
    })
    .await
}

#[tauri::command]
async fn assign_to_me(app: AppHandle, issue_id: String) -> CmdResult<IssuesPayload> {
    blocking(move || {
        let state = app.state::<AppState>();
        let (repo, number) = {
            let index = state.index.lock().unwrap();
            let issue = index.get(&issue_id).ok_or("issue not loaded")?;
            (issue.repo.clone(), issue.number)
        };
        github::assign_to_me(&repo, number).map_err(err)?;
        let mut index = state.index.lock().unwrap();
        let viewer = index.viewer.clone();
        if let Some(issue) = index.issues.get_mut(&issue_id)
            && !issue
                .assignees
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&viewer))
        {
            issue.assignees.push(viewer);
        }
        Ok(issues_payload(&index))
    })
    .await
}

#[tauri::command]
async fn set_issue_state(
    app: AppHandle,
    issue_id: String,
    state: github::IssueState,
) -> CmdResult<IssuesPayload> {
    blocking(move || {
        let app_state = app.state::<AppState>();
        let mut issue = app_state
            .index
            .lock()
            .unwrap()
            .get(&issue_id)
            .cloned()
            .ok_or("issue not loaded")?;
        let was_open = issue.state == "OPEN";
        github::set_issue_state(&mut issue, state).map_err(err)?;
        let now_open = issue.state == "OPEN";
        let mut index = app_state.index.lock().unwrap();
        if was_open != now_open
            && let Some(parent) = issue
                .parent_id
                .as_deref()
                .and_then(|p| index.issues.get_mut(p))
        {
            parent.sub_issues_completed = if now_open {
                parent.sub_issues_completed.saturating_sub(1)
            } else {
                parent.sub_issues_completed + 1
            };
        }
        index.issues.insert(issue.id.clone(), issue);
        Ok(issues_payload(&index))
    })
    .await
}

#[tauri::command]
async fn set_board_status(
    app: AppHandle,
    issue_id: String,
    project_id: String,
    option_id: String,
) -> CmdResult<IssuesPayload> {
    blocking(move || {
        let state = app.state::<AppState>();
        let mut issue = state
            .index
            .lock()
            .unwrap()
            .get(&issue_id)
            .cloned()
            .ok_or("issue not loaded")?;
        github::set_board_status(&mut issue, &project_id, &option_id).map_err(err)?;
        let mut index = state.index.lock().unwrap();
        index.issues.insert(issue.id.clone(), issue);
        Ok(issues_payload(&index))
    })
    .await
}

#[derive(Serialize)]
struct ProjectMemory {
    project: String,
    path: String,
    text: String,
}

#[tauri::command]
async fn list_memories() -> CmdResult<Vec<ProjectMemory>> {
    blocking(|| {
        let ws = workspace()?;
        ws.config
            .projects
            .iter()
            .map(|p| {
                let path = ws.config.paths.memory_file(&p.name);
                Ok(ProjectMemory {
                    project: p.name.clone(),
                    text: tworky_core::memory::read(&path).map_err(err)?,
                    path: path.display().to_string(),
                })
            })
            .collect()
    })
    .await
}

#[tauri::command]
async fn save_memory(project: String, text: String) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        ws.config.project(&project).ok_or("no such project")?;
        tworky_core::memory::save(&ws.config.paths.memory_file(&project), &text).map_err(err)
    })
    .await
}

#[tauri::command]
async fn list_workers(app: AppHandle) -> CmdResult<Vec<WorkerView>> {
    blocking(move || {
        let state = app.state::<AppState>();
        let busy = state.busy.lock().unwrap().clone();
        let activities = state.activity.lock().unwrap().clone();
        let ws = workspace()?;
        Ok(ws
            .state
            .list()
            .map_err(err)?
            .into_iter()
            .map(|record| {
                let session_alive = state.sessions.is_alive(&record.id);
                let session = activities.get(&record.id);
                WorkerView {
                    activity: session_alive.then(|| session.map(|s| s.activity)).flatten(),
                    handover: session.and_then(|s| s.handover.clone()),
                    account_requests: session
                        .map(|s| s.account_requests.clone())
                        .unwrap_or_default(),
                    session_alive,
                    busy: busy.contains(&record.id),
                    record,
                }
            })
            .collect())
    })
    .await
}

#[tauri::command]
async fn list_jobs(
    app: AppHandle,
) -> CmdResult<HashMap<String, Vec<tworky_core::worker::WorkerJob>>> {
    blocking(move || {
        let ws = workspace()?;
        let workers = ws.state.list().map_err(err)?;
        let state = app.state::<AppState>();
        let mut tasks = state.background_tasks.lock().unwrap();
        ws.jobs(&workers, &mut tasks).map_err(err)
    })
    .await
}

#[tauri::command]
async fn stop_job(app: AppHandle, id: String, pid: u32) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        let result = ws
            .stop_job(&id, pid, &mut |line| push_log(&app, &id, line))
            .map_err(err);
        let _ = app.emit("worker-changed", &id);
        result
    })
    .await
}

#[tauri::command]
async fn worker_local(id: String) -> CmdResult<tworky_core::local::LocalStatus> {
    blocking(move || workspace()?.worker_local(&id).map_err(err)).await
}

#[tauri::command]
async fn stop_service(app: AppHandle, id: String, name: String) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        let result = ws
            .stop_service(&id, &name, &mut |line| push_log(&app, &id, line))
            .map_err(err);
        let _ = app.emit("worker-changed", &id);
        result
    })
    .await
}

#[tauri::command]
async fn stop_stack(app: AppHandle, id: String) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        let result = ws
            .stop_stack(&id, &mut |line| push_log(&app, &id, line))
            .map_err(err);
        let _ = app.emit("worker-changed", &id);
        result
    })
    .await
}

#[tauri::command]
async fn job_output(
    path: PathBuf,
    offset: Option<u64>,
) -> CmdResult<tworky_core::claude_config::OutputChunk> {
    blocking(move || tworky_core::claude_config::read_output(&path, offset).map_err(err)).await
}

#[tauri::command]
fn worker_log(state: State<'_, AppState>, id: String) -> Vec<String> {
    state
        .logs
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .unwrap_or_default()
}

fn push_log(app: &AppHandle, id: &str, line: &str) {
    let state = app.state::<AppState>();
    {
        let mut logs = state.logs.lock().unwrap();
        let entry = logs.entry(id.to_string()).or_default();
        entry.push(line.to_string());
        if entry.len() > LOG_LIMIT {
            let excess = entry.len() - LOG_LIMIT;
            entry.drain(..excess);
        }
    }
    let _ = app.emit(
        "worker-log",
        LogEvent {
            id: id.to_string(),
            line: line.to_string(),
        },
    );
}

/// Runs `job` on a background thread with the worker marked busy, streaming its log lines to the UI.
fn run_job(
    app: AppHandle,
    id: String,
    job: impl FnOnce(&Workspace, &mut dyn FnMut(&str)) -> anyhow::Result<()> + Send + 'static,
) -> CmdResult<()> {
    {
        let state = app.state::<AppState>();
        if !state.busy.lock().unwrap().insert(id.clone()) {
            return Err(format!("{id} is already busy"));
        }
        state.logs.lock().unwrap().remove(&id);
    }
    let _ = app.emit("worker-changed", &id);
    std::thread::spawn(move || {
        let mut log = |line: &str| push_log(&app, &id, line);
        let result = Workspace::open().and_then(|ws| job(&ws, &mut log));
        if let Err(e) = result {
            log(&format!("✗ {e:#}"));
        }
        app.state::<AppState>().busy.lock().unwrap().remove(&id);
        let _ = app.emit("worker-changed", &id);
    });
    Ok(())
}

#[tauri::command]
async fn begin_session(app: AppHandle, issue_id: String) -> CmdResult<WorkerRecord> {
    blocking(move || {
        let index = app.state::<AppState>().index.lock().unwrap().clone();
        let ws = workspace()?;
        let record = ws.plan(&index, &issue_id).map_err(err)?;
        let record = ws.prepare(&record.id, &index).map_err(err)?;
        let _ = app.emit("worker-changed", &record.id);
        Ok(record)
    })
    .await
}

#[derive(Serialize)]
struct ProjectSummary {
    #[serde(flatten)]
    config: tworky_core::config::ProjectConfig,
    file: PathBuf,
    memory_file: PathBuf,
    /// The id its setup session's terminal runs under.
    setup_session: String,
    setup_alive: bool,
    setup_started: bool,
}

#[tauri::command]
async fn list_projects(app: AppHandle) -> CmdResult<Vec<ProjectSummary>> {
    blocking(move || {
        let ws = workspace()?;
        let sessions = &app.state::<AppState>().sessions;
        ws.config
            .projects
            .iter()
            .map(|p| {
                let setup_session = project_setup::session_id(&p.name);
                Ok(ProjectSummary {
                    config: p.clone(),
                    file: p.file.clone(),
                    memory_file: ws.config.paths.memory_file(&p.name),
                    setup_alive: sessions.is_alive(&setup_session),
                    setup_started: ws
                        .state
                        .project_session(&p.name)
                        .map_err(err)?
                        .is_some_and(|s| s.claude_started),
                    setup_session,
                })
            })
            .collect()
    })
    .await
}

/// Makes a project for repos no project covers yet, to be set up by its setup session.
#[tauri::command]
async fn create_project(repos: Vec<String>, name: Option<String>) -> CmdResult<String> {
    blocking(move || {
        workspace()?
            .create_repo_project(&repos, name.as_deref())
            .map_err(err)
    })
    .await
}

#[derive(Serialize)]
struct AccountView {
    #[serde(flatten)]
    account: tworky_core::config::AccountConfig,
    has_password: bool,
}

#[derive(Serialize)]
struct AccountsPayload {
    accounts: Vec<AccountView>,
}

fn project_named(ws: &Workspace, name: &str) -> CmdResult<tworky_core::config::ProjectConfig> {
    ws.config
        .project(name)
        .cloned()
        .ok_or_else(|| format!("no project called {name}"))
}

#[tauri::command]
async fn list_accounts(project: String) -> CmdResult<AccountsPayload> {
    use tworky_core::accounts;
    blocking(move || {
        let ws = workspace()?;
        let p = project_named(&ws, &project)?;
        Ok(AccountsPayload {
            accounts: p
                .accounts
                .into_iter()
                .map(|account| AccountView {
                    has_password: accounts::has_password(&p.name, &account.name),
                    account,
                })
                .collect(),
        })
    })
    .await
}

#[tauri::command]
async fn save_account(
    project: String,
    previous: Option<String>,
    account: tworky_core::config::AccountConfig,
) -> CmdResult<()> {
    blocking(move || {
        let p = project_named(&workspace()?, &project)?;
        tworky_core::accounts::save_account(&p, previous.as_deref(), &account).map_err(err)
    })
    .await
}

#[tauri::command]
async fn remove_account(project: String, name: String) -> CmdResult<()> {
    blocking(move || {
        let p = project_named(&workspace()?, &project)?;
        tworky_core::accounts::remove_account(&p, &name).map_err(err)
    })
    .await
}

#[tauri::command]
async fn set_account_password(project: String, name: String, password: String) -> CmdResult<()> {
    blocking(move || {
        let p = project_named(&workspace()?, &project)?;
        if !p.accounts.iter().any(|a| a.name == name) {
            return Err(format!("{project} has no account called {name}"));
        }
        tworky_core::accounts::set_password(&p.name, &name, &password).map_err(err)
    })
    .await
}

#[tauri::command]
async fn search_repos(query: String) -> CmdResult<tworky_core::items::RepoSearch> {
    blocking(move || Ok(workspace()?.repo_candidates(&query))).await
}

#[tauri::command]
async fn list_items() -> CmdResult<Vec<tworky_core::state::Item>> {
    blocking(|| workspace()?.state.list_items().map_err(err)).await
}

/// With `repos`, first makes a project for them (cloning any that aren't local yet).
#[tauri::command]
async fn create_item(
    title: String,
    description: String,
    project: Option<String>,
    repos: Vec<String>,
    project_name: Option<String>,
) -> CmdResult<AddOutcome> {
    blocking(move || {
        let (project, new_project) = match project {
            Some(p) if repos.is_empty() => (p, None),
            _ if !repos.is_empty() => {
                let made = workspace()?
                    .create_repo_project(&repos, project_name.as_deref())
                    .map_err(err)?;
                (made.clone(), Some(made))
            }
            _ => return Err("pick a project or at least one repository".into()),
        };
        let item = workspace()?
            .create_item(&project, &title, &description, false)
            .map_err(err)?;
        Ok(AddOutcome::Added {
            item: Box::new(item),
            new_project,
        })
    })
    .await
}

#[tauri::command]
async fn search_issues(query: String) -> CmdResult<Vec<Listed<github::IssueHit>>> {
    blocking(move || workspace()?.search_issues(&query).map_err(err)).await
}

#[tauri::command]
async fn review_requests() -> CmdResult<Vec<Listed<github::PrHit>>> {
    blocking(|| workspace()?.review_requests().map_err(err)).await
}

#[tauri::command]
async fn add_issue_item(url: String, choice: ProjectChoice) -> CmdResult<AddOutcome> {
    blocking(move || workspace()?.add_issue_item(&url, &choice).map_err(err)).await
}

#[tauri::command]
async fn add_pull_request_item(url: String, choice: ProjectChoice) -> CmdResult<AddOutcome> {
    blocking(move || {
        workspace()?
            .add_pull_request_item(&url, &choice)
            .map_err(err)
    })
    .await
}

#[tauri::command]
async fn begin_item_session(app: AppHandle, name: String) -> CmdResult<WorkerRecord> {
    blocking(move || {
        let index = app.state::<AppState>().index.lock().unwrap().clone();
        let ws = workspace()?;
        let record = ws.plan_item(&name, &index).map_err(err)?;
        let record = ws.prepare(&record.id, &index).map_err(err)?;
        let _ = app.emit("worker-changed", &record.id);
        Ok(record)
    })
    .await
}

#[tauri::command]
async fn link_item(app: AppHandle, name: String, issue_url: String) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        let worker = ws
            .link_item(&name, &issue_url, &|i| ws.config.is_subtask(i))
            .map_err(err)?;
        if let Some(w) = worker {
            let _ = app.emit("worker-changed", &w.id);
        }
        Ok(())
    })
    .await
}

#[tauri::command]
async fn remove_item(name: String) -> CmdResult<()> {
    blocking(move || workspace()?.remove_item(&name).map_err(err)).await
}

/// Puts the `tw` CLI that ships with the app (plus its transitional `wa` alias on Windows) first on
/// the session's PATH, so the agent's `tw worktree add` / `tw worker setup` use the same build as
/// the app. It sits next to the executable on Windows and in dev builds, and in the bundle's
/// resource folder on macOS and Linux.
fn with_cli_on_path(app: &AppHandle, env: &mut BTreeMap<String, String>) {
    let cli = if cfg!(windows) { "tw.exe" } else { "tw" };
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()));
    let Some(dir) = exe_dir
        .into_iter()
        .chain(app.path().resource_dir().ok())
        .find(|d| d.join(cli).is_file())
    else {
        return;
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    if let Ok(joined) =
        std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path)))
    {
        env.insert("PATH".into(), joined.to_string_lossy().into_owned());
    }
}

/// A macOS app started from Finder or the Dock gets launchd's bare PATH, without Homebrew, nvm or
/// anything else the user's shell profile adds, so `claude`, `gh`, `docker` and `node` wouldn't be
/// found. This takes the PATH an interactive login shell ends up with.
#[cfg(unix)]
fn import_login_shell_path() {
    use std::io::IsTerminal;
    use std::process::{Command, Stdio};

    const MARKER: &str = "__TWORKY_PATH__";
    if std::io::stdin().is_terminal() {
        return;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = Command::new(shell)
            .args(["-ilc", &format!("printf '{MARKER}%s' \"$PATH\"")])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    let Ok(Ok(out)) = rx.recv_timeout(std::time::Duration::from_secs(5)) else {
        eprintln!("could not read PATH from the login shell; keeping the inherited one");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(path) = text.rsplit(MARKER).next().filter(|p| !p.trim().is_empty())
        && text.contains(MARKER)
    {
        // SAFETY: called first thing in `main`, before any other thread reads the environment.
        unsafe { std::env::set_var("PATH", path.trim()) };
    }
}

/// Like `run_job`, for jobs that stop the session. Its pid is taken while `claude` still runs, so the
/// processes it started can be found through it; the session itself is dropped once they are stopped.
fn run_session_job(
    app: AppHandle,
    id: String,
    job: impl FnOnce(&Workspace, Option<u32>, &mut dyn FnMut(&str)) -> anyhow::Result<()>
    + Send
    + 'static,
) -> CmdResult<()> {
    let pid = app.state::<AppState>().sessions.pid(&id);
    let (job_app, job_id) = (app.clone(), id.clone());
    run_job(app, id, move |ws, log| {
        let result = job(ws, pid, log);
        job_app.state::<AppState>().sessions.kill(&job_id);
        result
    })
}

#[tauri::command]
async fn cleanup_worker(app: AppHandle, id: String, remove_item: bool) -> CmdResult<()> {
    browser::forget_worker(&app, &id).await;
    if let Some(mcp) = app.try_state::<browser::Mcp>() {
        mcp.revoke(&id);
    }
    let job_id = id.clone();
    blocking(move || {
        run_session_job(app, id, move |ws, pid, log| {
            if remove_item {
                ws.cleanup_and_remove_item(&job_id, pid, log)
            } else {
                ws.cleanup(&job_id, pid, log)
            }
        })
    })
    .await
}

#[tauri::command]
async fn pr_summary(id: String) -> CmdResult<PrSummary> {
    blocking(move || workspace()?.pr_summary(&id).map_err(err)).await
}

#[tauri::command]
async fn open_session(
    app: AppHandle,
    id: String,
    cols: u16,
    rows: u16,
    on_output: Channel<String>,
) -> CmdResult<()> {
    blocking(move || {
        let state = app.state::<AppState>();
        if !state.sessions.is_alive(&id) {
            let ws = workspace()?;
            let setup = project_setup::session_project(&id).map(String::from);
            let mut launch = match &setup {
                Some(project) => ws.launch_project_setup(project).map_err(err)?,
                None => {
                    let mcp = app.try_state::<browser::Mcp>().map(|mcp| mcp.endpoint(&id));
                    ws.launch(&id, mcp.as_ref()).map_err(err)?
                }
            };
            with_cli_on_path(&app, &mut launch.env);
            if launch.replaced_missing {
                push_log(
                    &app,
                    &id,
                    "The previous Claude conversation was never saved, so a new one is starting.",
                );
            }
            if let Some(config) = tworky_core::claude_config::global_config_path()
                && let Err(e) = tworky_core::claude_config::trust_directory(&config, &launch.cwd)
            {
                push_log(
                    &app,
                    &id,
                    &format!("could not pre-trust the session folder: {e:#}"),
                );
            }
            let exit_app = app.clone();
            let exit_id = id.clone();
            state
                .sessions
                .spawn(&id, &launch, cols.max(20), rows.max(5), move || {
                    let _ = exit_app.emit("session-exited", &exit_id);
                    let _ = exit_app.emit("worker-changed", &exit_id);
                })
                .map_err(err)?;
            match &setup {
                Some(project) => ws.mark_project_session_started(project).map_err(err)?,
                None => ws.mark_claude_started(&id).map_err(err)?,
            }
            let _ = app.emit("worker-changed", &id);
        }
        state.sessions.attach(&id, on_output).map_err(err)
    })
    .await
}

#[tauri::command]
fn write_session(state: State<'_, AppState>, id: String, data: String) -> CmdResult<()> {
    // No hook fires when a permission prompt or question is answered, only once the tool finishes,
    // so answering it is taken as the end of the wait. Focus changes are sent as input too, and
    // would otherwise end the wait as soon as the user clicked into the terminal to read the prompt.
    if !pty::is_terminal_report(&data)
        && state
            .activity
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|s| s.activity == Activity::Waiting)
        && let Ok(paths) = Paths::resolve()
    {
        let _ = activity::record(&paths, &id, Activity::Working);
    }
    state.sessions.write(&id, &data).map_err(err)
}

#[derive(Clone, Serialize)]
struct ActivityEvent {
    id: String,
    activity: Option<Activity>,
    handover: Option<Handover>,
    account_requests: Vec<AccountRequest>,
}

#[tauri::command]
fn dismiss_handover(id: String) -> CmdResult<()> {
    activity::dismiss(&Paths::resolve().map_err(err)?, &id).map_err(err)
}

#[tauri::command]
fn dismiss_account_request(id: String, name: String) -> CmdResult<()> {
    activity::dismiss_account_request(&Paths::resolve().map_err(err)?, &id, &name).map_err(err)
}

#[tauri::command]
fn dismiss_account_requests(id: String) -> CmdResult<()> {
    activity::dismiss_account_requests(&Paths::resolve().map_err(err)?, &id).map_err(err)
}

#[derive(Serialize)]
struct VersionInfo {
    commit: String,
    short: String,
    dev: bool,
    status: Option<update::Status>,
    error: Option<String>,
}

fn tworky_repo() -> CmdResult<String> {
    Ok(tworky_core::config::Config::load()
        .map_err(err)?
        .app
        .tworky_repo)
}

#[tauri::command]
async fn update_status() -> CmdResult<VersionInfo> {
    let mut info = VersionInfo {
        commit: build_info::COMMIT.into(),
        short: build_info::SHORT.into(),
        dev: feedback::is_dev_build(),
        status: None,
        error: None,
    };
    if info.dev {
        return Ok(info);
    }
    let repo = tworky_repo()?;
    match blocking(move || update::status(&repo, build_info::COMMIT).map_err(err)).await {
        Ok(status) => info.status = Some(status),
        Err(e) => info.error = Some(e),
    }
    Ok(info)
}

#[tauri::command]
fn update_progress() -> CmdResult<update::ProgressReport> {
    Ok(update::read_progress(&Paths::resolve().map_err(err)?))
}

#[tauri::command]
fn dismiss_update_failure() -> CmdResult<()> {
    update::clear_finished(&Paths::resolve().map_err(err)?);
    Ok(())
}

#[tauri::command]
async fn start_update() -> CmdResult<()> {
    let repo = tworky_repo()?;
    blocking(move || update::start(&Paths::resolve().map_err(err)?, &repo).map_err(err)).await
}

#[tauri::command]
fn restart_for_update(app: AppHandle) -> CmdResult<()> {
    update::request_relaunch(&Paths::resolve().map_err(err)?).map_err(err)?;
    app.exit(0);
    Ok(())
}

/// Claude's spinner repaints while it works, so a session silent this long isn't working. Esc on a
/// prompt ends the turn without a Stop hook, which would otherwise leave it marked as working.
const SILENT_IS_IDLE: std::time::Duration = std::time::Duration::from_secs(20);

/// Follows the activity files the sessions' hooks write, tells the UI about changes, and flashes
/// the taskbar when a session starts waiting or needing an account, or goes idle after working,
/// while the app is in the background.
fn watch_activity(app: AppHandle) {
    let Ok(paths) = Paths::resolve() else { return };
    loop {
        std::thread::sleep(std::time::Duration::from_millis(750));
        let state = app.state::<AppState>();
        let mut now = activity::read_all(&paths);
        for (id, session) in now.iter_mut() {
            if session.activity == Activity::Working
                && state
                    .sessions
                    .silent_for(id)
                    .is_some_and(|d| d > SILENT_IS_IDLE)
                && activity::record(&paths, id, Activity::Idle).is_ok()
                && let Some(updated) = activity::read(&paths, id)
            {
                *session = updated;
            }
        }
        let before = std::mem::replace(&mut *state.activity.lock().unwrap(), now.clone());
        let mut attention = None;
        let ids: HashSet<&String> = before.keys().chain(now.keys()).collect();
        for id in ids {
            let (was, is) = (before.get(id), now.get(id));
            if was == is {
                continue;
            }
            let _ = app.emit(
                "session-activity",
                ActivityEvent {
                    id: id.clone(),
                    activity: is.map(|s| s.activity),
                    handover: is.and_then(|s| s.handover.clone()),
                    account_requests: is.map(|s| s.account_requests.clone()).unwrap_or_default(),
                },
            );
            if !state.sessions.is_alive(id) {
                continue;
            }
            match (was.map(|s| s.activity), is.map(|s| s.activity)) {
                (_, Some(Activity::Waiting)) => {
                    attention = Some(tauri::UserAttentionType::Critical);
                }
                (was, Some(Activity::Account)) if was != Some(Activity::Account) => {
                    attention = Some(tauri::UserAttentionType::Critical);
                }
                (Some(Activity::Working), Some(Activity::Idle))
                | (Some(Activity::Working | Activity::Idle), Some(Activity::Uat)) => {
                    attention.get_or_insert(tauri::UserAttentionType::Informational);
                }
                _ => {}
            }
        }
        if let Some(kind) = attention
            && let Some(window) = app.get_webview_window("main")
            && !window.is_focused().unwrap_or(true)
        {
            let _ = window.request_user_attention(Some(kind));
        }
    }
}

#[tauri::command]
fn resize_session(state: State<'_, AppState>, id: String, cols: u16, rows: u16) -> CmdResult<()> {
    state
        .sessions
        .resize(&id, cols.max(20), rows.max(5))
        .map_err(err)
}

#[tauri::command]
async fn stop_session(app: AppHandle, id: String) -> CmdResult<()> {
    if project_setup::session_project(&id).is_some() {
        app.state::<AppState>().sessions.kill(&id);
        let _ = app.emit("worker-changed", &id);
        return Ok(());
    }
    let job_id = id.clone();
    blocking(move || run_session_job(app, id, move |ws, pid, log| ws.terminate(&job_id, pid, log)))
        .await
}

#[tauri::command]
async fn new_conversation(app: AppHandle, id: String) -> CmdResult<()> {
    app.state::<AppState>().sessions.kill(&id);
    blocking(move || {
        let ws = workspace()?;
        match project_setup::session_project(&id) {
            Some(project) => ws.reset_project_session(project),
            None => ws.reset_claude_session(&id),
        }
        .map_err(err)
    })
    .await
}

#[tauri::command]
async fn open_worker_dir(id: String, repo: Option<String>) -> CmdResult<()> {
    blocking(move || {
        let w = workspace()?
            .state
            .get(&id)
            .map_err(err)?
            .ok_or("worker not found")?;
        let dir = match repo {
            Some(repo) => {
                if !w.worktrees.iter().any(|t| t.repo == repo) {
                    return Err(format!("{repo} is not a worktree of {id}"));
                }
                w.dir.join(repo)
            }
            None => w.dir,
        };
        tauri_plugin_opener::open_path(&dir, None::<&str>).map_err(err)
    })
    .await
}

#[tauri::command]
async fn open_in_vscode(id: String, repo: String) -> CmdResult<()> {
    blocking(move || {
        let w = workspace()?
            .state
            .get(&id)
            .map_err(err)?
            .ok_or("worker not found")?;
        if !w.worktrees.iter().any(|t| t.repo == repo) {
            return Err(format!("{repo} is not a worktree of {id}"));
        }
        tworky_core::vscode::open(&w.dir.join(&repo)).map_err(err)
    })
    .await
}

#[tauri::command]
async fn open_project_config(name: String) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        let p = ws.config.project(&name).ok_or("project not found")?;
        tauri_plugin_opener::open_path(&p.file, None::<&str>).map_err(err)
    })
    .await
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn github_asset(app: &AppHandle, url: &str) -> CmdResult<std::sync::Arc<github::Asset>> {
    let state = app.state::<AppState>();
    if let Some(asset) = state.assets.lock().unwrap().get(url) {
        return Ok(asset.clone());
    }
    let asset = std::sync::Arc::new(github::fetch_asset(url).map_err(err)?);
    state
        .assets
        .lock()
        .unwrap()
        .insert(url.to_string(), asset.clone());
    Ok(asset)
}

/// `ghimg://localhost/<encoded url>` serves GitHub-hosted images fetched with the `gh` token.
fn serve_github_asset(app: &AppHandle, path: &str) -> tauri::http::Response<Vec<u8>> {
    let url = percent_decode(path.trim_start_matches('/'));
    let response = tauri::http::Response::builder();
    match github_asset(app, &url) {
        Ok(asset) => response
            .header(tauri::http::header::CONTENT_TYPE, &asset.content_type)
            .header(tauri::http::header::CACHE_CONTROL, "max-age=86400")
            .body(asset.bytes.clone()),
        Err(e) => response.status(502).body(e.into_bytes()),
    }
    .expect("static response parts")
}

#[cfg(debug_assertions)]
fn use_dev_home() {
    use tworky_core::config::Paths;
    let seeded = Paths::resolve().and_then(|installed| {
        let Some(home) = Paths::dev_home()? else {
            return Ok(vec![]);
        };
        // SAFETY: runs first thing in main, before any other thread exists.
        unsafe { std::env::set_var("TWORKY_HOME", &home) };
        eprintln!("dev build: using {}", home.display());
        Paths::resolve()?.seed_dev(
            &installed,
            tworky_core::worker::worker_id_from_env().as_deref(),
        )
    });
    match seeded {
        Ok(lines) => lines.iter().for_each(|line| eprintln!("{line}")),
        Err(e) => eprintln!("could not seed the dev home: {e:#}"),
    }
}

/// An app started inside a worker (dog-fooding) would otherwise pass that worker's id, ports and
/// compose project on to its own sessions, hooks and services wherever they define fewer.
fn forget_outer_worker() {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if name.starts_with("TW_") || name.starts_with("WA_") || name == "COMPOSE_PROJECT_NAME" {
            // SAFETY: runs in main before any other thread exists.
            unsafe { std::env::remove_var(&key) };
        }
    }
}

fn main() {
    #[cfg(debug_assertions)]
    use_dev_home();
    forget_outer_worker();
    #[cfg(unix)]
    import_login_shell_path();
    match tworky_core::config::Paths::resolve().and_then(|p| p.migrate_legacy()) {
        Ok(copied) => copied.iter().for_each(|line| eprintln!("{line}")),
        Err(e) => eprintln!("could not migrate work-app folders: {e:#}"),
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        - tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        .manage(AppState::default())
        .manage(browser::Browser::default())
        .setup(|app| {
            let handle = app.handle().clone();
            if cfg!(windows) {
                match browser::Mcp::start(&handle) {
                    Ok(mcp) => {
                        app.manage(mcp);
                    }
                    Err(e) => eprintln!("could not start the browser MCP server: {e}"),
                }
                browser::load(&handle);
                tauri::async_runtime::spawn(browser::restore(handle.clone()));
                tauri::async_runtime::spawn(browser::keep_cookies_periodically(handle.clone()));
            }
            let keep_awake_handle = handle.clone();
            std::thread::spawn(move || watch_activity(handle));
            std::thread::spawn(move || keep_awake::run(keep_awake_handle));
            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol("ghimg", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let path = request.uri().path().to_string();
            std::thread::spawn(move || responder.respond(serve_github_asset(&app, &path)));
        })
        .invoke_handler(tauri::generate_handler![
            get_issues,
            assign_to_me,
            set_issue_state,
            set_board_status,
            list_memories,
            save_memory,
            list_projects,
            search_repos,
            list_items,
            create_item,
            search_issues,
            review_requests,
            create_project,
            add_issue_item,
            add_pull_request_item,
            begin_item_session,
            link_item,
            remove_item,
            existing_work,
            delete_branch,
            list_workers,
            worker_log,
            list_jobs,
            stop_job,
            worker_local,
            stop_service,
            stop_stack,
            job_output,
            begin_session,
            cleanup_worker,
            pr_summary,
            open_session,
            write_session,
            resize_session,
            stop_session,
            new_conversation,
            open_worker_dir,
            open_in_vscode,
            open_project_config,
            list_accounts,
            save_account,
            remove_account,
            set_account_password,
            dismiss_handover,
            dismiss_account_request,
            dismiss_account_requests,
            update_status,
            update_progress,
            dismiss_update_failure,
            start_update,
            restart_for_update,
            keep_awake::keep_awake_status,
            keep_awake::set_keep_awake,
            browser::browser_tabs,
            browser::browser_open,
            browser::browser_navigate,
            browser::browser_start_service,
            browser::browser_retry,
            browser::browser_history,
            browser::browser_bounds,
            browser::browser_show,
            browser::browser_close,
            browser::browser_sign_in,
            browser::browser_fill_email,
            browser::browser_end_session,
        ])
        .build(tauri::generate_context!())
        .expect("building tauri app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                let sessions = &app.state::<AppState>().sessions;
                let live = sessions.live();
                if !live.is_empty()
                    && let Ok(ws) = workspace()
                    && let Err(e) = ws.stop_sessions(&live, &mut |line| eprintln!("{line}"))
                {
                    eprintln!("stopping sessions on exit: {e}");
                }
                sessions.kill_all();
                if let Ok(ws) = workspace() {
                    for worker in ws.workers_with_services() {
                        if let Err(e) = ws.stop_services(&worker, &mut |line| eprintln!("{line}")) {
                            eprintln!("stopping {worker}'s services on exit: {e:#}");
                        }
                    }
                }
            }
        });
}
