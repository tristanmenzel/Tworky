use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::state::State;

const MAX_LISTED: usize = 30;
const READ_LIMIT: u64 = 2 * 1024 * 1024;
const PROMPT_CHARS: usize = 100;
/// Claude Code shortens a longer transcript folder name and adds a hash, which Tworky can't
/// reproduce.
const MAX_FOLDER_NAME: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conversation {
    pub id: String,
    pub started: Option<String>,
    /// Unix milliseconds of the transcript's last write.
    pub last_active: u64,
    pub prompt: Option<String>,
    pub current: bool,
}

pub fn list(transcripts_dir: &Path, current: &str) -> Vec<Conversation> {
    let Ok(entries) = std::fs::read_dir(transcripts_dir) else {
        return vec![];
    };
    let mut found: Vec<Conversation> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "jsonl" {
                return None;
            }
            let id = path.file_stem()?.to_str()?;
            if !is_session_id(id) {
                return None;
            }
            let meta = entry.metadata().ok().filter(|m| m.is_file())?;
            let last_active = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as u64);
            let (started, prompt) = read_opening(&path)?;
            let current = id == current;
            (prompt.is_some() || current).then(|| Conversation {
                id: id.to_string(),
                started,
                last_active,
                prompt,
                current,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        b.last_active
            .cmp(&a.last_active)
            .then_with(|| a.id.cmp(&b.id))
    });
    if let Some(at) = found.iter().position(|c| c.current)
        && at >= MAX_LISTED
    {
        let current = found.remove(at);
        found.truncate(MAX_LISTED);
        found.push(current);
    } else {
        found.truncate(MAX_LISTED);
    }
    found
}

pub fn is_session_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id)
        .is_ok_and(|uuid| uuid.hyphenated().to_string().eq_ignore_ascii_case(id))
}

/// The transcript's first timestamp and first prompt. A slash command counts only once Claude
/// answers it or the transcript ends there, since built-in commands like `/model` run locally.
fn read_opening(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let mut reader = BufReader::new(File::open(path).ok()?.take(READ_LIMIT));
    let mut started = None;
    let mut command = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(entry) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if started.is_none() {
            started = entry["timestamp"].as_str().map(String::from);
        }
        match classify(&entry) {
            Entry::Prompt(prompt) => return Some((started, Some(prompt))),
            Entry::Command(name) => command = Some(name),
            Entry::LocalOutput => command = None,
            Entry::Reply if command.is_some() => return Some((started, command)),
            Entry::Reply | Entry::Other => {}
        }
    }
    Some((started, command))
}

enum Entry {
    Prompt(String),
    Command(String),
    LocalOutput,
    Reply,
    Other,
}

fn classify(entry: &Value) -> Entry {
    match entry["type"].as_str() {
        Some("assistant") => return Entry::Reply,
        Some("system") => {
            let local = entry["subtype"] == "local_command"
                && entry["content"]
                    .as_str()
                    .is_some_and(|c| c.trim_start().starts_with("<local-command-std"));
            return if local {
                Entry::LocalOutput
            } else {
                Entry::Other
            };
        }
        Some("user") if entry["isMeta"] != true => {}
        _ => return Entry::Other,
    }
    let text = match &entry["message"]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            if parts.iter().any(|p| p["type"] == "tool_result") {
                return Entry::Reply;
            }
            parts
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => return Entry::Other,
    };
    let text = text.trim();
    if text.starts_with("<local-command-") {
        return Entry::LocalOutput;
    }
    if text.starts_with("<bash-stdout>") || text.starts_with("<bash-stderr>") {
        return Entry::Other;
    }
    if let Some(input) = tag(text, "bash-input") {
        return one_line(&format!("! {input}")).map_or(Entry::Other, Entry::Prompt);
    }
    match tag(text, "command-name") {
        Some("/clear") => Entry::Other,
        Some(name) => {
            let command = match tag(text, "command-args") {
                Some(args) if !args.is_empty() => format!("{name} {args}"),
                _ => name.to_string(),
            };
            one_line(&command).map_or(Entry::Other, Entry::Command)
        }
        None => one_line(&strip_tags(text)).map_or(Entry::Other, Entry::Prompt),
    }
}

fn one_line(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then(|| shorten(&line))
}

/// Drops tags such as `<pasted_content source="x">` and `</pasted_content>`, keeping what they wrap.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match tag_len(rest) {
            Some(len) => {
                out.push(' ');
                rest = &rest[len..];
            }
            None => {
                out.push('<');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn tag_len(text: &str) -> Option<usize> {
    let name = text[1..].strip_prefix('/').unwrap_or(&text[1..]);
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return None;
    }
    let after = name.trim_start_matches(|c: char| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
    });
    if !after.starts_with(|c: char| c == '>' || c == '/' || c.is_whitespace()) {
        return None;
    }
    let close = after.find('>')?;
    if after[..close].contains('<') {
        return None;
    }
    Some(text.len() - after.len() + close + 1)
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))?;
    Some(text[start..start + end].trim())
}

fn shorten(line: &str) -> String {
    if line.chars().count() <= PROMPT_CHARS {
        return line.to_string();
    }
    let mut cut: String = line.chars().take(PROMPT_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[derive(Debug, PartialEq, Eq)]
pub enum Followed {
    /// Saved, or nothing to change.
    Yes,
    OtherFolder,
}

/// `id` is a worker or `setup@<project>`. A transcript outside `transcripts_dir` is refused because
/// `--resume` couldn't find it; when either is unknown, the session is followed.
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
fn same_folder_name(actual: &Path, expected: &Path) -> bool {
    match (actual.file_name(), expected.file_name()) {
        (Some(actual), Some(expected)) => {
            let expected = expected.to_string_lossy();
            expected.len() > MAX_FOLDER_NAME
                || actual.to_string_lossy().eq_ignore_ascii_case(&expected)
        }
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
    fn follows_unchecked_when_claude_would_shorten_the_folder_name() {
        let s = setup();
        let expected = s
            .transcripts
            .with_file_name("C".repeat(MAX_FOLDER_NAME + 1));
        let shortened = s
            .transcripts
            .with_file_name(format!("{}-1a2b3c", "C".repeat(MAX_FOLDER_NAME)))
            .join(format!("{NEW}.jsonl"));
        let followed = follow(&s.state, "acme-1", NEW, Some(&shortened), Some(&expected)).unwrap();
        assert_eq!(followed, Followed::Yes);
        assert_eq!(s.worker().claude_session, NEW);

        let expected = s.transcripts.with_file_name("C".repeat(MAX_FOLDER_NAME));
        let third = "33333333-3333-4333-8333-333333333333";
        let elsewhere = s
            .transcripts
            .with_file_name("C--elsewhere")
            .join(format!("{third}.jsonl"));
        let followed =
            follow(&s.state, "acme-1", third, Some(&elsewhere), Some(&expected)).unwrap();
        assert_eq!(
            followed,
            Followed::OtherFolder,
            "a name at the limit is still checked"
        );
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

    fn user(content: Value) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": "2026-11-26T10:00:01.000Z",
            "message": { "role": "user", "content": content },
        })
        .to_string()
    }

    fn write_transcript(dir: &Path, id: &str, lines: &[String], modified_secs: u64) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(modified_secs))
            .unwrap();
    }

    fn id(n: u32) -> String {
        format!("{n:08x}-0000-4000-8000-000000000000")
    }

    fn prompt(lines: &[String]) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        write_transcript(dir.path(), &id(1), lines, 1);
        list(dir.path(), &id(1)).remove(0).prompt
    }

    #[test]
    fn a_post_clear_transcript_shows_the_prompt_after_the_clear() {
        let lines = [
            r#"{"type":"mode","mode":"auto"}"#.to_string(),
            serde_json::json!({
                "type": "user",
                "isMeta": true,
                "timestamp": "2026-11-26T10:00:00.000Z",
                "message": { "role": "user", "content": "<local-command-caveat>Caveat: run directly.</local-command-caveat>" },
            })
            .to_string(),
            user(
                "<command-name>/clear</command-name>\n  <command-message>clear</command-message>\n  <command-args></command-args>"
                    .into(),
            ),
            serde_json::json!({
                "type": "system",
                "subtype": "local_command",
                "content": "<local-command-stdout></local-command-stdout>",
            })
            .to_string(),
            user("<local-command-stdout>done</local-command-stdout>".into()),
            "not json".into(),
            user("Fix   the\nlogin page".into()),
        ];
        let dir = tempfile::tempdir().unwrap();
        write_transcript(dir.path(), &id(1), &lines, 1);
        let listed = list(dir.path(), "");
        assert_eq!(
            listed,
            [Conversation {
                id: id(1),
                started: Some("2026-11-26T10:00:00.000Z".into()),
                last_active: 1000,
                prompt: Some("Fix the login page".into()),
                current: false,
            }]
        );
    }

    #[test]
    fn slash_commands_show_their_name_and_args() {
        assert_eq!(
            prompt(&[user(
                "<command-message>graft:yolo</command-message>\n<command-name>/graft:yolo</command-name>"
                    .into()
            )]),
            Some("/graft:yolo".into())
        );
        assert_eq!(
            prompt(&[user(
                "<command-message>graft:yolo</command-message>\n<command-name>/graft:yolo</command-name>\n<command-args> 45 </command-args>"
                    .into()
            )]),
            Some("/graft:yolo 45".into())
        );
        assert_eq!(
            prompt(&[user(
                "<command-name>/review</command-name>\n<command-message>review</command-message>\n<command-args>  </command-args>"
                    .into()
            )]),
            Some("/review".into())
        );
        assert_eq!(
            prompt(&[user(
                "<command-name>/review</command-name>\n<command-message>review</command-message>\n<command-args>pr 12</command-args>"
                    .into()
            )]),
            Some("/review pr 12".into())
        );
    }

    fn command(name: &str, args: &str) -> String {
        user(
            format!(
                "<command-name>{name}</command-name>
<command-message>{}</command-message>
<command-args>{args}</command-args>",
                name.trim_start_matches('/')
            )
            .into(),
        )
    }

    fn assistant() -> String {
        serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "On it" }] },
        })
        .to_string()
    }

    #[test]
    fn skips_slash_commands_that_ran_locally() {
        let stdout = |text: &str| {
            user(format!("<local-command-stdout>{text}</local-command-stdout>").into())
        };
        let system_stdout = serde_json::json!({
            "type": "system",
            "subtype": "local_command",
            "content": "<local-command-stdout>Set model to opus</local-command-stdout>",
        })
        .to_string();
        assert_eq!(
            prompt(&[
                command("/login", ""),
                stdout("Login successful"),
                command("/model", "opus"),
                system_stdout,
                command("/config", ""),
                stdout("Config dialog dismissed"),
                user("Fix the build".into()),
            ]),
            Some("Fix the build".into())
        );
        assert_eq!(
            prompt(&[
                command("/model", ""),
                stdout("Kept model"),
                command("/graft:orchestrate", "45"),
                serde_json::json!({
                    "type": "user",
                    "isMeta": true,
                    "message": { "content": [{ "type": "text", "text": "skill body" }] },
                })
                .to_string(),
                assistant(),
                user("later prompt".into()),
            ]),
            Some("/graft:orchestrate 45".into())
        );
        assert_eq!(
            prompt(&[command("/model", ""), user("Fix the build".into())]),
            Some("Fix the build".into()),
            "a command nobody answered before the next prompt"
        );
    }

    #[test]
    fn shows_bash_mode_input_as_a_bang_command() {
        assert_eq!(
            prompt(&[
                user("<bash-input>git status</bash-input>".into()),
                user("<bash-stdout>clean</bash-stdout><bash-stderr></bash-stderr>".into()),
            ]),
            Some("! git status".into())
        );
    }

    #[test]
    fn keeps_the_text_inside_wrapper_tags() {
        assert_eq!(
            prompt(&[user(
                "Review this:
<pasted_content source=\"clipboard\" lines=\"3\">fn main() {}</pasted_content>
if a < b and <Button> stays"
                    .into()
            )]),
            Some("Review this: fn main() {} if a < b and <Button> stays".into())
        );
    }

    #[test]
    fn reads_a_prompt_from_text_parts() {
        assert_eq!(
            prompt(&[user(serde_json::json!([
                { "type": "text", "text": "Look at" },
                { "type": "image", "source": {} },
                { "type": "text", "text": "this\nscreenshot" },
            ]))]),
            Some("Look at this screenshot".into())
        );
    }

    #[test]
    fn skips_meta_entries_and_tool_results() {
        let meta = serde_json::json!({
            "type": "user",
            "isMeta": true,
            "message": { "content": [{ "type": "text", "text": "Base directory for this skill" }] },
        })
        .to_string();
        let tool_result = user(serde_json::json!([
            { "type": "tool_result", "tool_use_id": "t", "content": "ok" },
        ]));
        let stderr = user("<local-command-stderr>oops</local-command-stderr>".into());
        assert_eq!(
            prompt(&[meta, tool_result, stderr, user("real".into())]),
            Some("real".into())
        );
    }

    #[test]
    fn cuts_a_long_prompt_to_one_short_line() {
        let long = "é".repeat(150);
        let shown = prompt(&[user(long.into())]).unwrap();
        assert_eq!(shown.chars().count(), PROMPT_CHARS);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn lists_only_transcripts_named_by_a_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let lines = [user("hi".into())];
        write_transcript(dir.path(), &id(1), &lines, 1);
        write_transcript(dir.path(), "notes", &lines, 1);
        write_transcript(dir.path(), &id(2).replace('-', ""), &lines, 1);
        std::fs::write(dir.path().join(format!("{}.json", id(3))), "").unwrap();
        write_transcript(&dir.path().join(id(4)), &id(5), &lines, 1);
        let ids: Vec<String> = list(dir.path(), "").into_iter().map(|c| c.id).collect();
        assert_eq!(ids, [id(1)]);
    }

    #[test]
    fn hides_a_transcript_with_no_prompt_unless_current() {
        let dir = tempfile::tempdir().unwrap();
        let lines = [r#"{"type":"mode","mode":"auto"}"#.to_string()];
        write_transcript(dir.path(), &id(1), &lines, 1);
        assert!(list(dir.path(), &id(2)).is_empty());
        let current = list(dir.path(), &id(1));
        assert_eq!(
            current,
            [Conversation {
                id: id(1),
                started: None,
                last_active: 1000,
                prompt: None,
                current: true,
            }]
        );
    }

    #[test]
    fn lists_the_newest_first_and_keeps_the_current_beyond_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let lines = [user("hi".into())];
        for n in 1..=40 {
            write_transcript(dir.path(), &id(n), &lines, u64::from(n));
        }
        let listed = list(dir.path(), &id(10));
        assert_eq!(listed.len(), MAX_LISTED + 1);
        assert_eq!(listed[0].id, id(40));
        assert_eq!(listed[MAX_LISTED - 1].id, id(11));
        assert_eq!(listed[MAX_LISTED].id, id(10));
        assert!(listed[MAX_LISTED].current);

        let listed = list(dir.path(), &id(40));
        assert_eq!(listed.len(), MAX_LISTED);
        assert!(listed[0].current);
        assert!(
            listed
                .windows(2)
                .all(|w| w[0].last_active > w[1].last_active)
        );
    }

    #[test]
    fn lists_nothing_without_a_transcripts_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(&dir.path().join("missing"), "").is_empty());
    }
}
