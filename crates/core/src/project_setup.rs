use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;

use crate::claude_config;
use crate::config::{Config, ProjectConfig};
use crate::git;
use crate::state::{ProjectSession, WorkerRecord, WorkerStatus};
use crate::template;
use crate::worker::{Launch, Workspace, session_settings};

const SESSION_PREFIX: &str = "setup@";

/// The id a project's setup session runs under, in the session table and activity files. Worker
/// ids never contain `@`.
pub fn session_id(project: &str) -> String {
    format!("{SESSION_PREFIX}{project}")
}

/// The project a setup session id is for.
pub fn session_project(id: &str) -> Option<&str> {
    id.strip_prefix(SESSION_PREFIX)
}

pub fn setup_dir(config: &Config, project: &str) -> PathBuf {
    config.paths.data_dir.join("project-setup").join(project)
}

pub fn claude_md(config: &Config, project: &ProjectConfig) -> Result<String> {
    let ctx = json!({
        "project": project,
        "project_file": project.file,
        "workers_root": config.app.workers_root.to_string_lossy().replace('\\', "/"),
        "port_stride": config.app.port_stride,
        "template_dir": config.template_dir(project),
        "memory_file": config.paths.memory_file(&project.name),
        "shell": config.app.shell.join(" "),
        "example_acme": include_str!("../../../examples/acme/acme.toml").trim(),
        "example_tworky": include_str!("../../../examples/tworky/tworky.toml").trim(),
    });
    template::render_str(include_str!("project_setup.md"), &ctx)
}

impl Workspace {
    fn configured(&self, name: &str) -> Result<&ProjectConfig> {
        self.config
            .project(name)
            .with_context(|| format!("no project named '{name}'"))
    }

    fn project_session(&self, name: &str) -> Result<ProjectSession> {
        Ok(self
            .state
            .project_session(name)?
            .unwrap_or_else(|| ProjectSession {
                claude_session: uuid::Uuid::new_v4().to_string(),
                claude_started: false,
            }))
    }

    /// Writes the setup folder's CLAUDE.md and builds the command for its session: it resumes the
    /// last conversation, or starts one with a prompt to set up or adjust the project.
    pub fn launch_project_setup(&self, name: &str) -> Result<Launch> {
        let project = self.configured(name)?;
        let dir = setup_dir(&self.config, name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join("CLAUDE.md"), claude_md(&self.config, project)?)?;

        let mut session = self.project_session(name)?;
        let mut replaced_missing = false;
        if session.claude_started
            && let Some(home) = &self.claude_home
            && !claude_config::transcript_path(home, &dir, &session.claude_session).exists()
        {
            session = ProjectSession {
                claude_session: uuid::Uuid::new_v4().to_string(),
                claude_started: false,
            };
            replaced_missing = true;
        }
        self.state.save_project_session(name, &session)?;

        let memory = self.config.paths.memory_file(name);
        let mut args: Vec<String> = self.config.app.claude[1..].to_vec();
        let mut settings: serde_json::Value = serde_json::from_str(&session_settings(
            name,
            &memory,
            false,
            false,
            Some(&session_id(name)),
        ))?;
        if let Some(allow) = settings["permissions"]["allow"].as_array_mut() {
            allow.push(json!("Bash(tw:*)"));
        }
        args.extend(["--settings".into(), settings.to_string()]);
        let readable = [
            self.config.paths.projects_dir(),
            self.config.template_dir(project),
        ]
        .into_iter()
        .chain(project.repos.iter().map(|r| r.source.clone()));
        for d in readable {
            std::fs::create_dir_all(&d).ok();
            args.extend(["--add-dir".into(), d.to_string_lossy().into_owned()]);
        }
        if session.claude_started {
            args.extend(["--resume".into(), session.claude_session.clone()]);
        } else {
            let prompt = if project.onboarding {
                format!("Set up the Tworky project {name}, following CLAUDE.md.")
            } else {
                format!(
                    "I want to adjust the Tworky project {name}. Read CLAUDE.md and the current config, then ask me what to change."
                )
            };
            args.extend([
                "--session-id".into(),
                session.claude_session.clone(),
                prompt,
            ]);
        }
        let mut env = std::collections::BTreeMap::new();
        env.insert("TW_PROJECT".into(), name.to_string());
        Ok(Launch {
            program: self.config.app.claude[0].clone(),
            args,
            cwd: dir,
            env,
            replaced_missing,
        })
    }

    pub fn mark_project_session_started(&self, name: &str) -> Result<()> {
        let mut session = self.project_session(name)?;
        session.claude_started = true;
        self.state.save_project_session(name, &session)
    }

    /// The next launch starts a new conversation instead of resuming.
    pub fn reset_project_session(&self, name: &str) -> Result<()> {
        self.state.save_project_session(
            name,
            &ProjectSession {
                claude_session: uuid::Uuid::new_v4().to_string(),
                claude_started: false,
            },
        )
    }

    /// Marks the project as set up, removing `onboarding` from its config.
    pub fn finish_onboarding(&self, name: &str) -> Result<()> {
        let project = self.configured(name)?;
        let text = std::fs::read_to_string(&project.file)?;
        let mut doc: toml_edit::DocumentMut = text.parse()?;
        if doc.remove("onboarding").is_none() {
            return Ok(());
        }
        let out = doc.to_string();
        toml::from_str::<ProjectConfig>(&out).context("the edited project config")?;
        let tmp = project.file.with_extension("toml.tmp");
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, &project.file)
            .with_context(|| format!("writing {}", project.file.display()))
    }

    /// Problems with a project's config that loading it doesn't catch, found by trying it on this
    /// machine: missing clones, templates that don't render, hooks with no command for an OS.
    pub fn check_project(&self, name: &str) -> Result<Vec<Finding>> {
        let project = self.configured(name)?;
        let mut found = Vec::new();
        let mut problem = |level: Level, text: String| found.push(Finding { level, text });

        if project.repos.is_empty() {
            problem(
                Level::Error,
                "no [[repos]]: workers would have nothing to check out".into(),
            );
        }
        for repo in &project.repos {
            if !repo.source.join(".git").exists() {
                problem(
                    Level::Error,
                    format!(
                        "repo {}: {} is not a git clone",
                        repo.name,
                        repo.source.display()
                    ),
                );
                continue;
            }
            match git::origin_name_with_owner(&repo.source) {
                Ok(nwo)
                    if self.config.project_for_repo(&nwo).map(|p| &p.name)
                        != Some(&project.name) =>
                {
                    problem(
                        Level::Warning,
                        format!(
                            "repo {}: {nwo} isn't matched by [match], so its issues and PRs won't route here",
                            repo.name
                        ),
                    )
                }
                Err(e) => problem(Level::Warning, format!("repo {}: {e:#}", repo.name)),
                _ => {}
            }
        }

        let sample = sample_worker(&self.config, project);
        let ctx = crate::worker::template_context(&sample, project, &Default::default());
        let templates = self.config.template_dir(project);
        let scratch = std::env::temp_dir().join(format!("tworky-check-{}", uuid::Uuid::new_v4()));
        for repo in &project.repos {
            let dir = templates.join(&repo.name);
            if let Err(e) = template::render_dir(&dir, &scratch.join(&repo.name), &ctx) {
                problem(Level::Error, format!("templates for {}: {e:#}", repo.name));
            }
        }
        let _ = std::fs::remove_dir_all(&scratch);
        if templates.is_dir() {
            for entry in std::fs::read_dir(&templates)?.flatten() {
                let folder = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() && !project.repos.iter().any(|r| r.name == folder) {
                    problem(
                        Level::Warning,
                        format!("templates/{folder} matches no repo name, so it's never rendered"),
                    );
                }
            }
        }

        let hooks = project.hooks.setup.iter().chain(&project.hooks.teardown);
        let commands = hooks.map(|h| (format!("hook in {}", h.cwd), &h.run)).chain(
            project
                .services
                .iter()
                .map(|s| (format!("service {}", s.name), &s.run)),
        );
        for (label, run) in commands {
            for os in ["windows", "macos", "linux"] {
                if run.for_os(os).is_none() {
                    problem(Level::Warning, format!("{label} has no command for {os}"));
                }
            }
            if let Some(cmd) = run.for_current_os()
                && let Err(e) = template::render_str(cmd, &ctx)
            {
                problem(Level::Error, format!("{label}: {e:#}"));
            }
        }
        for s in &project.services {
            if s.port.is_none() {
                problem(
                    Level::Warning,
                    format!(
                        "service {} has no port, so Tworky can't tell when it's ready",
                        s.name
                    ),
                );
            }
        }
        for a in &project.accounts {
            let has_url = !a.start_url.trim().is_empty();
            let has_service = a
                .start_service
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
            if has_url == has_service {
                let which = if has_url {
                    "both start_url and"
                } else {
                    "neither start_url nor"
                };
                problem(
                    Level::Error,
                    format!(
                        "account {} has {which} start_service; it needs exactly one",
                        a.name
                    ),
                );
            }
            for why in crate::accounts::unusable_services(project, &project.ports, a) {
                problem(Level::Error, format!("{why}, so workers leave it out"));
            }
        }
        if !project.ports.is_empty() && project.services.is_empty() {
            let unless = if project.stack.is_some() {
                ", unless the stack publishes them all"
            } else {
                ""
            };
            problem(
                Level::Warning,
                format!(
                    "[ports] but no [[services]]: Tworky can't start the dev servers on those ports{unless}, and sessions start them by hand where the user can't see or start them"
                ),
            );
        }
        for (key, port) in &project.ports {
            if crate::ports::is_browser_blocked(*port) {
                problem(
                    Level::Warning,
                    format!(
                        "port {key} = {port} is blocked by browsers (ERR_UNSAFE_PORT), so if it serves pages, the main checkout can't be opened on it in a browser tab"
                    ),
                );
            }
        }
        if project.stack.is_some() && project.ports.is_empty() {
            problem(
                Level::Warning,
                "a [stack] with no [ports]: every worker's containers would bind the same host ports".into(),
            );
        }
        if project.onboarding {
            problem(
                Level::Info,
                format!(
                    "still marked as onboarding; run `tw project done {name}` once it's tested"
                ),
            );
        }
        Ok(found)
    }
}

/// A worker on slot 1, to render templates and hooks against.
fn sample_worker(config: &Config, project: &ProjectConfig) -> WorkerRecord {
    let id = format!("{}-check", project.name);
    WorkerRecord {
        id: id.clone(),
        project: project.name.clone(),
        issue: None,
        item: Some(crate::state::ItemRef {
            name: "check".into(),
            title: "Config check".into(),
            description: String::new(),
        }),
        pr: None,
        branch: "feat/config-check".into(),
        slot: 1,
        ports: project
            .ports
            .iter()
            .map(|(k, v)| (k.clone(), v + config.app.port_stride))
            .collect(),
        dir: config.app.workers_root.join(&project.name).join("check"),
        compose_project: crate::stack::compose_project_name(&id),
        status: WorkerStatus::Ready,
        claude_session: String::new(),
        claude_started: false,
        error: None,
        created_at: 0,
        worktrees: vec![],
        stopped: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.level == Level::Error)
}

/// For `tw project check`'s caller to point at the folder a session would use.
pub fn describe(config: &Config, project: &ProjectConfig) -> Vec<(&'static str, String)> {
    let show = |p: &Path| p.display().to_string();
    vec![
        ("config", show(&project.file)),
        ("templates", show(&config.template_dir(project))),
        ("memory", show(&config.paths.memory_file(&project.name))),
        ("setup session", show(&setup_dir(config, &project.name))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_round_trip_and_never_look_like_workers() {
        assert_eq!(session_project(&session_id("acme")), Some("acme"));
        assert_eq!(session_project("acme-42"), None);
    }

    #[test]
    fn setup_claude_md_renders_for_a_new_project() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let repos = [crate::config::NewRepo {
            name_with_owner: "acme/portal".into(),
            source: dir.path().join("portal"),
            base: "origin/main".into(),
        }];
        crate::config::create_repo_project(&paths, &[], &repos, None).unwrap();
        let config = Config::load_from(paths).unwrap();
        let project = config.project("portal").unwrap();
        let md = claude_md(&config, project).unwrap();
        assert!(md.contains("# Tworky project setup: portal"));
        assert!(md.contains("This project was just created"));
        assert!(md.contains("`portal` at"));
        assert!(
            md.contains("feat/{{ issue.number }}-{{ issue.slug }}"),
            "raw blocks survive"
        );
        assert!(md.contains("name = \"acme\""), "embeds the worked examples");
        assert!(md.contains("tw project done portal"));
    }

    #[test]
    fn a_setup_session_starts_with_a_prompt_then_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let repos = [crate::config::NewRepo {
            name_with_owner: "acme/portal".into(),
            source: dir.path().join("portal"),
            base: "origin/main".into(),
        }];
        crate::config::create_repo_project(&paths, &[], &repos, None).unwrap();
        let config = Config::load_from(paths).unwrap();
        let ws = Workspace {
            state: crate::state::State::open(&config.paths.state_db()).unwrap(),
            config,
            claude_home: None,
        };

        let first = ws.launch_project_setup("portal").unwrap();
        assert!(first.cwd.join("CLAUDE.md").is_file());
        assert_eq!(first.env["TW_PROJECT"], "portal");
        assert!(first.args.contains(&"--session-id".to_string()));
        assert_eq!(
            first.args.last().unwrap(),
            "Set up the Tworky project portal, following CLAUDE.md."
        );
        let settings = &first.args[first.args.iter().position(|a| a == "--settings").unwrap() + 1];
        assert!(settings.contains("--worker 'setup@portal'"), "{settings}");
        assert!(settings.contains("Bash(tw:*)"));
        assert!(
            settings.contains("PowerShell(tw issue *)"),
            "filing is allowed: {settings}"
        );

        ws.mark_project_session_started("portal").unwrap();
        let again = ws.launch_project_setup("portal").unwrap();
        let resume = again.args.iter().position(|a| a == "--resume").unwrap();
        let session =
            first.args[first.args.iter().position(|a| a == "--session-id").unwrap() + 1].clone();
        assert_eq!(again.args[resume + 1], session);

        ws.reset_project_session("portal").unwrap();
        assert!(
            !ws.launch_project_setup("portal")
                .unwrap()
                .args
                .contains(&"--resume".to_string())
        );

        ws.finish_onboarding("portal").unwrap();
        let config = Config::load_from(ws.config.paths.clone()).unwrap();
        assert!(!config.project("portal").unwrap().onboarding);
    }

    #[test]
    fn check_reports_accounts_without_one_start_page_or_naming_a_bad_service() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let repos = [crate::config::NewRepo {
            name_with_owner: "acme/portal".into(),
            source: dir.path().join("portal"),
            base: "origin/main".into(),
        }];
        crate::config::create_repo_project(&paths, &[], &repos, None).unwrap();
        let file = paths.projects_dir().join("portal.toml");
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(
            r#"
[ports]
web = 5173

[[services]]
name = "web"
port = "web"
run = "npm run dev"

[[services]]
name = "queue"
run = "npm run queue"

[[accounts]]
name = "Tester"
start_service = "web"

[[accounts]]
name = "Ghost"
start_service = "nope"
login_origins = ["service:queue"]

[[accounts]]
name = "Nowhere"

[[accounts]]
name = "Twice"
start_url = "http://localhost:5173/"
start_service = "web"
"#,
        );
        std::fs::write(&file, text).unwrap();
        let config = Config::load_from(paths).unwrap();
        let ws = Workspace {
            state: crate::state::State::open(&config.paths.state_db()).unwrap(),
            config,
            claude_home: None,
        };
        let texts: Vec<String> = ws
            .check_project("portal")
            .unwrap()
            .into_iter()
            .filter(|f| f.text.starts_with("account "))
            .map(|f| f.text)
            .collect();
        assert_eq!(texts.len(), 4, "{texts:?}");
        assert!(texts[0].contains("Ghost names service nope, which doesn't exist"));
        assert!(texts[1].contains("Ghost names service queue, which has no port"));
        assert!(texts[2].contains("Nowhere has neither start_url nor start_service"));
        assert!(texts[3].contains("Twice has both start_url and start_service"));
    }

    #[test]
    fn check_warns_about_ports_without_services() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let repos = [crate::config::NewRepo {
            name_with_owner: "acme/portal".into(),
            source: dir.path().join("portal"),
            base: "origin/main".into(),
        }];
        crate::config::create_repo_project(&paths, &[], &repos, None).unwrap();
        let file = paths.projects_dir().join("portal.toml");
        let base = std::fs::read_to_string(&file).unwrap();
        let open = |extra: &str| {
            std::fs::write(&file, base.clone() + extra).unwrap();
            let config = Config::load_from(paths.clone()).unwrap();
            Workspace {
                state: crate::state::State::open(&config.paths.state_db()).unwrap(),
                config,
                claude_home: None,
            }
        };
        let warning = |ws: &Workspace| {
            ws.check_project("portal")
                .unwrap()
                .into_iter()
                .find(|f| f.level == Level::Warning && f.text.contains("no [[services]]"))
                .map(|f| f.text)
        };
        let warned = |ws: &Workspace| warning(ws).is_some();
        assert!(!warned(&open("")), "no ports, nothing to serve");
        let ports = "\n[ports]\nweb = 5173\n";
        assert!(!warning(&open(ports)).unwrap().contains("unless"));
        assert!(!warned(&open(&format!(
            "{ports}\n[[services]]\nname = \"web\"\nport = \"web\"\nrun = \"npm run dev\"\n"
        ))));
        let with_stack = warning(&open(&format!(
            "{ports}\n[stack]\ncompose_dir = \"portal\"\n"
        )))
        .expect("warns with a stack too");
        assert!(with_stack.contains("unless the stack publishes them all"));
    }

    #[test]
    fn check_warns_about_browser_blocked_base_ports() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let repos = [crate::config::NewRepo {
            name_with_owner: "acme/portal".into(),
            source: dir.path().join("portal"),
            base: "origin/main".into(),
        }];
        crate::config::create_repo_project(&paths, &[], &repos, None).unwrap();
        let file = paths.projects_dir().join("portal.toml");
        let base = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, base + "\n[ports]\nvite = 6000\napi = 4000\n").unwrap();
        let config = Config::load_from(paths).unwrap();
        let ws = Workspace {
            state: crate::state::State::open(&config.paths.state_db()).unwrap(),
            config,
            claude_home: None,
        };
        let blocked: Vec<_> = ws
            .check_project("portal")
            .unwrap()
            .into_iter()
            .filter(|f| f.level == Level::Warning && f.text.contains("blocked by browsers"))
            .map(|f| f.text)
            .collect();
        assert_eq!(
            blocked,
            [
                "port vite = 6000 is blocked by browsers (ERR_UNSAFE_PORT), so if it serves pages, the main checkout can't be opened on it in a browser tab"
            ]
        );
    }

    #[test]
    fn bails_for_unknown_projects() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(crate::config::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        })
        .unwrap();
        let ws = Workspace {
            state: crate::state::State::open(&config.paths.state_db()).unwrap(),
            config,
            claude_home: None,
        };
        assert!(ws.launch_project_setup("nope").is_err());
        let err = ws.check_project("nope").unwrap_err().to_string();
        assert!(err.contains("no project named"), "{err}");
    }
}
