#![allow(clippy::collapsible_if)]
mod app;
mod clipboard;
mod complete;
mod config;
mod keys;
mod providers;
mod state;
mod status;
mod term;
mod transcript;
mod ui;
mod vault;
mod worktree;

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;

#[derive(Parser)]
#[command(name = "office", version, about = "Issues on the left, Claude Code tabs on the right, notes in Obsidian.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Called by Claude Code hooks (reads the hook JSON on stdin).
    Hook,
    /// Add office status hooks to ~/.claude/settings.json (only needed if inject_hooks = false).
    InstallHooks,
    /// Create a local issue note.
    New { title: Vec<String> },
    /// Pull issues from GitHub/Jira and create missing notes.
    Sync,
    /// List issues in the vault.
    List,
    /// Print config, data and notes paths.
    Paths,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
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
            let (cfg, _) = config::Config::load()?;
            let v = vault::Vault::new(cfg.notes_root())?;
            let id = v.next_local_id(&cfg.local_prefix);
            let title = title.join(" ");
            let (n, _) = v.create(vault::NewNote { id: &id, title: &title, source: "local", url: None, repo: None })?;
            println!("{}", n.path.display());
            Ok(())
        }
        Some(Cmd::Sync) => {
            let (cfg, _) = config::Config::load()?;
            let v = vault::Vault::new(cfg.notes_root())?;
            let rep = providers::fetch_all(&providers::from_config(&cfg));
            for i in &rep.fetched {
                let (n, created) = v.create(vault::NewNote {
                    id: &i.id,
                    title: &i.title,
                    source: i.source,
                    url: i.url.as_deref(),
                    repo: i.repo.as_deref(),
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
            let (cfg, _) = config::Config::load()?;
            let v = vault::Vault::new(cfg.notes_root())?;
            for n in v.scan() {
                println!("{:<8} {:<16} {}", n.stage.dir(), n.id, n.title);
            }
            Ok(())
        }
        Some(Cmd::Paths) => {
            let (cfg, _) = config::Config::load()?;
            println!("config  {}", config::config_path().display());
            println!("data    {}", config::data_dir().display());
            println!("notes   {}", cfg.notes_root().display());
            println!("worktrees {}", cfg.worktrees_dir.display());
            Ok(())
        }
        None => run_tui(),
    }
}

fn run_tui() -> Result<()> {
    let (cfg, first_run) = config::Config::load()?;
    let mouse = cfg.mouse;
    let mut app = app::App::new(cfg, first_run)?;

    let mut terminal = ratatui::init();
    let mut out = std::io::stdout();
    let _ = execute!(out, EnableBracketedPaste);
    if mouse {
        let _ = execute!(out, EnableMouseCapture);
    }
    let enhanced = matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true));
    if enhanced {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    let res = app::run(&mut app, &mut terminal);

    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, DisableBracketedPaste);
    ratatui::restore();
    drop(app); // kills child processes
    res
}
