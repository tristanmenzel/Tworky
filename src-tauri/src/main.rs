#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod freshness;
mod keep_awake;
mod pty;
mod session_view;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};
use tworky_core::activity::{self, AccountRequest, Activity, Handover, Phase, Session};
use tworky_core::config::{Os, Paths};
use tworky_core::config_repo;
use tworky_core::existing::WorkSubject;
use tworky_core::github;
use tworky_core::hierarchy::IssueIndex;
use tworky_core::items::{AddOutcome, Listed, ProjectChoice};
use tworky_core::memory;
use tworky_core::project_setup;
use tworky_core::settings_session;
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

/// How often the UI refreshes issues from GitHub, and how long a project's repos go between
/// fetches for the existing-work view.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

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
    refresh_secs: u64,
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
        refresh_secs: REFRESH_INTERVAL.as_secs(),
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
    phase: Option<Phase>,
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
            let stale = refresh
                || fetched
                    .get(p)
                    .is_none_or(|t| t.elapsed() > REFRESH_INTERVAL);
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
async fn worker_stages() -> CmdResult<BTreeMap<String, tworky_core::stage::WorkerStage>> {
    blocking(|| tworky_core::stage::worker_stages(&workspace()?).map_err(err)).await
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
    os: Option<Os>,
    path: String,
    text: String,
    hash: String,
}

#[tauri::command]
async fn list_memories() -> CmdResult<Vec<ProjectMemory>> {
    blocking(|| {
        let ws = workspace()?;
        let targets = std::iter::once(None).chain(Os::ALL.map(Some));
        ws.config
            .projects
            .iter()
            .flat_map(|p| targets.clone().map(move |os| (p, os)))
            .map(|(p, os)| {
                let path = ws.config.paths.memory_file(&p.name, os);
                let text = memory::read(&path).map_err(err)?;
                Ok(ProjectMemory {
                    project: p.name.clone(),
                    os,
                    hash: memory::hash(&text),
                    text,
                    path: path.display().to_string(),
                })
            })
            .collect()
    })
    .await
}

#[derive(Serialize)]
struct SessionGuidanceSize {
    os: Os,
    bytes: u64,
    /// The project whose memory was counted: the one asked about, or without one the project with
    /// the most memory on this OS.
    project: Option<String>,
    /// What the session's guidance hook weighs, headers included: past `budget`, sessions get the
    /// files' paths instead of their text.
    measured: usize,
    budget: usize,
}

/// The combined size of the guidance and memory a session on this OS loads: one of `project`, or
/// without a project, one of the project with the most memory.
#[tauri::command]
async fn session_guidance_size(project: Option<String>) -> CmdResult<SessionGuidanceSize> {
    blocking(move || {
        let paths = Paths::resolve().map_err(err)?;
        let os = Os::current();
        let size = |files: &[PathBuf]| -> u64 {
            files
                .iter()
                .filter_map(|f| std::fs::metadata(f).ok())
                .map(|m| m.len())
                .sum()
        };
        let project = match project {
            Some(project) => Some(project),
            None => workspace().ok().and_then(|ws| {
                ws.config
                    .projects
                    .iter()
                    .max_by_key(|p| {
                        size(&[
                            paths.memory_file(&p.name, None),
                            paths.memory_file(&p.name, Some(os)),
                        ])
                    })
                    .map(|p| p.name.clone())
            }),
        };
        let bytes = match &project {
            Some(project) => size(&tworky_core::worker::session_guidance_files(
                &paths, project, os,
            )),
            None => size(&paths.guidance_files(os)),
        };
        Ok(SessionGuidanceSize {
            os,
            bytes,
            measured: tworky_core::worker::guidance_hook_size(&paths, project.as_deref(), os),
            budget: tworky_core::worker::GUIDANCE_BUDGET,
            project,
        })
    })
    .await
}

#[derive(Serialize)]
struct GuidanceFile {
    os: Option<Os>,
    path: String,
    text: String,
    hash: String,
}

/// The user's guidance for every session, then its refinement for each OS.
#[tauri::command]
async fn list_guidance() -> CmdResult<Vec<GuidanceFile>> {
    blocking(|| {
        let paths = Paths::resolve().map_err(err)?;
        std::iter::once(None)
            .chain(Os::ALL.map(Some))
            .map(|os| {
                let path = paths.guidance_file(os);
                let text = memory::read(&path).map_err(err)?;
                Ok(GuidanceFile {
                    os,
                    hash: memory::hash(&text),
                    text,
                    path: path.display().to_string(),
                })
            })
            .collect()
    })
    .await
}

#[tauri::command]
async fn save_guidance(os: Option<Os>, base: String, text: String) -> CmdResult<()> {
    blocking(move || {
        let paths = Paths::resolve().map_err(err)?;
        memory::save_guidance(&paths, os, &base, &text).map_err(err)?;
        wake_config_sync();
        Ok(())
    })
    .await
}

#[tauri::command]
async fn save_memory(project: String, os: Option<Os>, base: String, text: String) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        ws.config.project(&project).ok_or("no such project")?;
        memory::save(&ws.config.paths, &project, os, &base, &text).map_err(err)
    })
    .await
}

#[derive(Serialize)]
struct ProjectMemoryChanges {
    project: String,
    changes: Vec<memory::PendingChange>,
    error: Option<String>,
}

#[tauri::command]
async fn list_memory_changes() -> CmdResult<Vec<ProjectMemoryChanges>> {
    blocking(|| {
        let ws = workspace()?;
        Ok(ws
            .config
            .projects
            .iter()
            .map(|p| {
                let (changes, error) = match memory::pending(&ws.config.paths, &p.name) {
                    Ok(changes) => (changes, None),
                    Err(e) => (Vec::new(), Some(err(e))),
                };
                ProjectMemoryChanges {
                    project: p.name.clone(),
                    changes,
                    error,
                }
            })
            .collect())
    })
    .await
}

#[tauri::command]
async fn approve_memory_changes(
    project: String,
    ids: Vec<String>,
    bases: Vec<memory::Base>,
) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        ws.config.project(&project).ok_or("no such project")?;
        memory::approve(&ws.config.paths, &project, &ids, &bases).map_err(err)
    })
    .await
}

#[tauri::command]
async fn reject_memory_changes(project: String, ids: Vec<String>) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        ws.config.project(&project).ok_or("no such project")?;
        memory::reject(&ws.config.paths, &project, &ids).map_err(err)
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
                    phase: session.and_then(|s| s.phase),
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
                    memory_file: ws.config.paths.memory_file(&p.name, None),
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

#[tauri::command]
async fn list_project_errors() -> CmdResult<BTreeMap<String, String>> {
    blocking(|| Ok(workspace()?.config.project_errors)).await
}

/// Problems with the config as a whole: the config repo not set up, `machine.toml` not loading, or
/// the last commit or sweep failing.
#[tauri::command]
async fn list_config_errors() -> CmdResult<Vec<String>> {
    blocking(|| Ok(workspace()?.config.load_errors)).await
}

#[tauri::command]
async fn clone_project_repo(project: String, repo: String) -> CmdResult<PathBuf> {
    blocking(move || {
        workspace()?
            .clone_project_repo(&project, &repo)
            .map_err(err)
    })
    .await
}

#[tauri::command]
async fn find_project_clones(project: String) -> CmdResult<Vec<tworky_core::config::FoundSource>> {
    blocking(move || {
        let ws = workspace()?;
        tworky_core::config::find_sources(&ws.config, Some(&project)).map_err(err)
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
        let ws = workspace()?;
        let p = project_named(&ws, &project)?;
        tworky_core::accounts::save_account(&ws.config.paths, &p, previous.as_deref(), &account)
            .map_err(err)
    })
    .await
}

#[tauri::command]
async fn remove_account(project: String, name: String) -> CmdResult<()> {
    blocking(move || {
        let ws = workspace()?;
        let p = project_named(&ws, &project)?;
        tworky_core::accounts::remove_account(&ws.config.paths, &p, &name).map_err(err)
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
            let paths = Paths::resolve().map_err(err)?;
            let settings = id == settings_session::SESSION_ID;
            let ws = if settings { None } else { Some(workspace()?) };
            let setup = project_setup::session_project(&id).map(String::from);
            let mut launch = match (&ws, &setup) {
                (None, _) => settings_session::launch(
                    &paths,
                    tworky_core::claude_config::claude_home().as_deref(),
                )
                .map_err(err)?,
                (Some(ws), Some(project)) => ws.launch_project_setup(project).map_err(err)?,
                (Some(ws), None) => {
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
            let _ = activity::launching(&paths, &id);
            let exit_app = app.clone();
            let exit_id = id.clone();
            state
                .sessions
                .spawn(&id, &launch, cols.max(20), rows.max(5), move || {
                    let _ = exit_app.emit("session-exited", &exit_id);
                    let _ = exit_app.emit("worker-changed", &exit_id);
                })
                .map_err(err)?;
            match (&ws, &setup) {
                (None, _) => settings_session::mark_started(&paths).map_err(err)?,
                (Some(ws), Some(project)) => {
                    ws.mark_project_session_started(project).map_err(err)?
                }
                (Some(ws), None) => ws.mark_claude_started(&id).map_err(err)?,
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
        let _ = activity::correct(&paths, &id, Activity::Working);
    }
    state.sessions.write(&id, &data).map_err(err)
}

#[derive(Clone, Serialize)]
struct ActivityEvent {
    id: String,
    activity: Option<Activity>,
    handover: Option<Handover>,
    account_requests: Vec<AccountRequest>,
    phase: Option<Phase>,
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
    version: String,
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
        version: env!("CARGO_PKG_VERSION").into(),
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
    let mut memory_changes = memory_change_names(&paths);
    let mut memory_files = memory_file_stamps(&paths);
    let mut config_sync = None;
    let mut freshness = freshness::Freshness::default();
    let mut live_before = HashMap::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(750));
        let names = memory_change_names(&paths);
        let stamps = memory_file_stamps(&paths);
        if names != memory_changes || stamps != memory_files {
            memory_changes = names;
            memory_files = stamps;
            let _ = app.emit("memory-changes", ());
        }
        let sync_stamp = file_stamp(&paths.config_sync_file());
        if sync_stamp != config_sync {
            config_sync = sync_stamp;
            if let Ok(record) = config_repo::sync_status(&paths) {
                let _ = app.emit("config-sync", record);
            }
        }
        let state = app.state::<AppState>();
        // Liveness first: a launch between the two reads would otherwise pair its new generation
        // with the record from before its reset, and the reset would flash on the next poll.
        let live = state.sessions.live_sessions();
        let mut now = activity::read_all(&paths);
        let polled = std::time::Instant::now();
        freshness.poll(now.iter().map(|(id, s)| (id.as_str(), s.beat)), polled);
        let mut quietly_idle = HashSet::new();
        for (id, session) in now.iter_mut() {
            if session.recorded == Activity::Working
                && live
                    .get(id)
                    .and_then(|l| l.silent_for(polled))
                    .is_some_and(|d| d > SILENT_IS_IDLE)
                && activity::correct(&paths, id, Activity::Idle).is_ok()
                && let Some(updated) = activity::read(&paths, id)
            {
                *session = updated;
            }
            if session_view::settle(
                session,
                live.contains_key(id),
                freshness.is_stale(id, polled),
            ) {
                quietly_idle.insert(id.clone());
            }
        }
        let before = std::mem::replace(&mut *state.activity.lock().unwrap(), now.clone());
        let mut attention = None;
        let ids: HashSet<&String> = before.keys().chain(now.keys()).collect();
        for id in ids {
            let (was, is) = (before.get(id), now.get(id));
            if was.map(session_view::shown) == is.map(session_view::shown) {
                continue;
            }
            let _ = app.emit(
                "session-activity",
                ActivityEvent {
                    id: id.clone(),
                    activity: is.map(|s| s.activity),
                    handover: is.and_then(|s| s.handover.clone()),
                    account_requests: is.map(|s| s.account_requests.clone()).unwrap_or_default(),
                    phase: is.and_then(|s| s.phase),
                },
            );
            let generation = |live: &HashMap<String, pty::Live>| live.get(id).map(|l| l.generation);
            if !session_view::same_spawn(generation(&live_before), generation(&live)) {
                continue;
            }
            match session_view::flash(
                was.map(|s| s.activity),
                is.map(|s| s.activity),
                quietly_idle.contains(id),
            ) {
                Some(session_view::Flash::Urgent) => {
                    attention = Some(tauri::UserAttentionType::Critical);
                }
                Some(session_view::Flash::Gentle) => {
                    attention.get_or_insert(tauri::UserAttentionType::Informational);
                }
                None => {}
            }
        }
        live_before = live;
        if let Some(kind) = attention
            && let Some(window) = app.get_webview_window("main")
            && !window.is_focused().unwrap_or(true)
        {
            let _ = window.request_user_attention(Some(kind));
        }
    }
}

fn memory_change_names(paths: &Paths) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(projects) = std::fs::read_dir(paths.data_dir.join("memory-changes")) else {
        return names;
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") {
                names.insert(format!("{}/{name}", project.file_name().to_string_lossy()));
            }
        }
    }
    names
}

/// Each memory and guidance file's modified time and size, so an edit from anywhere, or a sync,
/// refreshes open diffs and views.
fn memory_file_stamps(paths: &Paths) -> BTreeMap<String, (Option<std::time::SystemTime>, u64)> {
    let mut stamps = BTreeMap::new();
    for file in std::iter::once(None)
        .chain(Os::ALL.map(Some))
        .map(|os| paths.guidance_file(os))
    {
        if let Some(stamp) = file_stamp(&file) {
            stamps.insert(file.display().to_string(), stamp);
        }
    }
    let Ok(projects) = std::fs::read_dir(paths.repo_projects_dir()) else {
        return stamps;
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.starts_with("memory")
                && name.ends_with(".md")
                && let Ok(meta) = file.metadata()
            {
                let key = format!("{}/{name}", project.file_name().to_string_lossy());
                stamps.insert(key, (meta.modified().ok(), meta.len()));
            }
        }
    }
    stamps
}

fn file_stamp(file: &std::path::Path) -> Option<(Option<std::time::SystemTime>, u64)> {
    let meta = std::fs::metadata(file).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

#[tauri::command]
async fn config_sync_status() -> CmdResult<config_repo::SyncRecord> {
    blocking(|| config_repo::sync_status(&Paths::resolve().map_err(err)?).map_err(err)).await
}

#[tauri::command]
async fn sync_config_now() -> CmdResult<config_repo::SyncRecord> {
    blocking(|| {
        let paths = Paths::resolve().map_err(err)?;
        config_repo::sync(&paths, config_repo::Trigger::User).map_err(err)
    })
    .await
}

/// Readies the settings session to resolve the stopped sync, stopping it when its conversation is
/// about another one. Returns whether it was restarted.
#[tauri::command]
async fn prepare_settings_resolve(app: AppHandle) -> CmdResult<bool> {
    blocking(move || {
        let restarted =
            settings_session::prepare_resolve(&Paths::resolve().map_err(err)?).map_err(err)?;
        if restarted {
            app.state::<AppState>()
                .sessions
                .kill(settings_session::SESSION_ID);
        }
        Ok(restarted)
    })
    .await
}

#[tauri::command]
async fn abort_config_sync() -> CmdResult<config_repo::SyncRecord> {
    blocking(|| config_repo::abort(&Paths::resolve().map_err(err)?).map_err(err)).await
}

#[tauri::command]
async fn dismiss_config_incoming() -> CmdResult<()> {
    blocking(|| config_repo::dismiss_incoming(&Paths::resolve().map_err(err)?).map_err(err)).await
}

#[derive(Serialize)]
struct ConfigRepoStatus {
    /// With any credentials in it redacted.
    remote: Option<String>,
    /// Uncommitted allowlisted files, such as a `project.toml` that doesn't parse.
    changes: Vec<String>,
    ahead_behind: Option<(u32, u32)>,
    stale_index_lock: bool,
}

#[tauri::command]
async fn config_repo_status() -> CmdResult<ConfigRepoStatus> {
    blocking(|| {
        let paths = Paths::resolve().map_err(err)?;
        let remote = config_repo::remote_url(&paths).map(|url| config_repo::redact_url(&url));
        if !paths.has_repo() {
            return Ok(ConfigRepoStatus {
                remote,
                changes: Vec::new(),
                ahead_behind: None,
                stale_index_lock: false,
            });
        }
        let status = config_repo::status(&paths).map_err(err)?;
        Ok(ConfigRepoStatus {
            remote,
            changes: status.changes,
            ahead_behind: status.ahead_behind,
            stale_index_lock: status.stale_index_lock,
        })
    })
    .await
}

#[tauri::command]
async fn config_first_run() -> CmdResult<bool> {
    blocking(|| {
        Ok(config_repo::awaits_first_run(
            &Paths::resolve().map_err(err)?,
        ))
    })
    .await
}

#[tauri::command]
async fn start_fresh_config() -> CmdResult<()> {
    blocking(|| {
        let paths = Paths::resolve().map_err(err)?;
        for line in config_repo::migrate(&paths).map_err(err)? {
            eprintln!("{line}");
        }
        Ok(())
    })
    .await
}

#[tauri::command]
async fn find_shared_configs() -> CmdResult<Vec<config_repo::SharedConfig>> {
    blocking(|| config_repo::find_shared_configs(&config_repo::Gh).map_err(err)).await
}

#[tauri::command]
async fn use_shared_config(
    remote: String,
    seen: Option<config_repo::Seen>,
) -> CmdResult<ConnectOutcome> {
    blocking(move || {
        let paths = Paths::resolve().map_err(err)?;
        let outcome = connect_outcome(config_repo::use_shared_config(
            &paths,
            &config_repo::Gh,
            &remote,
            seen,
        ))?;
        wake_config_sync();
        Ok(outcome)
    })
    .await
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConnectOutcome {
    Connected {
        connected: Box<config_repo::Connected>,
    },
    /// The remote's check found more than the user saw; they're shown it to confirm again.
    Changed { check: config_repo::RemoteCheck },
}

fn connect_outcome(result: anyhow::Result<config_repo::Connected>) -> CmdResult<ConnectOutcome> {
    match result {
        Ok(connected) => Ok(ConnectOutcome::Connected {
            connected: Box::new(connected),
        }),
        Err(e) => match e.downcast::<config_repo::CheckChanged>() {
            Ok(changed) => Ok(ConnectOutcome::Changed { check: changed.0 }),
            Err(e) => Err(err(e)),
        },
    }
}

#[tauri::command]
async fn config_github_owners() -> CmdResult<Vec<String>> {
    blocking(|| tworky_core::github::viewer_owners().map_err(err)).await
}

#[tauri::command]
async fn check_config_remote(remote: Option<String>) -> CmdResult<config_repo::RemoteCheck> {
    blocking(move || {
        match remote {
            Some(remote) => config_repo::check_remote(&config_repo::Gh, &remote),
            None => {
                config_repo::check_current_remote(&Paths::resolve().map_err(err)?, &config_repo::Gh)
            }
        }
        .map_err(err)
    })
    .await
}

#[tauri::command]
async fn set_config_remote(
    remote: String,
    seen: Option<config_repo::Seen>,
) -> CmdResult<ConnectOutcome> {
    blocking(move || {
        let paths = Paths::resolve().map_err(err)?;
        connect_outcome(config_repo::connect_remote(
            &paths,
            &config_repo::Gh,
            &remote,
            seen,
        ))
    })
    .await
}

/// Connects the repo's current remote, added outside Tworky, once the user has seen its check.
#[tauri::command]
async fn confirm_config_remote(seen: config_repo::Seen) -> CmdResult<ConnectOutcome> {
    blocking(move || {
        let paths = Paths::resolve().map_err(err)?;
        connect_outcome(config_repo::confirm_remote(&paths, &config_repo::Gh, &seen))
    })
    .await
}

#[tauri::command]
async fn create_config_remote(owner: Option<String>) -> CmdResult<config_repo::Connected> {
    blocking(move || {
        let paths = Paths::resolve().map_err(err)?;
        config_repo::create_remote(&paths, &config_repo::Gh, owner.as_deref()).map_err(err)
    })
    .await
}

#[tauri::command]
async fn remove_config_remote() -> CmdResult<config_repo::SyncRecord> {
    blocking(|| config_repo::remove_remote(&Paths::resolve().map_err(err)?).map_err(err)).await
}

const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// How long the config repo's `main` must stay put after a commit before it's synced, so a burst
/// of commits syncs once.
const COMMIT_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

static CONFIG_SYNC_WAKE: (Mutex<bool>, std::sync::Condvar) =
    (Mutex::new(false), std::sync::Condvar::new());

/// Sweeps and syncs the config repo soon, as on focus.
fn wake_config_sync() {
    let (woken, wake) = &CONFIG_SYNC_WAKE;
    *woken.lock().unwrap() = true;
    wake.notify_one();
}

/// Sweeps the config repo and syncs it with its remote: at start, on focus, every
/// [`SWEEP_INTERVAL`], and [`COMMIT_DEBOUNCE`] after a commit, which it notices by `main` moving.
fn sync_config_loop() {
    use std::time::Instant;
    let Ok(paths) = Paths::resolve() else { return };
    let mut due = true;
    let mut next_interval = Instant::now() + SWEEP_INTERVAL;
    let mut main = config_repo::main_ref_stamp(&paths);
    let mut moved_at = None;
    loop {
        if due {
            moved_at = None;
            next_interval = Instant::now() + SWEEP_INTERVAL;
            sweep_config(&paths);
            if paths.has_repo()
                && let Err(e) = config_repo::sync(&paths, config_repo::Trigger::Background)
            {
                eprintln!("config repo: sync failed: {e:#}");
            }
            main = config_repo::main_ref_stamp(&paths);
        }
        let (woken, wake) = &CONFIG_SYNC_WAKE;
        let mut woken = wake
            .wait_timeout(woken.lock().unwrap(), std::time::Duration::from_secs(1))
            .unwrap()
            .0;
        due = std::mem::take(&mut *woken);
        drop(woken);
        let now = Instant::now();
        let stamp = config_repo::main_ref_stamp(&paths);
        if stamp != main {
            main = stamp;
            moved_at = Some(now);
        }
        due |= now >= next_interval || moved_at.is_some_and(|at| now - at >= COMMIT_DEBOUNCE);
    }
}

/// Commits edits made to the config repo outside Tworky, such as by a setup session.
fn sweep_config(paths: &Paths) {
    let swept = (|| {
        if !paths.has_repo() {
            return Ok(None);
        }
        let guard = config_repo::with_lock(paths)?;
        config_repo::sweep(&guard, paths).map(Some)
    })();
    match swept {
        Ok(None) => {}
        Ok(Some(swept)) => {
            for file in swept.held_back {
                eprintln!(
                    "config repo: not committing {}: it doesn't parse",
                    file.display()
                );
            }
        }
        Err(e) => eprintln!("config repo: sweep failed: {e:#}"),
    }
}

/// Once per start, looks for clones of repos that have no source on this machine.
fn find_missing_sources() {
    let found = tworky_core::config::Config::load()
        .and_then(|config| tworky_core::config::find_sources(&config, None));
    match found {
        Ok(found) => {
            for f in found {
                eprintln!(
                    "found {} of {} at {}",
                    f.repo,
                    f.project,
                    f.source.display()
                );
            }
        }
        Err(e) => eprintln!("looking for clones: {e:#}"),
    }
}

#[tauri::command]
fn windows_build() -> Option<u32> {
    pty::windows_build()
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
    if id == settings_session::SESSION_ID || project_setup::session_project(&id).is_some() {
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
        if id == settings_session::SESSION_ID {
            return settings_session::reset(&Paths::resolve().map_err(err)?).map_err(err);
        }
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
    match tworky_core::config::Paths::resolve().and_then(|p| {
        if config_repo::awaits_first_run(&p) {
            Ok(vec![])
        } else {
            p.migrate_to_repo()
        }
    }) {
        Ok(lines) => lines.iter().for_each(|line| eprintln!("{line}")),
        Err(e) => eprintln!("{e:#}"),
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
            std::thread::spawn(find_missing_sources);
            std::thread::spawn(sync_config_loop);
            Ok(())
        })
        .on_window_event(|_, event| {
            if let tauri::WindowEvent::Focused(true) = event {
                wake_config_sync();
            }
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
            session_guidance_size,
            list_guidance,
            save_guidance,
            save_memory,
            list_memory_changes,
            approve_memory_changes,
            reject_memory_changes,
            list_projects,
            list_project_errors,
            list_config_errors,
            config_sync_status,
            config_repo_status,
            sync_config_now,
            abort_config_sync,
            prepare_settings_resolve,
            dismiss_config_incoming,
            config_first_run,
            start_fresh_config,
            find_shared_configs,
            use_shared_config,
            config_github_owners,
            check_config_remote,
            set_config_remote,
            confirm_config_remote,
            create_config_remote,
            remove_config_remote,
            clone_project_repo,
            find_project_clones,
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
            worker_stages,
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
            windows_build,
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
            browser::browser_set_viewport,
            browser::browser_show,
            browser::browser_close,
            browser::browser_sign_in,
            browser::browser_fill_email,
            browser::browser_reset_session,
        ])
        .build(tauri::generate_context!())
        .expect("building tauri app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                browser::save_tabs_now(app);
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
