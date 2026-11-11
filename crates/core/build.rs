use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let ci_commit = std::env::var("GITHUB_SHA").ok().filter(|s| !s.is_empty());
    let commit = ci_commit
        .clone()
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    let dirty = ci_commit.is_none()
        && git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());

    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let dir = PathBuf::from(dir);
        println!("cargo:rerun-if-changed={}", dir.join("HEAD").display());
        if let Some(common) = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
            let common = PathBuf::from(common);
            println!(
                "cargo:rerun-if-changed={}",
                common.join("packed-refs").display()
            );
            if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
                println!("cargo:rerun-if-changed={}", common.join(head_ref).display());
            }
        }
    }

    println!("cargo:rustc-env=TWORKY_COMMIT={commit}");
    let short: String = commit.chars().take(7).collect();
    let short = if dirty {
        format!("{short}-dirty")
    } else {
        short
    };
    println!("cargo:rustc-env=TWORKY_DIRTY={dirty}");
    println!("cargo:rustc-env=TWORKY_SHORT={short}");
}
