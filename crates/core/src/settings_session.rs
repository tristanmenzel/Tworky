use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::claude_config;
use crate::config::{AppConfig, Paths};
use crate::config_repo::{self, HeldBackCause, Operation, SyncRecord, SyncState};
use crate::state::{ProjectSession, State};
use crate::worker::{Launch, session_settings};

/// The settings session's id among sessions. A project named `tworky` has the setup session
/// `setup@tworky`, so this can't collide with it.
pub const SESSION_ID: &str = "settings@tworky";

/// Set in the session's environment, so `tw` can tell it runs there.
pub const ENV_MARKER: &str = "TW_SETTINGS_SESSION";

/// Its row in `app_sessions`.
pub const KEY: &str = "settings";

fn new_session() -> ProjectSession {
    ProjectSession {
        claude_session: uuid::Uuid::new_v4().to_string(),
        claude_started: false,
        prompted_for: None,
    }
}

fn open_state(paths: &Paths) -> Result<State> {
    State::open(&paths.state_db())
}

/// Claude in `repo-sync`, where syncs integrate the remote's changes, resuming its last
/// conversation or starting one about the stopped sync. It loads no project config, so it works
/// while that is broken.
pub fn launch(paths: &Paths, claude_home: Option<&Path>) -> Result<Launch> {
    let app = AppConfig::read(paths)?;
    let cwd = config_repo::sync_worktree_dir(paths)?;
    let state = open_state(paths)?;
    let record = config_repo::sync_status(paths)?;
    let mut session = state.app_session(KEY)?.unwrap_or_else(new_session);
    let mut replaced_missing = false;
    if session.claude_started
        && let Some(home) = claude_home
        && !claude_config::transcript_path(home, &cwd, &session.claude_session).exists()
    {
        session = new_session();
        replaced_missing = true;
    }
    if !session.claude_started {
        session.prompted_for = stopped_sync(&record);
    }
    state.save_app_session(KEY, &session)?;

    let mut args: Vec<String> = app.claude[1..].to_vec();
    args.extend([
        "--settings".into(),
        session_settings(paths, None, false, false, Some(SESSION_ID)),
    ]);
    if session.claude_started {
        args.extend(["--resume".into(), session.claude_session]);
    } else {
        args.extend([
            "--session-id".into(),
            session.claude_session,
            prompt(&record),
        ]);
    }
    let mut env: BTreeMap<String, String> = config_repo::identity_env(paths, &cwd)
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    env.insert("GIT_EDITOR".into(), "true".into());
    env.insert(ENV_MARKER.into(), "1".into());
    Ok(Launch {
        program: app.claude[0].clone(),
        args,
        cwd,
        env,
        replaced_missing,
    })
}

/// Which stopped sync a prompt for `record` is about, or `None` when there's none to resolve. A
/// sync stays the same one, keeping its base and upstream, while its files are being fixed.
fn stopped_sync(record: &SyncRecord) -> Option<String> {
    match &record.state {
        SyncState::Conflict { .. }
        | SyncState::HeldBack { .. }
        | SyncState::HeldInSyncWorktree { .. } => Some(match &record.base {
            Some(base) => format!("{base}..{}", record.upstream.as_deref().unwrap_or("")),
            None => serde_json::to_string(&record.state).unwrap_or_default(),
        }),
        _ => None,
    }
}

/// Readies the settings session to resolve the stopped sync: the next launch resumes its
/// conversation only when that started about the same stopped sync, and otherwise starts a new
/// one with the current prompt. Returns whether it started over, so a running session must be
/// stopped first.
pub fn prepare_resolve(paths: &Paths) -> Result<bool> {
    let state = open_state(paths)?;
    let Some(session) = state.app_session(KEY)? else {
        return Ok(false);
    };
    if !starts_over(&session, &config_repo::sync_status(paths)?) {
        return Ok(false);
    }
    state.save_app_session(KEY, &new_session())?;
    Ok(true)
}

fn starts_over(session: &ProjectSession, record: &SyncRecord) -> bool {
    let current = stopped_sync(record);
    session.claude_started && (current.is_none() || session.prompted_for != current)
}

fn prompt(record: &SyncRecord) -> String {
    let list = |files: &[String]| files.join(", ");
    let task = match (&record.state, record.operation) {
        (SyncState::Conflict { files }, Some(Operation::Rebase)) => format!(
            "A Tworky config sync stopped on a conflict while rebasing this machine's config changes onto the remote's, in: {}. Resolve each conflict, keeping what both sides meant, then `git add` the files and run `git rebase --continue`, repeating until the rebase finishes.",
            list(files)
        ),
        (SyncState::Conflict { files }, Some(Operation::Merge)) => format!(
            "A Tworky config sync stopped on a conflict while merging the remote's config into this machine's, in: {}. Resolve each conflict, keeping what both sides meant, then `git add` the files and run `git commit`. Where both sides have a project of the same name whose [match] repos differ, keep both projects under different names.",
            list(files)
        ),
        (SyncState::Conflict { files }, None) => format!(
            "A Tworky config sync stopped because these files still have conflicts or conflict markers: {}. Fix them, keeping what both sides meant, then `git add` them and run `git commit`.",
            list(files)
        ),
        (
            SyncState::HeldBack {
                files,
                cause: HeldBackCause::Unrecognised,
            },
            _,
        ) => format!(
            "A Tworky config sync held back the remote's changes because these project configs are valid TOML but not project config this Tworky understands: {}. They were most likely written by a newer Tworky on another machine. If so, don't edit them: tell me to update Tworky on this machine, after which the sync goes through. Only if they hold a genuine mistake, fix them so they parse as Tworky project config, then `git add` them and run `git commit`.",
            list(files)
        ),
        (SyncState::HeldInSyncWorktree { files }, _) => format!(
            "Tworky config syncs are on hold because these files in this folder have uncommitted changes: {}. If they're a fix in progress, finish it, then `git add` them and run `git commit`. If they're unwanted, tell me to abort the sync and then sync again: the abort pauses syncing, and the next sync I start discards them.",
            list(files)
        ),
        (SyncState::HeldBack { files, .. }, _) => format!(
            "A Tworky config sync held back the remote's changes because these project configs don't parse: {}. Fix them so they parse as Tworky project config, then `git add` them and run `git commit`.",
            list(files)
        ),
        _ => {
            return "This is a worktree of Tworky's config repo, where syncs bring in the remote's changes. Ask me what I want to do.".into();
        }
    };
    format!(
        "{task} Work only in this folder, a detached worktree of the config repo: don't push, don't touch the main branch and don't abort. Every projects/*/project.toml must still parse. When you're done, run `tw config sync`: Tworky checks the result, moves its config to it and pushes it."
    )
}

pub fn mark_started(paths: &Paths) -> Result<()> {
    let state = open_state(paths)?;
    let mut session = state.app_session(KEY)?.unwrap_or_else(new_session);
    session.claude_started = true;
    state.save_app_session(KEY, &session)
}

/// The next launch starts a new conversation instead of resuming.
pub fn reset(paths: &Paths) -> Result<()> {
    open_state(paths)?.save_app_session(KEY, &new_session())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(launch: &Launch, key: &str) -> Option<String> {
        launch.env.get(key).cloned()
    }

    #[test]
    fn launches_in_repo_sync_with_a_fallback_identity_and_no_editor() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config_repo::test_home(dir.path());

        let launch = launch(&paths, None).unwrap();
        assert_eq!(launch.cwd, paths.repo_sync_dir());
        assert!(paths.repo_sync_dir().join(".git").exists());
        assert_eq!(launch.program, "claude");
        assert_eq!(env(&launch, "GIT_EDITOR").as_deref(), Some("true"));
        assert_eq!(env(&launch, "TW_SETTINGS_SESSION").as_deref(), Some("1"));
        for (key, value) in [
            ("GIT_AUTHOR_NAME", "Tworky"),
            ("GIT_COMMITTER_NAME", "Tworky"),
            ("GIT_AUTHOR_EMAIL", "tworky@localhost"),
            ("GIT_COMMITTER_EMAIL", "tworky@localhost"),
        ] {
            assert_eq!(env(&launch, key).as_deref(), Some(value), "{key}");
        }
        assert!(!launch.args.iter().any(|a| a == "--permission-mode"));
        assert!(launch.args.contains(&"--session-id".to_string()));
        let settings =
            &launch.args[launch.args.iter().position(|a| a == "--settings").unwrap() + 1];
        assert!(
            settings.contains("--worker 'settings@tworky'"),
            "{settings}"
        );
        assert!(!settings.contains("claude.local"), "{settings}");

        mark_started(&paths).unwrap();
        let resumed = super::launch(&paths, None).unwrap();
        assert!(resumed.args.contains(&"--resume".to_string()));
        reset(&paths).unwrap();
        let fresh = super::launch(&paths, None).unwrap();
        assert!(fresh.args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn resolving_resumes_only_a_conversation_about_the_same_stopped_sync() {
        let stopped = |state, base: &str| SyncRecord {
            state,
            base: Some(base.into()),
            upstream: Some("u".into()),
            ..Default::default()
        };
        let files = || vec!["projects/acme/memory.md".to_string()];
        let conflict = stopped(SyncState::Conflict { files: files() }, "b1");
        let session = |started, record: &SyncRecord| ProjectSession {
            claude_started: started,
            prompted_for: stopped_sync(record),
            ..new_session()
        };
        let about_conflict = session(true, &conflict);
        assert!(!starts_over(&about_conflict, &conflict));
        let held = stopped(SyncState::HeldInSyncWorktree { files: files() }, "b1");
        assert!(
            !starts_over(&about_conflict, &held),
            "fixing the same sync keeps the conversation"
        );
        let another = stopped(SyncState::Conflict { files: files() }, "b2");
        assert!(starts_over(&about_conflict, &another));
        let idle = SyncRecord::default();
        assert!(starts_over(&session(true, &idle), &conflict));
        assert!(starts_over(&about_conflict, &idle));
        assert!(!starts_over(&session(false, &idle), &conflict));

        let dir = tempfile::tempdir().unwrap();
        let paths = config_repo::test_home(dir.path());
        assert!(!prepare_resolve(&paths).unwrap());
        launch(&paths, None).unwrap();
        mark_started(&paths).unwrap();
        assert!(prepare_resolve(&paths).unwrap());
        assert!(
            launch(&paths, None)
                .unwrap()
                .args
                .contains(&"--session-id".to_string())
        );
    }

    #[test]
    fn leaves_the_identity_to_git_when_the_user_has_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config_repo::test_home(dir.path());
        std::fs::write(
            dir.path().join("gitconfig"),
            "[user]\n\tname = Test User\n\temail = test@example.com\n",
        )
        .unwrap();

        let launch = launch(&paths, None).unwrap();
        assert_eq!(env(&launch, "GIT_AUTHOR_NAME"), None);
        assert_eq!(env(&launch, "GIT_COMMITTER_EMAIL"), None);
    }

    #[test]
    fn the_prompt_names_the_operation_or_the_held_back_files() {
        let conflict = |operation| SyncRecord {
            state: SyncState::Conflict {
                files: vec!["projects/acme/memory.md".into()],
            },
            operation,
            ..Default::default()
        };
        let rebase = prompt(&conflict(Some(Operation::Rebase)));
        assert!(rebase.contains("git rebase --continue"), "{rebase}");
        assert!(rebase.contains("projects/acme/memory.md"), "{rebase}");
        let merge = prompt(&conflict(Some(Operation::Merge)));
        assert!(merge.contains("`git commit`"), "{merge}");
        assert!(!merge.contains("rebase --continue"), "{merge}");
        let held_back = |cause| {
            prompt(&SyncRecord {
                state: SyncState::HeldBack {
                    files: vec!["projects/beta/project.toml".into()],
                    cause,
                },
                ..Default::default()
            })
        };
        let held = held_back(HeldBackCause::Invalid);
        assert!(held.contains("projects/beta/project.toml"), "{held}");
        assert!(held.contains("don't parse"), "{held}");
        assert!(!held.contains("newer Tworky"), "{held}");
        let newer = held_back(HeldBackCause::Unrecognised);
        assert!(newer.contains("projects/beta/project.toml"), "{newer}");
        assert!(newer.contains("newer Tworky"), "{newer}");
        assert!(newer.contains("update Tworky"), "{newer}");
        assert!(newer.contains("genuine mistake"), "{newer}");
        let on_hold = prompt(&SyncRecord {
            state: SyncState::HeldInSyncWorktree {
                files: vec!["projects/beta/project.toml".into()],
            },
            ..Default::default()
        });
        assert!(on_hold.contains("uncommitted"), "{on_hold}");
        assert!(on_hold.contains("sync again"), "{on_hold}");
        assert!(on_hold.contains("projects/beta/project.toml"), "{on_hold}");
    }
}
