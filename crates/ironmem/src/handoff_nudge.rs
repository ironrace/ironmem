//! Stop-hook handoff nudge (Claude Code transcripts only).
//!
//! After real work, the `stop` hook blocks the stop once and asks the session
//! to save a handoff drawer, so the next session — in any harness — resumes
//! from shared memory instead of a local file nobody else can read. "Real
//! work" is a commit, push or PR, or at least [`MIN_EDITS`] file edits since
//! the session last saved to ironmem (a save whose result was not an error)
//! or was last nudged. At most one nudge per
//! session per [`THROTTLE_SECS`], and a stop already retrying after a block
//! (`stop_hook_active`) always passes, so the hook can never loop.
//!
//! The transcript is scanned incrementally: a per-session state file under
//! `<state_dir>/handoff_nudge/` records the byte offset already counted, so a
//! stop reads only what its turn appended. Every failure fails open — no
//! nudge, never a hook error.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// File edits since the last save that count as real work on their own.
pub const MIN_EDITS: u32 = 5;
/// Minimum seconds between two nudges in one session.
pub const THROTTLE_SECS: i64 = 30 * 60;
/// Largest unread span one stop will parse. The first stop of a long resumed
/// session can find the whole transcript unread; past this the span is
/// skipped (never nudged on) rather than parsed inside a hook.
const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;
const STATE_SUBDIR: &str = "handoff_nudge";
/// Saves awaiting their result that the state remembers. A save whose result
/// never lands (an interrupted turn) must not grow the state file for ever.
const MAX_PENDING_SAVES: usize = 16;
const EDIT_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];

/// `git commit`/`git push` (after any `-C <dir>`, `-c <key=value>` or `--flag`
/// global options) and `gh pr` create/merge. The subcommand must end at
/// whitespace, a shell separator or end of input, so `git commit-graph` and
/// `merge-base` do not count. A heuristic: a miss or a false hit costs one
/// nudge more or less per throttle window.
static SHIP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\bgit(?:\s+(?:-[Cc]\s+\S+|--[\w-]+(?:=\S+)?))*\s+(?:commit|push)(?:[\s;&|)]|$)|\bgh\s+pr\s+(?:create|merge)(?:[\s;&|)]|$)",
    )
    .unwrap()
});

/// One `stop` event, as the hook parsed it from the harness payload.
pub struct StopEvent<'a> {
    /// Sanitized session id; keys the state file.
    pub session_id: &'a str,
    pub transcript_path: &'a Path,
    /// Claude Code sets this when the stop is a retry after a hook blocked it.
    pub stop_hook_active: bool,
}

/// Per-session progress, persisted between stops.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NudgeState {
    /// Transcript bytes already counted (always at a line boundary).
    offset: u64,
    edits: u32,
    shipped: bool,
    /// Unix seconds of the last nudge; 0 = never.
    last_nudge: i64,
    /// `tool_use` ids of ironmem saves whose result has not been read yet.
    #[serde(default)]
    pending_saves: Vec<String>,
}

/// Count the turn's new transcript lines and decide whether this stop should
/// be blocked with a handoff request. Pure apart from the state file; the
/// caller builds the request text with [`reason`] only when this returns true.
pub fn should_nudge(state_dir: &Path, event: &StopEvent<'_>, now: i64) -> bool {
    let path = state_path(state_dir, event.session_id);
    let mut state = load_state(&path);
    if let Err(e) = scan_transcript(event.transcript_path, &mut state) {
        tracing::debug!("handoff nudge: transcript not scanned: {e}");
        return false;
    }
    let nudge = !event.stop_hook_active
        && (state.shipped || state.edits >= MIN_EDITS)
        && now.saturating_sub(state.last_nudge) >= THROTTLE_SECS;
    if nudge {
        // The work is now asked about; only new work earns the next nudge.
        state.edits = 0;
        state.shipped = false;
        state.last_nudge = now;
    }
    if let Err(e) = save_state(&path, &state) {
        // An unsaved nudge would repeat on every stop, so drop it instead.
        tracing::warn!("handoff nudge: state not saved: {e}");
        return false;
    }
    nudge
}

/// The block reason Claude Code feeds back to the session. `wing` is the
/// repository name and `branch` the current branch, so peer sessions on other
/// worktrees of the same repository never overwrite each other's handoff.
pub fn reason(wing: &str, branch: &str) -> String {
    let key: String = format!("{branch}-handoff")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "Before stopping, save a handoff to ironmem with its add_drawer tool \
         (wing \"{wing}\", room \"handoffs\", logical_key \"{key}\"): what changed \
         this session, branches and PRs and their state (current branch: {branch}), \
         what is verified and what is not, and the exact next steps. Keep it under \
         ~40 lines, no secrets. If the tool is unavailable, say so in one line. Then \
         stop; do not restate the handoff to the user."
    )
}

fn state_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join(STATE_SUBDIR)
        .join(format!("{session_id}.json"))
}

fn load_state(path: &Path) -> NudgeState {
    let Ok(raw) = std::fs::read(path) else {
        return NudgeState::default();
    };
    serde_json::from_slice(&raw).unwrap_or_else(|e| {
        tracing::warn!(
            "handoff nudge: unreadable state {}, starting over: {e}",
            path.display()
        );
        NudgeState::default()
    })
}

fn save_state(path: &Path, state: &NudgeState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write, then rename over: a hook killed mid-write by a timeout wrapper
    // must not leave a truncated file that resets the offset and re-nudges
    // work already asked about.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(state)?)?;
    std::fs::rename(&tmp, path)
}

/// Fold the transcript lines appended since `state.offset` into `state`.
fn scan_transcript(path: &Path, state: &mut NudgeState) -> std::io::Result<()> {
    let mut file = open_regular_file(path)?;
    let len = file.metadata()?.len();
    if len < state.offset {
        // Truncated: nothing at the old offset is ours any more. (Claude Code
        // only appends, so a same-or-longer rewrite is not worth detecting.)
        *state = NudgeState {
            last_nudge: state.last_nudge,
            ..NudgeState::default()
        };
    }
    let unread = len - state.offset;
    if unread > MAX_SCAN_BYTES {
        state.offset = len;
        return Ok(());
    }
    file.seek(SeekFrom::Start(state.offset))?;
    let mut buf = Vec::new();
    file.take(unread).read_to_end(&mut buf)?;
    // Only complete lines: one still being written is counted next stop.
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(());
    };
    for line in buf[..end].split(|&b| b == b'\n') {
        count_line(line, state);
    }
    state.offset += end as u64 + 1;
    Ok(())
}

/// `O_NOFOLLOW` plus a regular-file check, the same guard the hook's other
/// transcript readers use.
fn open_regular_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("transcript is not a regular file"));
    }
    Ok(file)
}

fn count_line(line: &[u8], state: &mut NudgeState) {
    // Cheap prefilter: most lines carry neither a tool call nor the result of
    // a pending save, and parsing them is most of the cost.
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let may_settle_a_save = !state.pending_saves.is_empty() && text.contains("\"tool_result\"");
    if !may_settle_a_save && !text.contains("\"tool_use\"") {
        return;
    }
    let Ok(entry) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    match entry.get("type").and_then(Value::as_str) {
        Some("assistant") => content
            .iter()
            .for_each(|block| count_tool_use(block, state)),
        Some("user") => content.iter().for_each(|block| settle_save(block, state)),
        _ => {}
    }
}

fn count_tool_use(block: &Value, state: &mut NudgeState) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let Some(name) = block.get("name").and_then(Value::as_str) else {
        return;
    };
    if is_memory_save(name) {
        if let Some(id) = block.get("id").and_then(Value::as_str) {
            if state.pending_saves.len() >= MAX_PENDING_SAVES {
                state.pending_saves.remove(0);
            }
            state.pending_saves.push(id.to_string());
        }
    } else if EDIT_TOOLS.contains(&name) {
        state.edits = state.edits.saturating_add(1);
    } else if name == "Bash"
        && block
            .pointer("/input/command")
            .and_then(Value::as_str)
            .is_some_and(|command| SHIP_RE.is_match(command))
    {
        state.shipped = true;
    }
}

/// A save clears the work before it only once its result shows it landed: a
/// rejected write (read-only server, readiness timeout) saved nothing, and
/// clearing on the call alone would cancel the nudge exactly when it matters.
fn settle_save(block: &Value, state: &mut NudgeState) {
    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return;
    }
    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    let Some(at) = state.pending_saves.iter().position(|pending| pending == id) else {
        return;
    };
    state.pending_saves.swap_remove(at);
    if block.get("is_error").and_then(Value::as_bool) != Some(true) {
        state.edits = 0;
        state.shipped = false;
    }
}

/// An ironmem write the next session can resume from. Matched by suffix so a
/// plugin install's namespaced name (`mcp__plugin_ironmem_ironmem__add_drawer`)
/// counts as well as a hand-registered server's (`mcp__ironmem__add_drawer`).
fn is_memory_save(name: &str) -> bool {
    name.starts_with("mcp__")
        && name.contains("ironmem")
        && (name.ends_with("__add_drawer") || name.ends_with("__diary_write"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn tool_use(name: &str, input: Value) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "text", "text": "working" },
                { "type": "tool_use", "id": "t", "name": name, "input": input }
            ] }
        })
        .to_string()
    }

    fn edit() -> String {
        tool_use("Edit", serde_json::json!({ "file_path": "/x.rs" }))
    }

    fn bash(command: &str) -> String {
        tool_use("Bash", serde_json::json!({ "command": command }))
    }

    struct Fixture {
        dir: tempfile::TempDir,
        transcript: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let transcript = dir.path().join("t.jsonl");
            std::fs::write(&transcript, "").unwrap();
            Self { dir, transcript }
        }

        fn append(&self, lines: &[String]) {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.transcript)
                .unwrap();
            for line in lines {
                writeln!(file, "{line}").unwrap();
            }
        }

        fn stop_at(&self, now: i64, stop_hook_active: bool) -> bool {
            let event = StopEvent {
                session_id: "sess",
                transcript_path: &self.transcript,
                stop_hook_active,
            };
            should_nudge(&self.dir.path().join("state"), &event, now)
        }

        fn stop(&self) -> bool {
            self.stop_at(NOW, false)
        }
    }

    #[test]
    fn edits_below_the_threshold_do_not_nudge() {
        let f = Fixture::new();
        f.append(&vec![edit(); MIN_EDITS as usize - 1]);
        assert!(!f.stop());
    }

    #[test]
    fn edits_accumulate_across_stops_until_the_threshold() {
        let f = Fixture::new();
        f.append(&vec![edit(); MIN_EDITS as usize - 1]);
        assert!(!f.stop());
        f.append(&[edit()]);
        assert!(f.stop(), "the fifth edit, in a later turn, still counts");
    }

    #[test]
    fn each_line_is_counted_once() {
        let f = Fixture::new();
        f.append(&vec![edit(); MIN_EDITS as usize - 1]);
        assert!(!f.stop());
        assert!(
            !f.stop(),
            "a re-read of the same lines must not reach the threshold"
        );
    }

    #[test]
    fn commit_push_and_pr_count_as_shipped() {
        for command in [
            "git commit -m 'x'",
            "cd repo && git push -u origin HEAD",
            "git -C /work/repo commit -am x",
            "git --no-pager commit -m x",
            "(cd repo && git push)",
            "git push; echo done",
            "git push",
            "gh pr create --fill",
            "gh pr merge 12 --squash",
        ] {
            let f = Fixture::new();
            f.append(&[bash(command)]);
            assert!(f.stop(), "{command} should nudge");
        }
    }

    #[test]
    fn read_only_git_and_gh_do_not_count() {
        let f = Fixture::new();
        f.append(&[
            bash("git status"),
            bash("git log --oneline -5"),
            bash("gh pr view 12"),
            bash("git commit-graph verify"),
            bash("gh pr merge-base"),
        ]);
        assert!(!f.stop());
    }

    #[test]
    fn a_retrying_stop_is_never_blocked() {
        let f = Fixture::new();
        f.append(&[bash("git push")]);
        assert!(!f.stop_at(NOW, true), "fresh work, but a retry passes");
        assert!(f.stop(), "the work is asked about on the next real stop");
    }

    #[test]
    fn nudges_are_throttled_per_session() {
        let f = Fixture::new();
        f.append(&[bash("git push")]);
        assert!(f.stop());
        f.append(&[bash("git push")]);
        assert!(!f.stop_at(NOW + THROTTLE_SECS - 1, false));
        assert!(
            f.stop_at(NOW + THROTTLE_SECS, false),
            "unsaved work is asked about once the throttle lapses"
        );
    }

    #[test]
    fn a_nudge_is_not_repeated_for_the_same_work() {
        let f = Fixture::new();
        f.append(&[bash("git push")]);
        assert!(f.stop());
        assert!(!f.stop_at(NOW + THROTTLE_SECS, false));
    }

    /// A save call with its own `tool_use` id, as Claude Code records it.
    fn save(name: &str, id: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "id": id, "name": name, "input": {} }
            ] }
        })
        .to_string()
    }

    /// The result line Claude Code appends for `id`; `None` omits `is_error`.
    fn result(id: &str, is_error: Option<bool>) -> String {
        let mut block = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": "ok"
        });
        if let Some(flag) = is_error {
            block["is_error"] = serde_json::json!(flag);
        }
        serde_json::json!({ "type": "user", "message": { "content": [block] } }).to_string()
    }

    #[test]
    fn a_successful_save_clears_the_work_before_it() {
        for (name, is_error) in [
            ("mcp__ironmem__add_drawer", Some(false)),
            ("mcp__plugin_ironmem_ironmem__diary_write", None),
        ] {
            let f = Fixture::new();
            f.append(&[bash("git push"), save(name, "s1"), result("s1", is_error)]);
            assert!(!f.stop(), "{name} should count as a save");
        }
    }

    #[test]
    fn a_failed_save_keeps_the_work() {
        let f = Fixture::new();
        f.append(&[
            bash("git push"),
            save("mcp__ironmem__add_drawer", "s1"),
            result("s1", Some(true)),
        ]);
        assert!(f.stop(), "a rejected write saved nothing");
    }

    #[test]
    fn a_save_settles_on_its_own_result_only() {
        let f = Fixture::new();
        f.append(&[
            bash("git push"),
            save("mcp__ironmem__add_drawer", "s1"),
            result("other", Some(false)),
        ]);
        assert!(f.stop(), "another call's result does not settle the save");
    }

    #[test]
    fn a_save_result_read_in_a_later_stop_still_settles_it() {
        let f = Fixture::new();
        f.append(&vec![edit(); MIN_EDITS as usize - 1]);
        f.append(&[save("mcp__ironmem__add_drawer", "s1")]);
        assert!(!f.stop());
        f.append(&[result("s1", Some(false)), edit()]);
        assert!(!f.stop(), "the save cleared the four edits before it");
    }

    #[test]
    fn work_after_a_save_still_counts() {
        let f = Fixture::new();
        f.append(&[
            save("mcp__ironmem__add_drawer", "s1"),
            result("s1", Some(false)),
            bash("git push"),
        ]);
        assert!(f.stop());
    }

    #[test]
    fn other_servers_add_drawer_is_not_an_ironmem_save() {
        let f = Fixture::new();
        f.append(&[
            bash("git push"),
            save("mcp__notes__add_drawer", "s1"),
            result("s1", Some(false)),
        ]);
        assert!(f.stop());
    }

    #[test]
    fn unsettled_saves_are_bounded() {
        let f = Fixture::new();
        let saves: Vec<String> = (0..MAX_PENDING_SAVES + 4)
            .map(|i| save("mcp__ironmem__add_drawer", &format!("s{i}")))
            .collect();
        f.append(&saves);
        assert!(!f.stop());
        let state = load_state(&state_path(&f.dir.path().join("state"), "sess"));
        assert_eq!(state.pending_saves.len(), MAX_PENDING_SAVES);
        assert_eq!(state.pending_saves.last().map(String::as_str), Some("s19"));
    }

    #[test]
    fn a_partial_last_line_waits_for_the_next_stop() {
        let f = Fixture::new();
        f.append(&vec![edit(); MIN_EDITS as usize - 1]);
        let last = edit();
        let (head, tail) = last.split_at(last.len() / 2);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.transcript)
            .and_then(|mut file| std::io::Write::write_all(&mut file, head.as_bytes()))
            .unwrap();
        assert!(!f.stop(), "half a line is not counted");
        f.append(&[tail.to_string()]);
        assert!(f.stop(), "the completed line is counted whole");
    }

    #[test]
    fn a_truncated_transcript_is_rescanned_from_the_start() {
        let f = Fixture::new();
        f.append(&vec![edit(); MIN_EDITS as usize - 1]);
        assert!(!f.stop());
        std::fs::write(&f.transcript, "").unwrap();
        f.append(&vec![edit(); MIN_EDITS as usize - 2]);
        assert!(!f.stop(), "counts restart with the file");
        f.append(&vec![edit(); 2]);
        assert!(f.stop());
    }

    #[test]
    fn unreadable_transcript_fails_open() {
        let f = Fixture::new();
        let event = StopEvent {
            session_id: "sess",
            transcript_path: &f.dir.path().join("missing.jsonl"),
            stop_hook_active: false,
        };
        assert!(!should_nudge(&f.dir.path().join("state"), &event, NOW));
    }

    #[test]
    fn malformed_lines_and_state_are_ignored() {
        let f = Fixture::new();
        let state_dir = f.dir.path().join("state").join(STATE_SUBDIR);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("sess.json"), "{not json").unwrap();
        f.append(&["{\"type\":\"tool_use\"".to_string(), bash("git push")]);
        assert!(f.stop());
    }

    #[test]
    fn reason_names_the_drawer_and_sanitizes_the_key() {
        let text = reason("tenfourpro", "fix/us-phone input");
        assert!(text.contains("wing \"tenfourpro\""));
        assert!(text.contains("room \"handoffs\""));
        assert!(text.contains("logical_key \"fix-us-phone-input-handoff\""));
        assert!(text.contains("current branch: fix/us-phone input"));
    }
}
