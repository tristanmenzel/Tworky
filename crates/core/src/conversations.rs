use std::path::Path;

use anyhow::Result;

use crate::state::State;

#[derive(Debug, PartialEq, Eq)]
pub enum Followed {
    /// Saved, or nothing to change.
    Yes,
    /// The new conversation's transcript is in another folder than `launch` resumes from.
    OtherFolder,
}

/// Makes `session`, the conversation Claude moved to with `/clear` or `/resume`, the one the next
/// launch of `id` (a worker, or `setup@<project>`) resumes. A `transcript_path` outside
/// `transcripts_dir`, the folder `launch` resumes from, is refused, because `--resume` couldn't
/// find it; when either is unknown, the session is followed.
pub fn follow(
    state: &State,
    id: &str,
    session: &str,
    transcript_path: Option<&Path>,
    transcripts_dir: Option<&Path>,
) -> Result<Followed> {
    if uuid::Uuid::parse_str(session).is_err() {
        return Ok(Followed::Yes);
    }
    if let (Some(dir), Some(expected)) = (transcript_path.and_then(Path::parent), transcripts_dir)
        && !same_folder_name(dir, expected)
    {
        return Ok(Followed::OtherFolder);
    }
    match crate::project_setup::session_project(id) {
        Some(project) => {
            state.update_project_session(project, |s| {
                follow_session(
                    &mut s.claude_session,
                    &mut s.claude_started,
                    &mut s.running_session,
                    session,
                )
            })?;
        }
        None => {
            state.update(id, |w| {
                follow_session(
                    &mut w.claude_session,
                    &mut w.claude_started,
                    &mut w.running_session,
                    session,
                )
            })?;
        }
    }
    Ok(Followed::Yes)
}

/// Compares only the final component, since Claude and Tworky may spell the same folder's path
/// with different case or separators on Windows.
fn same_folder_name(a: &Path, b: &Path) -> bool {
    match (a.file_name(), b.file_name()) {
        (Some(a), Some(b)) => a
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => true,
    }
}

fn follow_session(
    current: &mut String,
    started: &mut bool,
    running: &mut Option<String>,
    session: &str,
) {
    if current == session {
        return;
    }
    if running.as_deref() == Some(session) {
        *running = None;
    } else if running.is_none() && *started {
        *running = Some(current.clone());
    }
    *current = session.to_string();
    *started = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ProjectSession, WorkerRecord, WorkerStatus};

    const OLD: &str = "11111111-1111-4111-8111-111111111111";
    const NEW: &str = "22222222-2222-4222-8222-222222222222";

    fn worker(id: &str) -> WorkerRecord {
        WorkerRecord {
            id: id.into(),
            project: "acme".into(),
            issue: None,
            item: None,
            pr: None,
            branch: "b".into(),
            slot: 1,
            ports: Default::default(),
            dir: "w".into(),
            compose_project: "tw-x".into(),
            status: WorkerStatus::Ready,
            claude_session: OLD.into(),
            claude_started: true,
            error: None,
            created_at: 0,
            worktrees: vec![],
            stopped: false,
            running_session: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        state: State,
        transcripts: std::path::PathBuf,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(&dir.path().join("s.db")).unwrap();
        state.insert(&worker("acme-1")).unwrap();
        let transcripts = dir.path().join("projects").join("C--w");
        Setup {
            _dir: dir,
            state,
            transcripts,
        }
    }

    impl Setup {
        fn transcript(&self, session: &str) -> std::path::PathBuf {
            self.transcripts.join(format!("{session}.jsonl"))
        }

        fn follow(&self, id: &str, session: &str) -> Followed {
            let transcript = self.transcript(session);
            follow(
                &self.state,
                id,
                session,
                Some(&transcript),
                Some(&self.transcripts),
            )
            .unwrap()
        }

        fn worker(&self) -> WorkerRecord {
            self.state.get("acme-1").unwrap().unwrap()
        }
    }

    #[test]
    fn follows_a_new_conversation_and_keeps_the_running_id() {
        let s = setup();
        assert_eq!(s.follow("acme-1", NEW), Followed::Yes);
        let w = s.worker();
        assert_eq!((w.claude_session.as_str(), w.claude_started), (NEW, true));
        assert_eq!(w.process_session(), OLD);

        let third = "33333333-3333-4333-8333-333333333333";
        s.follow("acme-1", third);
        let w = s.worker();
        assert_eq!(w.claude_session, third);
        assert_eq!(w.process_session(), OLD);

        s.follow("acme-1", OLD);
        let w = s.worker();
        assert_eq!(w.claude_session, OLD);
        assert_eq!(w.running_session, None, "back on the launched conversation");
    }

    #[test]
    fn follows_a_clear_before_the_first_prompt() {
        let s = setup();
        assert!(!s.transcript(OLD).exists());
        assert_eq!(s.follow("acme-1", NEW), Followed::Yes);
        let w = s.worker();
        assert_eq!((w.claude_session.as_str(), w.claude_started), (NEW, true));
    }

    #[test]
    fn a_session_never_started_keeps_no_running_id() {
        let s = setup();
        s.state
            .update("acme-1", |w| w.claude_started = false)
            .unwrap();
        s.follow("acme-1", NEW);
        let w = s.worker();
        assert_eq!((w.claude_session.as_str(), w.claude_started), (NEW, true));
        assert_eq!(w.running_session, None, "nothing was running it");
    }

    #[test]
    fn refuses_a_transcript_in_another_folder() {
        let s = setup();
        let elsewhere = s
            .transcripts
            .with_file_name("C--elsewhere")
            .join(format!("{NEW}.jsonl"));
        let followed = follow(
            &s.state,
            "acme-1",
            NEW,
            Some(&elsewhere),
            Some(&s.transcripts),
        )
        .unwrap();
        assert_eq!(followed, Followed::OtherFolder);
        assert_eq!(s.worker(), worker("acme-1"));
    }

    #[test]
    fn compares_folder_names_ignoring_case() {
        let s = setup();
        let transcript = s
            .transcripts
            .with_file_name("c--W")
            .join(format!("{NEW}.jsonl"));
        let followed = follow(
            &s.state,
            "acme-1",
            NEW,
            Some(&transcript),
            Some(&s.transcripts),
        )
        .unwrap();
        assert_eq!(followed, Followed::Yes);
        assert_eq!(s.worker().claude_session, NEW);
    }

    #[test]
    fn follows_without_an_expected_folder() {
        let s = setup();
        let elsewhere = s
            .transcripts
            .with_file_name("C--elsewhere")
            .join(format!("{NEW}.jsonl"));
        let followed = follow(&s.state, "acme-1", NEW, Some(&elsewhere), None).unwrap();
        assert_eq!(followed, Followed::Yes);
        assert_eq!(s.worker().claude_session, NEW);
    }

    #[test]
    fn ignores_unchanged_invalid_and_unknown_ids() {
        let s = setup();
        s.follow("acme-1", OLD);
        follow(&s.state, "acme-1", "", None, None).unwrap();
        follow(&s.state, "acme-1", "not-a-uuid", None, None).unwrap();
        assert_eq!(s.worker(), worker("acme-1"));
        follow(&s.state, "acme-2", NEW, None, None).unwrap();
        assert!(s.state.get("acme-2").unwrap().is_none());
        follow(&s.state, "setup@nobody", NEW, None, None).unwrap();
        assert!(s.state.project_session("nobody").unwrap().is_none());
    }

    #[test]
    fn follows_a_project_setup_session() {
        let s = setup();
        s.state
            .save_project_session(
                "acme",
                &ProjectSession {
                    claude_session: OLD.into(),
                    claude_started: true,
                    running_session: None,
                },
            )
            .unwrap();
        s.follow("setup@acme", NEW);
        let session = s.state.project_session("acme").unwrap().unwrap();
        assert_eq!(
            (session.claude_session.as_str(), session.claude_started),
            (NEW, true)
        );
        assert_eq!(session.process_session(), OLD);
        assert_eq!(s.worker(), worker("acme-1"), "workers untouched");
    }
}
