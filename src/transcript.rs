//! The conversation as terminal text, read from Claude's transcript. It stands
//! in for scrollback when the terminal kept none: fullscreen Claude draws on
//! the alternate screen, which has no history and is wiped on exit.

use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Only the end of a long transcript fits in the scrollback anyway.
const MAX_READ: u64 = 16 * 1024 * 1024;
/// Tool inputs worth showing, best first.
const TOOL_ARGS: [&str; 4] = ["description", "command", "file_path", "pattern"];
const TOOL_ARG_CHARS: usize = 100;
const USER: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

fn read_tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(MAX_READ))).ok()?;
    let mut bytes = vec![];
    f.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn push(out: &mut String, style: &str, mark: &str, text: &str) {
    let text: String = text
        .replace('\t', "    ")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    for (i, line) in text.trim().lines().enumerate() {
        out.push_str(style);
        out.push_str(if i == 0 { mark } else { "  " });
        out.push_str(line);
        out.push_str("\x1b[m\r\n");
    }
    out.push_str("\r\n");
}

fn tool_line(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let arg = TOOL_ARGS
        .iter()
        .find_map(|k| block.get("input")?.get(*k)?.as_str())
        .and_then(|a| a.lines().next());
    match arg {
        Some(a) => format!("{name}({})", a.chars().take(TOOL_ARG_CHARS).collect::<String>()),
        None => name.to_string(),
    }
}

fn push_entry(out: &mut String, entry: &Value) {
    let flag = |key: &str| entry.get(key).and_then(Value::as_bool).unwrap_or(false);
    if flag("isMeta") || flag("isSidechain") || flag("isCompactSummary") {
        return;
    }
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let Some(content) = entry.get("message").and_then(|m| m.get("content")) else { return };
    // Text starting with a tag is a slash command or injected context.
    let user_text = |out: &mut String, text: &str| {
        if !text.trim_start().starts_with('<') {
            push(out, USER, "❯ ", text);
        }
    };
    match (kind, content) {
        ("user", Value::String(text)) => user_text(out, text),
        ("user" | "assistant", Value::Array(blocks)) => {
            for b in blocks {
                let text = b.get("text").and_then(Value::as_str).unwrap_or("");
                match (kind, b.get("type").and_then(Value::as_str)) {
                    ("user", Some("text")) => user_text(out, text),
                    ("assistant", Some("text")) => push(out, "", "⏺ ", text),
                    ("assistant", Some("tool_use")) => push(out, DIM, "⏺ ", &tool_line(b)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn render(jsonl: &str) -> String {
    let mut out = String::new();
    // A cut-off first line just fails to parse.
    for entry in jsonl.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        push_entry(&mut out, &entry);
    }
    out
}

/// Lines to replay above a saved screen. Empty if there's nothing to show.
pub fn history(path: &Path) -> Vec<u8> {
    let mut out = read_tail(path).map(|t| render(&t)).unwrap_or_default();
    if !out.is_empty() {
        out.push_str(DIM);
        out.push_str("── from the transcript above, last screen below ──\x1b[m\r\n");
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jsonl(entries: &[Value]) -> String {
        entries.iter().map(|e| format!("{e}\n")).collect()
    }

    #[test]
    fn keeps_the_conversation_and_drops_the_rest() {
        let out = render(&jsonl(&[
            json!({"type":"user","message":{"content":"first\nsecond"}}),
            json!({"type":"user","isMeta":true,"message":{"content":"hidden meta"}}),
            json!({"type":"user","message":{"content":"<command-name>/exit</command-name>"}}),
            json!({"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"hidden agent"}]}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"hidden thought"},
                {"type":"text","text":"answer"},
                {"type":"tool_use","name":"Bash","input":{"command":"ls\nrm"}},
                {"type":"tool_use","name":"Glob","input":{}},
            ]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"hidden result"}]}}),
            json!({"type":"file-history-snapshot","snapshot":{}}),
        ]));
        assert!(out.contains("❯ first\x1b[m\r\n"));
        assert!(out.contains("  second"));
        assert!(out.contains("⏺ answer"));
        assert!(out.contains("⏺ Bash(ls)"));
        assert!(out.contains("⏺ Glob\x1b"));
        assert!(!out.contains("hidden") && !out.contains("exit"));
    }

    #[test]
    fn skips_broken_lines_and_strips_escapes() {
        let good = jsonl(&[json!({"type":"user","message":{"content":"a\u{1b}]0;title\u{7}b"}})]);
        let out = render(&format!("{{\"cut off\n{good}"));
        assert!(out.contains("a]0;titleb"));
        assert_eq!(out.matches('\u{1b}').count(), 2);
    }

    #[test]
    fn missing_or_empty_transcript_has_no_history() {
        assert!(history(Path::new("/nonexistent/transcript.jsonl")).is_empty());
        assert!(render("").is_empty());
    }
}
