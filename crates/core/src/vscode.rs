use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::process;

/// The first `*.code-workspace` file directly in `dir`, by name, or else `dir` itself.
pub fn target(dir: &Path) -> PathBuf {
    let mut workspaces: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "code-workspace"))
        .collect();
    workspaces.sort();
    workspaces
        .into_iter()
        .next()
        .unwrap_or_else(|| dir.to_path_buf())
}

/// Opens `dir` (or its workspace file) in a new VS Code window, using `code` from PATH.
pub fn open(dir: &Path) -> Result<()> {
    // Command::new("code") only finds code.exe on Windows; naming the .cmd shim also gets std's
    // batch-file argument escaping.
    let program = if cfg!(windows) { "code.cmd" } else { "code" };
    let mut cmd = process::command(program);
    cmd.arg("--new-window").arg(target(dir));
    process::spawn_detached(cmd)
        .with_context(|| format!("couldn't run `{program}`: is VS Code's `code` on PATH?"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_without_workspace_opens_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        assert_eq!(target(dir.path()), dir.path());
    }

    #[test]
    fn workspace_file_is_preferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.code-workspace"), "{}").unwrap();
        std::fs::write(dir.path().join("a.code-workspace"), "{}").unwrap();
        assert_eq!(target(dir.path()), dir.path().join("a.code-workspace"));
    }
}
