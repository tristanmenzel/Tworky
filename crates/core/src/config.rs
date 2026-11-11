use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

/// Folder name used before the rename to Tworky.
const LEGACY_NAME: &str = "work-app";

impl Paths {
    /// `TWORKY_HOME` (or the pre-rename `WORK_APP_HOME`) puts config and data under one directory,
    /// for tests and side-by-side installs.
    pub fn resolve() -> Result<Self> {
        if let Some(home) = home_override() {
            return Ok(Self {
                config_dir: home.join("config"),
                data_dir: home.join("data"),
            });
        }
        let base = directories::BaseDirs::new().context("no home directory")?;
        Ok(Self::under(base.config_dir(), base.data_local_dir()))
    }

    /// Data sits in a `data` folder so it doesn't share the installer's folder with the exe.
    fn under(config_base: &Path, local_base: &Path) -> Self {
        Self {
            config_dir: config_base.join("Tworky"),
            data_dir: local_base.join("Tworky").join("data"),
        }
    }

    /// Copies config and state from the pre-rename work-app folders the first time Tworky runs.
    /// The old folders are left in place as a backup. Returns a line per thing copied.
    pub fn migrate_legacy(&self) -> Result<Vec<String>> {
        if home_override().is_some() {
            return Ok(vec![]);
        }
        let base = directories::BaseDirs::new().context("no home directory")?;
        self.migrate_from(
            &base.config_dir().join(LEGACY_NAME),
            &base.data_local_dir().join(LEGACY_NAME).join("state.db"),
        )
    }

    fn migrate_from(&self, old_config: &Path, old_state: &Path) -> Result<Vec<String>> {
        let mut copied = Vec::new();
        if !self.config_dir.exists() && old_config.is_dir() {
            copy_dir(old_config, &self.config_dir)?;
            copied.push(format!(
                "copied {} to {}",
                old_config.display(),
                self.config_dir.display()
            ));
        }
        let state = self.state_db();
        if !state.exists() && old_state.is_file() {
            std::fs::create_dir_all(&self.data_dir)?;
            std::fs::copy(old_state, &state)
                .with_context(|| format!("copying {}", old_state.display()))?;
            copied.push(format!(
                "copied {} to {}",
                old_state.display(),
                state.display()
            ));
        }
        Ok(copied)
    }

    /// Where debug builds of the app keep config and data, so in-progress code never rewrites
    /// what the installed app reads. Inside a worker each dev build gets its own home, so several
    /// can run side by side. `None` when `TWORKY_HOME` already picks a home.
    pub fn dev_home() -> Result<Option<PathBuf>> {
        if home_override().is_some() {
            return Ok(None);
        }
        let base = directories::BaseDirs::new().context("no home directory")?;
        let worker = crate::worker::worker_id_from_env();
        Ok(Some(dev_home_under(
            base.data_local_dir(),
            worker.as_deref(),
        )))
    }

    /// Seeds a new dev home with the installed app's `config.toml` and nothing else: no projects,
    /// templates, memory or state, so a dev build never shares port slots or compose projects with
    /// the installed app's workers. Its workers go under a `-dev` sibling of the installed workers
    /// root, in a folder per `worker` when the dev build runs inside one.
    pub fn seed_dev(&self, installed: &Paths, worker: Option<&str>) -> Result<Vec<String>> {
        if self.config_dir.exists() {
            return Ok(vec![]);
        }
        let text = match std::fs::read_to_string(installed.app_config()) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).context("reading the installed app config"),
        };
        let app: AppConfig = toml::from_str(&text).context("parsing the installed app config")?;
        let mut doc: toml_edit::DocumentMut = text.parse()?;
        let mut root = PathBuf::from(format!("{}-dev", app.workers_root.display()));
        if let Some(worker) = worker {
            root.push(worker);
        }
        let root = root.display().to_string();
        doc["workers_root"] = toml_edit::value(root.as_str());
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::write(self.app_config(), doc.to_string())?;
        Ok(vec![format!(
            "dev build starts with no projects; dev workers go in {root}"
        )])
    }

    pub fn app_config(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.config_dir.join("projects")
    }

    pub fn templates_dir(&self) -> PathBuf {
        self.config_dir.join("templates")
    }

    pub fn memory_file(&self, project: &str) -> PathBuf {
        self.config_dir.join("memory").join(format!("{project}.md"))
    }

    pub fn state_db(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }
}

/// A repo for a project made from the Repositories picker.
pub struct NewRepo {
    pub name_with_owner: String,
    pub source: PathBuf,
    pub base: String,
}

fn short_name(name_with_owner: &str) -> &str {
    name_with_owner
        .rsplit('/')
        .next()
        .unwrap_or(name_with_owner)
}

/// Lower-case letters, digits, `-` and `_`, so the name works as a file, folder and compose name.
pub fn project_slug(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Writes a minimal `projects/<name>.toml` (match, repos; no ports, stack or hooks) for repos no
/// project covers yet, so items, worktrees and memory work for them, and marks it as still to be
/// set up. Never overwrites a file. `name` defaults to the first repo's. Returns the project's name.
pub fn create_repo_project(
    paths: &Paths,
    taken: &[String],
    repos: &[NewRepo],
    name: Option<&str>,
) -> Result<String> {
    let first = repos.first().context("pick at least one repository")?;
    let base_name = project_slug(name.unwrap_or_else(|| short_name(&first.name_with_owner)));
    if base_name.trim_matches('-').is_empty() {
        bail!("a project needs a name");
    }
    let dir = paths.projects_dir();
    let name = std::iter::once(base_name.clone())
        .chain((2..).map(|n| format!("{base_name}-{n}")))
        .find(|n| !taken.contains(n) && !dir.join(format!("{n}.toml")).exists())
        .expect("an unused name exists");
    let short = |nwo: &str| short_name(nwo).to_string();

    let mut table = toml::Table::new();
    table.insert("name".into(), name.clone().into());
    table.insert("onboarding".into(), true.into());
    let mut matcher = toml::Table::new();
    matcher.insert(
        "repos".into(),
        repos
            .iter()
            .map(|r| toml::Value::from(r.name_with_owner.clone()))
            .collect::<Vec<_>>()
            .into(),
    );
    table.insert("match".into(), matcher.into());
    table.insert(
        "repos".into(),
        repos
            .iter()
            .map(|r| {
                let mut t = toml::Table::new();
                t.insert("name".into(), short(&r.name_with_owner).into());
                t.insert(
                    "source".into(),
                    r.source.to_string_lossy().into_owned().into(),
                );
                t.insert("base".into(), r.base.clone().into());
                toml::Value::from(t)
            })
            .collect::<Vec<_>>()
            .into(),
    );
    let text = format!(
        "# Created by Tworky. Its project setup session adds ports, [stack], hooks and services.\n{}",
        toml::to_string(&table)?
    );
    let parsed: ProjectConfig = toml::from_str(&text).context("generated project config")?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.toml"));
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(parsed.name)
}

/// Adds `repo` to the project in `file`: to `match.repos`, so its issues and PRs route here, and to
/// `[[repos]]`, so workers can add worktrees for it. Comments and layout survive, and the file is
/// written only if the result still parses.
pub fn add_repo_to_project(file: &Path, repo: &NewRepo) -> Result<()> {
    use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("{} does not parse", file.display()))?;
    let current: ProjectConfig =
        toml::from_str(&text).with_context(|| format!("{} is not a project", file.display()))?;

    if !doc.contains_key("match") {
        doc.insert("match", Item::Table(Table::new()));
    }
    let matcher = doc["match"]
        .as_table_like_mut()
        .with_context(|| format!("match in {} is not a table", file.display()))?;
    if matcher.get("repos").is_none() {
        matcher.insert("repos", value(Array::new()));
    }
    let listed = matcher
        .get_mut("repos")
        .and_then(|r| r.as_array_mut())
        .with_context(|| format!("match.repos in {} is not a list", file.display()))?;
    if !listed.iter().any(|r| {
        r.as_str()
            .is_some_and(|r| r.eq_ignore_ascii_case(&repo.name_with_owner))
    }) {
        listed.push(repo.name_with_owner.as_str());
    }

    let taken: Vec<&str> = current.repos.iter().map(|r| r.name.as_str()).collect();
    let short = short_name(&repo.name_with_owner);
    let name = if taken.iter().any(|t| t.eq_ignore_ascii_case(short)) {
        project_slug(&repo.name_with_owner.replace('/', "-"))
    } else {
        short.to_string()
    };
    let has_worktree_entry = current.repos.iter().any(|r| r.source == repo.source)
        || taken.iter().any(|t| t.eq_ignore_ascii_case(&name));
    if !has_worktree_entry {
        if !doc.contains_key("repos") {
            doc.insert("repos", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let mut t = Table::new();
        t.insert("name", value(name));
        t.insert("source", value(repo.source.to_string_lossy().into_owned()));
        t.insert("base", value(repo.base.as_str()));
        doc["repos"]
            .as_array_of_tables_mut()
            .with_context(|| format!("repos in {} is not a list of tables", file.display()))?
            .push(t);
    }

    let out = doc.to_string();
    toml::from_str::<ProjectConfig>(&out).context("the edited project config")?;
    let tmp = file.with_extension("toml.tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, file).with_context(|| format!("writing {}", file.display()))
}

fn dev_home_under(local_base: &Path, worker: Option<&str>) -> PathBuf {
    let home = local_base.join("Tworky-dev");
    match worker {
        Some(worker) => home.join("workers").join(worker),
        None => home,
    }
}

fn home_override() -> Option<PathBuf> {
    ["TWORKY_HOME", "WORK_APP_HOME"]
        .into_iter()
        .find_map(std::env::var_os)
        .map(PathBuf::from)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AppConfig {
    pub workers_root: PathBuf,
    pub port_stride: u16,
    pub max_slots: u16,
    pub max_active_workers: Option<usize>,
    pub shell: Vec<String>,
    pub claude: Vec<String>,
    /// Folders searched for existing clones when an item is made for a repo with no project
    /// config. Missing repos are cloned into the first one.
    pub source_roots: Vec<PathBuf>,
    /// `owner/repo` that `tw issue` files Tworky bugs and friction reports in.
    pub tworky_repo: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            tworky_repo: "tristanmenzel/Tworky".into(),
            source_roots: vec![default_dir("Source")],
            workers_root: default_dir("work"),
            port_stride: 100,
            max_slots: 20,
            max_active_workers: None,
            shell: default_shell(),
            claude: vec!["claude".into()],
        }
    }
}

/// `C:\<name>` on Windows, `~/<name>` elsewhere.
fn default_dir(name: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"C:\{name}"))
    } else {
        directories::BaseDirs::new()
            .map(|b| b.home_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir)
            .join(name)
    }
}

fn default_shell() -> Vec<String> {
    let shell: &[&str] = if cfg!(windows) {
        &["pwsh", "-NoProfile", "-NonInteractive", "-Command"]
    } else {
        &["bash", "-lc"]
    };
    shell.iter().map(|s| s.to_string()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    /// Issue types listed under their parent story in the overview instead of in the tree.
    #[serde(default, alias = "work_unit_types")]
    pub subtask_types: Vec<String>,
    /// Issue types a card never rolls up into, even when assigned to me (so an epic I own doesn't
    /// swallow my stories).
    #[serde(default = "default_no_rollup_into")]
    pub no_rollup_into: Vec<String>,
    #[serde(default)]
    pub context_types: Vec<String>,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default = "default_session_cwd")]
    pub session_cwd: String,
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
    pub stack: Option<StackConfig>,
    #[serde(default)]
    pub hooks: Hooks,
    pub template_dir: Option<PathBuf>,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub notes: String,
    /// Number of the Projects board whose Status is shown and edited. Unset shows every board.
    #[serde(default)]
    pub status_board: Option<u64>,
    /// Made by Tworky and not yet set up by its project setup session, which removes this.
    #[serde(default)]
    pub onboarding: bool,
    #[serde(skip)]
    pub file: PathBuf,
}

fn default_no_rollup_into() -> Vec<String> {
    vec!["Epic".into()]
}

fn default_branch() -> String {
    "feat/{{ issue.number }}-{{ issue.slug }}".into()
}

fn default_session_cwd() -> String {
    ".".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchRule {
    pub owner: Option<String>,
    #[serde(default)]
    pub repos: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub name: String,
    pub source: PathBuf,
    #[serde(default = "default_base")]
    pub base: String,
}

fn default_base() -> String {
    "origin/main".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackConfig {
    pub compose_dir: String,
    #[serde(default)]
    pub compose_files: Vec<String>,
    pub env_file: Option<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default)]
    pub setup: Vec<Hook>,
    #[serde(default)]
    pub teardown: Vec<Hook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    #[serde(default = "default_session_cwd")]
    pub cwd: String,
    pub run: HookCommand,
}

/// A long-running process a worker needs, such as a dev server, which Tworky starts on demand:
/// opening a browser tab for an account whose start URL is on `port` starts it first.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    #[serde(default = "default_session_cwd")]
    pub cwd: String,
    /// Rendered like a hook, so `{{ ports.web }}` is the worker's port.
    pub run: HookCommand,
    /// A `[ports]` name. The service is up once the worker's port accepts connections.
    pub port: Option<String>,
    #[serde(default)]
    pub https: bool,
    /// Services started before this one.
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default = "default_ready_timeout")]
    pub ready_timeout_secs: u64,
}

fn default_ready_timeout() -> u64 {
    120
}

/// One command for every OS, or a table of per-OS commands. `macos` and `linux` beat `unix`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookCommand {
    Everywhere(String),
    PerOs(PerOsCommand),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerOsCommand {
    pub windows: Option<String>,
    pub unix: Option<String>,
    pub macos: Option<String>,
    pub linux: Option<String>,
}

impl HookCommand {
    pub fn for_current_os(&self) -> Option<&str> {
        self.for_os(std::env::consts::OS)
    }

    pub fn for_os(&self, os: &str) -> Option<&str> {
        let c = match self {
            Self::Everywhere(run) => return Some(run),
            Self::PerOs(c) => c,
        };
        match os {
            "windows" => c.windows.as_deref(),
            "macos" => c.macos.as_deref().or(c.unix.as_deref()),
            "linux" => c.linux.as_deref().or(c.unix.as_deref()),
            _ => c.unix.as_deref(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Everywhere(run) => run.trim().is_empty(),
            Self::PerOs(c) => [&c.windows, &c.unix, &c.macos, &c.linux]
                .iter()
                .all(|r| r.as_deref().is_none_or(|r| r.trim().is_empty())),
        }
    }
}

/// A test account (persona) a session's browser can sign in as. Its password is in the OS
/// credential store (`accounts::set_password`), never in the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    #[serde(default)]
    pub username: String,
    /// Written with the base ports; `localhost:<base port>` and `{{ ports.* }}` become the worker's.
    /// Empty when `start_service` names the start page instead.
    #[serde(default)]
    pub start_url: String,
    /// A service whose URL on the worker is the start page, in place of `start_url`.
    #[serde(default)]
    pub start_service: Option<String>,
    #[serde(default)]
    pub mobile: bool,
    /// Identity provider origins where the password may be filled, besides `start_url`'s own.
    /// `service:<name>` is that service's origin on the worker.
    #[serde(default)]
    pub login_origins: Vec<String>,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub paths: Paths,
    pub app: AppConfig,
    pub projects: Vec<ProjectConfig>,
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(Paths::resolve()?)
    }

    pub fn load_from(paths: Paths) -> Result<Self> {
        let app = match std::fs::read_to_string(paths.app_config()) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parsing {}", paths.app_config().display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AppConfig::default(),
            Err(e) => return Err(e).context("reading app config"),
        };
        let mut projects = Vec::new();
        if let Ok(entries) = std::fs::read_dir(paths.projects_dir()) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect();
            files.sort();
            for file in files {
                projects.push(load_project(&file)?);
            }
        }
        let config = Self {
            paths,
            app,
            projects,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.app.port_stride == 0 || self.app.max_slots == 0 {
            bail!("port_stride and max_slots must be greater than zero");
        }
        if self.app.shell.is_empty() || self.app.claude.is_empty() {
            bail!("shell and claude commands must not be empty");
        }
        let mut names = std::collections::HashSet::new();
        for p in &self.projects {
            if !names.insert(p.name.as_str()) {
                bail!("duplicate project name '{}'", p.name);
            }
            p.validate(&self.app)?;
        }
        Ok(())
    }

    pub fn project(&self, name: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|p| p.name == name)
    }

    /// Explicit repo lists win over owner matches so a repo can be carved out of an org-wide project.
    pub fn project_for_repo(&self, name_with_owner: &str) -> Option<&ProjectConfig> {
        let lower = name_with_owner.to_ascii_lowercase();
        self.projects
            .iter()
            .find(|p| {
                p.match_rule
                    .repos
                    .iter()
                    .any(|r| r.to_ascii_lowercase() == lower)
            })
            .or_else(|| {
                let owner = lower.split('/').next()?;
                self.projects.iter().find(|p| {
                    p.match_rule
                        .owner
                        .as_deref()
                        .is_some_and(|o| o.eq_ignore_ascii_case(owner))
                })
            })
    }

    pub fn is_subtask(&self, issue: &crate::hierarchy::Issue) -> bool {
        is_subtask_type(
            self.project_for_repo(&issue.repo),
            issue.issue_type.as_deref(),
        )
    }

    pub fn template_dir(&self, project: &ProjectConfig) -> PathBuf {
        project
            .template_dir
            .clone()
            .unwrap_or_else(|| self.paths.templates_dir().join(&project.name))
    }
}

fn load_project(path: &Path) -> Result<ProjectConfig> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut project: ProjectConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    project.file = path.to_path_buf();
    Ok(project)
}

impl ProjectConfig {
    fn validate(&self, app: &AppConfig) -> Result<()> {
        let ctx = || format!("project '{}'", self.name);
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!(
                "{}: name may only contain letters, digits, '-' and '_'",
                ctx()
            );
        }
        if self.match_rule.owner.is_none() && self.match_rule.repos.is_empty() {
            bail!("{}: match needs an owner or repos", ctx());
        }
        if self.repos.is_empty() {
            bail!("{}: at least one repo is required", ctx());
        }
        let mut repo_names = std::collections::HashSet::new();
        for r in &self.repos {
            if !repo_names.insert(r.name.as_str()) {
                bail!("{}: duplicate repo '{}'", ctx(), r.name);
            }
        }
        let max = u32::from(app.max_slots) * u32::from(app.port_stride);
        for (name, port) in &self.ports {
            if u32::from(*port) + max > u32::from(u16::MAX) {
                bail!(
                    "{}: port '{name}' ({port}) overflows with {} slots",
                    ctx(),
                    app.max_slots
                );
            }
        }
        crate::template::check(&self.branch)
            .with_context(|| format!("{}: branch template", ctx()))?;
        for hook in self.hooks.setup.iter().chain(&self.hooks.teardown) {
            if hook.run.is_empty() {
                bail!("{}: hook in '{}' has no command", ctx(), hook.cwd);
            }
        }
        self.validate_services()
            .with_context(|| format!("{}: services", ctx()))
    }

    fn validate_services(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for s in &self.services {
            if !names.insert(s.name.as_str()) {
                bail!("duplicate service '{}'", s.name);
            }
            if s.run.is_empty() {
                bail!("service '{}' has no command", s.name);
            }
            if let Some(port) = &s.port
                && !self.ports.contains_key(port)
            {
                bail!(
                    "service '{}' uses port '{port}', which isn't in [ports]",
                    s.name
                );
            }
        }
        for s in &self.services {
            if let Some(missing) = s.needs.iter().find(|n| !names.contains(n.as_str())) {
                bail!(
                    "service '{}' needs '{missing}', which isn't a service",
                    s.name
                );
            }
        }
        for s in &self.services {
            self.service_order(&s.name)?;
        }
        Ok(())
    }

    pub fn service(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|s| s.name == name)
    }

    /// `name` and the services it needs, each after the ones it needs.
    pub fn service_order(&self, name: &str) -> Result<Vec<&ServiceConfig>> {
        fn visit<'a>(
            p: &'a ProjectConfig,
            name: &str,
            path: &mut Vec<String>,
            out: &mut Vec<&'a ServiceConfig>,
        ) -> Result<()> {
            if out.iter().any(|s| s.name == name) {
                return Ok(());
            }
            if path.iter().any(|n| n == name) {
                bail!("services need each other: {} → {name}", path.join(" → "));
            }
            let service = p
                .service(name)
                .with_context(|| format!("no service called '{name}'"))?;
            path.push(name.to_string());
            for need in &service.needs {
                visit(p, need, path, out)?;
            }
            path.pop();
            out.push(service);
            Ok(())
        }
        let mut out = Vec::new();
        visit(self, name, &mut Vec::new(), &mut out)?;
        Ok(out)
    }
}

const DEFAULT_SUBTASK_TYPES: &[&str] = &["Subtask", "Sub-task", "Task"];

/// Uses the project's `subtask_types`, or common names when there is no project or none are set.
pub fn is_subtask_type(project: Option<&ProjectConfig>, issue_type: Option<&str>) -> bool {
    let Some(t) = issue_type else {
        return false;
    };
    match project.filter(|p| !p.subtask_types.is_empty()) {
        Some(p) => p.subtask_types.iter().any(|s| s.eq_ignore_ascii_case(t)),
        None => DEFAULT_SUBTASK_TYPES
            .iter()
            .any(|s| s.eq_ignore_ascii_case(t)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn paths(dir: &Path) -> Paths {
        Paths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
        }
    }

    #[test]
    fn creates_minimal_repo_projects_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let repo = |nwo: &str| NewRepo {
            name_with_owner: nwo.into(),
            source: dir.path().join("src").join(nwo),
            base: "origin/develop".into(),
        };
        let name = create_repo_project(
            &p,
            &[],
            &[repo("acme/Admin.Portal"), repo("acme/shared")],
            None,
        )
        .unwrap();
        assert_eq!(name, "admin-portal");
        let again = create_repo_project(&p, &[], &[repo("acme/Admin.Portal")], None).unwrap();
        assert_eq!(again, "admin-portal-2");
        assert_eq!(
            create_repo_project(&p, &["x".into()], &[repo("o/x")], None).unwrap(),
            "x-2"
        );
        assert_eq!(
            create_repo_project(&p, &[], &[repo("o/y")], Some("Staff Portal")).unwrap(),
            "staff-portal"
        );
        assert!(create_repo_project(&p, &[], &[repo("o/z")], Some(" / ")).is_err());

        let config = Config::load_from(p).unwrap();
        let project = config.project_for_repo("acme/shared").unwrap();
        assert_eq!(project.name, "admin-portal");
        assert!(project.onboarding);
        assert_eq!(project.repos.len(), 2);
        assert_eq!(project.repos[1].name, "shared");
        assert_eq!(project.repos[0].base, "origin/develop");
        assert!(project.ports.is_empty() && project.stack.is_none());
        assert!(create_repo_project(&paths(dir.path()), &[], &[], None).is_err());
    }

    #[test]
    fn adds_a_repo_to_a_project_keeping_comments() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acme.toml");
        let original = "# Acme\nname = \"acme\" # the name\n\n[match]\nowner = \"acme\"\n\n\
                        [[repos]]\nname = \"api\" # main api\nsource = \"C:/src/api\"\n";
        std::fs::write(&file, original).unwrap();
        let repo = |nwo: &str| NewRepo {
            name_with_owner: nwo.into(),
            source: PathBuf::from(format!("C:/src/{nwo}")),
            base: "origin/main".into(),
        };
        add_repo_to_project(&file, &repo("other/web")).unwrap();
        add_repo_to_project(&file, &repo("other/api")).unwrap();
        add_repo_to_project(&file, &repo("other/web")).unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("# Acme") && text.contains("# main api"),
            "{text}"
        );
        let p: ProjectConfig = toml::from_str(&text).unwrap();
        assert_eq!(p.match_rule.repos, ["other/web", "other/api"]);
        let names: Vec<_> = p.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["api", "web", "other-api"]);

        std::fs::write(&file, "name = [").unwrap();
        assert!(add_repo_to_project(&file, &repo("o/x")).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "name = [");
    }

    #[test]
    fn migrates_pre_rename_folders_once_without_touching_the_originals() {
        let dir = tempfile::tempdir().unwrap();
        let roaming = dir.path().join("Roaming");
        let local = dir.path().join("Local");
        write(&roaming, "work-app/projects/acme.toml", "name = \"acme\"");
        write(&roaming, "work-app/memory/acme.md", "# Acme");
        write(&local, "work-app/state.db", "db");
        let p = Paths::under(&roaming, &local);
        assert_eq!(p.data_dir, local.join("Tworky").join("data"));

        let copied = p
            .migrate_from(&roaming.join("work-app"), &local.join("work-app/state.db"))
            .unwrap();
        assert_eq!(copied.len(), 2, "{copied:?}");
        assert_eq!(
            std::fs::read_to_string(p.memory_file("acme")).unwrap(),
            "# Acme"
        );
        assert!(p.projects_dir().join("acme.toml").exists());
        assert_eq!(std::fs::read_to_string(p.state_db()).unwrap(), "db");
        assert!(
            roaming.join("work-app/memory/acme.md").exists(),
            "kept as backup"
        );

        std::fs::write(p.memory_file("acme"), "# edited").unwrap();
        let again = p
            .migrate_from(&roaming.join("work-app"), &local.join("work-app/state.db"))
            .unwrap();
        assert!(again.is_empty());
        assert_eq!(
            std::fs::read_to_string(p.memory_file("acme")).unwrap(),
            "# edited"
        );
    }

    #[test]
    fn seeds_a_dev_home_with_app_config_only() {
        let dir = tempfile::tempdir().unwrap();
        let installed = paths(&dir.path().join("installed"));
        write(&installed.config_dir, "projects/acme.toml", ACME);
        write(
            &installed.config_dir,
            "config.toml",
            "# mine\nworkers_root = 'D:\\w'\nport_stride = 50\n",
        );
        write(&installed.data_dir, "state.db", "db");
        let dev = paths(&dir.path().join("dev"));

        let seeded = dev.seed_dev(&installed, None).unwrap();
        assert_eq!(seeded.len(), 1, "{seeded:?}");
        let cfg = Config::load_from(dev.clone()).unwrap();
        assert_eq!(cfg.app.workers_root, PathBuf::from("D:\\w-dev"));
        assert_eq!(cfg.app.port_stride, 50);
        assert!(cfg.project("acme").is_none());
        assert!(!dev.projects_dir().exists());
        assert!(!dev.state_db().exists());

        std::fs::write(dev.app_config(), "port_stride = 7\n").unwrap();
        assert!(dev.seed_dev(&installed, None).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(dev.app_config()).unwrap(),
            "port_stride = 7\n"
        );
    }

    #[test]
    fn a_worker_gets_its_own_dev_home_and_workers_root() {
        let base = Path::new("L");
        assert_eq!(dev_home_under(base, None), base.join("Tworky-dev"));
        assert_eq!(
            dev_home_under(base, Some("tworky-12-fix")),
            base.join("Tworky-dev")
                .join("workers")
                .join("tworky-12-fix")
        );

        let dir = tempfile::tempdir().unwrap();
        let installed = paths(&dir.path().join("installed"));
        write(
            &installed.config_dir,
            "config.toml",
            "workers_root = 'D:\\w'\n",
        );
        let dev = paths(&dir.path().join("dev"));
        dev.seed_dev(&installed, Some("tworky-12-fix")).unwrap();
        let cfg = Config::load_from(dev).unwrap();
        assert_eq!(
            cfg.app.workers_root,
            PathBuf::from("D:\\w-dev").join("tworky-12-fix")
        );
    }

    const ACME: &str = r#"
name = "acme"
match = { owner = "acme" }
subtask_types = ["Subtask"]
[[repos]]
name = "platform"
source = 'C:\src\platform'
[ports]
api = 4000
"#;

    #[test]
    fn loads_defaults_and_projects() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "config/projects/acme.toml", ACME);
        let cfg = Config::load_from(paths(dir.path())).unwrap();
        assert_eq!(cfg.app.port_stride, 100);
        let p = cfg.project("acme").unwrap();
        assert_eq!(p.repos[0].base, "origin/main");
        assert!(is_subtask_type(Some(p), Some("subtask")));
        assert!(!is_subtask_type(Some(p), Some("Story")));
        assert!(is_subtask_type(None, Some("Task")));
        assert!(!is_subtask_type(None, None));
    }

    #[test]
    fn orders_services_after_what_they_need_and_rejects_bad_ones() {
        let services = r#"
[[services]]
name = "web"
run = "vite"
port = "api"
needs = ["api", "worker"]
[[services]]
name = "api"
run = "npm run dev"
needs = ["worker"]
[[services]]
name = "worker"
run = "npm run worker"
"#;
        let load = |extra: &str| {
            let dir = tempfile::tempdir().unwrap();
            write(
                dir.path(),
                "config/projects/acme.toml",
                &format!("{ACME}{extra}"),
            );
            Config::load_from(paths(dir.path())).map(|c| c.project("acme").unwrap().clone())
        };
        let p = load(services).unwrap();
        let order: Vec<_> = p
            .service_order("web")
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(order, ["worker", "api", "web"]);
        assert_eq!(p.service("api").unwrap().ready_timeout_secs, 120);

        let cycle = services.replace(
            "run = \"npm run worker\"",
            "run = \"npm run worker\"\nneeds = [\"web\"]",
        );
        assert!(format!("{:#}", load(&cycle).unwrap_err()).contains("need each other"));
        let unknown_port = services.replace("port = \"api\"", "port = \"web\"");
        assert!(format!("{:#}", load(&unknown_port).unwrap_err()).contains("isn't in [ports]"));
        let unknown_need = services.replace("needs = [\"worker\"]", "needs = [\"db\"]");
        assert!(format!("{:#}", load(&unknown_need).unwrap_err()).contains("isn't a service"));
    }

    #[test]
    fn routes_by_repo_before_owner() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "config/projects/acme.toml", ACME);
        write(
            dir.path(),
            "config/projects/ops.toml",
            "name = \"ops\"\nmatch = { repos = [\"acme/operations\"] }\n[[repos]]\nname = \"ops\"\nsource = 'C:\\ops'\n",
        );
        let cfg = Config::load_from(paths(dir.path())).unwrap();
        assert_eq!(cfg.project_for_repo("acme/acme-web").unwrap().name, "acme");
        assert_eq!(cfg.project_for_repo("Acme/operations").unwrap().name, "ops");
        assert!(cfg.project_for_repo("other/repo").is_none());
    }

    #[test]
    fn example_projects_load() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config/projects/acme.toml",
            include_str!("../../../examples/acme/acme.toml"),
        );
        let cfg = Config::load_from(paths(dir.path())).unwrap();
        let acme = cfg.project("acme").unwrap();
        assert_eq!(acme.repos.len(), 2);
        assert_eq!(acme.stack.as_ref().unwrap().compose_dir, "acme-api");
        assert_eq!(acme.accounts[0].start_service.as_deref(), Some("web"));
        let service = |name: &str| acme.services.iter().find(|s| s.name == name).unwrap();
        let api = service("api");
        assert_eq!(api.port.as_deref(), Some("api"));
        assert!(api.https);
        assert!(api.needs.is_empty());
        let web = service("web");
        assert_eq!(web.port.as_deref(), Some("web"));
        assert!(!web.https);
        assert_eq!(web.needs, ["api"]);
        for os in ["windows", "macos", "linux"] {
            assert!(
                acme.hooks.setup.iter().all(|h| h.run.for_os(os).is_some()),
                "{os}"
            );
            assert!(
                acme.services.iter().all(|s| s.run.for_os(os).is_some()),
                "{os}"
            );
        }
    }

    #[test]
    fn tworky_example_loads() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config/projects/tworky.toml",
            include_str!("../../../examples/tworky/tworky.toml"),
        );
        let cfg = Config::load_from(paths(dir.path())).unwrap();
        let p = cfg.project("tworky").unwrap();
        assert_eq!(p.ports["vite"], 1420);
        assert_eq!(p.services[0].port.as_deref(), Some("vite"));
        for os in ["windows", "macos", "linux"] {
            assert!(
                p.hooks.setup.iter().all(|h| h.run.for_os(os).is_some()),
                "{os}"
            );
            assert!(p.services[0].run.for_os(os).is_some(), "{os}");
        }
    }

    #[test]
    fn rejects_overflowing_ports_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config/projects/acme.toml",
            &ACME.replace("4000", "65000"),
        );
        assert!(Config::load_from(paths(dir.path())).is_err());
        write(
            dir.path(),
            "config/projects/acme.toml",
            &format!("bogus = 1\n{ACME}"),
        );
        assert!(Config::load_from(paths(dir.path())).is_err());
    }

    #[test]
    fn hooks_take_one_command_or_one_per_os() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = r#"
[[hooks.setup]]
run = "npm ci"
[[hooks.setup]]
cwd = "api"
run = { windows = "npm ci; if ($LASTEXITCODE) { exit $LASTEXITCODE }", unix = "npm ci", macos = "brew bundle && npm ci" }
[[hooks.teardown]]
run = { windows = "echo bye" }
"#;
        write(
            dir.path(),
            "config/projects/acme.toml",
            &format!("{ACME}{hooks}"),
        );
        let cfg = Config::load_from(paths(dir.path())).unwrap();
        let h = &cfg.project("acme").unwrap().hooks;
        assert_eq!(h.setup[0].run.for_os("linux"), Some("npm ci"));
        assert_eq!(h.setup[0].run.for_os("windows"), Some("npm ci"));
        assert_eq!(h.setup[1].run.for_os("linux"), Some("npm ci"));
        assert_eq!(
            h.setup[1].run.for_os("macos"),
            Some("brew bundle && npm ci")
        );
        assert!(
            h.setup[1]
                .run
                .for_os("windows")
                .unwrap()
                .contains("LASTEXITCODE")
        );
        assert_eq!(h.teardown[0].run.for_os("macos"), None);

        for bad in ["run = {}", "run = { solaris = \"x\" }", "run = \"  \""] {
            write(
                dir.path(),
                "config/projects/acme.toml",
                &format!("{ACME}[[hooks.setup]]\n{bad}\n"),
            );
            assert!(Config::load_from(paths(dir.path())).is_err(), "{bad}");
        }
    }
}
