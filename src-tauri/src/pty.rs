use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tauri::ipc::Channel;
use tworky_core::worker::Launch;

use crate::keep_awake::Ticks;
use crate::title::TitleTracker;

const SCROLLBACK_LIMIT: usize = 512 * 1024;

#[derive(Default)]
struct Shared {
    scrollback: String,
    channel: Option<Channel<String>>,
    /// Queries in output no view has seen yet are still unanswered; ConPTY waits for the reply to
    /// its opening cursor position query before the program can start.
    attached: bool,
    alive: bool,
    last_output: Option<std::time::Instant>,
}

struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    shared: Arc<Mutex<Shared>>,
    generation: u64,
}

struct SessionTicks {
    generation: u64,
    ticks: Option<Ticks>,
}

/// Each live session's spinner ticks, apart from `inner` so the keep awake controller never waits
/// behind a pty write that blocks. An entry is started, and replaced, only under the `inner` lock,
/// so its generation is always that of the session in `inner`. Ticks and ends from any other
/// generation are ignored, so an old reader finishing after a respawn leaves the new entry alone.
#[derive(Default, Clone)]
struct TickBoard(Arc<Mutex<HashMap<String, SessionTicks>>>);

impl TickBoard {
    fn start(&self, id: &str, generation: u64) {
        self.0.lock().unwrap().insert(
            id.to_string(),
            SessionTicks {
                generation,
                ticks: None,
            },
        );
    }

    fn tick(&self, id: &str, generation: u64, now: std::time::Instant) {
        if let Some(s) = self.0.lock().unwrap().get_mut(id)
            && s.generation == generation
        {
            s.ticks = Some(Ticks::after(s.ticks, now));
        }
    }

    fn end(&self, id: &str, generation: u64) {
        let mut board = self.0.lock().unwrap();
        if board.get(id).is_some_and(|s| s.generation == generation) {
            board.remove(id);
        }
    }

    fn current(&self) -> HashMap<String, Ticks> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, s)| Some((id.clone(), s.ticks?)))
            .collect()
    }
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, Session>>,
    ticks: TickBoard,
    generations: AtomicU64,
}

impl Sessions {
    pub fn is_alive(&self, id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|s| s.shared.lock().unwrap().alive)
    }

    /// How long a live session has written nothing to its terminal.
    pub fn silent_for(&self, id: &str) -> Option<std::time::Duration> {
        let sessions = self.inner.lock().unwrap();
        let s = sessions.get(id)?.shared.lock().unwrap();
        s.alive.then(|| s.last_output.map(|t| t.elapsed()))?
    }

    pub fn spawn(
        &self,
        id: &str,
        launch: &Launch,
        cols: u16,
        rows: u16,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        let pty = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(&launch.program);
        cmd.args(&launch.args);
        cmd.cwd(&launch.cwd);
        for (key, _) in std::env::vars_os() {
            if inherited_but_unwanted(&key.to_string_lossy()) {
                cmd.env_remove(&key);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in &launch.env {
            cmd.env(k, v);
        }
        let child = pty
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {} in {}", launch.program, launch.cwd.display()))?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        let shared = Arc::new(Mutex::new(Shared {
            alive: true,
            ..Default::default()
        }));

        let thread_shared = shared.clone();
        let board = self.ticks.clone();
        let thread_id = id.to_string();
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let previous = {
            let mut inner = self.inner.lock().unwrap();
            board.start(id, generation);
            inner.insert(
                id.to_string(),
                Session {
                    master: pty.master,
                    writer,
                    child,
                    shared,
                    generation,
                },
            )
        };
        if let Some(mut old) = previous {
            let _ = old.child.kill();
        }
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            let mut pending: Vec<u8> = Vec::new();
            let mut titles = TitleTracker::default();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                pending.extend_from_slice(&buf[..n]);
                let text = take_utf8(&mut pending);
                if text.is_empty() {
                    continue;
                }
                let now = std::time::Instant::now();
                if titles.feed(text.as_bytes()) {
                    board.tick(&thread_id, generation, now);
                }
                let mut s = thread_shared.lock().unwrap();
                s.last_output = Some(now);
                s.scrollback.push_str(&text);
                trim_front(&mut s.scrollback, SCROLLBACK_LIMIT);
                if let Some(ch) = &s.channel
                    && ch.send(text).is_err()
                {
                    s.channel = None;
                }
            }
            thread_shared.lock().unwrap().alive = false;
            board.end(&thread_id, generation);
            on_exit();
        });
        Ok(())
    }

    /// Replays the scrollback into `channel`, then streams live output to it. Only one view is attached at a time.
    pub fn attach(&self, id: &str, channel: Channel<String>) -> Result<()> {
        let sessions = self.inner.lock().unwrap();
        let session = sessions.get(id).context("no session")?;
        let mut s = session.shared.lock().unwrap();
        if !s.scrollback.is_empty() {
            let replay = if s.attached {
                strip_terminal_queries(&s.scrollback)
            } else {
                s.scrollback.clone()
            };
            channel.send(replay)?;
        }
        s.attached = true;
        s.channel = Some(channel);
        Ok(())
    }

    pub fn write(&self, id: &str, data: &str) -> Result<()> {
        let mut sessions = self.inner.lock().unwrap();
        let session = sessions.get_mut(id).context("no session")?;
        session.writer.write_all(data.as_bytes())?;
        session.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let sessions = self.inner.lock().unwrap();
        let session = sessions.get(id).context("no session")?;
        session.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub fn pid(&self, id: &str) -> Option<u32> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| s.shared.lock().unwrap().alive)
            .and_then(|s| s.child.process_id())
    }

    pub fn kill(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(mut s) = inner.remove(id) {
            self.ticks.end(id, s.generation);
            let _ = s.child.kill();
        }
    }

    pub fn live(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.shared.lock().unwrap().alive)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn ticks(&self) -> HashMap<String, Ticks> {
        self.ticks.current()
    }

    pub fn kill_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        self.ticks.0.lock().unwrap().clear();
        for (_, mut s) in inner.drain() {
            let _ = s.child.kill();
        }
    }
}

/// The app may itself be launched from a Claude Code session (or a shell with NO_COLOR set);
/// those variables would make the hosted `claude` drop colour or think it is a nested child session.
fn inherited_but_unwanted(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key == "NO_COLOR"
        || key == "FORCE_COLOR"
        || key == "CLAUDECODE"
        || key.starts_with("CLAUDE_CODE_")
}

/// Replayed output must not contain queries (cursor position, device attributes, colour), because
/// xterm.js would answer them again and the answers would arrive at the program as typed input.
fn strip_terminal_queries(text: &str) -> String {
    const QUERIES: &[&str] = &[
        "\x1b[6n",
        "\x1b[?6n",
        "\x1b[c",
        "\x1b[0c",
        "\x1b[>c",
        "\x1b[>0c",
        "\x1b[>q",
        "\x1b]10;?\x07",
        "\x1b]11;?\x07",
        "\x1b]10;?\x1b\\",
        "\x1b]11;?\x1b\\",
    ];
    let mut out = text.to_string();
    for q in QUERIES {
        if out.contains(q) {
            out = out.replace(q, "");
        }
    }
    out
}

/// Input xterm.js sends by itself rather than for a key the user pressed: focus changes, answers to
/// cursor position, device attribute, mode and colour queries, version replies and SGR mouse reports.
pub fn is_terminal_report(data: &str) -> bool {
    let mut rest = data;
    while !rest.is_empty() {
        let Some(seq) = rest.strip_prefix('\x1b') else {
            return false;
        };
        rest = if let Some(body) = seq.strip_prefix('[') {
            let Some(end) = body.find(|c: char| ('\x40'..='\x7e').contains(&c)) else {
                return false;
            };
            let (params, final_byte) = (&body[..end], &body[end..=end]);
            let report = match final_byte {
                "I" | "O" => params.is_empty(),
                "R" | "c" | "n" => true,
                "y" => params.ends_with('$'),
                "M" | "m" => params.starts_with('<'),
                _ => false,
            };
            if !report {
                return false;
            }
            &body[end + 1..]
        } else if let Some(body) = seq.strip_prefix(']').or_else(|| seq.strip_prefix('P')) {
            match (body.find('\x07'), body.find("\x1b\\")) {
                (Some(bel), Some(st)) if st < bel => &body[st + 2..],
                (Some(bel), _) => &body[bel + 1..],
                (None, Some(st)) => &body[st + 2..],
                (None, None) => return false,
            }
        } else {
            return false;
        };
    }
    true
}

/// Splits off the longest valid UTF-8 prefix, leaving an incomplete trailing sequence for the next read.
fn take_utf8(pending: &mut Vec<u8>) -> String {
    match std::str::from_utf8(pending) {
        Ok(s) => {
            let s = s.to_string();
            pending.clear();
            s
        }
        Err(e) if e.error_len().is_none() => {
            let valid = e.valid_up_to();
            let s = String::from_utf8_lossy(&pending[..valid]).into_owned();
            pending.drain(..valid);
            s
        }
        Err(_) => {
            let s = String::from_utf8_lossy(pending).into_owned();
            pending.clear();
            s
        }
    }
}

fn trim_front(s: &mut String, limit: usize) {
    if s.len() <= limit {
        return;
    }
    let mut cut = s.len() - limit;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    s.drain(..cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tick_board_follows_only_the_current_generation() {
        let board = TickBoard::default();
        let now = std::time::Instant::now();
        board.start("acme-1", 0);
        assert!(board.current().is_empty(), "no ticks yet");
        board.tick("acme-1", 0, now);
        assert_eq!(board.current()["acme-1"].last, now);

        board.start("acme-1", 1);
        assert!(board.current().is_empty(), "a respawn starts afresh");
        board.tick("acme-1", 0, now);
        assert!(
            board.current().is_empty(),
            "the old reader's ticks are ignored"
        );
        board.end("acme-1", 0);
        let later = now + std::time::Duration::from_secs(1);
        board.tick("acme-1", 1, later);
        assert_eq!(
            board.current()["acme-1"].last,
            later,
            "the old reader's end leaves the new entry"
        );

        board.end("acme-1", 1);
        assert!(board.current().is_empty());
        board.tick("acme-1", 1, later);
        assert!(board.current().is_empty(), "no entry after its end");
    }

    #[test]
    fn keeps_split_multibyte_sequences() {
        let bytes = "a→b".as_bytes();
        let mut pending = bytes[..2].to_vec();
        assert_eq!(take_utf8(&mut pending), "a");
        assert_eq!(pending.len(), 1);
        pending.extend_from_slice(&bytes[2..]);
        assert_eq!(take_utf8(&mut pending), "→b");
        assert!(pending.is_empty());
    }

    #[test]
    fn replay_drops_queries_but_keeps_colour() {
        let text = "\x1b[6n\x1b[38;2;215;119;87mhi\x1b[m\x1b]11;?\x07";
        assert_eq!(
            strip_terminal_queries(text),
            "\x1b[38;2;215;119;87mhi\x1b[m"
        );
    }

    #[test]
    fn tells_terminal_reports_from_keys() {
        for report in [
            "\x1b[I",
            "\x1b[O",
            "\x1b[12;1R",
            "\x1b[?1;2c",
            "\x1b[?2004;1$y",
            "\x1b[<0;10;5M",
            "\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\",
            "\x1bP>|xterm.js(5.5.0)\x1b\\",
            "\x1b[O\x1b[I",
        ] {
            assert!(is_terminal_report(report), "{report:?}");
        }
        for key in [
            "1", "\r", "\x1b", "\x1b[A", "\x1b[B", "\x1b[Z", "\x1b[3~", "y", "\x1b[Ia",
        ] {
            assert!(!is_terminal_report(key), "{key:?}");
        }
    }

    #[test]
    fn drops_inherited_claude_and_colour_overrides() {
        for key in [
            "NO_COLOR",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "claude_code_entrypoint",
        ] {
            assert!(inherited_but_unwanted(key), "{key}");
        }
        for key in ["PATH", "CLAUDE_CONFIG_DIR", "ANTHROPIC_API_KEY"] {
            assert!(!inherited_but_unwanted(key), "{key}");
        }
    }

    #[test]
    fn trims_on_char_boundary() {
        let mut s = "→→→".to_string();
        trim_front(&mut s, 4);
        assert_eq!(s, "→");
    }
}
