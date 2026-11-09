//! One git worktree + branch per issue: <worktrees_dir>/<repo>/<id>-<slug>,
//! or wherever the user's `worktree_command` puts it.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Create the worktree with the user's command: `<command…> <repo> <stem>`, run in
/// the repo. It prints the worktree's path as the last line of its output.
pub fn run_script(command: &[String], repo: &Path, worktrees_dir: &Path, stem: &str, issue: &str) -> Result<PathBuf> {
    let (prog, args) = command.split_first().context("worktree_command is empty")?;
    let out = Command::new(crate::config::expand(Path::new(prog)))
        .args(args)
        .arg(repo)
        .arg(stem)
        .current_dir(repo)
        .env("SALASH_REPO", repo)
        .env("SALASH_WORKTREE_NAME", stem)
        .env("SALASH_WORKTREES_DIR", worktrees_dir)
        .env("SALASH_ISSUE", issue)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {prog}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        let name = Path::new(prog).file_name().map_or(prog.into(), |n| n.to_string_lossy());
        bail!("{name} failed ({})\n{}", out.status, last_lines(detail, 8));
    }
    parse_path(&stdout, repo)
}

/// The worktree path from the script's output: its last non-empty line,
/// relative to the repo unless absolute. It has to exist.
fn parse_path(stdout: &str, repo: &Path) -> Result<PathBuf> {
    let Some(line) = stdout.lines().map(str::trim).rfind(|l| !l.is_empty()) else {
        bail!("the script printed no worktree path");
    };
    let path = repo.join(crate::config::expand(Path::new(line)));
    if !path.is_dir() {
        bail!("the script printed {line}, which is not a directory");
    }
    Ok(path)
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &Path, body: &str) -> Vec<String> {
        let p = dir.join("mkwt.sh");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        vec!["sh".into(), p.to_string_lossy().into_owned()]
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("salash-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    #[test]
    fn script_gets_repo_and_name_and_returns_last_line() {
        let d = tmp("ok");
        let cmd = script(&d, r#"echo "making $2 from $1"; mkdir -p "$SALASH_WORKTREES_DIR/$SALASH_WORKTREE_NAME"; echo "$SALASH_WORKTREES_DIR/$2""#);
        let p = run_script(&cmd, &d, &d.join("wts"), "PROJ-1-fix-it", "PROJ-1").unwrap();
        assert_eq!(p, d.join("wts").join("PROJ-1-fix-it"));
    }

    #[cfg(unix)]
    #[test]
    fn relative_path_is_inside_repo() {
        let d = tmp("rel");
        let cmd = script(&d, r#"mkdir -p "wt/$2"; echo "wt/$2""#);
        let p = run_script(&cmd, &d, &d, "X-1-a", "X-1").unwrap();
        assert_eq!(p, d.join("wt").join("X-1-a"));
    }

    #[cfg(unix)]
    #[test]
    fn failure_reports_stderr() {
        let d = tmp("fail");
        let cmd = script(&d, r#"echo "branch already checked out" >&2; exit 3"#);
        let e = format!("{:#}", run_script(&cmd, &d, &d, "X-1-a", "X-1").unwrap_err());
        assert!(e.contains("branch already checked out"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn missing_or_bogus_path_is_an_error() {
        let d = tmp("bogus");
        assert!(run_script(&script(&d, "true"), &d, &d, "X-1-a", "X-1").is_err());
        assert!(run_script(&script(&d, "echo /no/such/dir"), &d, &d, "X-1-a", "X-1").is_err());
    }
}
