//! `salash mcp [folder]`: an MCP server on stdio that lets a Claude session
//! see the folder's salash: issues, tabs, sessions, their status and screens,
//! and open a tab.
//!
//! It asks the folder's running server over the same socket clients attach
//! to (src/query.rs), so the server stays the only owner of tabs and state.
//! It never starts a server. Like tmux control mode, it gets state as text
//! rather than a drawn screen.

use crate::api::Request;
use crate::instance::Instance;
use crate::proto::{ClientMsg, ServerMsg, VERSION};
use crate::query::{Answer, Issue, Query, TabInfo};
use crate::server::Conn;
use crate::state::TabKind;
use crate::term::Snapshot;
use anyhow::{bail, Context, Result};
use interprocess::local_socket::prelude::*;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, ServerCapabilities, ServerConfig,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the server may take to answer. It answers between frames, so
/// anything near this means it is stuck.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Rows `read_screen` returns from scrollback when not told how many.
const SCROLLBACK_LINES: usize = 200;
/// Rows of a saved screen's last page, for `read_screen` of a suspended tab.
const SAVED_ROWS: u16 = 50;

const INSTRUCTIONS: &str = "salash runs Claude Code sessions, shells and note editors in tabs, \
grouped by issue. These tools read its live state: issues and their stage, each tab's status \
(working, attention = needs the user, done, idle), and what a tab shows. Without an id, tools \
act on the caller's own issue or tab; your own tab is marked \"self\": true.";

/// `salash mcp`: serve MCP on stdin/stdout until the client goes.
pub fn run(inst: Instance) -> Result<()> {
    let conn = Link::open(&inst)?;
    let me = Me::from_env(&inst);
    let link = Arc::new(Link { inst, conn: Mutex::new(Some(conn)) });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async move {
        let service = Salash { link, me, tool_router: Salash::tool_router() }
            .serve(rmcp::transport::stdio())
            .await
            .context("starting MCP")?;
        service.waiting().await?;
        Ok(())
    })
}

type Pipe = (BufReader<Conn>, BufWriter<Conn>);

/// The connection to the folder's server. One question at a time; after an
/// error the next question connects again.
struct Link {
    inst: Instance,
    conn: Mutex<Option<Pipe>>,
}

impl Link {
    fn open(inst: &Instance) -> Result<Pipe> {
        let stream = crate::client::connect(inst).map_err(|_| {
            anyhow::anyhow!("no salash running for {}; start salash first", inst.folder.display())
        })?;
        stream.set_recv_timeout(Some(REPLY_TIMEOUT))?;
        let conn = Arc::new(stream);
        let mut recv = BufReader::new(Conn(conn.clone()));
        match ServerMsg::read(&mut recv).context("talking to salash")? {
            Some(ServerMsg::Welcome { version, .. }) if version == VERSION => {}
            Some(ServerMsg::Welcome { .. }) => return Err(crate::client::other_version(inst)),
            Some(ServerMsg::Exit(why)) => bail!("{why}"),
            _ => bail!("salash sent something unexpected"),
        }
        Ok((recv, BufWriter::new(Conn(conn))))
    }

    fn ask(&self, query: Query) -> Result<Answer, String> {
        let mut conn = self.conn.lock().unwrap();
        if conn.is_none() {
            *conn = Some(Link::open(&self.inst).map_err(|e| format!("{e:#}"))?);
        }
        let (recv, send) = conn.as_mut().unwrap();
        let result = exchange(recv, send, query);
        if result.is_err() {
            *conn = None;
        }
        result
    }
}

/// Send one query and wait for its reply. One is in flight at a time, so
/// the id only has to differ from the last one's.
fn exchange(recv: &mut BufReader<Conn>, send: &mut BufWriter<Conn>, query: Query) -> Result<Answer, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let lost = |e: &dyn std::fmt::Display| format!("lost salash: {e}");
    ClientMsg::Query { id, query }.write(send).map_err(|e| lost(&e))?;
    loop {
        match ServerMsg::read(recv) {
            Ok(Some(ServerMsg::Reply { id: got, answer })) if got == id => return Ok(answer),
            Ok(Some(ServerMsg::Exit(why))) => return Err(format!("salash stopped: {why}")),
            Ok(Some(_)) => {}
            Ok(None) => return Err("salash stopped".into()),
            Err(e) => return Err(lost(&e)),
        }
    }
}

/// The salash tab this runs in, if it does: Claude passes its environment on.
#[derive(Clone, Default)]
struct Me {
    tab: Option<String>,
    issue: Option<String>,
}

impl Me {
    /// Only a tab of this folder's salash counts: `salash mcp` set up with
    /// `claude mcp add` can be asked from another folder's tab.
    fn from_env(inst: &Instance) -> Me {
        let ours = std::env::var("SALASH_DATA")
            .ok()
            .and_then(|d| std::fs::canonicalize(d).ok())
            .is_some_and(|d| std::fs::canonicalize(&inst.dir).is_ok_and(|ours| ours == d));
        if !ours {
            return Me::default();
        }
        Me { tab: std::env::var("SALASH_TAB").ok(), issue: std::env::var("SALASH_ISSUE").ok() }
    }
}

#[derive(Clone)]
struct Salash {
    link: Arc<Link>,
    me: Me,
    tool_router: ToolRouter<Salash>,
}

#[derive(Deserialize, JsonSchema)]
struct ListIssues {
    /// Only issues in this stage: todo, active, done or archive. Default: all but archive.
    stage: Option<String>,
    /// Only issues whose most urgent Claude tab has this status: attention, working, done, idle, starting.
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct IssueId {
    /// Issue id, e.g. TS-42, or "scratch". Default: the caller's issue.
    id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListTabs {
    /// Issue id, or "all" for every issue's tabs. Default: the caller's issue.
    issue: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TabId {
    /// Tab id. Default: the caller's tab.
    id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ReadScreen {
    /// Tab id. Default: the caller's tab.
    tab: Option<String>,
    /// How many of the last rows to return (at most 2000). Default: the screen,
    /// or 200 rows with scrollback.
    lines: Option<usize>,
    /// Reach back into the tab's scrollback, above the screen. Default: false.
    scrollback: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Claude,
    Shell,
    Notes,
}

#[derive(Deserialize, JsonSchema)]
struct OpenTab {
    /// Issue id, or "scratch" for no issue. Default: the caller's issue.
    issue: Option<String>,
    /// claude (default), shell, or notes (the issue's note in an editor).
    kind: Option<Kind>,
    /// Directory to start in. Default: the issue's worktree.
    cwd: Option<String>,
    /// Tab name (and Claude session name).
    name: Option<String>,
    /// Claude permission mode for the first launch: plan, auto, acceptEdits or manual.
    mode: Option<String>,
    /// Claude only: the first prompt. The new session starts with no other context.
    prompt: Option<String>,
}

fn json(v: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// The branch checked out in a worktree.
fn branch(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["branch", "--show-current"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let b = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !b.is_empty()).then_some(b)
}

/// An issue as `list_issues` shows it: tabs counted, not listed.
fn summary(i: &Issue) -> Value {
    let mut v = serde_json::to_value(i).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.remove("closed");
        o.insert("tabs".into(), i.tabs.len().into());
        o.insert("branch".into(), i.worktree.as_deref().and_then(branch).into());
    }
    v
}

impl Salash {
    /// Ask the server, off the async runtime: the socket blocks.
    async fn ask(&self, q: Query) -> Result<Answer, String> {
        let link = self.link.clone();
        tokio::task::spawn_blocking(move || link.ask(q)).await.map_err(|e| e.to_string())?
    }

    async fn issues(&self) -> Result<Vec<Issue>, String> {
        match self.ask(Query::State).await? {
            Answer::State(mut s) => {
                for t in s.issues.iter_mut().flat_map(|i| i.tabs.iter_mut()) {
                    t.own = self.me.tab.as_deref() == Some(t.id.as_str());
                }
                Ok(s.issues)
            }
            Answer::Error(e) => Err(e),
            _ => Err("salash answered something else".into()),
        }
    }

    fn issue_or_mine(&self, id: Option<String>) -> Result<String, String> {
        id.or_else(|| self.me.issue.clone()).ok_or_else(|| "not inside a salash tab: pass an issue id".into())
    }

    fn tab_or_mine(&self, id: Option<String>) -> Result<String, String> {
        id.or_else(|| self.me.tab.clone()).ok_or_else(|| "not inside a salash tab: pass a tab id".into())
    }

    async fn issue(&self, id: &str) -> Result<Value, String> {
        let issue = self.issues().await?.into_iter().find(|i| i.id == id).ok_or_else(|| format!("no issue {id}"))?;
        let mut v = serde_json::to_value(&issue).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("branch".into(), issue.worktree.as_deref().and_then(branch).into());
        }
        Ok(v)
    }

    async fn tab(&self, id: &str) -> Result<TabInfo, String> {
        self.issues()
            .await?
            .into_iter()
            .flat_map(|i| i.tabs)
            .find(|t| t.id == id)
            .ok_or_else(|| format!("no tab {id}"))
    }

    async fn screen(&self, tab: &str, lines: Option<usize>, scrollback: bool) -> Result<String, String> {
        let lines = lines.unwrap_or(if scrollback { SCROLLBACK_LINES } else { 0 });
        let query = || Query::Screen { tab: tab.to_string(), lines, scrollback };
        // A saved screen can go away as its tab resumes; then ask once more.
        for _ in 0..2 {
            match self.ask(query()).await? {
                Answer::Screen(s) => return Ok(s),
                Answer::Saved(path) => {
                    if let Some(s) = saved_text(&path, lines, scrollback) {
                        return Ok(s);
                    }
                }
                Answer::Error(e) => return Err(e),
                _ => return Err("salash answered something else".into()),
            }
        }
        Err(format!("tab {tab} has no screen right now"))
    }
}

/// The text of a saved screen. With `lines` 0, its last `SAVED_ROWS` rows.
fn saved_text(path: &Path, lines: usize, scrollback: bool) -> Option<String> {
    Some(Snapshot::load(path, SAVED_ROWS)?.text(lines, scrollback))
}

#[tool_router]
impl Salash {
    #[tool(
        description = "List salash's issues: id, title, stage (todo/active/done/archive), provider and URL, note path, worktree and branch, the most urgent Claude status of its tabs, and how many tabs it has. Scratch (tabs outside any issue) is included while it has tabs.",
        annotations(read_only_hint = true)
    )]
    async fn list_issues(&self, Parameters(p): Parameters<ListIssues>) -> Result<String, String> {
        let issues = self.issues().await?;
        let status = p.status.map(|s| s.to_lowercase());
        let v: Vec<Value> = issues
            .iter()
            .filter(|i| match (&p.stage, i.stage) {
                (Some(want), Some(st)) => st.dir() == want.to_lowercase(),
                (Some(_), None) => false,
                (None, st) => st != Some(crate::vault::Stage::Archive),
            })
            .filter(|i| {
                status.as_ref().is_none_or(|want| {
                    i.status.is_some_and(|s| serde_json::to_value(s).ok().and_then(|v| v.as_str().map(String::from)).as_ref() == Some(want))
                })
            })
            .map(summary)
            .collect();
        Ok(json(&v))
    }

    #[tool(
        description = "One issue with all its tabs and its closed Claude tabs (which can be reopened). Default: the caller's issue.",
        annotations(read_only_hint = true)
    )]
    async fn get_issue(&self, Parameters(p): Parameters<IssueId>) -> Result<String, String> {
        let id = self.issue_or_mine(p.id)?;
        Ok(json(&self.issue(&id).await?))
    }

    #[tool(
        description = "Tabs of an issue: id, name, kind (claude/shell/notes), cwd, whether active, process (running/suspended/exited/waiting/stopped), and for Claude tabs the session id, status (starting/idle/working/attention/done/exited), shown_status (done turns idle once the user looked), last hook event and message with time, transcript path and resume command. The caller's own tab has \"self\": true.",
        annotations(read_only_hint = true)
    )]
    async fn list_tabs(&self, Parameters(p): Parameters<ListTabs>) -> Result<String, String> {
        let issues = self.issues().await?;
        let tabs: Vec<&TabInfo> = match p.issue.as_deref() {
            Some("all") => issues.iter().flat_map(|i| &i.tabs).collect(),
            _ => {
                let id = self.issue_or_mine(p.issue)?;
                let i = issues.iter().find(|i| i.id == id).ok_or_else(|| format!("no issue {id}"))?;
                i.tabs.iter().collect()
            }
        };
        Ok(json(&tabs))
    }

    #[tool(description = "One tab, as list_tabs shows it. Default: the caller's tab.", annotations(read_only_hint = true))]
    async fn get_tab(&self, Parameters(p): Parameters<TabId>) -> Result<String, String> {
        let id = self.tab_or_mine(p.id)?;
        Ok(json(&self.tab(&id).await?))
    }

    #[tool(
        description = "What a tab shows, as plain text (like tmux capture-pane -p): its screen, optionally with scrollback above it. A suspended Claude tab gives its saved screen. Output is capped to the last 2000 lines and 32 KiB. Default: the caller's tab.",
        annotations(read_only_hint = true)
    )]
    async fn read_screen(&self, Parameters(p): Parameters<ReadScreen>) -> Result<String, String> {
        let tab = self.tab_or_mine(p.tab)?;
        self.screen(&tab, p.lines, p.scrollback.unwrap_or(false)).await
    }

    #[tool(
        description = "Open a new tab in salash, in the background, like `salash tab new`: a Claude session (optionally with a permission mode and a first prompt), a shell, or the issue's note. Modes bypassPermissions and dontAsk are refused here. Returns the new tab's id.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn open_tab(&self, Parameters(p): Parameters<OpenTab>) -> Result<String, String> {
        let issue = self.issue_or_mine(p.issue)?;
        let cwd = p
            .cwd
            .map(|d| {
                std::fs::canonicalize(crate::config::expand(Path::new(&d))).map_err(|_| format!("not a directory: {d}"))
            })
            .transpose()?;
        let kind = match p.kind.unwrap_or(Kind::Claude) {
            Kind::Claude => TabKind::Claude,
            Kind::Shell => TabKind::Shell,
            Kind::Notes => TabKind::Notes,
        };
        let req = Request { issue: issue.clone(), kind, cwd, name: p.name, mode: p.mode, prompt: p.prompt };
        match self.ask(Query::OpenTab(req)).await? {
            Answer::Opened(tab) => Ok(json(&serde_json::json!({ "tab": tab, "issue": issue }))),
            Answer::Error(e) => Err(e),
            _ => Err("salash answered something else".into()),
        }
    }
}

const ISSUES_URI: &str = "salash://issues";

fn resource_error(e: String) -> ErrorData {
    ErrorData::resource_not_found(e, None)
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Salash {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(Implementation::new("salash", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let issues = self.issues().await.map_err(|e| ErrorData::internal_error(e, None))?;
        let mut all = vec![Resource::new(ISSUES_URI, "issues")
            .with_description("Every issue, as list_issues gives it")
            .with_mime_type("application/json")];
        for i in issues.iter().filter(|i| !i.tabs.is_empty()) {
            all.push(
                Resource::new(format!("salash://issue/{}", i.id), i.id.clone())
                    .with_title(i.title.clone())
                    .with_mime_type("application/json"),
            );
        }
        Ok(ListResourcesResult::with_all_items(all))
    }

    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let t = |uri: &str, name: &str, desc: &str, mime: &str| {
            ResourceTemplate::new(uri, name).with_description(desc).with_mime_type(mime)
        };
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            t("salash://issue/{id}", "issue", "An issue with its tabs", "application/json"),
            t("salash://tab/{id}", "tab", "A tab and its Claude status", "application/json"),
            t("salash://tab/{id}/screen", "screen", "What a tab shows, as plain text", "text/plain"),
        ]))
    }

    async fn read_resource(
        &self,
        req: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = req.uri;
        let (text, mime) = if uri == ISSUES_URI {
            let issues = self.issues().await.map_err(resource_error)?;
            (json(&issues.iter().map(summary).collect::<Vec<_>>()), "application/json")
        } else if let Some(id) = uri.strip_prefix("salash://issue/") {
            (json(&self.issue(id).await.map_err(resource_error)?), "application/json")
        } else if let Some(id) = uri.strip_prefix("salash://tab/").and_then(|r| r.strip_suffix("/screen")) {
            (self.screen(id, None, false).await.map_err(resource_error)?, "text/plain")
        } else if let Some(id) = uri.strip_prefix("salash://tab/") {
            (json(&self.tab(id).await.map_err(resource_error)?), "application/json")
        } else {
            return Err(resource_error(format!("no resource {uri}")));
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri).with_mime_type(mime)]).into())
    }
}

/// The folder `salash mcp` serves: the one given, else the one this runs in.
pub fn instance(folder: Option<PathBuf>) -> Result<Instance> {
    match folder {
        Some(f) => Instance::new(Some(&f)),
        None => Instance::find(&std::env::current_dir()?)
            .context("not in a salash folder; pass one: salash mcp <folder>"),
    }
}
