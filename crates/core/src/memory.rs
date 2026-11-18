use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Paths;

pub const DEFAULT_SECTION: &str = "Learned";

/// Hook output past this is truncated before it reaches Claude.
pub const SIZE_WARNING: usize = 8 * 1024;

const CONTEXT_LINES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChangeKind {
    Add { section: String, text: String },
    Edit { old: String, new: String },
    Remove { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    #[serde(flatten)]
    pub kind: ChangeKind,
    pub author: String,
    /// UTC date the change was filed, as YYYY-MM-DD.
    pub date: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineTag {
    Ctx,
    Del,
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkLine {
    pub tag: LineTag,
    /// The line without its line ending.
    pub text: String,
    /// Byte range of `text` that differs from the line it replaces or is replaced by.
    pub changed: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub hunk: Hunk,
    /// `hash` of the memory the hunk was built from.
    pub base: String,
    /// The `## ` heading, without the `## `, that the change lands under.
    pub section: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PreviewResult {
    Ready(Preview),
    Stale { reason: String },
}

impl From<Result<Preview>> for PreviewResult {
    fn from(result: Result<Preview>) -> Self {
        match result {
            Ok(preview) => Self::Ready(preview),
            Err(e) => Self::Stale {
                reason: format!("{e:#}"),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingChange {
    pub id: String,
    pub change: Change,
    pub preview: PreviewResult,
}

pub fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// FNV-1a 64-bit, as 16 hex characters. Detects changes; it isn't meant to resist attack.
pub fn hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Files a change for the user to approve, after checking it applies to the current memory.
/// Returns the change's id.
pub fn propose(paths: &Paths, project: &str, kind: ChangeKind, author: &str) -> Result<String> {
    let change = Change {
        kind,
        author: author.to_string(),
        date: today(),
    };
    apply(&read(&paths.memory_file(project))?, &change)?;
    let dir = paths.memory_changes_dir(project);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let json = serde_json::to_vec_pretty(&change)?;
    loop {
        let id = new_id();
        if write_new(&dir, &id, &json)? {
            return Ok(id);
        }
    }
}

/// Writes `<id>.json` atomically, never replacing an existing file. False when the id is taken.
fn write_new(dir: &Path, id: &str, bytes: &[u8]) -> Result<bool> {
    let file = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!("{id}.json.tmp"));
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
    };
    let written = f.write_all(bytes).and_then(|()| f.sync_all());
    drop(f);
    let linked = written.and_then(|()| std::fs::hard_link(&tmp, &file));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("writing {}", file.display())),
    }
}

/// The project's pending changes in filing order, each with its preview against the current memory.
pub fn pending(paths: &Paths, project: &str) -> Result<Vec<PendingChange>> {
    let dir = paths.memory_changes_dir(project);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let current = read(&paths.memory_file(project))?;
    let mut out = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let Some(id) = change_id(&path) else {
            continue;
        };
        if !is_change_id(&id) {
            eprintln!(
                "memory changes: skipping {}: not a change id",
                path.display()
            );
            continue;
        }
        match read_change(&path) {
            Ok(change) => out.push(PendingChange {
                preview: preview(&current, &change).into(),
                id,
                change,
            }),
            Err(e) => eprintln!("memory changes: skipping {}: {e:#}", path.display()),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Applies the selected changes in id order as one write, or none of them.
pub fn approve(paths: &Paths, project: &str, ids: &[String], base: &str) -> Result<()> {
    let mut ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        bail!("no changes selected");
    }
    let dir = paths.memory_changes_dir(project);
    let files = ids
        .iter()
        .map(|id| change_file(&dir, id))
        .collect::<Result<Vec<_>>>()?;
    locked_with(
        &paths.memory_file(project),
        |current| {
            let mut claimed = Vec::new();
            for (id, file) in ids.iter().zip(&files) {
                let bytes = match std::fs::read(file) {
                    Ok(bytes) => bytes,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        bail!("change {id} was already handled")
                    }
                    Err(e) => {
                        return Err(e).with_context(|| format!("reading {}", file.display()));
                    }
                };
                let change: Change = serde_json::from_slice(&bytes)
                    .with_context(|| format!("reading change {id}"))?;
                claimed.push((id, file, bytes, change));
            }
            if hash(current) != base {
                bail!("Memory changed since this diff was shown; review it again");
            }
            let mut next = current.to_string();
            for (id, _, _, change) in &claimed {
                next = apply(&next, change).with_context(|| format!("change {id}"))?;
            }
            let mut deleted: Vec<(PathBuf, Vec<u8>)> = Vec::new();
            for (_, file, bytes, _) in claimed {
                if let Err(e) = std::fs::remove_file(file) {
                    let e = anyhow::Error::from(e).context(format!("deleting {}", file.display()));
                    return Err(with_restore(e, &deleted));
                }
                deleted.push((file.clone(), bytes));
            }
            Ok((next, deleted))
        },
        |deleted, e| with_restore(e, &deleted),
    )
}

/// Deletes the selected change files, skipping any already gone.
pub fn reject(paths: &Paths, project: &str, ids: &[String]) -> Result<()> {
    let dir = paths.memory_changes_dir(project);
    let files = ids
        .iter()
        .map(|id| change_file(&dir, id))
        .collect::<Result<Vec<_>>>()?;
    with_lock(&paths.memory_file(project), || {
        for file in &files {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("deleting {}", file.display())),
            }
        }
        Ok(())
    })
}

/// Replaces the memory, unless it changed since `base` was taken.
pub fn save(path: &Path, base: &str, text: &str) -> Result<()> {
    locked(path, |current| {
        if hash(current) != base {
            bail!("Memory changed since you started editing");
        }
        Ok(text.to_string())
    })
}

pub fn apply(current: &str, change: &Change) -> Result<String> {
    match &change.kind {
        ChangeKind::Add { section, text } => {
            if section.trim().is_empty() {
                bail!("the section name is empty");
            }
            if section.contains(['\n', '\r']) {
                bail!("the section name must be on one line");
            }
            let text = one_line(text);
            let text = text.trim();
            if text.is_empty() {
                bail!("nothing to add");
            }
            Ok(insert_entry(current, section, &entry(text, change)))
        }
        ChangeKind::Edit { old, new } => apply_edit(current, old, new, change),
        ChangeKind::Remove { text } => apply_remove(current, text),
    }
}

/// Applies the change and returns the one region it touches, with context.
pub fn preview(current: &str, change: &Change) -> Result<Preview> {
    let next = apply(current, change)?;
    let section = match &change.kind {
        ChangeKind::Add { section, .. } => Some(
            find_section(current, section)
                .map_or(section.trim(), |i| heading_name(split_lines(current)[i])),
        ),
        ChangeKind::Edit { .. } | ChangeKind::Remove { .. } => {
            let old = split_lines(current);
            let first_changed = common_prefix(&old, &split_lines(&next));
            old[..first_changed]
                .iter()
                .rev()
                .find(|l| is_heading(l))
                .map(|l| heading_name(l))
        }
    };
    Ok(Preview {
        hunk: hunk(current, &next),
        base: hash(current),
        section: section.map(String::from),
    })
}

fn with_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let dir = path.parent().context("memory path has no folder")?;
    std::fs::create_dir_all(dir)?;
    let lock_path = path.with_extension("md.lock");
    let lock = std::fs::File::create(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    let out = f()?;
    lock.unlock()?;
    Ok(out)
}

fn locked(path: &Path, edit: impl FnOnce(&str) -> Result<String>) -> Result<()> {
    locked_with(path, |current| Ok((edit(current)?, ())), |(), e| e)
}

/// Under the lock, `prepare` turns the current text into the next one plus whatever `undo` needs
/// to put things back when the write fails.
fn locked_with<S>(
    path: &Path,
    prepare: impl FnOnce(&str) -> Result<(String, S)>,
    undo: impl FnOnce(S, anyhow::Error) -> anyhow::Error,
) -> Result<()> {
    with_lock(path, || {
        let (next, state) = prepare(&read(path)?)?;
        write_atomic(path, next.as_bytes()).map_err(|e| undo(state, e))
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.file_name().context("no file name")?.to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

fn with_restore(e: anyhow::Error, files: &[(PathBuf, Vec<u8>)]) -> anyhow::Error {
    let failures: Vec<String> = files
        .iter()
        .filter_map(|(file, bytes)| {
            write_atomic(file, bytes)
                .err()
                .map(|r| format!("{}: {r:#}", file.display()))
        })
        .collect();
    if failures.is_empty() {
        e
    } else {
        e.context(format!(
            "restoring change files also failed: {}",
            failures.join("; ")
        ))
    }
}

fn is_change_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn change_file(dir: &Path, id: &str) -> Result<PathBuf> {
    if !is_change_id(id) {
        bail!("not a change id: {id}");
    }
    Ok(dir.join(format!("{id}.json")))
}

fn change_id(path: &Path) -> Option<String> {
    if path.extension()? != "json" {
        return None;
    }
    Some(path.file_stem()?.to_str()?.to_string())
}

fn read_change(path: &Path) -> Result<Change> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn new_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let bytes = uuid::Uuid::new_v4().into_bytes();
    format!("{millis}-{:02x}{:02x}", bytes[0], bytes[1])
}

fn entry(text: &str, change: &Change) -> String {
    format!("- {text} _({}, {})_", change.author, change.date)
}

fn one_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

/// Lines with their endings, so joining them gives back the exact text.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn content(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn is_blank(line: &str) -> bool {
    content(line).trim().is_empty()
}

fn is_heading(line: &str) -> bool {
    line.starts_with("## ")
}

fn heading_name(line: &str) -> &str {
    content(line)[3..].trim()
}

fn find_section(current: &str, section: &str) -> Option<usize> {
    split_lines(current).iter().position(|l| {
        let l = content(l);
        is_heading(l) && heading_name(l).eq_ignore_ascii_case(section.trim())
    })
}

fn line_ending(text: &str) -> &'static str {
    let (mut crlf, mut lf) = (0, 0);
    for line in split_lines(text) {
        if line.ends_with("\r\n") {
            crlf += 1;
        } else if line.ends_with('\n') {
            lf += 1;
        }
    }
    if crlf > lf { "\r\n" } else { "\n" }
}

fn with_ending(text: &str, ending: &str) -> String {
    let text = text.replace("\r\n", "\n");
    if ending == "\n" {
        text
    } else {
        text.replace('\n', ending)
    }
}

fn strip_attribution(line: &str) -> &str {
    let trimmed = line.trim_end();
    if trimmed.ends_with(")_")
        && let Some(i) = trimmed.rfind(" _(")
    {
        return &trimmed[..i];
    }
    trimmed
}

fn is_entry(line: &str) -> bool {
    line.starts_with("- ") && strip_attribution(line).len() < line.trim_end().len()
}

/// At the end of the section's entries, or in a new section at the end of the file.
fn insert_entry(current: &str, section: &str, entry: &str) -> String {
    let ending = line_ending(current);
    let mut lines: Vec<String> = split_lines(current).into_iter().map(String::from).collect();
    let heading = find_section(current, section);
    let mut new = Vec::new();
    let after = match heading {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|l| is_heading(content(l)))
                .map_or(lines.len(), |i| start + 1 + i);
            let last = (start..end)
                .rev()
                .find(|&i| !is_blank(&lines[i]))
                .unwrap_or(start);
            if last == start {
                new.push(String::new());
            }
            new.push(entry.to_string());
            if lines.get(last + 1).is_some_and(|l| is_heading(content(l))) {
                new.push(String::new());
            }
            Some(last)
        }
        None => {
            let last = lines.iter().rposition(|l| !is_blank(l));
            if last.is_some() {
                new.push(String::new());
            }
            new.extend([
                format!("## {}", section.trim()),
                String::new(),
                entry.to_string(),
            ]);
            last
        }
    };
    if let Some(i) = after
        && !lines[i].ends_with('\n')
    {
        lines[i].push_str(ending);
    }
    let at = after.map_or(0, |i| i + 1);
    lines.splice(at..at, new.into_iter().map(|l| l + ending));
    lines.concat()
}

/// Byte offset of the one occurrence of `needle`, counting overlapping occurrences.
fn find_once(haystack: &str, needle: &str) -> Result<usize> {
    let step = needle.chars().next().map_or(1, char::len_utf8);
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(i) = haystack[from..].find(needle) {
        found.push(from + i);
        from += i + step;
    }
    match found[..] {
        [] => bail!("not found in the memory"),
        [at] => Ok(at),
        _ => bail!(
            "matches {} places in the memory; give more of the text so it matches once",
            found.len()
        ),
    }
}

fn apply_edit(current: &str, old: &str, new: &str, change: &Change) -> Result<String> {
    if old.trim().is_empty() {
        bail!("the text to replace is empty");
    }
    let new = if !old.ends_with('\n') {
        match new.strip_suffix('\n') {
            Some(n) => n.strip_suffix('\r').unwrap_or(n).to_string(),
            None => new.to_string(),
        }
    } else if new.is_empty() || new.ends_with('\n') {
        new.to_string()
    } else {
        format!("{new}\n")
    };
    let ending = line_ending(current);
    let old = with_ending(old, ending);
    let at = find_once(current, &old)?;
    let end = at + old.len();
    let line_start = current[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = current[at..].find('\n').map_or(current.len(), |i| at + i);
    let line = content(&current[line_start..line_end]);
    let content_end = line_start + line.len();
    if end <= content_end && is_entry(line) {
        let edited = format!(
            "{}{}{}",
            &current[line_start..at],
            one_line(&new),
            &current[end..content_end]
        );
        let body = strip_attribution(&edited);
        let body = body.strip_prefix('-').unwrap_or(body).trim();
        if body.is_empty() {
            bail!("that leaves the entry empty; use `tw memory remove` to delete it");
        }
        return Ok(format!(
            "{}{}{}",
            &current[..line_start],
            entry(body, change),
            &current[content_end..]
        ));
    }
    let next = format!(
        "{}{}{}",
        &current[..at],
        with_ending(&new, ending),
        &current[end..]
    );
    if next == current {
        bail!("the new text is the same as the old");
    }
    Ok(next)
}

fn apply_remove(current: &str, text: &str) -> Result<String> {
    if text.trim().is_empty() {
        bail!("nothing to remove");
    }
    if text.contains(['\n', '\r']) {
        bail!("the text to remove must be on one line");
    }
    let at = find_once(current, text)?;
    let index = current[..at].matches('\n').count();
    let mut lines = split_lines(current);
    if is_heading(content(lines[index])) {
        bail!("won't remove a heading");
    }
    lines.remove(index);
    while index > 0 && index < lines.len() && is_blank(lines[index - 1]) && is_blank(lines[index]) {
        lines.remove(index);
    }
    Ok(lines.concat())
}

/// Lines are compared without their endings, so a last line that only gains one stays context.
fn common_prefix(old: &[&str], new: &[&str]) -> usize {
    old.iter()
        .zip(new)
        .take_while(|(a, b)| content(a) == content(b))
        .count()
}

fn hunk(before: &str, after: &str) -> Hunk {
    let old = split_lines(before);
    let new = split_lines(after);
    let prefix = common_prefix(&old, &new);
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(old.len().min(new.len()) - prefix)
        .take_while(|(a, b)| content(a) == content(b))
        .count();
    let deleted = &old[prefix..old.len() - suffix];
    let added = &new[prefix..new.len() - suffix];
    let ctx = |l: &&str| HunkLine {
        tag: LineTag::Ctx,
        text: content(l).to_string(),
        changed: None,
    };
    let changed = |tag, lines: &[&str], others: &[&str]| -> Vec<HunkLine> {
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| HunkLine {
                tag,
                text: content(l).to_string(),
                changed: others
                    .get(i)
                    .and_then(|other| changed_span(content(l), content(other))),
            })
            .collect()
    };
    let tail = old.len() - suffix;
    let mut lines: Vec<HunkLine> = old[prefix.saturating_sub(CONTEXT_LINES)..prefix]
        .iter()
        .map(ctx)
        .collect();
    lines.extend(changed(LineTag::Del, deleted, added));
    lines.extend(changed(LineTag::Add, added, deleted));
    lines.extend(
        old[tail..(tail + CONTEXT_LINES).min(old.len())]
            .iter()
            .map(ctx),
    );
    Hunk { lines }
}

/// The middle of `line` left once the prefix and suffix it shares with `other` are cut.
fn changed_span(line: &str, other: &str) -> Option<Span> {
    if line == other {
        return None;
    }
    let prefix: usize = line
        .chars()
        .zip(other.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let suffix: usize = line[prefix..]
        .chars()
        .rev()
        .zip(other[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    Some(Span {
        start: prefix,
        end: line.len() - suffix,
    })
}

/// UTC date as YYYY-MM-DD.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    civil_date((secs / 86_400) as i64)
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "# Acme\n\n## Test database\n\nClone it.\n\n## Learned\n\n- old _(acme-1, 2026-01-01)_\n\n## Starting a story\n\nSubtasks.\n";

    fn change(kind: ChangeKind) -> Change {
        Change {
            kind,
            author: "w2".into(),
            date: "2026-02-02".into(),
        }
    }

    fn adding(section: &str, text: &str) -> Change {
        change(ChangeKind::Add {
            section: section.into(),
            text: text.into(),
        })
    }

    fn editing(old: &str, new: &str) -> Change {
        change(ChangeKind::Edit {
            old: old.into(),
            new: new.into(),
        })
    }

    fn removing(text: &str) -> Change {
        change(ChangeKind::Remove { text: text.into() })
    }

    fn error(current: &str, change: &Change) -> String {
        format!("{:#}", apply(current, change).unwrap_err())
    }

    fn paths(dir: &Path) -> Paths {
        Paths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
        }
    }

    fn seeded(text: &str) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let file = paths.memory_file("p");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, text).unwrap();
        (dir, paths)
    }

    fn memory(paths: &Paths) -> String {
        read(&paths.memory_file("p")).unwrap()
    }

    fn change_files(paths: &Paths) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(paths.memory_changes_dir("p"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hashes_with_fnv1a() {
        assert_eq!(hash(""), "cbf29ce484222325");
        assert_eq!(hash("a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn adds_to_the_end_of_an_existing_section() {
        assert_eq!(
            apply(SEED, &adding("learned", "new")).unwrap(),
            "# Acme\n\n## Test database\n\nClone it.\n\n## Learned\n\n- old _(acme-1, 2026-01-01)_\n- new _(w2, 2026-02-02)_\n\n## Starting a story\n\nSubtasks.\n"
        );
    }

    #[test]
    fn adds_a_missing_section_and_to_an_empty_file() {
        assert_eq!(
            apply("# Acme\n", &adding("Learned", "a")).unwrap(),
            "# Acme\n\n## Learned\n\n- a _(w2, 2026-02-02)_\n"
        );
        assert_eq!(
            apply("", &adding("Learned", "a")).unwrap(),
            "## Learned\n\n- a _(w2, 2026-02-02)_\n"
        );
        assert_eq!(
            apply("## Learned\n", &adding("Learned", "a")).unwrap(),
            "## Learned\n\n- a _(w2, 2026-02-02)_\n"
        );
        assert_eq!(
            apply("## Learned\n## Next\n", &adding("Learned", "a")).unwrap(),
            "## Learned\n\n- a _(w2, 2026-02-02)_\n\n## Next\n"
        );
    }

    #[test]
    fn add_keeps_trailing_blank_lines_and_ends_an_unterminated_last_line() {
        assert_eq!(
            apply("## Learned\n\n- x _(a, b)_\n\n\n", &adding("Learned", "y")).unwrap(),
            "## Learned\n\n- x _(a, b)_\n- y _(w2, 2026-02-02)_\n\n\n"
        );
        assert_eq!(
            apply("# Acme\n\n\n", &adding("Learned", "y")).unwrap(),
            "# Acme\n\n## Learned\n\n- y _(w2, 2026-02-02)_\n\n\n"
        );
        assert_eq!(
            apply("## Learned\n\n- x _(a, b)_", &adding("Learned", "y")).unwrap(),
            "## Learned\n\n- x _(a, b)_\n- y _(w2, 2026-02-02)_\n"
        );
        assert_eq!(
            apply("# Acme", &adding("Learned", "y")).unwrap(),
            "# Acme\n\n## Learned\n\n- y _(w2, 2026-02-02)_\n"
        );
    }

    #[test]
    fn add_collapses_newlines_and_rejects_blank_text() {
        assert_eq!(
            apply("", &adding("Learned", "first\r\nsecond\nthird\rfourth")).unwrap(),
            "## Learned\n\n- first second third fourth _(w2, 2026-02-02)_\n"
        );
        assert_eq!(error("", &adding("Learned", " \n ")), "nothing to add");
    }

    #[test]
    fn add_to_a_crlf_file_changes_only_the_inserted_line() {
        let crlf = "## Learned\r\n\r\n- x _(a, b)_\r\nlf only\n\r\n## Next\r\n";
        assert_eq!(
            apply(crlf, &adding("Learned", "y")).unwrap(),
            "## Learned\r\n\r\n- x _(a, b)_\r\nlf only\n- y _(w2, 2026-02-02)_\r\n\r\n## Next\r\n"
        );
    }

    #[test]
    fn entry_edits_restamp_the_entry() {
        assert_eq!(
            apply(SEED, &editing("old", "corrected")).unwrap(),
            SEED.replace(
                "- old _(acme-1, 2026-01-01)_",
                "- corrected _(w2, 2026-02-02)_"
            )
        );
        assert_eq!(
            apply(
                SEED,
                &editing("old _(acme-1, 2026-01-01)_", "corrected _(x, 1999-01-01)_")
            )
            .unwrap(),
            SEED.replace(
                "- old _(acme-1, 2026-01-01)_",
                "- corrected _(w2, 2026-02-02)_"
            )
        );
        assert_eq!(
            apply(SEED, &editing("- old", "corrected")).unwrap(),
            SEED.replace(
                "- old _(acme-1, 2026-01-01)_",
                "- corrected _(w2, 2026-02-02)_"
            )
        );
    }

    #[test]
    fn entry_edits_collapse_newlines_and_strip_a_trailing_one() {
        let text = "## Learned\r\n\r\n- old fact _(a, b)_\r\n";
        assert_eq!(
            apply(text, &editing("old", "new\r\nmulti\nline\n")).unwrap(),
            "## Learned\r\n\r\n- new multi line fact _(w2, 2026-02-02)_\r\n"
        );
    }

    #[test]
    fn literal_edits_replace_prose_and_keep_line_endings() {
        assert_eq!(
            apply(SEED, &editing("Clone it.", "Clone it twice.")).unwrap(),
            SEED.replace("Clone it.", "Clone it twice.")
        );
        let crlf = "Intro\r\nline one\r\nline two\r\n";
        assert_eq!(
            apply(crlf, &editing("line one\nline two", "one line\n")).unwrap(),
            "Intro\r\none line\r\n"
        );
        assert_eq!(apply("a\nb\n", &editing("a\nb", "x\ny")).unwrap(), "x\ny\n");
    }

    #[test]
    fn edits_that_would_empty_an_entry_or_do_not_match_are_rejected() {
        assert!(error(SEED, &editing("- old _(acme-1, 2026-01-01)_", "")).contains("remove"));
        assert!(error(SEED, &editing("old", " ")).contains("remove"));
        assert_eq!(
            error(SEED, &editing("missing", "x")),
            "not found in the memory"
        );
        assert!(error(SEED, &editing("## ", "x")).starts_with("matches 3 places"));
        assert!(error("- aaa _(a, b)_\n", &editing("aa", "x")).starts_with("matches 2 places"));
        assert_eq!(
            error(SEED, &editing(" \n", "x")),
            "the text to replace is empty"
        );
        assert_eq!(
            error(SEED, &editing("", "x")),
            "the text to replace is empty"
        );
    }

    #[test]
    fn entry_edits_that_keep_only_the_dash_are_rejected() {
        assert!(error(SEED, &editing("old _(acme-1, 2026-01-01)_", "")).contains("remove"));
        assert!(error(SEED, &editing("old _(acme-1, 2026-01-01)_", " ")).contains("remove"));
    }

    #[test]
    fn literal_edits_keep_a_line_ending_that_old_ends_with() {
        let text = "Intro\nline one\nline two\n## Next\n";
        assert_eq!(
            apply(text, &editing("line one\nline two\n", "line 1\nline 2\n")).unwrap(),
            "Intro\nline 1\nline 2\n## Next\n"
        );
        assert_eq!(
            apply(text, &editing("line one\nline two\n", "line 1\nline 2")).unwrap(),
            "Intro\nline 1\nline 2\n## Next\n"
        );
        let crlf = "Intro\r\nline one\r\n## Next\r\n";
        assert_eq!(
            apply(crlf, &editing("line one\n", "line 1")).unwrap(),
            "Intro\r\nline 1\r\n## Next\r\n"
        );
    }

    #[test]
    fn add_rejects_a_blank_or_multi_line_section() {
        assert_eq!(error(SEED, &adding(" ", "x")), "the section name is empty");
        assert_eq!(
            error(SEED, &adding("A\nB", "x")),
            "the section name must be on one line"
        );
        assert_eq!(
            error(SEED, &adding("A\r", "x")),
            "the section name must be on one line"
        );
    }

    #[test]
    fn edits_match_text_starting_with_a_multibyte_character() {
        let text = "## Learned\n\n- Projects ›› Memory › tab _(a, b)_\n";
        assert_eq!(
            apply(text, &editing("› Memory", "› Changes")).unwrap(),
            "## Learned\n\n- Projects ›› Changes › tab _(w2, 2026-02-02)_\n"
        );
        assert!(error(text, &editing("›", "x")).starts_with("matches 3 places"));
    }

    #[test]
    fn removes_whole_lines_without_leaving_two_blank_lines() {
        let text = "## Learned\n\n- a _(x, y)_\n- b _(x, y)_\n- c _(x, y)_\n\n## Next\n\nText.\n";
        assert_eq!(
            apply(text, &removing("b _(")).unwrap(),
            "## Learned\n\n- a _(x, y)_\n- c _(x, y)_\n\n## Next\n\nText.\n"
        );
        assert_eq!(
            apply(SEED, &removing("old")).unwrap(),
            "# Acme\n\n## Test database\n\nClone it.\n\n## Learned\n\n## Starting a story\n\nSubtasks.\n"
        );
        assert_eq!(
            apply("Intro\n\nOnly\n\n\nEnd\n", &removing("Only")).unwrap(),
            "Intro\n\nEnd\n"
        );
        assert_eq!(
            apply("Intro\r\n\r\nOnly\r\n\r\nEnd", &removing("Only")).unwrap(),
            "Intro\r\n\r\nEnd"
        );
    }

    #[test]
    fn remove_refuses_headings_newlines_and_ambiguity() {
        assert_eq!(
            error(SEED, &removing("## Learned")),
            "won't remove a heading"
        );
        assert_eq!(error(SEED, &removing("Learned")), "won't remove a heading");
        assert_eq!(
            error(SEED, &removing("old\nx")),
            "the text to remove must be on one line"
        );
        assert_eq!(error(SEED, &removing("  ")), "nothing to remove");
        assert!(error(SEED, &removing("a")).starts_with("matches"));
    }

    fn lines(hunk: &Hunk) -> Vec<String> {
        hunk.lines
            .iter()
            .map(|l| {
                let tag = match l.tag {
                    LineTag::Ctx => ' ',
                    LineTag::Del => '-',
                    LineTag::Add => '+',
                };
                format!("{tag}{}", l.text)
            })
            .collect()
    }

    #[test]
    fn previews_a_hunk_in_the_middle_with_context() {
        let text = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let p = preview(text, &editing("5", "five")).unwrap();
        assert_eq!(p.base, hash(text));
        assert_eq!(
            lines(&p.hunk),
            [" 2", " 3", " 4", "-5", "+five", " 6", " 7", " 8"]
        );
        assert_eq!(p.hunk.lines[3].changed, Some(Span { start: 0, end: 1 }));
        assert_eq!(p.hunk.lines[4].changed, Some(Span { start: 0, end: 4 }));
    }

    #[test]
    fn previews_hunks_at_the_start_and_end() {
        let text = "1\n2\n3\n4\n5\n";
        let start = preview(text, &editing("1\n", "")).unwrap();
        assert_eq!(lines(&start.hunk), ["-1", " 2", " 3", " 4"]);
        let end = preview(text, &adding("Learned", "x")).unwrap();
        assert_eq!(
            lines(&end.hunk),
            [
                " 3",
                " 4",
                " 5",
                "+",
                "+## Learned",
                "+",
                "+- x _(w2, 2026-02-02)_"
            ]
        );
    }

    #[test]
    fn preview_highlights_the_changed_words_of_an_edited_line() {
        let text = "- the old › fact _(a, b)_\n";
        let p = preview(text, &editing("old", "new")).unwrap();
        let del = &p.hunk.lines[0];
        let add = &p.hunk.lines[1];
        assert_eq!(
            &del.text[del.changed.unwrap().start..del.changed.unwrap().end],
            "old › fact _(a, b"
        );
        assert_eq!(
            &add.text[add.changed.unwrap().start..add.changed.unwrap().end],
            "new › fact _(w2, 2026-02-02"
        );
    }

    #[test]
    fn preview_handles_identical_adjacent_entries() {
        let text = "## Learned\n\n- x _(w2, 2026-02-02)_\n";
        let p = preview(text, &adding("Learned", "x")).unwrap();
        assert_eq!(
            lines(&p.hunk),
            [
                " ## Learned",
                " ",
                " - x _(w2, 2026-02-02)_",
                "+- x _(w2, 2026-02-02)_"
            ]
        );
    }

    #[test]
    fn preview_keeps_an_unterminated_last_line_as_context() {
        let p = preview("## Learned\n\n- x _(a, b)_", &adding("Learned", "y")).unwrap();
        assert_eq!(
            lines(&p.hunk),
            [
                " ## Learned",
                " ",
                " - x _(a, b)_",
                "+- y _(w2, 2026-02-02)_"
            ]
        );
    }

    #[test]
    fn preview_names_the_section_the_change_lands_under() {
        let section = |current: &str, change: &Change| preview(current, change).unwrap().section;
        assert_eq!(
            section(SEED, &adding("learned", "x")).as_deref(),
            Some("Learned")
        );
        assert_eq!(section(SEED, &adding(" New ", "x")).as_deref(), Some("New"));
        assert_eq!(
            section(SEED, &editing("old", "new")).as_deref(),
            Some("Learned")
        );
        assert_eq!(
            section(SEED, &removing("Subtasks.")).as_deref(),
            Some("Starting a story")
        );
        assert_eq!(
            section(SEED, &editing("Clone it.", "Clone.")).as_deref(),
            Some("Test database")
        );
        assert_eq!(section(SEED, &editing("# Acme", "# Corp")), None);
    }

    #[test]
    fn pending_previews_serialise_as_tagged_results() {
        let ready: PreviewResult = preview("", &adding("L", "x")).into();
        let json = serde_json::to_value(&ready).unwrap();
        assert_eq!(json["status"], "ready");
        assert!(json["hunk"]["lines"].is_array());
        let stale: PreviewResult = preview("", &removing("x")).into();
        assert_eq!(
            serde_json::to_value(&stale).unwrap(),
            serde_json::json!({"status": "stale", "reason": "not found in the memory"})
        );
        let json = serde_json::to_value(adding("L", "x")).unwrap();
        assert_eq!(json["kind"], "add");
        assert_eq!(json["section"], "L");
    }

    #[test]
    fn propose_files_changes_that_apply_and_pending_lists_them() {
        let (_dir, paths) = seeded(SEED);
        let err = propose(
            &paths,
            "p",
            ChangeKind::Remove {
                text: "missing".into(),
            },
            "w",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not found"));
        let a = propose(
            &paths,
            "p",
            ChangeKind::Add {
                section: DEFAULT_SECTION.into(),
                text: "fact".into(),
            },
            "w",
        )
        .unwrap();
        let b = propose(&paths, "p", ChangeKind::Remove { text: "old".into() }, "w").unwrap();
        assert_eq!(memory(&paths), SEED);
        std::fs::write(paths.memory_changes_dir("p").join("x.json.tmp"), "{").unwrap();
        std::fs::write(paths.memory_changes_dir("p").join("0-bad.json"), "{").unwrap();
        let valid = serde_json::to_vec(&adding("Learned", "x")).unwrap();
        for name in ["x (1).json", "a.b.json"] {
            std::fs::write(paths.memory_changes_dir("p").join(name), &valid).unwrap();
        }
        let list = pending(&paths, "p").unwrap();
        let mut expected = [a, b];
        expected.sort();
        assert_eq!(
            list.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(list[0].change.author, "w");
        assert!(
            list.iter()
                .all(|c| matches!(&c.preview, PreviewResult::Ready(p) if p.base == hash(SEED)))
        );
        assert!(pending(&paths, "other").unwrap().is_empty());
    }

    fn propose_edit(paths: &Paths, old: &str, new: &str) -> String {
        propose(
            paths,
            "p",
            ChangeKind::Edit {
                old: old.into(),
                new: new.into(),
            },
            "w",
        )
        .unwrap()
    }

    #[test]
    fn writing_a_change_never_replaces_one_with_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1-aaaa.json"), "first").unwrap();
        assert!(!write_new(dir.path(), "1-aaaa", b"second").unwrap());
        assert!(write_new(dir.path(), "1-bbbb", b"third").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("1-aaaa.json")).unwrap(),
            "first"
        );
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["1-aaaa.json", "1-bbbb.json"]);
    }

    #[test]
    fn restore_attempts_every_file_and_names_each_failure() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let files = [
            (missing.join("a.json"), b"a".to_vec()),
            (dir.path().join("b.json"), b"b".to_vec()),
            (missing.join("c.json"), b"c".to_vec()),
        ];
        let err = format!("{:#}", with_restore(anyhow::anyhow!("boom"), &files));
        assert!(err.contains("a.json") && err.contains("c.json"), "{err}");
        assert!(err.ends_with("boom"), "{err}");
        assert_eq!(std::fs::read(dir.path().join("b.json")).unwrap(), b"b");
    }

    #[test]
    fn approve_applies_one_change_and_deletes_its_file() {
        let (_dir, paths) = seeded(SEED);
        let id = propose_edit(&paths, "old", "new");
        approve(&paths, "p", &ids(&[&id]), &hash(SEED)).unwrap();
        assert!(memory(&paths).contains("- new _(w, "));
        assert!(change_files(&paths).is_empty());
        let err = approve(&paths, "p", &ids(&[&id]), &hash(&memory(&paths))).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!("change {id} was already handled")
        );
    }

    #[test]
    fn approve_applies_a_batch_in_id_order_in_one_write() {
        let (_dir, paths) = seeded(SEED);
        let add = propose(
            &paths,
            "p",
            ChangeKind::Add {
                section: DEFAULT_SECTION.into(),
                text: "added".into(),
            },
            "w",
        )
        .unwrap();
        let edit = propose_edit(&paths, "Clone it.", "Clone it twice.");
        let remove = propose(&paths, "p", ChangeKind::Remove { text: "old".into() }, "w").unwrap();
        approve(&paths, "p", &ids(&[&remove, &add, &edit]), &hash(SEED)).unwrap();
        let text = memory(&paths);
        assert!(text.contains("Clone it twice."));
        assert!(!text.contains("- old"));
        assert!(text.contains("## Learned\n\n- added _(w, "), "{text}");
        assert!(change_files(&paths).is_empty());
    }

    #[test]
    fn approve_fails_the_whole_batch_when_one_change_does_not_apply() {
        let (_dir, paths) = seeded(SEED);
        let mut pair = [
            propose_edit(&paths, "old", "one"),
            propose_edit(&paths, "old", "two"),
        ];
        pair.sort();
        let [first, second] = pair;
        let before = change_files(&paths);
        let err = approve(&paths, "p", &ids(&[&first, &second]), &hash(SEED)).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.starts_with(&format!("change {second}: not found")),
            "{err}"
        );
        assert_eq!(memory(&paths), SEED);
        assert_eq!(change_files(&paths), before);

        let stale = propose_edit(&paths, "Subtasks.", "Sub-issues.");
        let changed = SEED.replace("Subtasks.", "Tasks.");
        save(&paths.memory_file("p"), &hash(SEED), &changed).unwrap();
        let err = approve(&paths, "p", &ids(&[&first, &stale]), &hash(&changed)).unwrap_err();
        assert!(format!("{err:#}").starts_with(&format!("change {stale}: not found")));
        assert_eq!(memory(&paths), changed);
        assert_eq!(change_files(&paths).len(), 3);
    }

    #[test]
    fn approve_refuses_a_stale_base() {
        let (_dir, paths) = seeded(SEED);
        let id = propose_edit(&paths, "old", "new");
        let err = approve(&paths, "p", &ids(&[&id]), &hash("something else")).unwrap_err();
        assert!(format!("{err:#}").starts_with("Memory changed since this diff was shown"));
        assert_eq!(memory(&paths), SEED);
        assert_eq!(change_files(&paths).len(), 1);
        assert!(approve(&paths, "p", &ids(&["../x"]), &hash(SEED)).is_err());
    }

    #[test]
    fn reject_deletes_selected_files_and_skips_missing_ones() {
        let (_dir, paths) = seeded(SEED);
        let a = propose_edit(&paths, "old", "a");
        let b = propose_edit(&paths, "old", "b");
        let c = propose_edit(&paths, "old", "c");
        reject(&paths, "p", &ids(&[&a, &c])).unwrap();
        assert_eq!(change_files(&paths), [format!("{b}.json")]);
        reject(&paths, "p", &ids(&[&a, &b])).unwrap();
        assert!(change_files(&paths).is_empty());
        assert_eq!(memory(&paths), SEED);
    }

    #[test]
    fn parallel_proposals_keep_every_file() {
        let (_dir, paths) = seeded(SEED);
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let paths = paths.clone();
                std::thread::spawn(move || {
                    propose(
                        &paths,
                        "p",
                        ChangeKind::Add {
                            section: DEFAULT_SECTION.into(),
                            text: format!("fact {i}"),
                        },
                        "w",
                    )
                    .unwrap()
                })
            })
            .collect();
        let mut ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 16);
        assert_eq!(pending(&paths, "p").unwrap().len(), 16);
    }

    #[test]
    fn of_two_parallel_approves_with_one_base_exactly_one_lands() {
        let (_dir, paths) = seeded(SEED);
        let a = propose_edit(&paths, "old", "a");
        let b = propose_edit(&paths, "Clone it.", "Clone.");
        let base = hash(SEED);
        let handles: Vec<_> = [a, b]
            .into_iter()
            .map(|id| {
                let (paths, base) = (paths.clone(), base.clone());
                std::thread::spawn(move || approve(&paths, "p", &[id], &base))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        let err = results.into_iter().find_map(Result::err).unwrap();
        assert!(format!("{err:#}").starts_with("Memory changed"));
        assert_eq!(change_files(&paths).len(), 1);
    }

    #[test]
    fn approving_one_after_the_other_with_fresh_bases_lands_both() {
        let (_dir, paths) = seeded(SEED);
        let a = propose_edit(&paths, "old", "a");
        let b = propose_edit(&paths, "Clone it.", "Clone.");
        approve(&paths, "p", &ids(&[&a]), &hash(&memory(&paths))).unwrap();
        approve(&paths, "p", &ids(&[&b]), &hash(&memory(&paths))).unwrap();
        let text = memory(&paths);
        assert!(
            text.contains("- a _(w, ") && text.contains("Clone.\n"),
            "{text}"
        );
    }

    #[test]
    fn save_refuses_a_stale_base() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory").join("acme.md");
        let err = save(&path, &hash("old"), "new\n").unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "Memory changed since you started editing"
        );
        assert_eq!(read(&path).unwrap(), "");
        save(&path, &hash(""), "new\n").unwrap();
        assert_eq!(read(&path).unwrap(), "new\n");
    }

    #[test]
    fn formats_civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_745), "2026-10-19");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }
}
