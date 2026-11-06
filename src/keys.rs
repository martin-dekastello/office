//! Encode crossterm key and mouse events as the bytes a terminal would send.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

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

/// Encode a mouse event at 0-based pane coordinates, or None if the child's
/// mouse mode doesn't report it or the encoding can't hold the position.
pub fn encode_mouse(
    m: &MouseEvent,
    mode: MouseProtocolMode,
    enc: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use MouseProtocolMode::{AnyMotion, ButtonMotion, PressRelease};
    let reported = match m.kind {
        MouseEventKind::Up(_) => matches!(mode, PressRelease | ButtonMotion | AnyMotion),
        MouseEventKind::Drag(_) => matches!(mode, ButtonMotion | AnyMotion),
        MouseEventKind::Moved => mode == AnyMotion,
        _ => mode != MouseProtocolMode::None,
    };
    if !reported {
        return None;
    }
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0u32,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let code = match m.kind {
        MouseEventKind::Down(b) | MouseEventKind::Up(b) => button(b),
        MouseEventKind::Drag(b) => button(b) + 32,
        MouseEventKind::Moved => 3 + 32,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    let mods = 4 * (m.modifiers.contains(KeyModifiers::SHIFT) as u32)
        + 8 * (m.modifiers.contains(KeyModifiers::ALT) as u32)
        + 16 * (m.modifiers.contains(KeyModifiers::CONTROL) as u32);
    let release = matches!(m.kind, MouseEventKind::Up(_));
    let (x, y) = (m.column as u32 + 1, m.row as u32 + 1);
    // Only SGR can say which button was released.
    let legacy = 32 + mods + if release { 3 } else { code };
    match enc {
        MouseProtocolEncoding::Sgr => {
            let end = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{x};{y}{end}", code + mods).into_bytes())
        }
        MouseProtocolEncoding::Default => {
            let (x, y) = (u8::try_from(32 + x).ok()?, u8::try_from(32 + y).ok()?);
            Some(vec![0x1b, b'[', b'M', legacy as u8, x, y])
        }
        MouseProtocolEncoding::Utf8 => {
            if x.max(y) > 2015 {
                return None;
            }
            let c = |v| char::from_u32(v).unwrap_or(' ');
            Some(format!("\x1b[M{}{}{}", c(legacy), c(32 + x), c(32 + y)).into_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }
    fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers }
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
    #[test]
    fn mouse_sgr() {
        let enc = |kind, m| {
            encode_mouse(&mouse(kind, 4, 9, m), MouseProtocolMode::AnyMotion, MouseProtocolEncoding::Sgr).unwrap()
        };
        let none = KeyModifiers::NONE;
        assert_eq!(enc(MouseEventKind::ScrollUp, none), b"\x1b[<64;5;10M");
        assert_eq!(enc(MouseEventKind::ScrollDown, none), b"\x1b[<65;5;10M");
        assert_eq!(enc(MouseEventKind::Down(MouseButton::Left), none), b"\x1b[<0;5;10M");
        assert_eq!(enc(MouseEventKind::Up(MouseButton::Right), none), b"\x1b[<2;5;10m");
        assert_eq!(enc(MouseEventKind::Drag(MouseButton::Left), none), b"\x1b[<32;5;10M");
        assert_eq!(enc(MouseEventKind::Moved, none), b"\x1b[<35;5;10M");
        let all = KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL;
        assert_eq!(enc(MouseEventKind::ScrollUp, all), b"\x1b[<92;5;10M");
    }
    #[test]
    fn mouse_legacy_encodings() {
        let none = KeyModifiers::NONE;
        let (mode, def, utf8) =
            (MouseProtocolMode::PressRelease, MouseProtocolEncoding::Default, MouseProtocolEncoding::Utf8);
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        assert_eq!(encode_mouse(&mouse(down, 0, 1, none), mode, def).unwrap(), b"\x1b[M\x20\x21\x22");
        assert_eq!(encode_mouse(&mouse(up, 0, 1, none), mode, def).unwrap(), b"\x1b[M\x23\x21\x22");
        assert_eq!(encode_mouse(&mouse(down, 222, 0, none), mode, def).unwrap()[4], 255);
        assert_eq!(encode_mouse(&mouse(down, 223, 0, none), mode, def), None);
        assert_eq!(
            encode_mouse(&mouse(down, 299, 0, none), mode, utf8).unwrap(),
            "\x1b[M\u{20}\u{14c}\u{21}".as_bytes()
        );
        assert_eq!(encode_mouse(&mouse(down, 2015, 0, none), mode, utf8), None);
    }
    #[test]
    fn mouse_mode_filters_events() {
        use MouseProtocolMode as M;
        let reports = |kind, mode| {
            encode_mouse(&mouse(kind, 0, 0, KeyModifiers::NONE), mode, MouseProtocolEncoding::Sgr).is_some()
        };
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let wheel = MouseEventKind::ScrollUp;
        for kind in [down, up, drag, wheel, MouseEventKind::Moved] {
            assert!(!reports(kind, M::None));
            assert!(reports(kind, M::AnyMotion));
        }
        assert!(reports(down, M::Press) && reports(wheel, M::Press) && !reports(up, M::Press));
        assert!(reports(up, M::PressRelease) && !reports(drag, M::PressRelease));
        assert!(reports(drag, M::ButtonMotion) && !reports(MouseEventKind::Moved, M::ButtonMotion));
    }
}
