//! Pluggable issue sources. Each provider returns open issues; syncing
//! creates a note in todo/ for any issue that has no note yet.

use crate::config::{Config, GithubSource, JiraSource};
use crate::token::{self, Lookup, Token};
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RemoteIssue {
    pub id: String,
    pub title: String,
    pub url: Option<String>,
    pub source: &'static str,
    /// GitHub "owner/name" or Jira site URL, used to find the local repo.
    pub repo: Option<String>,
}

pub trait Provider: Send {
    fn name(&self) -> String;
    fn fetch(&self) -> Result<Vec<RemoteIssue>>;
    /// Where this source's token comes from, for `salash token status`. Never the token.
    fn auth(&self) -> String;
}

/// A fine-grained token for the repo's owner with only Issues: read and
/// Metadata: read, prefilled through GitHub's template URL.
fn github_token_url(repo: &str) -> String {
    let (host, owner) = match repo.split('/').collect::<Vec<_>>()[..] {
        [host, owner, _] => (host, owner),
        [owner, ..] => ("github.com", owner),
        [] => ("github.com", repo),
    };
    format!(
        "https://{host}/settings/personal-access-tokens/new?name=salash&description=salash+issue+sync&target_name={}&issues=read&metadata=read",
        urlencoding::encode(owner)
    )
}

/// Whether a `gh` error looks like a missing or wrong token. GitHub answers
/// "Could not resolve to a Repository" for a private repo the token can't see.
fn looks_like_auth(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    ["401", "403", "bad credentials", "auth", "not accessible", "could not resolve to a repository"]
        .iter()
        .any(|s| e.contains(s))
}

const JIRA_TOKEN_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";
const CLOUD_ID_HELP: &str = "https://support.atlassian.com/jira/kb/retrieve-my-atlassian-sites-cloud-id/";
/// Scoped Atlassian API tokens only work through this gateway.
const JIRA_GATEWAY: &str = "https://api.atlassian.com";
/// How long one Jira request may take in all, so a slow site can't hold up sync.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Github(pub GithubSource);

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    url: String,
}

impl Github {
    /// The env vars `gh` reads a token from for this repo's host, ours first
    /// (`gh help environment`): GitHub Enterprise Server has its own.
    fn gh_vars(&self) -> [&'static str; 2] {
        match self.0.repo.split('/').collect::<Vec<_>>()[..] {
            [host, _, _] if host != "github.com" && !host.ends_with(".ghe.com") => {
                ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
            }
            _ => ["GH_TOKEN", "GITHUB_TOKEN"],
        }
    }

    fn token(&self) -> Lookup {
        token::find(&self.name(), self.0.token_env.as_deref(), self.gh_vars()[0])
    }

    /// What `gh` uses when salash gives it no token.
    fn gh_fallback(&self, env: impl Fn(&str) -> Option<String>) -> String {
        let var = self.gh_vars()[1];
        match env(var).filter(|v| !v.is_empty()) {
            Some(_) => format!("${var}, which gh reads itself,"),
            None => "gh's own login".into(),
        }
    }

    /// `gh issue list` for this repo. A token goes to `gh` as `GH_TOKEN` (or
    /// `GH_ENTERPRISE_TOKEN`) in its own environment only; without one, `gh`
    /// uses its own login.
    fn command(&self, token: Option<&Token>) -> Command {
        let search = self.0.search.clone().unwrap_or_else(|| "assignee:@me".into());
        let mut cmd = Command::new("gh");
        cmd.args(["issue", "list", "--repo", &self.0.repo, "--state", "open", "--limit", "200"])
            .args(["--json", "number,title,url"]);
        if !search.is_empty() {
            cmd.args(["--search", &search]);
        }
        if let Some(t) = token {
            cmd.env(self.gh_vars()[0], t.expose());
        }
        cmd
    }

    fn token_hint(&self) -> String {
        let owner = self.0.repo.split('/').rev().nth(1).unwrap_or(&self.0.repo);
        format!(
            "Store one with `salash token set {}` (Issues: read): {} (if {owner} can't be the token's owner, e.g. you only collaborate on the repo, use a classic token with the repo scope)",
            self.name(),
            github_token_url(&self.0.repo)
        )
    }
}

impl Provider for Github {
    fn name(&self) -> String {
        format!("github:{}", self.0.repo)
    }

    fn fetch(&self) -> Result<Vec<RemoteIssue>> {
        let found = self.token();
        if found.token.is_none() && self.0.token_env.is_some() {
            // Not gh's login or $GH_TOKEN: those are likely another account.
            bail!("no token: looked in {}. {}", found.looked(), self.token_hint());
        }
        let out = self
            .command(found.token.as_ref().map(|(t, _)| t))
            .output()
            .context("running `gh` (is the GitHub CLI installed?)")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            match &found.token {
                Some((_, origin)) => bail!("gh: {err} (token from {origin})"),
                None if !looks_like_auth(err) => bail!("gh: {err}"),
                None => bail!(
                    "gh: {err}. No token in {}, so {} was used. {}",
                    found.looked(),
                    self.gh_fallback(|k| std::env::var(k).ok()),
                    self.token_hint()
                ),
            }
        }
        let issues: Vec<GhIssue> = serde_json::from_slice(&out.stdout).context("parsing gh output")?;
        let prefix = self.0.id_prefix.clone().unwrap_or_else(|| {
            self.0.repo.rsplit('/').next().unwrap_or(&self.0.repo).to_string()
        });
        Ok(issues
            .into_iter()
            .map(|i| RemoteIssue {
                id: format!("{}-{}", prefix, i.number),
                title: i.title,
                url: Some(i.url),
                source: "github",
                repo: Some(self.0.repo.clone()),
            })
            .collect())
    }

    fn auth(&self) -> String {
        self.token().describe(|looked| match self.0.token_env {
            Some(_) => format!("none: looked in {looked}; sync fails until one is set"),
            None => format!("{} (no token in {looked})", self.gh_fallback(|k| std::env::var(k).ok())),
        })
    }
}

pub struct Jira(pub JiraSource);

#[derive(Deserialize)]
struct JiraSearch {
    issues: Vec<JiraIssue>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct JiraIssue {
    key: String,
    fields: JiraFields,
}

#[derive(Deserialize)]
struct JiraFields {
    summary: String,
}

#[derive(Deserialize)]
struct TenantInfo {
    #[serde(rename = "cloudId")]
    cloud_id: String,
}

fn http() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| ureq::Agent::config_builder().timeout_global(Some(HTTP_TIMEOUT)).build().into())
}

/// The gateway base each site's token needed last time, so a scoped token
/// goes there directly instead of asking the site and its cloudId first.
fn gateways() -> &'static Mutex<HashMap<String, String>> {
    static G: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    G.get_or_init(Mutex::default)
}

impl Jira {
    fn site(&self) -> &str {
        self.0.url.trim_end_matches('/')
    }

    fn token(&self) -> Lookup {
        token::find(&self.name(), self.0.token_env.as_deref(), "JIRA_API_TOKEN")
    }

    /// Search the site; if it refuses the token (a scoped token, which only
    /// works through the gateway), search `gateway` with the site's cloudId.
    fn fetch_with(&self, found: Lookup, gateway: &str) -> Result<Vec<RemoteIssue>> {
        let Some((token, origin)) = &found.token else {
            bail!(
                "no API token: looked in {}. Create a scoped token with read:jira-work at {JIRA_TOKEN_URL}, then run `salash token set {}`",
                found.looked(),
                self.name()
            )
        };
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.0.email, token.expose()));
        let jql = self.0.jql.clone().unwrap_or_else(|| {
            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".into()
        });
        let site = self.site();
        // None: the gateway, whose cloudId is looked up only if needed.
        let tries = match gateways().lock().unwrap().get(site) {
            Some(gw) => [Some(gw.clone()), Some(site.to_string())],
            None => [Some(site.to_string()), None],
        };
        let mut first = None;
        for base in tries {
            let base = match base {
                Some(b) => b,
                None => format!("{gateway}/ex/jira/{}", urlencoding::encode(&cloud_id(site)?)),
            };
            // ureq's errors don't include request headers, so the token can't
            // reach these messages. Check again when upgrading ureq.
            match search(&base, &auth, &jql, None) {
                Ok(r) => {
                    first = Some((base, r));
                    break;
                }
                Err(ureq::Error::StatusCode(401)) => continue,
                Err(e) => return Err(anyhow::Error::new(e).context("Jira request failed")),
            }
        }
        let Some((base, mut res)) = first else {
            gateways().lock().unwrap().remove(site);
            bail!(
                "Jira refused the token from {origin} at the site and at api.atlassian.com: check the email and token; a scoped token needs read:jira-work"
            )
        };
        if base == site {
            gateways().lock().unwrap().remove(site);
        } else {
            gateways().lock().unwrap().insert(site.to_string(), base.clone());
        }
        let mut out = Vec::new();
        for pages in 1.. {
            let JiraSearch { issues, next_page_token } = res;
            out.extend(issues.into_iter().map(|i| RemoteIssue {
                url: Some(format!("{site}/browse/{}", i.key)),
                id: i.key,
                title: i.fields.summary,
                source: "jira",
                repo: Some(self.0.url.clone()),
            }));
            match next_page_token {
                Some(t) if pages < 10 => {
                    res = search(&base, &auth, &jql, Some(&t)).context("Jira request failed")?;
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

/// One page of a JQL search. A login Jira refused comes back as a 401, also
/// when Jira Cloud answers it with 200, an empty list and
/// `X-Seraph-LoginReason: AUTHENTICATED_FAILED` (or `AUTHENTICATION_DENIED`),
/// as it does for a scoped token or a wrong one at the site URL
/// (JRACLOUD-96181, JRACLOUD-82932).
fn search(base: &str, auth: &str, jql: &str, page: Option<&str>) -> Result<JiraSearch, ureq::Error> {
    let mut req = http()
        .get(format!("{base}/rest/api/3/search/jql"))
        .header("Authorization", format!("Basic {auth}"))
        .header("Accept", "application/json")
        .query("jql", jql)
        .query("fields", "summary")
        .query("maxResults", "100");
    if let Some(p) = page {
        req = req.query("nextPageToken", p);
    }
    let mut resp = req.call()?;
    let refused = resp
        .headers()
        .get("x-seraph-loginreason")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("AUTHENTICATED_FAILED") || v.starts_with("AUTHENTICATION_DENIED"));
    if refused {
        return Err(ureq::Error::StatusCode(401));
    }
    resp.body_mut().read_json()
}

/// The site's cloudId, which the gateway needs. `_edge/tenant_info` isn't a
/// documented API, but it is what Atlassian's own KB points to, and it
/// answers without logging in.
fn cloud_id(site: &str) -> Result<String> {
    http()
        .get(format!("{site}/_edge/tenant_info"))
        .call()
        .and_then(|mut r| r.body_mut().read_json::<TenantInfo>())
        .map_err(anyhow::Error::new)
        .and_then(|t| match t.cloud_id.trim() {
            "" => bail!("empty cloudId"),
            id => Ok(id.to_string()),
        })
        .with_context(|| {
            format!("the site refused the token; reading its cloudId for a scoped token from {site}/_edge/tenant_info failed (see {CLOUD_ID_HELP})")
        })
}

impl Provider for Jira {
    fn name(&self) -> String {
        format!("jira:{}", self.site())
    }

    fn fetch(&self) -> Result<Vec<RemoteIssue>> {
        if !self.site().starts_with("https://") {
            bail!("url has to start with https://: the token goes with every request");
        }
        self.fetch_with(self.token(), JIRA_GATEWAY)
    }

    fn auth(&self) -> String {
        self.token().describe(|looked| format!("none: looked in {looked}; run `salash token set {}`", self.name()))
    }
}

pub fn from_config(cfg: &Config) -> Vec<Box<dyn Provider>> {
    let mut v: Vec<Box<dyn Provider>> = Vec::new();
    for g in &cfg.github {
        v.push(Box::new(Github(g.clone())));
    }
    for j in &cfg.jira {
        v.push(Box::new(Jira(j.clone())));
    }
    v
}

pub struct SyncReport {
    pub fetched: Vec<RemoteIssue>,
    pub errors: Vec<String>,
}

/// Fetch from all providers (blocking; run on a background thread).
pub fn fetch_all(providers: &[Box<dyn Provider>]) -> SyncReport {
    let mut fetched = Vec::new();
    let mut errors = Vec::new();
    for p in providers {
        match p.fetch() {
            Ok(v) => fetched.extend(v),
            Err(e) => errors.push(format!("{}: {:#}", p.name(), e)),
        }
    }
    SyncReport { fetched, errors }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Keychain;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    struct NoKeychain;

    impl Keychain for NoKeychain {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    fn with_token(t: &str) -> Lookup {
        let t = t.to_string();
        token::lookup("x", None, "T", move |_| Some(t.clone()), &NoKeychain)
    }

    fn without_token() -> Lookup {
        token::lookup("x", None, "T", |_| None, &NoKeychain)
    }

    fn github_repo(repo: &str) -> Github {
        Github(GithubSource { repo: repo.into(), path: None, id_prefix: None, search: None, token_env: None })
    }

    fn github() -> Github {
        github_repo("acme/app")
    }

    fn jira(url: &str) -> Jira {
        Jira(JiraSource { url: url.into(), email: "me@x.io".into(), token_env: None, jql: None, path: None })
    }

    #[test]
    fn gh_gets_the_token_in_its_env_only() {
        let found = with_token("s3cret");
        let cmd = github().command(found.token.as_ref().map(|(t, _)| t));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert_eq!(envs, vec![(std::ffi::OsStr::new("GH_TOKEN"), Some(std::ffi::OsStr::new("s3cret")))]);
        assert!(cmd.get_args().all(|a| !a.to_string_lossy().contains("s3cret")));
        assert_eq!(github().command(None).get_envs().count(), 0);
    }

    #[test]
    fn github_link_asks_for_issues_and_metadata_read_only() {
        let url = github_token_url("acme/app");
        assert!(url.starts_with("https://github.com/settings/personal-access-tokens/new?"));
        assert!(url.contains("target_name=acme&issues=read&metadata=read"));
    }

    #[test]
    fn an_enterprise_server_repo_gets_gh_enterprise_token_and_its_own_link() {
        let g = github_repo("ghe.corp/team/app");
        let found = with_token("s3cret");
        let cmd = g.command(found.token.as_ref().map(|(t, _)| t));
        let envs: Vec<_> = cmd.get_envs().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        assert_eq!(envs, vec!["GH_ENTERPRISE_TOKEN"]);
        assert!(github_token_url(&g.0.repo).starts_with("https://ghe.corp/settings/personal-access-tokens/new?"));
        assert!(github_token_url(&g.0.repo).contains("target_name=team&"));
        assert_eq!(github_repo("github.com/acme/app").gh_vars()[0], "GH_TOKEN");
        assert_eq!(github_repo("acme.ghe.com/acme/app").gh_vars()[0], "GH_TOKEN");
    }

    #[test]
    fn gh_fallback_names_github_token_when_gh_would_use_it() {
        assert_eq!(github().gh_fallback(|_| None), "gh's own login");
        let f = github().gh_fallback(|k| (k == "GITHUB_TOKEN").then(|| "ci".to_string()));
        assert!(f.starts_with("$GITHUB_TOKEN"), "{f}");
        let f = github_repo("ghe.corp/team/app").gh_fallback(|k| (k == "GITHUB_ENTERPRISE_TOKEN").then(|| "x".to_string()));
        assert!(f.starts_with("$GITHUB_ENTERPRISE_TOKEN"), "{f}");
    }

    #[test]
    fn only_auth_errors_get_the_token_hint() {
        assert!(looks_like_auth("HTTP 401: Bad credentials (https://api.github.com/graphql)"));
        assert!(looks_like_auth("To get started with GitHub CLI, please run:  gh auth login"));
        assert!(looks_like_auth("GraphQL: Could not resolve to a Repository with the name 'acme/app'."));
        assert!(!looks_like_auth("error connecting to api.github.com"));
    }

    #[test]
    fn plain_http_jira_is_refused_before_sending_the_token() {
        let e = jira("http://x.atlassian.net").fetch().unwrap_err();
        assert!(e.to_string().contains("https://"), "{e}");
    }

    #[test]
    fn jira_names_drop_a_trailing_slash() {
        assert_eq!(jira("https://x.atlassian.net/").name(), "jira:https://x.atlassian.net");
    }

    #[test]
    fn jira_without_a_token_says_where_it_looked() {
        let e = jira("https://x.atlassian.net").fetch_with(without_token(), "http://unused").unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("$T (unset)"), "{e}");
        assert!(e.contains(JIRA_TOKEN_URL), "{e}");
        assert!(e.contains("salash token set jira:https://x.atlassian.net"), "{e}");
    }

    /// A tiny HTTP server: answers each request with `route(path)` (status,
    /// extra header lines, body) and records "path auth" for every request.
    fn serve(route: fn(&str) -> (u16, &'static str, String)) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let log = seen.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut r = BufReader::new(&s);
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let mut auth = String::new();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap() <= 2 {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("authorization: ") {
                        auth = v.trim().to_string();
                    }
                }
                log.lock().unwrap().push(format!("{path} {auth}"));
                let (code, headers, body) = route(&path);
                let _ = write!(
                    &s,
                    "HTTP/1.1 {code} X\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, seen)
    }

    const ISSUES: &str = r#"{"issues":[{"key":"P-1","fields":{"summary":"First"}}]}"#;
    /// What Jira Cloud's site answers a login it refused: 200 and no issues.
    const REFUSED: &str = "X-Seraph-LoginReason: AUTHENTICATED_FAILED\r\n";
    const NONE: &str = r#"{"issues":[],"isLast":true}"#;

    #[test]
    fn a_scoped_token_refused_by_the_site_goes_through_the_gateway() {
        let (base, seen) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") {
                (200, "", r#"{"cloudId":"abc-ecosystem"}"#.into())
            } else if path.starts_with("/gw/ex/jira/abc-ecosystem/rest/api/3/search/jql") {
                (200, "", ISSUES.into())
            } else {
                (200, REFUSED, NONE.into())
            }
        });
        let issues = jira(&format!("{base}/")).fetch_with(with_token("tok"), &format!("{base}/gw")).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "P-1");
        assert_eq!(issues[0].url.as_deref(), Some(format!("{base}/browse/P-1").as_str()));
        let seen = seen.lock().unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode("me@x.io:tok").to_ascii_lowercase();
        assert!(seen[0].starts_with("/rest/api/3/search/jql"), "{seen:?}");
        assert!(seen[1].starts_with("/_edge/tenant_info"), "{seen:?}");
        assert!(seen[2].starts_with("/gw/ex/jira/abc-ecosystem/"), "{seen:?}");
        assert!(seen[2].ends_with(&format!("basic {auth}")), "{seen:?}");
    }

    #[test]
    fn a_site_401_also_goes_through_the_gateway() {
        let (base, _) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") {
                (200, "", r#"{"cloudId":"c1"}"#.into())
            } else if path.starts_with("/gw/") {
                (200, "", ISSUES.into())
            } else {
                (401, "", "{}".into())
            }
        });
        let issues = jira(&base).fetch_with(with_token("tok"), &format!("{base}/gw")).unwrap();
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn the_gateway_pages_and_is_remembered_for_the_next_sync() {
        let (base, seen) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") {
                (200, "", r#"{"cloudId":"c1"}"#.into())
            } else if path.starts_with("/gw/") && path.contains("nextPageToken=p2") {
                (200, "", r#"{"issues":[{"key":"P-2","fields":{"summary":"Second"}}]}"#.into())
            } else if path.starts_with("/gw/") {
                (200, "", r#"{"issues":[{"key":"P-1","fields":{"summary":"First"}}],"nextPageToken":"p2"}"#.into())
            } else {
                (200, REFUSED, NONE.into())
            }
        });
        let j = jira(&base);
        let ids = |v: Vec<RemoteIssue>| v.into_iter().map(|i| i.id).collect::<Vec<_>>();
        assert_eq!(ids(j.fetch_with(with_token("tok"), &format!("{base}/gw")).unwrap()), ["P-1", "P-2"]);
        assert_eq!(seen.lock().unwrap().len(), 4, "{:?}", seen.lock().unwrap());
        assert!(seen.lock().unwrap()[3].contains("nextPageToken=p2"));
        seen.lock().unwrap().clear();
        assert_eq!(ids(j.fetch_with(with_token("tok"), &format!("{base}/gw")).unwrap()), ["P-1", "P-2"]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen.iter().all(|r| r.starts_with("/gw/ex/jira/c1/")), "{seen:?}");
    }

    #[test]
    fn an_empty_answer_without_a_refusal_is_just_no_issues() {
        let (base, seen) = serve(|_| (200, "", NONE.into()));
        assert!(jira(&base).fetch_with(with_token("tok"), "http://unused").unwrap().is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_classic_token_stays_on_the_site() {
        let (base, seen) = serve(|path| {
            if path.starts_with("/rest/api/3/search/jql") { (200, "", ISSUES.into()) } else { (404, "", "{}".into()) }
        });
        let issues = jira(&base).fetch_with(with_token("tok"), "http://unused").unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_cloud_id_points_at_atlassians_help() {
        let (base, _) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") { (200, "", r#"{"other":1}"#.into()) } else { (200, REFUSED, NONE.into()) }
        });
        let e = jira(&base).fetch_with(with_token("tok"), &format!("{base}/gw")).unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains(CLOUD_ID_HELP), "{e}");
    }

    #[test]
    fn a_cloud_id_that_isnt_json_or_cant_be_reached_points_at_atlassians_help() {
        let (base, _) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") { (200, "", "<html>".into()) } else { (200, REFUSED, NONE.into()) }
        });
        let e = format!("{:#}", jira(&base).fetch_with(with_token("tok"), &format!("{base}/gw")).unwrap_err());
        assert!(e.contains(CLOUD_ID_HELP), "{e}");
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let e = format!("{:#}", cloud_id(&format!("http://{closed}")).unwrap_err());
        assert!(e.contains(CLOUD_ID_HELP), "{e}");
    }

    #[test]
    fn a_token_refused_everywhere_names_its_origin_not_its_value() {
        let (base, _) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") {
                (200, "", r#"{"cloudId":"c1"}"#.into())
            } else if path.starts_with("/gw/") {
                (401, "", "{}".into())
            } else {
                (200, REFUSED, NONE.into())
            }
        });
        let e = jira(&base).fetch_with(with_token("s3cret"), &format!("{base}/gw")).unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("refused the token from $T"), "{e}");
        assert!(!e.contains("s3cret"), "{e}");
    }
}
