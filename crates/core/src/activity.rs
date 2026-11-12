use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::Result;
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Session {
    pub activity: Activity,
    pub handover: Option<Handover>,
    pub account_requests: Vec<AccountRequest>,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    activity: Activity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handover: Option<Handover>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    account_requests: Vec<AccountRequest>,
}

impl Record {
    fn session(self) -> Session {
        let activity = match self.activity {
            Activity::Idle if !self.account_requests.is_empty() => Activity::Account,
            Activity::Idle if self.handover.is_some() => Activity::Uat,
            activity => activity,
        };
        Session {
            activity,
            handover: self.handover,
            account_requests: self.account_requests,
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

/// Written atomically, so the app polling the folder never reads half a file.
fn update(paths: &Paths, worker: &str, change: impl FnOnce(&mut Record)) -> Result<()> {
    let _updating = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
    let dir = dir(paths);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{worker}.json"));
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
pub fn new_turn(paths: &Paths, worker: &str) -> Result<()> {
    update(paths, worker, |r| {
        r.activity = Activity::Working;
        r.handover = None;
        r.account_requests.clear();
    })
}

/// The session started a new conversation: a handover or account request from the old one is over.
pub fn new_conversation(paths: &Paths, worker: &str) -> Result<()> {
    if read(paths, worker).is_none() {
        return Ok(());
    }
    update(paths, worker, |r| {
        r.handover = None;
        r.account_requests.clear();
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

/// When each record was last written, kept apart from `Session` so a write that changes nothing
/// the UI shows isn't reported as a change.
pub fn modified_all(paths: &Paths) -> HashMap<String, SystemTime> {
    std::fs::read_dir(dir(paths))
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let entry = e.ok()?;
            let path = entry.path();
            if path.extension()? != "json" {
                return None;
            }
            let worker = path.file_stem()?.to_str()?.to_string();
            Some((worker, entry.metadata().ok()?.modified().ok()?))
        })
        .collect()
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
        assert!(modified_all(&paths).is_empty());
        record(&paths, "acme-1", Activity::Working).unwrap();
        std::fs::write(dir(&paths).join("acme-2.1.0.tmp"), "{}").unwrap();
        let modified = modified_all(&paths);
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
    }
}
