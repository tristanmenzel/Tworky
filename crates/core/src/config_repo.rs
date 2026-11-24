use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{MachineConfig, Os, Paths, ProjectConfig};
use crate::github::{self, RepoView, Visibility};
use crate::process;

pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

const FALLBACK_NAME: &str = "Tworky";
const FALLBACK_EMAIL: &str = "tworky@localhost";
const FALLBACK_NAME_ARG: &str = "user.name=Tworky";
const FALLBACK_EMAIL_ARG: &str = "user.email=tworky@localhost";

const INIT_MESSAGE: &str = "Start the Tworky config";
const JOIN_MESSAGE: &str = "Join the shared Tworky config";
const MERGE_MESSAGE: &str = "Merge the shared Tworky config";

const REMOTE: &str = "origin";

/// The repo name the installed app's remote defaults to, which a debug build never syncs with.
const RELEASE_REMOTE_NAME: &str = "tworky-config";

const DEV_REMOTE_NAME: &str = "tworky-config-dev";

/// How long a rename in the config folder is retried while Windows holds a file in it open.
const RENAME_RETRY: Duration = Duration::from_secs(10);

/// [`RENAME_RETRY`] for renaming the legacy folders, which is retried on every start anyway.
const LEGACY_RENAME_RETRY: Duration = Duration::from_secs(1);

/// Keyed by config folder, for [`recorded_failures`].
type Failures = Mutex<BTreeMap<PathBuf, String>>;

static MIGRATION_FAILURES: Failures = Mutex::new(BTreeMap::new());
static COMMIT_FAILURES: Failures = Mutex::new(BTreeMap::new());

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

/// [`with_lock`] for a writer of files in `repo/`, which refuses while the repo isn't set up, so a
/// write never lands in a `repo/` folder that git doesn't track.
pub fn lock_for_write(paths: &Paths) -> Result<RepoLock> {
    let guard = with_lock(paths)?;
    if !paths.has_repo() {
        bail!(
            "the config repo {} isn't set up, so config can't be changed yet; restart Tworky or run `tw project ls` to set it up",
            paths.repo_dir().display()
        );
    }
    Ok(guard)
}

/// What went wrong in this process that the user should see: setting up the config repo, while
/// it's still missing, and the last commit or sweep, until one succeeds.
pub fn recorded_failures(paths: &Paths) -> Vec<String> {
    let mut failures = Vec::new();
    let dir = &paths.config_dir;
    if !paths.has_repo()
        && let Some(e) = MIGRATION_FAILURES.lock().unwrap().get(dir)
    {
        failures.push(format!("couldn't set up the config repo: {e}"));
    }
    if let Some(e) = COMMIT_FAILURES.lock().unwrap().get(dir) {
        failures.push(format!("couldn't commit to the config repo: {e}"));
    }
    failures
}

fn record<T>(failures: &Failures, paths: &Paths, result: &Result<T>) {
    let mut failures = failures.lock().unwrap();
    match result {
        Ok(_) => failures.remove(&paths.config_dir),
        Err(e) => failures.insert(paths.config_dir.clone(), format!("{e:#}")),
    };
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
/// the user's own git config can't make it fail or hang. Git never looks above the config folder,
/// so a `repo/` without `.git` can't resolve to a repo enclosing it.
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
    .env("GIT_EDITOR", "true")
    .env(
        "GIT_CEILING_DIRECTORIES",
        forward_slashes(&paths.config_dir),
    );
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
    let shown: Vec<String> = args.iter().map(|arg| redact_url(arg)).collect();
    let shown: Vec<&str> = shown.iter().map(String::as_str).collect();
    process::output_timeout(cmd, &process::describe("git", &shown), timeout).map_err(|e| {
        let message = format!("{e:#}");
        let redacted = redact_url(&message);
        if redacted == message {
            e
        } else {
            anyhow::anyhow!(redacted)
        }
    })
}

/// Replaces the userinfo of every `scheme://user:password@host` URL anywhere in `text` with `***`.
pub fn redact_url(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| {
                matches!(c, '/' | '\\' | '?' | '#' | '\'' | '"' | '`') || c.is_whitespace()
            })
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Creates `repo/` with its first commit. It's built in `repo.tmp/` and renamed into place, so a
/// failure leaves no `repo/`.
pub fn init(_guard: &RepoLock, paths: &Paths) -> Result<()> {
    create(paths, INIT_MESSAGE, |_| Ok(vec![]))
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
        .and_then(|()| rename_retrying(&tmp, &repo, RENAME_RETRY));
    if built.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    built.with_context(|| format!("creating the config repo {}", repo.display()))?;
    forget_sync_record(paths);
    Ok(())
}

/// Starts a new sync record for a repo that was just created or cloned, since the commits an
/// older repo's record names aren't in it. A record that can't be reset is only logged:
/// [`resumable`] starts over from commits it can't find anyway.
fn forget_sync_record(paths: &Paths) {
    if let Err(e) = update_record(paths, |r| *r = SyncRecord::default()) {
        eprintln!("resetting the config sync record: {e:#}");
    }
}

fn scaffold(paths: &Paths, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::create_dir_all(paths.repo_hooks_dir())?;
    git(paths, dir, &["init", "-q", "-b", "main"], LOCAL_TIMEOUT)?;
    configure(paths, dir)?;

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

/// Writes the repo-local git config every config repo has, which its linked worktrees share.
fn configure(paths: &Paths, dir: &Path) -> Result<()> {
    let hooks = forward_slashes(&paths.repo_hooks_dir());
    for (key, value) in [
        ("commit.gpgsign", "false"),
        ("core.hooksPath", hooks.as_str()),
        ("core.autocrlf", "false"),
        ("rerere.enabled", "true"),
    ] {
        git(paths, dir, &["config", key, value], LOCAL_TIMEOUT)?;
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
    let identity = identity_args(paths, dir);
    git(&[&identity[..], &["commit", "-q", "-m", message]].concat())?;
    Ok(())
}

/// `-c` arguments for a commit, giving git Tworky's identity for whichever of `user.name` and
/// `user.email` the user hasn't set. Never written to the repo's config, so an identity the user
/// sets later takes over.
fn identity_args(paths: &Paths, dir: &Path) -> Vec<&'static str> {
    let (name, email) = unset_identity(paths, dir);
    let mut args = Vec::new();
    if name {
        args.extend(["-c", FALLBACK_NAME_ARG]);
    }
    if email {
        args.extend(["-c", FALLBACK_EMAIL_ARG]);
    }
    args
}

/// [`identity_args`] as environment variables, for git commands Tworky doesn't run itself, such
/// as the settings session's.
pub fn identity_env(paths: &Paths, dir: &Path) -> Vec<(&'static str, &'static str)> {
    let (name, email) = unset_identity(paths, dir);
    let mut env = Vec::new();
    if name {
        env.extend([
            ("GIT_AUTHOR_NAME", FALLBACK_NAME),
            ("GIT_COMMITTER_NAME", FALLBACK_NAME),
        ]);
    }
    if email {
        env.extend([
            ("GIT_AUTHOR_EMAIL", FALLBACK_EMAIL),
            ("GIT_COMMITTER_EMAIL", FALLBACK_EMAIL),
        ]);
    }
    env
}

/// Whether `user.name` and `user.email` are unset for git in `dir`.
fn unset_identity(paths: &Paths, dir: &Path) -> (bool, bool) {
    let unset = |key: &str| {
        !git(paths, dir, &["config", key], LOCAL_TIMEOUT).is_ok_and(|v| !v.trim().is_empty())
    };
    (unset("user.name"), unset("user.email"))
}

/// Brings the config repo into being, then renames the legacy folders. See
/// [`Paths::migrate_to_repo`]. A failure is kept for [`recorded_failures`].
pub fn migrate(paths: &Paths) -> Result<Vec<String>> {
    let set_up = set_up(paths);
    record(&MIGRATION_FAILURES, paths, &set_up);
    let mut done = set_up.context("could not set up the config repo")?;
    done.extend(rename_legacy(paths)?);
    Ok(done)
}

fn set_up(paths: &Paths) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if !paths.has_repo() {
        let guard = with_lock(paths)?;
        if !paths.has_repo() {
            done.extend(set_aside_untracked_repo(paths)?);
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
    Ok(done)
}

/// Moves a `repo/` with no `.git` out of the way of the new repo, keeping it for the user to look
/// through. This build never makes one, but a build from before writers and setup launches
/// checked for the repo could, after a failed migration.
fn set_aside_untracked_repo(paths: &Paths) -> Result<Vec<String>> {
    let repo = paths.repo_dir();
    if !repo.exists() {
        return Ok(vec![]);
    }
    let aside = unused_sibling(&repo, "no-git");
    rename_retrying(&repo, &aside, RENAME_RETRY)?;
    Ok(vec![format!(
        "moved {} aside to {}: it had no .git",
        repo.display(),
        aside.display()
    )])
}

/// `<dir>.<suffix>`, or `<dir>.<suffix>-<n>` for the first `n` not taken.
fn unused_sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    std::iter::once(format!("{name}.{suffix}"))
        .chain((2..).map(|n| format!("{name}.{suffix}-{n}")))
        .map(|b| dir.with_file_name(b))
        .find(|b| !b.exists())
        .expect("an unused name exists")
}

fn has_legacy_config(paths: &Paths) -> bool {
    !files_with_extension(&paths.legacy_projects_dir(), "toml").is_empty()
        || !files_with_extension(&paths.legacy_memory_dir(), "md").is_empty()
}

/// Files directly in `dir` with `extension`, sorted.
pub(crate) fn files_with_extension(dir: &Path, extension: &str) -> Vec<PathBuf> {
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
                (Ok(imported), _) if folders.contains(&imported.name) => {
                    report.push(format!(
                        "left {} in the backup: project {} was already imported",
                        file.display(),
                        imported.name
                    ));
                    continue;
                }
                (Ok(imported), _) => {
                    report.push(format!(
                        "imported {} as project {}",
                        file.display(),
                        imported.name
                    ));
                    for repo in &imported.without_repo {
                        report.push(format!(
                            "project {}: repo {repo} has no `repo = \"owner/name\"`; add it so its clone can be found",
                            imported.name
                        ));
                    }
                    (imported.name, imported.text)
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

struct Imported {
    name: String,
    text: String,
    /// Repos left with no `repo`, which couldn't be guessed.
    without_repo: Vec<String>,
}

/// The project's file for the repo. Each repo's `source` and the `template_dir` move to `machine`
/// unless it already has one, even when the path is missing right now (on a drive that isn't
/// mounted, say). Each repo gets a `repo` from its clone's origin, else from the one `match.repos`
/// entry with its name.
fn import_project(text: &str, machine: &mut MachineConfig) -> Result<Imported> {
    use toml_edit::{DocumentMut, TableLike, value};
    let parsed: ProjectConfig = toml::from_str(text)?;
    let name = parsed.name.clone();
    if !is_project_name(&name) {
        bail!("project name '{name}' may only contain letters, digits, '-' and '_'");
    }
    let mut doc: DocumentMut = text.parse()?;
    if let Some(dir) = doc.remove("template_dir")
        && let Some(dir) = dir.as_str().map(PathBuf::from)
        && machine.template_dir(&name).is_none()
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
    let mut without_repo = Vec::new();
    for table in repos {
        let Some(repo) = table.get("name").and_then(|n| n.as_str()).map(String::from) else {
            continue;
        };
        let source = table
            .remove("source")
            .and_then(|s| s.as_str().map(PathBuf::from));
        if let Some(source) = &source
            && machine.source(&name, &repo).is_none()
        {
            machine.set_source(&name, &repo, source.clone());
        }
        if table.get("repo").is_none() {
            match guess_repo(source.as_deref(), &repo, &parsed.match_rule.repos) {
                Some(nwo) => {
                    table.insert("repo", value(nwo));
                }
                None => without_repo.push(repo),
            }
        }
    }
    let out = doc.to_string();
    toml::from_str::<ProjectConfig>(&out).context("the imported project config")?;
    Ok(Imported {
        name,
        text: out,
        without_repo,
    })
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
    if !paths.has_repo() {
        return Ok(done);
    }
    for dir in [paths.legacy_projects_dir(), paths.legacy_memory_dir()] {
        if !dir.exists() {
            continue;
        }
        let backup = unused_sibling(&dir, "pre-repo");
        match rename_retrying(&dir, &backup, LEGACY_RENAME_RETRY) {
            Ok(()) => done.push(format!("moved {} to {}", dir.display(), backup.display())),
            Err(_) if !dir.exists() => {}
            Err(e) => {
                let name = |d: &Path| {
                    d.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                };
                return Err(e.context(format!(
                    "couldn't rename the old {}/ folder to {}; Tworky tries again on its next start",
                    name(&dir),
                    name(&backup)
                )));
            }
        }
    }
    Ok(done)
}

/// On Windows a scanner or indexer can briefly hold a file in the new repo open, which makes the
/// rename fail with a sharing or access error.
fn rename_retrying(from: &Path, to: &Path, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
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
    let committed = commit_paths(paths, &repo, &relatives, message, true)?;
    if let Some(file) = committed.held_back.first() {
        bail!("{} doesn't parse as staged", repo.join(file).display());
    }
    Ok(committed.committed)
}

/// [`commit`], but a failure is only logged and kept for [`recorded_failures`]: the file is
/// already written, and the next sweep commits it.
pub fn commit_logged(guard: &RepoLock, paths: &Paths, files: &[impl AsRef<Path>], message: &str) {
    let committed = commit(guard, paths, files, message);
    if let Err(e) = &committed {
        eprintln!("config repo: couldn't commit \"{message}\": {e:#}");
    }
    record(&COMMIT_FAILURES, paths, &committed);
}

struct Committed {
    committed: bool,
    /// Staged `project.toml` files that didn't parse, so were unstaged and not committed.
    held_back: Vec<String>,
}

/// Commits `relatives` (allowlisted, `/`-separated) as they are in the working tree, except a
/// `project.toml` whose staged content doesn't parse. That's checked after staging, because the
/// file can change between a parse check and `git add`. With `all_or_nothing`, such a file stops
/// the whole commit.
fn commit_paths(
    paths: &Paths,
    repo: &Path,
    relatives: &[String],
    message: &str,
    all_or_nothing: bool,
) -> Result<Committed> {
    let mut held_back = Vec::new();
    if relatives.is_empty() {
        return Ok(Committed {
            committed: false,
            held_back,
        });
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
    for relative in present
        .iter()
        .filter(|r| is_project_file(Path::new(r.as_str())))
    {
        let staged = git(
            paths,
            repo,
            &["show", &format!(":{relative}")],
            LOCAL_TIMEOUT,
        )?;
        if toml::from_str::<ProjectConfig>(&staged).is_err() {
            held_back.push(relative.to_string());
        }
    }
    if !held_back.is_empty() {
        let unstage: Vec<&String> = if all_or_nothing {
            relatives.iter().collect()
        } else {
            held_back.iter().collect()
        };
        run(&["reset", "-q", "--"], &unstage)?;
        if all_or_nothing {
            return Ok(Committed {
                committed: false,
                held_back,
            });
        }
    }
    let all: Vec<&String> = relatives
        .iter()
        .filter(|r| !held_back.contains(r))
        .collect();
    let out = if all.is_empty() {
        String::new()
    } else {
        run(
            &[
                "diff",
                "--cached",
                "--name-only",
                "--no-renames",
                "-z",
                "--",
            ],
            &all,
        )?
    };
    let staged: Vec<String> = out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    if staged.is_empty() {
        return Ok(Committed {
            committed: false,
            held_back,
        });
    }
    let staged: Vec<&String> = staged.iter().collect();
    let identity = identity_args(paths, repo);
    run(
        &[
            &identity[..],
            &["commit", "-q", "-m", message, "--only", "--"],
        ]
        .concat(),
        &staged,
    )?;
    Ok(Committed {
        committed: true,
        held_back,
    })
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
/// a `project.toml` that doesn't parse. A failure is kept for [`recorded_failures`].
pub fn sweep(_guard: &RepoLock, paths: &Paths) -> Result<Sweep> {
    let swept = sweep_unrecorded(paths);
    record(&COMMIT_FAILURES, paths, &swept);
    swept
}

fn sweep_unrecorded(paths: &Paths) -> Result<Sweep> {
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
    let committed = commit_paths(
        paths,
        &repo,
        &relatives,
        "Commit edits made outside Tworky",
        false,
    )?;
    held_back.extend(committed.held_back.iter().map(|r| repo.join(r)));
    Ok(Sweep {
        committed: committed.committed,
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
    /// An `index.lock` in `repo/` or `repo-sync/` older than any git command this module runs, so
    /// one a crash left behind.
    pub stale_index_lock: bool,
    pub sync: SyncRecord,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.changes.is_empty()
    }
}

pub fn status(paths: &Paths) -> Result<Status> {
    let repo = paths.repo_dir();
    let run = |args: &[&str]| git(paths, &repo, args, LOCAL_TIMEOUT);
    let changes = changes(paths, &repo)?;
    let ahead_behind =
        if run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).is_ok() {
            let counts = run(&["rev-list", "--left-right", "--count", "HEAD...@{u}"])?;
            let mut counts = counts.split_whitespace().map(str::parse::<u32>);
            match (counts.next(), counts.next()) {
                (Some(Ok(ahead)), Some(Ok(behind))) => Some((ahead, behind)),
                _ => bail!("unexpected `git rev-list --count` output: {counts:?}"),
            }
        } else {
            None
        };
    let mut index_locks = vec![repo.join(".git").join("index.lock")];
    let sync_dir = paths.repo_sync_dir();
    if sync_dir.join(".git").exists()
        && let Ok(lock) = git(
            paths,
            &sync_dir,
            &["rev-parse", "--git-path", "index.lock"],
            LOCAL_TIMEOUT,
        )
    {
        index_locks.push(sync_dir.join(lock.trim()));
    }
    let stale_index_lock = index_locks.iter().any(|lock| {
        std::fs::metadata(lock)
            .and_then(|m| m.modified())
            .is_ok_and(|modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .is_ok_and(|age| age > LOCAL_TIMEOUT)
            })
    });
    Ok(Status {
        changes,
        ahead_behind,
        stale_index_lock,
        sync: sync_status(paths)?,
    })
}

/// The config repo's sync with its remote, kept in `<data>/config-sync.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncRecord {
    pub state: SyncState,
    /// `main` when the stopped sync began integrating (`S`): the commits after it are this
    /// machine's.
    pub base: Option<String>,
    /// What the stopped sync puts this machine's commits on. A resolution in `repo-sync` contains
    /// it.
    pub upstream: Option<String>,
    /// The result a sync held back, so the next one can tell whether it was fixed since.
    pub result: Option<String>,
    /// What `repo-sync` stopped in.
    pub operation: Option<Operation>,
    /// Set by [`abort`]: background syncs are skipped until the user asks for one, which starts
    /// over.
    pub paused: bool,
    /// Unix seconds of the last sync that finished.
    pub last_sync: Option<u64>,
    /// What pulled changes touched, until dismissed.
    pub incoming: Incoming,
    /// Who could see the remote when it was set.
    pub visibility: Option<Visibility>,
    /// Where `origin` fetched from and pushed to when the user connected it. A sync refuses an
    /// `origin` that differs, such as one added with plain git.
    pub remote: Option<RemoteUrls>,
}

/// `origin`'s fetch and push URLs as git resolves them, redacted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteUrls {
    pub fetch: Vec<String>,
    pub push: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncState {
    #[default]
    Idle,
    Syncing,
    /// A rebase or merge in `repo-sync` stopped on these files, or its result still has conflict
    /// markers in them.
    Conflict {
        files: Vec<String>,
    },
    /// `project.toml` files from the remote that don't parse.
    HeldBack {
        files: Vec<String>,
        #[serde(default)]
        cause: HeldBackCause,
    },
    /// Uncommitted files in `repo/` that incoming changes would overwrite.
    BlockedByLocal {
        files: Vec<String>,
    },
    /// Uncommitted files in `repo-sync`, which a sync won't discard: committing them resumes, and
    /// an abort discards them.
    HeldInSyncWorktree {
        files: Vec<String>,
    },
    Error {
        message: String,
    },
    NoRemote,
    /// `origin` isn't the remote the user connected in Tworky, so nothing is fetched or pushed
    /// until they connect it in Settings › Remote or remove it. `url` is redacted.
    UnconfirmedRemote {
        url: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldBackCause {
    /// At least one isn't valid TOML.
    #[default]
    Invalid,
    /// Every one is valid TOML but not project config this build understands, as config written by
    /// a newer Tworky would be.
    Unrecognised,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Rebase,
    Merge,
}

/// What pulled changes touched that the user should look at, since synced config runs commands
/// and steers sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Incoming {
    /// Projects whose `[[hooks.*]]` or `[[services]]` changed.
    pub hooks_or_services: Vec<String>,
    /// Guidance and memory files that changed, relative to the repo.
    pub guidance: Vec<String>,
    /// Subjects of this machine's commits that a conflict resolution left out of the result.
    pub dropped: Vec<String>,
}

impl Incoming {
    pub fn is_empty(&self) -> bool {
        self.hooks_or_services.is_empty() && self.guidance.is_empty() && self.dropped.is_empty()
    }

    fn add(&mut self, other: Incoming) {
        for (mine, theirs) in [
            (&mut self.hooks_or_services, other.hooks_or_services),
            (&mut self.guidance, other.guidance),
            (&mut self.dropped, other.dropped),
        ] {
            mine.extend(theirs);
            mine.sort();
            mine.dedup();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The app's start, focus, timer or a commit. Skipped while an aborted sync is paused.
    Background,
    /// The user asked. Starts over after an abort.
    User,
}

fn read_record(paths: &Paths) -> SyncRecord {
    std::fs::read(paths.config_sync_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn update_record(paths: &Paths, change: impl FnOnce(&mut SyncRecord)) -> Result<SyncRecord> {
    let _lock = lock(&paths.config_sync_lock())?;
    let before = read_record(paths);
    let mut record = before.clone();
    change(&mut record);
    if record != before {
        crate::memory::write_atomic(
            &paths.config_sync_file(),
            serde_json::to_string_pretty(&record)?.as_bytes(),
        )?;
    }
    Ok(record)
}

/// The recorded sync state, or `no_remote` while the repo has no remote.
pub fn sync_status(paths: &Paths) -> Result<SyncRecord> {
    let mut record = read_record(paths);
    if !paths.has_repo() || remote_url(paths).is_none() {
        record.state = SyncState::NoRemote;
    } else if let Some(url) = unconfirmed_remote(paths, &record, &[paths.repo_dir()]) {
        record.state = SyncState::UnconfirmedRemote { url };
    }
    Ok(record)
}

fn remote_urls_in(paths: &Paths, dir: &Path) -> Option<RemoteUrls> {
    let read = |push: bool| {
        let mut args = vec!["remote", "get-url"];
        if push {
            args.push("--push");
        }
        args.extend(["--all", REMOTE]);
        git(paths, dir, &args, LOCAL_TIMEOUT).ok().map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(redact_url)
                .collect::<Vec<_>>()
        })
    };
    Some(RemoteUrls {
        fetch: read(false)?,
        push: read(true)?,
    })
}

/// A redacted URL `origin` fetches from or pushes to in one of `dirs` that the user didn't
/// connect, or `None` when `origin` is what [`record_remote`] recorded.
fn unconfirmed_remote(paths: &Paths, record: &SyncRecord, dirs: &[PathBuf]) -> Option<String> {
    let recorded = record.remote.clone().unwrap_or_default();
    for dir in dirs {
        let urls = remote_urls_in(paths, dir).unwrap_or_default();
        if record.remote.is_some() && urls == recorded {
            continue;
        }
        let known = |url: &&String| recorded.fetch.contains(url) || recorded.push.contains(url);
        return Some(
            urls.fetch
                .iter()
                .chain(&urls.push)
                .find(|url| !known(url))
                .or(urls.fetch.first())
                .cloned()
                .unwrap_or_default(),
        );
    }
    None
}

/// The dirs whose `origin` a sync uses: `repo/`, and `repo-sync` once it exists.
fn remote_dirs(paths: &Paths) -> Vec<PathBuf> {
    let mut dirs = vec![paths.repo_dir()];
    if is_worktree(paths, &paths.repo_sync_dir()) {
        dirs.push(paths.repo_sync_dir());
    }
    dirs
}

/// The URLs `origin` fetches from and pushes to in `dirs` once git applies `pushurl`, `insteadOf`
/// and `pushInsteadOf`. Refuses unless every one names the repo at `checked` ([`same_repo`]) and
/// every dir resolves alike, so git config the user never saw can't send the config elsewhere.
fn resolved_remote(paths: &Paths, dirs: &[PathBuf], checked: &str) -> Result<RemoteUrls> {
    let checked = redact_url(checked);
    let mut resolved: Option<RemoteUrls> = None;
    for dir in dirs {
        let urls = remote_urls_in(paths, dir).context("the config repo has no remote")?;
        let elsewhere = urls.fetch.is_empty()
            || urls.push.is_empty()
            || !urls
                .fetch
                .iter()
                .chain(&urls.push)
                .all(|url| same_repo(url, &checked));
        if elsewhere {
            bail!(
                "with your git config, the config repo would fetch from {} and push to {}, not only {checked}; check the remote's pushurl and any insteadOf or pushInsteadOf in your git config",
                urls.fetch.join(", "),
                urls.push.join(", ")
            );
        }
        if let Some(first) = &resolved
            && *first != urls
        {
            bail!(
                "the config sync's worktree {} fetches from {} and pushes to {}, unlike the config repo; remove the remote and connect it again",
                dir.display(),
                urls.fetch.join(", "),
                urls.push.join(", ")
            );
        }
        resolved = Some(urls);
    }
    resolved.context("the config repo has no remote")
}

/// Whether two redacted remote URLs name the same repo: the same GitHub `owner/name` over https,
/// `ssh://` or scp-like SSH, else the same text.
fn same_repo(a: &str, b: &str) -> bool {
    a == b || github_repo(a).is_some_and(|repo| github_repo(b) == Some(repo))
}

/// The lowercased `owner/name` of an https, `ssh://` or scp-like URL on github.com.
fn github_repo(url: &str) -> Option<(String, String)> {
    if let Some((scheme, _)) = url.split_once("://")
        && !scheme.eq_ignore_ascii_case("https")
        && !scheme.eq_ignore_ascii_case("ssh")
    {
        return None;
    }
    let remote = split_remote(url)?;
    if !remote.host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let (owner, name) = owner_and_name(remote.path)?;
    Some((owner.to_ascii_lowercase(), name.to_ascii_lowercase()))
}

/// Records `urls`, from [`resolved_remote`], as the remote the user connected, and who could see
/// it, so syncs use it. The caller holds the sync lock.
fn record_remote(paths: &Paths, urls: RemoteUrls, visibility: Visibility) -> Result<SyncRecord> {
    update_record(paths, |r| {
        r.remote = Some(urls);
        r.visibility = Some(visibility);
    })
}

pub fn remote_url(paths: &Paths) -> Option<String> {
    git(
        paths,
        &paths.repo_dir(),
        &["remote", "get-url", REMOTE],
        LOCAL_TIMEOUT,
    )
    .ok()
    .map(|url| url.trim().to_string())
}

pub fn dismiss_incoming(paths: &Paths) -> Result<()> {
    update_record(paths, |r| r.incoming = Incoming::default())?;
    Ok(())
}

/// Something that changes with every commit, for noticing commits without running git: `main`'s
/// loose ref, else the times the packed refs or the reftable last changed.
pub fn main_ref_stamp(paths: &Paths) -> Option<String> {
    let git_dir = paths.repo_dir().join(".git");
    if let Ok(loose) = std::fs::read_to_string(git_dir.join("refs/heads/main")) {
        return Some(loose);
    }
    let times: Vec<String> = ["packed-refs", "reftable", "reftable/tables.list"]
        .iter()
        .filter_map(|file| std::fs::metadata(git_dir.join(file)).ok())
        .map(|m| format!("{:?}/{}", m.modified().ok(), m.len()))
        .collect();
    (!times.is_empty()).then(|| times.join(" "))
}

/// Brings `repo/` and the remote together: fetches, rebases this machine's commits onto the
/// remote's in `repo-sync`, checks the result, moves `main` to it under the repo lock, and pushes.
/// A conflict, a result that doesn't check out or local files in the way stop it in a state the
/// next sync resumes from once fixed. Returns the recorded state, which holds any failure.
pub fn sync(paths: &Paths, trigger: Trigger) -> Result<SyncRecord> {
    sync_within(paths, trigger, NETWORK_TIMEOUT)
}

fn sync_within(paths: &Paths, trigger: Trigger, network: Duration) -> Result<SyncRecord> {
    if !paths.has_repo() {
        bail!(
            "the config repo {} isn't set up",
            paths.repo_dir().display()
        );
    }
    let _sync = with_sync_lock(paths)?;
    sync_locked(paths, trigger, network)
}

fn sync_locked(paths: &Paths, trigger: Trigger, network: Duration) -> Result<SyncRecord> {
    if remote_url(paths).is_none() {
        return update_record(paths, |r| {
            *r = SyncRecord {
                state: SyncState::NoRemote,
                last_sync: r.last_sync,
                incoming: std::mem::take(&mut r.incoming),
                ..Default::default()
            }
        });
    }
    let before = read_record(paths);
    if let Some(url) = unconfirmed_remote(paths, &before, &remote_dirs(paths)) {
        return update_record(paths, |r| r.state = SyncState::UnconfirmedRemote { url });
    }
    if trigger == Trigger::Background && before.paused {
        return Ok(before);
    }
    let step = match run_sync(paths, &before, network) {
        Ok(step) => step,
        Err(e) if e.is::<Held>() => {
            let held = e.downcast_ref::<Held>().expect("checked above");
            let record = if held.uncommitted.is_empty() {
                read_record(paths)
            } else {
                update_record(paths, |r| {
                    r.state = SyncState::HeldInSyncWorktree {
                        files: held.uncommitted.clone(),
                    }
                })?
            };
            return match trigger {
                Trigger::Background => Ok(record),
                Trigger::User => Err(e),
            };
        }
        Err(e) => Step::failed(&e),
    };
    update_record(paths, |r| step.apply(r))
}

#[derive(Default)]
struct Step {
    state: SyncState,
    base: Option<String>,
    upstream: Option<String>,
    result: Option<String>,
    operation: Option<Operation>,
    finished: bool,
    /// The push was rejected because the remote moved, so the sync starts over.
    rejected: bool,
    incoming: Incoming,
}

/// A sync that won't start over because `repo-sync` holds work starting over would discard. The
/// record keeps its base, so the work resumes once it's committed.
#[derive(Debug)]
struct Held {
    message: String,
    /// The uncommitted changes in `repo-sync`, if that's why.
    uncommitted: Vec<String>,
}

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Held {}

impl Step {
    fn failed(e: &anyhow::Error) -> Step {
        Step {
            state: SyncState::Error {
                message: failure_message(e),
            },
            ..Default::default()
        }
    }

    fn stopped(
        state: SyncState,
        base: &str,
        upstream: Option<&str>,
        result: Option<&str>,
        operation: Option<Operation>,
    ) -> Step {
        Step {
            state,
            base: Some(base.to_string()),
            upstream: upstream.map(String::from),
            result: result.map(String::from),
            operation,
            ..Default::default()
        }
    }

    fn apply(self, r: &mut SyncRecord) {
        r.state = self.state;
        r.base = self.base;
        r.upstream = self.upstream;
        r.result = self.result;
        r.operation = self.operation;
        r.paused = false;
        if self.finished {
            r.last_sync = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs());
        }
        r.incoming.add(self.incoming);
    }
}

fn run_sync(paths: &Paths, before: &SyncRecord, network: Duration) -> Result<Step> {
    let dir = sync_worktree(paths)?;
    let progress = progress(paths, &dir)?;
    if progress != Progress::None {
        let mut files = unmerged(paths, &dir)?;
        if files.is_empty()
            && let SyncState::Conflict { files: before } = &before.state
        {
            files = before.clone();
        }
        return Ok(Step {
            state: SyncState::Conflict { files },
            base: before.base.clone(),
            upstream: before.upstream.clone(),
            operation: progress.operation(),
            ..Default::default()
        });
    }
    let failed_keeping_base = |e: &anyhow::Error| Step {
        base: before.base.clone(),
        upstream: before.upstream.clone(),
        result: before.result.clone(),
        ..Step::failed(e)
    };
    let resume = match resumable(paths, &dir, before) {
        Ok(resume) => resume,
        Err(e) if e.is::<Held>() => return Err(e),
        Err(e) => return Ok(failed_keeping_base(&e)),
    };
    update_record(paths, |r| r.state = SyncState::Syncing)?;
    let mut step = match resume {
        Some((base, upstream)) => match integrate(paths, &dir, base, upstream, network) {
            Ok(step) => step,
            Err(e) => return Ok(failed_keeping_base(&e)),
        },
        None => start(paths, &dir, before.paused, network)?,
    };
    let mut incoming = Incoming::default();
    for _ in 0..2 {
        if !step.rejected {
            break;
        }
        incoming.add(std::mem::take(&mut step.incoming));
        step = start(paths, &dir, false, network).unwrap_or_else(|e| Step::failed(&e));
    }
    step.incoming.add(incoming);
    Ok(step)
}

/// The base and upstream to resume a stopped sync from, once the user has fixed it in `repo-sync`:
/// a conflict whose rebase or merge has finished, a held-back result with a commit on top, a
/// result blocked by local files, or a resumed sync that failed before moving `main`. `None` starts
/// over, as for a held-back result nothing was committed on, which the remote may have fixed. Fails
/// with [`Held`] while starting over would throw away work in `repo-sync`: uncommitted changes, or
/// commits that can't be resumed.
fn resumable<'a>(
    paths: &Paths,
    dir: &Path,
    before: &'a SyncRecord,
) -> Result<Option<(&'a str, Option<&'a str>)>> {
    if before.paused {
        return Ok(None);
    }
    if let Some(held) = uncommitted_in(paths, dir)? {
        return Err(held.into());
    }
    let Some(base) = before.base.as_deref() else {
        return Ok(None);
    };
    let recorded = [
        Some(base),
        before.upstream.as_deref(),
        before.result.as_deref(),
    ];
    if let Some(missing) = recorded
        .into_iter()
        .flatten()
        .find(|rev| rev_parse(paths, dir, rev).is_err())
    {
        eprintln!(
            "the stopped config sync names {missing}, which the config repo doesn't have; starting over"
        );
        return Ok(None);
    }
    let head = rev_parse(paths, dir, "HEAD")?;
    let main = rev_parse(paths, &paths.repo_dir(), "refs/heads/main")?;
    if is_ancestor(paths, dir, &head, &main)? {
        return Ok(None);
    }
    let upstream = before.upstream.as_deref();
    let fixed = match &before.state {
        SyncState::Conflict { .. }
        | SyncState::Error { .. }
        | SyncState::BlockedByLocal { .. }
        | SyncState::Syncing => true,
        SyncState::HeldBack { .. } | SyncState::HeldInSyncWorktree { .. } => {
            if before.result.as_deref() == Some(head.as_str()) {
                return Ok(None);
            }
            true
        }
        SyncState::Idle | SyncState::NoRemote | SyncState::UnconfirmedRemote { .. } => false,
    };
    let builds_on_both = is_ancestor(paths, dir, base, &main)?
        && match upstream {
            Some(upstream) => is_ancestor(paths, dir, upstream, &head)?,
            None => true,
        };
    if fixed && builds_on_both {
        return Ok(Some((base, upstream)));
    }
    let worktree = dir.display();
    let abort = "or run `tw config abort` and sync again to start over";
    let message = match &before.state {
        SyncState::Conflict { files } => format!(
            "the config sync stopped on a conflict in {}; resolve it and commit in {worktree}, {abort}",
            files.join(", ")
        ),
        _ => format!(
            "the stopped config sync's result in {worktree} doesn't build on both the remote's config and this machine's; fix it there, {abort}"
        ),
    };
    Err(Held {
        message,
        uncommitted: Vec::new(),
    }
    .into())
}

/// Why `repo-sync` can't be reset: the uncommitted changes in it.
fn uncommitted_in(paths: &Paths, dir: &Path) -> Result<Option<Held>> {
    let files = changes(paths, dir)?;
    Ok((!files.is_empty()).then(|| Held {
        message: format!(
            "the config sync's worktree {} has uncommitted changes to {}; commit them to carry on, or run `tw config abort` and sync again to discard them",
            dir.display(),
            files.join(", ")
        ),
        uncommitted: files,
    }))
}

/// Steps 1 and 2: fetches, then rebases `main`'s commits onto the remote's in `repo-sync`, or
/// merges where the two histories haven't joined on both sides. Refuses to discard uncommitted
/// changes there unless `discard`.
fn start(paths: &Paths, dir: &Path, discard: bool, network: Duration) -> Result<Step> {
    if !discard && let Some(held) = uncommitted_in(paths, dir)? {
        bail!(held.message);
    }
    git(paths, dir, &["fetch", "-q", "--prune", REMOTE], network)?;
    let base = rev_parse(paths, &paths.repo_dir(), "refs/heads/main")?;
    let upstream = remote_main(paths, dir)?;
    let run = |args: &[&str]| git(paths, dir, args, LOCAL_TIMEOUT);
    run(&["checkout", "-q", "-f", "--detach", &base])?;
    run(&["clean", "-fdq"])?;
    if let Some(upstream) = &upstream
        && !is_ancestor(paths, dir, upstream, &base)?
    {
        let ours = roots(paths, dir, &base)?;
        let theirs = roots(paths, dir, upstream)?;
        let shared = ours.iter().any(|root| theirs.contains(root));
        let unpushed_join = shared && ours.iter().any(|root| !theirs.contains(root));
        let integrated = if unpushed_join {
            // `main` holds a join the remote hasn't taken yet. Rebasing would make the join's
            // merge again and raise every clash it settled, so the remote's commits are merged in.
            git_committing(
                paths,
                dir,
                &["merge", "-q", "--no-edit", "-m", MERGE_MESSAGE, upstream],
            )
            .map(drop)
        } else if shared {
            git_committing(paths, dir, &["rebase", "-q", "--rebase-merges", upstream]).map(drop)
        } else {
            match git_committing(
                paths,
                dir,
                &[
                    "merge",
                    "-q",
                    "--no-edit",
                    "--allow-unrelated-histories",
                    "-m",
                    JOIN_MESSAGE,
                    upstream,
                ],
            ) {
                Ok(_) => Ok(()),
                Err(_) if settle_empty_sides(paths, dir)? => Ok(()),
                Err(e) => Err(e),
            }
        };
        if let Err(e) = integrated {
            return stopped_in(paths, dir, e, &base, upstream);
        }
    }
    integrate(paths, dir, &base, upstream.as_deref(), network)
}

/// Settles the clashes a join's merge stopped on where one side's file is empty, keeping the other
/// side's: every config repo starts with empty guidance files, and two unrelated histories have no
/// common version to tell an untouched file from an emptied one. Commits the merge once nothing
/// else clashes, and returns whether it did.
fn settle_empty_sides(paths: &Paths, dir: &Path) -> Result<bool> {
    if progress(paths, dir)? != Progress::Merge {
        return Ok(false);
    }
    let run = |args: &[&str]| git(paths, dir, args, LOCAL_TIMEOUT);
    for file in unmerged(paths, dir)? {
        let side = |stage: u8| run(&["show", &format!(":{stage}:{file}")]).ok();
        let keep = match (side(2), side(3)) {
            (Some(ours), Some(_)) if ours.trim().is_empty() => "--theirs",
            (Some(_), Some(theirs)) if theirs.trim().is_empty() => "--ours",
            _ => continue,
        };
        let spec = pathspec(&file);
        run(&["checkout", keep, "--", &spec])?;
        run(&["add", "--", &spec])?;
    }
    if !unmerged(paths, dir)?.is_empty() {
        return Ok(false);
    }
    git_committing(paths, dir, &["commit", "-q", "--no-edit"])?;
    Ok(true)
}

/// The first commits `rev` grew from. Two machines' config repos that each started on their own
/// share none until one joins the other's history, and a join has both.
fn roots(paths: &Paths, dir: &Path, rev: &str) -> Result<Vec<String>> {
    Ok(git(
        paths,
        dir,
        &["rev-list", "--max-parents=0", rev],
        LOCAL_TIMEOUT,
    )?
    .lines()
    .map(String::from)
    .collect())
}

/// A rebase or merge in `repo-sync` failed: a conflict if it stopped, else the failure.
fn stopped_in(
    paths: &Paths,
    dir: &Path,
    failure: anyhow::Error,
    base: &str,
    upstream: &str,
) -> Result<Step> {
    let progress = progress(paths, dir)?;
    if progress == Progress::None {
        return Err(failure);
    }
    Ok(Step::stopped(
        SyncState::Conflict {
            files: unmerged(paths, dir)?,
        },
        base,
        Some(upstream),
        None,
        progress.operation(),
    ))
}

/// Steps 3 and 4: checks `repo-sync`'s `HEAD`, which holds `base`'s commits on `upstream`, moves
/// `main` to it (replaying any commits made since `base` first), then pushes it.
fn integrate(
    paths: &Paths,
    dir: &Path,
    base: &str,
    upstream: Option<&str>,
    network: Duration,
) -> Result<Step> {
    let result = rev_parse(paths, dir, "HEAD")?;
    if let Some(state) = check(paths, dir, base, &result)? {
        return Ok(Step::stopped(state, base, upstream, Some(&result), None));
    }
    let repo = paths.repo_dir();
    let guard = with_lock(paths)?;
    sweep(&guard, paths)?;
    let main = rev_parse(paths, &repo, "refs/heads/main")?;
    let result = if main == base {
        result
    } else {
        if let Err(e) = git_committing(
            paths,
            dir,
            &[
                "rebase",
                "-q",
                "--rebase-merges",
                "--onto",
                &result,
                base,
                &main,
            ],
        ) {
            return stopped_in(paths, dir, e, &main, &result);
        }
        let replayed = rev_parse(paths, dir, "HEAD")?;
        if let Some(state) = check(paths, dir, &main, &replayed)? {
            return Ok(Step::stopped(
                state,
                &main,
                Some(&result),
                Some(&replayed),
                None,
            ));
        }
        replayed
    };
    let incoming_files = diff_names(paths, &repo, &main, &result)?;
    let blocked: Vec<String> = changes(paths, &repo)?
        .into_iter()
        .filter(|f| incoming_files.contains(f))
        .collect();
    if !blocked.is_empty() {
        return Ok(Step::stopped(
            SyncState::BlockedByLocal { files: blocked },
            &main,
            upstream,
            None,
            None,
        ));
    }
    let dropped = match upstream {
        Some(upstream) => dropped_commits(paths, dir, upstream, &main, &result)?,
        None => Vec::new(),
    };
    git(
        paths,
        &repo,
        &["reset", "-q", "--keep", &result],
        LOCAL_TIMEOUT,
    )?;
    let mut incoming = summarize(paths, &repo, &main, &result, &incoming_files);
    incoming.dropped = dropped;
    drop(guard);
    let pushed = if upstream == Some(result.as_str()) {
        Ok(())
    } else {
        push(paths, dir, &result, network)
    };
    Ok(match pushed {
        Ok(()) => Step {
            finished: true,
            incoming,
            ..Default::default()
        },
        Err(e) => Step {
            rejected: e.is::<PushRejected>(),
            incoming,
            ..Step::failed(&e)
        },
    })
}

/// Subjects of this machine's commits (those on `main` since `upstream`) that `result` has no copy
/// of. A copy has the same changes, or, for a commit whose conflict was resolved, the same author,
/// author date and subject.
fn dropped_commits(
    paths: &Paths,
    dir: &Path,
    upstream: &str,
    main: &str,
    result: &str,
) -> Result<Vec<String>> {
    let log = |args: &[&str]| -> Result<Vec<String>> {
        let out = git(
            paths,
            dir,
            &[
                &["log", "--no-merges", "--format=%an%x1f%ae%x1f%at%x1f%s"],
                args,
            ]
            .concat(),
            LOCAL_TIMEOUT,
        )?;
        Ok(out.lines().map(String::from).collect())
    };
    let kept = log(&[&format!("{upstream}..{result}")])?;
    Ok(log(&[
        "--cherry-pick",
        "--right-only",
        &format!("{result}...{main}"),
    ])?
    .into_iter()
    .filter(|commit| !kept.contains(commit))
    .filter_map(|commit| commit.splitn(4, '\x1f').nth(3).map(String::from))
    .collect())
}

/// Why `result` can't become `main`: unmerged files in `repo-sync`, conflict markers it adds to
/// config files since `base`, or a `project.toml` it changes that doesn't parse.
fn check(paths: &Paths, dir: &Path, base: &str, result: &str) -> Result<Option<SyncState>> {
    let files = unmerged(paths, dir)?;
    if !files.is_empty() {
        return Ok(Some(SyncState::Conflict { files }));
    }
    let changed = git(
        paths,
        dir,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=d",
            "-z",
            base,
            result,
        ],
        LOCAL_TIMEOUT,
    )?;
    let show = |commit: &str, file: &str| {
        git(
            paths,
            dir,
            &["show", &format!("{commit}:{file}")],
            LOCAL_TIMEOUT,
        )
    };
    let markers = |text: &str| text.lines().filter(|l| is_conflict_marker(l)).count();
    let mut marked = Vec::new();
    let mut projects = Vec::new();
    for file in changed.split('\0').filter(|f| is_allowlisted(f)) {
        let text = show(result, file)?;
        let added = markers(&text);
        if added > 0 && added > show(base, file).map_or(0, |old| markers(&old)) {
            marked.push(file.to_string());
        }
        if is_project_file(Path::new(file)) {
            projects.push((file, text));
        }
    }
    if !marked.is_empty() {
        return Ok(Some(SyncState::Conflict { files: marked }));
    }
    let mut broken = Vec::new();
    let mut cause = HeldBackCause::Unrecognised;
    for (file, text) in projects {
        if toml::from_str::<ProjectConfig>(&text).is_err() {
            broken.push(file.to_string());
            if toml::from_str::<toml::Table>(&text).is_err() {
                cause = HeldBackCause::Invalid;
            }
        }
    }
    Ok((!broken.is_empty()).then_some(SyncState::HeldBack {
        files: broken,
        cause,
    }))
}

fn is_conflict_marker(line: &str) -> bool {
    ["<<<<<<<", ">>>>>>>"].iter().any(|marker| {
        line.strip_prefix(marker)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

/// Which projects' commands and which guidance and memory files differ between `old` and `new`.
fn summarize(paths: &Paths, dir: &Path, old: &str, new: &str, files: &[String]) -> Incoming {
    let mut incoming = Incoming::default();
    for file in files {
        match file.split('/').collect::<Vec<_>>().as_slice() {
            ["projects", project, "project.toml"] => {
                let commands = |commit: &str| {
                    let project = git(
                        paths,
                        dir,
                        &["show", &format!("{commit}:{file}")],
                        LOCAL_TIMEOUT,
                    )
                    .ok()
                    .and_then(|text| toml::from_str::<ProjectConfig>(&text).ok());
                    project.map(|p| {
                        (
                            serde_json::to_value(&p.hooks).ok(),
                            serde_json::to_value(&p.services).ok(),
                        )
                    })
                };
                if commands(old) != commands(new) {
                    incoming.hooks_or_services.push(project.to_string());
                }
            }
            ["tworky", _] | ["projects", _, _] if is_allowlisted(file) => {
                incoming.guidance.push(file.clone())
            }
            _ => {}
        }
    }
    incoming
}

/// Runs git in `repo-sync` for a command that may write commits: with Tworky's identity where the
/// user has none, and with the user's rebase settings that would move other branches or stash
/// turned off.
fn git_committing(paths: &Paths, dir: &Path, args: &[&str]) -> Result<String> {
    let identity = identity_args(paths, dir);
    let settings = [
        "-c",
        "rebase.updateRefs=false",
        "-c",
        "rebase.autoStash=false",
    ];
    git(
        paths,
        dir,
        &[&identity[..], &settings, args].concat(),
        LOCAL_TIMEOUT,
    )
}

#[derive(Debug)]
struct PushRejected(String);

impl std::fmt::Display for PushRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the remote changed while syncing: {}", self.0)
    }
}

impl std::error::Error for PushRejected {}

fn push(paths: &Paths, dir: &Path, result: &str, network: Duration) -> Result<()> {
    match git(
        paths,
        dir,
        &["push", "-q", REMOTE, &format!("{result}:refs/heads/main")],
        network,
    ) {
        Ok(_) => Ok(()),
        Err(e) => {
            let message = format!("{e:#}");
            let rejected = [
                "[rejected]",
                "non-fast-forward",
                "fetch first",
                "Updates were rejected",
            ];
            if rejected.iter().any(|r| message.contains(r)) {
                Err(PushRejected(message).into())
            } else {
                Err(e)
            }
        }
    }
}

fn failure_message(e: &anyhow::Error) -> String {
    let message = format!("{e:#}");
    let auth = [
        "Authentication failed",
        "could not read Username",
        "could not read Password",
        "terminal prompts disabled",
        "Permission denied (publickey",
        "returned error: 403",
        "Repository not found",
    ];
    if auth.iter().any(|a| message.contains(a)) {
        format!(
            "{message}\ngit couldn't sign in to the remote; for a GitHub remote, run `gh auth setup-git` so git uses gh's login"
        )
    } else {
        message
    }
}

/// `repo-sync`, a detached worktree of `repo/`, created when missing. The caller holds the sync
/// lock.
fn sync_worktree(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.repo_sync_dir();
    if is_worktree(paths, &dir) {
        return Ok(dir);
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    let repo = paths.repo_dir();
    git(paths, &repo, &["worktree", "prune"], LOCAL_TIMEOUT)?;
    git(
        paths,
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &forward_slashes(&dir),
            "main",
        ],
        LOCAL_TIMEOUT,
    )?;
    Ok(dir)
}

fn is_worktree(paths: &Paths, dir: &Path) -> bool {
    dir.join(".git").exists() && git(paths, dir, &["rev-parse", "--git-dir"], LOCAL_TIMEOUT).is_ok()
}

/// [`sync_worktree`] for the settings session, taking the sync lock only when it has to create it.
pub fn sync_worktree_dir(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.repo_sync_dir();
    if is_worktree(paths, &dir) {
        return Ok(dir);
    }
    let _sync = with_sync_lock(paths)?;
    sync_worktree(paths)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    None,
    Rebase,
    Merge,
    /// Unmerged entries with no rebase or merge, such as from a cherry-pick.
    Unmerged,
}

impl Progress {
    fn operation(self) -> Option<Operation> {
        match self {
            Progress::Rebase => Some(Operation::Rebase),
            Progress::Merge => Some(Operation::Merge),
            Progress::None | Progress::Unmerged => None,
        }
    }
}

fn progress(paths: &Paths, dir: &Path) -> Result<Progress> {
    let out = git(
        paths,
        dir,
        &[
            "rev-parse",
            "--git-path",
            "rebase-merge",
            "--git-path",
            "rebase-apply",
            "--git-path",
            "MERGE_HEAD",
        ],
        LOCAL_TIMEOUT,
    )?;
    let exists: Vec<bool> = out.lines().map(|l| dir.join(l.trim()).exists()).collect();
    Ok(match exists.as_slice() {
        [true, _, _] | [_, true, _] => Progress::Rebase,
        [_, _, true] => Progress::Merge,
        _ if !unmerged(paths, dir)?.is_empty() => Progress::Unmerged,
        _ => Progress::None,
    })
}

fn unmerged(paths: &Paths, dir: &Path) -> Result<Vec<String>> {
    let out = git(paths, dir, &["ls-files", "--unmerged", "-z"], LOCAL_TIMEOUT)?;
    let mut files: Vec<String> = out
        .split('\0')
        .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path.to_string()))
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

fn rev_parse(paths: &Paths, dir: &Path, rev: &str) -> Result<String> {
    Ok(git(
        paths,
        dir,
        &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")],
        LOCAL_TIMEOUT,
    )
    .with_context(|| format!("resolving {rev}"))?
    .trim()
    .to_string())
}

/// The fetched `main` of the remote, or `None` while the remote is empty.
fn remote_main(paths: &Paths, dir: &Path) -> Result<Option<String>> {
    let out = git(
        paths,
        dir,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/remotes/origin/main",
        ],
        LOCAL_TIMEOUT,
    )?;
    Ok(Some(out.trim().to_string()).filter(|sha| !sha.is_empty()))
}

/// Whether `ancestor` is `descendant` or one of its ancestors.
fn is_ancestor(paths: &Paths, dir: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let missing = git(
        paths,
        dir,
        &["rev-list", "-n", "1", &format!("{descendant}..{ancestor}")],
        LOCAL_TIMEOUT,
    )?;
    Ok(missing.trim().is_empty())
}

fn diff_names(paths: &Paths, dir: &Path, old: &str, new: &str) -> Result<Vec<String>> {
    let out = git(
        paths,
        dir,
        &["diff", "--name-only", "--no-renames", "-z", old, new],
        LOCAL_TIMEOUT,
    )?;
    Ok(out
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(String::from)
        .collect())
}

/// Stops the rebase or merge `repo-sync` is in, and pauses syncing until the user asks for a sync,
/// which starts over and discards whatever else is left uncommitted in `repo-sync`.
pub fn abort(paths: &Paths) -> Result<SyncRecord> {
    let _sync = with_sync_lock(paths)?;
    abort_in_progress(paths)?;
    update_record(paths, |r| {
        if !matches!(
            r.state,
            SyncState::NoRemote | SyncState::UnconfirmedRemote { .. }
        ) {
            r.state = SyncState::Idle;
        }
        r.operation = None;
        r.paused = true;
    })
}

fn abort_in_progress(paths: &Paths) -> Result<()> {
    let dir = paths.repo_sync_dir();
    if !is_worktree(paths, &dir) {
        return Ok(());
    }
    let run = |args: &[&str]| git(paths, &dir, args, LOCAL_TIMEOUT);
    match progress(paths, &dir)? {
        Progress::None => {}
        Progress::Rebase => {
            run(&["rebase", "--abort"])?;
        }
        Progress::Merge => {
            run(&["merge", "--abort"])?;
        }
        Progress::Unmerged => {
            run(&["reset", "-q", "--hard"])?;
        }
    }
    Ok(())
}

/// Makes `url` the repo's remote and syncs with it. A repo holding only its first commit takes the
/// remote's history when the remote has any. A repo with history of its own joins the remote's: the
/// sync merges the two in `repo-sync`, and a conflict stops it there like any other. A failure
/// removes the remote again, unless the repo already took the remote's history: then the remote
/// stays and the sync record shows why it stopped.
fn set_remote(paths: &Paths, url: &str, visibility: Visibility) -> Result<SyncRecord> {
    set_remote_within(paths, url, visibility, NETWORK_TIMEOUT)
}

fn set_remote_within(
    paths: &Paths,
    url: &str,
    visibility: Visibility,
    network: Duration,
) -> Result<SyncRecord> {
    let url = url.trim();
    if url.is_empty() {
        bail!("no remote URL given");
    }
    refuse_release_remote(remote_repo_name(url))?;
    if !paths.has_repo() {
        bail!(
            "the config repo {} isn't set up",
            paths.repo_dir().display()
        );
    }
    let _sync = with_sync_lock(paths)?;
    if let Some(current) = remote_url(paths) {
        bail!(
            "the config repo already syncs with {}; remove it first with `tw config remote rm`",
            redact_url(&current)
        );
    }
    let repo = paths.repo_dir();
    git(paths, &repo, &["remote", "add", REMOTE, url], LOCAL_TIMEOUT)?;
    let recorded = resolved_remote(paths, &remote_dirs(paths), url)
        .and_then(|urls| record_remote(paths, urls, visibility));
    if let Err(e) = recorded {
        forget_remote(paths);
        return Err(e);
    }
    let taken = match take_remote_history(paths, network) {
        Ok(taken) => taken,
        Err(e) => {
            forget_remote(paths);
            bail!("{}", failure_message(&e));
        }
    };
    if let Err(e) = sync_locked(paths, Trigger::User, network) {
        if taken.is_none() {
            forget_remote(paths);
            return Err(e);
        }
        update_record(paths, |r| {
            if !matches!(r.state, SyncState::HeldInSyncWorktree { .. }) {
                r.state = SyncState::Error {
                    message: failure_message(&e),
                };
            }
        })?;
    }
    update_record(paths, |r| r.incoming.add(taken.unwrap_or_default()))
}

/// Removes the remote and what [`set_remote`] configured for it, and resets the sync record, so
/// the repo is as it was before the remote was set. Best effort: it runs after another failure.
fn forget_remote(paths: &Paths) {
    let repo = paths.repo_dir();
    let run = |args: &[&str]| git(paths, &repo, args, LOCAL_TIMEOUT);
    let _ = run(&["remote", "remove", REMOTE]);
    for key in ["branch.main.remote", "branch.main.merge"] {
        let _ = run(&["config", "--unset-all", key]);
    }
    let _ = clear_worktree_origins(paths);
    let _ = update_record(paths, reset_to_no_remote);
}

/// Removes any `origin` set in a worktree's own config (`extensions.worktreeConfig`) of `repo/` or
/// `repo-sync`, which `git remote remove` leaves behind and the sync's check would still see.
fn clear_worktree_origins(paths: &Paths) -> Result<()> {
    let repo = paths.repo_dir();
    let enabled = git(
        paths,
        &repo,
        &["config", "--bool", "extensions.worktreeConfig"],
        LOCAL_TIMEOUT,
    )
    .is_ok_and(|out| out.trim() == "true");
    if !enabled {
        return Ok(());
    }
    for dir in remote_dirs(paths) {
        let run = |args: &[&str]| git(paths, &dir, args, LOCAL_TIMEOUT);
        let pattern = format!("^remote\\.{REMOTE}\\.");
        if run(&["config", "--worktree", "--get-regexp", &pattern]).is_ok() {
            run(&[
                "config",
                "--worktree",
                "--remove-section",
                &format!("remote.{REMOTE}"),
            ])?;
        }
    }
    Ok(())
}

fn reset_to_no_remote(r: &mut SyncRecord) {
    *r = SyncRecord {
        state: SyncState::NoRemote,
        last_sync: r.last_sync,
        incoming: std::mem::take(&mut r.incoming),
        ..Default::default()
    }
}

/// Takes the remote's history into a repo that has only its first commit, returning what came in.
/// `None`, having taken nothing, while the remote is empty, or when the repo has history of its
/// own, which the sync joins to the remote's.
fn take_remote_history(paths: &Paths, network: Duration) -> Result<Option<Incoming>> {
    let repo = paths.repo_dir();
    let run = |args: &[&str]| git(paths, &repo, args, LOCAL_TIMEOUT);
    run(&["config", "branch.main.remote", REMOTE])?;
    run(&["config", "branch.main.merge", "refs/heads/main"])?;
    git(paths, &repo, &["fetch", "-q", "--prune", REMOTE], network)?;
    let Some(upstream) = remote_main(paths, &repo)? else {
        return Ok(None);
    };
    let guard = with_lock(paths)?;
    sweep(&guard, paths)?;
    let main = rev_parse(paths, &repo, "refs/heads/main")?;
    if !is_init_only(paths)? {
        return Ok(None);
    }
    let local = changes(paths, &repo)?;
    if !local.is_empty() {
        bail!(
            "the config repo has uncommitted changes ({}); fix or remove them first",
            local.join(", ")
        );
    }
    run(&["reset", "-q", "--keep", &upstream])?;
    let files = diff_names(paths, &repo, &main, &upstream)?;
    Ok(Some(summarize(paths, &repo, &main, &upstream, &files)))
}

/// Whether `main` is still only the first commit [`init`] made.
fn is_init_only(paths: &Paths) -> Result<bool> {
    let repo = paths.repo_dir();
    let run = |args: &[&str]| git(paths, &repo, args, LOCAL_TIMEOUT);
    Ok(run(&["rev-list", "--count", "main"])?.trim() == "1"
        && run(&["log", "-1", "--format=%s", "main"])?.trim() == INIT_MESSAGE)
}

/// Refuses, in a debug build, the repo name the installed Tworky's remote has, so a dev home never
/// syncs with the user's real config.
fn refuse_release_remote(name: &str) -> Result<()> {
    if cfg!(debug_assertions) && name.eq_ignore_ascii_case(RELEASE_REMOTE_NAME) {
        bail!(
            "a debug build doesn't sync with a repo named {RELEASE_REMOTE_NAME}, which the installed Tworky uses; pick another, such as {DEV_REMOTE_NAME}"
        );
    }
    Ok(())
}

/// The repo's name in a remote URL or path: `tworky-config` for
/// `git@github.com:me/tworky-config.git`.
fn remote_repo_name(url: &str) -> &str {
    let url = url.trim_end_matches(['/', '\\']);
    without_git_suffix(url.rsplit(['/', ':', '\\']).next().unwrap_or(url))
}

/// `name` without one trailing `.git`, in any case.
fn without_git_suffix(name: &str) -> &str {
    match name.len().checked_sub(4) {
        Some(at) if name.is_char_boundary(at) && name[at..].eq_ignore_ascii_case(".git") => {
            &name[..at]
        }
        _ => name,
    }
}

/// Stops syncing: aborts anything in progress in `repo-sync`, clears it and removes the remote.
/// Refuses while `repo-sync` holds changes the user hasn't committed or given up with [`abort`].
pub fn remove_remote(paths: &Paths) -> Result<SyncRecord> {
    if !paths.has_repo() {
        bail!(
            "the config repo {} isn't set up",
            paths.repo_dir().display()
        );
    }
    let _sync = with_sync_lock(paths)?;
    if remote_url(paths).is_none() {
        bail!("the config repo has no remote");
    }
    let dir = paths.repo_sync_dir();
    if is_worktree(paths, &dir) {
        let files = changes(paths, &dir)?;
        if !files.is_empty() && !read_record(paths).paused {
            bail!(
                "the config sync's worktree {} has changes to {} that removing the remote would discard; commit them there, or run `tw config abort` to give them up, then remove the remote",
                dir.display(),
                files.join(", ")
            );
        }
        abort_in_progress(paths)?;
        let run = |args: &[&str]| git(paths, &dir, args, LOCAL_TIMEOUT);
        run(&["reset", "-q", "--hard"])?;
        run(&["clean", "-fdq"])?;
    }
    git(
        paths,
        &paths.repo_dir(),
        &["remote", "remove", REMOTE],
        LOCAL_TIMEOUT,
    )?;
    clear_worktree_origins(paths)?;
    update_record(paths, reset_to_no_remote)
}

/// What Tworky asks GitHub, and the user's SSH config, about remotes. [`Gh`] asks `gh` and `ssh`;
/// tests answer themselves.
pub trait GitHub: Sync {
    /// The user's login, then their organisations'.
    fn owners(&self) -> Result<Vec<String>>;
    /// `owner/name`, or `None` when it doesn't exist or the user can't see it.
    fn repo(&self, name_with_owner: &str) -> Result<Option<RepoView>>;
    fn create_private(&self, name_with_owner: &str) -> Result<()>;
    /// The host name an SSH host alias resolves to in the user's SSH config (`ssh -G`), connecting
    /// as `user` on `port` when the URL names them, as git would.
    fn ssh_hostname(&self, host: &str, user: Option<&str>, port: Option<&str>) -> Result<String>;
}

pub struct Gh;

const SSH_CONFIG_TIMEOUT: Duration = Duration::from_secs(5);

impl GitHub for Gh {
    fn owners(&self) -> Result<Vec<String>> {
        github::viewer_owners()
    }

    fn repo(&self, name_with_owner: &str) -> Result<Option<RepoView>> {
        github::repo_view(name_with_owner)
    }

    fn create_private(&self, name_with_owner: &str) -> Result<()> {
        github::create_private_repo(name_with_owner)
    }

    fn ssh_hostname(&self, host: &str, user: Option<&str>, port: Option<&str>) -> Result<String> {
        let mut cmd = process::command("ssh");
        cmd.args(ssh_config_args(host, user, port));
        let out = process::output_timeout(cmd, &format!("ssh -G {host}"), SSH_CONFIG_TIMEOUT)?;
        out.lines()
            .find_map(|line| line.strip_prefix("hostname "))
            .map(|name| name.trim().to_string())
            .with_context(|| format!("`ssh -G {host}` printed no hostname"))
    }
}

/// The arguments to `ssh -G` for `host`, as `user` on `port` when given.
fn ssh_config_args<'a>(
    host: &'a str,
    user: Option<&'a str>,
    port: Option<&'a str>,
) -> Vec<&'a str> {
    let mut args = vec!["-G"];
    if let Some(user) = user {
        args.extend(["-l", user]);
    }
    if let Some(port) = port {
        args.extend(["-p", port]);
    }
    args.extend(["--", host]);
    args
}

/// The name of the repo [`create_remote`] makes and [`find_shared_configs`] looks for:
/// `tworky-config`, or `tworky-config-dev` in a debug build.
pub fn default_remote_name() -> &'static str {
    if cfg!(debug_assertions) {
        DEV_REMOTE_NAME
    } else {
        RELEASE_REMOTE_NAME
    }
}

/// What the user should know about a remote before their config syncs with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteCheck {
    /// The URL git uses: an `owner/name` given as the remote becomes its GitHub URL. Shown with
    /// any credentials in it redacted.
    #[serde(serialize_with = "serialize_redacted")]
    pub url: String,
    pub visibility: Visibility,
    pub warnings: Vec<String>,
    /// The repo is public, or Tworky can't tell who can see it, so it's used only once the user
    /// confirms.
    pub needs_confirmation: bool,
    /// The repo's owner isn't the user, which one of the warnings says.
    pub foreign_owner: bool,
}

/// What the user was shown of a [`RemoteCheck`] when they chose to connect the remote. Connecting
/// refuses when the check made then is of another URL, or finds the remote riskier than this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// The checked URL, redacted.
    pub url: String,
    pub visibility: Visibility,
    #[serde(default)]
    pub foreign_owner: bool,
}

impl Seen {
    pub fn of(check: &RemoteCheck) -> Seen {
        Seen {
            url: redact_url(&check.url),
            visibility: check.visibility,
            foreign_owner: check.foreign_owner,
        }
    }
}

/// A connect refused because the remote's check found more to warn about than the user saw. It
/// holds the new check to show them.
#[derive(Debug)]
pub struct CheckChanged(pub RemoteCheck);

impl std::fmt::Display for CheckChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} changed since you confirmed it: {}; check it again",
            redact_url(&self.0.url),
            self.0.warnings.join("; ")
        )
    }
}

impl std::error::Error for CheckChanged {}

fn serialize_redacted<S: serde::Serializer>(url: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&redact_url(url))
}

/// Who can see `remote` (a URL, path or GitHub `owner/name`), and whether its owner is someone else.
pub fn check_remote(gh: &dyn GitHub, remote: &str) -> Result<RemoteCheck> {
    let url = remote_url_for(remote)?;
    refuse_release_remote(remote_repo_name(&url))?;
    let shown = redact_url(&url);
    let unknown = |warning: String| RemoteCheck {
        warnings: vec![warning],
        url: url.clone(),
        visibility: Visibility::Unknown,
        needs_confirmation: true,
        foreign_owner: false,
    };
    let (owner, name) = match github_target(gh, &url) {
        Target::GitHub(owner, name) => (owner, name),
        Target::Elsewhere => {
            return Ok(unknown(format!(
                "Tworky can't check who can see {shown}; make sure it's private"
            )));
        }
        Target::UnresolvedAlias { host, seems } => {
            return Ok(unknown(format!(
                "Tworky couldn't look up the SSH host {host} in your SSH config, so it can't check who can see {shown}; if it's {seems} on GitHub, make sure that's private"
            )));
        }
    };
    let nwo = format!("{owner}/{name}");
    let mut warnings = Vec::new();
    let visibility = match gh.repo(&nwo) {
        Ok(Some(view)) => view.visibility,
        Ok(None) => {
            warnings.push(format!(
                "gh can't see {nwo}, so Tworky can't check who can see it; make sure it's private"
            ));
            Visibility::Unknown
        }
        Err(e) => {
            warnings.push(format!(
                "couldn't check who can see {nwo}; make sure it's private: {e:#}"
            ));
            Visibility::Unknown
        }
    };
    match visibility {
        Visibility::Public => warnings.push(format!(
            "{nwo} is public: anyone can read your guidance, project config and memory"
        )),
        Visibility::Internal => warnings.push(format!(
            "{nwo} is internal: everyone in its enterprise can read your guidance, project config and memory"
        )),
        Visibility::Unknown if warnings.is_empty() => warnings.push(format!(
            "gh didn't say who can see {nwo}; make sure it's private"
        )),
        _ => {}
    }
    let owner_warning = owner_warning(gh, &owner);
    let foreign_owner = owner_warning.is_some();
    warnings.extend(owner_warning);
    Ok(RemoteCheck {
        url,
        visibility,
        warnings,
        needs_confirmation: matches!(visibility, Visibility::Public | Visibility::Unknown),
        foreign_owner,
    })
}

enum Target {
    GitHub(String, String),
    /// An SSH host alias Tworky couldn't resolve, whose path looks like the GitHub repo `seems`.
    UnresolvedAlias {
        host: String,
        seems: String,
    },
    Elsewhere,
}

/// The GitHub repo `url` points at: a github.com URL, or an SSH URL on a host alias such as
/// `git@github-personal:me/tworky-config.git` that the user's SSH config resolves to github.com.
fn github_target(gh: &dyn GitHub, url: &str) -> Target {
    let Some(remote) = split_remote(url) else {
        return Target::Elsewhere;
    };
    let Some((owner, name)) = owner_and_name(remote.path) else {
        return Target::Elsewhere;
    };
    let host = remote.host.to_ascii_lowercase();
    if host == "github.com" || host == "www.github.com" {
        return Target::GitHub(owner, name);
    }
    let safe = |arg: &str| {
        !arg.is_empty()
            && !arg.starts_with('-')
            && arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    if !remote.ssh || !safe(&host) || !remote.user.is_none_or(safe) {
        return Target::Elsewhere;
    }
    match gh.ssh_hostname(remote.host, remote.user, remote.port) {
        Ok(resolved)
            if resolved.eq_ignore_ascii_case("github.com")
                || resolved.eq_ignore_ascii_case("ssh.github.com") =>
        {
            Target::GitHub(owner, name)
        }
        Ok(_) => Target::Elsewhere,
        Err(e) => {
            eprintln!("resolving the SSH host {host}: {e:#}");
            Target::UnresolvedAlias {
                host,
                seems: format!("{owner}/{name}"),
            }
        }
    }
}

/// A warning when `owner` isn't the user, since whoever can push to the remote changes what runs
/// here.
fn owner_warning(gh: &dyn GitHub, owner: &str) -> Option<String> {
    let login = gh.owners().ok()?.into_iter().next()?;
    (!login.eq_ignore_ascii_case(owner)).then(|| {
        format!(
            "{owner} isn't your account ({login}): anyone who can push to its repo can change the hooks, services and guidance Tworky runs on this machine"
        )
    })
}

/// `remote` trimmed, with a GitHub `owner/name` made its URL.
fn remote_url_for(remote: &str) -> Result<String> {
    let remote = remote.trim();
    if remote.is_empty() {
        bail!("no remote given");
    }
    let shorthand = match remote.split('/').collect::<Vec<_>>().as_slice() {
        [owner, name] => [owner, name].iter().all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        }),
        _ => false,
    };
    Ok(if shorthand {
        format!("https://github.com/{}.git", without_git_suffix(remote))
    } else {
        remote.to_string()
    })
}

/// A remote URL's host and path, and the user and port it names. `ssh` is set for `ssh://` URLs
/// and scp-like `user@host:path` ones, whose host may be an alias from the user's SSH config.
struct RemoteParts<'a> {
    host: &'a str,
    path: &'a str,
    ssh: bool,
    user: Option<&'a str>,
    port: Option<&'a str>,
}

/// Splits a `scheme://[userinfo@]host[:port]/path` URL or a scp-like `[user@]host:path`, or
/// returns `None` for a local path, and for a URL whose host Tworky might read differently from
/// git: one with a query, fragment or whitespace, or a backslash before the path.
fn split_remote(url: &str) -> Option<RemoteParts<'_>> {
    if url.contains(['#', '?']) || url.contains(char::is_whitespace) {
        return None;
    }
    let (authority, path, ssh) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            if scheme.contains('\\') || authority.contains('\\') {
                return None;
            }
            (authority, path, scheme.eq_ignore_ascii_case("ssh"))
        }
        None => {
            let (authority, path) = url.split_once(':')?;
            // A drive letter (`C:/config.git`) or a path holding a colon isn't a host.
            if authority.len() < 2 || authority.contains(['/', '\\']) {
                return None;
            }
            (authority, path, true)
        }
    };
    let (user, host) = match authority.rsplit_once('@') {
        Some((userinfo, host)) => (Some(userinfo.split(':').next().unwrap_or(userinfo)), host),
        None => (None, authority),
    };
    let (host, port) = match host.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
            (host, Some(port).filter(|p| !p.is_empty()))
        }
        _ => (host, None),
    };
    (!host.is_empty()).then_some(RemoteParts {
        host,
        path,
        ssh,
        user,
        port,
    })
}

/// `owner/name` from a remote's path, with any `.git` and slashes around it dropped.
fn owner_and_name(path: &str) -> Option<(String, String)> {
    let path = without_git_suffix(path.trim_matches('/'));
    match path.split('/').collect::<Vec<_>>().as_slice() {
        [owner, name] if !owner.is_empty() && !name.is_empty() => {
            Some((owner.to_string(), name.to_string()))
        }
        _ => None,
    }
}

/// A remote the config now syncs with, and what the user was warned about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Connected {
    pub check: RemoteCheck,
    pub sync: SyncRecord,
}

/// Refuses a remote that needs confirming when the user saw no check, and one whose check now
/// differs from what the user saw ([`CheckChanged`]): another URL, riskier visibility, or an owner
/// warning they didn't get.
fn needs_confirmation(check: &RemoteCheck, seen: Option<&Seen>) -> Result<()> {
    let Some(seen) = seen else {
        if check.needs_confirmation {
            bail!(
                "{}; confirm to sync your config with it anyway",
                check.risk()
            );
        }
        return Ok(());
    };
    let risk = |visibility: Visibility| match visibility {
        Visibility::Private => 0,
        Visibility::Internal | Visibility::Unknown => 1,
        Visibility::Public => 2,
    };
    if seen.url != redact_url(&check.url)
        || risk(check.visibility) > risk(seen.visibility)
        || (check.foreign_owner && !seen.foreign_owner)
    {
        return Err(CheckChanged(check.clone()).into());
    }
    Ok(())
}

impl RemoteCheck {
    /// Why the remote needs confirming, for a question or an error.
    pub fn risk(&self) -> String {
        let url = redact_url(&self.url);
        match self.visibility {
            Visibility::Public => format!("{url} is public"),
            _ => format!("Tworky can't check who can see {url}"),
        }
    }
}

/// Checks `remote` ([`check_remote`]), then makes it the remote and syncs with it. A remote that
/// needs confirming is refused unless the user `seen` its check, and so is one whose check finds
/// more than they saw.
pub fn connect_remote(
    paths: &Paths,
    gh: &dyn GitHub,
    remote: &str,
    seen: Option<Seen>,
) -> Result<Connected> {
    let check = check_remote(gh, remote)?;
    needs_confirmation(&check, seen.as_ref())?;
    let sync = set_remote(paths, &check.url, check.visibility)?;
    Ok(Connected { check, sync })
}

/// [`check_remote`] for the repo's current `origin`.
pub fn check_current_remote(paths: &Paths, gh: &dyn GitHub) -> Result<RemoteCheck> {
    check_remote(
        gh,
        &remote_url(paths).context("the config repo has no remote")?,
    )
}

/// Connects an `origin` that wasn't connected through Tworky, such as one added with plain git,
/// once the user has `seen` its check: checks it like [`connect_remote`], records it and syncs
/// with it. Refuses an `origin` that isn't the URL the user saw, and one that fetches or pushes
/// anywhere but the repo it names ([`resolved_remote`]).
pub fn confirm_remote(paths: &Paths, gh: &dyn GitHub, seen: &Seen) -> Result<Connected> {
    let check = check_current_remote(paths, gh)?;
    needs_confirmation(&check, Some(seen))?;
    let _sync = with_sync_lock(paths)?;
    let urls = resolved_remote(paths, &remote_dirs(paths), &check.url)?;
    record_remote(paths, urls, check.visibility)?;
    let sync = sync_locked(paths, Trigger::User, NETWORK_TIMEOUT)?;
    Ok(Connected { check, sync })
}

/// Creates a private [`default_remote_name`] repo for `owner` (the user when `None`), makes it
/// the remote and pushes the config to it.
pub fn create_remote(paths: &Paths, gh: &dyn GitHub, owner: Option<&str>) -> Result<Connected> {
    create_remote_named(paths, gh, owner, default_remote_name())
}

fn create_remote_named(
    paths: &Paths,
    gh: &dyn GitHub,
    owner: Option<&str>,
    name: &str,
) -> Result<Connected> {
    refuse_release_remote(name)?;
    if !paths.has_repo() {
        bail!(
            "the config repo {} isn't set up",
            paths.repo_dir().display()
        );
    }
    if let Some(current) = remote_url(paths) {
        bail!(
            "the config repo already syncs with {}; remove it first with `tw config remote rm`",
            redact_url(&current)
        );
    }
    let owner = match owner.map(str::trim).filter(|o| !o.is_empty()) {
        Some(owner) => owner.to_string(),
        None => gh
            .owners()?
            .into_iter()
            .next()
            .context("gh didn't say which account it's signed in to")?,
    };
    let nwo = format!("{owner}/{name}");
    gh.create_private(&nwo)
        .with_context(|| format!("creating {nwo}"))?;
    let view = gh
        .repo(&nwo)?
        .with_context(|| format!("gh created {nwo} but can't find it"))?;
    let owner_warning = owner_warning(gh, &owner);
    let check = RemoteCheck {
        url: view.url,
        visibility: view.visibility,
        foreign_owner: owner_warning.is_some(),
        warnings: owner_warning.into_iter().collect(),
        needs_confirmation: false,
    };
    let sync = set_remote(paths, &check.url, check.visibility).with_context(|| {
        format!(
            "created {nwo}, but couldn't sync with it, so the config doesn't use it yet; once the problem below is fixed, connect it from Settings › Remote or with `tw config remote set {nwo}` in your own terminal"
        )
    })?;
    Ok(Connected { check, sync })
}

/// A config repo the user or one of their organisations already has on GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SharedConfig {
    pub owner: String,
    pub url: String,
    pub visibility: Visibility,
}

/// The [`default_remote_name`] repos of the user and each of their organisations, looked up in
/// parallel. Fails only when every lookup does.
pub fn find_shared_configs(gh: &dyn GitHub) -> Result<Vec<SharedConfig>> {
    let owners = gh.owners()?;
    let name = default_remote_name();
    let lookups: Vec<Result<Option<RepoView>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = owners
            .iter()
            .map(|owner| scope.spawn(move || gh.repo(&format!("{owner}/{name}"))))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("the lookup panicked")))
            })
            .collect()
    });
    let mut found = Vec::new();
    let mut first_error = None;
    let failed_all = lookups.iter().all(|l| l.is_err());
    for (owner, lookup) in owners.iter().zip(lookups) {
        match lookup {
            Ok(Some(view)) => found.push(SharedConfig {
                owner: owner.clone(),
                url: view.url,
                visibility: view.visibility,
            }),
            Ok(None) => {}
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) if failed_all => Err(e),
        _ => Ok(found),
    }
}

/// Whether the app should ask how to set up config, rather than start fresh: there's no config
/// repo and no legacy config to import.
pub fn awaits_first_run(paths: &Paths) -> bool {
    !paths.has_repo() && !has_legacy_config(paths)
}

/// Sets this machine up from a shared config: clones `remote` into `repo/` and syncs with it, then
/// looks for the clones its projects name in `source_roots`. An empty remote gets a fresh config
/// pushed to it. The remote is confirmed as [`connect_remote`] does.
pub fn use_shared_config(
    paths: &Paths,
    gh: &dyn GitHub,
    remote: &str,
    seen: Option<Seen>,
) -> Result<Connected> {
    use_shared_config_within(paths, gh, remote, seen, NETWORK_TIMEOUT)
}

fn use_shared_config_within(
    paths: &Paths,
    gh: &dyn GitHub,
    remote: &str,
    seen: Option<Seen>,
    network: Duration,
) -> Result<Connected> {
    let check = check_remote(gh, remote)?;
    needs_confirmation(&check, seen.as_ref())?;
    let cloned = {
        let _sync = with_sync_lock(paths)?;
        let cloned = !is_unconnected_init(paths)? && clone_into_repo(paths, &check.url, network)?;
        if cloned {
            let urls = resolved_remote(paths, &remote_dirs(paths), &check.url)?;
            record_remote(paths, urls, check.visibility)?;
        }
        cloned
    };
    let sync = if cloned {
        read_record(paths)
    } else {
        {
            let guard = with_lock(paths)?;
            if !paths.has_repo() {
                init(&guard, paths)?;
            } else if !is_unconnected_init(paths)? {
                bail!(
                    "{} already exists; set its remote with `tw config remote set` instead",
                    paths.repo_dir().display()
                );
            }
        }
        set_remote_within(paths, &check.url, check.visibility, network)?
    };
    match crate::config::Config::load_from(paths.clone())
        .and_then(|config| crate::config::find_sources(&config, None))
    {
        Ok(found) => {
            for f in found {
                eprintln!(
                    "found {} of {} at {}",
                    f.repo,
                    f.project,
                    f.source.display()
                );
            }
        }
        Err(e) => eprintln!("looking for clones: {e:#}"),
    }
    Ok(Connected { check, sync })
}

/// Whether `repo/` holds only its first commit and has no remote, as [`use_shared_config`] leaves
/// it when connecting an empty remote fails, so trying again connects it rather than cloning.
fn is_unconnected_init(paths: &Paths) -> Result<bool> {
    Ok(paths.has_repo() && remote_url(paths).is_none() && is_init_only(paths)?)
}

/// Clones `url` into `repo.tmp/`, gives it the repo-local config [`init`] writes and checks out its
/// `main`, then renames it to `repo/`, so a clone that fails or is interrupted leaves no `repo/`.
/// Returns `false`, having made nothing, when the remote has no `main` yet.
fn clone_into_repo(paths: &Paths, url: &str, network: Duration) -> Result<bool> {
    let _guard = with_lock(paths)?;
    let repo = paths.repo_dir();
    if paths.has_repo() {
        bail!(
            "{} already exists; set its remote with `tw config remote set` instead",
            repo.display()
        );
    }
    set_aside_untracked_repo(paths)?;
    let tmp = paths.repo_tmp_dir();
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .with_context(|| format!("removing the leftover {}", tmp.display()))?;
    }
    std::fs::create_dir_all(paths.repo_hooks_dir())?;
    let cloned = (|| {
        git(
            paths,
            &paths.config_dir,
            &["clone", "-q", "--no-checkout", url, &forward_slashes(&tmp)],
            network,
        )?;
        configure(paths, &tmp)?;
        resolved_remote(paths, std::slice::from_ref(&tmp), url)?;
        if remote_main(paths, &tmp)?.is_none() {
            return Ok(false);
        }
        let run = |args: &[&str]| git(paths, &tmp, args, LOCAL_TIMEOUT);
        run(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
        run(&["reset", "-q", "--hard", "refs/remotes/origin/main"])?;
        run(&["config", "branch.main.remote", REMOTE])?;
        run(&["config", "branch.main.merge", "refs/heads/main"])?;
        rename_retrying(&tmp, &repo, RENAME_RETRY)?;
        Ok(true)
    })();
    if matches!(cloned, Ok(true)) {
        forget_sync_record(paths);
    } else {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    cloned
        .map_err(|e: anyhow::Error| anyhow::anyhow!("{}", failure_message(&e)))
        .with_context(|| format!("cloning {}", redact_url(url)))
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
        static BEFORE_ADD: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
        static SSH_COMMAND: RefCell<Option<String>> = const { RefCell::new(None) };
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
        if let Some(ssh) = SSH_COMMAND.with_borrow(|s| s.clone()) {
            cmd.env("GIT_SSH_COMMAND", ssh)
                .env("GIT_SSH_VARIANT", "simple");
        }
        if args.contains(&"add")
            && let Some(before) = BEFORE_ADD.take()
        {
            before();
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
        for key in ["user.name", "user.email"] {
            assert!(
                git(
                    &home.paths,
                    &home.paths.repo_dir(),
                    &["config", "--local", key],
                    LOCAL_TIMEOUT
                )
                .is_err(),
                "{key} is resolved per commit, never stored"
            );
        }
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
        assert_eq!(status.sync.state, SyncState::NoRemote);

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
        record_remote(
            &home.paths,
            remote_urls_in(&home.paths, &home.paths.repo_dir()).unwrap(),
            Visibility::Unknown,
        )
        .unwrap();
        home.git(&["push", "-q", "-u", "origin", "main"]);
        let guard = with_lock(&home.paths).unwrap();
        let memory = home.write("projects/acme/memory.md", "# Acme\n");
        commit(&guard, &home.paths, &[&memory], "Save acme memory").unwrap();

        let lock = home.paths.repo_dir().join(".git").join("index.lock");
        File::create(&lock).unwrap();
        let fresh = status(&home.paths).unwrap();
        assert_eq!(fresh.ahead_behind, Some((1, 0)));
        assert_eq!(fresh.sync.state, SyncState::Idle);
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
        assert_eq!(
            machine.source("acme", "web"),
            Some(&home.dir.path().join("gone")),
            "kept though it's missing right now"
        );
        assert_eq!(
            machine.source("acme", "misc"),
            Some(&PathBuf::from("C:/nowhere/misc"))
        );
        assert!(
            done.iter().any(|l| l.contains("repo misc has no `repo")),
            "{done:?}"
        );
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
    fn identity_is_resolved_per_commit() {
        let home = Home::new("");
        home.init();
        let global = home.dir.path().join("gitconfig");
        std::fs::write(
            &global,
            "[user]\n\tname = Later User\n\temail = later@example.com\n",
        )
        .unwrap();
        let guard = with_lock(&home.paths).unwrap();
        let memory = home.write("projects/acme/memory.md", "# Acme\n");
        assert!(commit(&guard, &home.paths, &[&memory], "Save acme memory").unwrap());
        assert_eq!(
            home.lines(&["log", "--format=%an <%ae>"]),
            [
                "Later User <later@example.com>",
                "Tworky <tworky@localhost>"
            ]
        );
    }

    #[test]
    fn a_project_toml_broken_between_check_and_staging_is_held_back() {
        let home = Home::new("");
        home.init();
        let guard = with_lock(&home.paths).unwrap();
        let project = home.write("projects/acme/project.toml", PROJECT);
        let memory = home.write("projects/acme/memory.md", "# Acme\n");

        let file = project.clone();
        BEFORE_ADD.set(Some(Box::new(move || {
            std::fs::write(file, "name = [").unwrap()
        })));
        let swept = sweep(&guard, &home.paths).unwrap();
        assert!(swept.committed);
        assert_eq!(swept.held_back, std::slice::from_ref(&project));
        assert_eq!(
            home.lines(&["ls-files", "projects"]),
            ["projects/acme/memory.md"]
        );
        assert_eq!(
            status(&home.paths).unwrap().changes,
            ["projects/acme/project.toml"]
        );

        std::fs::write(&project, PROJECT).unwrap();
        std::fs::write(&memory, "# Acme, again\n").unwrap();
        let file = project.clone();
        BEFORE_ADD.set(Some(Box::new(move || {
            std::fs::write(file, "name = [").unwrap()
        })));
        let e = commit(&guard, &home.paths, &[&project, &memory], "Both").unwrap_err();
        assert!(e.to_string().contains("doesn't parse"), "{e:#}");
        assert_eq!(home.log().len(), 2, "nothing committed");
        assert!(
            home.git(&["diff", "--cached", "--name-only"]).is_empty(),
            "nothing left staged"
        );
    }

    #[test]
    fn git_never_looks_above_the_config_folder() {
        let home = Home::new("");
        git(
            &home.paths,
            home.dir.path(),
            &["init", "-q", "-b", "main"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        let repo = home.paths.repo_dir();
        std::fs::create_dir_all(&repo).unwrap();

        let e = git(
            &home.paths,
            &repo,
            &["rev-parse", "--show-toplevel"],
            LOCAL_TIMEOUT,
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("not a git repository"), "{e:#}");
    }

    #[test]
    fn a_failed_import_keeps_the_legacy_config_readable_and_writers_out() {
        let (home, clone, _) = legacy_home();
        let paths = &home.paths;
        FAIL.set(Some("commit"));
        migrate(paths).unwrap_err();
        assert!(recorded_failures(paths)[0].contains("injected failure"));

        let config = crate::config::Config::load_from(paths.clone()).unwrap();
        let acme = config.project("acme").expect("read from the legacy file");
        assert_eq!(acme.repos[0].source, Some(clone));
        assert!(config.project_errors.contains_key("beta"));
        assert!(
            config.load_errors[0].contains("isn't set up"),
            "{:?}",
            config.load_errors
        );
        assert_eq!(
            crate::memory::read(&paths.memory_file("acme", None)).unwrap(),
            "# Acme memory\n"
        );

        let base = crate::memory::hash("# Acme memory\n");
        let e = crate::memory::save(paths, "acme", None, &base, "# Changed\n").unwrap_err();
        assert!(e.to_string().contains("isn't set up"), "{e:#}");
        let e = crate::config::create_repo_project(
            paths,
            &[],
            &[crate::config::NewRepo {
                name_with_owner: "acme/new".into(),
                source: home.dir.path().join("new"),
                base: "origin/main".into(),
            }],
            None,
        )
        .unwrap_err();
        assert!(e.to_string().contains("isn't set up"), "{e:#}");
        assert!(!paths.repo_dir().exists(), "no writer made repo/");
        assert_eq!(
            std::fs::read_to_string(paths.legacy_memory_dir().join("acme.md")).unwrap(),
            "# Acme memory\n"
        );

        FAIL.set(None);
        migrate(paths).unwrap();
        assert_eq!(home.log(), ["Import existing config"]);
        assert!(recorded_failures(paths).is_empty());
        let config = crate::config::Config::load_from(paths.clone()).unwrap();
        assert!(config.load_errors.is_empty(), "{:?}", config.load_errors);
        assert!(config.project("acme").is_some());
    }

    #[test]
    fn a_repo_folder_without_git_is_set_aside_and_the_import_runs() {
        let (home, _, _) = legacy_home();
        let paths = &home.paths;
        let stray = paths.project_dir("acme").join("memory.md");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "# Written without a repo\n").unwrap();

        let done = migrate(paths).unwrap();
        assert!(
            done.iter().any(|l| l.contains("it had no .git")),
            "{done:?}"
        );
        assert_eq!(home.log(), ["Import existing config"]);
        assert_eq!(
            std::fs::read_to_string(paths.memory_file("acme", None)).unwrap(),
            "# Acme memory\n"
        );
        assert!(
            paths
                .config_dir
                .join("repo.no-git/projects/acme/memory.md")
                .exists()
        );
        assert!(!paths.legacy_projects_dir().exists());
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

    #[cfg(windows)]
    #[test]
    fn a_blocked_legacy_rename_fails_fast_as_a_backup_rename() {
        use std::os::windows::fs::OpenOptionsExt;
        let (home, _, _) = legacy_home();
        let paths = &home.paths;
        import(&with_lock(paths).unwrap(), paths).unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(paths.legacy_projects_dir().join("acme-old.toml"))
            .unwrap();

        let started = Instant::now();
        let e = migrate(paths).unwrap_err();
        assert!(
            started.elapsed() < RENAME_RETRY / 2,
            "{:?}",
            started.elapsed()
        );
        let e = format!("{e:#}");
        assert!(
            e.starts_with("couldn't rename the old projects/ folder to projects.pre-repo"),
            "{e}"
        );
        assert!(!e.contains("set up the config repo"), "{e}");
        assert!(recorded_failures(paths).is_empty());

        drop(held);
        migrate(paths).unwrap();
        assert!(!paths.legacy_projects_dir().exists());
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

    /// Two machines' homes sharing a bare remote: A pushed its new repo there, and B, also new,
    /// took A's history.
    struct Machines {
        dir: tempfile::TempDir,
        a: Paths,
        b: Paths,
        remote: String,
    }

    fn machines() -> Machines {
        let dir = tempfile::tempdir().unwrap();
        isolate_git_for_tests(dir.path());
        let a = paths_under(&dir.path().join("a"));
        let b = paths_under(&dir.path().join("b"));
        git(
            &a,
            dir.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        let remote = forward_slashes(&dir.path().join("remote.git"));
        for paths in [&a, &b] {
            init(&with_lock(paths).unwrap(), paths).unwrap();
            assert_eq!(
                set_remote(paths, &remote, Visibility::Unknown)
                    .unwrap()
                    .state,
                SyncState::Idle
            );
        }
        Machines { dir, a, b, remote }
    }

    impl Machines {
        fn remote_head(&self) -> String {
            rev_parse(&self.a, &self.dir.path().join("remote.git"), "main").unwrap()
        }

        fn global_config(&self) -> PathBuf {
            self.dir.path().join("gitconfig")
        }
    }

    const MEMORY: &str = "projects/acme/memory.md";
    const PROJECT_FILE: &str = "projects/acme/project.toml";

    fn write_file(paths: &Paths, relative: &str, text: &str) -> PathBuf {
        let file = paths.repo_dir().join(relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, text).unwrap();
        file
    }

    fn commit_file(paths: &Paths, relative: &str, text: &str) {
        let file = write_file(paths, relative, text);
        let guard = with_lock(paths).unwrap();
        assert!(commit(&guard, paths, &[file], &format!("Save {relative}")).unwrap());
    }

    fn read_file(paths: &Paths, relative: &str) -> String {
        std::fs::read_to_string(paths.repo_dir().join(relative)).unwrap()
    }

    fn head(paths: &Paths) -> String {
        rev_parse(paths, &paths.repo_dir(), "refs/heads/main").unwrap()
    }

    fn sync_now(paths: &Paths) -> SyncRecord {
        sync(paths, Trigger::User).unwrap()
    }

    fn in_sync_dir(paths: &Paths, args: &[&str]) -> String {
        git(paths, &paths.repo_sync_dir(), args, LOCAL_TIMEOUT).unwrap()
    }

    /// Resolves a conflict in `repo-sync` the way the settings session would, as a user with an
    /// identity of their own.
    fn finish_rebase(paths: &Paths, file: &str, text: &str) {
        std::fs::write(paths.repo_sync_dir().join(file), text).unwrap();
        in_sync_dir(paths, &["add", file]);
        in_sync_dir(
            paths,
            &[
                "-c",
                "user.name=Resolver",
                "-c",
                "user.email=resolver@example.com",
                "rebase",
                "--continue",
            ],
        );
    }

    fn conflicted(m: &Machines) -> String {
        commit_file(&m.a, MEMORY, "# Acme\n- from A\n");
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        commit_file(&m.b, MEMORY, "# Acme\n- from B\n");
        let before = head(&m.b);
        let stopped = sync_now(&m.b);
        assert_eq!(
            stopped.state,
            SyncState::Conflict {
                files: vec![MEMORY.into()]
            }
        );
        assert_eq!(stopped.operation, Some(Operation::Rebase));
        before
    }

    #[test]
    fn a_commit_is_pushed() {
        let m = machines();
        assert_eq!(head(&m.b), head(&m.a), "B took A's history");
        assert_eq!(m.remote_head(), head(&m.a));
        assert_eq!(remote_url(&m.a).as_deref(), Some(m.remote.as_str()));

        commit_file(&m.a, MEMORY, "# Acme\n");
        let synced = sync_now(&m.a);
        assert_eq!(synced.state, SyncState::Idle);
        assert!(synced.last_sync.is_some());
        assert_eq!(m.remote_head(), head(&m.a));
        assert_eq!(status(&m.a).unwrap().ahead_behind, Some((0, 0)));
    }

    #[test]
    fn a_remote_change_reaches_repo_and_is_summarised_until_dismissed() {
        let m = machines();
        commit_file(
            &m.a,
            PROJECT_FILE,
            &format!("{PROJECT}\n[[hooks.setup]]\ncwd = \"api\"\nrun = \"npm ci\"\n"),
        );
        commit_file(&m.a, MEMORY, "# Acme\n");
        sync_now(&m.a);

        let synced = sync_now(&m.b);
        assert_eq!(synced.state, SyncState::Idle);
        assert_eq!(head(&m.b), head(&m.a));
        assert_eq!(read_file(&m.b, MEMORY), "# Acme\n");
        assert_eq!(synced.incoming.hooks_or_services, ["acme"]);
        assert_eq!(synced.incoming.guidance, [MEMORY]);
        let config = crate::config::Config::load_from(m.b.clone()).unwrap();
        assert!(config.project("acme").is_some());

        commit_file(&m.a, "tworky/claude.local.md", "Be brief.\n");
        sync_now(&m.a);
        let again = sync_now(&m.b);
        assert_eq!(
            again.incoming.guidance,
            [MEMORY, "tworky/claude.local.md"],
            "kept until dismissed"
        );
        dismiss_incoming(&m.b).unwrap();
        assert!(sync_status(&m.b).unwrap().incoming.is_empty());
    }

    #[test]
    fn both_sides_with_commits_converge_under_tworkys_identity() {
        let m = machines();
        commit_file(&m.a, "tworky/claude.local.md", "From A\n");
        sync_now(&m.a);
        commit_file(&m.b, MEMORY, "From B\n");

        assert_eq!(sync_now(&m.b).state, SyncState::Idle);
        assert_eq!(read_file(&m.b, "tworky/claude.local.md"), "From A\n");
        assert_eq!(m.remote_head(), head(&m.b));
        let repo = m.b.repo_dir();
        let moved = git(
            &m.b,
            &repo,
            &["reflog", "-1", "--format=%gs", "main"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        assert!(moved.starts_with("reset: moving to"), "{moved}");
        let rebased = git(
            &m.b,
            &repo,
            &["log", "-1", "--format=%s|%cn <%ce>", "main"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        assert_eq!(
            rebased.trim(),
            format!("Save {MEMORY}|Tworky <tworky@localhost>")
        );

        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(read_file(&m.a, MEMORY), "From B\n");
        assert_eq!(head(&m.a), head(&m.b));
    }

    #[test]
    fn a_conflict_leaves_repo_alone_until_resolved_in_repo_sync() {
        let m = machines();
        let before = conflicted(&m);
        assert_eq!(head(&m.b), before);
        assert_eq!(read_file(&m.b, MEMORY), "# Acme\n- from B\n");
        let config = crate::config::Config::load_from(m.b.clone()).unwrap();
        assert!(config.load_errors.is_empty(), "{:?}", config.load_errors);
        assert!(status(&m.b).unwrap().is_clean());
        assert!(matches!(
            sync(&m.b, Trigger::Background).unwrap().state,
            SyncState::Conflict { .. }
        ));

        finish_rebase(&m.b, MEMORY, "# Acme\n- from A\n- from B\n");
        let synced = sync_now(&m.b);
        assert_eq!(synced.state, SyncState::Idle);
        assert!(synced.incoming.dropped.is_empty(), "{synced:?}");
        assert_eq!(read_file(&m.b, MEMORY), "# Acme\n- from A\n- from B\n");
        assert_eq!(m.remote_head(), head(&m.b));
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(head(&m.a), head(&m.b));
    }

    #[test]
    fn a_commit_made_during_a_conflict_is_replayed_after_resolution() {
        let m = machines();
        conflicted(&m);
        commit_file(&m.b, "tworky/claude.local.md", "B guidance\n");

        finish_rebase(&m.b, MEMORY, "# Acme\n- from A\n- from B\n");
        let synced = sync_now(&m.b);
        assert_eq!(synced.state, SyncState::Idle);
        assert!(synced.incoming.dropped.is_empty(), "{synced:?}");
        assert_eq!(read_file(&m.b, "tworky/claude.local.md"), "B guidance\n");
        assert_eq!(read_file(&m.b, MEMORY), "# Acme\n- from A\n- from B\n");
        assert_eq!(m.remote_head(), head(&m.b));
        assert!(status(&m.b).unwrap().is_clean());
    }

    #[test]
    fn a_resolution_that_keeps_conflict_markers_is_refused() {
        let m = machines();
        let before = conflicted(&m);
        let marked = std::fs::read_to_string(m.b.repo_sync_dir().join(MEMORY)).unwrap();
        assert!(marked.contains("<<<<<<< "), "{marked}");
        finish_rebase(&m.b, MEMORY, &marked);

        let refused = sync_now(&m.b);
        assert_eq!(
            refused.state,
            SyncState::Conflict {
                files: vec![MEMORY.into()]
            }
        );
        assert_eq!(refused.operation, None);
        assert_eq!(head(&m.b), before);

        std::fs::write(m.b.repo_sync_dir().join(MEMORY), "# Acme\n- from both\n").unwrap();
        in_sync_dir(
            &m.b,
            &[
                "-c",
                "user.name=Resolver",
                "-c",
                "user.email=resolver@example.com",
                "commit",
                "-q",
                "-am",
                "Fix the markers",
            ],
        );
        assert_eq!(sync_now(&m.b).state, SyncState::Idle);
        assert_eq!(read_file(&m.b, MEMORY), "# Acme\n- from both\n");
    }

    #[test]
    fn markers_are_found_whatever_the_users_diff_config_and_path() {
        let m = machines();
        std::fs::write(
            m.global_config(),
            "[diff]\n\tnoprefix = true\n\tmnemonicPrefix = true\n\tsrcPrefix = x/\n\tdstPrefix = y/\n",
        )
        .unwrap();
        let file = "projects/café/memory.md";
        commit_file(&m.a, file, "# Café\n- from A\n");
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        commit_file(&m.b, file, "# Café\n- from B\n");
        let before = head(&m.b);
        assert_eq!(
            sync_now(&m.b).state,
            SyncState::Conflict {
                files: vec![file.into()]
            }
        );
        let marked = std::fs::read_to_string(m.b.repo_sync_dir().join(file)).unwrap();
        assert!(marked.contains("<<<<<<< "), "{marked}");
        finish_rebase(&m.b, file, &marked);

        let refused = sync_now(&m.b);
        assert_eq!(
            refused.state,
            SyncState::Conflict {
                files: vec![file.into()]
            }
        );
        assert_eq!(head(&m.b), before);
    }

    #[test]
    fn a_failed_push_after_a_resolution_never_replays_it() {
        let m = machines();
        let before = conflicted(&m);
        let upstream = m.remote_head();
        let resolved = "# Acme\n- from A\n- from B\n";
        finish_rebase(&m.b, MEMORY, resolved);
        let remote = m.dir.path().join("remote.git");
        let away = m.dir.path().join("remote.away");
        std::fs::rename(&remote, &away).unwrap();

        let failed = sync_now(&m.b);
        assert!(
            matches!(failed.state, SyncState::Error { .. }),
            "{failed:?}"
        );
        assert_eq!(
            (failed.base.as_deref(), failed.upstream.as_deref()),
            (None, None)
        );
        assert_eq!(failed.incoming.guidance, [MEMORY]);
        assert_eq!(read_file(&m.b, MEMORY), resolved);
        std::fs::rename(&away, &remote).unwrap();

        update_record(&m.b, |r| {
            r.base = Some(before);
            r.upstream = Some(upstream);
        })
        .unwrap();
        let synced = sync_now(&m.b);
        assert_eq!(synced.state, SyncState::Idle, "{synced:?}");
        assert_eq!(read_file(&m.b, MEMORY), resolved);
        assert_eq!(m.remote_head(), head(&m.b));
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(head(&m.a), head(&m.b));
    }

    #[test]
    fn a_resolution_blocked_by_a_local_file_resumes_once_it_is_fixed() {
        let m = machines();
        commit_file(&m.a, PROJECT_FILE, PROJECT);
        commit_file(&m.a, MEMORY, "# Acme\n- from A\n");
        sync_now(&m.a);
        commit_file(&m.b, MEMORY, "# Acme\n- from B\n");
        assert!(matches!(sync_now(&m.b).state, SyncState::Conflict { .. }));
        write_file(&m.b, PROJECT_FILE, "name = [");
        let resolved = "# Acme\n- from A\n- from B\n";
        finish_rebase(&m.b, MEMORY, resolved);
        let resolution = in_sync_dir(&m.b, &["rev-parse", "HEAD"]);

        let blocked = sync_now(&m.b);
        assert_eq!(
            blocked.state,
            SyncState::BlockedByLocal {
                files: vec![PROJECT_FILE.into()]
            }
        );
        assert!(blocked.base.is_some() && blocked.upstream.is_some());
        let again = sync(&m.b, Trigger::Background).unwrap();
        assert_eq!(again.state, blocked.state);
        assert_eq!(in_sync_dir(&m.b, &["rev-parse", "HEAD"]), resolution);

        std::fs::remove_file(m.b.repo_dir().join(PROJECT_FILE)).unwrap();
        let synced = sync_now(&m.b);
        assert_eq!(synced.state, SyncState::Idle, "{synced:?}");
        assert_eq!(read_file(&m.b, MEMORY), resolved);
        assert_eq!(read_file(&m.b, PROJECT_FILE), PROJECT);
        assert_eq!(m.remote_head(), head(&m.b));
    }

    #[test]
    fn a_resolution_that_drops_this_machines_commit_is_recorded() {
        let m = machines();
        conflicted(&m);
        in_sync_dir(&m.b, &["rebase", "--skip"]);

        let synced = sync_now(&m.b);
        assert_eq!(synced.state, SyncState::Idle);
        assert_eq!(read_file(&m.b, MEMORY), "# Acme\n- from A\n");
        assert_eq!(synced.incoming.dropped, [format!("Save {MEMORY}")]);
        dismiss_incoming(&m.b).unwrap();
        assert!(sync_status(&m.b).unwrap().incoming.dropped.is_empty());
    }

    #[test]
    fn the_main_stamp_changes_with_packed_refs_and_reftables() {
        let home = Home::new("");
        home.init();
        let loose = main_ref_stamp(&home.paths).unwrap();
        home.git(&["pack-refs", "--all"]);
        let packed = main_ref_stamp(&home.paths).unwrap();
        assert_ne!(packed, loose);
        let memory = home.write(MEMORY, "# Acme\n");
        assert!(
            commit(
                &with_lock(&home.paths).unwrap(),
                &home.paths,
                &[&memory],
                "Save"
            )
            .unwrap()
        );
        home.git(&["pack-refs", "--all"]);
        assert_ne!(main_ref_stamp(&home.paths).unwrap(), packed);

        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        assert_eq!(main_ref_stamp(&paths), None);
        let tables = paths.repo_dir().join(".git/reftable/tables.list");
        std::fs::create_dir_all(tables.parent().unwrap()).unwrap();
        std::fs::write(&tables, "0x1.ref\n").unwrap();
        let first = main_ref_stamp(&paths).unwrap();
        std::fs::write(&tables, "0x1.ref\n0x2.ref\n").unwrap();
        assert_ne!(main_ref_stamp(&paths).unwrap(), first);
    }

    #[test]
    fn abort_stops_the_rebase_and_pauses_background_syncs() {
        let m = machines();
        let before = conflicted(&m);

        let aborted = abort(&m.b).unwrap();
        assert!(aborted.paused);
        assert_eq!(aborted.operation, None);
        assert_eq!(aborted.state, SyncState::Idle, "the conflict is gone");
        assert_eq!(sync_status(&m.b).unwrap(), aborted);
        assert_eq!(
            progress(&m.b, &m.b.repo_sync_dir()).unwrap(),
            Progress::None
        );
        assert_eq!(sync(&m.b, Trigger::Background).unwrap(), aborted);
        assert_eq!(head(&m.b), before);

        let again = sync_now(&m.b);
        assert!(!again.paused);
        assert_eq!(again.operation, Some(Operation::Rebase));
    }

    /// Commits `text` as `projects/beta/project.toml` in A, past the parse check, and pushes it.
    fn push_broken_project(m: &Machines, text: &str) -> &'static str {
        let broken = "projects/beta/project.toml";
        write_file(&m.a, broken, text);
        let a = m.a.repo_dir();
        for args in [
            &["add", broken][..],
            &[
                "-c",
                "user.name=A",
                "-c",
                "user.email=a@example.com",
                "commit",
                "-q",
                "-m",
                "Break beta",
            ],
            &["push", "-q", "origin", "main"],
        ] {
            git(&m.a, &a, args, LOCAL_TIMEOUT).unwrap();
        }
        broken
    }

    fn commit_in_sync_dir(paths: &Paths, message: &str) {
        in_sync_dir(
            paths,
            &[
                "-c",
                "user.name=Resolver",
                "-c",
                "user.email=resolver@example.com",
                "commit",
                "-q",
                "-am",
                message,
            ],
        );
    }

    #[test]
    fn a_remote_project_toml_that_does_not_parse_is_held_back() {
        let m = machines();
        let broken = push_broken_project(&m, "name = [");
        let before = head(&m.b);

        let held = sync_now(&m.b);
        assert_eq!(
            held.state,
            SyncState::HeldBack {
                files: vec![broken.into()],
                cause: HeldBackCause::Invalid,
            }
        );
        assert_eq!(head(&m.b), before);
        assert!(!m.b.project_file("beta").exists());
        assert_eq!(
            sync(&m.b, Trigger::Background).unwrap().state,
            held.state,
            "still held back"
        );

        std::fs::write(
            m.b.repo_sync_dir().join(broken),
            PROJECT.replace("acme", "beta"),
        )
        .unwrap();
        commit_in_sync_dir(&m.b, "Fix beta");
        assert_eq!(sync_now(&m.b).state, SyncState::Idle);
        assert!(m.b.project_file("beta").exists());
        assert_eq!(m.remote_head(), head(&m.b));
    }

    #[test]
    fn a_held_back_sync_takes_a_fix_made_on_the_remote() {
        let m = machines();
        let broken = push_broken_project(&m, "name = [");
        assert!(matches!(sync_now(&m.b).state, SyncState::HeldBack { .. }));

        commit_file(&m.a, broken, &PROJECT.replace("acme", "beta"));
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        let synced = sync(&m.b, Trigger::Background).unwrap();
        assert_eq!(synced.state, SyncState::Idle, "{synced:?}");
        assert_eq!(synced.base, None);
        assert!(m.b.project_file("beta").exists());
        assert_eq!(head(&m.b), head(&m.a));
    }

    #[test]
    fn syncs_keep_an_uncommitted_fix_in_repo_sync_until_aborted() {
        let m = machines();
        let broken = push_broken_project(&m, "name = 5\n");
        let held = sync_now(&m.b);
        assert_eq!(
            held.state,
            SyncState::HeldBack {
                files: vec![broken.into()],
                cause: HeldBackCause::Unrecognised,
            }
        );
        let fix = PROJECT.replace("acme", "beta");
        let in_progress = m.b.repo_sync_dir().join(broken);
        std::fs::write(&in_progress, &fix).unwrap();

        let on_hold = SyncRecord {
            state: SyncState::HeldInSyncWorktree {
                files: vec![broken.into()],
            },
            ..held.clone()
        };
        assert_eq!(sync(&m.b, Trigger::Background).unwrap(), on_hold);
        assert_eq!(std::fs::read_to_string(&in_progress).unwrap(), fix);
        let written = std::fs::metadata(m.b.config_sync_file())
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(sync(&m.b, Trigger::Background).unwrap(), on_hold);
        assert_eq!(
            std::fs::metadata(m.b.config_sync_file())
                .unwrap()
                .modified()
                .unwrap(),
            written,
            "an unchanged hold isn't written again"
        );
        let e = sync(&m.b, Trigger::User).unwrap_err();
        assert!(
            e.to_string().contains("uncommitted") && e.to_string().contains(broken),
            "{e:#}"
        );
        assert_eq!(std::fs::read_to_string(&in_progress).unwrap(), fix);
        assert_eq!(read_record(&m.b), on_hold);

        abort(&m.b).unwrap();
        assert!(matches!(sync_now(&m.b).state, SyncState::HeldBack { .. }));
        assert_eq!(std::fs::read_to_string(&in_progress).unwrap(), "name = 5\n");

        std::fs::write(&in_progress, &fix).unwrap();
        commit_in_sync_dir(&m.b, "Fix beta");
        assert_eq!(
            sync(&m.b, Trigger::Background).unwrap().state,
            SyncState::Idle
        );
        assert!(m.b.project_file("beta").exists());
    }

    #[test]
    fn an_uncommitted_file_in_the_way_of_an_incoming_one_blocks_the_sync() {
        let m = machines();
        commit_file(&m.a, PROJECT_FILE, PROJECT);
        sync_now(&m.a);
        write_file(&m.b, PROJECT_FILE, "name = [");
        let before = head(&m.b);

        assert_eq!(
            sync_now(&m.b).state,
            SyncState::BlockedByLocal {
                files: vec![PROJECT_FILE.into()]
            }
        );
        assert_eq!(head(&m.b), before);
        assert_eq!(read_file(&m.b, PROJECT_FILE), "name = [");

        std::fs::remove_file(m.b.repo_dir().join(PROJECT_FILE)).unwrap();
        assert_eq!(sync_now(&m.b).state, SyncState::Idle);
        assert_eq!(read_file(&m.b, PROJECT_FILE), PROJECT);
    }

    #[test]
    fn overlapping_syncs_take_turns_on_the_sync_lock() {
        let m = machines();
        commit_file(&m.a, MEMORY, "# Acme\n");
        let spawn_sync = || {
            let (paths, global) = (m.a.clone(), m.global_config());
            std::thread::spawn(move || {
                use_global_config(global);
                sync(&paths, Trigger::User).unwrap()
            })
        };

        let held = with_sync_lock(&m.a).unwrap();
        let first = spawn_sync();
        std::thread::sleep(Duration::from_millis(750));
        assert!(!first.is_finished(), "the sync went ahead without the lock");
        assert_ne!(m.remote_head(), head(&m.a));
        drop(held);
        let second = spawn_sync();

        assert_eq!(first.join().unwrap().state, SyncState::Idle);
        assert_eq!(second.join().unwrap().state, SyncState::Idle);
        assert_eq!(m.remote_head(), head(&m.a));
    }

    #[test]
    fn a_hung_remote_times_out_without_blocking_commits() {
        let home = Home::new("");
        home.init();
        home.git(&["remote", "add", "origin", "ssh://tworky.invalid/config.git"]);
        record_remote(
            &home.paths,
            remote_urls_in(&home.paths, &home.paths.repo_dir()).unwrap(),
            Visibility::Unknown,
        )
        .unwrap();
        let (paths, global) = (home.paths.clone(), home.dir.path().join("gitconfig"));
        let started = Instant::now();
        let syncing = std::thread::spawn(move || {
            use_global_config(global);
            SSH_COMMAND.set(Some("sleep 30 #".into()));
            sync_within(&paths, Trigger::User, Duration::from_secs(2)).unwrap()
        });
        while read_record(&home.paths).state != SyncState::Syncing {
            assert!(started.elapsed() < Duration::from_secs(20), "never synced");
            std::thread::sleep(Duration::from_millis(20));
        }

        let locking = Instant::now();
        let guard = with_lock(&home.paths).unwrap();
        assert!(locking.elapsed() < Duration::from_secs(1));
        assert!(!syncing.is_finished());
        let memory = home.write(MEMORY, "# Acme\n");
        assert!(commit(&guard, &home.paths, &[&memory], "Save acme memory").unwrap());
        drop(guard);

        match syncing.join().unwrap().state {
            SyncState::Error { message } => assert!(message.contains("timed out"), "{message}"),
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    /// A home with a config repo of its own, which never synced. Its first commit gets its own
    /// author date because two inits in the same second by the same identity are the same
    /// commit, and the histories would then not be unrelated.
    fn own_home(m: &Machines, name: &str) -> Paths {
        let paths = paths_under(&m.dir.path().join(name));
        init(&with_lock(&paths).unwrap(), &paths).unwrap();
        let seconds: u64 = name.bytes().map(u64::from).sum::<u64>() * 7919;
        let date = format!("@{} +0000", 1_000_000_000 + seconds);
        git_committing(
            &paths,
            &paths.repo_dir(),
            &["commit", "-q", "--amend", "--no-edit", "--date", &date],
        )
        .unwrap();
        paths
    }

    fn merges(paths: &Paths) -> Vec<String> {
        git(
            paths,
            &paths.repo_dir(),
            &["log", "--merges", "--format=%s", "main"],
            LOCAL_TIMEOUT,
        )
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
    }

    #[test]
    fn a_join_merges_two_unrelated_histories_and_later_syncs_keep_the_merge() {
        let m = machines();
        commit_file(&m.a, MEMORY, "# Acme\n");
        sync_now(&m.a);
        let c = own_home(&m, "c");
        commit_file(&c, "tworky/claude.local.md", "Mine\n");
        commit_file(&c, "projects/beta/memory.md", "# Beta\n");

        let joined = set_remote(&c, &m.remote, Visibility::Unknown).unwrap();
        assert_eq!(joined.state, SyncState::Idle, "{joined:?}");
        assert_eq!(read_file(&c, MEMORY), "# Acme\n");
        assert_eq!(read_file(&c, "tworky/claude.local.md"), "Mine\n");
        assert_eq!(read_file(&c, "projects/beta/memory.md"), "# Beta\n");
        assert_eq!(merges(&c), [JOIN_MESSAGE]);
        assert_eq!(m.remote_head(), head(&c));
        assert!(status(&c).unwrap().is_clean());
        let again = set_remote(&c, &m.remote, Visibility::Unknown).unwrap_err();
        assert!(again.to_string().contains("already syncs"), "{again:#}");

        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(head(&m.a), head(&c));
        assert_eq!(read_file(&m.a, "tworky/claude.local.md"), "Mine\n");

        commit_file(&m.a, MEMORY, "# Acme\n- from A\n");
        sync_now(&m.a);
        commit_file(&c, "projects/beta/memory.md", "# Beta\n- from C\n");
        assert_eq!(sync_now(&c).state, SyncState::Idle);
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(head(&m.a), head(&c));
        assert_eq!(read_file(&c, MEMORY), "# Acme\n- from A\n");
        assert_eq!(
            read_file(&m.a, "projects/beta/memory.md"),
            "# Beta\n- from C\n"
        );
        for paths in [&m.a, &c] {
            assert_eq!(merges(paths), [JOIN_MESSAGE], "the join's merge survives");
        }
    }

    #[test]
    fn a_join_conflict_is_resolved_in_repo_sync_and_resumes() {
        let m = machines();
        commit_file(&m.a, MEMORY, "# Acme\n- from A\n");
        sync_now(&m.a);
        let c = own_home(&m, "c");
        commit_file(&c, MEMORY, "# Acme\n- from C\n");
        let before = head(&c);

        let stopped = set_remote(&c, &m.remote, Visibility::Unknown).unwrap();
        assert_eq!(
            stopped.state,
            SyncState::Conflict {
                files: vec![MEMORY.into()]
            }
        );
        assert_eq!(stopped.operation, Some(Operation::Merge));
        assert_eq!(head(&c), before);
        assert_eq!(read_file(&c, MEMORY), "# Acme\n- from C\n");
        assert_eq!(remote_url(&c).as_deref(), Some(m.remote.as_str()));
        commit_file(&c, "tworky/claude.local.md", "C guidance\n");

        std::fs::write(
            c.repo_sync_dir().join(MEMORY),
            "# Acme\n- from A\n- from C\n",
        )
        .unwrap();
        in_sync_dir(&c, &["add", MEMORY]);
        in_sync_dir(
            &c,
            &[
                "-c",
                "user.name=Resolver",
                "-c",
                "user.email=resolver@example.com",
                "commit",
                "-q",
                "--no-edit",
            ],
        );
        let synced = sync_now(&c);
        assert_eq!(synced.state, SyncState::Idle, "{synced:?}");
        assert!(synced.incoming.dropped.is_empty(), "{synced:?}");
        assert_eq!(read_file(&c, MEMORY), "# Acme\n- from A\n- from C\n");
        assert_eq!(read_file(&c, "tworky/claude.local.md"), "C guidance\n");
        assert_eq!(m.remote_head(), head(&c));
        assert_eq!(merges(&c), [JOIN_MESSAGE]);
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(head(&m.a), head(&c));
    }

    #[test]
    fn a_join_the_remote_has_not_taken_is_merged_with_its_new_commits() {
        let m = machines();
        commit_file(&m.a, MEMORY, "# Acme\n- from A\n");
        sync_now(&m.a);
        let c = own_home(&m, "c");
        commit_file(&c, MEMORY, "# Acme\n- from C\n");
        assert!(matches!(
            set_remote(&c, &m.remote, Visibility::Unknown)
                .unwrap()
                .state,
            SyncState::Conflict { .. }
        ));
        let resolved = "# Acme\n- from A\n- from C\n";
        std::fs::write(c.repo_sync_dir().join(MEMORY), resolved).unwrap();
        in_sync_dir(&c, &["add", MEMORY]);
        commit_in_sync_dir(&c, JOIN_MESSAGE);

        commit_file(&m.a, "projects/gamma/memory.md", "# Gamma\n");
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        let remote = m.dir.path().join("remote.git");
        let away = m.dir.path().join("remote.away");
        std::fs::rename(&remote, &away).unwrap();
        let failed = sync_now(&c);
        assert!(
            matches!(failed.state, SyncState::Error { .. }),
            "{failed:?}"
        );
        assert_eq!(read_file(&c, MEMORY), resolved, "main took the join");
        std::fs::rename(&away, &remote).unwrap();

        let synced = sync_now(&c);
        assert_eq!(synced.state, SyncState::Idle, "{synced:?}");
        assert_eq!(read_file(&c, MEMORY), resolved);
        assert_eq!(read_file(&c, "projects/gamma/memory.md"), "# Gamma\n");
        assert_eq!(m.remote_head(), head(&c));
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(head(&m.a), head(&c));
        assert_eq!(read_file(&m.a, MEMORY), resolved);

        commit_file(&m.a, MEMORY, "# Acme\n- from A\n- from C\n- again\n");
        sync_now(&m.a);
        commit_file(&c, "projects/gamma/memory.md", "# Gamma\n- from C\n");
        assert_eq!(sync_now(&c).state, SyncState::Idle);
        assert_eq!(
            read_file(&c, MEMORY),
            "# Acme\n- from A\n- from C\n- again\n"
        );
    }

    #[test]
    fn a_join_whose_push_is_rejected_converges_on_the_next_try() {
        let m = machines();
        commit_file(&m.a, MEMORY, "# Acme\n- from A\n");
        sync_now(&m.a);
        let c = own_home(&m, "c");
        commit_file(&c, MEMORY, "# Acme\n- from C\n");
        assert!(matches!(
            set_remote(&c, &m.remote, Visibility::Unknown)
                .unwrap()
                .state,
            SyncState::Conflict { .. }
        ));
        let resolved = "# Acme\n- from A\n- from C\n";
        std::fs::write(c.repo_sync_dir().join(MEMORY), resolved).unwrap();
        in_sync_dir(&c, &["add", MEMORY]);
        commit_in_sync_dir(&c, JOIN_MESSAGE);
        commit_file(&m.a, "projects/gamma/memory.md", "# Gamma\n");
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);

        let synced = sync_now(&c);
        assert_eq!(synced.state, SyncState::Idle, "{synced:?}");
        assert_eq!(read_file(&c, MEMORY), resolved);
        assert_eq!(read_file(&c, "projects/gamma/memory.md"), "# Gamma\n");
        assert_eq!(m.remote_head(), head(&c));
    }

    #[derive(Default)]
    struct FakeGitHub {
        owners: Vec<String>,
        repos: Mutex<BTreeMap<String, RepoView>>,
        created: Mutex<Vec<String>>,
        /// Where `create_private` makes a bare repo for each new one.
        bare_dir: Option<PathBuf>,
        /// What `ssh -G` would resolve each SSH host to: `None` fails, and a host missing here
        /// resolves to itself, as `ssh -G` does for a host with no config.
        ssh_hosts: BTreeMap<String, Option<String>>,
        /// The arguments of each `ssh -G` asked.
        ssh_asked: Mutex<Vec<String>>,
    }

    /// What the user saw of a remote at `url` Tworky can't check, such as a local bare repo.
    fn saw(url: &str) -> Seen {
        Seen {
            url: url.into(),
            visibility: Visibility::Unknown,
            foreign_owner: false,
        }
    }

    impl FakeGitHub {
        fn new(owners: &[&str]) -> FakeGitHub {
            FakeGitHub {
                owners: owners.iter().map(|o| o.to_string()).collect(),
                ..Default::default()
            }
        }

        fn with_ssh_host(mut self, alias: &str, host: Option<&str>) -> FakeGitHub {
            self.ssh_hosts.insert(alias.into(), host.map(String::from));
            self
        }

        fn with_repo(self, nwo: &str, url: &str, visibility: Visibility) -> FakeGitHub {
            self.repos.lock().unwrap().insert(
                nwo.into(),
                RepoView {
                    url: url.into(),
                    visibility,
                },
            );
            self
        }
    }

    impl GitHub for FakeGitHub {
        fn owners(&self) -> Result<Vec<String>> {
            Ok(self.owners.clone())
        }

        fn ssh_hostname(
            &self,
            host: &str,
            user: Option<&str>,
            port: Option<&str>,
        ) -> Result<String> {
            self.ssh_asked
                .lock()
                .unwrap()
                .push(ssh_config_args(host, user, port).join(" "));
            match self.ssh_hosts.get(host) {
                Some(Some(resolved)) => Ok(resolved.clone()),
                Some(None) => bail!("ssh isn't installed"),
                None => Ok(host.to_string()),
            }
        }

        fn repo(&self, name_with_owner: &str) -> Result<Option<RepoView>> {
            Ok(self.repos.lock().unwrap().get(name_with_owner).cloned())
        }

        fn create_private(&self, name_with_owner: &str) -> Result<()> {
            self.created.lock().unwrap().push(name_with_owner.into());
            let dir = self.bare_dir.as_ref().expect("the test can create repos");
            std::fs::create_dir_all(dir)?;
            let bare = dir.join(name_with_owner.replace('/', "-") + ".git");
            git(
                &paths_under(dir),
                dir,
                &[
                    "init",
                    "-q",
                    "--bare",
                    "-b",
                    "main",
                    &forward_slashes(&bare),
                ],
                LOCAL_TIMEOUT,
            )?;
            self.repos.lock().unwrap().insert(
                name_with_owner.into(),
                RepoView {
                    url: forward_slashes(&bare),
                    visibility: Visibility::Private,
                },
            );
            Ok(())
        }
    }

    #[test]
    fn a_remote_is_checked_for_who_can_see_it_and_who_owns_it() {
        let gh = FakeGitHub::new(&["me", "acme"])
            .with_repo(
                "me/private",
                "https://github.com/me/private",
                Visibility::Private,
            )
            .with_repo("me/open", "https://github.com/me/open", Visibility::Public)
            .with_repo(
                "me/corp",
                "https://github.com/me/corp",
                Visibility::Internal,
            )
            .with_repo("me/odd", "https://github.com/me/odd", Visibility::Unknown)
            .with_repo(
                "acme/config",
                "https://github.com/acme/config",
                Visibility::Private,
            )
            .with_ssh_host("github-personal", Some("github.com"))
            .with_ssh_host("github.com-work", Some("SSH.GitHub.com"))
            .with_ssh_host("no-ssh", None);

        let private = check_remote(&gh, "me/private").unwrap();
        assert_eq!(private.url, "https://github.com/me/private.git");
        assert_eq!(private.visibility, Visibility::Private);
        assert!(private.warnings.is_empty(), "{private:?}");
        assert!(!private.needs_confirmation);

        let public = check_remote(&gh, "git@github.com:me/open.git").unwrap();
        assert_eq!(public.url, "git@github.com:me/open.git");
        assert_eq!(public.visibility, Visibility::Public);
        assert!(public.needs_confirmation);
        assert!(public.warnings[0].contains("public"), "{public:?}");

        for (remote, visibility) in [
            ("https://github.com/me/corp", Visibility::Internal),
            ("https://github.com/me/odd/", Visibility::Unknown),
            ("https://github.com/me/missing.git", Visibility::Unknown),
            ("https://gitlab.com/me/config.git", Visibility::Unknown),
            ("git@gitlab.com:me/open.git", Visibility::Unknown),
            ("C:/elsewhere/config.git", Visibility::Unknown),
            ("/srv/config.git", Visibility::Unknown),
        ] {
            let check = check_remote(&gh, remote).unwrap();
            assert_eq!(check.visibility, visibility, "{remote}");
            assert_eq!(check.warnings.len(), 1, "{remote}: {check:?}");
            assert_eq!(
                check.needs_confirmation,
                visibility == Visibility::Unknown,
                "{remote}"
            );
            assert!(
                !check.warnings[0].contains("isn't a GitHub repo"),
                "{remote}: {check:?}"
            );
        }
        let elsewhere = check_remote(&gh, "https://gitlab.com/me/config.git").unwrap();
        assert!(
            elsewhere.warnings[0].contains("can't check who can see"),
            "{elsewhere:?}"
        );

        for remote in [
            "https://user@github.com/me/open",
            "https://x:TOKEN@github.com/me/open.git",
            "https://www.github.com/me/open",
            "HTTPS://GitHub.com/me/open.GIT",
            "ssh://git@github.com:22/me/open.git",
            "ssh://git@github.com/me/open",
            "git@GitHub.com:me/open.git",
            "me@github.com:me/open",
            "git@github-personal:me/open.git",
            "git@github.com-work:me/open.git",
            "me/open.git",
        ] {
            let check = check_remote(&gh, remote).unwrap();
            assert_eq!(check.visibility, Visibility::Public, "{remote}");
            assert!(check.needs_confirmation, "{remote}");
            assert!(check.warnings[0].contains("public"), "{remote}: {check:?}");
        }
        assert_eq!(
            check_remote(&gh, "me/open.git").unwrap().url,
            "https://github.com/me/open.git"
        );
        let alias = check_remote(&gh, "git@github-personal:me/private.git").unwrap();
        assert_eq!(alias.visibility, Visibility::Private);
        assert!(!alias.needs_confirmation);
        let unknown_alias = check_remote(&gh, "git@github-personal:me/gone.git").unwrap();
        assert_eq!(unknown_alias.visibility, Visibility::Unknown);
        assert!(unknown_alias.needs_confirmation);

        let org = check_remote(&gh, "https://github.com/acme/config").unwrap();
        assert_eq!(org.warnings.len(), 1, "{org:?}");
        assert!(
            org.warnings[0].contains("hooks, services and guidance"),
            "{org:?}"
        );

        let home = Home::new("");
        home.init();
        let e = connect_remote(&home.paths, &gh, "me/open", None).unwrap_err();
        assert!(e.to_string().contains("confirm"), "{e:#}");
        assert_eq!(remote_url(&home.paths), None);
        let e = check_remote(&gh, "me/tworky-config").unwrap_err();
        assert!(e.to_string().contains("debug build"), "{e:#}");
    }

    #[test]
    fn only_hosts_that_resolve_to_github_are_checked_on_github() {
        let gh = FakeGitHub::new(&["me"])
            .with_repo(
                "me/tworky-config-dev",
                "https://github.com/me/tworky-config-dev",
                Visibility::Private,
            )
            .with_ssh_host("nas", Some("nas.local"))
            .with_ssh_host("github-personal", Some("github.com"))
            .with_ssh_host("gh-broken", None);
        let private = check_remote(&gh, "git@github-personal:me/tworky-config-dev.git").unwrap();
        assert_eq!(private.visibility, Visibility::Private, "{private:?}");
        assert!(!private.needs_confirmation);

        for remote in [
            "git@nas:me/tworky-config-dev.git",
            "git@github.mycorp.example:me/tworky-config-dev.git",
            "ssh://git@github.evil.example/me/tworky-config-dev.git",
            "https://github.evil.example/me/tworky-config-dev.git",
            "https://evil#@github.com/me/tworky-config-dev.git",
            "https://evil?@github.com/me/tworky-config-dev.git",
            "https://evil\\@github.com/me/tworky-config-dev.git",
            "https://evil @github.com/me/tworky-config-dev.git",
            "git@-oProxyCommand=x:me/tworky-config-dev.git",
        ] {
            let check = check_remote(&gh, remote).unwrap();
            assert_eq!(check.visibility, Visibility::Unknown, "{remote}");
            assert!(check.needs_confirmation, "{remote}");
            assert!(
                check.warnings[0].contains("can't check who can see"),
                "{remote}: {check:?}"
            );
        }

        let unresolved = check_remote(&gh, "git@gh-broken:me/tworky-config-dev.git").unwrap();
        assert_eq!(unresolved.visibility, Visibility::Unknown);
        assert!(unresolved.needs_confirmation);
        assert!(
            unresolved.warnings[0].contains("gh-broken")
                && unresolved.warnings[0].contains("me/tworky-config-dev"),
            "{unresolved:?}"
        );

        for url in [
            "https://evil#@github.com/me/x",
            "https://a\\b@github.com/me/x",
            "https://github.com/me/x?ref=1",
        ] {
            assert!(split_remote(url).is_none(), "{url}");
        }
        assert_eq!(split_remote("git@nas:me/x").unwrap().host, "nas");
    }

    #[test]
    fn a_confirmation_holds_only_for_what_the_user_saw() {
        let check = |visibility, foreign_owner| RemoteCheck {
            url: "https://github.com/me/config.git".into(),
            visibility,
            warnings: vec!["a warning".into()],
            needs_confirmation: matches!(visibility, Visibility::Public | Visibility::Unknown),
            foreign_owner,
        };
        let seen = |visibility, foreign_owner| {
            Some(Seen {
                url: "https://github.com/me/config.git".into(),
                visibility,
                foreign_owner,
            })
        };
        use Visibility::*;
        for (saw, now) in [
            (Unknown, Public),
            (Private, Public),
            (Internal, Public),
            (Private, Unknown),
            (Private, Internal),
        ] {
            let e = needs_confirmation(&check(now, false), seen(saw, false).as_ref()).unwrap_err();
            let changed = e.downcast_ref::<CheckChanged>().expect("the new check");
            assert_eq!(changed.0.visibility, now);
        }
        for (saw, now) in [
            (Public, Public),
            (Public, Private),
            (Unknown, Internal),
            (Internal, Unknown),
            (Unknown, Unknown),
            (Private, Private),
        ] {
            needs_confirmation(&check(now, false), seen(saw, false).as_ref())
                .unwrap_or_else(|e| panic!("{saw:?} then {now:?}: {e:#}"));
        }
        let e =
            needs_confirmation(&check(Private, true), seen(Private, false).as_ref()).unwrap_err();
        assert!(e.is::<CheckChanged>(), "{e:#}");
        needs_confirmation(&check(Private, true), seen(Private, true).as_ref()).unwrap();
        assert!(needs_confirmation(&check(Public, false), None).is_err());
        needs_confirmation(&check(Private, false), None).unwrap();
        let elsewhere = Seen {
            url: "https://github.com/me/other.git".into(),
            ..seen(Public, true).unwrap()
        };
        let e = needs_confirmation(&check(Private, false), Some(&elsewhere)).unwrap_err();
        assert!(e.is::<CheckChanged>(), "{e:#}");

        let home = Home::new("");
        home.init();
        let gh = FakeGitHub::new(&["me"]).with_repo(
            "me/config",
            "https://github.com/me/config",
            Visibility::Public,
        );
        let e = connect_remote(&home.paths, &gh, "me/config", seen(Unknown, false)).unwrap_err();
        assert!(e.is::<CheckChanged>(), "{e:#}");
        assert_eq!(remote_url(&home.paths), None);
    }

    #[test]
    fn creating_a_remote_makes_a_private_dev_repo_and_pushes_to_it() {
        let home = Home::new("");
        home.init();
        commit_file(&home.paths, MEMORY, "# Acme\n");
        let gh = FakeGitHub {
            bare_dir: Some(home.dir.path().join("github")),
            ..FakeGitHub::new(&["me", "acme"])
        };

        let e = create_remote_named(&home.paths, &gh, None, "tworky-config").unwrap_err();
        assert!(e.to_string().contains("debug build"), "{e:#}");
        assert!(gh.created.lock().unwrap().is_empty());

        let created = create_remote(&home.paths, &gh, Some("acme")).unwrap();
        assert_eq!(*gh.created.lock().unwrap(), ["acme/tworky-config-dev"]);
        assert_eq!(created.sync.state, SyncState::Idle);
        assert_eq!(created.sync.visibility, Some(Visibility::Private));
        assert_eq!(created.check.warnings.len(), 1, "the org warning");
        assert_eq!(
            remote_url(&home.paths).as_deref(),
            Some(created.check.url.as_str())
        );
        let remote = PathBuf::from(&created.check.url);
        assert_eq!(
            rev_parse(&home.paths, &remote, "main").unwrap(),
            head(&home.paths)
        );
        assert_eq!(status(&home.paths).unwrap().ahead_behind, Some((0, 0)));

        let again = create_remote(&home.paths, &gh, None).unwrap_err();
        assert!(again.to_string().contains("already syncs"), "{again:#}");
        assert_eq!(gh.created.lock().unwrap().len(), 1);
    }

    #[test]
    fn shared_configs_are_found_for_the_user_and_their_orgs() {
        let gh = FakeGitHub::new(&["me", "acme", "other"])
            .with_repo(
                "acme/tworky-config-dev",
                "https://github.com/acme/tworky-config-dev",
                Visibility::Private,
            )
            .with_repo(
                "me/tworky-config",
                "https://github.com/me/tworky-config",
                Visibility::Private,
            );
        assert_eq!(
            find_shared_configs(&gh).unwrap(),
            [SharedConfig {
                owner: "acme".into(),
                url: "https://github.com/acme/tworky-config-dev".into(),
                visibility: Visibility::Private,
            }]
        );
    }

    #[test]
    fn a_shared_config_is_cloned_into_an_empty_home() {
        let m = machines();
        commit_file(&m.a, PROJECT_FILE, PROJECT);
        commit_file(&m.a, MEMORY, "# Acme\n");
        sync_now(&m.a);
        let c = paths_under(&m.dir.path().join("c"));
        assert!(awaits_first_run(&c));
        let gh = FakeGitHub::new(&["me"]);

        let set_up = use_shared_config(&c, &gh, &m.remote, Some(saw(&m.remote))).unwrap();
        assert!(!awaits_first_run(&c));
        assert!(!c.repo_tmp_dir().exists());
        assert_eq!(set_up.sync.visibility, Some(Visibility::Unknown));
        assert_eq!(head(&c), head(&m.a));
        assert_eq!(read_file(&c, MEMORY), "# Acme\n");
        let run = |args: &[&str]| git(&c, &c.repo_dir(), args, LOCAL_TIMEOUT).unwrap();
        assert_eq!(run(&["branch", "--show-current"]).trim(), "main");
        assert_eq!(run(&["config", "--local", "rerere.enabled"]).trim(), "true");
        assert_eq!(run(&["config", "--local", "core.autocrlf"]).trim(), "false");
        assert_eq!(
            run(&["config", "--local", "core.hooksPath"]).trim(),
            forward_slashes(&c.repo_hooks_dir())
        );
        let config = crate::config::Config::load_from(c.clone()).unwrap();
        assert!(config.project("acme").is_some());
        let status = status(&c).unwrap();
        assert!(status.is_clean(), "{status:?}");
        assert_eq!(status.ahead_behind, Some((0, 0)));

        commit_file(&c, MEMORY, "# Acme\n- from C\n");
        assert_eq!(sync_now(&c).state, SyncState::Idle);
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(read_file(&m.a, MEMORY), "# Acme\n- from C\n");

        let e = use_shared_config(&c, &gh, &m.remote, Some(saw(&m.remote))).unwrap_err();
        assert!(format!("{e:#}").contains("already exists"), "{e:#}");
    }

    #[test]
    fn a_shared_config_from_an_empty_remote_starts_fresh_and_pushes() {
        let m = machines();
        git(
            &m.a,
            m.dir.path(),
            &["init", "-q", "--bare", "-b", "main", "empty.git"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        let empty = forward_slashes(&m.dir.path().join("empty.git"));
        let c = paths_under(&m.dir.path().join("c"));
        let gh = FakeGitHub::new(&["me"]);

        let e = use_shared_config(&c, &gh, &empty, None).unwrap_err();
        assert!(e.to_string().contains("confirm"), "{e:#}");
        assert!(awaits_first_run(&c));

        FAIL.set(Some("fetch"));
        let e = use_shared_config(&c, &gh, &empty, Some(saw(&empty))).unwrap_err();
        FAIL.set(None);
        assert!(format!("{e:#}").contains("injected"), "{e:#}");
        assert_eq!(log_for_tests(&c), [INIT_MESSAGE]);
        assert_eq!(remote_url(&c), None);

        use_shared_config(&c, &gh, &empty, Some(saw(&empty))).unwrap();
        assert_eq!(log_for_tests(&c), [INIT_MESSAGE]);
        assert_eq!(rev_parse(&c, Path::new(&empty), "main").unwrap(), head(&c));
        assert!(!c.repo_tmp_dir().exists());
    }

    #[test]
    fn a_first_run_that_failed_on_an_empty_remote_takes_the_remote_once_it_has_content() {
        let m = machines();
        let c = paths_under(&m.dir.path().join("c"));
        let gh = FakeGitHub::new(&["me"]);
        let empty = m.dir.path().join("empty.git");
        git(
            &m.a,
            m.dir.path(),
            &["init", "-q", "--bare", "-b", "main", "empty.git"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        FAIL.set(Some("fetch"));
        use_shared_config(
            &c,
            &gh,
            &forward_slashes(&empty),
            Some(saw(&forward_slashes(&empty))),
        )
        .unwrap_err();
        FAIL.set(None);

        commit_file(&m.a, MEMORY, "# Acme\n");
        sync_now(&m.a);
        use_shared_config(&c, &gh, &m.remote, Some(saw(&m.remote))).unwrap();
        assert_eq!(head(&c), head(&m.a));
        assert_eq!(read_file(&c, MEMORY), "# Acme\n");

        commit_file(&c, "tworky/claude.local.md", "Mine\n");
        let e = use_shared_config(&c, &gh, &m.remote, Some(saw(&m.remote))).unwrap_err();
        assert!(format!("{e:#}").contains("already exists"), "{e:#}");
    }

    #[test]
    fn an_interrupted_clone_leaves_no_repo() {
        let m = machines();
        let c = paths_under(&m.dir.path().join("c"));
        let gh = FakeGitHub::new(&["me"]);
        SSH_COMMAND.set(Some("sleep 30 #".into()));
        let started = Instant::now();
        let e = use_shared_config_within(
            &c,
            &gh,
            "ssh://tworky.invalid/config.git",
            Some(saw("ssh://tworky.invalid/config.git")),
            Duration::from_secs(2),
        )
        .unwrap_err();
        SSH_COMMAND.set(None);
        assert!(format!("{e:#}").contains("timed out"), "{e:#}");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(!c.repo_dir().exists());
        assert!(awaits_first_run(&c));

        use_shared_config(&c, &gh, &m.remote, Some(saw(&m.remote))).unwrap();
        assert_eq!(head(&c), head(&m.a));
        assert!(!c.repo_tmp_dir().exists());
    }

    #[test]
    fn a_debug_build_refuses_the_release_remote_name() {
        let home = Home::new("");
        home.init();
        for url in [
            "https://github.com/me/tworky-config.git",
            "git@github.com:me/Tworky-Config",
            "https://github.com/me/tworky-config/",
            "https://github.com/me/Tworky-Config.GIT",
            "C:/remotes/TWORKY-CONFIG.Git",
        ] {
            let e = set_remote(&home.paths, url, Visibility::Unknown).unwrap_err();
            assert!(e.to_string().contains("debug build"), "{url}: {e:#}");
        }
        assert_eq!(remote_url(&home.paths), None);
        let gh = FakeGitHub::new(&["me"]);
        for remote in ["me/tworky-config.git", "me/Tworky-Config.GIT"] {
            let e = check_remote(&gh, remote).unwrap_err();
            assert!(e.to_string().contains("debug build"), "{remote}: {e:#}");
        }
        assert_eq!(
            remote_repo_name("git@github.com:me/tworky-config-dev.git"),
            "tworky-config-dev"
        );
        assert_eq!(remote_repo_name("me/Tworky-Config.GIT"), "Tworky-Config");
        assert_eq!(remote_repo_name("me/config.git.git"), "config.git");
        assert_eq!(
            remote_url_for("me/tworky-config-dev.git").unwrap(),
            "https://github.com/me/tworky-config-dev.git"
        );
    }

    #[test]
    fn credentials_in_a_remote_url_are_never_shown() {
        assert_eq!(
            redact_url("https://x:TOKEN@github.com/me/config.git"),
            "https://***@github.com/me/config.git"
        );
        assert_eq!(
            redact_url("`git clone https://TOKEN@github.com/a/b dir` failed: 'https://u:p@h:1'"),
            "`git clone https://***@github.com/a/b dir` failed: 'https://***@h:1'"
        );
        for plain in [
            "git@github.com:me/config.git",
            "https://github.com/me/config.git",
            "C:/config.git",
        ] {
            assert_eq!(redact_url(plain), plain);
        }

        let gh = FakeGitHub::new(&["me"]);
        let check = check_remote(&gh, "https://x:TOKEN@gitlab.com/me/config.git").unwrap();
        assert!(
            !format!("{check:?}")
                .replace(&check.url, "")
                .contains("TOKEN")
        );
        assert!(!serde_json::to_string(&check).unwrap().contains("TOKEN"));
        let e = needs_confirmation(&check, None).unwrap_err();
        assert!(!format!("{e:#}").contains("TOKEN"), "{e:#}");

        let dir = tempfile::tempdir().unwrap();
        isolate_git_for_tests(dir.path());
        let c = paths_under(dir.path());
        SSH_COMMAND.set(Some("sleep 30 #".into()));
        let e = clone_into_repo(
            &c,
            "ssh://me:TOKEN@tworky.invalid/config.git",
            Duration::from_secs(1),
        )
        .unwrap_err();
        SSH_COMMAND.set(None);
        let message = format!("{e:#}");
        assert!(message.contains("timed out"), "{message}");
        assert!(!message.contains("TOKEN"), "{message}");
        let e = clone_into_repo(
            &c,
            "https://me:TOKEN@127.0.0.1:1/missing.git",
            LOCAL_TIMEOUT,
        )
        .unwrap_err();
        let message = format!("{e:#}");
        assert!(!message.contains("TOKEN"), "{message}");
    }

    #[test]
    fn removing_the_remote_stops_syncing() {
        let m = machines();
        conflicted(&m);
        let e = remove_remote(&m.b).unwrap_err();
        assert!(
            e.to_string().contains(MEMORY) && e.to_string().contains("tw config abort"),
            "{e:#}"
        );
        assert!(remote_url(&m.b).is_some());

        abort(&m.b).unwrap();
        let removed = remove_remote(&m.b).unwrap();
        assert_eq!(removed.state, SyncState::NoRemote);
        assert_eq!(remote_url(&m.b), None);
        assert_eq!(
            progress(&m.b, &m.b.repo_sync_dir()).unwrap(),
            Progress::None
        );
        assert_eq!(sync_now(&m.b).state, SyncState::NoRemote);
        assert!(remove_remote(&m.b).is_err());
    }

    #[test]
    fn an_aborted_change_in_repo_sync_does_not_block_setting_the_remote_again() {
        let m = machines();
        let stray = m.b.repo_sync_dir().join("projects/stray/memory.md");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "# Stray\n").unwrap();
        let e = remove_remote(&m.b).unwrap_err();
        assert!(e.to_string().contains("projects/stray/memory.md"), "{e:#}");

        abort(&m.b).unwrap();
        remove_remote(&m.b).unwrap();
        assert!(!stray.exists());
        let git_config = |key: &str| git(&m.b, &m.b.repo_dir(), &["config", key], LOCAL_TIMEOUT);
        assert!(git_config("branch.main.remote").is_err());

        let again = set_remote(&m.b, &m.remote, Visibility::Unknown).unwrap();
        assert_eq!(again.state, SyncState::Idle, "{again:?}");
        assert!(!again.paused);
    }

    #[test]
    fn a_remote_that_fails_to_sync_is_not_kept() {
        let m = machines();
        let c = own_home(&m, "c");
        commit_file(&c, "tworky/claude.local.md", "Mine\n");
        let stray = sync_worktree_dir(&c).unwrap().join(MEMORY);
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "# Half done\n").unwrap();

        let e = set_remote(&c, &m.remote, Visibility::Unknown).unwrap_err();
        assert!(e.to_string().contains("uncommitted"), "{e:#}");
        assert_eq!(remote_url(&c), None);
        let run = |args: &[&str]| git(&c, &c.repo_dir(), args, LOCAL_TIMEOUT);
        assert!(run(&["config", "branch.main.remote"]).is_err());
        assert!(run(&["config", "branch.main.merge"]).is_err());
        assert_eq!(sync_status(&c).unwrap().state, SyncState::NoRemote);

        std::fs::remove_file(&stray).unwrap();
        let joined = set_remote(&c, &m.remote, Visibility::Unknown).unwrap();
        assert_eq!(joined.state, SyncState::Idle, "{joined:?}");
        assert_eq!(m.remote_head(), head(&c));
    }

    #[test]
    fn a_remote_whose_history_was_taken_stays_when_the_sync_stops() {
        let m = machines();
        commit_file(&m.a, MEMORY, "# Acme\n");
        sync_now(&m.a);
        let c = own_home(&m, "c");
        let stray = sync_worktree_dir(&c)
            .unwrap()
            .join("projects/stray/memory.md");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "# Half done\n").unwrap();

        let connected = set_remote(&c, &m.remote, Visibility::Unknown).unwrap();
        assert!(
            matches!(connected.state, SyncState::HeldInSyncWorktree { .. }),
            "{connected:?}"
        );
        assert_eq!(remote_url(&c).as_deref(), Some(m.remote.as_str()));
        assert_eq!(head(&c), head(&m.a));
        assert_eq!(connected.incoming.guidance, [MEMORY]);
    }

    #[test]
    fn a_sync_record_from_an_older_repo_is_forgotten() {
        let m = machines();
        let stale = |paths: &Paths| {
            update_record(paths, |r| {
                r.state = SyncState::Conflict {
                    files: vec![MEMORY.into()],
                };
                r.base = Some("1".repeat(40));
                r.upstream = Some("2".repeat(40));
            })
            .unwrap();
        };

        stale(&m.b);
        let synced = sync_now(&m.b);
        assert_eq!(synced.state, SyncState::Idle, "{synced:?}");
        assert_eq!(synced.base, None);

        let c = paths_under(&m.dir.path().join("c"));
        stale(&c);
        init(&with_lock(&c).unwrap(), &c).unwrap();
        assert_eq!(read_record(&c), SyncRecord::default());

        let d = paths_under(&m.dir.path().join("d"));
        stale(&d);
        use_shared_config(
            &d,
            &FakeGitHub::new(&["me"]),
            &m.remote,
            Some(saw(&m.remote)),
        )
        .unwrap();
        let record = read_record(&d);
        assert_eq!((record.base, record.upstream), (None, None));
        assert_eq!(record.state, SyncState::Idle);
    }

    #[test]
    fn a_remote_added_with_plain_git_is_never_fetched_or_pushed() {
        let m = machines();
        let c = own_home(&m, "c");
        commit_file(&c, MEMORY, "# Secret\n");
        let run = |args: &[&str]| git(&c, &c.repo_dir(), args, LOCAL_TIMEOUT).unwrap();
        run(&["remote", "add", "origin", &m.remote]);
        let before = m.remote_head();

        let unconfirmed = SyncState::UnconfirmedRemote {
            url: m.remote.clone(),
        };
        assert_eq!(sync_status(&c).unwrap().state, unconfirmed);
        for trigger in [Trigger::Background, Trigger::User] {
            assert_eq!(sync(&c, trigger).unwrap().state, unconfirmed);
        }
        assert_eq!(m.remote_head(), before, "nothing was pushed");
        assert!(
            git(
                &c,
                &c.repo_dir(),
                &["rev-parse", "refs/remotes/origin/main"],
                LOCAL_TIMEOUT
            )
            .is_err(),
            "nothing was fetched"
        );

        let gh = FakeGitHub::new(&["me"]);
        let e = confirm_remote(&c, &gh, &saw("C:/elsewhere/config.git")).unwrap_err();
        let changed = e.downcast_ref::<CheckChanged>().expect("the fresh check");
        assert_eq!(changed.0.url, m.remote);
        assert_eq!(m.remote_head(), before, "nothing was pushed");
        let connected = confirm_remote(&c, &gh, &saw(&m.remote)).unwrap();
        assert_eq!(connected.sync.state, SyncState::Idle, "{connected:?}");
        assert_eq!(connected.sync.visibility, Some(Visibility::Unknown));
        assert_eq!(m.remote_head(), head(&c));
    }

    #[test]
    fn a_push_url_or_a_new_url_the_user_did_not_connect_stops_syncing() {
        let m = machines();
        git(
            &m.a,
            m.dir.path(),
            &["init", "-q", "--bare", "-b", "main", "other.git"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        let other = forward_slashes(&m.dir.path().join("other.git"));
        let run = |args: &[&str]| git(&m.a, &m.a.repo_dir(), args, LOCAL_TIMEOUT).unwrap();
        commit_file(&m.a, MEMORY, "# Acme\n- from A\n");

        run(&["config", "remote.origin.pushurl", &other]);
        let stopped = sync_now(&m.a);
        assert_eq!(
            stopped.state,
            SyncState::UnconfirmedRemote { url: other.clone() }
        );
        assert!(
            git(
                &m.a,
                Path::new(&other),
                &["rev-parse", "main"],
                LOCAL_TIMEOUT
            )
            .is_err(),
            "nothing was pushed to the other repo"
        );
        let gh = FakeGitHub::new(&["me"]);
        let e = confirm_remote(&m.a, &gh, &saw(&m.remote)).unwrap_err();
        assert!(e.to_string().contains(&format!("push to {other}")), "{e:#}");

        run(&["config", "--unset", "remote.origin.pushurl"]);
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(m.remote_head(), head(&m.a));

        run(&["remote", "set-url", "origin", &other]);
        commit_file(&m.a, MEMORY, "# Acme\n- more\n");
        assert_eq!(
            sync_now(&m.a).state,
            SyncState::UnconfirmedRemote { url: other.clone() }
        );
        assert!(
            git(
                &m.a,
                Path::new(&other),
                &["rev-parse", "main"],
                LOCAL_TIMEOUT
            )
            .is_err()
        );

        remove_remote(&m.a).unwrap();
        connect_remote(&m.a, &gh, &m.remote, Some(saw(&m.remote))).unwrap();
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(m.remote_head(), head(&m.a));
    }

    fn bare(m: &Machines, name: &str) -> String {
        git(
            &m.a,
            m.dir.path(),
            &["init", "-q", "--bare", "-b", "main", name],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        forward_slashes(&m.dir.path().join(name))
    }

    #[test]
    fn git_config_that_sends_the_config_elsewhere_is_refused() {
        let m = machines();
        let other = bare(&m, "other.git");
        let before = m.remote_head();
        std::fs::write(
            m.global_config(),
            format!("[remote \"origin\"]\n\tpushurl = {other}\n"),
        )
        .unwrap();
        let gh = FakeGitHub::new(&["me"]);

        let c = own_home(&m, "c");
        commit_file(&c, MEMORY, "# Secret\n");
        let e = connect_remote(&c, &gh, &m.remote, Some(saw(&m.remote))).unwrap_err();
        assert!(e.to_string().contains(&other), "{e:#}");
        assert_eq!(remote_url(&c), None);
        assert_eq!(sync_status(&c).unwrap().remote, None);

        let d = paths_under(&m.dir.path().join("d"));
        let e = use_shared_config(&d, &gh, &m.remote, Some(saw(&m.remote))).unwrap_err();
        assert!(format!("{e:#}").contains(&other), "{e:#}");
        assert!(!d.repo_dir().exists());
        assert!(awaits_first_run(&d));

        assert!(
            git(
                &m.a,
                Path::new(&other),
                &["rev-parse", "main"],
                LOCAL_TIMEOUT
            )
            .is_err(),
            "nothing was pushed to the other repo"
        );
        assert_eq!(m.remote_head(), before);

        std::fs::write(
            m.global_config(),
            "[url \"https://gitlab.com/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();
        let gh = gh.with_repo(
            "me/config",
            "https://github.com/me/config",
            Visibility::Private,
        );
        let e = connect_remote(&c, &gh, "me/config", None).unwrap_err();
        assert!(
            e.to_string()
                .contains("fetch from https://gitlab.com/me/config.git"),
            "{e:#}"
        );
        assert_eq!(remote_url(&c), None);
    }

    /// Serves `owner/name` over SSH from the bare repo `<dir>/owner/name`, whatever the host.
    fn serve_github_over_ssh(dir: &Path) {
        SSH_COMMAND.set(Some(format!(
            "sh -c 'cd \"$0\" && eval \"$2\"' '{}'",
            forward_slashes(dir)
        )));
    }

    #[test]
    fn a_push_insteadof_to_the_same_github_repo_over_ssh_is_accepted() {
        let m = machines();
        let github = m.dir.path().join("github");
        std::fs::create_dir_all(github.join("me")).unwrap();
        bare(&m, "github/me/config.git");
        std::fs::write(
            m.global_config(),
            "[url \"git@github.com:\"]\n\tinsteadOf = https://github.com/\n\
             [url \"git@GitHub.com:\"]\n\tpushInsteadOf = https://github.com/\n",
        )
        .unwrap();
        serve_github_over_ssh(&github);
        let gh = FakeGitHub::new(&["me"]).with_repo(
            "me/config",
            "https://github.com/me/config",
            Visibility::Private,
        );
        let expected = RemoteUrls {
            fetch: vec!["git@github.com:me/config.git".into()],
            push: vec!["git@GitHub.com:me/config.git".into()],
        };
        let served =
            |paths: &Paths| rev_parse(paths, &github.join("me/config.git"), "main").unwrap();

        let c = own_home(&m, "c");
        commit_file(&c, MEMORY, "# From C\n");
        let connected = connect_remote(&c, &gh, "me/config", None).unwrap();
        assert_eq!(connected.sync.state, SyncState::Idle, "{connected:?}");
        assert_eq!(connected.sync.remote.as_ref(), Some(&expected));
        assert_eq!(served(&c), head(&c));

        let d = own_home(&m, "d");
        commit_file(&d, "tworky/claude.local.md", "Mine\n");
        git(
            &d,
            &d.repo_dir(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/me/config.git",
            ],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        let check = check_current_remote(&d, &gh).unwrap();
        assert_eq!(check.url, "git@github.com:me/config.git");
        let connected = confirm_remote(&d, &gh, &Seen::of(&check)).unwrap();
        SSH_COMMAND.set(None);
        assert_eq!(connected.sync.state, SyncState::Idle, "{connected:?}");
        assert_eq!(connected.sync.remote.as_ref(), Some(&expected));
        assert_eq!(served(&d), head(&d));
    }

    #[test]
    fn the_same_github_repo_is_matched_across_url_forms() {
        for url in [
            "https://github.com/me/config.git",
            "https://***@github.com/Me/Config",
            "ssh://git@github.com/me/config.git",
            "ssh://git@GITHUB.com:22/me/config.GIT",
            "git@github.com:me/config.git",
            "github.com:me/config",
        ] {
            assert!(same_repo(url, "https://github.com/me/config"), "{url}");
        }
        for url in [
            "https://github.com/me/other.git",
            "https://github.com/you/config.git",
            "http://github.com/me/config.git",
            "file://github.com/me/config.git",
            "git@github-personal:me/config.git",
            "https://gitlab.com/me/config.git",
            "C:/github.com/me/config.git",
        ] {
            assert!(!same_repo(url, "https://github.com/me/config"), "{url}");
        }
        assert!(same_repo("C:/config.git", "C:/config.git"));
        assert!(!same_repo("C:/config.git", "c:/config.git"));
    }

    #[test]
    fn a_worktree_local_origin_in_repo_sync_is_cleared_by_removing_the_remote() {
        let m = machines();
        let other = bare(&m, "other.git");
        let before = m.remote_head();
        git(
            &m.a,
            &m.a.repo_dir(),
            &["config", "extensions.worktreeConfig", "true"],
            LOCAL_TIMEOUT,
        )
        .unwrap();
        in_sync_dir(&m.a, &["config", "--worktree", "remote.origin.url", &other]);
        commit_file(&m.a, MEMORY, "# Acme\n");
        assert_eq!(
            sync_now(&m.a).state,
            SyncState::UnconfirmedRemote { url: other.clone() }
        );

        let gh = FakeGitHub::new(&["me"]);
        let e = confirm_remote(&m.a, &gh, &saw(&m.remote)).unwrap_err();
        assert!(e.to_string().contains(&other), "{e:#}");
        assert_eq!(m.remote_head(), before);

        remove_remote(&m.a).unwrap();
        assert!(
            git(
                &m.a,
                &m.a.repo_sync_dir(),
                &["config", "--worktree", "--get-regexp", "^remote\\."],
                LOCAL_TIMEOUT
            )
            .is_err()
        );
        connect_remote(&m.a, &gh, &m.remote, Some(saw(&m.remote))).unwrap();
        assert_eq!(sync_now(&m.a).state, SyncState::Idle);
        assert_eq!(m.remote_head(), head(&m.a));
        assert!(
            git(
                &m.a,
                Path::new(&other),
                &["rev-parse", "main"],
                LOCAL_TIMEOUT
            )
            .is_err()
        );
    }

    #[test]
    fn ssh_config_is_resolved_as_the_user_and_port_git_connects_with() {
        assert_eq!(
            ssh_config_args("alias", Some("git"), Some("2222")),
            ["-G", "-l", "git", "-p", "2222", "--", "alias"]
        );
        assert_eq!(ssh_config_args("alias", None, None), ["-G", "--", "alias"]);

        let gh = FakeGitHub::new(&["me"]);
        for remote in [
            "ssh://me:secret@work:2222/me/config.git",
            "git@personal:me/config.git",
            "ssh://plain/me/config.git",
            "ssh://-oProxyCommand=x@evil/me/config.git",
            "ssh://a+b@evil/me/config.git",
        ] {
            check_remote(&gh, remote).unwrap();
        }
        assert_eq!(
            *gh.ssh_asked.lock().unwrap(),
            [
                "-G -l me -p 2222 -- work",
                "-G -l git -- personal",
                "-G -- plain",
            ]
        );
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
