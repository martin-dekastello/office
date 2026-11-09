use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A GitHub repository to pull issues from (via the `gh` CLI).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GithubSource {
    /// "owner/name"
    pub repo: String,
    /// Local clone used to create per-issue worktrees.
    pub path: Option<PathBuf>,
    /// ID prefix for notes, defaults to the repo name ("transport-sim" -> "transport-sim-42").
    pub id_prefix: Option<String>,
    /// `gh issue list --search` query. Default: "assignee:@me".
    pub search: Option<String>,
}

/// A Jira Cloud site to pull issues from (REST API v3).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JiraSource {
    /// e.g. "https://yourcompany.atlassian.net"
    pub url: String,
    pub email: String,
    /// Environment variable that holds the API token. Default: JIRA_API_TOKEN.
    pub token_env: Option<String>,
    /// Default: "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
    pub jql: Option<String>,
    /// Local clone used to create per-issue worktrees.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Obsidian vault root.
    pub vault: PathBuf,
    /// Folder inside the vault holding active/ done/ archive/.
    pub notes_dir: String,
    /// Where per-issue git worktrees are created: <worktrees_dir>/<repo>/<id>-<slug>
    pub worktrees_dir: PathBuf,
    /// Creates an issue's worktree instead of plain `git worktree add`. Run as
    /// `<command…> <repo> <id>-<slug>` in the repo; prints the worktree's path last.
    pub worktree_command: Vec<String>,
    /// Command used for Claude tabs.
    pub claude_command: Vec<String>,
    /// Command used for shell tabs. Empty = $SHELL / %COMSPEC%.
    pub shell_command: Vec<String>,
    /// Command used for the notes tab. Empty = $VISUAL / $EDITOR / vi / notepad.
    pub editor_command: Vec<String>,
    /// Pass a generated `--settings` file to claude so status hooks work
    /// without touching ~/.claude/settings.json.
    pub inject_hooks: bool,
    /// Prefix key, tmux style: "ctrl-a", "ctrl-b", "ctrl-g", ...
    pub prefix: String,
    /// Prefix for locally created issues ("LOCAL-1").
    pub local_prefix: String,
    /// Repo used for worktrees of local issues (optional).
    pub local_repo: Option<PathBuf>,
    /// Ring the host terminal bell when a background tab needs you or finishes.
    pub bell: bool,
    /// Capture the mouse (click sidebar/tabs, wheel and clicks in tabs, drag to copy in shell tabs).
    /// Elsewhere, hold Shift/Option to select text.
    pub mouse: bool,
    pub github: Vec<GithubSource>,
    pub jira: Vec<JiraSource>,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let claude = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "claude".into()]
        } else {
            vec!["claude".into()]
        };
        Config {
            vault: dirs::document_dir()
                .unwrap_or_else(|| home.clone())
                .join("Obsidian"),
            notes_dir: "Issues".into(),
            worktrees_dir: home.join("worktrees"),
            worktree_command: vec![],
            claude_command: claude,
            shell_command: vec![],
            editor_command: vec![],
            inject_hooks: true,
            prefix: "ctrl-a".into(),
            local_prefix: "LOCAL".into(),
            local_repo: None,
            bell: true,
            mouse: true,
            github: vec![],
            jira: vec![],
        }
    }
}

const TEMPLATE: &str = r#"# salash configuration

# Obsidian vault root and the folder inside it that holds active/ done/ archive/
vault = "{VAULT}"
notes_dir = "Issues"

# Per-issue git worktrees go to <worktrees_dir>/<repo>/<id>-<slug>
worktrees_dir = "{WORKTREES}"

# Your own script for creating worktrees, run as `<command…> <repo> <id>-<slug>` in the
# repo when an issue's first Claude or shell tab opens. It must print the worktree's
# path as its last line of output. Default: `git worktree add` into worktrees_dir.
# worktree_command = ["~/bin/make-worktree.sh"]

# tmux-style prefix key. Press it twice to send it to the terminal.
prefix = "ctrl-a"

# claude_command = ["claude"]
# shell_command  = []          # default: $SHELL / %COMSPEC%
# editor_command = ["nvim"]    # default: $VISUAL / $EDITOR
# inject_hooks = true          # pass status hooks via `claude --settings`
# local_prefix = "LOCAL"
# local_repo = "/path/to/repo" # worktrees for local issues
# bell = true
# mouse = true

# [[github]]
# repo = "owner/transport-sim"
# path = "~/code/transport-sim"   # local clone for worktrees
# id_prefix = "TS"                # notes become TS-42-<slug>.md
# search = "assignee:@me"

# [[jira]]
# url = "https://yourcompany.atlassian.net"
# email = "you@example.com"
# token_env = "JIRA_API_TOKEN"
# jql = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
# path = "~/code/backend"
"#;

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SALASH_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("salash")
        .join("config.toml")
}

pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SALASH_DATA") {
        return PathBuf::from(p);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("salash")
}

/// Expand a leading "~" to the home directory.
pub fn expand(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest.trim_start_matches(['/', '\\']));
        }
    }
    p.to_path_buf()
}

/// A path for display, with the home directory as "~".
pub fn shorten(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => p.display().to_string(),
    }
}

impl Config {
    /// Load the config, writing a commented template on first run.
    pub fn load() -> Result<(Config, bool)> {
        let path = config_path();
        let mut created = false;
        if !path.exists() {
            let d = Config::default();
            let text = TEMPLATE
                .replace("{VAULT}", &toml_escape(&d.vault))
                .replace("{WORKTREES}", &toml_escape(&d.worktrees_dir));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
            created = true;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.vault = expand(&cfg.vault);
        cfg.worktrees_dir = expand(&cfg.worktrees_dir);
        cfg.local_repo = cfg.local_repo.as_deref().map(expand);
        for g in &mut cfg.github {
            g.path = g.path.as_deref().map(expand);
        }
        for j in &mut cfg.jira {
            j.path = j.path.as_deref().map(expand);
        }
        Ok((cfg, created))
    }

    pub fn notes_root(&self) -> PathBuf {
        self.vault.join(&self.notes_dir)
    }

    pub fn shell(&self) -> Vec<String> {
        if !self.shell_command.is_empty() {
            return self.shell_command.clone();
        }
        if cfg!(windows) {
            vec![std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())]
        } else {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
        }
    }

    pub fn editor(&self) -> Vec<String> {
        if !self.editor_command.is_empty() {
            return self.editor_command.clone();
        }
        for var in ["VISUAL", "EDITOR"] {
            if let Ok(v) = std::env::var(var) {
                let parts: Vec<String> = v.split_whitespace().map(String::from).collect();
                if !parts.is_empty() {
                    return parts;
                }
            }
        }
        if cfg!(windows) {
            vec!["notepad".into()]
        } else {
            vec!["vi".into()]
        }
    }

    /// Resolve the local repository path for an issue.
    pub fn repo_path_for(&self, source: &str, repo: Option<&str>) -> Option<PathBuf> {
        match source {
            "github" => self
                .github
                .iter()
                .find(|g| Some(g.repo.as_str()) == repo)
                .and_then(|g| g.path.clone()),
            "jira" => self
                .jira
                .iter()
                .find(|j| Some(j.url.as_str()) == repo)
                .or(self.jira.first())
                .and_then(|j| j.path.clone()),
            _ => self.local_repo.clone(),
        }
    }
}

fn toml_escape(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")
}
