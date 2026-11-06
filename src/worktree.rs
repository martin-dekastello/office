//! One git worktree + branch per issue: <worktrees_dir>/<repo>/<id>-<slug>

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Create (or reuse) the worktree for an issue. Returns its path.
pub fn ensure(repo: &Path, worktrees_dir: &Path, stem: &str) -> Result<PathBuf> {
    let top = git(repo, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not a git repository", repo.display()))?;
    let top = PathBuf::from(top);
    let repo_name = top
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let path = worktrees_dir.join(repo_name).join(stem);
    if path.join(".git").exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    let p = path.to_string_lossy().into_owned();
    let branch_exists = git(&top, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{stem}")]).is_ok();
    if branch_exists {
        git(&top, &["worktree", "add", &p, stem])?;
    } else {
        git(&top, &["worktree", "add", "-b", stem, &p])?;
    }
    Ok(path)
}
