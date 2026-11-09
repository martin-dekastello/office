//! One office per folder. Its server, state and socket live in `<folder>/.office`,
//! so everyone who can see the folder can find it and attach.

use anyhow::{bail, Context, Result};
use interprocess::local_socket::Name;
use std::fs::File;
use std::path::{Path, PathBuf};

/// The folder's own directory, next to the user's files.
pub const DIR: &str = ".office";
/// Longest socket path that fits `sun_path` on every Unix (macOS has 104 bytes).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub folder: PathBuf,
    /// `<folder>/.office`
    pub dir: PathBuf,
}

/// FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
fn hash(p: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in p.to_string_lossy().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

impl Instance {
    /// The instance for a folder, or for the current directory.
    pub fn new(folder: Option<&Path>) -> Result<Instance> {
        let folder = match folder {
            Some(f) => crate::config::expand(f),
            None => std::env::current_dir()?,
        };
        let folder = std::fs::canonicalize(&folder)
            .with_context(|| format!("no such folder: {}", folder.display()))?;
        if !folder.is_dir() {
            bail!("not a folder: {}", folder.display());
        }
        Ok(Instance::at(folder))
    }

    fn at(folder: PathBuf) -> Instance {
        Instance { dir: folder.join(DIR), folder }
    }

    /// The instance a command run here belongs to: the one whose tab this is,
    /// else the nearest folder above `cwd` that has a `.office`, as git finds `.git`.
    pub fn find(cwd: &Path) -> Option<Instance> {
        if let Ok(d) = std::env::var("OFFICE_DATA") {
            let d = PathBuf::from(d);
            if d.file_name() == Some(std::ffi::OsStr::new(DIR)) {
                return d.parent().map(|f| Instance::at(f.to_path_buf()));
            }
        }
        Instance::find_from(cwd)
    }

    fn find_from(cwd: &Path) -> Option<Instance> {
        cwd.ancestors()
            .find(|d| d.join(DIR).is_dir())
            .map(|f| Instance::at(f.to_path_buf()))
    }

    /// Create `.office`. It ignores itself, so the folder's repo stays clean.
    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let ignore = self.dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "*\n")?;
        }
        Ok(())
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("server.log")
    }

    /// Where the socket is. In `.office` when the path is short enough for a
    /// Unix socket, else in /tmp under a name every user derives the same way.
    #[cfg(unix)]
    pub fn socket_path(&self) -> PathBuf {
        let p = self.dir.join("office.sock");
        if p.as_os_str().len() <= MAX_SOCKET_PATH {
            p
        } else {
            PathBuf::from(format!("/tmp/office-{}.sock", hash(&self.folder)))
        }
    }

    /// Refuse a socket that isn't this office's: a symlink, or in the `/tmp`
    /// fallback one owned by someone other than the owner of `.office`. That
    /// name is one anyone can work out, so another user could bind it first
    /// and collect the keys a client sends. Inside `.office` only those who
    /// may write there can make one, and they could change the state anyway.
    #[cfg(unix)]
    pub fn check_socket(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let sock = self.socket_path();
        let Ok(meta) = std::fs::symlink_metadata(&sock) else { return Ok(()) };
        if meta.file_type().is_symlink() {
            bail!("{} is a symlink, not office's socket", sock.display());
        }
        if sock.starts_with(&self.dir) {
            return Ok(());
        }
        let owner = std::fs::metadata(&self.dir)
            .with_context(|| format!("reading {}", self.dir.display()))?
            .uid();
        if meta.uid() != owner {
            bail!("{} belongs to another user, not to the owner of {}", sock.display(), self.dir.display());
        }
        Ok(())
    }

    #[cfg(unix)]
    pub fn socket_name(&self) -> std::io::Result<Name<'static>> {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        self.socket_path().to_fs_name::<GenericFilePath>().map(|n| n.into_owned())
    }

    /// Named pipes don't live in the file system, so the name comes from the folder.
    #[cfg(windows)]
    pub fn socket_name(&self) -> std::io::Result<Name<'static>> {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        format!("office-{}.sock", hash(&self.folder)).to_ns_name::<GenericNamespaced>().map(|n| n.into_owned())
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("server.lock")
    }

    /// Held by the server while it runs, so a folder never gets two. It holds
    /// the server's pid, so `kill $(cat .office/server.lock)` stops it cleanly.
    pub fn lock(&self) -> Result<File> {
        use std::io::Write;
        let path = self.lock_path();
        let mut f = File::options().create(true).truncate(false).write(true).open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        if f.try_lock().is_err() {
            bail!("office is already running in {}", self.folder.display());
        }
        f.set_len(0)?;
        writeln!(f, "{}", std::process::id())?;
        Ok(f)
    }
}

/// Move the state from before office ran per folder into the first folder
/// started, once, so its tabs and sessions come along. Tab directories are
/// absolute, so sessions resume from where they started as before. True if
/// anything moved.
pub fn migrate(global: &Path, dir: &Path) -> Result<bool> {
    let marker = global.join("migrated");
    let state = global.join("state.json");
    if marker.exists() || !state.exists() || dir.join("state.json").exists() || global == dir {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    // state.json last: while it is still here, a failed move is tried again.
    for name in ["snapshots", "status", "state.json"] {
        let from = global.join(name);
        if from.exists() {
            let to = dir.join(name);
            std::fs::rename(&from, &to)
                .or_else(|_| copy_then_remove(&from, &to))
                .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
    }
    let folder = dir.parent().unwrap_or(dir);
    std::fs::write(&marker, format!("{}\n", folder.display()))?;
    Ok(true)
}

/// For a folder on another file system, where rename fails.
fn copy_then_remove(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_then_remove(&e.path(), &to.join(e.file_name()))?;
        }
        std::fs::remove_dir(from)
    } else {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("office-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    #[test]
    fn finds_the_nearest_office_above() {
        let root = temp();
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(Instance::find_from(&deep), None);
        std::fs::create_dir_all(root.join("a").join(DIR)).unwrap();
        assert_eq!(Instance::find_from(&deep), Some(Instance::at(root.join("a"))));
        std::fs::create_dir_all(root.join("a/b").join(DIR)).unwrap();
        assert_eq!(Instance::find_from(&deep).unwrap().folder, root.join("a/b"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn office_dir_ignores_itself() {
        let root = temp();
        let i = Instance::new(Some(&root)).unwrap();
        i.ensure_dir().unwrap();
        assert_eq!(std::fs::read_to_string(i.dir.join(".gitignore")).unwrap(), "*\n");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_second_lock_is_refused_until_the_first_goes() {
        let root = temp();
        let i = Instance::new(Some(&root)).unwrap();
        i.ensure_dir().unwrap();
        let held = i.lock().unwrap();
        assert_eq!(std::fs::read_to_string(i.lock_path()).unwrap().trim(), std::process::id().to_string());
        assert!(i.lock().unwrap_err().to_string().contains("already running"));
        drop(held);
        assert!(i.lock().is_ok());
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn long_folders_get_a_short_socket() {
        let short = Instance::at(PathBuf::from("/srv/team"));
        assert_eq!(short.socket_path(), PathBuf::from("/srv/team/.office/office.sock"));
        let long = Instance::at(PathBuf::from(format!("/srv/{}", "x".repeat(120))));
        let p = long.socket_path();
        assert!(p.starts_with("/tmp") && p.as_os_str().len() <= MAX_SOCKET_PATH);
        assert_eq!(p, Instance::at(long.folder.clone()).socket_path());
    }

    // Another user's socket can't be made here without root; a symlink stands
    // in for a socket that isn't ours.
    #[cfg(unix)]
    #[test]
    fn only_our_own_socket_is_trusted() {
        let root = temp();
        let i = Instance::new(Some(&root)).unwrap();
        i.ensure_dir().unwrap();
        assert!(i.check_socket().is_ok(), "no socket yet is fine");
        let sock = std::os::unix::net::UnixListener::bind(i.socket_path()).unwrap();
        assert!(i.check_socket().is_ok());
        drop(sock);
        std::fs::remove_file(i.socket_path()).unwrap();
        std::os::unix::fs::symlink("/tmp/elsewhere.sock", i.socket_path()).unwrap();
        assert!(i.check_socket().unwrap_err().to_string().contains("symlink"));
        // macOS's long $TMPDIR puts this socket in /tmp, outside root.
        std::fs::remove_file(i.socket_path()).ok();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn global_state_moves_once() {
        let root = temp();
        let global = root.join("global");
        let dir = root.join("folder").join(DIR);
        std::fs::create_dir_all(global.join("snapshots")).unwrap();
        std::fs::write(global.join("state.json"), "{}").unwrap();
        std::fs::write(global.join("snapshots/t.ansi"), "screen").unwrap();
        std::fs::write(global.join("config-unrelated"), "x").unwrap();

        assert!(migrate(&global, &dir).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("state.json")).unwrap(), "{}");
        assert_eq!(std::fs::read_to_string(dir.join("snapshots/t.ansi")).unwrap(), "screen");
        assert!(!global.join("state.json").exists());
        assert!(global.join("config-unrelated").exists());
        let marker = std::fs::read_to_string(global.join("migrated")).unwrap();
        assert_eq!(marker.trim(), root.join("folder").display().to_string());

        // A second folder, or new global state written by an old office, stays put.
        std::fs::write(global.join("state.json"), "{\"new\":1}").unwrap();
        let other = root.join("other").join(DIR);
        assert!(!migrate(&global, &other).unwrap());
        assert!(!other.join("state.json").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_folder_with_state_keeps_it() {
        let root = temp();
        let global = root.join("global");
        let dir = root.join("folder").join(DIR);
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(global.join("state.json"), "old").unwrap();
        std::fs::write(dir.join("state.json"), "mine").unwrap();
        assert!(!migrate(&global, &dir).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("state.json")).unwrap(), "mine");
        assert!(global.join("state.json").exists());
        std::fs::remove_dir_all(root).ok();
    }
}
