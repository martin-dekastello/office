#![allow(clippy::collapsible_if)]
mod api;
mod app;
mod client;
mod clipboard;
mod complete;
mod config;
mod instance;
mod keys;
mod proto;
mod providers;
mod server;
mod state;
mod status;
mod term;
mod theme;
mod transcript;
mod ui;
mod vault;
mod worktree;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use instance::Instance;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "office",
    version,
    about = "Issues on the left, Claude Code tabs on the right, notes in Obsidian.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Folder to run office in (default: the current directory). One office
    /// runs per folder, in the background; its state lives in <folder>/.office.
    folder: Option<PathBuf>,
    /// Attach to the office already running in the folder.
    #[arg(long, short)]
    attach: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a folder's office in the foreground, without a terminal (started by `office`).
    #[command(hide = true)]
    Server { folder: PathBuf },
    /// Called by Claude Code hooks (reads the hook JSON on stdin).
    Hook,
    /// Add salash status hooks to ~/.claude/settings.json (only needed if inject_hooks = false).
    InstallHooks,
    /// Create a local issue note.
    New { title: Vec<String> },
    /// Pull issues from GitHub/Jira and create missing notes.
    Sync,
    /// List issues in the vault.
    List,
    /// Print config, data and notes paths.
    Paths,
    /// Work with tabs of the running salash.
    Tab {
        #[command(subcommand)]
        cmd: TabCmd,
    },
    /// Install the /new-salash-tab Claude Code skill into ~/.claude/skills.
    InstallSkill,
}

#[derive(Subcommand)]
enum TabCmd {
    /// Open a tab in an issue of the running salash, in the background.
    New {
        /// Issue id; defaults to $SALASH_ISSUE (set inside salash tabs).
        #[arg(long)]
        issue: Option<String>,
        #[arg(long, value_enum, default_value = "claude")]
        kind: TabKindArg,
        /// Directory to start in; defaults to the issue's worktree.
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        /// Tab name (and Claude session name).
        #[arg(long)]
        name: Option<String>,
        /// Claude permission mode: plan, auto, acceptEdits, manual, dontAsk, bypassPermissions.
        #[arg(long)]
        mode: Option<String>,
        /// File whose text is Claude's first prompt.
        #[arg(long)]
        prompt_file: Option<std::path::PathBuf>,
        /// Folder of the office to open it in; defaults to this tab's office,
        /// else the nearest folder above with a .office.
        #[arg(long)]
        folder: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TabKindArg {
    Claude,
    Shell,
    Notes,
}

const SKILL: &str = include_str!("../skills/new-salash-tab/SKILL.md");

/// The office a command run here belongs to, if any. Its data dir applies
/// from here on.
fn here() -> Option<Instance> {
    let inst = Instance::find(&std::env::current_dir().ok()?)?;
    config::set_data_dir(inst.dir.clone());
    Some(inst)
}

/// The config, with the folder's own on top when run inside one.
fn load_config() -> Result<config::Config> {
    let inst = here();
    Ok(config::Config::load_for(inst.as_ref().map(|i| i.dir.as_path()))?.0)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Server { folder }) => server::run(Instance::new(Some(&folder))?),
        Some(Cmd::Hook) => {
            let _ = status::run_hook();
            Ok(())
        }
        Some(Cmd::InstallHooks) => {
            let p = status::install_global()?;
            println!("Hooks added to {}", p.display());
            Ok(())
        }
        Some(Cmd::New { title }) => {
            let cfg = load_config()?;
            let v = vault::Vault::new(cfg.notes_root())?;
            let id = v.next_local_id(&cfg.local_prefix);
            let title = title.join(" ");
            let (n, _) = v.create(vault::NewNote { id: &id, title: &title, source: "local", url: None, repo: None, stage: vault::Stage::Todo })?;
            println!("{}", n.path.display());
            Ok(())
        }
        Some(Cmd::Sync) => {
            let cfg = load_config()?;
            let v = vault::Vault::new(cfg.notes_root())?;
            let rep = providers::fetch_all(&providers::from_config(&cfg));
            for i in &rep.fetched {
                let (n, created) = v.create(vault::NewNote {
                    id: &i.id,
                    title: &i.title,
                    source: i.source,
                    url: i.url.as_deref(),
                    repo: i.repo.as_deref(),
                    stage: vault::Stage::Todo,
                })?;
                if created {
                    println!("+ {}", n.path.display());
                }
            }
            for e in &rep.errors {
                eprintln!("! {e}");
            }
            println!("{} issues fetched", rep.fetched.len());
            Ok(())
        }
        Some(Cmd::List) => {
            let cfg = load_config()?;
            let v = vault::Vault::new(cfg.notes_root())?;
            for n in v.scan() {
                println!("{:<8} {:<16} {}", n.stage.dir(), n.id, n.title);
            }
            Ok(())
        }
        Some(Cmd::Paths) => {
            let inst = here();
            let cfg = config::Config::load_for(inst.as_ref().map(|i| i.dir.as_path()))?.0;
            if let Some(i) = &inst {
                println!("folder  {}", i.folder.display());
                #[cfg(unix)]
                println!("socket  {}", i.socket_path().display());
            }
            println!("config  {}", config::config_path().display());
            println!("data    {}", config::data_dir().display());
            println!("notes   {}", cfg.notes_root().display());
            println!("worktrees {}", cfg.worktrees_dir.display());
            Ok(())
        }
        Some(Cmd::Tab { cmd: TabCmd::New { issue, kind, cwd, name, mode, prompt_file, folder } }) => {
            match folder {
                Some(f) => config::set_data_dir(Instance::new(Some(&f))?.dir),
                None => {
                    here();
                }
            }
            let issue = issue
                .or_else(|| std::env::var("SALASH_ISSUE").ok())
                .context("not inside a salash tab: pass --issue")?;
            let cwd = cwd
                .map(|d| std::fs::canonicalize(config::expand(&d)).with_context(|| format!("not a directory: {}", d.display())))
                .transpose()?;
            let prompt = prompt_file
                .map(|f| std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display())))
                .transpose()?;
            let kind = match kind {
                TabKindArg::Claude => state::TabKind::Claude,
                TabKindArg::Shell => state::TabKind::Shell,
                TabKindArg::Notes => state::TabKind::Notes,
            };
            let req = api::Request { issue: issue.clone(), kind, cwd, name, mode, prompt };
            let reply = api::submit(&req)?;
            if !reply.ok {
                bail!("{}", reply.error.unwrap_or_else(|| "failed".into()));
            }
            println!("tab:{} issue:{issue}", reply.tab.unwrap_or_default());
            Ok(())
        }
        Some(Cmd::InstallSkill) => {
            let p = status::claude_dir().join("skills").join("new-salash-tab").join("SKILL.md");
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, SKILL)?;
            println!("Skill written to {}", p.display());
            Ok(())
        }
        None => {
            let inst = Instance::new(cli.folder.as_deref())?;
            if cli.attach { client::attach(&inst) } else { client::start(&inst) }
        }
    }
}
