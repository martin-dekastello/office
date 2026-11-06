//! Encode crossterm key events as the bytes a terminal would send.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Parse "ctrl-a" / "C-b" / "ctrl-space" into a key event matcher.
pub fn parse_prefix(s: &str) -> (KeyCode, KeyModifiers) {
    let s = s.trim().to_lowercase();
    let key = s
        .strip_prefix("ctrl-")
        .or_else(|| s.strip_prefix("c-"))
        .or_else(|| s.strip_prefix("ctrl+"))
        .unwrap_or("a");
    let code = match key {
        "space" => KeyCode::Char(' '),
        k => KeyCode::Char(k.chars().next().unwrap_or('a')),
    };
    (code, KeyModifiers::CONTROL)
}

pub fn is_prefix(k: &KeyEvent, p: &(KeyCode, KeyModifiers)) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL)
        && match (k.code, p.0) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
            (a, b) => a == b,
        }
}

fn modifier_param(m: KeyModifiers) -> u8 {
    1 + (m.contains(KeyModifiers::SHIFT) as u8)
        + 2 * (m.contains(KeyModifiers::ALT) as u8)
        + 4 * (m.contains(KeyModifiers::CONTROL) as u8)
}

pub fn encode(k: &KeyEvent, app_cursor: bool) -> Vec<u8> {
    let m = k.modifiers;
    let alt = m.contains(KeyModifiers::ALT);
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let shift = m.contains(KeyModifiers::SHIFT);
    let has_mods = m.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL);

    let csi_letter = |letter: char| -> Vec<u8> {
        if has_mods {
            format!("\x1b[1;{}{}", modifier_param(m), letter).into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let csi_tilde = |n: u8| -> Vec<u8> {
        if has_mods {
            format!("\x1b[{};{}~", n, modifier_param(m)).into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let mut out = match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = match c {
                    'a'..='z' | 'A'..='Z' => (c.to_ascii_lowercase() as u8) & 0x1f,
                    ' ' | '@' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '7' => 0x1f,
                    '8' | '?' => 0x7f,
                    _ => return c.to_string().into_bytes(),
                };
                vec![b]
            } else {
                c.to_string().into_bytes()
            }
        }
        // Shift+Enter: Claude Code treats ESC+CR as "newline in prompt".
        KeyCode::Enter if shift || alt => return b"\x1b\r".to_vec(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => {
            if ctrl {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Tab if shift => return b"\x1b[Z".to_vec(),
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => return b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => return csi_letter('A'),
        KeyCode::Down => return csi_letter('B'),
        KeyCode::Right => return csi_letter('C'),
        KeyCode::Left => return csi_letter('D'),
        KeyCode::Home => return csi_letter('H'),
        KeyCode::End => return csi_letter('F'),
        KeyCode::Insert => return csi_tilde(2),
        KeyCode::Delete => return csi_tilde(3),
        KeyCode::PageUp => return csi_tilde(5),
        KeyCode::PageDown => return csi_tilde(6),
        KeyCode::F(n) => {
            return match n {
                1..=4 => {
                    let l = b"PQRS"[(n - 1) as usize] as char;
                    if has_mods {
                        format!("\x1b[1;{}{}", modifier_param(m), l).into_bytes()
                    } else {
                        format!("\x1bO{l}").into_bytes()
                    }
                }
                5 => csi_tilde(15),
                6 => csi_tilde(17),
                7 => csi_tilde(18),
                8 => csi_tilde(19),
                9 => csi_tilde(20),
                10 => csi_tilde(21),
                11 => csi_tilde(23),
                12 => csi_tilde(24),
                _ => vec![],
            }
        }
        _ => vec![],
    };
    if alt && !out.is_empty() {
        out.insert(0, 0x1b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }
    #[test]
    fn encoding() {
        assert_eq!(encode(&k(KeyCode::Char('c'), KeyModifiers::CONTROL), false), vec![3]);
        assert_eq!(encode(&k(KeyCode::Char('x'), KeyModifiers::ALT), false), b"\x1bx");
        assert_eq!(encode(&k(KeyCode::Up, KeyModifiers::NONE), true), b"\x1bOA");
        assert_eq!(encode(&k(KeyCode::Left, KeyModifiers::CONTROL), false), b"\x1b[1;5D");
        assert_eq!(encode(&k(KeyCode::Enter, KeyModifiers::SHIFT), false), b"\x1b\r");
        assert_eq!(encode(&k(KeyCode::Char('é'), KeyModifiers::NONE), false), "é".as_bytes());
        let p = parse_prefix("ctrl-b");
        assert!(is_prefix(&k(KeyCode::Char('b'), KeyModifiers::CONTROL), &p));
    }
}
