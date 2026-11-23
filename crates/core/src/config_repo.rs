use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::{MachineConfig, Os, Paths, ProjectConfig};
use crate::process;

pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

const FALLBACK_NAME: &str = "Tworky";
const FALLBACK_EMAIL: &str = "tworky@localhost";

const GITIGNORE: &str = "\
*
!/.gitignore
!/.gitattributes
!/tworky/
!/tworky/claude.local*.md
!/projects/
!/projects/*/
!/projects/*/project.toml
!/projects/*/memory*.md
";

const GITATTRIBUTES: &str = "* text eol=lf\n";

/// Held while reading, writing or committing files in `repo/`. Not re-entrant: taking it again in
/// the same process while holding it deadlocks.
pub struct RepoLock {
    _file: File,
}

/// Held across a whole sync, resolve or abort. Take it before [`RepoLock`], never after.
pub struct SyncLock {
    _file: File,
}

pub fn with_lock(paths: &Paths) -> Result<RepoLock> {
    Ok(RepoLock {
        _file: lock(&paths.repo_lock())?,
    })
}

pub fn with_sync_lock(paths: &Paths) -> Result<SyncLock> {
    Ok(SyncLock {
        _file: lock(&paths.repo_sync_lock())?,
    })
}

fn lock(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// Runs git in `dir` with signing, hooks, line-ending conversion and every prompt turned off, so
/// the user's own git config can't make it fail or hang.
pub fn git(paths: &Paths, dir: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let hooks = format!(
        "core.hooksPath={}",
        forward_slashes(&paths.repo_hooks_dir())
    );
    let mut cmd = process::command("git");
    cmd.args([
        "-c",
        "commit.gpgsign=false",
        "-c",
        &hooks,
        "-c",
        "core.autocrlf=false",
    ])
    .args(args)
    .current_dir(dir)
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GCM_INTERACTIVE", "never")
    .env("GIT_EDITOR", "true");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_COMMON_DIR",
    ] {
        cmd.env_remove(var);
    }
    #[cfg(test)]
    tests::isolate(&mut cmd, args)?;
    process::output_timeout(cmd, &process::describe("git", args), timeout)
}

fn forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Creates `repo/` with its first commit. It's built in `repo.tmp/` and renamed into place, so a
/// failure leaves no `repo/`.
pub fn init(_guard: &RepoLock, paths: &Paths) -> Result<()> {
    create(paths, "Start the Tworky config", |_| Ok(vec![]))
}

/// Builds `repo/` in `repo.tmp/`: its git config, allowlist and guidance files plus whatever
/// `fill` writes, in one commit, then renames it into place. `fill` returns paths to leave
/// uncommitted.
fn create(
    paths: &Paths,
    message: &str,
    fill: impl FnOnce(&Path) -> Result<Vec<String>>,
) -> Result<()> {
    let repo = paths.repo_dir();
    if repo.exists() {
        bail!("{} already exists", repo.display());
    }
    let tmp = paths.repo_tmp_dir();
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .with_context(|| format!("removing the leftover {}", tmp.display()))?;
    }
    let built = scaffold(paths, &tmp)
        .and_then(|()| fill(&tmp))
        .and_then(|unstaged| commit_everything(paths, &tmp, &unstaged, message))
        .and_then(|()| rename_retrying(&tmp, &repo));
    if built.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    built.with_context(|| format!("creating the config repo {}", repo.display()))
}

fn scaffold(paths: &Paths, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::create_dir_all(paths.repo_hooks_dir())?;
    let git = |args: &[&str]| git(paths, dir, args, LOCAL_TIMEOUT);
    git(&["init", "-q", "-b", "main"])?;
    let hooks = forward_slashes(&paths.repo_hooks_dir());
    for (key, value) in [
        ("commit.gpgsign", "false"),
        ("core.hooksPath", hooks.as_str()),
        ("core.autocrlf", "false"),
        ("rerere.enabled", "true"),
    ] {
        git(&["config", key, value])?;
    }
    let configured = |key: &str| git(&["config", key]).is_ok_and(|v| !v.trim().is_empty());
    if !(configured("user.name") && configured("user.email")) {
        git(&["config", "user.name", FALLBACK_NAME])?;
        git(&["config", "user.email", FALLBACK_EMAIL])?;
    }

    std::fs::write(dir.join(".gitignore"), GITIGNORE)?;
    std::fs::write(dir.join(".gitattributes"), GITATTRIBUTES)?;
    let repo = paths.repo_dir();
    let guidance = std::iter::once(paths.guidance_files(Os::current())[0].clone())
        .chain(Os::ALL.map(|os| paths.guidance_files(os)[1].clone()));
    for file in guidance {
        let file = dir.join(file.strip_prefix(&repo)?);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file, "")?;
    }
    Ok(())
}

fn commit_everything(paths: &Paths, dir: &Path, unstaged: &[String], message: &str) -> Result<()> {
    let git = |args: &[&str]| git(paths, dir, args, LOCAL_TIMEOUT);
    git(&["add", "-A"])?;
    if !unstaged.is_empty() {
        let specs: Vec<String> = unstaged.iter().map(|r| pathspec(r)).collect();
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        git(&[
            &["rm", "--cached", "-q", "--ignore-unmatch", "--"][..],
            &specs,
        ]
        .concat())?;
    }
    git(&["commit", "-q", "-m", message])?;
    Ok(())
}

/// Brings the config repo into being, then renames the legacy folders. See
/// [`Paths::migrate_to_repo`].
pub fn migrate(paths: &Paths) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if !paths.repo_dir().exists() {
        let guard = with_lock(paths)?;
        if !paths.repo_dir().exists() {
            if has_legacy_config(paths) {
                done.extend(import(&guard, paths)?);
            } else {
                init(&guard, paths)?;
                done.push(format!(
                    "created the config repo {}",
                    paths.repo_dir().display()
                ));
            }
        }
    }
    done.extend(rename_legacy(paths)?);
    Ok(done)
}

fn has_legacy_config(paths: &Paths) -> bool {
    !files_with_extension(&paths.legacy_projects_dir(), "toml").is_empty()
        || !files_with_extension(&paths.legacy_memory_dir(), "md").is_empty()
}

/// Files directly in `dir` with `extension`, sorted.
fn files_with_extension(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == extension))
        .collect();
    files.sort();
    files
}

/// Builds `repo/` from the legacy `projects/*.toml` and `memory/*.md` in one commit, moving each
/// clone path and template folder to `machine.toml`.
fn import(guard: &RepoLock, paths: &Paths) -> Result<Vec<String>> {
    let mut report = Vec::new();
    create(paths, "Import existing config", |dir| {
        let mut machine = MachineConfig::read(paths)?;
        let mut folders: Vec<String> = Vec::new();
        let mut unstaged = Vec::new();
        for file in files_with_extension(&paths.legacy_projects_dir(), "toml") {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| is_project_name(s));
            let (folder, out) = match (import_project(&text, &mut machine), stem) {
                (Ok((name, _)), _) if folders.contains(&name) => {
                    report.push(format!(
                        "left {} in the backup: project {name} was already imported",
                        file.display()
                    ));
                    continue;
                }
                (Ok((name, out)), _) => {
                    report.push(format!("imported {} as project {name}", file.display()));
                    (name, out)
                }
                (Err(e), Some(stem)) if !folders.iter().any(|f| f == stem) => {
                    report.push(format!(
                        "copied {} as it is to projects/{stem}, uncommitted: {e:#}",
                        file.display()
                    ));
                    unstaged.push(format!("projects/{stem}/project.toml"));
                    (stem.to_string(), text)
                }
                (Err(e), _) => {
                    report.push(format!("left {} in the backup: {e:#}", file.display()));
                    continue;
                }
            };
            let target = dir.join("projects").join(&folder);
            std::fs::create_dir_all(&target)?;
            std::fs::write(target.join("project.toml"), out)?;
            folders.push(folder);
        }
        for file in files_with_extension(&paths.legacy_memory_dir(), "md") {
            let name = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if folders.iter().any(|f| f == name) {
                std::fs::copy(&file, dir.join("projects").join(name).join("memory.md"))
                    .with_context(|| format!("copying {}", file.display()))?;
            } else {
                report.push(format!(
                    "left {} in the backup: no project is called {name}",
                    file.display()
                ));
            }
        }
        write_machine(guard, paths, &toml::to_string(&machine)?)?;
        Ok(unstaged)
    })?;
    report.push(format!(
        "imported the config into {}",
        paths.repo_dir().display()
    ));
    Ok(report)
}

/// The project's name and its file for the repo: each repo's `source` and the `template_dir` move
/// to `machine` when they exist on this machine, and each repo gets a `repo` from its clone's
/// origin, else from the one `match.repos` entry with its name.
fn import_project(text: &str, machine: &mut MachineConfig) -> Result<(String, String)> {
    use toml_edit::{DocumentMut, TableLike, value};
    let parsed: ProjectConfig = toml::from_str(text)?;
    let name = parsed.name.clone();
    if !is_project_name(&name) {
        bail!("project name '{name}' may only contain letters, digits, '-' and '_'");
    }
    let mut doc: DocumentMut = text.parse()?;
    if let Some(dir) = doc.remove("template_dir")
        && let Some(dir) = dir.as_str().map(PathBuf::from)
        && dir.exists()
    {
        machine.set_template_dir(&name, dir);
    }
    let mut repos: Vec<&mut dyn TableLike> = Vec::new();
    match doc.get_mut("repos") {
        Some(toml_edit::Item::ArrayOfTables(tables)) => {
            repos.extend(tables.iter_mut().map(|t| t as &mut dyn TableLike))
        }
        Some(toml_edit::Item::Value(toml_edit::Value::Array(array))) => repos.extend(
            array
                .iter_mut()
                .filter_map(|v| v.as_inline_table_mut())
                .map(|t| t as &mut dyn TableLike),
        ),
        _ => {}
    }
    for table in repos {
        let Some(repo) = table.get("name").and_then(|n| n.as_str()).map(String::from) else {
            continue;
        };
        let source = table
            .remove("source")
            .and_then(|s| s.as_str().map(PathBuf::from));
        if let Some(source) = source.as_ref().filter(|s| s.exists()) {
            machine.set_source(&name, &repo, source.clone());
        }
        if table.get("repo").is_none()
            && let Some(nwo) = guess_repo(source.as_deref(), &repo, &parsed.match_rule.repos)
        {
            table.insert("repo", value(nwo));
        }
    }
    let out = doc.to_string();
    toml::from_str::<ProjectConfig>(&out).context("the imported project config")?;
    Ok((name, out))
}

fn guess_repo(source: Option<&Path>, repo: &str, matched: &[String]) -> Option<String> {
    if let Some(nwo) = source
        .filter(|s| s.exists())
        .and_then(|s| crate::git::origin_name_with_owner(s).ok())
    {
        return Some(nwo);
    }
    let named: Vec<&String> = matched
        .iter()
        .filter(|m| crate::config::short_name(m).eq_ignore_ascii_case(repo))
        .collect();
    match named.as_slice() {
        [only] => Some(only.to_string()),
        _ => None,
    }
}

fn is_project_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Renames the legacy `projects/` and `memory/` to `*.pre-repo` once the repo exists, so older
/// builds and this one never read both.
fn rename_legacy(paths: &Paths) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for dir in [paths.legacy_projects_dir(), paths.legacy_memory_dir()] {
        if !dir.exists() {
            continue;
        }
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let backup = std::iter::once(format!("{name}.pre-repo"))
            .chain((2..).map(|n| format!("{name}.pre-repo-{n}")))
            .map(|b| dir.with_file_name(b))
            .find(|b| !b.exists())
            .expect("an unused name exists");
        match rename_retrying(&dir, &backup) {
            Ok(()) => done.push(format!("moved {} to {}", dir.display(), backup.display())),
            Err(_) if !dir.exists() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// On Windows a scanner or indexer can briefly hold a file in the new repo open, which makes the
/// rename fail with a sharing or access error.
fn rename_retrying(from: &Path, to: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e)
                if cfg!(windows)
                    && matches!(e.raw_os_error(), Some(5 | 32))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("renaming {} to {}", from.display(), to.display()));
            }
        }
    }
}

/// Stages `files` (paths in `repo/`, absolute or relative to it) and commits them, and nothing
/// else, if any changed. A path outside the allowlist, or a `project.toml` that doesn't parse, is
/// refused. A missing file is committed as deleted, or skipped if it was never tracked. Returns
/// whether it committed.
pub fn commit(
    _guard: &RepoLock,
    paths: &Paths,
    files: &[impl AsRef<Path>],
    message: &str,
) -> Result<bool> {
    let repo = paths.repo_dir();
    let mut relatives = Vec::new();
    for file in files {
        let file = file.as_ref();
        let relative = if file.is_absolute() {
            file.strip_prefix(&repo)
                .with_context(|| format!("{} is outside {}", file.display(), repo.display()))?
        } else {
            file
        };
        let Some(relative) = repo_relative(relative).filter(|r| is_allowlisted(r)) else {
            bail!("{} isn't a file the config repo tracks", file.display());
        };
        let absolute = repo.join(&relative);
        if is_project_file(Path::new(&relative)) && absolute.exists() {
            check_project(&absolute)?;
        }
        relatives.push(relative);
    }
    commit_paths(paths, &repo, &relatives, message)
}

/// [`commit`], but a failure is only logged: the file is already written, and the next sweep
/// commits it.
pub fn commit_logged(guard: &RepoLock, paths: &Paths, files: &[impl AsRef<Path>], message: &str) {
    if let Err(e) = commit(guard, paths, files, message) {
        eprintln!("config repo: couldn't commit \"{message}\": {e:#}");
    }
}

/// Commits `relatives` (allowlisted, `/`-separated) as they are in the working tree.
fn commit_paths(paths: &Paths, repo: &Path, relatives: &[String], message: &str) -> Result<bool> {
    if relatives.is_empty() {
        return Ok(false);
    }
    let run = |args: &[&str], relatives: &[&String]| {
        let specs: Vec<String> = relatives.iter().map(|r| pathspec(r)).collect();
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        git(paths, repo, &[args, &specs].concat(), LOCAL_TIMEOUT)
    };
    let (present, missing): (Vec<&String>, Vec<&String>) = relatives
        .iter()
        .partition(|r| std::fs::symlink_metadata(repo.join(r)).is_ok());
    if !present.is_empty() {
        run(&["add", "-A", "--"], &present)?;
    }
    if !missing.is_empty() {
        run(
            &["rm", "--cached", "-q", "--ignore-unmatch", "--"],
            &missing,
        )?;
    }
    let all: Vec<&String> = relatives.iter().collect();
    let out = run(
        &[
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "-z",
            "--",
        ],
        &all,
    )?;
    let staged: Vec<String> = out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    if staged.is_empty() {
        return Ok(false);
    }
    let staged: Vec<&String> = staged.iter().collect();
    run(&["commit", "-q", "-m", message, "--only", "--"], &staged)?;
    Ok(true)
}

fn pathspec(relative: &str) -> String {
    format!(":(literal){relative}")
}

/// `relative` with `/` separators, or `None` if it has anything but plain names, such as `..`.
fn repo_relative(relative: &Path) -> Option<String> {
    let names = relative
        .components()
        .map(|c| match c {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!names.is_empty()).then(|| names.join("/"))
}

/// Whether `relative` (`/`-separated) has one of the shapes the `.gitignore` lets through. A nested
/// `.gitignore` can un-ignore anything below it, so this is checked too.
fn is_allowlisted(relative: &str) -> bool {
    let shaped = |name: &str, prefix: &str| name.starts_with(prefix) && name.ends_with(".md");
    match relative.split('/').collect::<Vec<_>>().as_slice() {
        [".gitignore" | ".gitattributes"] => true,
        ["tworky", file] => shaped(file, "claude.local"),
        ["projects", project, file] if !project.is_empty() => {
            *file == "project.toml" || shaped(file, "memory")
        }
        _ => false,
    }
}

fn is_project_file(relative: &Path) -> bool {
    let parts: Vec<_> = relative.components().collect();
    matches!(
        parts.as_slice(),
        [Component::Normal(projects), Component::Normal(_), Component::Normal(file)]
            if *projects == "projects" && *file == "project.toml"
    )
}

fn check_project(file: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    toml::from_str::<ProjectConfig>(&text)
        .with_context(|| format!("{} doesn't parse", file.display()))?;
    Ok(())
}

pub struct Sweep {
    pub committed: bool,
    /// `project.toml` files that changed but don't parse, so weren't committed.
    pub held_back: Vec<PathBuf>,
}

/// Commits every allowlisted change in `repo/`, such as edits by setup sessions or by hand, except
/// a `project.toml` that doesn't parse.
pub fn sweep(_guard: &RepoLock, paths: &Paths) -> Result<Sweep> {
    let repo = paths.repo_dir();
    let mut held_back = Vec::new();
    let mut relatives = Vec::new();
    for relative in changes(paths, &repo)? {
        let file = repo.join(&relative);
        if is_project_file(Path::new(&relative)) && file.exists() && check_project(&file).is_err() {
            held_back.push(file);
        } else {
            relatives.push(relative);
        }
    }
    let committed = commit_paths(paths, &repo, &relatives, "Commit edits made outside Tworky")?;
    Ok(Sweep {
        committed,
        held_back,
    })
}

/// Changed, new and deleted allowlisted paths, relative to the repo with `/` separators.
fn changes(paths: &Paths, repo: &Path) -> Result<Vec<String>> {
    let out = git(
        paths,
        repo,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
        LOCAL_TIMEOUT,
    )?;
    let mut changed = Vec::new();
    let mut entries = out.split('\0');
    while let Some(entry) = entries.next() {
        let Some((code, path)) = entry.split_at_checked(3) else {
            continue;
        };
        changed.push(path.to_string());
        if code.contains(['R', 'C'])
            && let Some(original) = entries.next()
        {
            changed.push(original.to_string());
        }
    }
    changed.retain(|path| is_allowlisted(path));
    Ok(changed)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// Uncommitted allowlisted paths, relative to the repo.
    pub changes: Vec<String>,
    /// Commits `main` is ahead of and behind its upstream, when it has one.
    pub ahead_behind: Option<(u32, u32)>,
    /// An `index.lock` older than any git command this module runs, so one a crash left behind.
    pub stale_index_lock: bool,
    pub sync: SyncState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    NoRemote,
    Idle,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.changes.is_empty()
    }
}

pub fn status(paths: &Paths) -> Result<Status> {
    let repo = paths.repo_dir();
    let git = |args: &[&str]| git(paths, &repo, args, LOCAL_TIMEOUT);
    let changes = changes(paths, &repo)?;
    let ahead_behind =
        if git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).is_ok() {
            let counts = git(&["rev-list", "--left-right", "--count", "HEAD...@{u}"])?;
            let mut counts = counts.split_whitespace().map(str::parse::<u32>);
            match (counts.next(), counts.next()) {
                (Some(Ok(ahead)), Some(Ok(behind))) => Some((ahead, behind)),
                _ => bail!("unexpected `git rev-list --count` output: {counts:?}"),
            }
        } else {
            None
        };
    let stale_index_lock = std::fs::metadata(repo.join(".git").join("index.lock"))
        .and_then(|m| m.modified())
        .is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > LOCAL_TIMEOUT)
        });
    let sync = if git(&["remote", "get-url", "origin"]).is_ok() {
        SyncState::Idle
    } else {
        SyncState::NoRemote
    };
    Ok(Status {
        changes,
        ahead_behind,
        stale_index_lock,
        sync,
    })
}

pub fn write_machine(_guard: &RepoLock, paths: &Paths, text: &str) -> Result<()> {
    std::fs::create_dir_all(&paths.config_dir)?;
    crate::memory::write_atomic(&paths.machine_file(), text.as_bytes())
}

/// Points this thread's git calls at an empty global config in `dir`, which every test reaching
/// [`git`] needs.
#[cfg(test)]
pub(crate) fn isolate_git_for_tests(dir: &Path) {
    let global = dir.join("gitconfig");
    std::fs::write(&global, "").unwrap();
    tests::use_global_config(global);
}

/// A config home in `dir` with git isolated for this thread and an empty repo.
#[cfg(test)]
pub(crate) fn test_home(dir: &Path) -> Paths {
    isolate_git_for_tests(dir);
    let paths = Paths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
    };
    init(&with_lock(&paths).unwrap(), &paths).unwrap();
    paths
}

/// The repo's commit subjects, newest first.
#[cfg(test)]
pub(crate) fn log_for_tests(paths: &Paths) -> Vec<String> {
    git(
        paths,
        &paths.repo_dir(),
        &["log", "--format=%s"],
        LOCAL_TIMEOUT,
    )
    .unwrap()
    .lines()
    .map(String::from)
    .collect()
}

/// The files the newest commit changed.
#[cfg(test)]
pub(crate) fn last_commit_files_for_tests(paths: &Paths) -> Vec<String> {
    git(
        paths,
        &paths.repo_dir(),
        &["show", "--name-only", "--format=", "HEAD"],
        LOCAL_TIMEOUT,
    )
    .unwrap()
    .lines()
    .filter(|l| !l.is_empty())
    .map(String::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    thread_local! {
        static GLOBAL_CONFIG: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static FAIL: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    const CHILD_HOME: &str = "TWORKY_TEST_LOCK_CHILD_HOME";

    pub(super) fn isolate(cmd: &mut std::process::Command, args: &[&str]) -> Result<()> {
        let global = GLOBAL_CONFIG
            .with_borrow(|g| g.clone())
            .expect("the test sets a global git config");
        cmd.env("GIT_CONFIG_GLOBAL", global)
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for var in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "EMAIL",
        ] {
            cmd.env_remove(var);
        }
        if let Some(fail) = FAIL.get()
            && args.contains(&fail)
        {
            bail!("injected failure of `git {fail}`");
        }
        Ok(())
    }

    pub(super) fn use_global_config(global: PathBuf) {
        GLOBAL_CONFIG.set(Some(global));
        FAIL.set(None);
    }

    fn paths_under(home: &Path) -> Paths {
        Paths {
            config_dir: home.join("config"),
            data_dir: home.join("data"),
        }
    }

    struct Home {
        dir: tempfile::TempDir,
        paths: Paths,
    }

    impl Home {
        fn new(global_config: &str) -> Home {
            let dir = tempfile::tempdir().unwrap();
            let global = dir.path().join("gitconfig");
            std::fs::write(&global, global_config).unwrap();
            use_global_config(global);
            let paths = paths_under(dir.path());
            Home { dir, paths }
        }

        fn init(&self) {
            init(&with_lock(&self.paths).unwrap(), &self.paths).unwrap();
        }

        fn git(&self, args: &[&str]) -> String {
            git(&self.paths, &self.paths.repo_dir(), args, LOCAL_TIMEOUT).unwrap()
        }

        fn lines(&self, args: &[&str]) -> Vec<String> {
            self.git(args).lines().map(String::from).collect()
        }

        fn log(&self) -> Vec<String> {
            self.lines(&["log", "--format=%s"])
        }

        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let file = self.paths.repo_dir().join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, text).unwrap();
            file
        }
    }

    const PROJECT: &str = "name = \"acme\"\n\n[match]\nowner = \"acme\"\n\n\
                           [[repos]]\nname = \"api\"\nsource = \"C:/src/api\"\n";

    #[test]
    fn init_makes_a_repo_with_its_allowlist_config_and_guidance() {
        let home = Home::new("");
        home.init();

        assert!(!home.paths.repo_tmp_dir().exists());
        assert_eq!(home.log(), ["Start the Tworky config"]);
        assert_eq!(
            home.git(&["log", "-1", "--format=%an <%ae>"]).trim(),
            "Tworky <tworky@localhost>"
        );
        assert_eq!(home.git(&["branch", "--show-current"]).trim(), "main");
        assert_eq!(
            home.lines(&["ls-files"]),
            [
                ".gitattributes",
                ".gitignore",
                "tworky/claude.local.linux.md",
                "tworky/claude.local.macos.md",
                "tworky/claude.local.md",
                "tworky/claude.local.windows.md",
            ]
        );
        let local = |key: &str| home.git(&["config", "--local", key]).trim().to_string();
        assert_eq!(local("commit.gpgsign"), "false");
        assert_eq!(local("core.autocrlf"), "false");
        assert_eq!(local("rerere.enabled"), "true");
        assert_eq!(
            local("core.hooksPath"),
            forward_slashes(&home.paths.repo_hooks_dir())
        );

        let status = status(&home.paths).unwrap();
        assert!(status.is_clean(), "{status:?}");
        assert_eq!(status.ahead_behind, None);
        assert!(!status.stale_index_lock);
        assert_eq!(status.sync, SyncState::NoRemote);

        let again = init(&with_lock(&home.paths).unwrap(), &home.paths).unwrap_err();
        assert!(again.to_string().contains("already exists"), "{again:#}");
    }

    #[test]
    fn a_failed_first_commit_leaves_no_repo() {
        let home = Home::new("");
        FAIL.set(Some("commit"));
        let e = init(&with_lock(&home.paths).unwrap(), &home.paths).unwrap_err();
        assert!(format!("{e:#}").contains("injected failure"), "{e:#}");
        assert!(!home.paths.repo_dir().exists());
        assert!(!home.paths.repo_tmp_dir().exists());

        FAIL.set(None);
        home.init();
        assert_eq!(home.log(), ["Start the Tworky config"]);
    }

    #[test]
    fn init_replaces_a_leftover_tmp_repo() {
        let home = Home::new("");
        let tmp = home.paths.repo_tmp_dir();
        std::fs::create_dir_all(tmp.join("projects").join("old")).unwrap();
        let leftover = tmp.join("projects").join("old").join("project.toml");
        std::fs::write(&leftover, PROJECT).unwrap();
        let mut readonly = std::fs::metadata(&leftover).unwrap().permissions();
        readonly.set_readonly(true);
        std::fs::set_permissions(&leftover, readonly).unwrap();

        home.init();

        assert!(!tmp.exists());
        assert!(!home.paths.project_dir("old").exists());
        assert_eq!(home.log(), ["Start the Tworky config"]);
    }

    #[test]
    fn commit_stages_only_the_given_files_and_skips_no_ops() {
        let home = Home::new("");
        home.init();
        let guard = with_lock(&home.paths).unwrap();
        let memory = home.write("projects/acme/memory.md", "# Acme\n");
        let project = home.write("projects/acme/project.toml", PROJECT);

        assert!(commit(&guard, &home.paths, &[&memory], "Save acme memory").unwrap());
        assert!(!commit(&guard, &home.paths, &[&memory], "Again").unwrap());
        assert_eq!(home.log(), ["Save acme memory", "Start the Tworky config"]);
        assert!(
            home.lines(&["ls-files"])
                .contains(&"projects/acme/memory.md".into())
        );
        assert_eq!(
            status(&home.paths).unwrap().changes,
            ["projects/acme/project.toml"]
        );

        std::fs::write(&project, "name = [").unwrap();
        let e = commit(&guard, &home.paths, &[&project], "Broken").unwrap_err();
        assert!(e.to_string().contains("doesn't parse"), "{e:#}");
        assert_eq!(home.log().len(), 2);

        std::fs::remove_file(&memory).unwrap();
        assert!(
            commit(
                &guard,
                &home.paths,
                &[Path::new("projects/acme/memory.md")],
                "Forget acme memory"
            )
            .unwrap()
        );
        assert!(
            !home
                .lines(&["ls-files"])
                .contains(&"projects/acme/memory.md".into())
        );
    }

    #[test]
    fn sweep_holds_back_a_broken_project_toml_and_ignores_strays() {
        let home = Home::new("");
        home.init();
        let guard = with_lock(&home.paths).unwrap();
        home.write("projects/acme/project.toml", PROJECT);
        home.write("projects/acme/memory.md", "# Acme\n");
        home.write("projects/acme/memory.windows.md", "# Windows\n");
        home.write("projects/acme/.env", "SECRET=1\n");
        home.write("projects/acme/templates/api/.env", "SECRET=1\n");
        home.write("notes.txt", "stray\n");
        home.write("tworky/claude.local.md", "Be brief.\n");
        let broken = home.write("projects/beta/project.toml", "name = [");

        let swept = sweep(&guard, &home.paths).unwrap();
        assert!(swept.committed);
        assert_eq!(swept.held_back, std::slice::from_ref(&broken));
        let tracked = home.lines(&["ls-files"]);
        for file in [
            "projects/acme/project.toml",
            "projects/acme/memory.md",
            "projects/acme/memory.windows.md",
        ] {
            assert!(tracked.contains(&file.into()), "{file} in {tracked:?}");
        }
        assert_eq!(tracked.len(), 9, "{tracked:?}");
        assert_eq!(
            home.git(&["show", "HEAD:tworky/claude.local.md"]),
            "Be brief.\n"
        );
        assert_eq!(
            status(&home.paths).unwrap().changes,
            ["projects/beta/project.toml"]
        );

        let again = sweep(&guard, &home.paths).unwrap();
        assert!(!again.committed);
        assert_eq!(again.held_back, [broken]);
        assert_eq!(home.log().len(), 2);
    }

    #[test]
    fn commit_skips_a_file_that_never_existed_and_refuses_one_outside_the_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        isolate_git_for_tests(dir.path());
        let paths = paths_under(dir.path());
        init(&with_lock(&paths).unwrap(), &paths).unwrap();
        let guard = with_lock(&paths).unwrap();
        let ghost = Path::new("projects/ghost/memory.md");

        assert!(!commit(&guard, &paths, &[ghost], "Nothing").unwrap());

        let memory = paths.project_dir("acme").join("memory.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, "# Acme\n").unwrap();
        assert!(
            commit(
                &guard,
                &paths,
                &[ghost, memory.as_path()],
                "Save acme memory"
            )
            .unwrap()
        );
        let log = git(
            &paths,
            &paths.repo_dir(),
            &["log", "--format=%s"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        assert_eq!(log, "Save acme memory\nStart the Tworky config\n");

        let env = paths.project_dir("acme").join(".env");
        std::fs::write(&env, "SECRET=1\n").unwrap();
        for file in [env.as_path(), Path::new("projects/acme/memory.md.tmp")] {
            let e = commit(&guard, &paths, &[file], "Secret").unwrap_err();
            assert!(e.to_string().contains("isn't a file"), "{e:#}");
        }
    }

    #[test]
    fn sweep_commits_a_deletion_already_staged() {
        let home = Home::new("");
        home.init();
        let guard = with_lock(&home.paths).unwrap();
        let memory = home.write("projects/acme/memory.md", "# Acme\n");
        assert!(commit(&guard, &home.paths, &[&memory], "Save acme memory").unwrap());

        home.git(&["rm", "-q", "projects/acme/memory.md"]);
        let swept = sweep(&guard, &home.paths).unwrap();
        assert!(swept.committed);
        assert!(
            !home
                .lines(&["ls-files"])
                .contains(&"projects/acme/memory.md".into())
        );
        assert!(status(&home.paths).unwrap().is_clean());
        assert!(!sweep(&guard, &home.paths).unwrap().committed);
    }

    #[test]
    fn sweep_ignores_files_a_nested_gitignore_lets_through() {
        let home = Home::new("");
        home.init();
        let guard = with_lock(&home.paths).unwrap();
        home.write("projects/acme/.gitignore", "!*\n");
        home.write("projects/acme/.env", "SECRET=1\n");
        home.write("projects/acme/memory.md", "# Acme\n");
        let untracked = home.git(&["status", "--porcelain", "--untracked-files=all"]);
        assert!(untracked.contains("projects/acme/.env"), "{untracked}");

        assert!(sweep(&guard, &home.paths).unwrap().committed);
        let tracked = home.lines(&["ls-files"]);
        assert!(tracked.contains(&"projects/acme/memory.md".into()));
        assert!(
            !tracked.contains(&"projects/acme/.env".into()),
            "{tracked:?}"
        );
        assert!(
            !tracked.contains(&"projects/acme/.gitignore".into()),
            "{tracked:?}"
        );
        assert!(status(&home.paths).unwrap().is_clean());
    }

    #[test]
    fn the_allowlist_matches_only_config_shapes() {
        for path in [
            ".gitignore",
            ".gitattributes",
            "tworky/claude.local.md",
            "tworky/claude.local.windows.md",
            "projects/acme/project.toml",
            "projects/acme/memory.md",
            "projects/acme/memory.linux.md",
        ] {
            assert!(is_allowlisted(path), "{path}");
        }
        for path in [
            "notes.txt",
            "projects/acme/.gitignore",
            "projects/acme/.env",
            "projects/acme/memory.md.tmp",
            "projects/acme/memory.md.lock",
            "projects/acme/sub/memory.md",
            "projects//memory.md",
            "projects/project.toml",
            "tworky/claude.md",
            "tworky/sub/claude.local.md",
        ] {
            assert!(!is_allowlisted(path), "{path}");
        }
    }

    #[test]
    fn a_global_gpgsign_does_not_affect_commits() {
        let home = Home::new(
            "[user]\n\tname = Test User\n\temail = test@example.com\n\
             [commit]\n\tgpgsign = true\n[gpg]\n\tprogram = tworky-missing-gpg\n",
        );
        home.init();
        let guard = with_lock(&home.paths).unwrap();
        let memory = home.write("projects/acme/memory.md", "# Acme\n");
        assert!(commit(&guard, &home.paths, &[&memory], "Save acme memory").unwrap());

        assert_eq!(
            home.lines(&["log", "--format=%an <%ae>"]),
            [
                "Test User <test@example.com>",
                "Test User <test@example.com>"
            ]
        );
        assert!(
            git(
                &home.paths,
                &home.paths.repo_dir(),
                &["config", "--local", "user.name"],
                LOCAL_TIMEOUT
            )
            .is_err()
        );
        let signed = git(
            &home.paths,
            &home.paths.repo_dir(),
            &["commit", "--allow-empty", "-S", "-m", "Signed"],
            LOCAL_TIMEOUT,
        );
        assert!(signed.is_err(), "signing should fail with this config");
    }

    #[test]
    fn status_reports_ahead_behind_and_a_stale_index_lock() {
        let home = Home::new("");
        home.init();
        let remote = home.dir.path().join("remote.git");
        git(
            &home.paths,
            home.dir.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        home.git(&["remote", "add", "origin", &forward_slashes(&remote)]);
        home.git(&["push", "-q", "-u", "origin", "main"]);
        let guard = with_lock(&home.paths).unwrap();
        let memory = home.write("projects/acme/memory.md", "# Acme\n");
        commit(&guard, &home.paths, &[&memory], "Save acme memory").unwrap();

        let lock = home.paths.repo_dir().join(".git").join("index.lock");
        File::create(&lock).unwrap();
        let fresh = status(&home.paths).unwrap();
        assert_eq!(fresh.ahead_behind, Some((1, 0)));
        assert_eq!(fresh.sync, SyncState::Idle);
        assert!(!fresh.stale_index_lock);

        File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        assert!(status(&home.paths).unwrap().stale_index_lock);
        std::fs::remove_file(&lock).unwrap();
    }

    #[test]
    fn writers_in_two_processes_take_turns_on_the_lock() {
        let home = Home::new("");
        home.init();
        let guard = with_lock(&home.paths).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "config_repo::tests::lock_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_HOME, home.dir.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        let waiting = home.dir.path().join("child-waiting");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !waiting.exists() {
            assert!(
                Instant::now() < deadline,
                "the child never reached the lock"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "the child exited early"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(750));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the child went ahead while the lock was held"
        );
        assert_eq!(home.log(), ["Start the Tworky config"]);

        drop(guard);
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert_eq!(home.log(), ["Child write", "Start the Tworky config"]);
    }

    /// A home laid out as before the config repo: a project file whose stem differs from its name,
    /// with a real clone, a repo only `match.repos` names, one nothing names and a template folder;
    /// a broken project; memory for both, an orphan memory and a stale lock file.
    fn legacy_home() -> (Home, PathBuf, PathBuf) {
        let home = Home::new("");
        let clone = home.dir.path().join("src").join("api");
        std::fs::create_dir_all(&clone).unwrap();
        git(
            &home.paths,
            &clone,
            &["init", "-q", "-b", "main"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        git(
            &home.paths,
            &clone,
            &["remote", "add", "origin", "git@github.com:acme/api.git"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        let templates = home.dir.path().join("acme-templates");
        std::fs::create_dir_all(&templates).unwrap();
        let config = &home.paths.config_dir;
        let write = |relative: &str, text: &str| {
            let file = config.join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        };
        write(
            "projects/acme-old.toml",
            &format!(
                "# Acme\nname = \"acme\"\ntemplate_dir = '{}'\n\n[match]\nrepos = [\"acme/api\", \"acme/web\"]\n\n\
                 [[repos]]\nname = \"api\" # the API\nsource = '{}'\n\n\
                 [[repos]]\nname = \"web\"\nsource = '{}'\n\n\
                 [[repos]]\nname = \"misc\"\nsource = 'C:/nowhere/misc'\n",
                templates.display(),
                clone.display(),
                home.dir.path().join("gone").display(),
            ),
        );
        write("projects/beta.toml", "name = [");
        write("projects/notes.txt", "not a project");
        write("memory/acme.md", "# Acme memory\n");
        write("memory/beta.md", "# Beta memory\n");
        write("memory/orphan.md", "# Orphan\n");
        write("memory/acme.md.lock", "");
        write("config.toml", "port_stride = 50\n");
        (home, clone, templates)
    }

    #[test]
    fn migration_imports_a_legacy_home_in_one_commit() {
        let (home, clone, templates) = legacy_home();
        let paths = &home.paths;

        let done = migrate(paths).unwrap();
        assert!(
            done.iter()
                .any(|l| l.contains("orphan.md") && l.contains("no project")),
            "{done:?}"
        );
        assert_eq!(home.log(), ["Import existing config"]);
        assert!(!paths.repo_tmp_dir().exists());

        let acme = std::fs::read_to_string(paths.project_file("acme")).unwrap();
        assert!(
            acme.contains("# Acme") && acme.contains("# the API"),
            "{acme}"
        );
        assert!(
            !acme.contains("source") && !acme.contains("template_dir"),
            "{acme}"
        );
        let (project, warnings) = crate::config::parse_project(&acme).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let repos: Vec<_> = project
            .repos
            .iter()
            .map(|r| (r.name.as_str(), r.repo.as_deref()))
            .collect();
        assert_eq!(
            repos,
            [
                ("api", Some("acme/api")),
                ("web", Some("acme/web")),
                ("misc", None)
            ]
        );

        let machine = MachineConfig::read(paths).unwrap();
        assert_eq!(machine.source("acme", "api"), Some(&clone));
        assert_eq!(machine.source("acme", "web"), None, "its clone is gone");
        assert_eq!(machine.template_dir("acme"), Some(&templates));

        assert_eq!(
            std::fs::read_to_string(paths.memory_file("acme", None)).unwrap(),
            "# Acme memory\n"
        );
        let tracked = home.lines(&["ls-files", "projects"]);
        assert_eq!(
            tracked,
            [
                "projects/acme/memory.md",
                "projects/acme/project.toml",
                "projects/beta/memory.md"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(paths.project_file("beta")).unwrap(),
            "name = [",
            "kept as it was, for the user to fix"
        );
        assert_eq!(
            status(paths).unwrap().changes,
            ["projects/beta/project.toml"]
        );
        assert!(!paths.project_dir("orphan").exists());

        assert!(!paths.legacy_projects_dir().exists());
        assert!(!paths.legacy_memory_dir().exists());
        let backup = paths.config_dir.join("memory.pre-repo");
        assert!(backup.join("orphan.md").exists());
        assert!(
            paths
                .config_dir
                .join("projects.pre-repo")
                .join("acme-old.toml")
                .exists()
        );

        let config = crate::config::Config::load_from(paths.clone()).unwrap();
        assert_eq!(config.app.port_stride, 50);
        let loaded = config.project("acme").unwrap();
        assert_eq!(loaded.repos[0].source, Some(clone));
        assert_eq!(loaded.repos[1].source, None);
        assert_eq!(config.template_dir(loaded), templates);
        assert!(config.project_errors.contains_key("beta"));

        assert!(migrate(paths).unwrap().is_empty(), "a rerun does nothing");
        assert_eq!(home.log(), ["Import existing config"]);
    }

    #[test]
    fn a_failed_import_leaves_the_legacy_layout_and_retries_next_time() {
        let (home, _, _) = legacy_home();
        let paths = &home.paths;
        FAIL.set(Some("commit"));
        let e = migrate(paths).unwrap_err();
        assert!(format!("{e:#}").contains("injected failure"), "{e:#}");
        assert!(!paths.repo_dir().exists());
        assert!(!paths.repo_tmp_dir().exists());
        assert!(paths.legacy_projects_dir().join("acme-old.toml").exists());
        assert!(paths.legacy_memory_dir().join("acme.md").exists());

        FAIL.set(None);
        migrate(paths).unwrap();
        assert_eq!(home.log(), ["Import existing config"]);
        assert!(!paths.legacy_projects_dir().exists());
    }

    #[test]
    fn a_crash_before_the_legacy_rename_is_finished_next_time() {
        let (home, _, _) = legacy_home();
        let paths = &home.paths;
        import(&with_lock(paths).unwrap(), paths).unwrap();
        std::fs::rename(
            paths.legacy_projects_dir(),
            paths.config_dir.join("projects.pre-repo"),
        )
        .unwrap();
        assert!(paths.legacy_memory_dir().exists());

        let done = migrate(paths).unwrap();
        assert_eq!(done.len(), 1, "{done:?}");
        assert!(!paths.legacy_memory_dir().exists());
        assert!(
            paths
                .config_dir
                .join("memory.pre-repo")
                .join("acme.md")
                .exists()
        );
        assert_eq!(home.log(), ["Import existing config"]);
    }

    #[test]
    fn a_home_with_only_config_toml_gets_an_empty_repo() {
        let home = Home::new("");
        std::fs::create_dir_all(&home.paths.config_dir).unwrap();
        std::fs::write(home.paths.app_config(), "port_stride = 50\n").unwrap();
        std::fs::create_dir_all(home.paths.legacy_projects_dir()).unwrap();

        let done = migrate(&home.paths).unwrap();
        assert!(done[0].starts_with("created the config repo"), "{done:?}");
        assert_eq!(home.log(), ["Start the Tworky config"]);
        assert!(!home.paths.legacy_projects_dir().exists());
        assert!(migrate(&home.paths).unwrap().is_empty());
    }

    /// The other process in `writers_in_two_processes_take_turns_on_the_lock`; does nothing in a
    /// normal run.
    #[test]
    fn lock_child() {
        let Some(home) = std::env::var_os(CHILD_HOME).map(PathBuf::from) else {
            return;
        };
        GLOBAL_CONFIG.set(Some(home.join("gitconfig")));
        let paths = paths_under(&home);
        std::fs::write(home.join("child-waiting"), "").unwrap();
        let guard = with_lock(&paths).unwrap();
        let memory = paths.project_dir("acme").join("memory.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, "# From the child\n").unwrap();
        assert!(commit(&guard, &paths, &[&memory], "Child write").unwrap());
    }
}
