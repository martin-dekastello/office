//! zsh-style Tab completion for a typed directory path.

use crate::config;
use std::path::{MAIN_SEPARATOR, Path, is_separator};

/// The matches on show after an ambiguous Tab.
pub struct Completion {
    /// Typed text before the last path segment.
    parent: String,
    /// Matching directories, each ending in a separator.
    pub names: Vec<String>,
    /// None until a second Tab starts cycling.
    pub selected: Option<usize>,
}

impl Completion {
    fn step(&mut self, forward: bool) -> String {
        let n = self.names.len();
        let i = match (self.selected, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.selected = Some(i);
        format!("{}{}", self.parent, self.names[i])
    }
}

/// Apply one Tab (or Shift-Tab) to `buf`. False if nothing matched.
pub fn tab(buf: &mut String, state: &mut Option<Completion>, forward: bool) -> bool {
    if let Some(c) = state {
        *buf = c.step(forward);
        return true;
    }
    let input = buf.trim_start();
    let split = input.rfind(is_separator).map_or(0, |i| i + 1);
    let (parent, stem) = input.split_at(split);
    let names = matches(parent, stem);
    let parent = parent.to_string();
    match names.as_slice() {
        [] => return false,
        [only] => *buf = format!("{parent}{only}{MAIN_SEPARATOR}"),
        _ => {
            *buf = format!("{parent}{}", common_prefix(&names));
            let names = names.into_iter().map(|n| format!("{n}{MAIN_SEPARATOR}")).collect();
            *state = Some(Completion { parent, names, selected: None });
        }
    }
    true
}

/// Sorted names of the directories in `parent` starting with `stem`.
fn matches(parent: &str, stem: &str) -> Vec<String> {
    if stem == ".." || (stem == "~" && parent.is_empty()) {
        return vec![stem.to_string()];
    }
    let dir = if parent.is_empty() { Path::new(".").to_path_buf() } else { config::expand(Path::new(parent)) };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let wanted = name.starts_with(stem) && (stem.starts_with('.') || !name.starts_with('.'));
            // metadata, not file_type, so symlinks to directories count.
            (wanted && std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir())).then_some(name)
        })
        .collect();
    names.sort();
    names
}

fn common_prefix(names: &[String]) -> String {
    let first = &names[0];
    let shared = |n: &String| first.chars().zip(n.chars()).take_while(|(a, b)| a == b).count();
    let len = names.iter().map(shared).min().unwrap_or(0);
    first.chars().take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const S: char = MAIN_SEPARATOR;

    fn tree(dirs: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        for d in dirs {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        root
    }

    fn typed(root: &Path, rest: &str) -> String {
        format!("{}{S}{rest}", root.display())
    }

    #[test]
    fn one_match_completes_and_the_next_tab_descends() {
        let root = tree(&["alpha/inner", "beta"]);
        let mut buf = typed(&root, "al");
        let mut state = None;
        assert!(tab(&mut buf, &mut state, true));
        assert_eq!(buf, typed(&root, &format!("alpha{S}")));
        assert!(state.is_none());
        assert!(tab(&mut buf, &mut state, true));
        assert_eq!(buf, typed(&root, &format!("alpha{S}inner{S}")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn several_matches_insert_the_common_prefix_then_cycle() {
        let root = tree(&["project-a", "project-b", "other"]);
        let mut buf = typed(&root, "pr");
        let mut state = None;
        assert!(tab(&mut buf, &mut state, true));
        assert_eq!(buf, typed(&root, "project-"));
        let shown = state.as_ref().unwrap();
        assert_eq!(shown.names, [format!("project-a{S}"), format!("project-b{S}")]);
        assert_eq!(shown.selected, None);

        tab(&mut buf, &mut state, true);
        assert_eq!(buf, typed(&root, &format!("project-a{S}")));
        tab(&mut buf, &mut state, true);
        assert_eq!(buf, typed(&root, &format!("project-b{S}")));
        tab(&mut buf, &mut state, true);
        assert_eq!(buf, typed(&root, &format!("project-a{S}")));
        tab(&mut buf, &mut state, false);
        assert_eq!(buf, typed(&root, &format!("project-b{S}")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn shift_tab_starts_from_the_last_match() {
        let root = tree(&["a1", "a2", "a3"]);
        let mut buf = typed(&root, "a");
        let mut state = None;
        tab(&mut buf, &mut state, true);
        tab(&mut buf, &mut state, false);
        assert_eq!(buf, typed(&root, &format!("a3{S}")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn no_match_leaves_the_text_alone() {
        let root = tree(&["alpha"]);
        let mut buf = typed(&root, "zzz");
        let before = buf.clone();
        let mut state = None;
        assert!(!tab(&mut buf, &mut state, true));
        assert_eq!(buf, before);
        assert!(state.is_none());
        let mut missing = typed(&root, &format!("nope{S}x"));
        assert!(!tab(&mut missing, &mut state, true));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn only_directories_are_offered() {
        let root = tree(&["data"]);
        std::fs::write(root.join("data.txt"), "x").unwrap();
        let mut buf = typed(&root, "da");
        let mut state = None;
        tab(&mut buf, &mut state, true);
        assert_eq!(buf, typed(&root, &format!("data{S}")));
        assert!(state.is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_directories_count() {
        let root = tree(&["real"]);
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let mut buf = typed(&root, "li");
        tab(&mut buf, &mut None, true);
        assert_eq!(buf, typed(&root, &format!("link{S}")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn dot_folders_need_a_typed_dot() {
        let root = tree(&[".hidden", "shown"]);
        let mut buf = typed(&root, "");
        tab(&mut buf, &mut None, true);
        assert_eq!(buf, typed(&root, &format!("shown{S}")));
        let mut buf = typed(&root, ".");
        tab(&mut buf, &mut None, true);
        assert_eq!(buf, typed(&root, &format!(".hidden{S}")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn home_and_parent_keep_their_short_form() {
        let mut buf = "~".to_string();
        assert!(tab(&mut buf, &mut None, true));
        assert_eq!(buf, format!("~{S}"));
        tab(&mut buf, &mut None, true);
        assert!(buf.starts_with(&format!("~{S}")));

        let mut buf = "..".to_string();
        assert!(tab(&mut buf, &mut None, true));
        assert_eq!(buf, format!("..{S}"));
    }

    #[test]
    fn spaces_and_non_ascii_names_survive() {
        let root = tree(&["my folder", "příliš-a", "příliš-b"]);
        let mut buf = typed(&root, "my");
        tab(&mut buf, &mut None, true);
        assert_eq!(buf, typed(&root, &format!("my folder{S}")));
        let mut buf = typed(&root, "př");
        tab(&mut buf, &mut None, true);
        assert_eq!(buf, typed(&root, "příliš-"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn prefix_stops_inside_a_shared_first_byte() {
        // "č" and "ď" share their first UTF-8 byte but no characters.
        assert_eq!(common_prefix(&["č".to_string(), "ď".to_string()]), "");
    }
}
