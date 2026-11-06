//! Copy to the clipboard. OSC 52 reaches the terminal you're sitting at, even
//! over SSH, but not every terminal honours it, so the local tool runs too.

use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
const TOOLS: &[&[&str]] = &[&["pbcopy"]];
#[cfg(windows)]
const TOOLS: &[&[&str]] = &[&["clip"]];
#[cfg(not(any(target_os = "macos", windows)))]
const TOOLS: &[&[&str]] = &[
    &["wl-copy"],
    &["xclip", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--input"],
];

/// False if no local tool took it, so it's up to the terminal.
pub fn copy(text: &str) -> bool {
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
    TOOLS.iter().any(|argv| run(argv, text))
}

fn osc52(text: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{b64}\x07")
}

fn run(argv: &[&str], text: &str) -> bool {
    let child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encodes_the_text() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
