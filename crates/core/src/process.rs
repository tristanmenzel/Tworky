use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};

pub fn command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the GUI app has no console, so every child would otherwise flash one.
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Windows gives a child every inheritable handle this process holds, not just the stdio set on its
/// `Command`. That includes the stdout and stderr this process was started with, so a long-running
/// child (a service) would hold the caller's pipe open, and `tw service start | cat` would never end.
/// Children still get the standard handles they're given: std passes each one an inheritable copy.
pub fn keep_std_handles_from_children() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain Win32 calls on this process's own standard handles.
            unsafe {
                let handle = GetStdHandle(which);
                if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
}

/// Spawns a process that keeps running after this one exits, including out of a kill-on-close job.
pub fn spawn_detached(mut cmd: Command) -> std::io::Result<std::process::Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            // A job that doesn't allow breakaway refuses the flag outright.
            Err(e) if e.raw_os_error() == Some(5) => {
                cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
                cmd.spawn()
            }
            result => result,
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        cmd.spawn()
    }
}

pub fn run(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = command(program);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    output(cmd, &describe(program, args))
}

pub fn output(mut cmd: Command, label: &str) -> Result<String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("starting `{label}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!("`{label}` failed ({}): {}", out.status, detail.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// On timeout, stops the command's whole process tree.
pub fn output_timeout(mut cmd: Command, label: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `{label}`"))?;
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    let pipes = [
        child
            .stdout
            .take()
            .map(|s| (false, Box::new(s) as Box<dyn std::io::Read + Send>)),
        child
            .stderr
            .take()
            .map(|s| (true, Box::new(s) as Box<dyn std::io::Read + Send>)),
    ];
    for (is_stderr, mut pipe) in pipes.into_iter().flatten() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = tx.send((is_stderr, bytes));
        });
    }
    drop(tx);
    let (mut stdout, mut stderr, mut status) = (None, None, None);
    while stdout.is_none() || stderr.is_none() || status.is_none() {
        if status.is_none() {
            status = child
                .try_wait()
                .with_context(|| format!("waiting for `{label}`"))?;
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok((true, bytes)) => stderr = Some(bytes),
            Ok((false, bytes)) => stdout = Some(bytes),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(20))
            }
        }
        if Instant::now() >= deadline {
            // The tree first: a grandchild (docker's compose plugin) holds the pipes, and once the
            // child is gone nothing links the grandchild back to it.
            let _ = stop_tree(child.id(), &program, &mut |_| {});
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{label}` timed out after {}s", timeout.as_secs_f32());
        }
    }
    let (stdout, stderr) = (stdout.unwrap_or_default(), stderr.unwrap_or_default());
    let status = status.expect("the loop ends once the command has exited");
    if !status.success() {
        let detail = if String::from_utf8_lossy(&stderr).trim().is_empty() {
            String::from_utf8_lossy(&stdout)
        } else {
            String::from_utf8_lossy(&stderr)
        };
        bail!("`{label}` failed ({status}): {}", detail.trim());
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Runs a command, sending each stdout/stderr line to `log` as it arrives.
pub fn stream(mut cmd: Command, label: &str, log: &mut dyn FnMut(&str)) -> Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `{label}`"))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|pipe| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    })
    .collect();
    drop(tx);
    for line in rx {
        log(&line);
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("`{label}` failed ({status})");
    }
    Ok(())
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProcessInfo {
    process_id: u32,
    #[serde(default)]
    parent_process_id: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    command_line: Option<String>,
    #[serde(default)]
    executable_path: Option<String>,
    #[serde(skip)]
    cwd: Option<String>,
    #[serde(default)]
    working_set_size: Option<u64>,
    #[serde(default)]
    age_secs: Option<u64>,
}

#[cfg(windows)]
fn snapshot() -> Result<Vec<ProcessInfo>> {
    let json = run(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "@(Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,CommandLine,ExecutablePath,WorkingSetSize,\
@{n='AgeSecs';e={try { [int64]((Get-Date) - $_.CreationDate).TotalSeconds } catch { $null }}}) | ConvertTo-Json -Compress",
        ],
        None,
    )?;
    serde_json::from_str(json.trim()).context("parsing the process list")
}

#[cfg(unix)]
fn snapshot() -> Result<Vec<ProcessInfo>> {
    let out = run("ps", &["-axww", "-o", "pid=,ppid=,rss=,etime=,args="], None)?;
    let mut processes = parse_ps(&out);
    let cwds = working_dirs();
    for p in &mut processes {
        p.cwd = cwds.get(&p.process_id).cloned();
    }
    Ok(processes)
}

#[cfg(any(unix, test))]
fn parse_ps(out: &str) -> Vec<ProcessInfo> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let process_id = fields.next()?.parse().ok()?;
            let parent_process_id = fields.next()?.parse().ok()?;
            let rss_kb: u64 = fields.next()?.parse().ok()?;
            let age_secs = parse_etime(fields.next()?);
            let args: Vec<&str> = fields.collect();
            let name = args
                .first()
                .map(|a| a.rsplit('/').next().unwrap_or(a).to_string());
            Some(ProcessInfo {
                process_id,
                parent_process_id,
                name,
                command_line: Some(args.join(" ")),
                working_set_size: Some(rss_kb * 1024),
                age_secs,
                ..Default::default()
            })
        })
        .collect()
}

/// `ps -o etime`: `[[dd-]hh:]mm:ss`.
#[cfg(any(unix, test))]
fn parse_etime(text: &str) -> Option<u64> {
    let (days, clock) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, text),
    };
    let mut secs = 0;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + secs)
}

/// A dev server started as `npm run dev` inside a worktree often has the worktree only as its
/// working directory, not in its arguments.
#[cfg(target_os = "linux")]
fn working_dirs() -> std::collections::HashMap<u32, String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Default::default();
    };
    entries
        .filter_map(|e| {
            let e = e.ok()?;
            let pid = e.file_name().to_str()?.parse().ok()?;
            let cwd = std::fs::read_link(e.path().join("cwd")).ok()?;
            Some((pid, cwd.to_string_lossy().into_owned()))
        })
        .collect()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn working_dirs() -> std::collections::HashMap<u32, String> {
    // lsof exits non-zero when it can't inspect some processes, but still lists the rest.
    let Ok(out) = command("lsof")
        .args(["-n", "-P", "-d", "cwd", "-Fpn"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Default::default();
    };
    parse_lsof(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(any(all(unix, not(target_os = "linux")), test))]
fn parse_lsof(out: &str) -> std::collections::HashMap<u32, String> {
    let mut cwds = std::collections::HashMap::new();
    let mut pid = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(path), Some(p)) = (line.strip_prefix('n'), pid) {
            cwds.insert(p, path.to_string());
        }
    }
    cwds
}

/// `root` and every process below it, parents before children.
fn tree(processes: &[ProcessInfo], root: u32) -> Vec<u32> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        for p in processes {
            if p.parent_process_id == parent
                && p.process_id != parent
                && !out.contains(&p.process_id)
            {
                out.push(p.process_id);
            }
        }
        i += 1;
    }
    out
}

/// `pid` and its parents: `tw worker stop` run by a session's own shell must not stop that shell.
fn ancestry(processes: &[ProcessInfo], pid: u32) -> Vec<u32> {
    let mut out = vec![pid];
    while let Some(parent) = processes
        .iter()
        .find(|p| p.process_id == *out.last().unwrap())
        .map(|p| p.parent_process_id)
        .filter(|parent| *parent != 0 && !out.contains(parent))
    {
        out.push(parent);
    }
    out
}

/// Whether `p` was started from inside `dir`: by its command line, executable or working directory.
fn in_dir(p: &ProcessInfo, dir: &Path, canonical: Option<&Path>) -> bool {
    let mentions = |t: &Option<String>| {
        t.as_deref().is_some_and(|t| {
            mentions_path(t, dir) || canonical.is_some_and(|c| mentions_path(t, c))
        })
    };
    mentions(&p.command_line) || mentions(&p.executable_path) || mentions(&p.cwd)
}

/// Working directories come back with symlinks resolved (macOS reports /var as /private/var).
fn canonical(dir: &Path) -> Option<PathBuf> {
    dir.canonicalize().ok().filter(|c| c != dir)
}

/// Stops the process tree under `root` (a session's `claude`) and anything else started from inside
/// `dir`. Dev servers and test runs a session started outlive it and hold files open; once their
/// parent shell has exited they are no longer in the tree, so they are found by path instead.
pub fn stop_processes(root: Option<u32>, dir: &Path, log: &mut dyn FnMut(&str)) -> Result<()> {
    let processes = snapshot()?;
    let own = ancestry(&processes, std::process::id());
    let mut targets = root.map(|r| tree(&processes, r)).unwrap_or_default();
    let canonical = canonical(dir);
    for p in &processes {
        if in_dir(p, dir, canonical.as_deref()) && !targets.contains(&p.process_id) {
            targets.push(p.process_id);
        }
    }
    let targets: Vec<u32> = targets
        .into_iter()
        .filter(|pid| !own.contains(pid) && processes.iter().any(|p| p.process_id == *pid))
        .collect();
    for pid in &targets {
        let name = processes
            .iter()
            .find(|p| p.process_id == *pid)
            .and_then(|p| p.name.as_deref());
        log(&format!(
            "▸ stopping {} (pid {pid})",
            name.unwrap_or("process")
        ));
    }
    kill(&processes, &targets, &own, log);
    Ok(())
}

/// Windows shows a `"` inside a quoted argument as `\"`, so `node -e "x"` run through a shell appears
/// as `... -Command "node -e \"x\""`. Quotes and backslashes are ignored on both sides.
fn runs_command(command_line: &str, command: &str) -> bool {
    let bare = |s: &str| s.replace(['"', '\\'], "");
    bare(command_line).contains(&bare(command))
}

fn still_runs(processes: &[ProcessInfo], pid: u32, command: &str) -> bool {
    processes.iter().any(|p| {
        p.process_id == pid
            && p.command_line
                .as_deref()
                .is_some_and(|line| runs_command(line, command))
    })
}

/// Stops `pid` and everything under it, if it is still running a command line that contains
/// `command`. Returns whether it was running; the check guards against a reused pid.
pub fn stop_tree(pid: u32, command: &str, log: &mut dyn FnMut(&str)) -> Result<bool> {
    let processes = snapshot()?;
    let own = ancestry(&processes, std::process::id());
    let running = still_runs(&processes, pid, command);
    if running && !own.contains(&pid) {
        kill(&processes, &[pid], &own, log);
    }
    Ok(running)
}

/// A process tree a worker's session left running: a dev server, watcher or test run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Job {
    /// The tree's root, which stopping the job kills along with everything under it.
    pub pid: u32,
    /// The command Claude's Bash tool ran, the first real command under a PowerShell-tool shell,
    /// or else the root's command line.
    pub command: String,
    /// Set when the job is rooted at a shell Claude's Bash or PowerShell tool started.
    pub from_claude: bool,
    /// The task output file a PowerShell-tool shell redirects to.
    #[serde(skip)]
    pub task_output: Option<PathBuf>,
    pub age_secs: u64,
    pub memory_bytes: u64,
    pub process_count: usize,
    /// Every tree the job is made of, `pid` first. See `merge_detached`.
    pub roots: Vec<u32>,
}

/// Jobs younger than this are left out, so a quick `git status` doesn't flash up as a tab.
const MIN_JOB_AGE_SECS: u64 = 10;

/// The label of a PowerShell-tool job that runs no other program, whose launcher's command line
/// doesn't show what it runs.
const POWERSHELL_COMMAND: &str = "PowerShell command";

/// The jobs of each `(worker folder, Claude session id)`, from one process snapshot.
pub fn jobs(workers: &[(PathBuf, String)]) -> Result<Vec<Vec<Job>>> {
    let processes = snapshot()?;
    let own = ancestry(&processes, std::process::id());
    Ok(workers
        .iter()
        .map(|(dir, session)| find_jobs(&processes, dir, session_pid(&processes, session), &own))
        .collect())
}

/// Stops job `pid` and everything under it, refusing a pid that isn't one of the worker's jobs.
pub fn stop_job(dir: &Path, session: &str, pid: u32, log: &mut dyn FnMut(&str)) -> Result<()> {
    let processes = snapshot()?;
    let own = ancestry(&processes, std::process::id());
    let Some(job) = find_jobs(&processes, dir, session_pid(&processes, session), &own)
        .into_iter()
        .find(|j| j.pid == pid)
    else {
        bail!("process {pid} is no longer running in this worker");
    };
    kill(&processes, &job.roots, &own, log);
    Ok(())
}

/// Stops each `(worker folder, Claude session id)`'s `claude` with everything under it, and its
/// jobs, from one process snapshot.
pub fn stop_sessions(sessions: &[(PathBuf, String)], log: &mut dyn FnMut(&str)) -> Result<()> {
    let processes = snapshot()?;
    let own = ancestry(&processes, std::process::id());
    let targets: Vec<u32> = sessions
        .iter()
        .flat_map(|(dir, session)| session_targets(&processes, dir, session, &own))
        .collect();
    kill(&processes, &targets, &own, log);
    Ok(())
}

fn session_targets(processes: &[ProcessInfo], dir: &Path, session: &str, own: &[u32]) -> Vec<u32> {
    let pid = session_pid(processes, session);
    pid.into_iter()
        .chain(
            find_jobs(processes, dir, pid, own)
                .into_iter()
                .flat_map(|j| j.roots),
        )
        .filter(|pid| !own.contains(pid))
        .collect()
}

/// The outermost process whose command line names the session: `claude --resume <id>`, or the
/// script wrapper that started it.
fn session_pid(processes: &[ProcessInfo], session: &str) -> Option<u32> {
    let names = |p: &ProcessInfo| {
        p.command_line
            .as_deref()
            .is_some_and(|l| l.contains(session))
    };
    let parent_names = |p: &ProcessInfo| {
        processes
            .iter()
            .any(|q| q.process_id == p.parent_process_id && names(q))
    };
    (!session.is_empty())
        .then(|| {
            processes
                .iter()
                .find(|p| names(p) && !parent_names(p))
                .map(|p| p.process_id)
        })
        .flatten()
}

/// A job is a tree of processes that are either below a shell started by Claude's Bash or
/// PowerShell tool, or started from inside `dir`: once Claude has exited, its shells' children are
/// orphans found only by path. The session's other children, such as MCP servers, are not jobs.
/// Each tree is reported from its outermost launcher (`npm run`, `sh -c`, ...), so stopping it
/// takes the wrappers too.
fn find_jobs(processes: &[ProcessInfo], dir: &Path, session: Option<u32>, own: &[u32]) -> Vec<Job> {
    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.process_id, p)).collect();
    let canonical = canonical(dir);
    let under_claude_shell = |pid: u32| {
        let chain = ancestry(processes, pid);
        let at = chain.iter().position(|p| Some(*p) == session)?;
        Some(
            chain[..at]
                .iter()
                .any(|p| by_pid.get(p).is_some_and(|p| claude_tool_shell(p))),
        )
    };
    let members: HashSet<u32> = processes
        .iter()
        .filter(|p| {
            !own.contains(&p.process_id)
                && Some(p.process_id) != session
                && under_claude_shell(p.process_id)
                    .unwrap_or_else(|| in_dir(p, dir, canonical.as_deref()))
        })
        .map(|p| p.process_id)
        .collect();
    let mut roots: Vec<u32> = Vec::new();
    for &pid in &members {
        let mut root = pid;
        while let Some(parent) = by_pid
            .get(&root)
            .and_then(|p| by_pid.get(&p.parent_process_id))
            .filter(|parent| {
                parent.process_id != root
                    && !own.contains(&parent.process_id)
                    && Some(parent.process_id) != session
                    && (members.contains(&parent.process_id) || is_launcher(parent))
            })
        {
            root = parent.process_id;
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let trees: Vec<(u32, Vec<u32>)> = roots.iter().map(|&r| (r, tree(processes, r))).collect();
    let jobs: Vec<Job> = trees
        .iter()
        .filter(|(root, _)| {
            !trees
                .iter()
                .any(|(other, t)| other != root && t.contains(root))
        })
        .filter_map(|(root, pids)| {
            let p = by_pid.get(root)?;
            let age_secs = p.age_secs.unwrap_or_default();
            if age_secs < MIN_JOB_AGE_SECS {
                return None;
            }
            let task_output = powershell_task_output(p);
            let command = match &task_output {
                Some(_) => Some(
                    pids.iter()
                        .filter_map(|pid| by_pid.get(pid))
                        .find(|q| !is_launcher(q) && !is_console_host(q))
                        .and_then(|q| q.command_line.clone())
                        .unwrap_or_else(|| POWERSHELL_COMMAND.into()),
                ),
                None => claude_shell_command(p),
            };
            Some(Job {
                pid: *root,
                from_claude: claude_tool_shell(p),
                task_output,
                command: command
                    .or_else(|| p.command_line.clone())
                    .or_else(|| p.name.clone())
                    .unwrap_or_default(),
                age_secs,
                memory_bytes: pids
                    .iter()
                    .filter_map(|pid| by_pid.get(pid)?.working_set_size)
                    .sum(),
                process_count: pids.len(),
                roots: vec![*root],
            })
        })
        .collect();
    let detached = |j: &Job| {
        by_pid
            .get(&j.pid)
            .is_some_and(|p| !by_pid.contains_key(&p.parent_process_id))
    };
    let mut merged: Vec<Job> = Vec::new();
    let (claude, others): (Vec<Job>, Vec<Job>) = jobs.into_iter().partition(|j| j.from_claude);
    merged.extend(claude);
    for job in others {
        let owner = detached(&job)
            .then(|| {
                let run = launched_command(&job.command)?;
                merged.iter_mut().find(|c| {
                    c.from_claude
                        && c.age_secs.abs_diff(job.age_secs) <= 5
                        && c.command.contains(&run)
                })
            })
            .flatten();
        match owner {
            Some(c) => {
                c.memory_bytes += job.memory_bytes;
                c.process_count += job.process_count;
                c.roots.extend(job.roots);
            }
            None => merged.push(job),
        }
    }
    merged.sort_by_key(|j| (std::cmp::Reverse(j.age_secs), j.pid));
    merged
}

/// What a detached launcher runs, without the shell around it: `"C:\...\sh.exe" /c/.../npx expo
/// start` gives `npx expo start`. Git Bash on Windows starts programs through short-lived helper
/// processes, so the tree under a shell Claude started often loses its parent link. Such a tree is
/// merged back into the Claude job that started at the same time and whose command contains this.
fn launched_command(line: &str) -> Option<String> {
    let words: Vec<String> = line
        .split('"')
        .enumerate()
        .flat_map(|(i, part)| {
            if i % 2 == 1 {
                vec![part]
            } else {
                part.split_whitespace().collect()
            }
        })
        .map(|w| match w.strip_prefix('/') {
            Some(flag) if !flag.contains(['/', '\\']) => w.to_string(),
            _ => w.rsplit(['/', '\\']).next().unwrap_or(w).to_string(),
        })
        .collect();
    let shell = |w: &str| {
        let w = w.to_ascii_lowercase();
        matches!(
            w.trim_end_matches(".exe"),
            "sh" | "bash" | "zsh" | "cmd" | "/d" | "/s" | "/c" | "-c"
        )
    };
    let run: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .skip_while(|w| shell(w))
        .collect();
    (run.len() >= 2).then(|| run.join(" "))
}

/// The command run by a shell that Claude Code's Bash tool started. The shell sources a shell
/// snapshot, then `eval`s the command in single quotes, with each `'` written as `'\''`.
fn claude_shell_command(p: &ProcessInfo) -> Option<String> {
    let line = p.command_line.as_deref()?;
    if !line.contains("shell-snapshots") {
        return None;
    }
    let start = line.find("eval '")? + "eval '".len();
    let mut out = String::new();
    let mut rest = &line[start..];
    loop {
        let end = rest.find('\'')?;
        out.push_str(&rest[..end]);
        rest = &rest[end..];
        match rest.strip_prefix(r"'\''") {
            Some(after) => {
                out.push('\'');
                rest = after;
            }
            None => return Some(out),
        }
    }
}

fn claude_tool_shell(p: &ProcessInfo) -> bool {
    claude_shell_command(p).is_some() || powershell_tool_shell(p)
}

/// Claude Code's PowerShell tool runs `cmd /d /s /c "chcp.com 65001 >nul & pwsh ... -Command
/// "$__claudeCodeScript = $env:CLAUDE_CODE_SHELL_LAUNCHER_SCRIPT; ..." > "<tasks>/<id>.output"
/// 2>&1"`: the command itself is passed in that environment variable, so neither process's command
/// line shows it. This is Claude Code's undocumented internals and may change.
fn powershell_tool_shell(p: &ProcessInfo) -> bool {
    p.command_line
        .as_deref()
        .is_some_and(|l| l.contains("CLAUDE_CODE_SHELL_LAUNCHER_SCRIPT"))
}

/// The `.output` file a PowerShell-tool launcher redirects to, foreground or background.
fn powershell_task_output(p: &ProcessInfo) -> Option<PathBuf> {
    if !powershell_tool_shell(p) {
        return None;
    }
    let line = p.command_line.as_deref()?;
    line.match_indices("> \"")
        .filter_map(|(at, m)| {
            let rest = &line[at + m.len()..];
            let path = &rest[..rest.find('"')?];
            path.ends_with(".output").then(|| PathBuf::from(path))
        })
        .next()
}

/// Windows gives each console program a `conhost.exe` child.
fn is_console_host(p: &ProcessInfo) -> bool {
    p.name
        .as_deref()
        .is_some_and(|n| n.eq_ignore_ascii_case("conhost.exe"))
}

/// Wrappers that only start the real work: shells running `-c`, `cmd /c`, npm, npx and `mise x`.
fn is_launcher(p: &ProcessInfo) -> bool {
    if claude_shell_command(p).is_some() {
        return true;
    }
    let name = p.name.as_deref().unwrap_or_default().to_ascii_lowercase();
    let name = name.trim_end_matches(".exe");
    let line = p
        .command_line
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let args: Vec<&str> = line.split_whitespace().collect();
    match name {
        "cmd" => args.contains(&"/c"),
        "sh" | "bash" | "zsh" | "dash" => args.iter().any(|a| {
            (a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                || a.ends_with("/npm")
                || a.ends_with("/npx")
        }),
        "pwsh" | "powershell" => args.contains(&"-command") || args.contains(&"-c"),
        "npm" | "npx" => true,
        "node" => ["npm-cli.js", "npx-cli.js", "/bin/npm", "/bin/npx"]
            .iter()
            .any(|m| line.contains(m)),
        "mise" => args.iter().any(|a| *a == "x" || *a == "exec"),
        _ => false,
    }
}

#[cfg(windows)]
fn kill(_processes: &[ProcessInfo], targets: &[u32], _own: &[u32], log: &mut dyn FnMut(&str)) {
    for &pid in targets {
        if let Err(e) = run("taskkill", &["/PID", &pid.to_string(), "/T", "/F"], None)
            && process_exists(pid)
        {
            log(&format!("  could not stop pid {pid}: {e:#}"));
        }
    }
}

/// SIGTERM to each target and its descendants, children first, then SIGKILL to whatever is left.
#[cfg(unix)]
fn kill(processes: &[ProcessInfo], targets: &[u32], own: &[u32], log: &mut dyn FnMut(&str)) {
    let mut all: Vec<u32> = Vec::new();
    for &pid in targets {
        for p in tree(processes, pid) {
            if !own.contains(&p) && !all.contains(&p) {
                all.push(p);
            }
        }
    }
    all.reverse();
    let signal = |sig: &str, pids: &[u32]| {
        let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
        let mut args = vec![sig];
        args.extend(pids.iter().map(String::as_str));
        let _ = run("kill", &args, None);
    };
    if all.is_empty() {
        return;
    }
    signal("-TERM", &all);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut left = all;
    while !left.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        left.retain(|&pid| process_exists(pid));
    }
    if left.is_empty() {
        return;
    }
    signal("-KILL", &left);
    std::thread::sleep(Duration::from_millis(200));
    for pid in left.into_iter().filter(|&pid| process_exists(pid)) {
        log(&format!("  could not stop pid {pid}"));
    }
}

#[cfg(windows)]
pub fn process_exists(pid: u32) -> bool {
    run(
        "tasklist",
        &["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"],
        None,
    )
    .is_ok_and(|out| out.contains(&format!("\"{pid}\"")))
}

/// A zombie still answers `kill -0`, so an exited child that hasn't been reaped yet counts as gone.
#[cfg(unix)]
pub fn process_exists(pid: u32) -> bool {
    run("ps", &["-o", "stat=", "-p", &pid.to_string()], None)
        .is_ok_and(|stat| !stat.trim().is_empty() && !stat.trim().starts_with('Z'))
}

/// When each of `pids` that is still running started, in Unix seconds, from one listing. A pid
/// that is running but whose start can't be read maps to `None`; one that isn't running is absent.
#[cfg(windows)]
pub fn start_times(pids: &[u32]) -> Result<HashMap<u32, Option<u64>>> {
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Started {
        process_id: u32,
        start_secs: Option<u64>,
    }
    let script = format!(
        "ConvertTo-Json -Compress -InputObject @(Get-CimInstance Win32_Process -Filter '{}' | Select-Object ProcessId,@{{n='StartSecs';e={{if ($_.CreationDate) {{ ([DateTimeOffset]$_.CreationDate).ToUnixTimeSeconds() }}}}}})",
        pid_filter(pids)
    );
    let json = run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
        None,
    )?;
    let started: Vec<Started> =
        serde_json::from_str(json.trim()).context("parsing process start times")?;
    Ok(started
        .into_iter()
        .map(|s| (s.process_id, s.start_secs))
        .collect())
}

/// `lstart` is local time, so `ps` is run in UTC and the C locale.
#[cfg(unix)]
pub fn start_times(pids: &[u32]) -> Result<HashMap<u32, Option<u64>>> {
    // macOS `ps` fails the whole call for a pid above PID_MAX (99999), which can't be running.
    let possible = |pid: &&u32| cfg!(not(target_os = "macos")) || **pid <= 99_999;
    let pids: Vec<&u32> = pids.iter().filter(possible).collect();
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = pids
        .iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let out = command("ps")
        .args(["-o", "pid=,stat=,lstart=", "-p", &list])
        .env("TZ", "UTC0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .context("starting `ps`")?;
    // ps exits 1, saying nothing, when none of the pids is running.
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && !stderr.trim().is_empty() {
        bail!("`ps -o lstart` failed: {}", stderr.trim());
    }
    Ok(parse_ps_starts(&String::from_utf8_lossy(&out.stdout)))
}

/// `ps -o pid=,stat=,lstart=` in UTC: `1234 Ss Tue Nov 10 09:12:33 2026`. Zombies are left out.
#[cfg(any(unix, test))]
fn parse_ps_starts(out: &str) -> HashMap<u32, Option<u64>> {
    out.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let pid = fields.first()?.parse().ok()?;
            let stat = fields.get(1)?;
            (!stat.starts_with('Z')).then(|| (pid, parse_lstart(&fields[2..])))
        })
        .collect()
}

/// `Tue Nov 10 09:12:33 2026`, in UTC, as Unix seconds.
#[cfg(any(unix, test))]
fn parse_lstart(fields: &[&str]) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let [_, month, day, clock, year] = fields else {
        return None;
    };
    let month = MONTHS.iter().position(|m| m == month)? as i64 + 1;
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let mut secs = 0;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<i64>().ok()?;
    }
    // Days from 1970-01-01 to the date, in the proleptic Gregorian calendar (Howard Hinnant's
    // days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + secs).ok()
}

/// What tells a recorded process apart from a later one that reused its pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartCheck {
    /// The token `start_tokens` gives for it, read when it was spawned.
    Token(u64),
    /// For records made before tokens: it started no later than this, in Unix seconds.
    NoLaterThan(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// Running, and the recorded process.
    Same,
    /// Running, but whether it's the recorded process can't be read.
    Unknown,
    /// Not running, or its pid has been reused.
    Gone,
}

/// Whether each `(pid, check)` is still the process it records. Token checks are cheap; only
/// `NoLaterThan` ones list processes.
pub fn identify(checks: &[(u32, StartCheck)]) -> Result<Vec<Identity>> {
    let pids = |token: bool| -> Vec<u32> {
        checks
            .iter()
            .filter(|(_, c)| matches!(c, StartCheck::Token(_)) == token)
            .map(|(pid, _)| *pid)
            .collect()
    };
    let tokens = start_tokens(&pids(true))?;
    let starts = start_times(&pids(false))?;
    Ok(checks
        .iter()
        .map(|&(pid, check)| {
            let read = match check {
                StartCheck::Token(_) => tokens.get(&pid),
                StartCheck::NoLaterThan(_) => starts.get(&pid),
            };
            identity_of(check, read.copied())
        })
        .collect())
}

/// `read` is what `start_tokens` or `start_times` gave for the pid: absent when it isn't running.
fn identity_of(check: StartCheck, read: Option<Option<u64>>) -> Identity {
    match (check, read) {
        (_, None) => Identity::Gone,
        (_, Some(None)) => Identity::Unknown,
        (StartCheck::Token(token), Some(Some(read))) if read == token => Identity::Same,
        (StartCheck::NoLaterThan(latest), Some(Some(read))) if read <= latest => Identity::Same,
        _ => Identity::Gone,
    }
}

/// An opaque token for when each of `pids` that is still running started, from the OS's own
/// record of it rather than the wall clock, so clock steps and DST don't change it: the creation
/// FILETIME on Windows, the start in clock ticks since boot on Linux, and `ps`'s start in UTC
/// seconds elsewhere. A pid that is running but whose token can't be read maps to `None`; one
/// that isn't running is absent.
#[cfg(windows)]
pub fn start_tokens(pids: &[u32]) -> Result<HashMap<u32, Option<u64>>> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut tokens = HashMap::new();
    for &pid in pids {
        // SAFETY: the handle is checked before use and closed after.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                if GetLastError() == ERROR_ACCESS_DENIED {
                    tokens.insert(pid, None);
                }
                continue;
            }
            let mut code = 0;
            let exited = GetExitCodeProcess(handle, &mut code) != 0 && code != STILL_ACTIVE as u32;
            if !exited {
                tokens.insert(pid, creation_token(handle));
            }
            CloseHandle(handle);
        }
    }
    Ok(tokens)
}

/// # Safety
/// `handle` must be a valid process handle.
#[cfg(windows)]
unsafe fn creation_token(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let [mut created, mut exited, mut kernel, mut user] = [zero; 4];
    // SAFETY: the caller guarantees the handle; the out pointers are to locals.
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    (ok != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

#[cfg(target_os = "linux")]
pub fn start_tokens(pids: &[u32]) -> Result<HashMap<u32, Option<u64>>> {
    Ok(pids
        .iter()
        .filter_map(|&pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            match parse_proc_stat(&stat) {
                Some(('Z' | 'X', _)) => None,
                Some((_, start)) => Some((pid, Some(start))),
                None => Some((pid, None)),
            }
        })
        .collect())
}

#[cfg(all(unix, not(target_os = "linux")))]
pub fn start_tokens(pids: &[u32]) -> Result<HashMap<u32, Option<u64>>> {
    start_times(pids)
}

/// The state and start time (field 22, in clock ticks since boot) from `/proc/<pid>/stat`. The
/// program name in field 2 can hold spaces and parentheses, so fields are counted from the last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(stat: &str) -> Option<(char, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    Some((state, fields.get(19)?.parse().ok()?))
}

/// `child`'s `start_tokens` token, read from the child itself, so it can't be another process
/// that reused the pid.
pub fn child_start_token(child: &std::process::Child) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: `child` owns the handle and outlives the call.
        unsafe { creation_token(child.as_raw_handle()) }
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).ok()?;
        parse_proc_stat(&stat).map(|(_, start)| start)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        start_times(&[child.id()])
            .ok()?
            .remove(&child.id())
            .flatten()
    }
}

/// Stops `pid` and everything under it if it is still the process `check` records. When that
/// can't be read, its command line must contain `command` instead. Returns whether it was running.
pub fn stop_tree_started_by(
    pid: u32,
    command: &str,
    check: StartCheck,
    log: &mut dyn FnMut(&str),
) -> Result<bool> {
    let identity = identify(&[(pid, check)])?[0];
    if identity == Identity::Gone {
        return Ok(false);
    }
    let processes = snapshot()?;
    let running = match identity {
        Identity::Same => processes.iter().any(|p| p.process_id == pid),
        _ => still_runs(&processes, pid, command),
    };
    let own = ancestry(&processes, std::process::id());
    if running && !own.contains(&pid) {
        kill(&processes, &[pid], &own, log);
    }
    Ok(running)
}

/// A process listening on a local TCP port.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Listener {
    pub port: u16,
    /// `None` when the OS doesn't say, such as another user's process on Linux.
    pub pid: Option<u32>,
    /// The program's file name, such as `node.exe`.
    pub name: Option<String>,
    pub command: Option<String>,
}

/// What listens on any of `ports`, on any address and either address family, from one listing.
pub fn listeners(ports: &[u16]) -> Result<Vec<Listener>> {
    if ports.is_empty() {
        return Ok(Vec::new());
    }
    let mut found: Vec<(u16, Option<u32>)> = Vec::new();
    for (port, pid) in listening_sockets()? {
        if ports.contains(&port) && !found.contains(&(port, pid)) {
            found.push((port, pid));
        }
    }
    let pids: Vec<u32> = found.iter().filter_map(|(_, pid)| *pid).collect();
    let commands = command_lines(&pids);
    Ok(found
        .into_iter()
        .map(|(port, pid)| {
            let (name, command) = pid
                .and_then(|pid| commands.get(&pid).cloned())
                .unwrap_or_default();
            Listener {
                port,
                pid,
                name,
                command,
            }
        })
        .collect())
}

/// Every TCP socket with no remote port, from the IPv4 and IPv6 tables. The state isn't used:
/// some Windows builds report a listening socket's state as a value `netstat` doesn't know, and
/// `netstat` then leaves the socket out.
#[cfg(windows)]
fn listening_sockets() -> Result<Vec<(u16, Option<u32>)>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
    };
    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    let port = |p: u32| u16::from_be(p as u16);
    let v4 = tcp_table::<MIB_TCPROW_OWNER_PID>(AF_INET)?
        .into_iter()
        .map(|r| (port(r.dwLocalPort), port(r.dwRemotePort), r.dwOwningPid));
    let v6 = tcp_table::<MIB_TCP6ROW_OWNER_PID>(AF_INET6)?
        .into_iter()
        .map(|r| (port(r.dwLocalPort), port(r.dwRemotePort), r.dwOwningPid));
    Ok(v4
        .chain(v6)
        .filter(|(_, remote, _)| *remote == 0)
        .map(|(local, _, pid)| (local, Some(pid)))
        .collect())
}

/// The rows of `GetExtendedTcpTable`'s `TCP_TABLE_OWNER_PID_ALL` table for one address family.
#[cfg(windows)]
fn tcp_table<Row: Copy>(family: u32) -> Result<Vec<Row>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, TCP_TABLE_OWNER_PID_ALL,
    };
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let mut size = 0u32;
    let mut buf: Vec<u64> = Vec::new();
    loop {
        // SAFETY: `buf` holds at least `size` bytes, aligned for the table's u32 and row fields.
        let rc = unsafe {
            GetExtendedTcpTable(
                buf.as_mut_ptr().cast(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        match rc {
            0 => break,
            ERROR_INSUFFICIENT_BUFFER => buf = vec![0; (size as usize).div_ceil(8) + 1],
            e => bail!("listing TCP sockets failed (error {e})"),
        }
    }
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    let base = buf.as_ptr().cast::<u8>();
    // SAFETY: the table is a u32 count followed by that many rows, starting at the rows' alignment.
    unsafe {
        let count = base.cast::<u32>().read() as usize;
        let offset = std::mem::size_of::<u32>().next_multiple_of(std::mem::align_of::<Row>());
        let rows = base.add(offset).cast::<Row>();
        Ok((0..count).map(|i| rows.add(i).read()).collect())
    }
}

#[cfg(target_os = "macos")]
fn listening_sockets() -> Result<Vec<(u16, Option<u32>)>> {
    // lsof exits 1 when nothing matches.
    let out = command("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("starting `lsof`")?;
    Ok(parse_lsof_listen(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn listening_sockets() -> Result<Vec<(u16, Option<u32>)>> {
    Ok(parse_ss(&run("ss", &["-ltnp"], None)?))
}

/// The port after the last `:` of an address such as `0.0.0.0:5173`, `[::1]:5173` or `*:5173`.
#[cfg(any(unix, test))]
fn address_port(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

/// `lsof -nP -iTCP -sTCP:LISTEN`: `node 1234 t 23u IPv6 0x1 0t0 TCP [::1]:5173 (LISTEN)`.
#[cfg(any(target_os = "macos", test))]
fn parse_lsof_listen(out: &str) -> Vec<(u16, Option<u32>)> {
    out.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let pid = fields.get(1)?.parse().ok()?;
            let at = fields.iter().position(|f| *f == "TCP")?;
            Some((address_port(fields.get(at + 1)?)?, Some(pid)))
        })
        .collect()
}

/// `ss -ltnp`, after its header: `LISTEN 0 511 [::1]:5173 [::]:* users:(("node",pid=1234,fd=20))`.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn parse_ss(out: &str) -> Vec<(u16, Option<u32>)> {
    out.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let port = address_port(fields.get(3)?)?;
            let pid = line
                .split_once("pid=")
                .and_then(|(_, rest)| rest.split([',', ')']).next()?.parse().ok());
            Some((port, pid))
        })
        .collect()
}

/// Each pid's program name and command line, for those that are still running.
#[cfg(windows)]
fn command_lines(pids: &[u32]) -> HashMap<u32, (Option<String>, Option<String>)> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let script = format!(
        "ConvertTo-Json -Compress -InputObject @(Get-CimInstance Win32_Process -Filter '{}' | Select-Object ProcessId,Name,CommandLine)",
        pid_filter(pids)
    );
    run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
        None,
    )
    .ok()
    .and_then(|json| serde_json::from_str::<Vec<ProcessInfo>>(json.trim()).ok())
    .into_iter()
    .flatten()
    .map(|p| {
        let command = p.command_line.or_else(|| p.name.clone());
        (p.process_id, (p.name, command))
    })
    .collect()
}

#[cfg(unix)]
fn command_lines(pids: &[u32]) -> HashMap<u32, (Option<String>, Option<String>)> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    run("ps", &["-ww", "-o", "pid=,args=", "-p", &list], None)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (pid, args) = line.trim().split_once(char::is_whitespace)?;
            let args = args.trim();
            let program = args.split_whitespace().next()?;
            let name = program.rsplit('/').next().unwrap_or(program).to_string();
            Some((pid.parse().ok()?, (Some(name), Some(args.to_string()))))
        })
        .collect()
}

#[cfg(windows)]
fn pid_filter(pids: &[u32]) -> String {
    pids.iter()
        .map(|pid| format!("ProcessId={pid}"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Only at a path boundary, so `C:\w\1` doesn't match `C:\w\10`. Case- and slash-insensitive on
/// Windows, exact elsewhere.
fn mentions_path(text: &str, dir: &Path) -> bool {
    mentions_path_on(text, &dir.to_string_lossy(), cfg!(windows))
}

fn mentions_path_on(text: &str, dir: &str, windows: bool) -> bool {
    let (text, dir, sep) = if windows {
        let norm = |s: &str| s.to_ascii_lowercase().replace('/', "\\");
        (norm(text), norm(dir), '\\')
    } else {
        (text.to_string(), dir.to_string(), '/')
    };
    let dir = dir.trim_end_matches(sep);
    !dir.is_empty()
        && text.match_indices(dir).any(|(at, _)| {
            text[at + dir.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == sep || matches!(c, '"' | '\'' | ' '))
        })
}

pub fn describe(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_timeout_stops_a_command_that_runs_too_long() {
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("ping", &["-n", "30", "127.0.0.1"])
        } else {
            ("sleep", &["30"])
        };
        let mut slow = command(program);
        slow.args(args);
        let started = Instant::now();
        let e = output_timeout(slow, "slow", Duration::from_millis(500)).unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e:#}");
        assert!(started.elapsed() < Duration::from_secs(15));

        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/c", "echo hi"])
        } else {
            ("sh", &["-c", "echo hi"])
        };
        let mut quick = command(program);
        quick.args(args);
        let out = output_timeout(quick, "quick", Duration::from_secs(10)).unwrap();
        assert_eq!(out.trim(), "hi");
    }

    #[test]
    fn matches_a_command_through_windows_quote_escaping() {
        let command = r#"node -e "require('http').listen(1)""#;
        let line = r#""pwsh" -NoProfile -Command "node -e \"require('http').listen(1)\"""#;
        assert!(runs_command(line, command));
        assert!(runs_command("sh -c npm run dev", "npm run dev"));
        assert!(!runs_command("sh -c npm run build", "npm run dev"));
    }

    fn p(pid: u32, parent: u32) -> ProcessInfo {
        ProcessInfo {
            process_id: pid,
            parent_process_id: parent,
            ..Default::default()
        }
    }

    #[cfg(windows)]
    #[test]
    fn stops_a_session_tree_and_processes_running_from_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ping = |extra: &str| {
            command("cmd")
                .args(["/c", &format!("ping -n 60 127.0.0.1 >nul{extra}")])
                .spawn()
                .unwrap()
        };
        let mut session = ping("");
        let mut from_folder = ping(&format!(" & rem {}", dir.path().display()));
        let mut unrelated = ping("");
        std::thread::sleep(std::time::Duration::from_millis(500));

        stop_processes(Some(session.id()), dir.path(), &mut |_| {}).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(session.try_wait().unwrap().is_some());
        assert!(from_folder.try_wait().unwrap().is_some());
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stops_a_session_tree_and_processes_running_from_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let sleep = |cwd: Option<&Path>| {
            let mut cmd = command("sh");
            cmd.args(["-c", "sleep 60; true"]);
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            cmd.spawn().unwrap()
        };
        let mut session = sleep(None);
        let mut from_folder = sleep(Some(dir.path()));
        let mut unrelated = sleep(None);
        std::thread::sleep(std::time::Duration::from_millis(500));

        stop_processes(Some(session.id()), dir.path(), &mut |_| {}).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(session.try_wait().unwrap().is_some());
        assert!(from_folder.try_wait().unwrap().is_some());
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
    }

    fn proc(pid: u32, parent: u32, name: &str, line: &str) -> ProcessInfo {
        ProcessInfo {
            process_id: pid,
            parent_process_id: parent,
            name: Some(name.into()),
            command_line: Some(line.into()),
            working_set_size: Some(1 << 20),
            age_secs: Some(60),
            ..Default::default()
        }
    }

    const SHELL: &str = r#""C:\Program Files\Git\bin\bash.exe" -c "source /c/Users/t/.claude/shell-snapshots/snapshot-bash-1.sh 2>/dev/null || true && eval 'cd C:/work/acme/1-x/api && npm run dev -- --name='\''a b'\''' < /dev/null && pwd -P >| /tmp/cwd""#;

    #[test]
    fn reads_the_command_claude_evals() {
        assert_eq!(
            claude_shell_command(&proc(1, 0, "bash.exe", SHELL)).as_deref(),
            Some("cd C:/work/acme/1-x/api && npm run dev -- --name='a b'")
        );
        assert_eq!(
            claude_shell_command(&proc(1, 0, "bash.exe", "bash -c 'eval x'")),
            None
        );
    }

    #[test]
    // The fixtures are Windows paths, which Path doesn't split on `\` elsewhere.
    #[cfg(windows)]
    fn finds_claude_shells_and_orphans_but_not_mcp_servers() {
        let dir = Path::new(r"C:\work\acme\1-x");
        let procs = [
            proc(1, 0, "tworky.exe", "tworky"),
            proc(10, 1, "claude.exe", "claude --resume s"),
            proc(11, 10, "node.exe", r"node C:\mcp\server.js"),
            proc(12, 10, "bash.exe", SHELL),
            proc(13, 12, "node.exe", "node npm-cli.js run dev"),
            proc(
                14,
                13,
                "node.exe",
                r"node C:\work\acme\1-x\api\node_modules\.bin\ts-node-dev",
            ),
            proc(20, 99, "sh.exe", r"sh.exe /c/node/npx expo start"),
            proc(
                21,
                20,
                "node.exe",
                r"node C:\work\acme\1-x\app\node_modules\expo\bin\cli",
            ),
            proc(
                22,
                21,
                "esbuild.exe",
                r"C:\work\acme\1-x\app\node_modules\esbuild.exe",
            ),
            proc(30, 0, "node.exe", r"node C:\work\acme\1-x-2\server.js"),
        ];
        let jobs = find_jobs(&procs, dir, Some(10), &[1]);
        let summary: Vec<_> = jobs
            .iter()
            .map(|j| (j.pid, j.from_claude, j.process_count))
            .collect();
        assert_eq!(summary, vec![(12, true, 3), (20, false, 3)]);
        assert_eq!(jobs[0].memory_bytes, 3 << 20);
        assert_eq!(
            find_jobs(&procs, dir, None, &[1])
                .iter()
                .map(|j| j.pid)
                .collect::<Vec<_>>(),
            vec![12, 20],
            "with the session gone, the shell is found through its child's path"
        );
    }

    #[test]
    // The fixtures are Windows paths, which Path doesn't split on `\` elsewhere.
    #[cfg(windows)]
    fn merges_a_detached_tree_into_the_claude_command_that_started_it() {
        let dir = Path::new(r"C:\work\acme\1-x");
        let shell = SHELL.replace(
            "cd C:/work/acme/1-x/api && npm run dev -- --name='\\''a b'\\''",
            "cd C:/work/acme/1-x/app && npx expo start --web",
        );
        let procs = [
            proc(10, 1, "claude.exe", "claude --resume s"),
            proc(12, 10, "bash.exe", &shell),
            proc(
                20,
                99,
                "sh.exe",
                r#""C:\Git\usr\bin\sh.exe" /c/node/npx expo start --web"#,
            ),
            proc(
                21,
                20,
                "node.exe",
                r"node C:\work\acme\1-x\app\node_modules\expo\bin\cli",
            ),
            proc(
                30,
                98,
                "sh.exe",
                r#""C:\Git\usr\bin\sh.exe" /c/node/npx vite"#,
            ),
            proc(
                31,
                30,
                "node.exe",
                r"node C:\work\acme\1-x\web\node_modules\vite\bin\vite.js",
            ),
        ];
        let jobs = find_jobs(&procs, dir, Some(10), &[]);
        let summary: Vec<_> = jobs
            .iter()
            .map(|j| (j.pid, j.roots.clone(), j.process_count))
            .collect();
        assert_eq!(summary, vec![(12, vec![12, 20], 3), (30, vec![30], 2)]);
    }

    #[test]
    // The fixtures are Windows paths, which Path doesn't split on `\` elsewhere.
    #[cfg(windows)]
    fn a_closing_session_takes_its_claude_and_jobs_but_not_other_workers() {
        let dir = Path::new(r"C:\work\acme\1-x");
        let procs = [
            proc(1, 0, "tworky.exe", "tworky"),
            proc(10, 1, "claude.exe", "claude --resume s"),
            proc(11, 10, "node.exe", r"node C:\mcp\server.js"),
            proc(12, 10, "bash.exe", SHELL),
            proc(20, 99, "node.exe", r"node C:\work\acme\1-x\app\server.js"),
            proc(30, 1, "claude.exe", "claude --resume other"),
            proc(31, 0, "node.exe", r"node C:\work\acme\1-x-2\server.js"),
        ];
        assert_eq!(session_targets(&procs, dir, "s", &[1]), vec![10, 12, 20]);
        assert_eq!(
            session_targets(&procs, dir, "gone", &[1]),
            vec![12, 20],
            "a session that already exited still leaves its jobs to stop"
        );
    }

    const PWSH_LAUNCHER: &str = r#"C:\WINDOWS\System32\cmd.exe /d /s /c ""C:\WINDOWS\System32\chcp.com" 65001 >nul & "C:\Program Files\PowerShell\7\pwsh.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$__claudeCodeScript = $env:CLAUDE_CODE_SHELL_LAUNCHER_SCRIPT; $env:CLAUDE_CODE_SHELL_LAUNCHER_SCRIPT = $null; Invoke-Expression -Command $__claudeCodeScript" > "C:\Users\t\AppData\Local\Temp\claude\C--work-acme-1-x\s\tasks\b55v59mme.output" 2>&1""#;
    const PWSH_SHELL: &str = r#""C:\Program Files\PowerShell\7\pwsh.exe"  -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$__claudeCodeScript = $env:CLAUDE_CODE_SHELL_LAUNCHER_SCRIPT; $env:CLAUDE_CODE_SHELL_LAUNCHER_SCRIPT = $null; Invoke-Expression -Command $__claudeCodeScript""#;

    #[test]
    fn reads_the_output_file_a_powershell_launcher_redirects_to() {
        assert_eq!(
            powershell_task_output(&proc(1, 0, "cmd.exe", PWSH_LAUNCHER)),
            Some(PathBuf::from(
                r"C:\Users\t\AppData\Local\Temp\claude\C--work-acme-1-x\s\tasks\b55v59mme.output"
            ))
        );
        assert_eq!(
            powershell_task_output(&proc(1, 0, "pwsh.exe", PWSH_SHELL)),
            None
        );
        assert_eq!(
            powershell_task_output(&proc(1, 0, "cmd.exe", r#"cmd /c npm test > "x.output""#)),
            None,
            "only Claude's launcher"
        );
    }

    #[test]
    fn finds_powershell_tool_jobs_but_not_mcp_servers() {
        let procs = [
            proc(1, 0, "tworky.exe", "tworky"),
            proc(10, 1, "claude.exe", "claude --resume s"),
            proc(11, 10, "node.exe", r"node C:\mcp\server.js"),
            proc(12, 10, "cmd.exe", PWSH_LAUNCHER),
            proc(
                13,
                12,
                "conhost.exe",
                r"\??\C:\WINDOWS\system32\conhost.exe 0x4",
            ),
            proc(14, 12, "pwsh.exe", PWSH_SHELL),
            proc(15, 14, "node.exe", r"node C:\nvm\npm-cli.js run dev"),
            proc(
                16,
                15,
                "cmd.exe",
                r#"C:\WINDOWS\system32\cmd.exe /d /s /c "vite --port 5173""#,
            ),
            proc(
                17,
                16,
                "node.exe",
                r"node C:\work\acme\1-x\web\node_modules\vite\bin\vite.js --port 5173",
            ),
        ];
        let jobs = find_jobs(&procs, Path::new(r"C:\work\acme\1-x"), Some(10), &[1]);
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        let job = &jobs[0];
        assert_eq!((job.pid, job.from_claude, job.process_count), (12, true, 6));
        assert_eq!(
            job.command,
            r"node C:\work\acme\1-x\web\node_modules\vite\bin\vite.js --port 5173"
        );
        assert!(
            job.task_output
                .as_deref()
                .is_some_and(|p| p.to_string_lossy().ends_with(r"\b55v59mme.output"))
        );
    }

    #[test]
    fn parses_ps_start_times_as_utc() {
        let out = "  123 Ss   Tue Nov 10 09:12:33 2026\n  456 Z    Thu Feb 29 23:59:59 2024\n  789 R+   Thu Feb 29 23:59:59 2024\n 1011 S    Wed Mar  1 00:00:00 2000\n 1213 S    garbled\n";
        let starts = parse_ps_starts(out);
        assert_eq!(starts.get(&123), Some(&Some(1_794_301_953)));
        assert_eq!(starts.get(&456), None, "zombies have exited");
        assert_eq!(starts.get(&789), Some(&Some(1_709_251_199)));
        assert_eq!(starts.get(&1011), Some(&Some(951_868_800)));
        assert_eq!(starts.get(&1213), Some(&None), "running, start unknown");
        assert_eq!(
            parse_lstart(&["Thu", "Jan", "1", "00:00:00", "1970"]),
            Some(0)
        );
    }

    #[test]
    fn reads_this_process_start_time() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let pid = std::process::id();
        let starts = start_times(&[pid, 4_000_001]).unwrap();
        let start = starts[&pid].expect("our own start time");
        assert!(start <= now + 1, "{start} is after {now}");
        assert!(
            now - start < 3600,
            "{start} is an hour or more before {now}"
        );
        assert!(!starts.contains_key(&4_000_001));
        assert!(start_times(&[]).unwrap().is_empty());
    }

    #[test]
    fn reads_this_process_start_token_and_not_a_missing_one() {
        let pid = std::process::id();
        let tokens = start_tokens(&[pid, 4_000_001]).unwrap();
        let token = tokens[&pid].expect("our own start token");
        assert_eq!(start_tokens(&[pid]).unwrap()[&pid], Some(token), "stable");
        assert!(!tokens.contains_key(&4_000_001));
        assert!(start_tokens(&[]).unwrap().is_empty());
    }

    #[test]
    fn a_spawned_child_has_the_token_its_pid_reads_as() {
        let mut cmd = command(if cfg!(windows) { "cmd" } else { "sh" });
        if cfg!(windows) {
            cmd.args(["/c", "ping -n 30 127.0.0.1 >nul"]);
        } else {
            cmd.args(["-c", "sleep 30"]);
        }
        let mut child = cmd.stdout(Stdio::null()).spawn().unwrap();
        let token = child_start_token(&child).expect("the child's start token");
        let pid = child.id();
        let identities = identify(&[
            (pid, StartCheck::Token(token)),
            (pid, StartCheck::Token(token + 1)),
        ])
        .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(identities, [Identity::Same, Identity::Gone]);
        assert_eq!(
            identify(&[(pid, StartCheck::Token(token))]).unwrap(),
            [Identity::Gone]
        );
    }

    #[test]
    fn a_reused_pid_reads_as_another_process() {
        use StartCheck::*;
        assert_eq!(identity_of(Token(5), Some(Some(5))), Identity::Same);
        assert_eq!(identity_of(Token(5), Some(Some(6))), Identity::Gone);
        assert_eq!(identity_of(Token(5), Some(None)), Identity::Unknown);
        assert_eq!(identity_of(Token(5), None), Identity::Gone);
        assert_eq!(identity_of(NoLaterThan(10), Some(Some(10))), Identity::Same);
        assert_eq!(identity_of(NoLaterThan(10), Some(Some(11))), Identity::Gone);
        assert_eq!(identity_of(NoLaterThan(10), Some(None)), Identity::Unknown);
    }

    #[test]
    fn parses_the_start_from_proc_stat_after_the_last_parenthesis() {
        let stat = "4242 (my (odd) prog) S 1 4242 4242 0 -1 4194560 120 0 0 0 1 2 0 0 20 0 1 0 987654 1234 56 18446744073709551615";
        assert_eq!(parse_proc_stat(stat), Some(('S', 987_654)));
        assert_eq!(parse_proc_stat("12 (x) Z 1"), None);
        assert_eq!(parse_proc_stat("garbled"), None);
    }

    #[test]
    fn labels_a_powershell_tool_job_with_no_program_under_it() {
        let procs = [
            proc(10, 1, "claude.exe", "claude --resume s"),
            proc(12, 10, "cmd.exe", PWSH_LAUNCHER),
            proc(
                13,
                12,
                "conhost.exe",
                r"\??\C:\WINDOWS\system32\conhost.exe 0x4",
            ),
            proc(14, 12, "pwsh.exe", PWSH_SHELL),
        ];
        let jobs = find_jobs(&procs, Path::new(r"C:\work\acme\1-x"), Some(10), &[]);
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].command, POWERSHELL_COMMAND);
    }

    #[test]
    fn parses_listening_sockets() {
        let lsof = "COMMAND   PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME\nnode    12345 t     23u  IPv6 0x1234567890abcdef      0t0  TCP [::1]:5173 (LISTEN)\nnode    12345 t     24u  IPv4 0x1234567890abcdee      0t0  TCP *:5174 (LISTEN)\n";
        assert_eq!(
            parse_lsof_listen(lsof),
            vec![(5173, Some(12345)), (5174, Some(12345))]
        );
        let ss = "State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process\nLISTEN 0      511    127.0.0.1:5173     0.0.0.0:*    users:((\"node\",pid=1234,fd=20))\nLISTEN 0      4096   [::]:5174          [::]:*\n";
        assert_eq!(parse_ss(ss), vec![(5173, Some(1234)), (5174, None)]);
    }

    #[test]
    fn strips_the_shell_from_a_launched_command() {
        assert_eq!(
            launched_command(
                r#""C:\Program Files\Git\usr\bin\sh.exe" /c/mise/shims/npx jest --ci"#
            )
            .as_deref(),
            Some("npx jest --ci")
        );
        assert_eq!(launched_command("cmd.exe /d /s /c npm"), None);
    }

    #[test]
    fn finds_the_session_by_its_id() {
        let procs = [
            proc(1, 0, "cmd.exe", "cmd /c claude.cmd --resume abc-123"),
            proc(2, 1, "claude.exe", "claude.exe --resume abc-123"),
            proc(3, 0, "claude.exe", "claude.exe --resume other"),
        ];
        assert_eq!(session_pid(&procs, "abc-123"), Some(1));
        assert_eq!(session_pid(&procs, "missing"), None);
        assert_eq!(session_pid(&procs, ""), None);
    }

    #[test]
    fn leaves_out_young_jobs() {
        let mut fresh = proc(5, 0, "node.exe", r"node C:\work\acme\1-x\a.js");
        fresh.age_secs = Some(2);
        assert!(find_jobs(&[fresh], Path::new(r"C:\work\acme\1-x"), None, &[]).is_empty());
    }

    #[test]
    fn parses_ps_elapsed_time() {
        assert_eq!(parse_etime("05"), Some(5));
        assert_eq!(parse_etime("01:05"), Some(65));
        assert_eq!(parse_etime("02:01:05"), Some(7265));
        assert_eq!(parse_etime("3-02:01:05"), Some(3 * 86_400 + 7265));
        assert_eq!(parse_etime("x"), None);
    }

    #[test]
    fn tree_walks_descendants_only() {
        let procs = [
            p(1, 0),
            p(10, 1),
            p(11, 10),
            p(12, 11),
            p(20, 1),
            p(30, 99),
            p(0, 0),
        ];
        assert_eq!(tree(&procs, 10), vec![10, 11, 12]);
        assert_eq!(tree(&procs, 30), vec![30]);
        assert_eq!(ancestry(&procs, 12), vec![12, 11, 10, 1]);
    }

    #[test]
    fn matches_windows_paths_only_at_a_boundary() {
        let dir = r"C:\work\acme\42-fix-header";
        let on = |text: &str| mentions_path_on(text, dir, true);
        assert!(on(
            r#""node" "C:\work\acme\42-fix-header\api\node_modules\.bin\vite.js" --port 5373"#
        ));
        assert!(on("node C:/Work/ACME/42-fix-header/api/src/index.ts"));
        assert!(on(r"C:\work\acme\42-fix-header"));
        assert!(!on(r"node C:\work\acme\42-fix-header-2\x.js"));
        assert!(!on("node server.js"));
    }

    #[test]
    fn matches_unix_paths_exactly_and_only_at_a_boundary() {
        let dir = "/Users/t/work/acme/42-fix-header/";
        let on = |text: &str| mentions_path_on(text, dir, false);
        assert!(on(
            "node /Users/t/work/acme/42-fix-header/api/node_modules/.bin/vite --port 5373"
        ));
        assert!(on("/Users/t/work/acme/42-fix-header"));
        assert!(!on("node /Users/t/work/acme/42-fix-header-2/x.js"));
        assert!(!on("node /users/t/work/ACME/42-fix-header/x.js"));
    }

    #[test]
    fn parses_ps_output() {
        let out = "    1     0  1024    3-02:01:05 /sbin/launchd\n  512     1  2048 01:05 /Applications/Docker.app/Contents/MacOS/com.docker.backend --flag x\n  bad line\n";
        let procs = parse_ps(out);
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[1].process_id, 512);
        assert_eq!(procs[1].parent_process_id, 1);
        assert_eq!(procs[1].name.as_deref(), Some("com.docker.backend"));
        assert_eq!(procs[1].working_set_size, Some(2048 * 1024));
        assert_eq!(procs[1].age_secs, Some(65));
        assert_eq!(
            procs[1].command_line.as_deref(),
            Some("/Applications/Docker.app/Contents/MacOS/com.docker.backend --flag x")
        );
    }

    #[test]
    fn parses_lsof_cwd_fields() {
        let cwds = parse_lsof("p42\nfcwd\nn/Users/t/work/acme/x\np43\nfcwd\nn/\n");
        assert_eq!(
            cwds.get(&42).map(String::as_str),
            Some("/Users/t/work/acme/x")
        );
        assert_eq!(cwds.get(&43).map(String::as_str), Some("/"));
    }
}
