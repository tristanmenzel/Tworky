use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
pub fn global_config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join(".claude.json"));
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join(".claude.json"))
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`: where Claude Code keeps `projects/<dir>/<session>.jsonl`.
pub fn claude_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join(".claude"))
}

/// The transcript `claude --resume <session>` needs.
pub fn transcript_path(claude_home: &Path, cwd: &Path, session: &str) -> PathBuf {
    transcripts_dir(claude_home, cwd).join(format!("{session}.jsonl"))
}

/// Where the transcripts of sessions started in `cwd` live. Claude Code names the directory after
/// the working directory with every non-alphanumeric character replaced by `-`.
pub fn transcripts_dir(claude_home: &Path, cwd: &Path) -> PathBuf {
    let dir: String = cwd
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    claude_home.join("projects").join(dir)
}

/// The output files of the commands Claude ran in the background, read from session transcripts.
/// Each transcript is read incrementally, from where the last call stopped.
#[derive(Default)]
pub struct BackgroundTasks {
    files: HashMap<PathBuf, TranscriptTasks>,
}

/// Bash and PowerShell tool output files, over every transcript in a folder. Files that no longer
/// exist are left out.
#[derive(Debug, Default)]
pub struct TaskOutputs {
    /// The output file of the latest background run of each Bash command.
    pub by_command: HashMap<String, PathBuf>,
    /// The PowerShell command writing to each output file. A PowerShell job is found by the file
    /// its launcher redirects to, since its command line doesn't show the command.
    pub powershell: HashMap<PathBuf, String>,
}

#[derive(Default)]
struct TranscriptTasks {
    offset: u64,
    modified: Option<std::time::SystemTime>,
    /// Bash and PowerShell tool calls awaiting their result, by tool use id.
    pending: HashMap<String, (Tool, String)>,
    outputs: HashMap<String, PathBuf>,
    powershell: HashMap<PathBuf, String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Bash,
    PowerShell,
}

impl BackgroundTasks {
    pub fn outputs(&mut self, dir: &Path) -> TaskOutputs {
        let mut transcripts: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let path = e.ok()?.path();
                let modified = path.metadata().ok()?.modified().ok()?;
                (path.extension()? == "jsonl").then_some((modified, path))
            })
            .collect();
        transcripts.sort();
        let mut out = TaskOutputs::default();
        for (modified, path) in transcripts {
            let tasks = self.files.entry(path.clone()).or_default();
            if tasks.modified != Some(modified) {
                tasks.modified = Some(modified);
                let _ = tasks.read_from(&path);
            }
            out.by_command.extend(
                tasks
                    .outputs
                    .iter()
                    .filter(|(_, file)| file.exists())
                    .map(|(c, f)| (c.clone(), f.clone())),
            );
            out.powershell.extend(
                tasks
                    .powershell
                    .iter()
                    .filter(|(file, _)| file.exists())
                    .map(|(f, c)| (f.clone(), c.clone())),
            );
        }
        out
    }
}

impl TranscriptTasks {
    fn read_from(&mut self, path: &Path) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        // A line still being written is read again next time.
        let complete = text.rfind('\n').map_or(0, |i| i + 1);
        for line in text[..complete].lines() {
            self.read_line(line);
        }
        self.offset += complete as u64;
        Ok(())
    }

    fn read_line(&mut self, line: &str) {
        if !line.contains("\"tool_use") && !line.contains("backgroundTaskId") {
            return;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let content = entry["message"]["content"].as_array();
        for item in content.into_iter().flatten() {
            match item["type"].as_str() {
                Some("tool_use") => {
                    let tool = match item["name"].as_str() {
                        Some("Bash") => Tool::Bash,
                        Some("PowerShell") => Tool::PowerShell,
                        _ => continue,
                    };
                    if let (Some(id), Some(command)) =
                        (item["id"].as_str(), item["input"]["command"].as_str())
                    {
                        self.pending.insert(id.into(), (tool, command.into()));
                    }
                }
                Some("tool_result") => {
                    let Some((tool, command)) = item["tool_use_id"]
                        .as_str()
                        .and_then(|id| self.pending.remove(id))
                    else {
                        continue;
                    };
                    let Some(output) = output_file(&item["content"]) else {
                        continue;
                    };
                    match tool {
                        Tool::Bash
                            if entry["toolUseResult"]["backgroundTaskId"]
                                .as_str()
                                .is_some() =>
                        {
                            self.outputs.insert(command, output);
                        }
                        Tool::Bash => {}
                        Tool::PowerShell => {
                            self.powershell.insert(output, command);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Output read from a background task's file, and where to continue from next time.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OutputChunk {
    pub text: String,
    pub offset: u64,
}

/// How much of an output file a first read shows, and the most one read returns.
const OUTPUT_TAIL: u64 = 256 * 1024;

/// Reads a background task's output from `offset`, or the last part of it when `offset` is
/// `None`. Only files Claude Code writes task output to are read. A character split by the end of
/// the read is left for the next one.
pub fn read_output(path: &Path, offset: Option<u64>) -> Result<OutputChunk> {
    use std::io::{Read, Seek, SeekFrom};

    let tasks_root = std::env::temp_dir().join("claude");
    let task_output = path.extension().is_some_and(|e| e == "output")
        && path.starts_with(&tasks_root)
        && !path.components().any(|c| c == Component::ParentDir);
    let service_log =
        || crate::config::Paths::resolve().is_ok_and(|paths| crate::services::is_log(&paths, path));
    if !task_output && !service_log() {
        bail!(
            "{} is not a Claude task output file or service log",
            path.display()
        );
    }
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut start = offset.unwrap_or(len.saturating_sub(OUTPUT_TAIL));
    if start > len {
        start = 0;
    }
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(OUTPUT_TAIL).read_to_end(&mut bytes)?;
    let valid = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => bytes.len(),
    };
    Ok(OutputChunk {
        text: String::from_utf8_lossy(&bytes[..valid]).into_owned(),
        offset: start + valid as u64,
    })
}

/// The result of a backgrounded command says "Output is being written to: <path>.output".
fn output_file(content: &Value) -> Option<PathBuf> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let start = text.find("written to: ")? + "written to: ".len();
    let end = start + text[start..].find(".output")? + ".output".len();
    Some(PathBuf::from(&text[start..end]))
}

/// Claude Code keys projects by absolute path with forward slashes, e.g. `C:/work/acme/327-x`.
pub fn project_key(dir: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in dir.components() {
        match c {
            Component::Prefix(p) => parts.push(p.as_os_str().to_string_lossy().into_owned()),
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
        }
    }
    if dir.has_root() && !matches!(dir.components().next(), Some(Component::Prefix(_))) {
        format!("/{}", parts.join("/"))
    } else {
        parts.join("/")
    }
}

/// Marks `dir` as trusted so the session opens without the "trust this folder" prompt.
/// Leaves every other field untouched, and writes only when something changes.
pub fn trust_directory(config: &Path, dir: &Path) -> Result<bool> {
    let mut value: Value = match std::fs::read_to_string(config) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} does not parse; leaving it alone", config.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", config.display())),
    };
    let Some(root) = value.as_object_mut() else {
        bail!("{} is not a JSON object", config.display());
    };
    let projects = root.entry("projects").or_insert_with(|| json!({}));
    let Some(projects) = projects.as_object_mut() else {
        bail!("`projects` in {} is not an object", config.display());
    };
    let entry = projects
        .entry(project_key(dir))
        .or_insert_with(|| json!({ "allowedTools": [] }));
    if entry["hasTrustDialogAccepted"] == json!(true) {
        return Ok(false);
    }
    entry["hasTrustDialogAccepted"] = json!(true);
    let tmp = config.with_extension("json.tworky.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&value)?)?;
    std::fs::rename(&tmp, config)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_claude_code_format() {
        if cfg!(windows) {
            assert_eq!(
                project_key(Path::new(r"C:\work\acme\327-x\.")),
                "C:/work/acme/327-x"
            );
        } else {
            assert_eq!(
                project_key(Path::new("/work/acme/327-x/.")),
                "/work/acme/327-x"
            );
        }
    }

    #[test]
    fn maps_background_commands_to_their_output_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("b1.output");
        std::fs::write(&out, "ready\n").unwrap();
        let use_line = |id: &str, command: &str, background: bool| {
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id,
                "name": "Bash", "input": {"command": command, "run_in_background": background}}]}})
            .to_string()
        };
        let result_line = |id: &str, task: Option<&str>| {
            let mut entry = json!({"type": "user", "message": {"content": [{"type": "tool_result",
                "tool_use_id": id, "content": format!(
                    "Command running in background with ID: b1. Output is being written to: {}. You will be notified.",
                    out.display())}]}});
            if let Some(task) = task {
                entry["toolUseResult"] = json!({"backgroundTaskId": task});
            }
            entry.to_string()
        };
        let transcript = dir.path().join("s.jsonl");
        let lines = [
            use_line("t1", "npm run dev", true),
            result_line("t1", Some("b1")),
            use_line("t2", "git status", false),
            result_line("t2", None),
        ];
        std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();

        let mut tasks = BackgroundTasks::default();
        let outputs = tasks.outputs(dir.path()).by_command;
        assert_eq!(outputs.get("npm run dev"), Some(&out));
        assert_eq!(outputs.len(), 1);

        std::fs::remove_file(&out).unwrap();
        assert!(
            tasks.outputs(dir.path()).by_command.is_empty(),
            "a deleted output file is left out"
        );
    }

    #[test]
    fn maps_powershell_output_files_to_their_commands() {
        let dir = tempfile::tempdir().unwrap();
        let file = |id: &str| {
            let path = dir.path().join(format!("{id}.output"));
            std::fs::write(&path, "").unwrap();
            path
        };
        let (dev, test) = (file("b1"), file("b2"));
        let tool_use = |id: &str, command: &str| {
            json!({"type": "tool_use", "id": id, "name": "PowerShell",
                "input": {"command": command, "run_in_background": true}})
        };
        let result = |id: &str, out: &Path| {
            json!({"type": "user", "message": {"content": [{"type": "tool_result",
                "tool_use_id": id, "content": format!(
                    "Command running in background with ID: x. Output is being written to: {}. You will be notified when it completes.",
                    out.display())}]}, "toolUseResult": {"backgroundTaskId": "x"}})
            .to_string()
        };
        let lines = [
            json!({"type": "assistant", "message": {"content": [
                tool_use("t1", "npm run dev"), tool_use("t2", "npm test")]}})
            .to_string(),
            result("t2", &test),
            result("t1", &dev),
        ];
        std::fs::write(dir.path().join("s.jsonl"), lines.join("\n") + "\n").unwrap();

        let outputs = BackgroundTasks::default().outputs(dir.path());
        assert_eq!(
            outputs.powershell,
            HashMap::from([(dev, "npm run dev".into()), (test, "npm test".into())])
        );
        assert!(outputs.by_command.is_empty());
    }

    #[test]
    fn transcript_path_matches_claude_code_layout() {
        let p = transcript_path(
            Path::new(r"C:\Users\t\.claude"),
            Path::new(r"C:\work\acme\42-fix-header-on-mobile"),
            "a60d7237",
        );
        assert!(
            p.ends_with(r"projects\C--work-acme-42-fix-header-on-mobile\a60d7237.jsonl")
                || p.ends_with("projects/C--work-acme-42-fix-header-on-mobile/a60d7237.jsonl"),
            "{}",
            p.display()
        );
    }

    #[test]
    fn trusts_without_disturbing_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".claude.json");
        std::fs::write(
            &config,
            r#"{"zeta":1,"projects":{"C:/other":{"hasTrustDialogAccepted":true,"lastCost":2}},"alpha":2}"#,
        )
        .unwrap();
        let target = dir.path().join("worker");
        assert!(trust_directory(&config, &target).unwrap());
        assert!(!trust_directory(&config, &target).unwrap());
        let text = std::fs::read_to_string(&config).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["projects"][project_key(&target)]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(v["projects"]["C:/other"]["lastCost"], 2);
        assert!(
            text.find("zeta").unwrap() < text.find("alpha").unwrap(),
            "key order kept"
        );
    }

    #[test]
    fn refuses_corrupt_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".claude.json");
        std::fs::write(&config, "{oops").unwrap();
        assert!(trust_directory(&config, dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "{oops");
    }
}
