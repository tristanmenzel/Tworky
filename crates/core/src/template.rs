use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use minijinja::{Environment, UndefinedBehavior};
use serde::Serialize;

fn env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_keep_trailing_newline(true);
    env
}

pub fn render_str<S: Serialize>(template: &str, ctx: &S) -> Result<String> {
    Ok(env().render_str(template, ctx)?)
}

pub fn check(template: &str) -> Result<()> {
    env().template_from_str(template)?;
    Ok(())
}

/// Renders every file under `template_dir` into the same relative path under `dest_root`.
/// Files that are not UTF-8 are copied unchanged.
pub fn render_dir<S: Serialize>(
    template_dir: &Path,
    dest_root: &Path,
    ctx: &S,
) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    if !template_dir.exists() {
        return Ok(written);
    }
    let env = env();
    let mut stack = vec![template_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path.strip_prefix(template_dir)?;
            let dest = dest_root.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = std::fs::read(&path)?;
            match String::from_utf8(bytes) {
                Ok(text) => {
                    let rendered = env
                        .render_str(&text, ctx)
                        .with_context(|| format!("rendering {}", path.display()))?;
                    std::fs::write(&dest, rendered)?;
                }
                Err(e) => std::fs::write(&dest, e.into_bytes())?,
            }
            written.push(rel.to_path_buf());
        }
    }
    written.sort();
    Ok(written)
}

pub fn slugify(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let mut slug: String = slug.chars().take(max).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slugify_trims_and_collapses() {
        assert_eq!(
            slugify("[Invoice Export] Printing layout fixes", 24),
            "invoice-export-printing"
        );
        assert_eq!(slugify("  Implement - Web  ", 40), "implement-web");
    }

    #[test]
    fn strict_undefined_fails() {
        assert!(render_str("{{ ports.nope }}", &json!({"ports": {}})).is_err());
        assert_eq!(
            render_str("{{ ports.api }}", &json!({"ports": {"api": 4100}})).unwrap(),
            "4100"
        );
    }

    #[test]
    fn renders_directory_tree() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("repo/api")).unwrap();
        std::fs::write(src.path().join("repo/api/.env"), "PORT={{ ports.api }}\n").unwrap();
        std::fs::write(src.path().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        let written =
            render_dir(src.path(), dest.path(), &json!({"ports": {"api": 4100}})).unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read_to_string(dest.path().join("repo/api/.env")).unwrap(),
            "PORT=4100\n"
        );
        assert_eq!(
            std::fs::read(dest.path().join("bin.dat")).unwrap(),
            vec![0xff, 0xfe, 0x00]
        );
    }
}
