//! Messages between `office` clients and the server of a folder.
//!
//! A frame is a tag byte, a big-endian u32 length and the payload. The client
//! sends what the user does as crossterm events; the server renders and sends
//! back the bytes to print, like zellij does.

use anyhow::{bail, Context, Result};
use crossterm::event::Event;
use std::io::{Read, Write};

/// Larger frames mean the other end isn't office.
const MAX_FRAME: u32 = 64 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMsg {
    /// First message: the client's terminal size.
    Hello { cols: u16, rows: u16 },
    Event(Event),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMsg {
    /// First message: how to set up the terminal.
    Welcome { mouse: bool },
    /// Bytes to print as they are.
    Output(Vec<u8>),
    /// Text to put on the clipboard of whoever copied it.
    Clipboard(String),
    /// A URL to open on the client's side.
    Open(String),
    /// The client is done: detached, or office quit.
    Exit(String),
}

fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = tag;
    head[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// None at a clean end of the stream.
fn read_frame(r: &mut impl Read) -> Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head[1..].try_into().unwrap());
    if len > MAX_FRAME {
        bail!("frame too large ({len} bytes)");
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
}

fn text(b: Vec<u8>) -> Result<String> {
    String::from_utf8(b).context("not UTF-8")
}

impl ClientMsg {
    pub fn write(&self, w: &mut impl Write) -> std::io::Result<()> {
        match self {
            ClientMsg::Hello { cols, rows } => {
                let mut b = cols.to_be_bytes().to_vec();
                b.extend_from_slice(&rows.to_be_bytes());
                write_frame(w, 1, &b)
            }
            ClientMsg::Event(e) => write_frame(w, 2, &serde_json::to_vec(e)?),
        }
    }

    pub fn read(r: &mut impl Read) -> Result<Option<ClientMsg>> {
        let Some((tag, b)) = read_frame(r)? else { return Ok(None) };
        Ok(Some(match tag {
            1 if b.len() == 4 => ClientMsg::Hello {
                cols: u16::from_be_bytes([b[0], b[1]]),
                rows: u16::from_be_bytes([b[2], b[3]]),
            },
            2 => ClientMsg::Event(serde_json::from_slice(&b).context("parsing event")?),
            _ => bail!("unknown client message {tag}"),
        }))
    }
}

impl ServerMsg {
    pub fn write(&self, w: &mut impl Write) -> std::io::Result<()> {
        match self {
            ServerMsg::Welcome { mouse } => write_frame(w, 10, &[*mouse as u8]),
            ServerMsg::Output(b) => write_frame(w, 11, b),
            ServerMsg::Clipboard(s) => write_frame(w, 12, s.as_bytes()),
            ServerMsg::Open(s) => write_frame(w, 13, s.as_bytes()),
            ServerMsg::Exit(s) => write_frame(w, 14, s.as_bytes()),
        }
    }

    pub fn read(r: &mut impl Read) -> Result<Option<ServerMsg>> {
        let Some((tag, b)) = read_frame(r)? else { return Ok(None) };
        Ok(Some(match tag {
            10 if b.len() == 1 => ServerMsg::Welcome { mouse: b[0] != 0 },
            11 => ServerMsg::Output(b),
            12 => ServerMsg::Clipboard(text(b)?),
            13 => ServerMsg::Open(text(b)?),
            14 => ServerMsg::Exit(text(b)?),
            _ => bail!("unknown server message {tag}"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    #[test]
    fn client_messages_round_trip() {
        let msgs = [
            ClientMsg::Hello { cols: 211, rows: 54 },
            ClientMsg::Event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))),
            ClientMsg::Event(Event::Paste("two\nlines".into())),
            ClientMsg::Event(Event::Resize(80, 24)),
            ClientMsg::Event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::SHIFT,
            })),
        ];
        let mut buf = vec![];
        for m in &msgs {
            m.write(&mut buf).unwrap();
        }
        let mut r = buf.as_slice();
        for m in &msgs {
            assert_eq!(&ClientMsg::read(&mut r).unwrap().unwrap(), m);
        }
        assert!(ClientMsg::read(&mut r).unwrap().is_none());
    }

    #[test]
    fn server_messages_round_trip() {
        let msgs = [
            ServerMsg::Welcome { mouse: true },
            ServerMsg::Output(b"\x1b[2J\x07".to_vec()),
            ServerMsg::Output(vec![]),
            ServerMsg::Clipboard("copied ✓".into()),
            ServerMsg::Open("obsidian://open?path=x".into()),
            ServerMsg::Exit("detached".into()),
        ];
        let mut buf = vec![];
        for m in &msgs {
            m.write(&mut buf).unwrap();
        }
        let mut r = buf.as_slice();
        for m in &msgs {
            assert_eq!(&ServerMsg::read(&mut r).unwrap().unwrap(), m);
        }
        assert!(ServerMsg::read(&mut r).unwrap().is_none());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(ServerMsg::read(&mut [99u8, 0, 0, 0, 0].as_slice()).is_err());
        assert!(ClientMsg::read(&mut [1u8, 0xff, 0xff, 0xff, 0xff].as_slice()).is_err());
        // Cut off mid-frame.
        assert!(ClientMsg::read(&mut [1u8, 0, 0, 0, 4, 0].as_slice()).is_err());
    }
}
