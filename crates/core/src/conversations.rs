use std::path::Path;

use anyhow::Result;

use crate::state::State;

/// Makes `session`, the conversation Claude moved to with `/clear` or `/resume`, the one the next
/// launch of `id` (a worker, or `setup@<project>`) resumes. `transcript_path` is where Claude keeps
/// the new conversation: one in another folder than the current conversation's is refused, because
/// `launch` couldn't resume it.
pub fn follow(
    state: &State,
    id: &str,
    session: &str,
    transcript_path: Option<&Path>,
) -> Result<()> {
    if uuid::Uuid::parse_str(session).is_err() {
        return Ok(());
    }
    match crate::project_setup::session_project(id) {
        Some(project) => {
            state.update_project_session(project, |s| {
                follow_session(
                    &mut s.claude_session,
                    &mut s.claude_started,
                    &mut s.running_session,
                    session,
                    transcript_path,
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
                    transcript_path,
                )
            })?;
        }
    }
    Ok(())
}

fn follow_session(
    current: &mut String,
    started: &mut bool,
    running: &mut Option<String>,
    session: &str,
    transcript_path: Option<&Path>,
) {
    if current == session {
        return;
    }
    let same_folder = |dir: &Path| {
        std::iter::once(current.as_str())
            .chain(running.as_deref())
            .any(|old| dir.join(format!("{old}.jsonl")).exists())
    };
    if *started
        && let Some(dir) = transcript_path.and_then(Path::parent)
        && !same_folder(dir)
    {
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
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(transcripts.join(format!("{OLD}.jsonl")), "{}\n").unwrap();
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

        fn worker(&self) -> WorkerRecord {
            self.state.get("acme-1").unwrap().unwrap()
        }
    }

    #[test]
    fn follows_a_new_conversation_and_keeps_the_running_id() {
        let s = setup();
        follow(&s.state, "acme-1", NEW, Some(&s.transcript(NEW))).unwrap();
        let w = s.worker();
        assert_eq!((w.claude_session.as_str(), w.claude_started), (NEW, true));
        assert_eq!(w.process_session(), OLD);

        let third = "33333333-3333-4333-8333-333333333333";
        follow(&s.state, "acme-1", third, Some(&s.transcript(third))).unwrap();
        let w = s.worker();
        assert_eq!(w.claude_session, third, "NEW has no transcript, OLD does");
        assert_eq!(w.process_session(), OLD);

        follow(&s.state, "acme-1", OLD, Some(&s.transcript(OLD))).unwrap();
        let w = s.worker();
        assert_eq!(w.claude_session, OLD);
        assert_eq!(w.running_session, None, "back on the launched conversation");
    }

    #[test]
    fn a_session_never_started_is_followed_without_a_transcript() {
        let s = setup();
        s.state
            .update("acme-1", |w| w.claude_started = false)
            .unwrap();
        let elsewhere = s.transcripts.with_file_name("other").join("x.jsonl");
        follow(&s.state, "acme-1", NEW, Some(&elsewhere)).unwrap();
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
        follow(&s.state, "acme-1", NEW, Some(&elsewhere)).unwrap();
        assert_eq!(s.worker(), worker("acme-1"));
    }

    #[test]
    fn ignores_unchanged_invalid_and_unknown_ids() {
        let s = setup();
        follow(&s.state, "acme-1", OLD, Some(&s.transcript(OLD))).unwrap();
        follow(&s.state, "acme-1", "", None).unwrap();
        follow(&s.state, "acme-1", "not-a-uuid", None).unwrap();
        assert_eq!(s.worker(), worker("acme-1"));
        follow(&s.state, "acme-2", NEW, None).unwrap();
        assert!(s.state.get("acme-2").unwrap().is_none());
        follow(&s.state, "setup@nobody", NEW, None).unwrap();
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
        follow(&s.state, "setup@acme", NEW, Some(&s.transcript(NEW))).unwrap();
        let session = s.state.project_session("acme").unwrap().unwrap();
        assert_eq!(
            (session.claude_session.as_str(), session.claude_started),
            (NEW, true)
        );
        assert_eq!(session.process_session(), OLD);
        assert_eq!(s.worker(), worker("acme-1"), "workers untouched");
    }
}
