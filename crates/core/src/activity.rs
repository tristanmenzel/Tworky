use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Paths;

/// What a worker's Claude is doing, as reported by the session's hooks (see `worker::session_settings`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    Working,
    /// Finished its turn: waiting for the next prompt.
    #[default]
    Idle,
    /// Blocked on the user mid-turn: a permission prompt, a question or a plan to approve.
    Waiting,
    /// Idle with a handover for the user to test. Never recorded; `read` derives it.
    Uat,
    /// Idle with accounts for the user to create. Never recorded; `read` derives it.
    Account,
}

impl std::str::FromStr for Activity {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(serde_json::from_value(serde_json::Value::String(s.into()))?)
    }
}

/// What Claude asks the user to check once it has finished a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    pub summary: String,
    pub steps: Vec<String>,
    /// Browser tabs left open on the pages to check.
    #[serde(default)]
    pub tabs: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRequest {
    pub name: String,
    pub username: String,
    /// The service whose page the account starts on.
    pub service: String,
    pub reason: String,
}

/// Work the session started that outlives its turn, as listed by a Stop hook's `background_tasks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: String,
}

/// Shells and monitors are left out because dev servers and watchers run indefinitely.
const COUNTED_KINDS: [&str; 3] = ["subagent", "workflow", "teammate"];

const TERMINAL_STATUSES: [&str; 6] = [
    "completed",
    "failed",
    "killed",
    "stopped",
    "cancelled",
    "canceled",
];

fn in_flight(task: &serde_json::Value) -> bool {
    task["status"].as_str().is_none_or(|status| {
        !TERMINAL_STATUSES
            .iter()
            .any(|t| t.eq_ignore_ascii_case(status))
    })
}

/// The in-flight tasks of the counted kinds in a Stop or StopFailure hook's input, or `None` when it
/// has no `background_tasks` list. The hooks docs list no status values, so any status but a known
/// terminal one counts, as does a missing one. Entries of another kind are skipped.
pub fn background_from_hook(input: &serde_json::Value) -> Option<Vec<BackgroundTask>> {
    let tasks = input.get("background_tasks")?.as_array()?;
    Some(
        tasks
            .iter()
            .filter(|t| in_flight(t))
            .filter_map(|t| {
                let kind = t["type"].as_str()?;
                COUNTED_KINDS.contains(&kind).then(|| BackgroundTask {
                    id: t["id"].as_str().unwrap_or_default().to_string(),
                    kind: kind.to_string(),
                })
            })
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Session {
    pub activity: Activity,
    pub handover: Option<Handover>,
    pub account_requests: Vec<AccountRequest>,
    /// What the hooks last wrote, before handovers, account requests and background work rank it.
    #[serde(skip)]
    pub recorded: Activity,
    #[serde(skip)]
    pub background: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    activity: Activity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handover: Option<Handover>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    account_requests: Vec<AccountRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    background: Vec<BackgroundTask>,
}

impl Record {
    fn session(self) -> Session {
        let background = !self.background.is_empty();
        let activity = match self.activity {
            Activity::Idle if !self.account_requests.is_empty() => Activity::Account,
            Activity::Idle if self.handover.is_some() => Activity::Uat,
            Activity::Idle if background => Activity::Working,
            activity => activity,
        };
        Session {
            activity,
            handover: self.handover,
            account_requests: self.account_requests,
            recorded: self.activity,
            background,
        }
    }
}

fn dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("sessions")
}

fn load(file: &Path) -> Option<Record> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

static UPDATING: Mutex<()> = Mutex::new(());
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Written atomically, so the app polling the folder never reads half a file. Main-agent and
/// subagent hooks run `tw` at the same time, hence the file lock. Every call writes, even when
/// nothing changes, because the app reads a new mtime as a sign of life.
fn update(paths: &Paths, worker: &str, change: impl FnOnce(&mut Record)) -> Result<()> {
    let _updating = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
    let dir = dir(paths);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{worker}.json"));
    let lock_path = dir.join(format!("{worker}.json.lock"));
    let lock = std::fs::File::create(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    let mut record = load(&file).unwrap_or_default();
    change(&mut record);
    let tmp = dir.join(format!(
        "{worker}.{}.{}.tmp",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, serde_json::to_string(&record)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Records what the session is doing. A handover stays until the next prompt or a dismiss.
pub fn record(paths: &Paths, worker: &str, activity: Activity) -> Result<()> {
    update(paths, worker, |r| r.activity = activity)
}

/// The user sent a new prompt: the session is working and any handover or account request is over.
/// Background work stays listed, because Esc can end this turn without a Stop that would re-list it.
pub fn new_turn(paths: &Paths, worker: &str) -> Result<()> {
    update(paths, worker, |r| {
        r.activity = Activity::Working;
        r.handover = None;
        r.account_requests.clear();
    })
}

/// Background work reported back, and the session takes a turn over it. Unlike a prompt from the
/// user, it leaves a handover and account requests pending.
pub fn background_report(paths: &Paths, worker: &str) -> Result<()> {
    update(paths, worker, |r| r.activity = Activity::Working)
}

/// The turn ended. `background` replaces the listed background work; `None` keeps it.
pub fn record_stop(
    paths: &Paths,
    worker: &str,
    background: Option<Vec<BackgroundTask>>,
) -> Result<()> {
    update(paths, worker, |r| {
        r.activity = Activity::Idle;
        if let Some(background) = background {
            r.background = background;
        }
    })
}

/// The session started or resumed, so background work from before is gone.
pub fn session_start(paths: &Paths, worker: &str) -> Result<()> {
    update(paths, worker, |r| {
        r.activity = Activity::Idle;
        r.background.clear();
    })
}

/// The session started a new conversation: a handover, account request or background work from
/// the old one is over.
pub fn new_conversation(paths: &Paths, worker: &str) -> Result<()> {
    if read(paths, worker).is_none() {
        return Ok(());
    }
    update(paths, worker, |r| {
        r.handover = None;
        r.account_requests.clear();
        r.background.clear();
    })
}

/// A subagent finished a tool call. Rewrites the record so it reads as fresh, and ends a `Waiting`,
/// since the call proves its dialog was answered (by mouse, which doesn't count as typing).
/// `Idle` and `Working` stay as they are. Makes no record when there is none.
pub fn touch(paths: &Paths, worker: &str) -> Result<()> {
    if read(paths, worker).is_none() {
        return Ok(());
    }
    update(paths, worker, |r| {
        if r.activity == Activity::Waiting {
            r.activity = Activity::Working;
        }
    })
}

pub fn hand_over(paths: &Paths, worker: &str, handover: Handover) -> Result<()> {
    update(paths, worker, |r| r.handover = Some(handover))
}

pub fn dismiss(paths: &Paths, worker: &str) -> Result<()> {
    update(paths, worker, |r| r.handover = None)
}

/// Adds a request, replacing a pending one for the same account name (ignoring case).
pub fn request_account(paths: &Paths, worker: &str, request: AccountRequest) -> Result<()> {
    update(paths, worker, |r| {
        r.account_requests
            .retain(|pending| !pending.name.eq_ignore_ascii_case(&request.name));
        r.account_requests.push(request);
    })
}

pub fn dismiss_account_request(paths: &Paths, worker: &str, name: &str) -> Result<()> {
    update(paths, worker, |r| {
        r.account_requests
            .retain(|pending| !pending.name.eq_ignore_ascii_case(name))
    })
}

pub fn dismiss_account_requests(paths: &Paths, worker: &str) -> Result<()> {
    update(paths, worker, |r| r.account_requests.clear())
}

pub fn forget(paths: &Paths, worker: &str) {
    let _ = std::fs::remove_file(dir(paths).join(format!("{worker}.json")));
}

pub fn read(paths: &Paths, worker: &str) -> Option<Session> {
    load(&dir(paths).join(format!("{worker}.json"))).map(Record::session)
}

pub fn read_all(paths: &Paths) -> HashMap<String, Session> {
    std::fs::read_dir(dir(paths))
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension()? != "json" {
                return None;
            }
            let worker = path.file_stem()?.to_str()?.to_string();
            Some((worker, load(&path)?.session()))
        })
        .collect()
}

/// Kept apart from `Session` so a write that changes nothing the UI shows isn't reported as a
/// change. A record whose time can't be read is left out.
pub fn modified_all(paths: &Paths) -> std::io::Result<HashMap<String, SystemTime>> {
    let entries = match std::fs::read_dir(dir(paths)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e),
    };
    Ok(entries
        .filter_map(|e| {
            let entry = e.ok()?;
            let path = entry.path();
            if path.extension()? != "json" {
                return None;
            }
            let worker = path.file_stem()?.to_str()?.to_string();
            Some((worker, entry.metadata().ok()?.modified().ok()?))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(home: &Path) -> Paths {
        Paths {
            config_dir: home.join("config"),
            data_dir: home.join("data"),
        }
    }

    fn activity(paths: &Paths, worker: &str) -> Option<Activity> {
        read(paths, worker).map(|s| s.activity)
    }

    #[test]
    fn records_and_reads_back() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        assert_eq!(read(&paths, "acme-1"), None);
        record(&paths, "acme-1", Activity::Working).unwrap();
        record(&paths, "acme-1", Activity::Waiting).unwrap();
        record(&paths, "acme-2", Activity::Idle).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Waiting));
        let all = read_all(&paths);
        assert_eq!(all.len(), 2);
        assert_eq!(all["acme-2"].activity, Activity::Idle);
    }

    #[test]
    fn a_handover_shows_once_idle_and_lasts_until_the_next_prompt() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        record(&paths, "acme-1", Activity::Working).unwrap();
        let handover = Handover {
            summary: "Admins can see the audit page".into(),
            steps: vec!["Open Audit".into()],
            tabs: vec![3],
        };
        hand_over(&paths, "acme-1", handover.clone()).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Working));

        record(&paths, "acme-1", Activity::Idle).unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(session.activity, Activity::Uat);
        assert_eq!(session.handover, Some(handover));

        record(&paths, "acme-1", Activity::Waiting).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Waiting));
        record(&paths, "acme-1", Activity::Idle).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Uat));

        new_turn(&paths, "acme-1").unwrap();
        assert_eq!(read(&paths, "acme-1").unwrap().handover, None);
        record(&paths, "acme-1", Activity::Idle).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Idle));
    }

    #[test]
    fn dismissing_ends_the_handover() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        let handover = Handover {
            summary: "s".into(),
            steps: vec![],
            tabs: vec![],
        };
        hand_over(&paths, "acme-1", handover).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Uat));
        dismiss(&paths, "acme-1").unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Idle));
    }

    fn request(name: &str, reason: &str) -> AccountRequest {
        AccountRequest {
            name: name.into(),
            username: format!("{}@example.com", name.to_lowercase().replace(' ', "")),
            service: "web".into(),
            reason: reason.into(),
        }
    }

    fn requested(paths: &Paths, worker: &str) -> Vec<String> {
        read(paths, worker)
            .unwrap()
            .account_requests
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn an_account_request_shows_once_idle_and_outranks_a_handover() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        record(&paths, "acme-1", Activity::Working).unwrap();
        let handover = Handover {
            summary: "s".into(),
            steps: vec![],
            tabs: vec![],
        };
        hand_over(&paths, "acme-1", handover).unwrap();
        request_account(&paths, "acme-1", request("Admin", "sees audit")).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Working));

        record(&paths, "acme-1", Activity::Idle).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Account));
        record(&paths, "acme-1", Activity::Waiting).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Waiting));
        record(&paths, "acme-1", Activity::Idle).unwrap();

        dismiss_account_requests(&paths, "acme-1").unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Uat));
    }

    #[test]
    fn requests_for_other_names_are_listed_and_the_same_name_replaces() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        request_account(&paths, "acme-1", request("Admin", "first")).unwrap();
        request_account(&paths, "acme-1", request("Tester 2", "second")).unwrap();
        assert_eq!(requested(&paths, "acme-1"), ["Admin", "Tester 2"]);

        request_account(&paths, "acme-1", request("admin", "again")).unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(session.activity, Activity::Account);
        assert_eq!(
            session.account_requests,
            [request("Tester 2", "second"), request("admin", "again")]
        );
    }

    #[test]
    fn account_requests_end_on_the_next_prompt_or_a_dismiss() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        request_account(&paths, "acme-1", request("Admin", "r")).unwrap();
        request_account(&paths, "acme-1", request("Tester 2", "r")).unwrap();

        dismiss_account_request(&paths, "acme-1", "ADMIN").unwrap();
        assert_eq!(requested(&paths, "acme-1"), ["Tester 2"]);
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Account));
        dismiss_account_request(&paths, "acme-1", "Tester 2").unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Idle));

        request_account(&paths, "acme-1", request("Admin", "r")).unwrap();
        new_turn(&paths, "acme-1").unwrap();
        record(&paths, "acme-1", Activity::Idle).unwrap();
        assert_eq!(requested(&paths, "acme-1"), Vec::<String>::new());
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Idle));
    }

    #[test]
    fn a_new_conversation_ends_the_handover_and_account_requests() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        new_conversation(&paths, "acme-1").unwrap();
        assert_eq!(read(&paths, "acme-1"), None, "no record is made");

        let handover = Handover {
            summary: "s".into(),
            steps: vec![],
            tabs: vec![],
        };
        hand_over(&paths, "acme-1", handover).unwrap();
        request_account(&paths, "acme-1", request("Admin", "r")).unwrap();
        new_conversation(&paths, "acme-1").unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.activity, session.handover, session.account_requests),
            (Activity::Idle, None, vec![])
        );
    }

    #[test]
    fn concurrent_updates_in_one_process_all_land() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        std::thread::scope(|scope| {
            for i in 0..8 {
                let paths = &paths;
                scope.spawn(move || {
                    request_account(paths, "acme-1", request(&format!("User {i}"), "r")).unwrap()
                });
            }
        });
        assert_eq!(requested(&paths, "acme-1").len(), 8);
    }

    #[test]
    fn modification_times_cover_records_only() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        assert!(modified_all(&paths).unwrap().is_empty());
        record(&paths, "acme-1", Activity::Working).unwrap();
        std::fs::write(dir(&paths).join("acme-2.1.0.tmp"), "{}").unwrap();
        let modified = modified_all(&paths).unwrap();
        assert_eq!(modified.keys().collect::<Vec<_>>(), ["acme-1"]);
    }

    #[test]
    fn a_record_without_requests_reads_as_none() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        std::fs::create_dir_all(dir(&paths)).unwrap();
        std::fs::write(dir(&paths).join("acme-1.json"), r#"{"activity":"idle"}"#).unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(session.activity, Activity::Idle);
        assert!(session.account_requests.is_empty());
        assert!(!session.background);
    }

    fn stop_input() -> serde_json::Value {
        serde_json::json!({
            "hook_event_name": "Stop",
            "background_tasks": [
                { "id": "a1", "type": "subagent", "status": "running", "description": "d" },
                { "id": "a2", "type": "subagent", "status": "completed" },
                { "id": "a3", "type": "subagent", "status": "pending" },
                { "id": "a4", "type": "subagent" },
                { "id": "a5", "type": "subagent", "status": "Failed" },
                { "id": "a6", "type": "subagent", "status": "killed" },
                { "id": "a7", "type": "subagent", "status": "stopped" },
                { "id": "a8", "type": "subagent", "status": "cancelled" },
                { "id": "a9", "type": "subagent", "status": "CANCELED" },
                { "id": "w1", "type": "workflow", "status": "running" },
                { "id": "t1", "type": "teammate", "status": "running" },
                { "id": "s1", "type": "shell", "status": "running" },
                { "id": "m1", "type": "monitor", "status": "running" },
                { "id": "x1", "status": "running" },
                "junk",
            ],
        })
    }

    fn task(id: &str, kind: &str) -> BackgroundTask {
        BackgroundTask {
            id: id.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn unfinished_tasks_of_counted_kinds_are_background_work() {
        assert_eq!(
            background_from_hook(&stop_input()),
            Some(vec![
                task("a1", "subagent"),
                task("a3", "subagent"),
                task("a4", "subagent"),
                task("w1", "workflow"),
                task("t1", "teammate")
            ])
        );
        assert_eq!(
            background_from_hook(&serde_json::json!({ "background_tasks": [] })),
            Some(vec![])
        );
        assert_eq!(
            background_from_hook(&serde_json::json!({ "hook_event_name": "StopFailure" })),
            None
        );
    }

    #[test]
    fn a_stop_with_background_work_shows_as_working() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        new_turn(&paths, "acme-1").unwrap();
        record_stop(&paths, "acme-1", background_from_hook(&stop_input())).unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.activity, session.recorded, session.background),
            (Activity::Working, Activity::Idle, true)
        );

        record_stop(
            &paths,
            "acme-1",
            background_from_hook(&serde_json::json!({ "background_tasks": [
                { "id": "a2", "type": "subagent", "status": "completed" },
                { "id": "s1", "type": "shell", "status": "running" },
            ] })),
        )
        .unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.activity, session.background),
            (Activity::Idle, false)
        );
    }

    #[test]
    fn a_stop_without_a_list_keeps_background_work_and_an_empty_one_clears_it() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        record_stop(&paths, "acme-1", Some(vec![task("a1", "subagent")])).unwrap();
        record_stop(&paths, "acme-1", None).unwrap();
        assert!(read(&paths, "acme-1").unwrap().background);
        record_stop(&paths, "acme-1", Some(vec![])).unwrap();
        assert!(!read(&paths, "acme-1").unwrap().background);
    }

    #[test]
    fn a_new_turn_keeps_background_work_and_a_session_start_or_new_conversation_clears_it() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        record_stop(&paths, "acme-1", Some(vec![task("a1", "subagent")])).unwrap();
        new_turn(&paths, "acme-1").unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.recorded, session.background),
            (Activity::Working, true)
        );

        session_start(&paths, "acme-1").unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.activity, session.background),
            (Activity::Idle, false)
        );

        record_stop(&paths, "acme-1", Some(vec![task("a1", "subagent")])).unwrap();
        new_conversation(&paths, "acme-1").unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.activity, session.background),
            (Activity::Idle, false)
        );
    }

    #[test]
    fn a_handover_or_account_request_outranks_background_work() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        let handover = Handover {
            summary: "s".into(),
            steps: vec![],
            tabs: vec![],
        };
        hand_over(&paths, "acme-1", handover).unwrap();
        record_stop(&paths, "acme-1", Some(vec![task("a1", "subagent")])).unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(
            (session.activity, session.background),
            (Activity::Uat, true)
        );
        request_account(&paths, "acme-1", request("Admin", "r")).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Account));
    }

    #[test]
    fn a_background_report_keeps_the_handover_and_account_requests() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        let handover = Handover {
            summary: "s".into(),
            steps: vec![],
            tabs: vec![],
        };
        hand_over(&paths, "acme-1", handover.clone()).unwrap();
        request_account(&paths, "acme-1", request("Admin", "r")).unwrap();
        background_report(&paths, "acme-1").unwrap();
        let session = read(&paths, "acme-1").unwrap();
        assert_eq!(session.activity, Activity::Working);
        assert_eq!(session.handover, Some(handover));
        assert_eq!(requested(&paths, "acme-1"), ["Admin"]);
        record(&paths, "acme-1", Activity::Idle).unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Account));
    }

    #[test]
    fn touch_ends_waiting_and_leaves_idle_and_working_alone() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        for (before, after) in [
            (Activity::Waiting, Activity::Working),
            (Activity::Working, Activity::Working),
            (Activity::Idle, Activity::Idle),
        ] {
            record(&paths, "acme-1", before).unwrap();
            touch(&paths, "acme-1").unwrap();
            assert_eq!(activity(&paths, "acme-1"), Some(after), "from {before:?}");
        }
    }

    #[test]
    fn touch_moves_the_mtime_and_changes_nothing_else() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        touch(&paths, "acme-1").unwrap();
        assert_eq!(read(&paths, "acme-1"), None, "no record is made");

        hand_over(
            &paths,
            "acme-1",
            Handover {
                summary: "s".into(),
                steps: vec![],
                tabs: vec![],
            },
        )
        .unwrap();
        record_stop(&paths, "acme-1", Some(vec![task("a1", "subagent")])).unwrap();
        let file = dir(&paths).join("acme-1.json");
        let before = std::fs::read_to_string(&file).unwrap();
        let past = SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(past)
            .unwrap();

        touch(&paths, "acme-1").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert!(modified_all(&paths).unwrap()["acme-1"] > past);
    }

    #[test]
    fn an_update_waits_for_the_file_lock() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        std::fs::create_dir_all(dir(&paths)).unwrap();
        let lock = std::fs::File::create(dir(&paths).join("acme-1.json.lock")).unwrap();
        lock.lock().unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        let writer = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                record(&paths, "acme-1", Activity::Working).unwrap();
                done.send(()).unwrap();
            })
        };
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the update went ahead while the lock was held"
        );
        assert_eq!(read(&paths, "acme-1"), None);

        lock.unlock().unwrap();
        finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the update finishes once the lock is released");
        writer.join().unwrap();
        assert_eq!(activity(&paths, "acme-1"), Some(Activity::Working));
    }
}
