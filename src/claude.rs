//! What Claude Code records about its own sessions: `~/.claude/sessions/<pid>.json`
//! for name and status, and the transcript for how full the context is.
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::SystemTime;

/// How much of a transcript's end is searched for the latest token count.
const TAIL_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Busy,
    Idle,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    /// Only a name the user gave; derived ones just repeat the folder.
    pub name: Option<String>,
    pub status: Option<Status>,
    /// When the status last changed, in epoch milliseconds.
    pub since_ms: Option<u64>,
    /// Tokens in the context at the last reply.
    pub context: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawSession {
    session_id: String,
    cwd: String,
    name: Option<String>,
    name_source: Option<String>,
    status: Option<String>,
    status_updated_at: Option<u64>,
}

/// Parses a session file into its meta, session id and starting folder.
pub fn parse_session(text: &str) -> Option<(Meta, String, String)> {
    let raw: RawSession = serde_json::from_str(text).ok()?;
    let meta = Meta {
        name: raw
            .name
            .filter(|_| raw.name_source.as_deref() == Some("user")),
        status: match raw.status.as_deref() {
            Some("busy") => Some(Status::Busy),
            Some("idle") => Some(Status::Idle),
            _ => None,
        },
        since_ms: raw.status_updated_at,
        context: None,
    };
    Some((meta, raw.session_id, raw.cwd))
}

/// The folder under `~/.claude/projects` for sessions started in `cwd`.
pub fn project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Context size at the newest main-thread reply in a transcript's tail: what
/// was sent, cached or not. Subagent turns have their own contexts.
pub fn last_context(tail: &str) -> Option<u64> {
    tail.lines().rev().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        if entry["isSidechain"].as_bool() == Some(true) {
            return None;
        }
        let usage = entry["message"]["usage"].as_object()?;
        let tokens = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        Some(
            tokens("input_tokens")
                + tokens("cache_read_input_tokens")
                + tokens("cache_creation_input_tokens"),
        )
    })
}

/// Reads session files, re-reading a transcript only when it has changed.
pub struct Reader {
    root: Option<PathBuf>,
    transcripts: HashMap<i32, (SystemTime, Option<u64>)>,
}

impl Reader {
    pub fn new() -> Self {
        Reader {
            root: std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")),
            transcripts: HashMap::new(),
        }
    }

    /// None if Claude Code keeps no file for this pid.
    pub fn read(&mut self, pid: i32) -> Option<Meta> {
        let root = self.root.as_ref()?;
        let path = root.join("sessions").join(format!("{pid}.json"));
        let text = std::fs::read_to_string(path).ok()?;
        let (mut meta, session, cwd) = parse_session(&text)?;
        let transcript = root
            .join("projects")
            .join(project_dir(&cwd))
            .join(format!("{session}.jsonl"));
        if let Ok(modified) = std::fs::metadata(&transcript).and_then(|m| m.modified()) {
            let cached = self.transcripts.get(&pid).copied();
            let context = match cached {
                Some((seen, context)) if seen == modified => context,
                // A tail with no reply in it keeps the last figure.
                _ => read_tail(&transcript)
                    .and_then(|tail| last_context(&tail))
                    .or(cached.and_then(|(_, context)| context)),
            };
            self.transcripts.insert(pid, (modified, context));
            meta.context = context;
        }
        Some(meta)
    }

    pub fn retain(&mut self, keep: impl Fn(i32) -> bool) {
        self.transcripts.retain(|pid, _| keep(*pid));
    }
}

fn read_tail(path: &std::path::Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file() {
        let text = r#"{"pid":7,"sessionId":"abc","cwd":"/Users/me/my.app","name":"fix-login",
            "nameSource":"user","status":"busy","statusUpdatedAt":1700000000000,"extra":1}"#;
        let (meta, session, cwd) = parse_session(text).unwrap();
        assert_eq!(meta.name.as_deref(), Some("fix-login"));
        assert_eq!(meta.status, Some(Status::Busy));
        assert_eq!(meta.since_ms, Some(1_700_000_000_000));
        assert_eq!(
            (session.as_str(), cwd.as_str()),
            ("abc", "/Users/me/my.app")
        );
    }

    #[test]
    fn derived_names_and_unknown_statuses_are_dropped() {
        let text = r#"{"name":"webapp-6f","nameSource":"derived","status":"napping"}"#;
        let (meta, ..) = parse_session(text).unwrap();
        assert_eq!(meta, Meta::default());
        assert!(parse_session("not json").is_none());
    }

    #[test]
    fn project_folder_flattens_the_path() {
        assert_eq!(project_dir("/Users/me/my.app_2"), "-Users-me-my-app-2");
    }

    #[test]
    fn context_is_the_newest_main_reply() {
        let tail = concat!(
            "cut off mid-line\"}\n",
            r#"{"message":{"usage":{"input_tokens":1,"cache_read_input_tokens":10}}}"#,
            "\n",
            r#"{"message":{"usage":{"input_tokens":2,"cache_read_input_tokens":200,"cache_creation_input_tokens":30,"output_tokens":9}}}"#,
            "\n",
            r#"{"isSidechain":true,"message":{"usage":{"input_tokens":99999}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"hi"}}"#,
            "\n",
        );
        assert_eq!(last_context(tail), Some(232));
        assert_eq!(last_context("nothing\n"), None);
    }
}
