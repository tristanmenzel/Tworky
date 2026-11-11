use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub const DEFAULT_SECTION: &str = "Learned";

/// Hook output past this is truncated before it reaches Claude.
pub const SIZE_WARNING: usize = 8 * 1024;

pub fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Appends `- text _(author, date)_` to `section`, under a lock so parallel workers don't lose writes.
pub fn add(path: &Path, section: &str, text: &str, author: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to add");
    }
    let entry = format!("- {} _({author}, {})_", text.replace('\n', " "), today());
    locked(path, |current| Ok(insert(current, section, &entry)))
}

pub fn save(path: &Path, text: &str) -> Result<()> {
    locked(path, |_| Ok(text.to_string()))
}

fn locked(path: &Path, edit: impl FnOnce(&str) -> Result<String>) -> Result<()> {
    let dir = path.parent().context("memory path has no folder")?;
    std::fs::create_dir_all(dir)?;
    let lock_path = path.with_extension("md.lock");
    let lock = std::fs::File::create(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    let next = edit(&read(path)?)?;
    let tmp = path.with_extension("md.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(next.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    lock.unlock()?;
    Ok(())
}

fn is_heading(line: &str) -> bool {
    line.starts_with("## ")
}

fn insert(current: &str, section: &str, entry: &str) -> String {
    let lines: Vec<&str> = current.lines().collect();
    let start = lines
        .iter()
        .position(|l| is_heading(l) && l[3..].trim().eq_ignore_ascii_case(section.trim()));
    let Some(start) = start else {
        let mut out = current.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("## {}\n\n{entry}\n", section.trim()));
        return out;
    };
    let end = lines[start + 1..]
        .iter()
        .position(|l| is_heading(l))
        .map_or(lines.len(), |i| start + 1 + i);
    let last_content = (start..end)
        .rev()
        .find(|&i| !lines[i].trim().is_empty())
        .unwrap_or(start);
    let mut out: Vec<&str> = lines[..=last_content].to_vec();
    if last_content == start {
        out.push("");
    }
    out.push(entry);
    if end < lines.len() {
        out.push("");
        out.extend(&lines[end..]);
    }
    out.join("\n") + "\n"
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

    #[test]
    fn appends_to_the_end_of_an_existing_section() {
        let out = insert(SEED, "learned", "- new");
        assert_eq!(
            out,
            "# Acme\n\n## Test database\n\nClone it.\n\n## Learned\n\n- old _(acme-1, 2026-01-01)_\n- new\n\n## Starting a story\n\nSubtasks.\n"
        );
    }

    #[test]
    fn creates_a_missing_section_and_an_empty_file() {
        assert_eq!(
            insert("# Acme\n", "Learned", "- a"),
            "# Acme\n\n## Learned\n\n- a\n"
        );
        assert_eq!(insert("", "Learned", "- a"), "## Learned\n\n- a\n");
        assert_eq!(
            insert("## Learned\n", "Learned", "- a"),
            "## Learned\n\n- a\n"
        );
    }

    #[test]
    fn add_writes_attributed_entries_and_save_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory").join("acme.md");
        assert_eq!(read(&path).unwrap(), "");
        add(&path, DEFAULT_SECTION, "first\nfact", "acme-1").unwrap();
        add(&path, DEFAULT_SECTION, "second", "acme-2").unwrap();
        let text = read(&path).unwrap();
        assert!(text.contains("- first fact _(acme-1, "));
        assert!(text.find("first").unwrap() < text.find("second").unwrap());
        assert!(add(&path, DEFAULT_SECTION, "  ", "x").is_err());
        save(&path, "replaced\n").unwrap();
        assert_eq!(read(&path).unwrap(), "replaced\n");
    }

    #[test]
    fn parallel_adds_keep_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme.md");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    add(&path, DEFAULT_SECTION, &format!("fact {i}"), "w").unwrap()
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let text = read(&path).unwrap();
        for i in 0..8 {
            assert!(text.contains(&format!("fact {i} ")), "{text}");
        }
    }

    #[test]
    fn formats_civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_745), "2026-10-19");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }
}
