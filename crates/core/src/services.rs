use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Paths, ProjectConfig, ServiceConfig};
use crate::hierarchy::IssueIndex;
use crate::state::WorkerRecord;
use crate::worker::{Log, Workspace, template_context, worker_env};
use crate::{process, template};

/// A service Tworky started, kept so the app and `tw` can find and stop it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Running {
    pub pid: u32,
    /// The rendered command. Stopping checks that the process's command line still contains it
    /// only when its start time can't be read.
    pub command: String,
    pub log: PathBuf,
    pub started: u64,
    /// `process::start_tokens`'s token for it, read when it was spawned. Records made before tokens
    /// fall back to comparing the process's start time with `started`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_token: Option<u64>,
}

type Record = BTreeMap<String, Running>;

/// Shown to Claude when a project has no `[[services]]`.
pub const NO_SERVICES_NOTE: &str = "This project defines no services, so Tworky can't start its \
dev servers, and a dev server you start yourself won't show to the user as a service they can \
start or stop. Ask the user to add [[services]] to the project in its project setup session \
(Projects > Setup).";

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub needs: Vec<String>,
    pub running: bool,
    pub state: ServiceState,
    pub pid: Option<u32>,
    pub log: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Running,
    /// Its process is alive but nothing answers on its port yet.
    Starting,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Started,
    AlreadyRunning,
}

/// Held while the services record is read and rewritten: two tabs opened together must not both
/// start the same dev server, and a stop must not drop the entry of a start running alongside it.
static STARTING: Mutex<()> = Mutex::new(());

fn dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("services")
}

fn record_file(paths: &Paths, worker: &str) -> PathBuf {
    dir(paths).join(format!("{worker}.json"))
}

fn log_file(paths: &Paths, worker: &str, name: &str) -> PathBuf {
    dir(paths).join(worker).join(format!("{name}.log"))
}

/// Whether `path` is a service log, which the job view may read.
pub fn is_log(paths: &Paths, path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "log")
        && path.starts_with(dir(paths))
        && !path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
}

pub fn read(paths: &Paths, worker: &str) -> Record {
    std::fs::read_to_string(record_file(paths, worker))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub(crate) fn write(paths: &Paths, worker: &str, record: &Record) -> Result<()> {
    let file = record_file(paths, worker);
    if record.is_empty() {
        return match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(dir(paths))?;
    let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(record)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Dev servers often listen on `localhost`, which Node may bind to IPv6 only.
pub fn port_open(port: u16) -> bool {
    [
        SocketAddr::from(([127, 0, 0, 1], port)),
        SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|addr| TcpStream::connect_timeout(addr, Duration::from_millis(300)).is_ok())
}

pub fn service_origin(w: &WorkerRecord, service: &ServiceConfig) -> Option<String> {
    let port = Workspace::service_port(w, service)?;
    let scheme = if service.https { "https" } else { "http" };
    Some(format!("{scheme}://localhost:{port}"))
}

pub fn service_url(w: &WorkerRecord, service: &ServiceConfig) -> Option<String> {
    service_origin(w, service).map(|origin| format!("{origin}/"))
}

fn url_port(url: &str) -> Option<u16> {
    let rest = ["localhost:", "127.0.0.1:", "[::1]:"]
        .iter()
        .find_map(|host| url.find(host).map(|i| &url[i + host.len()..]))?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    rest[..digits].parse().ok()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Allows for start times being read to the second, and rounded.
const START_SLACK_SECS: u64 = 2;

/// How `running` is told apart from a later process that reused its pid. The command line isn't
/// compared: on Unix the shell execs the command, so `PORT=1 npm run dev` runs as
/// `node /usr/bin/npm run dev`.
fn start_check(running: &Running) -> process::StartCheck {
    match running.start_token {
        Some(token) => process::StartCheck::Token(token),
        None => process::StartCheck::NoLaterThan(running.started + START_SLACK_SECS),
    }
}

/// Which of `records` still run. A process whose identity can't be read counts.
fn alive(records: &[&Running]) -> Vec<bool> {
    let checks: Vec<_> = records.iter().map(|r| (r.pid, start_check(r))).collect();
    match process::identify(&checks) {
        Ok(identities) => identities
            .into_iter()
            .map(|i| i != process::Identity::Gone)
            .collect(),
        Err(_) => records
            .iter()
            .map(|r| process::process_exists(r.pid))
            .collect(),
    }
}

fn still_alive(running: &Running) -> bool {
    alive(&[running])[0]
}

/// Waits for `service` to accept connections on `port`, failing early if its process exits, and
/// once `ready_timeout_secs` have passed since it was started.
pub(crate) fn wait_until_listening(
    paths: &Paths,
    worker: &str,
    service: &ServiceConfig,
    port: u16,
    mut running: Running,
) -> Result<()> {
    let name = &service.name;
    let timeout = Duration::from_secs(service.ready_timeout_secs);
    let deadline_for = |r: &Running| {
        let elapsed = Duration::from_secs(now_secs().saturating_sub(r.started));
        Instant::now() + timeout.saturating_sub(elapsed)
    };
    let mut deadline = deadline_for(&running);
    let mut next_check = Instant::now() + Duration::from_secs(3);
    loop {
        if port_open(port) {
            return Ok(());
        }
        if Instant::now() >= next_check {
            if !process::process_exists(running.pid) {
                let restarted = read(paths, worker)
                    .remove(name)
                    .filter(|r| *r != running && still_alive(r));
                let Some(restarted) = restarted else {
                    bail!(
                        "service '{name}' exited before listening on port {port}:
{}",
                        tail(&running.log, 15)
                    );
                };
                deadline = deadline_for(&restarted);
                running = restarted;
            }
            next_check = Instant::now() + Duration::from_secs(3);
        }
        if Instant::now() >= deadline {
            bail!(
                "service '{name}' is not listening on port {port} after {}s:
{}",
                service.ready_timeout_secs,
                tail(&running.log, 15)
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

impl Workspace {
    fn service_port(w: &WorkerRecord, service: &ServiceConfig) -> Option<u16> {
        service.port.as_ref().and_then(|p| w.ports.get(p)).copied()
    }

    /// The service serving `url`, a URL on one of the worker's ports.
    pub fn service_for_url(&self, w: &WorkerRecord, url: &str) -> Option<String> {
        let port = url_port(url)?;
        let project = self.project_of(w).ok()?;
        project
            .services
            .iter()
            .find(|s| Self::service_port(w, s) == Some(port))
            .map(|s| s.name.clone())
    }

    /// The service serving `url` and its port, when nothing is listening there.
    pub fn stopped_service_for_url(&self, id: &str, url: &str) -> Result<Option<(String, u16)>> {
        let w = self.worker(id)?;
        let (Some(port), Some(name)) = (url_port(url), self.service_for_url(&w, url)) else {
            return Ok(None);
        };
        Ok((!port_open(port)).then_some((name, port)))
    }

    pub fn services(&self, id: &str) -> Result<Vec<ServiceStatus>> {
        let w = self.worker(id)?;
        let project = self.project_of(&w)?;
        let record = read(&self.config.paths, id);
        let alive: BTreeMap<&str, bool> = record
            .keys()
            .map(String::as_str)
            .zip(alive(&record.values().collect::<Vec<_>>()))
            .collect();
        Ok(project
            .services
            .iter()
            .map(|s| {
                let port = Self::service_port(&w, s);
                let started = record.get(&s.name);
                let alive = alive.get(s.name.as_str()).copied().unwrap_or(false);
                let running = match port {
                    Some(port) => port_open(port),
                    None => alive,
                };
                let state = match (running, alive) {
                    (true, _) => ServiceState::Running,
                    (false, true) => ServiceState::Starting,
                    (false, false) => ServiceState::Stopped,
                };
                ServiceStatus {
                    name: s.name.clone(),
                    port,
                    url: service_url(&w, s),
                    needs: s.needs.clone(),
                    running,
                    state,
                    pid: started.filter(|_| alive).map(|r| r.pid),
                    log: started.map(|r| r.log.clone()),
                }
            })
            .collect())
    }

    /// Starts `name` and the services it needs, skipping any already running: on their port, or
    /// started by Tworky and not yet listening. With `wait`, a service that isn't listening yet is
    /// waited for before anything after it is started, and `name` itself is waited for; a
    /// dependency isn't waited for when nothing after it has to be started.
    pub fn start_service(
        &self,
        id: &str,
        name: &str,
        wait: bool,
        log: Log,
    ) -> Result<Vec<(String, Outcome)>> {
        let w = self.worker(id)?;
        let project = self.project_of(&w)?;
        let order = project.service_order(name)?;
        let mut outcomes = Vec::new();
        let mut not_listening: Vec<(&ServiceConfig, u16, Running, Outcome)> = Vec::new();
        let wait_for =
            |(service, port, running, outcome): (&ServiceConfig, u16, Running, Outcome),
             log: Log|
             -> Result<()> {
                if outcome == Outcome::AlreadyRunning {
                    log(&format!(
                        "▸ service {}: waiting for port {port} (pid {})",
                        service.name, running.pid
                    ));
                }
                wait_until_listening(&self.config.paths, id, service, port, running)?;
                log(&format!("✓ {} is up", service.name));
                Ok(())
            };
        for &service in &order {
            let port = Self::service_port(&w, service);
            let outcome = loop {
                let starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
                let mut record = read(&self.config.paths, id);
                if port.is_some_and(port_open) {
                    break Outcome::AlreadyRunning;
                }
                if let Some(running) = record.get(&service.name).filter(|r| still_alive(r)) {
                    if let Some(port) = port {
                        not_listening.push((
                            service,
                            port,
                            running.clone(),
                            Outcome::AlreadyRunning,
                        ));
                    }
                    break Outcome::AlreadyRunning;
                }
                if wait && !not_listening.is_empty() {
                    drop(starting);
                    for dependency in not_listening.drain(..) {
                        wait_for(dependency, log)?;
                    }
                    continue;
                }
                let running = self.spawn(&w, project, service)?;
                log(&format!(
                    "▸ service {}: {} (pid {})",
                    service.name, running.command, running.pid
                ));
                record.insert(service.name.clone(), running.clone());
                write(&self.config.paths, id, &record)?;
                match port {
                    Some(port) => not_listening.push((service, port, running, Outcome::Started)),
                    None if wait => log(&format!("✓ {} started", service.name)),
                    None => {}
                }
                break Outcome::Started;
            };
            outcomes.push((service.name.clone(), outcome));
        }
        let target = order.last().map(|s| s.name.as_str());
        if let Some(waiting) = not_listening
            .pop()
            .filter(|n| wait && Some(n.0.name.as_str()) == target)
        {
            wait_for(waiting, log)?;
        }
        Ok(outcomes)
    }

    fn spawn(
        &self,
        w: &WorkerRecord,
        project: &ProjectConfig,
        service: &ServiceConfig,
    ) -> Result<Running> {
        let Some(run) = service.run.for_current_os() else {
            bail!(
                "service '{}' has no command for {}",
                service.name,
                std::env::consts::OS
            );
        };
        let first = service.cwd.split(['/', '\\']).next().unwrap_or_default();
        if project
            .repos
            .iter()
            .any(|r| r.name.eq_ignore_ascii_case(first))
            && !w
                .worktrees
                .iter()
                .any(|t| t.repo.eq_ignore_ascii_case(first))
        {
            bail!(
                "service '{}' runs in {first}, which has no worktree yet; add it with `tw worktree add {first}`",
                service.name
            );
        }
        let cwd = w.dir.join(&service.cwd);
        if !cwd.is_dir() {
            bail!(
                "service '{}': {} does not exist",
                service.name,
                cwd.display()
            );
        }
        let ctx = template_context(w, project, &IssueIndex::default());
        let command = template::render_str(run, &ctx)?;
        let log = log_file(&self.config.paths, &w.id, &service.name);
        std::fs::create_dir_all(log.parent().expect("log has a folder"))?;
        let out = std::fs::File::create(&log)?;
        let err = out.try_clone()?;
        let shell = &self.config.app.shell;
        let mut cmd = process::command(&shell[0]);
        cmd.args(&shell[1..])
            .arg(&command)
            .current_dir(&cwd)
            .envs(worker_env(w))
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        process::keep_std_handles_from_children();
        let child = cmd
            .spawn()
            .with_context(|| format!("starting service '{}'", service.name))?;
        Ok(Running {
            pid: child.id(),
            command,
            log,
            started: now_secs(),
            start_token: process::child_start_token(&child),
        })
    }

    /// Fails when the process had already exited or been replaced, but forgets it either way.
    pub fn stop_service(&self, id: &str, name: &str, log: Log) -> Result<()> {
        let pid = self.stop_recorded(id, name, log)?;
        if let Some(pid) = pid {
            bail!("service '{name}' was no longer running (pid {pid}); Tworky has forgotten it");
        }
        Ok(())
    }

    /// Returns the pid when the service was no longer running; it is forgotten either way.
    fn stop_recorded(&self, id: &str, name: &str, log: Log) -> Result<Option<u32>> {
        let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
        let mut record = read(&self.config.paths, id);
        let Some(running) = record.remove(name) else {
            bail!("service '{name}' was not started by Tworky");
        };
        let stopped = process::stop_tree_started_by(
            running.pid,
            &running.command,
            start_check(&running),
            log,
        )?;
        if stopped {
            log(&format!("▸ stopped service {name} (pid {})", running.pid));
        }
        write(&self.config.paths, id, &record)?;
        Ok((!stopped).then_some(running.pid))
    }

    /// Unlike `stop_service`, doesn't fail for a service that had already exited.
    pub fn stop_services(&self, id: &str, log: Log) -> Result<()> {
        for name in read(&self.config.paths, id).into_keys() {
            self.stop_recorded(id, &name, log)?;
        }
        Ok(())
    }

    /// Forgets the worker's services once something else has stopped them.
    pub(crate) fn forget_services(&self, id: &str) -> Result<()> {
        let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
        write(&self.config.paths, id, &Record::new())
    }

    /// The log of the worker's service whose process is one of `pids`.
    pub fn service_log(&self, id: &str, pids: &[u32]) -> Option<PathBuf> {
        read(&self.config.paths, id)
            .into_values()
            .find(|r| pids.contains(&r.pid))
            .map(|r| r.log)
    }

    /// Workers with services Tworky started, for stopping them when the app closes.
    pub fn workers_with_services(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir(&self.config.paths)) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(str::to_string)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_port_in_a_local_url() {
        assert_eq!(url_port("http://localhost:5273/clients"), Some(5273));
        assert_eq!(url_port("http://127.0.0.1:8181"), Some(8181));
        assert_eq!(url_port("http://[::1]:4100/x"), Some(4100));
        assert_eq!(url_port("https://example.com:5273/"), None);
        assert_eq!(url_port("http://localhost/"), None);
    }

    #[test]
    fn sees_a_listening_port() {
        use socket2::{Domain, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket
            .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        assert!(!port_open(port));
        socket.listen(1).unwrap();
        assert!(port_open(port));
    }

    #[test]
    fn only_service_logs_are_readable() {
        let paths = Paths {
            config_dir: PathBuf::from("/c"),
            data_dir: PathBuf::from("/d"),
        };
        assert!(is_log(&paths, Path::new("/d/services/w/web.log")));
        assert!(!is_log(&paths, Path::new("/d/services/w.json")));
        assert!(!is_log(&paths, Path::new("/d/state.log")));
        assert!(!is_log(&paths, Path::new("/d/services/../secret.log")));
    }

    #[test]
    fn a_record_with_a_start_token_is_checked_by_it() {
        let mut running = Running {
            pid: 7,
            command: "vite".into(),
            log: PathBuf::from("web.log"),
            started: 1_000,
            start_token: None,
        };
        assert_eq!(
            start_check(&running),
            process::StartCheck::NoLaterThan(1_000 + START_SLACK_SECS)
        );
        running.start_token = Some(42);
        assert_eq!(start_check(&running), process::StartCheck::Token(42));
    }

    #[test]
    fn a_record_without_a_start_token_still_reads() {
        let old = r#"{"web":{"pid":7,"command":"vite","log":"web.log","started":1}}"#;
        let record: Record = serde_json::from_str(old).unwrap();
        assert_eq!(record["web"].start_token, None);
    }

    #[test]
    fn records_round_trip_and_an_empty_one_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("c"),
            data_dir: dir.path().join("d"),
        };
        let mut record = Record::new();
        record.insert(
            "web".into(),
            Running {
                pid: 7,
                command: "vite".into(),
                log: log_file(&paths, "w", "web"),
                started: 1,
                start_token: Some(9),
            },
        );
        write(&paths, "w", &record).unwrap();
        assert_eq!(read(&paths, "w"), record);
        write(&paths, "w", &Record::new()).unwrap();
        assert!(!record_file(&paths, "w").exists());
        assert!(read(&paths, "w").is_empty());
    }
}
