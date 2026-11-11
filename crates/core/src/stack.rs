use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::StackConfig;
use crate::process;

pub struct Compose {
    pub project_name: String,
    pub dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub env_file: Option<PathBuf>,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    #[serde(rename = "Service", default)]
    pub service: String,
    #[serde(rename = "State", default)]
    pub state: String,
    #[serde(rename = "Health", default)]
    pub health: String,
    #[serde(rename = "Publishers", default)]
    pub publishers: Option<Vec<Publisher>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publisher {
    #[serde(rename = "PublishedPort", default)]
    pub published_port: u16,
    #[serde(rename = "TargetPort", default)]
    pub target_port: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerStatus {
    pub service: String,
    pub state: String,
    pub health: Option<String>,
    pub ports: Vec<ContainerPort>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContainerPort {
    pub published: u16,
    pub target: u16,
}

impl From<ServiceStatus> for ContainerStatus {
    /// Compose lists a port once per address family, and an exposed but unpublished one as port 0.
    fn from(s: ServiceStatus) -> Self {
        let mut ports: Vec<ContainerPort> = Vec::new();
        for p in s.publishers.into_iter().flatten() {
            let port = ContainerPort {
                published: p.published_port,
                target: p.target_port,
            };
            if port.published != 0 && !ports.contains(&port) {
                ports.push(port);
            }
        }
        Self {
            service: s.service,
            state: s.state,
            health: Some(s.health).filter(|h| !h.is_empty()),
            ports,
        }
    }
}

impl Compose {
    pub fn new(project_name: &str, worker_dir: &Path, cfg: &StackConfig) -> Self {
        Self {
            project_name: project_name.to_string(),
            dir: worker_dir.join(&cfg.compose_dir),
            files: cfg
                .compose_files
                .iter()
                .map(|f| worker_dir.join(f))
                .collect(),
            env_file: cfg.env_file.as_ref().map(|f| worker_dir.join(f)),
            profiles: cfg.profiles.clone(),
        }
    }

    /// Without the stack config (e.g. config edited after the worker was made), `down` still works by project name.
    pub fn by_name(project_name: &str) -> Self {
        Self {
            project_name: project_name.to_string(),
            dir: std::env::temp_dir(),
            files: vec![],
            env_file: None,
            profiles: vec![],
        }
    }

    pub fn files_exist(&self) -> bool {
        self.dir.exists()
            && self.files.iter().all(|f| f.exists())
            && self.env_file.as_ref().is_none_or(|f| f.exists())
    }

    fn base(&self) -> (Command, Vec<String>) {
        let mut args: Vec<String> = vec!["compose".into(), "-p".into(), self.project_name.clone()];
        args.push("--project-directory".into());
        args.push(self.dir.to_string_lossy().into_owned());
        for f in &self.files {
            args.push("-f".into());
            args.push(f.to_string_lossy().into_owned());
        }
        if let Some(env) = &self.env_file {
            args.push("--env-file".into());
            args.push(env.to_string_lossy().into_owned());
        }
        for p in &self.profiles {
            args.push("--profile".into());
            args.push(p.clone());
        }
        let mut cmd = process::command("docker");
        cmd.current_dir(&self.dir);
        (cmd, args)
    }

    fn stream(&self, extra: &[&str], log: &mut dyn FnMut(&str)) -> Result<()> {
        let (mut cmd, mut args) = self.base();
        args.extend(extra.iter().map(|s| s.to_string()));
        let label = format!("docker {}", args.join(" "));
        cmd.args(&args);
        process::stream(cmd, &label, log)
    }

    pub fn up(&self, log: &mut dyn FnMut(&str)) -> Result<()> {
        self.stream(&["up", "-d", "--wait"], log)
    }

    pub fn stop(&self, log: &mut dyn FnMut(&str)) -> Result<()> {
        unless_docker_missing(self.stream(&["stop"], log), log)
    }

    pub fn down(&self, log: &mut dyn FnMut(&str)) -> Result<()> {
        unless_docker_missing(self.stream(&["down", "-v", "--remove-orphans"], log), log)
    }

    pub fn ps(&self, timeout: Duration) -> Result<Vec<ServiceStatus>> {
        let mut cmd = process::command("docker");
        cmd.args([
            "compose",
            "-p",
            &self.project_name,
            "ps",
            "--all",
            "--format",
            "json",
        ]);
        parse_ps(&process::output_timeout(cmd, "docker compose ps", timeout)?)
    }
}

/// Compose prints one JSON object per line in current versions and a single array in older ones.
fn parse_ps(text: &str) -> Result<Vec<ServiceStatus>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    if trimmed.starts_with('[') {
        return Ok(serde_json::from_str(trimmed)?);
    }
    trimmed
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

/// With no docker there can be no containers to stop or remove. The working directory always exists
/// (`files_exist`, or the temp folder for `by_name`), so a spawn failing with NotFound means docker.
fn unless_docker_missing(result: Result<()>, log: &mut dyn FnMut(&str)) -> Result<()> {
    match result {
        Err(e) if docker_missing(&e) => {
            log("docker isn't installed, so there's no stack to stop");
            Ok(())
        }
        r => r,
    }
}

pub(crate) fn docker_missing(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

pub fn compose_project_name(worker_id: &str) -> String {
    let name: String = worker_id
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("tw-{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_docker_has_no_stack_to_stop() {
        let missing = process::stream(
            process::command("tworky-no-such-program"),
            "tworky-no-such-program",
            &mut |_| {},
        );
        let mut logged = vec![];
        unless_docker_missing(missing, &mut |l| logged.push(l.to_string())).unwrap();
        assert_eq!(logged.len(), 1);

        let failed: Result<()> = Err(anyhow::anyhow!("`docker compose down` failed (exit 1)"));
        assert!(unless_docker_missing(failed, &mut |_| {}).is_err());
    }

    #[test]
    fn parses_both_ps_formats() {
        let lines = r#"{"Service":"postgres","State":"running","Health":"healthy","Publishers":[{"PublishedPort":5532,"TargetPort":5432}]}
{"Service":"redis","State":"running","Health":""}"#;
        let a = parse_ps(lines).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].publishers.as_ref().unwrap()[0].published_port, 5532);
        let b = parse_ps(r#"[{"Service":"redis","State":"exited"}]"#).unwrap();
        assert_eq!(b[0].state, "exited");
        assert!(parse_ps("\n").unwrap().is_empty());
    }

    #[test]
    fn output_timeout_keeps_a_missing_docker_recognisable() {
        let missing = process::output_timeout(
            process::command("tworky-no-such-program"),
            "tworky-no-such-program",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(docker_missing(&missing));
    }

    #[test]
    fn maps_a_ps_line_to_a_container_status() {
        let line = r#"{"Service":"postgres","State":"running","Health":"healthy","Publishers":[{"PublishedPort":5532,"TargetPort":5432},{"PublishedPort":5532,"TargetPort":5432},{"PublishedPort":0,"TargetPort":8080}]}"#;
        let status: ContainerStatus = parse_ps(line).unwrap().remove(0).into();
        assert_eq!(
            status,
            ContainerStatus {
                service: "postgres".into(),
                state: "running".into(),
                health: Some("healthy".into()),
                ports: vec![ContainerPort {
                    published: 5532,
                    target: 5432
                }],
            }
        );
        let bare: ContainerStatus = parse_ps(r#"{"Service":"redis","State":"exited","Health":""}"#)
            .unwrap()
            .remove(0)
            .into();
        assert_eq!(bare.health, None);
        assert!(bare.ports.is_empty());
    }

    #[test]
    fn sanitises_project_name() {
        assert_eq!(compose_project_name("ACME-318.x"), "tw-acme-318-x");
    }
}
