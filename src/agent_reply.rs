//! Reads the latest assistant reply from an agent's own session transcript.
//!
//! Notifications use this so the body can show what the agent said instead of
//! only where it is. Lookups are best effort: a missing, unreadable, stale or
//! unrecognized transcript yields `None` and callers fall back to location-only
//! text. This runs only when a notification is produced, never per frame or
//! per PTY byte.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Only the end of a transcript can hold the latest reply.
const TAIL_BYTES: u64 = 512 * 1024;
/// A transcript untouched for longer cannot belong to the turn that just ended.
const MAX_TRANSCRIPT_AGE: Duration = Duration::from_secs(120);
const MAX_REPLY_CHARS: usize = 240;
/// Bounds directory fan-out while searching Codex's date-sharded sessions.
const MAX_CODEX_DIRS: usize = 800;

/// Returns the condensed latest reply for `agent`'s session, if one is readable.
pub(crate) fn latest_reply(agent: &str, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    if agent == "hermes" {
        return hermes_reply(session_id).and_then(|reply| condense(&reply, MAX_REPLY_CHARS));
    }
    let path = match agent {
        "claude" => claude_transcript(session_id)?,
        "codex" => codex_rollout(session_id)?,
        _ => return None,
    };
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    if SystemTime::now().duration_since(modified).ok()? > MAX_TRANSCRIPT_AGE {
        return None;
    }
    let tail = read_tail(&path)?;
    let reply = match agent {
        "claude" => claude_reply(&tail),
        _ => codex_reply(&tail),
    }?;
    condense(&reply, MAX_REPLY_CHARS)
}

fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn config_root(env_var: &str, default_dir: &str) -> Option<PathBuf> {
    std::env::var_os(env_var)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(default_dir)))
}

fn claude_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = config_root("CLAUDE_CONFIG_DIR", ".claude")?.join("projects");
    let file_name = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|project| project.path().join(&file_name))
        .find(|candidate| candidate.is_file())
}

fn codex_rollout(session_id: &str) -> Option<PathBuf> {
    let sessions = config_root("CODEX_HOME", ".codex")?.join("sessions");
    let suffix = format!("-{session_id}.jsonl");
    let mut visited = 0usize;
    // Year/month/day shards, newest first: a live session is almost always recent.
    for year in sorted_dirs_desc(&sessions) {
        for month in sorted_dirs_desc(&year) {
            for day in sorted_dirs_desc(&month) {
                visited += 1;
                if visited > MAX_CODEX_DIRS {
                    return None;
                }
                let found = std::fs::read_dir(&day).ok()?.flatten().find(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("rollout-") && name.ends_with(&suffix)
                });
                if let Some(entry) = found {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

fn sorted_dirs_desc(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort_by(|a, b| b.cmp(a));
    dirs
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let truncated = len > TAIL_BYTES;
    if truncated {
        file.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        // The first line is cut mid-record.
        return text.split_once('\n').map(|(_, rest)| rest.to_owned());
    }
    Some(text)
}

fn joined_text_blocks(content: &Value, block_type: &str) -> Option<String> {
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(block_type))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// Latest assistant text of the current turn in a Claude Code transcript.
fn claude_reply(tail: &str) -> Option<String> {
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let content = &entry["message"]["content"];
        match entry.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                if let Some(text) = joined_text_blocks(content, "text") {
                    return Some(text);
                }
            }
            Some("user") => {
                // Tool results belong to the running turn; anything else is a new prompt.
                let only_tool_results = content.as_array().is_some_and(|blocks| {
                    blocks.iter().all(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                });
                if !only_tool_results {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

/// Latest assistant text of the current turn in a Codex rollout.
fn codex_reply(tail: &str) -> Option<String> {
    let mut seen_turn_item = false;
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = &entry["payload"];
        let payload_type = payload.get("type").and_then(Value::as_str);
        match (entry.get("type").and_then(Value::as_str), payload_type) {
            (Some("event_msg"), Some("task_complete")) => {
                // A completion met after turn items belongs to an earlier turn.
                if seen_turn_item {
                    return None;
                }
                if let Some(text) = payload
                    .get("last_agent_message")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    return Some(text.to_owned());
                }
            }
            (Some("event_msg"), Some("task_started")) => return None,
            (Some("response_item"), Some("message")) => {
                match payload.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        seen_turn_item = true;
                        if let Some(text) = joined_text_blocks(&payload["content"], "output_text") {
                            return Some(text);
                        }
                    }
                    Some("user") => return None,
                    _ => {}
                }
            }
            (Some("response_item"), _) => seen_turn_item = true,
            _ => {}
        }
    }
    None
}

/// Hermes keeps sessions in SQLite, so ask the system `sqlite3` for the newest
/// user/assistant message and use it only when it is a fresh assistant reply.
fn hermes_reply(session_id: &str) -> Option<String> {
    let db = config_root("HERMES_HOME", ".hermes")?.join("state.db");
    if !db.is_file() {
        return None;
    }
    // The id is restricted to [A-Za-z0-9_-], so quoting it inline is safe.
    let query = format!(
        "SELECT role, content, timestamp FROM messages WHERE session_id = '{session_id}' \
         AND active = 1 AND role IN ('user','assistant') AND content IS NOT NULL \
         AND content != '' ORDER BY timestamp DESC LIMIT 1"
    );
    let output = std::process::Command::new("sqlite3")
        .args(["-readonly", "-json", "-cmd", ".timeout 300"])
        .arg(&db)
        .arg(query)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    hermes_reply_from_json(&String::from_utf8_lossy(&output.stdout), now)
}

fn hermes_reply_from_json(json: &str, now_epoch_secs: f64) -> Option<String> {
    let rows: Vec<Value> = serde_json::from_str(json).ok()?;
    let row = rows.first()?;
    if row.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let timestamp = row.get("timestamp").and_then(Value::as_f64)?;
    if now_epoch_secs - timestamp > MAX_TRANSCRIPT_AGE.as_secs_f64() {
        return None;
    }
    row.get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

/// Strips Markdown emphasis, collapses whitespace and truncates for a notification.
pub(crate) fn condense(text: &str, max_chars: usize) -> Option<String> {
    let flattened = text
        .replace("**", "")
        .replace('`', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flattened.is_empty() {
        return None;
    }
    if flattened.chars().count() <= max_chars {
        return Some(flattened);
    }
    let mut truncated: String = flattened
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    truncated.push('…');
    Some(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_returns_latest_text_and_skips_tool_only_entries() {
        let tail = concat!(
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"先看一下"},{"type":"tool_use","id":"t"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"u"}]}}"#,
            "\n",
        );
        assert_eq!(claude_reply(tail).as_deref(), Some("先看一下"));
    }

    #[test]
    fn claude_does_not_cross_into_the_previous_turn() {
        let tail = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"上一轮"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"新问题"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"u"}]}}"#,
            "\n",
        );
        assert_eq!(claude_reply(tail), None);
    }

    #[test]
    fn claude_ignores_malformed_lines() {
        let tail = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"42"}]}}"#,
            "\n",
            "not json\n",
        );
        assert_eq!(claude_reply(tail).as_deref(), Some("42"));
    }

    #[test]
    fn codex_prefers_task_complete_message() {
        let tail = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"中间"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"最终回复"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
            "\n",
        );
        assert_eq!(codex_reply(tail).as_deref(), Some("最终回复"));
    }

    #[test]
    fn codex_in_progress_turn_uses_latest_assistant_message() {
        let tail = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"上一轮"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"需要确认"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call"}}"#,
            "\n",
        );
        assert_eq!(codex_reply(tail).as_deref(), Some("需要确认"));
    }

    #[test]
    fn codex_does_not_return_an_earlier_turn() {
        let tail = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"上一轮"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call"}}"#,
            "\n",
        );
        assert_eq!(codex_reply(tail), None);
    }

    #[test]
    fn hermes_uses_only_a_fresh_assistant_row() {
        let row = |role: &str, ts: f64| {
            format!(r#"[{{"role":"{role}","content":"42","timestamp":{ts}}}]"#)
        };
        assert_eq!(
            hermes_reply_from_json(&row("assistant", 990.0), 1000.0).as_deref(),
            Some("42")
        );
        assert_eq!(hermes_reply_from_json(&row("user", 990.0), 1000.0), None);
        assert_eq!(
            hermes_reply_from_json(&row("assistant", 100.0), 1000.0),
            None
        );
        assert_eq!(hermes_reply_from_json("[]", 1000.0), None);
        assert_eq!(hermes_reply_from_json("garbage", 1000.0), None);
    }

    #[test]
    fn condense_strips_markdown_collapses_whitespace_and_truncates() {
        assert_eq!(condense("**完成**\n\n`42`", 20).as_deref(), Some("完成 42"));
        assert_eq!(condense("   \n", 20), None);
        let long = "字".repeat(300);
        let out = condense(&long, 10).unwrap();
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn rejects_unsafe_session_ids() {
        assert!(!is_safe_session_id("../etc/passwd"));
        assert!(!is_safe_session_id(""));
        assert!(is_safe_session_id("5c4261da-e844-4f0c-bfac-685c0819e95d"));
        assert_eq!(latest_reply("claude", "../x"), None);
        assert_eq!(latest_reply("unknown-agent", "abc"), None);
    }
}
