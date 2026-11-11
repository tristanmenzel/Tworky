use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::config::{ProjectConfig, ServiceConfig};
use crate::git;
use crate::ports::PortMap;
use crate::process::{self, Listener};
use crate::services::ServiceStatus;
use crate::stack::{self, Compose, ContainerStatus};
use crate::state::WorkerRecord;
use crate::worker::Workspace;

const PS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize)]
pub struct LocalStatus {
    pub worktrees: Vec<LocalWorktree>,
    pub services: Vec<ServiceStatus>,
    /// `None` when the project has no stack or docker isn't installed.
    pub stack: Option<Vec<ContainerStatus>>,
    pub stack_error: Option<String>,
    pub listeners: Vec<PortListener>,
}

/// Something listening on one of the worker's ports that isn't a service or the stack, such as a
/// dev server started by hand.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PortListener {
    /// The port's name in the project's `[ports]`.
    pub name: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalWorktree {
    pub repo: String,
    pub branch: String,
    pub dirty_files: usize,
    pub unpushed: usize,
    pub error: Option<String>,
}

/// Claude may have switched branches since `tw worktree add`, so this can differ from the record.
pub(crate) fn checked_out(worktree: &Path) -> Option<String> {
    if !worktree.exists() {
        return None;
    }
    git::current_branch(worktree)
        .ok()
        .filter(|b| !b.is_empty() && b != "HEAD")
}

impl Workspace {
    pub fn worker_local(&self, id: &str) -> Result<LocalStatus> {
        let w = self.worker(id)?;
        let worktrees = w
            .worktrees
            .iter()
            .map(|t| {
                let dir = w.dir.join(&t.repo);
                let status = if dir.exists() {
                    git::status(&dir).map_err(|e| format!("{e:#}"))
                } else {
                    Err(format!("{} is missing", dir.display()))
                };
                let (dirty_files, unpushed, error) = match status {
                    Ok(s) => (s.dirty_files, s.unpushed_commits, None),
                    Err(e) => (0, 0, Some(e)),
                };
                LocalWorktree {
                    repo: t.repo.clone(),
                    branch: checked_out(&dir).unwrap_or_else(|| t.branch.clone()),
                    dirty_files,
                    unpushed,
                    error,
                }
            })
            .collect();
        let project = self.config.project(&w.project);
        let services = match project {
            Some(_) => self.services(id)?,
            None => vec![],
        };
        let (stack, stack_error) = stack_containers(&w, project);
        let listeners = match project {
            Some(project) => listeners(
                &w,
                project,
                stack.as_deref().unwrap_or_default(),
                stack_error.is_some(),
            )
            .unwrap_or_default(),
            None => vec![],
        };
        Ok(LocalStatus {
            worktrees,
            services,
            stack,
            stack_error,
            listeners,
        })
    }

    /// What listens on the worker's ports that no service owns and its stack doesn't publish.
    pub fn port_listeners(&self, id: &str) -> Result<Vec<PortListener>> {
        let w = self.worker(id)?;
        let project = self.project_of(&w)?;
        let (stack, stack_error) = stack_containers(&w, Some(project));
        listeners(
            &w,
            project,
            stack.as_deref().unwrap_or_default(),
            stack_error.is_some(),
        )
    }
}

fn stack_containers(
    w: &WorkerRecord,
    project: Option<&ProjectConfig>,
) -> (Option<Vec<ContainerStatus>>, Option<String>) {
    match project.and_then(|p| p.stack.as_ref()) {
        None => (None, None),
        Some(_) => match Compose::by_name(&w.compose_project).ps(PS_TIMEOUT) {
            Ok(containers) => (
                Some(containers.into_iter().map(ContainerStatus::from).collect()),
                None,
            ),
            Err(e) if stack::docker_missing(&e) => (None, None),
            Err(e) => (None, Some(format!("{e:#}"))),
        },
    }
}

fn listeners(
    w: &WorkerRecord,
    project: &ProjectConfig,
    stack: &[ContainerStatus],
    stack_unknown: bool,
) -> Result<Vec<PortListener>> {
    let ports = unowned_ports(&w.ports, &project.services, stack);
    let numbers: Vec<u16> = ports.iter().map(|(_, port)| *port).collect();
    Ok(name_listeners(
        &ports,
        process::listeners(&numbers)?,
        stack_unknown,
    ))
}

/// The worker's ports, by name, less those a service runs on or a container publishes.
fn unowned_ports(
    ports: &PortMap,
    services: &[ServiceConfig],
    stack: &[ContainerStatus],
) -> Vec<(String, u16)> {
    let owned = |name: &str, port: u16| {
        services.iter().any(|s| s.port.as_deref() == Some(name))
            || stack
                .iter()
                .any(|c| c.ports.iter().any(|p| p.published == port))
    };
    ports
        .iter()
        .filter(|(name, port)| !owned(name, **port))
        .map(|(name, port)| (name.clone(), *port))
        .collect()
}

/// With `stack_unknown` (the stack's containers couldn't be listed), ports Docker holds are taken
/// to be the stack's.
fn name_listeners(
    ports: &[(String, u16)],
    found: Vec<Listener>,
    stack_unknown: bool,
) -> Vec<PortListener> {
    found
        .into_iter()
        .filter(|l| !(stack_unknown && l.name.as_deref().is_some_and(is_docker_port_proxy)))
        .filter_map(|l| {
            let (name, _) = ports.iter().find(|(_, port)| *port == l.port)?;
            Some(PortListener {
                name: name.clone(),
                port: l.port,
                pid: l.pid,
                command: l.command,
            })
        })
        .collect()
}

/// What holds a container's published port on the host: Docker Desktop's backend (and on Windows
/// its WSL relay, whose command line doesn't name it), `vpnkit` in older Docker Desktops, or
/// `docker-proxy` on Linux.
fn is_docker_port_proxy(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.trim_end_matches(".exe"),
        "com.docker.backend"
            | "wslrelay"
            | "vpnkit"
            | "vpnkit-bridge"
            | "com.docker.vpnkit"
            | "docker-proxy"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookCommand;
    use crate::stack::ContainerPort;

    fn service(name: &str, port: &str) -> ServiceConfig {
        ServiceConfig {
            name: name.into(),
            cwd: ".".into(),
            run: HookCommand::Everywhere("npm run dev".into()),
            port: Some(port.into()),
            https: false,
            needs: vec![],
            ready_timeout_secs: 120,
        }
    }

    #[test]
    fn listeners_skip_service_and_stack_ports() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free = held.local_addr().unwrap().port();
        let web = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let web_port = web.local_addr().unwrap().port();
        let ports: PortMap = [
            ("vite".to_string(), free),
            ("web".to_string(), web_port),
            ("db".to_string(), 1),
        ]
        .into();
        let stack = [ContainerStatus {
            service: "db".into(),
            state: "running".into(),
            health: None,
            ports: vec![ContainerPort {
                published: 1,
                target: 5432,
            }],
        }];
        let unowned = unowned_ports(&ports, &[service("web", "web")], &stack);
        assert_eq!(unowned, vec![("vite".to_string(), free)]);

        let numbers: Vec<u16> = unowned.iter().map(|(_, p)| *p).collect();
        let found = name_listeners(&unowned, process::listeners(&numbers).unwrap(), true);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].name.as_str(), found[0].port), ("vite", free));
        assert_eq!(found[0].pid, Some(std::process::id()));
        assert!(found[0].command.is_some());
    }

    #[test]
    fn leaves_out_docker_held_ports_only_when_the_stack_is_unknown() {
        let ports = [("db".to_string(), 5433), ("vite".to_string(), 5174)];
        let listener = |port, name: &str| Listener {
            port,
            pid: Some(1),
            name: Some(name.into()),
            command: Some(name.into()),
        };
        let found = || {
            vec![
                listener(5433, "com.docker.backend.exe"),
                listener(5433, "wslrelay.exe"),
                listener(5433, "docker-proxy"),
                listener(5174, "node.exe"),
            ]
        };
        let named = |stack_unknown| {
            name_listeners(&ports, found(), stack_unknown)
                .into_iter()
                .map(|l| l.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(named(true), ["vite"]);
        assert_eq!(named(false), ["db", "db", "db", "vite"]);
    }
}
