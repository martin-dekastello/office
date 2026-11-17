//! Where sync gets GitHub and Jira tokens: an env var or the OS keychain
//! (macOS Keychain, the Secret Service on Linux, Credential Manager on
//! Windows). Token values never go into config.toml, argv or `.salash/`.

use crate::config::Config;
use crate::providers;
use anyhow::{anyhow, bail, Result};
use std::fmt;
use std::io::{IsTerminal, Read};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Keychain service name; the account is the source's name, e.g. "github:owner/repo".
pub const SERVICE: &str = "salash";

/// How long a keychain read may take. A locked keychain over SSH, or a
/// Secret Service with no one to answer its unlock prompt, can block forever.
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `salash token set` / `delete` wait. Longer than a read: someone is
/// there and may have to answer an unlock dialog.
const KEYCHAIN_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

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

/// The result of looking for a source's token: the token and where it was
/// found ("$NAME" or "keychain (…)"), and the places tried before it (names
/// only, never values).
pub struct Lookup {
    pub token: Option<(Token, String)>,
    pub looked: Vec<String>,
}

impl Lookup {
    pub fn looked(&self) -> String {
        self.looked.join(", ")
    }

    /// For `salash token status`: where the token comes from, or `none(looked)`.
    pub fn describe(&self, none: impl FnOnce(String) -> String) -> String {
        match &self.token {
            Some((_, origin)) => origin.clone(),
            None => none(self.looked()),
        }
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
/// While a read that gave up is still waiting, later reads fail at once, so a
/// stuck keychain costs one timeout per sync, not one per source, and leaves
/// at most one thread waiting.
pub struct Timed<K> {
    kc: Arc<K>,
    wait: Duration,
    /// Reads that timed out and haven't answered yet.
    stuck: Arc<Mutex<usize>>,
}

impl<K> Timed<K> {
    pub fn new(kc: K, wait: Duration) -> Self {
        Timed { kc: Arc::new(kc), wait, stuck: Arc::default() }
    }
}

impl<K: Keychain + Send + Sync + 'static> Keychain for Timed<K> {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        if *self.stuck.lock().unwrap() > 0 {
            return Err("skipped: an earlier keychain read hasn't answered yet".into());
        }
        let (tx, rx) = mpsc::channel();
        let (kc, stuck, account) = (self.kc.clone(), self.stuck.clone(), account.to_string());
        let gave_up = Arc::new(Mutex::new(false));
        let gave_up2 = gave_up.clone();
        std::thread::spawn(move || {
            let r = kc.get(&account);
            // The caller checks `gave_up` under the same lock, so either it
            // gets this answer or it counts this read as stuck, never both.
            let mut stuck = stuck.lock().unwrap();
            if *gave_up2.lock().unwrap() {
                *stuck -= 1;
            } else {
                let _ = tx.send(r);
            }
        });
        if let Ok(r) = rx.recv_timeout(self.wait) {
            return r;
        }
        let mut stuck = self.stuck.lock().unwrap();
        match rx.try_recv() {
            Ok(r) => r,
            Err(_) => {
                *gave_up.lock().unwrap() = true;
                *stuck += 1;
                Err("keychain read timed out — over SSH? use an env var or unlock the keychain".into())
            }
        }
    }
}

/// The OS keychain, one per process so that a stuck read is remembered
/// across syncs until it answers.
fn os_keychain() -> &'static Timed<OsKeychain> {
    static KC: OnceLock<Timed<OsKeychain>> = OnceLock::new();
    KC.get_or_init(|| Timed::new(OsKeychain, KEYCHAIN_TIMEOUT))
}

/// Run a keychain write in a thread and give up after `wait`, so a locked
/// keychain over SSH ends with a message instead of hanging.
fn timed<T: Send + 'static>(wait: Duration, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(wait).map_err(|_| {
        anyhow!("the keychain didn't answer within {}s — over SSH? unlock it, or use an env var instead", wait.as_secs())
    })
}

/// Find a source's token: `token_env` if set, then the keychain entry
/// `salash` / `account`, then `default_env`. Empty values count as unset,
/// and a keychain that can't be read is passed over. A source with its own
/// `token_env` never falls back to `default_env`: that is likely another
/// account's token.
pub fn lookup(
    account: &str,
    token_env: Option<&str>,
    default_env: &str,
    env: impl Fn(&str) -> Option<String>,
    kc: &dyn Keychain,
) -> Lookup {
    let from_env = |name: &str, looked: &mut Vec<String>| match env(name).filter(|v| !v.is_empty()) {
        Some(v) => Some((Token(v), format!("${name}"))),
        None => {
            looked.push(format!("${name} (unset)"));
            None
        }
    };
    let mut looked = vec![];
    let mut token = token_env.and_then(|name| from_env(name, &mut looked));
    if token.is_none() {
        match kc.get(account) {
            Ok(Some(v)) if !v.is_empty() => token = Some((Token(v), format!("keychain ({SERVICE} / {account})"))),
            Ok(_) => looked.push(format!("keychain {SERVICE} / {account} (none)")),
            Err(e) => looked.push(format!("keychain (unavailable: {e})")),
        }
    }
    if token.is_none() && token_env.is_none() {
        token = from_env(default_env, &mut looked);
    }
    Lookup { token, looked }
}

/// `lookup` with the process environment and the OS keychain.
pub fn find(account: &str, token_env: Option<&str>, default_env: &str) -> Lookup {
    lookup(account, token_env, default_env, |k| std::env::var(k).ok(), os_keychain())
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

/// Whether any config, the merged one or your own without the folder's,
/// has this source. Only for a note, so a config that doesn't load is a yes.
fn configured(account: &str, load: impl FnOnce() -> Result<Config>) -> bool {
    if !crate::config::config_path().exists() {
        return true;
    }
    let has = |cfg: Result<Config>| cfg.map_or(true, |c| providers::from_config(&c).iter().any(|p| p.name() == account));
    has(load()) || has(Config::load_for(None).map(|(c, _)| c))
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
    /// Print a source's token from the keychain, e.g. for GH_TOKEN=$(…).
    Get { source: String },
    /// Remove a source's token from the keychain.
    Delete { source: String },
    /// Show where each configured source's token comes from (not the token).
    Status,
}

/// `load` reads the config; only `status` needs it, so `set`, `get` and
/// `delete` work even when it doesn't parse.
pub fn run(cmd: TokenCmd, load: impl FnOnce() -> Result<Config>) -> Result<()> {
    match cmd {
        TokenCmd::Set { source } => {
            let account = account(&source)?;
            if !configured(&account, load) {
                eprintln!("note: {account} isn't a source in your config");
            }
            let token = read_token(&account)?;
            let entry = entry(&account)?;
            timed(KEYCHAIN_WRITE_TIMEOUT, move || entry.set_password(token.expose()).map_err(|e| e.to_string()))?
                .map_err(|e| anyhow!("storing in the keychain: {e}"))?;
            println!("Stored the token for {account} in the keychain");
        }
        TokenCmd::Get { source } => {
            let account = account(&source)?;
            match os_keychain().get(&account).map_err(|e| anyhow!("{e}"))? {
                Some(v) => {
                    if std::io::stdout().is_terminal() {
                        eprintln!("note: printing a secret; it stays in this terminal's scrollback and, in a salash tab, in .salash/snapshots/");
                    }
                    println!("{v}")
                }
                None => bail!("no token stored for {account}"),
            }
        }
        TokenCmd::Delete { source } => {
            let account = account(&source)?;
            let entry = entry(&account)?;
            match timed(KEYCHAIN_WRITE_TIMEOUT, move || entry.delete_credential())? {
                Ok(()) => println!("Deleted the token for {account}"),
                Err(keyring::Error::NoEntry) => println!("No token stored for {account}"),
                Err(e) => bail!("deleting from the keychain: {e}"),
            }
        }
        TokenCmd::Status => {
            let sources = providers::from_config(&load()?);
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

    fn found(l: &Lookup) -> Option<(&str, String)> {
        l.token.as_ref().map(|(t, o)| (t.expose(), o.clone()))
    }

    const ACC: &str = "github:o/r";

    #[test]
    fn token_env_wins_over_keychain_and_default() {
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[("WORK", "w"), ("GH_TOKEN", "d")]), &Fake(Ok(Some("k".into()))));
        assert_eq!(found(&l), Some(("w", "$WORK".to_string())));
    }

    #[test]
    fn unset_token_env_falls_through_to_keychain() {
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &Fake(Ok(Some("k".into()))));
        assert_eq!(found(&l), Some(("k", format!("keychain (salash / {ACC})"))));
        assert_eq!(l.looked, vec!["$WORK (unset)"]);
    }

    #[test]
    fn keychain_wins_over_default_env() {
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &Fake(Ok(Some("k".into()))));
        assert_eq!(found(&l), Some(("k", format!("keychain (salash / {ACC})"))));
    }

    #[test]
    fn default_env_when_keychain_has_nothing() {
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &Fake(Ok(None)));
        assert_eq!(found(&l), Some(("d", "$GH_TOKEN".to_string())));
        assert_eq!(l.looked, vec!["keychain salash / github:o/r (none)"]);
    }

    #[test]
    fn unavailable_keychain_is_passed_over_quietly() {
        let l = lookup(ACC, None, "JIRA_API_TOKEN", env(&[("JIRA_API_TOKEN", "d")]), &Fake(Err("no Secret Service".into())));
        assert_eq!(found(&l), Some(("d", "$JIRA_API_TOKEN".to_string())));
        assert_eq!(l.looked, vec!["keychain (unavailable: no Secret Service)"]);
    }

    #[test]
    fn nothing_found_lists_every_place_in_order() {
        let l = lookup(ACC, None, "GH_TOKEN", env(&[]), &Fake(Ok(None)));
        assert!(l.token.is_none());
        assert_eq!(l.looked(), "keychain salash / github:o/r (none), $GH_TOKEN (unset)");
    }

    #[test]
    fn an_unset_token_env_never_falls_back_to_the_shared_default() {
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[("GH_TOKEN", "other-account")]), &Fake(Ok(None)));
        assert!(l.token.is_none());
        assert_eq!(l.looked(), "$WORK (unset), keychain salash / github:o/r (none)");
    }

    #[test]
    fn empty_values_count_as_unset() {
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "")]), &Fake(Ok(Some(String::new()))));
        assert!(l.token.is_none());
        assert_eq!(l.looked.len(), 2);
        let l = lookup(ACC, Some("WORK"), "GH_TOKEN", env(&[("WORK", "")]), &Fake(Ok(None)));
        assert!(l.token.is_none());
    }

    #[test]
    fn token_env_naming_the_default_is_checked_once() {
        let l = lookup(ACC, Some("GH_TOKEN"), "GH_TOKEN", env(&[]), &Fake(Ok(None)));
        assert_eq!(l.looked(), "$GH_TOKEN (unset), keychain salash / github:o/r (none)");
    }

    /// A keychain that answers after this long.
    struct Blocking(Duration);

    impl Keychain for Blocking {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            std::thread::sleep(self.0);
            Ok(Some("late".into()))
        }
    }

    #[test]
    fn a_blocking_keychain_times_out_and_falls_through() {
        let kc = Timed::new(Blocking(Duration::from_secs(60)), Duration::from_millis(50));
        let start = std::time::Instant::now();
        let l = lookup(ACC, None, "GH_TOKEN", env(&[("GH_TOKEN", "d")]), &kc);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(found(&l), Some(("d", "$GH_TOKEN".to_string())));
        assert!(l.looked[0].contains("timed out"), "{:?}", l.looked);
    }

    #[test]
    fn a_timed_keychain_passes_answers_through() {
        let kc = Timed::new(Fake(Ok(Some("k".into()))), Duration::from_secs(5));
        assert_eq!(kc.get(ACC), Ok(Some("k".into())));
    }

    /// Several sources against one stuck keychain, as in a sync or `salash
    /// token status`: one timeout, then the rest are skipped at once, and the
    /// keychain is used again once the stuck read answers.
    #[test]
    fn a_stuck_keychain_costs_one_timeout_not_one_per_source() {
        let kc = Timed::new(Blocking(Duration::from_millis(400)), Duration::from_millis(50));
        let vars = env(&[("GH_TOKEN", "d")]);
        let start = std::time::Instant::now();
        let first = lookup("github:a/one", None, "GH_TOKEN", &vars, &kc);
        let rest: Vec<_> = ["github:a/two", "jira:https://x"].iter().map(|a| lookup(a, None, "GH_TOKEN", &vars, &kc)).collect();
        assert!(start.elapsed() < Duration::from_millis(300), "{:?}", start.elapsed());
        assert!(first.looked[0].contains("timed out"), "{:?}", first.looked);
        for l in &rest {
            assert_eq!(found(l), Some(("d", "$GH_TOKEN".to_string())));
            assert!(l.looked[0].contains("hasn't answered yet"), "{:?}", l.looked);
        }
        std::thread::sleep(Duration::from_millis(600));
        let kc_back = Timed { kc: kc.kc.clone(), wait: Duration::from_secs(5), stuck: kc.stuck.clone() };
        assert_eq!(kc_back.get(ACC), Ok(Some("late".into())));
    }

    #[test]
    fn a_keychain_write_that_hangs_gives_up_with_a_message() {
        let e = timed(Duration::from_millis(50), || std::thread::sleep(Duration::from_secs(60))).unwrap_err();
        assert!(e.to_string().contains("didn't answer"), "{e}");
        assert_eq!(timed(Duration::from_secs(5), || 7).unwrap(), 7);
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
