//! Where sync gets GitHub and Jira tokens: an env var or the OS keychain
//! (macOS Keychain, the Secret Service on Linux, Credential Manager on
//! Windows). Token values never go into config.toml, argv or `.salash/`.

use crate::config::Config;
use crate::providers;
use anyhow::{anyhow, bail, Result};
use std::fmt;
use std::io::{IsTerminal, Read};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// Keychain service name; the account is the source's name, e.g. "github:owner/repo".
pub const SERVICE: &str = "salash";

/// How long a keychain read may take. A locked keychain over SSH, or a
/// Secret Service with no one to answer its unlock prompt, can block forever.
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A secret. `Debug` hides it and there is no `Display`, so it can't end up
/// in an error, a flash message or the log by accident.
pub struct Token(String);

impl Token {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

/// Where a token was found.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    Env(String),
    Keychain(String),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Origin::Env(name) => write!(f, "${name}"),
            Origin::Keychain(account) => write!(f, "keychain ({SERVICE} / {account})"),
        }
    }
}

/// The result of looking for a source's token: the token if found, and the
/// places tried before it (names only, never values).
pub struct Lookup {
    pub token: Option<(Token, Origin)>,
    pub looked: Vec<String>,
}

impl Lookup {
    pub fn looked(&self) -> String {
        self.looked.join(", ")
    }
}

pub trait Keychain {
    /// `Ok(None)`: no entry. `Err`: no keychain, or no access to it.
    fn get(&self, account: &str) -> Result<Option<String>, String>;
}

/// The platform's keychain, through the `keyring` crate.
pub struct OsKeychain;

impl Keychain for OsKeychain {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(SERVICE, account).and_then(|e| e.get_password()) {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// A keychain whose reads give up after a while instead of blocking. The
/// read goes on in its thread, which ends whenever the keychain answers.
pub struct Timed<K>(pub Arc<K>, pub Duration);

impl<K: Keychain + Send + Sync + 'static> Keychain for Timed<K> {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        let (tx, rx) = mpsc::channel();
        let (kc, account) = (self.0.clone(), account.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(kc.get(&account));
        });
        match rx.recv_timeout(self.1) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                Err("keychain read timed out — over SSH? use an env var or unlock the keychain".into())
            }
            Err(RecvTimeoutError::Disconnected) => Err("keychain read failed".into()),
        }
    }
}

fn os_keychain() -> Timed<OsKeychain> {
    Timed(Arc::new(OsKeychain), KEYCHAIN_TIMEOUT)
}

/// Find a source's token: `token_env` if set, then the keychain entry
/// `salash` / `account`, then `default_env`. Empty values count as unset,
/// and a keychain that can't be read is passed over.
pub fn lookup(
    account: &str,
    token_env: Option<&str>,
    default_env: &str,
    env: impl Fn(&str) -> Option<String>,
    kc: &dyn Keychain,
) -> Lookup {
    let from_env = |name: &str, looked: &mut Vec<String>| match env(name).filter(|v| !v.is_empty()) {
        Some(v) => Some((Token(v), Origin::Env(name.to_string()))),
        None => {
            looked.push(format!("${name} (unset)"));
            None
        }
    };
    let mut looked = vec![];
    let mut token = token_env.and_then(|name| from_env(name, &mut looked));
    if token.is_none() {
        match kc.get(account) {
            Ok(Some(v)) if !v.is_empty() => token = Some((Token(v), Origin::Keychain(account.to_string()))),
            Ok(_) => looked.push(format!("keychain {SERVICE} / {account} (none)")),
            Err(e) => looked.push(format!("keychain (unavailable: {e})")),
        }
    }
    if token.is_none() && token_env != Some(default_env) {
        token = from_env(default_env, &mut looked);
    }
    Lookup { token, looked }
}

/// `lookup` with the process environment and the OS keychain.
pub fn find(account: &str, token_env: Option<&str>, default_env: &str) -> Lookup {
    lookup(account, token_env, default_env, |k| std::env::var(k).ok(), &os_keychain())
}

/// The keychain account for a source as typed on the command line:
/// "github:owner/repo" or "jira:https://site.atlassian.net".
fn account(source: &str) -> Result<String> {
    if let Some(repo) = source.strip_prefix("github:").filter(|r| !r.is_empty()) {
        return Ok(format!("github:{repo}"));
    }
    if let Some(url) = source.strip_prefix("jira:").map(|u| u.trim_end_matches('/')).filter(|u| !u.is_empty()) {
        return Ok(format!("jira:{url}"));
    }
    bail!("a source is github:owner/repo or jira:https://site.atlassian.net, not {source:?}")
}

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, account).map_err(|e| {
        anyhow!("keychain unavailable ({e}): use an env var instead (token_env, $GH_TOKEN or $JIRA_API_TOKEN)")
    })
}

/// The token for `set`, from a no-echo prompt or from stdin, never from argv.
fn read_token(account: &str) -> Result<Token> {
    let text = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("Token for {account}: "))?
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    };
    let text = text.trim();
    if text.is_empty() {
        bail!("empty token, nothing stored");
    }
    Ok(Token(text.to_string()))
}

#[derive(clap::Subcommand)]
pub enum TokenCmd {
    /// Store a source's token in the keychain, read from stdin or a prompt.
    Set { source: String },
    /// Print a source's token from the keychain.
    Get { source: String },
    /// Remove a source's token from the keychain.
    Delete { source: String },
    /// Show where each configured source's token comes from (not the token).
    Status,
}

pub fn run(cmd: TokenCmd, cfg: &Config) -> Result<()> {
    let sources = providers::from_config(cfg);
    match cmd {
        TokenCmd::Set { source } => {
            let account = account(&source)?;
            if !sources.iter().any(|p| p.name() == account) {
                eprintln!("note: {account} isn't a source in your config");
            }
            let token = read_token(&account)?;
            entry(&account)?
                .set_password(token.expose())
                .map_err(|e| anyhow!("storing in the keychain: {e}"))?;
            println!("Stored the token for {account} in the keychain");
        }
        TokenCmd::Get { source } => {
            let account = account(&source)?;
            match os_keychain().get(&account).map_err(|e| anyhow!("{e}"))? {
                Some(v) => println!("{v}"),
                None => bail!("no token stored for {account}"),
            }
        }
        TokenCmd::Delete { source } => {
            let account = account(&source)?;
            match entry(&account)?.delete_credential() {
                Ok(()) => println!("Deleted the token for {account}"),
                Err(keyring::Error::NoEntry) => println!("No token stored for {account}"),
                Err(e) => bail!("deleting from the keychain: {e}"),
            }
        }
        TokenCmd::Status => {
            if sources.is_empty() {
                println!("No [[github]] or [[jira]] sources in config");
            }
            for p in &sources {
                println!("{:<40} {}", p.name(), p.auth());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake(Result<Option<String>, String>);

    impl Keychain for Fake {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            self.0.clone()
        }
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    fn found(l: &Lookup) -> Option<(&str, Origin)> {
        l.token.as_ref().map(|(t, o)| (t.expose(), o.clone()))
    }

    const ACC: &str = "github:o/r";

    #[test]
    fn token_env_wins_over_keychain_and_default() {
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[("WORK", "w"), ("GH_TOKEN", "d")]), &Fake(Ok(Some("k".into()))));
        assert_eq!(found(&l), Some(("w", Origin::Env("WORK".into()))));
    }

    #[test]
    fn unset_token_env_falls_through_to_keychain() {
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &Fake(Ok(Some("k".into()))));
        assert_eq!(found(&l), Some(("k", Origin::Keychain(ACC.into()))));
        assert_eq!(l.looked, vec!["$WORK (unset)"]);
    }

    #[test]
    fn keychain_wins_over_default_env() {
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &Fake(Ok(Some("k".into()))));
        assert_eq!(found(&l), Some(("k", Origin::Keychain(ACC.into()))));
    }

    #[test]
    fn default_env_when_keychain_has_nothing() {
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &Fake(Ok(None)));
        assert_eq!(found(&l), Some(("d", Origin::Env("GH_TOKEN".into()))));
        assert_eq!(l.looked, vec!["keychain salash / github:o/r (none)"]);
    }

    #[test]
    fn unavailable_keychain_is_passed_over_quietly() {
        let l = lookup(ACC, None, "JIRA_API_TOKEN", env(&[("JIRA_API_TOKEN", "d")]), &Fake(Err("no Secret Service".into())));
        assert_eq!(found(&l), Some(("d", Origin::Env("JIRA_API_TOKEN".into()))));
        assert_eq!(l.looked, vec!["keychain (unavailable: no Secret Service)"]);
    }

    #[test]
    fn nothing_found_lists_every_place_in_order() {
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[]), &Fake(Ok(None)));
        assert!(l.token.is_none());
        assert_eq!(l.looked(), "$WORK (unset), keychain salash / github:o/r (none), $GH_TOKEN (unset)");
    }

    #[test]
    fn empty_values_count_as_unset() {
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[("WORK", ""), ("GH_TOKEN", "")]), &Fake(Ok(Some(String::new()))));
        assert!(l.token.is_none());
        assert_eq!(l.looked.len(), 3);
    }

    #[test]
    fn token_env_naming_the_default_is_checked_once() {
        let l = lookup(ACC, Some("GH_TOKEN"), "GH_TOKEN", env(&[]), &Fake(Ok(None)));
        assert_eq!(l.looked(), "$GH_TOKEN (unset), keychain salash / github:o/r (none)");
    }

    struct Blocking;

    impl Keychain for Blocking {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            std::thread::sleep(Duration::from_secs(60));
            Ok(Some("late".into()))
        }
    }

    #[test]
    fn a_blocking_keychain_times_out_and_falls_through() {
        let kc = Timed(Arc::new(Blocking), Duration::from_millis(50));
        let start = std::time::Instant::now();
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &kc);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(found(&l), Some(("d", Origin::Env("GH_TOKEN".into()))));
        assert!(l.looked[0].contains("timed out"), "{:?}", l.looked);
    }

    #[test]
    fn a_timed_keychain_passes_answers_through() {
        let kc = Timed(Arc::new(Fake(Ok(Some("k".into())))), Duration::from_secs(5));
        assert_eq!(kc.get(ACC), Ok(Some("k".into())));
    }

    #[test]
    fn debug_hides_the_token() {
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "s3cret")]), &Fake(Ok(None)));
        let (t, _) = l.token.unwrap();
        assert!(!format!("{t:?}").contains("s3cret"));
    }

    #[test]
    fn sources_are_normalised() {
        assert_eq!(account("github:o/r").unwrap(), "github:o/r");
        assert_eq!(account("jira:https://x.atlassian.net/").unwrap(), "jira:https://x.atlassian.net");
        assert!(account("o/r").is_err());
        assert!(account("github:").is_err());
    }
}
