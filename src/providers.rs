//! Pluggable issue sources. Each provider returns open issues; syncing
//! creates a note in todo/ for any issue that has no note yet.

use crate::config::{Config, GithubSource, JiraSource};
use anyhow::{bail, Context, Result};
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
}

pub struct Github(pub GithubSource);

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    url: String,
}

impl Provider for Github {
    fn name(&self) -> String {
        format!("github:{}", self.0.repo)
    }

    fn fetch(&self) -> Result<Vec<RemoteIssue>> {
        let search = self.0.search.clone().unwrap_or_else(|| "assignee:@me".into());
        let mut cmd = Command::new("gh");
        cmd.args(["issue", "list", "--repo", &self.0.repo, "--state", "open", "--limit", "200"])
            .args(["--json", "number,title,url"]);
        if !search.is_empty() {
            cmd.args(["--search", &search]);
        }
        let out = cmd.output().context("running `gh` (is the GitHub CLI installed?)")?;
        if !out.status.success() {
            bail!("gh: {}", String::from_utf8_lossy(&out.stderr).trim());
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

impl Provider for Jira {
    fn name(&self) -> String {
        format!("jira:{}", self.0.url)
    }

    fn fetch(&self) -> Result<Vec<RemoteIssue>> {
        let env = self.0.token_env.clone().unwrap_or_else(|| "JIRA_API_TOKEN".into());
        let token = std::env::var(&env).with_context(|| format!("${env} is not set"))?;
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.0.email, token));
        let jql = self.0.jql.clone().unwrap_or_else(|| {
            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".into()
        });
        let base = self.0.url.trim_end_matches('/');
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        for _ in 0..10 {
            let mut req = ureq::get(format!("{base}/rest/api/3/search/jql"))
                .header("Authorization", format!("Basic {auth}"))
                .header("Accept", "application/json")
                .query("jql", &jql)
                .query("fields", "summary")
                .query("maxResults", "100");
            if let Some(p) = &page {
                req = req.query("nextPageToken", p);
            }
            let res: JiraSearch = req
                .call()
                .context("Jira request failed")?
                .body_mut()
                .read_json()
                .context("parsing Jira response")?;
            out.extend(res.issues.into_iter().map(|i| RemoteIssue {
                url: Some(format!("{base}/browse/{}", i.key)),
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
