//! salash's Claude Code plugin: the skills in the repo's `skills/`, embedded
//! in the binary. The server writes it to `<data>/plugin` at startup and
//! Claude tabs load it with `claude --plugin-dir`, so the skills match this
//! binary and only salash's tabs see them.

use anyhow::{bail, Context, Result};
use include_dir::{include_dir, Dir, DirEntry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

static SKILLS: Dir = include_dir!("$CARGO_MANIFEST_DIR/skills");

/// The plugin's name. Claude Code shows its skills as `/salash:<skill>`.
pub const NAME: &str = "salash";

/// Hidden files such as `.DS_Store` that a checkout may hold aren't shipped.
fn hidden(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'))
}

/// `.claude-plugin/plugin.json`, versioned with this binary.
fn manifest() -> Value {
    json!({
        "name": NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Skills for Claude Code tabs running inside salash",
        "author": { "name": "salash" },
        "repository": env!("CARGO_PKG_REPOSITORY"),
        "license": env!("CARGO_PKG_LICENSE"),
    })
}

/// Write an embedded directory's files under `base`, at their embedded paths.
fn write_dir(dir: &Dir, base: &Path) -> Result<()> {
    std::fs::create_dir_all(base.join(dir.path()))?;
    for entry in dir.entries() {
        if hidden(entry.path()) {
            continue;
        }
        match entry {
            DirEntry::Dir(d) => write_dir(d, base)?,
            DirEntry::File(f) => {
                let p = base.join(f.path());
                std::fs::write(&p, f.contents()).with_context(|| format!("writing {}", p.display()))?;
            }
        }
    }
    Ok(())
}

/// The embedded skills, one directory each.
fn skills() -> impl Iterator<Item = &'static Dir<'static>> {
    SKILLS.dirs().filter(|d| !hidden(d.path()))
}

/// Names of the embedded skills.
pub fn skill_names() -> Vec<&'static str> {
    skills().filter_map(|d| d.path().file_name()?.to_str()).collect()
}

/// Replace `root` with the plugin: the manifest and every skill. What was
/// there before is removed first, so a skill this binary no longer has goes
/// too. If `root` is a symlink, the link is removed, not what it points to.
pub fn write_to(root: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(root) {
        if meta.is_dir() {
            std::fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))?;
        } else {
            std::fs::remove_file(root).with_context(|| format!("removing {}", root.display()))?;
        }
    }
    let manifest_path = root.join(".claude-plugin").join("plugin.json");
    std::fs::create_dir_all(manifest_path.parent().unwrap())?;
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest())?)?;
    write_dir(&SKILLS, &root.join("skills"))
}

/// Write the plugin to `<data>/plugin`, the `--plugin-dir` Claude tabs get.
/// Returns its path.
pub fn write() -> Result<PathBuf> {
    let root = crate::config::data_dir().join("plugin");
    write_to(&root)?;
    Ok(root)
}

/// `salash install-skill`: write every embedded skill to
/// `<claude_dir>/skills/<name>`, overwriting salash's own files there.
/// Returns the path of each `SKILL.md`.
pub fn install_skills(claude_dir: &Path) -> Result<Vec<PathBuf>> {
    let base = claude_dir.join("skills");
    let mut written = vec![];
    for d in skills() {
        write_dir(d, &base)?;
        written.push(base.join(d.path()).join("SKILL.md"));
    }
    if written.is_empty() {
        bail!("this salash has no skills built in");
    }
    Ok(written)
}

/// Embedded skills that also exist in `<claude_dir>/skills`, as an earlier
/// `salash install-skill` left them. Only looks; never removes anything.
pub fn global_copies(claude_dir: &Path) -> Vec<&'static str> {
    skill_names().into_iter().filter(|n| claude_dir.join("skills").join(n).exists()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn repo_skill(name: &str) -> String {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills").join(name).join("SKILL.md");
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn write_to_writes_manifest_and_every_skill() {
        let dir = tmp();
        let root = dir.join("plugin");
        write_to(&root).unwrap();

        let m: Value = serde_json::from_slice(&std::fs::read(root.join(".claude-plugin/plugin.json")).unwrap()).unwrap();
        assert_eq!(m["name"], NAME);
        assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
        assert!(skill_names().contains(&"new-salash-tab"));
        for name in skill_names() {
            let got = std::fs::read_to_string(root.join("skills").join(name).join("SKILL.md")).unwrap();
            assert_eq!(got, repo_skill(name));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn write_to_drops_stale_files() {
        let dir = tmp();
        let root = dir.join("plugin");
        std::fs::create_dir_all(root.join("skills/gone")).unwrap();
        std::fs::write(root.join("skills/gone/SKILL.md"), "old").unwrap();

        write_to(&root).unwrap();
        assert!(!root.join("skills/gone").exists());
        assert!(root.join("skills/new-salash-tab/SKILL.md").is_file());
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_to_replaces_a_symlink_without_following_it() {
        let dir = tmp();
        let target = dir.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep"), "mine").unwrap();
        let root = dir.join("plugin");
        std::os::unix::fs::symlink(&target, &root).unwrap();

        write_to(&root).unwrap();
        assert_eq!(std::fs::read_to_string(target.join("keep")).unwrap(), "mine");
        assert!(!std::fs::symlink_metadata(&root).unwrap().file_type().is_symlink());
        assert!(root.join(".claude-plugin/plugin.json").is_file());
        assert!(!target.join(".claude-plugin").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn every_embedded_skill_is_well_formed() {
        for name in skill_names() {
            let text = repo_skill(name);
            assert!(text.starts_with("---\n"), "{name}: no frontmatter");
            let front = text[4..].split("\n---").next().unwrap();
            assert!(front.lines().any(|l| l.trim() == format!("name: {name}")), "{name}: frontmatter name differs");
            assert!(front.lines().any(|l| l.starts_with("description: ")), "{name}: no description");
        }
    }

    #[test]
    fn install_skills_writes_each_skill() {
        let dir = tmp();
        let written = install_skills(&dir).unwrap();
        assert_eq!(written.len(), skill_names().len());
        for name in skill_names() {
            let p = dir.join("skills").join(name).join("SKILL.md");
            assert!(written.contains(&p));
            assert_eq!(std::fs::read_to_string(p).unwrap(), repo_skill(name));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn global_copies_finds_only_existing_ones_and_keeps_them() {
        let dir = tmp();
        assert!(global_copies(&dir).is_empty());
        let old = dir.join("skills/new-salash-tab");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), "old").unwrap();
        assert_eq!(global_copies(&dir), vec!["new-salash-tab"]);
        assert_eq!(std::fs::read_to_string(old.join("SKILL.md")).unwrap(), "old");
        std::fs::remove_dir_all(dir).ok();
    }
}
