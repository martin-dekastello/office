//! Obsidian-compatible notes: one markdown file per issue in
//! <vault>/<notes_dir>/{active,done,archive}/<id>-<slug>.md
//! The vault is the source of truth for the issue list.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Active,
    Done,
    Archive,
}

impl Stage {
    pub const ALL: [Stage; 3] = [Stage::Active, Stage::Done, Stage::Archive];

    pub fn dir(self) -> &'static str {
        match self {
            Stage::Active => "active",
            Stage::Done => "done",
            Stage::Archive => "archive",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Stage::Active => "Active",
            Stage::Done => "Done",
            Stage::Archive => "Archive",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub stage: Stage,
    pub path: PathBuf,
    pub source: String,
    pub url: Option<String>,
    pub repo: Option<String>,
    /// Explicit local repo path from frontmatter (`repo_path:`), editable in Obsidian.
    pub repo_path: Option<PathBuf>,
    pub created: Option<String>,
}

impl Note {
    /// "<id>-<slug>", used for file names, branch names and worktree dirs.
    pub fn stem(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file_stem(&self.id, &self.title))
    }
}

pub struct NewNote<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub source: &'a str,
    pub url: Option<&'a str>,
    pub repo: Option<&'a str>,
}

/// ASCII-folds (ř -> r, ž -> z), lowercases, and joins words with '-'.
pub fn slugify(s: &str) -> String {
    let folded = deunicode::deunicode(s).to_lowercase();
    let mut out = String::new();
    let mut dash = false;
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let mut out = out.trim_end_matches('-').to_string();
    if out.len() > 60 {
        out.truncate(60);
        if let Some(i) = out.rfind('-') {
            if i > 20 {
                out.truncate(i);
            }
        }
        out = out.trim_end_matches('-').to_string();
    }
    if out.is_empty() {
        out.push_str("untitled");
    }
    out
}

/// Keep IDs readable but filesystem/branch safe ("owner/repo#12" -> "owner-repo-12").
pub fn sanitize_id(id: &str) -> String {
    let mut out = String::new();
    for c in deunicode::deunicode(id).chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

pub fn file_stem(id: &str, title: &str) -> String {
    format!("{}-{}", sanitize_id(id), slugify(title))
}

fn yaml_str(s: &str) -> String {
    // JSON strings are valid YAML double-quoted scalars.
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s))
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
        if let Ok(s) = serde_json::from_str::<String>(v) {
            return s;
        }
        return v[1..v.len() - 1].to_string();
    }
    if v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2 {
        return v[1..v.len() - 1].replace("''", "'");
    }
    v.to_string()
}

/// Minimal frontmatter reader: top-level `key: scalar` pairs only.
pub fn parse_frontmatter(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return map;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if line.starts_with([' ', '\t', '-']) {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim();
            if !v.is_empty() {
                map.insert(k.trim().to_string(), unquote(v));
            }
        }
    }
    map
}

/// Replace (or insert) a top-level frontmatter scalar, leaving the rest untouched.
pub fn set_frontmatter_key(text: &str, key: &str, value: &str) -> String {
    let line_new = format!("{}: {}", key, value);
    let mut out: Vec<String> = Vec::new();
    let mut lines = text.lines();
    if text.lines().next().map(str::trim) != Some("---") {
        return format!("---\n{}\n---\n{}", line_new, text);
    }
    out.push(lines.next().unwrap().to_string());
    let mut done = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            if !done {
                out.push(line_new.clone());
            }
            out.push(line.to_string());
            break;
        }
        if !done && line.split_once(':').map(|(k, _)| k.trim()) == Some(key) && !line.starts_with(' ') {
            out.push(line_new.clone());
            done = true;
        } else {
            out.push(line.to_string());
        }
    }
    out.extend(lines.map(String::from));
    let mut s = out.join("\n");
    if text.ends_with('\n') {
        s.push('\n');
    }
    s
}

pub struct Vault {
    pub root: PathBuf,
}

impl Vault {
    pub fn new(root: PathBuf) -> Result<Self> {
        for st in Stage::ALL {
            fs::create_dir_all(root.join(st.dir()))
                .with_context(|| format!("creating {}", root.join(st.dir()).display()))?;
        }
        Ok(Vault { root })
    }

    fn read_note(&self, path: &Path, stage: Stage) -> Option<Note> {
        let text = fs::read_to_string(path).ok()?;
        let fm = parse_frontmatter(&text);
        let stem = path.file_stem()?.to_string_lossy().into_owned();
        let id = fm.get("id").cloned().unwrap_or_else(|| stem.clone());
        let title = fm
            .get("title")
            .cloned()
            .or_else(|| {
                text.lines()
                    .find_map(|l| l.strip_prefix("# ").map(|s| s.trim().to_string()))
            })
            .unwrap_or_else(|| stem.clone());
        Some(Note {
            id,
            title,
            stage,
            path: path.to_path_buf(),
            source: fm.get("source").cloned().unwrap_or_else(|| "local".into()),
            url: fm.get("url").cloned(),
            repo: fm.get("repo").cloned(),
            repo_path: fm.get("repo_path").map(|p| crate::config::expand(Path::new(p))),
            created: fm.get("created").cloned(),
        })
    }

    /// All notes, newest-created first within each stage.
    pub fn scan(&self) -> Vec<Note> {
        let mut notes = Vec::new();
        for st in Stage::ALL {
            let Ok(rd) = fs::read_dir(self.root.join(st.dir())) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("md") {
                    if let Some(n) = self.read_note(&p, st) {
                        notes.push(n);
                    }
                }
            }
        }
        notes.sort_by(|a, b| {
            a.stage
                .cmp(&b.stage)
                .then_with(|| b.created.cmp(&a.created))
                .then_with(|| a.id.cmp(&b.id))
        });
        notes
    }

    pub fn find(&self, id: &str) -> Option<Note> {
        self.scan().into_iter().find(|n| n.id == id)
    }

    /// Create the note in active/ unless a note with this id exists in any folder.
    pub fn create(&self, n: NewNote) -> Result<(Note, bool)> {
        if let Some(existing) = self.find(n.id) {
            return Ok((existing, false));
        }
        let stem = file_stem(n.id, n.title);
        let path = self.root.join(Stage::Active.dir()).join(format!("{stem}.md"));
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let mut fm = format!(
            "---\nid: {}\ntitle: {}\nstatus: active\nsource: {}\n",
            yaml_str(n.id),
            yaml_str(n.title),
            n.source
        );
        if let Some(u) = n.url {
            fm.push_str(&format!("url: {}\n", yaml_str(u)));
        }
        if let Some(r) = n.repo {
            fm.push_str(&format!("repo: {}\n", yaml_str(r)));
        }
        fm.push_str(&format!("created: {today}\ntags:\n  - issue\n---\n"));
        let link = n
            .url
            .map(|u| format!("\n[Open {} issue]({})\n", n.source, u))
            .unwrap_or_default();
        let body = format!(
            "# {} {}\n{}\n## Notes\n\n\n## Sessions\n",
            n.id, n.title, link
        );
        fs::write(&path, fm + &body)?;
        let note = self
            .read_note(&path, Stage::Active)
            .context("reading back new note")?;
        Ok((note, true))
    }

    /// Move a note between active/done/archive and update its `status:`.
    pub fn move_to(&self, note: &Note, stage: Stage) -> Result<Note> {
        if note.stage == stage {
            return Ok(note.clone());
        }
        let file = note.path.file_name().context("note has no file name")?;
        let dest = self.root.join(stage.dir()).join(file);
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        let text = fs::read_to_string(&note.path)?;
        let text = set_frontmatter_key(&text, "status", stage.dir());
        fs::write(&note.path, text)?;
        fs::rename(&note.path, &dest)?;
        let mut n = note.clone();
        n.path = dest;
        n.stage = stage;
        Ok(n)
    }

    /// Append a line under the "## Sessions" heading (or at the end).
    pub fn append_session_line(&self, note: &Note, line: &str) -> Result<()> {
        let text = fs::read_to_string(&note.path)?;
        let mut out = text.trim_end_matches('\n').to_string();
        if !out.contains("\n## Sessions") {
            out.push_str("\n\n## Sessions");
        }
        // Sessions is the last section in the template; appending at the end
        // never touches anything the user wrote above it.
        out.push_str(&format!("\n{line}\n"));
        fs::write(&note.path, out)?;
        Ok(())
    }

    pub fn next_local_id(&self, prefix: &str) -> String {
        let max = self
            .scan()
            .iter()
            .filter_map(|n| n.id.strip_prefix(&format!("{prefix}-"))?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        format!("{prefix}-{}", max + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Přidat podporu pro Žluťoučký kůň!"), "pridat-podporu-pro-zlutoucky-kun");
        assert_eq!(slugify("  Fix: login / OAuth  "), "fix-login-oauth");
        assert_eq!(slugify("???"), "untitled");
        assert_eq!(sanitize_id("owner/repo#12"), "owner-repo-12");
        assert_eq!(file_stem("PROJ-123", "Session naming request"), "PROJ-123-session-naming-request");
    }

    #[test]
    fn frontmatter_roundtrip() {
        let t = "---\nid: \"PROJ-1\"\ntitle: \"a: b\"\nstatus: active\ntags:\n  - issue\n---\n# body\n";
        let fm = parse_frontmatter(t);
        assert_eq!(fm["id"], "PROJ-1");
        assert_eq!(fm["title"], "a: b");
        let t2 = set_frontmatter_key(t, "status", "done");
        assert!(t2.contains("status: done"));
        assert!(!t2.contains("status: active"));
        assert!(t2.ends_with("# body\n"));
    }

    #[test]
    fn create_move() {
        let dir = std::env::temp_dir().join(format!("office-test-{}", uuid::Uuid::new_v4()));
        let v = Vault::new(dir.clone()).unwrap();
        let (n, created) = v
            .create(NewNote { id: "GH-7", title: "Mapa dlaždic", source: "github", url: Some("https://x"), repo: Some("o/r") })
            .unwrap();
        assert!(created);
        assert!(n.path.ends_with("active/GH-7-mapa-dlazdic.md"));
        let (_, again) = v
            .create(NewNote { id: "GH-7", title: "other", source: "github", url: None, repo: None })
            .unwrap();
        assert!(!again);
        let d = v.move_to(&n, Stage::Done).unwrap();
        assert!(d.path.ends_with("done/GH-7-mapa-dlazdic.md"));
        assert_eq!(v.find("GH-7").unwrap().stage, Stage::Done);
        v.append_session_line(&d, "- test").unwrap();
        assert!(fs::read_to_string(&d.path).unwrap().trim_end().ends_with("- test"));
        assert_eq!(v.next_local_id("LOCAL"), "LOCAL-1");
        fs::remove_dir_all(dir).ok();
    }
}
