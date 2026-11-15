//! Pluggable issue sources. Each provider returns open issues; syncing
//! creates a note in todo/ for any issue that has no note yet.

use crate::config::{Config, GithubSource, JiraSource};
use crate::token::{self, Lookup, Origin, Token};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use std::process::Command;

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
    let owner = repo.split('/').next().unwrap_or(repo);
    format!(
        "https://github.com/settings/personal-access-tokens/new?name=salash&description=salash+issue+sync&target_name={}&issues=read&metadata=read",
        urlencoding::encode(owner)
    )
}

const JIRA_TOKEN_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";
const CLOUD_ID_HELP: &str = "https://support.atlassian.com/jira/kb/retrieve-my-atlassian-sites-cloud-id/";
/// Scoped Atlassian API tokens only work through this gateway.
const JIRA_GATEWAY: &str = "https://api.atlassian.com";

pub struct Github(pub GithubSource);

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    url: String,
}

impl Github {
    fn token(&self) -> Lookup {
        token::find(&self.name(), self.0.token_env.as_deref(), "GH_TOKEN")
    }

    /// `gh issue list` for this repo. A token goes to `gh` as `GH_TOKEN` in
    /// its own environment only; without one, `gh` uses its own login.
    fn command(&self, token: Option<&Token>) -> Command {
        let search = self.0.search.clone().unwrap_or_else(|| "assignee:@me".into());
        let mut cmd = Command::new("gh");
        cmd.args(["issue", "list", "--repo", &self.0.repo, "--state", "open", "--limit", "200"])
            .args(["--json", "number,title,url"]);
        if !search.is_empty() {
            cmd.args(["--search", &search]);
        }
        if let Some(t) = token {
            cmd.env("GH_TOKEN", t.expose());
        }
        cmd
    }
}

impl Provider for Github {
    fn name(&self) -> String {
        format!("github:{}", self.0.repo)
    }

    fn fetch(&self) -> Result<Vec<RemoteIssue>> {
        let found = self.token();
        let out = self
            .command(found.token.as_ref().map(|(t, _)| t))
            .output()
            .context("running `gh` (is the GitHub CLI installed?)")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            match &found.token {
                Some((_, origin)) => bail!("gh: {err} (token from {origin})"),
                None => bail!(
                    "gh: {err}. No token in {}, so gh's own login was used. Store one with `salash token set {}` (Issues: read): {}",
                    found.looked(),
                    self.name(),
                    github_token_url(&self.0.repo)
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
        let found = self.token();
        match &found.token {
            Some((_, origin)) => origin.to_string(),
            None => format!("gh's own login (no token in {})", found.looked()),
        }
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

impl Jira {
    fn site(&self) -> &str {
        self.0.url.trim_end_matches('/')
    }

    fn token(&self) -> Lookup {
        token::find(&self.name(), self.0.token_env.as_deref(), "JIRA_API_TOKEN")
    }

    fn fetch_with(&self, found: Lookup, gateway: &str) -> Result<Vec<RemoteIssue>> {
        match &found.token {
            Some((t, origin)) => self.fetch_from(t, origin, gateway),
            None => bail!(
                "no API token: looked in {}. Create a scoped token with read:jira-work at {JIRA_TOKEN_URL}, then run `salash token set {}`",
                found.looked(),
                self.name()
            ),
        }
    }

    /// Search the site with a classic API token. A scoped token is refused
    /// there (401), and then goes through `gateway` with the site's cloudId.
    fn fetch_from(&self, token: &Token, origin: &Origin, gateway: &str) -> Result<Vec<RemoteIssue>> {
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.0.email, token.expose()));
        let jql = self.0.jql.clone().unwrap_or_else(|| {
            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".into()
        });
        let site = self.site();
        let mut base = site.to_string();
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        for _ in 0..10 {
            // ureq's errors don't include request headers, so the token can't
            // reach these messages. Check again when upgrading ureq.
            let res = match search(&base, &auth, &jql, page.as_deref()) {
                Ok(r) => r,
                Err(ureq::Error::StatusCode(401)) if page.is_none() && base == site => {
                    base = format!("{gateway}/ex/jira/{}", urlencoding::encode(&cloud_id(site)?));
                    search(&base, &auth, &jql, None).map_err(|e| match e {
                        ureq::Error::StatusCode(401) => anyhow!(
                            "Jira refused the token from {origin} at the site and at api.atlassian.com (401): check the email and token; a scoped token needs read:jira-work"
                        ),
                        e => anyhow::Error::new(e).context("Jira request failed"),
                    })?
                }
                Err(e) => return Err(anyhow::Error::new(e).context("Jira request failed")),
            };
            out.extend(res.issues.into_iter().map(|i| RemoteIssue {
                url: Some(format!("{site}/browse/{}", i.key)),
                id: i.key,
                title: i.fields.summary,
                source: "jira",
                repo: Some(self.0.url.clone()),
            }));
            match res.next_page_token {
                Some(t) => page = Some(t),
                None => break,
            }
        }
        Ok(out)
    }
}

fn search(base: &str, auth: &str, jql: &str, page: Option<&str>) -> Result<JiraSearch, ureq::Error> {
    let mut req = ureq::get(format!("{base}/rest/api/3/search/jql"))
        .header("Authorization", format!("Basic {auth}"))
        .header("Accept", "application/json")
        .query("jql", jql)
        .query("fields", "summary")
        .query("maxResults", "100");
    if let Some(p) = page {
        req = req.query("nextPageToken", p);
    }
    req.call()?.body_mut().read_json()
}

/// The site's cloudId, which the gateway needs. `_edge/tenant_info` isn't a
/// documented API, but it is what Atlassian's own KB points to, and it
/// answers without logging in.
fn cloud_id(site: &str) -> Result<String> {
    ureq::get(format!("{site}/_edge/tenant_info"))
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
        self.fetch_with(self.token(), JIRA_GATEWAY)
    }

    fn auth(&self) -> String {
        let found = self.token();
        match &found.token {
            Some((_, origin)) => origin.to_string(),
            None => format!("none: looked in {}; run `salash token set {}`", found.looked(), self.name()),
        }
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

    fn github() -> Github {
        Github(GithubSource { repo: "acme/app".into(), path: None, id_prefix: None, search: None, token_env: None })
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

    /// A tiny HTTP server: answers each request with `route(path)` and records
    /// "path auth" for every request.
    fn serve(route: fn(&str) -> (u16, String)) -> (String, Arc<Mutex<Vec<String>>>) {
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
                let (code, body) = route(&path);
                let _ = write!(
                    &s,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, seen)
    }

    const ISSUES: &str = r#"{"issues":[{"key":"P-1","fields":{"summary":"First"}}]}"#;

    #[test]
    fn a_scoped_token_refused_by_the_site_goes_through_the_gateway() {
        let (base, seen) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") {
                (200, r#"{"cloudId":"abc-ecosystem"}"#.into())
            } else if path.starts_with("/gw/ex/jira/abc-ecosystem/rest/api/3/search/jql") {
                (200, ISSUES.into())
            } else {
                (401, "{}".into())
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
    fn a_classic_token_stays_on_the_site() {
        let (base, seen) = serve(|path| {
            if path.starts_with("/rest/api/3/search/jql") { (200, ISSUES.into()) } else { (404, "{}".into()) }
        });
        let issues = jira(&base).fetch_with(with_token("tok"), "http://unused").unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_cloud_id_points_at_atlassians_help() {
        let (base, _) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") { (200, r#"{"other":1}"#.into()) } else { (401, "{}".into()) }
        });
        let e = jira(&base).fetch_with(with_token("tok"), &format!("{base}/gw")).unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains(CLOUD_ID_HELP), "{e}");
    }

    #[test]
    fn a_token_refused_everywhere_names_its_origin_not_its_value() {
        let (base, _) = serve(|path| {
            if path.starts_with("/_edge/tenant_info") { (200, r#"{"cloudId":"c1"}"#.into()) } else { (401, "{}".into()) }
        });
        let e = jira(&base).fetch_with(with_token("s3cret"), &format!("{base}/gw")).unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("refused the token from $T"), "{e}");
        assert!(!e.contains("s3cret"), "{e}");
    }
}
