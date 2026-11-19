//! salash's Claude Code plugin: the skills in the repo's `skills/`, embedded
//! in the binary. The server writes it to `<data>/plugin` at startup and
//! Claude tabs load it with `claude --plugin-dir`, so the skills match this
//! binary and only salash's tabs see them.

use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;

/// The embedded skills: name and `SKILL.md`.
const SKILLS: &[(&str, &str)] = &[("new-tab", include_str!("../skills/new-tab/SKILL.md"))];

/// Replace `root` with the plugin: the manifest and every skill. What was
/// there before is removed first, so a skill this binary no longer has goes
/// too. If `root` is a symlink, the link is removed, not what it points to.
pub fn write_to(root: &Path) -> Result<()> {
    match std::fs::remove_dir_all(root) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", root.display()))
        }
        _ => {}
    }
    // Only name is required, but `claude plugin validate` warns without the rest.
    let manifest = json!({
        "name": "salash",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Skills for Claude Code tabs running inside salash",
        "author": { "name": "salash" },
        "repository": env!("CARGO_PKG_REPOSITORY"),
        "license": env!("CARGO_PKG_LICENSE"),
    });
    let manifest_path = root.join(".claude-plugin").join("plugin.json");
    std::fs::create_dir_all(manifest_path.parent().unwrap())?;
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    for (name, text) in SKILLS {
        let dir = root.join("skills").join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), text).with_context(|| format!("writing {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_to_writes_manifest_and_every_skill() {
        let dir = tmp();
        let root = dir.join("plugin");
        write_to(&root).unwrap();

        let m: Value = serde_json::from_slice(&std::fs::read(root.join(".claude-plugin/plugin.json")).unwrap()).unwrap();
        assert_eq!(m["name"], "salash");
        assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
        assert!(SKILLS.iter().any(|(n, _)| *n == "new-tab"));
        for (name, text) in SKILLS {
            let got = std::fs::read_to_string(root.join("skills").join(name).join("SKILL.md")).unwrap();
            assert_eq!(got, *text);
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
        assert!(root.join("skills/new-tab/SKILL.md").is_file());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn write_to_fails_when_it_cannot_write() {
        // App::new then leaves --plugin-dir out rather than failing.
        let dir = tmp();
        std::fs::write(dir.join("file"), "").unwrap();
        assert!(write_to(&dir.join("file/plugin")).is_err());
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
        for (name, text) in SKILLS {
            assert!(text.starts_with("---\n"), "{name}: no frontmatter");
            let front = text[4..].split("\n---").next().unwrap();
            assert!(front.lines().any(|l| l.trim() == format!("name: {name}")), "{name}: frontmatter name differs");
            assert!(front.lines().any(|l| l.starts_with("description: ")), "{name}: no description");
        }
    }
}
