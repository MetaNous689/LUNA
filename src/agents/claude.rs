//! Claude Code agents, read from Claude Code's own state.
//!
//! Interactive sessions: Claude Code keeps one `sessions/<pid>.json` per running
//! process in its config directory (`CLAUDE_CONFIG_DIR`, default `~/.claude`).
//! Only the process id, name, working folder and status are read, and a file
//! counts only while its process is alive.
//!
//! Background agents: `claude agents --json`, the CLI's documented script
//! interface. It costs a process start, so it runs far less often. A background
//! agent counts only while its worker process in the daemon roster is alive;
//! the CLI also lists agents that are blocked forever because nothing runs.
//!
//! Luna shows agents that are working or blocked, and idle agents that are
//! still attached to an open terminal.
//!
//! The status mapping is Claude Code's own: `busy` and `shell` are working,
//! `waiting` and a `blocked` agent are blocked, everything else is idle.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use serde_json::Value;

use super::{on_path, AgentProvider, AgentState, RawAgent, SourceResult};

/// The size limit Claude Code itself applies to a session file.
const MAX_SESSION_FILE: u64 = 262_144;
const MAX_SESSIONS: usize = 200;
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKGROUND_OUTPUT: usize = 1024 * 1024;

pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
}

#[derive(Debug, PartialEq, Deserialize)]
struct Session {
    pid: Option<u32>,
    name: Option<String>,
    cwd: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    #[serde(rename = "procStart")]
    proc_start: Option<String>,
}

fn state_of_status(status: Option<&str>) -> AgentState {
    match status {
        Some("busy" | "shell") => AgentState::Working,
        Some("waiting") => AgentState::Blocked,
        Some("idle") => AgentState::Idle,
        _ => AgentState::Unknown,
    }
}

fn label_of(name: Option<&str>, cwd: Option<&str>) -> String {
    name.map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| cwd.map(super::folder_name))
        .unwrap_or_else(|| "claude".into())
}

/// Start time of a process in clock ticks since boot, `/proc/<pid>/stat` field
/// 22. Claude Code records it as `procStart`, which tells a live session from a
/// recycled process id.
fn process_start(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    after_name.split_whitespace().nth(19).map(str::to_owned)
}

/// Whether a path is a terminal device, which is what an interactive session
/// reads from while someone has it open.
fn is_terminal_path(path: &str) -> bool {
    path.starts_with("/dev/pts/") || path.starts_with("/dev/tty")
}

/// `None` when it cannot be told, so an unreadable process is never hidden.
fn has_terminal(pid: u32) -> Option<bool> {
    let target = fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    Some(is_terminal_path(&target.to_string_lossy()))
}

fn is_alive(session: &Session, pid: u32) -> bool {
    match (process_start(pid), session.proc_start.as_deref()) {
        (None, _) => false,
        (Some(actual), Some(recorded)) => actual == recorded,
        (Some(_), None) => true,
    }
}

fn parse_session(text: &str) -> Option<Session> {
    serde_json::from_str(text).ok()
}

pub fn interactive(dir: &Path) -> SourceResult {
    let sessions = dir.join("sessions");
    let Ok(entries) = fs::read_dir(&sessions) else {
        return SourceResult::Absent;
    };
    let mut agents = Vec::new();
    for entry in entries.flatten().take(MAX_SESSIONS) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_SESSION_FILE {
            continue;
        }
        let Some(session) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| parse_session(&text))
        else {
            continue;
        };
        if session.kind.as_deref() != Some("interactive") {
            continue;
        }
        let Some(pid) = session.pid.filter(|pid| is_alive(&session, *pid)) else {
            continue;
        };
        let state = state_of_status(session.status.as_deref());
        // An idle session nobody has open is not an agent in use.
        if state == AgentState::Idle && has_terminal(pid) == Some(false) {
            continue;
        }
        agents.push(RawAgent {
            key: format!("claude:{pid}"),
            provider: AgentProvider::Claude,
            label: label_of(session.name.as_deref(), session.cwd.as_deref()),
            state,
        });
    }
    SourceResult::Agents(agents)
}

#[derive(Debug, Deserialize)]
struct Background {
    id: Option<String>,
    name: Option<String>,
    cwd: Option<String>,
    kind: Option<String>,
    state: Option<String>,
}

fn state_of_background(state: Option<&str>) -> AgentState {
    match state {
        Some("working" | "running" | "starting" | "resuming" | "adopted") => AgentState::Working,
        Some("blocked") => AgentState::Blocked,
        Some("done") => AgentState::Done,
        Some("idle") => AgentState::Idle,
        _ => AgentState::Unknown,
    }
}

/// Ids of the daemon workers whose process is still running. `None` when the
/// roster cannot be read.
fn live_workers(dir: &Path) -> Option<Vec<String>> {
    let text = fs::read_to_string(dir.join("daemon/roster.json")).ok()?;
    parse_roster(&text, process_start)
}

fn parse_roster(text: &str, start_of: impl Fn(u32) -> Option<String>) -> Option<Vec<String>> {
    let roster: Value = serde_json::from_str(text).ok()?;
    let workers = roster.get("workers")?.as_object()?;
    Some(
        workers
            .iter()
            .filter(|(_, worker)| {
                let Some(pid) = worker
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok())
                else {
                    return false;
                };
                let Some(actual) = start_of(pid) else {
                    return false;
                };
                match worker.get("procStart") {
                    Some(Value::String(recorded)) => *recorded == actual,
                    Some(Value::Number(recorded)) => recorded.to_string() == actual,
                    _ => true,
                }
            })
            .map(|(id, _)| id.clone())
            .collect(),
    )
}

/// `live` is the set of running worker ids. Without a roster only an agent that
/// reports itself as working is trusted to be running.
fn parse_background(text: &str, live: Option<&[String]>) -> Option<Vec<RawAgent>> {
    let all: Vec<Background> = serde_json::from_str(text).ok()?;
    Some(
        all.into_iter()
            .filter(|agent| agent.kind.as_deref() == Some("background"))
            .filter_map(|agent| {
                let id = agent.id?;
                let state = state_of_background(agent.state.as_deref());
                let running = match live {
                    Some(live) => live
                        .iter()
                        .any(|worker| id.starts_with(worker.as_str()) || worker.starts_with(&id)),
                    None => state == AgentState::Working,
                };
                if !running {
                    return None;
                }
                Some(RawAgent {
                    key: format!("claude-bg:{id}"),
                    provider: AgentProvider::Claude,
                    label: label_of(agent.name.as_deref(), agent.cwd.as_deref()),
                    state,
                })
            })
            .collect(),
    )
}

/// Background agents via the CLI, with a hard time limit. `None` when the CLI
/// is missing, fails, or answers something unexpected.
pub fn background(config_dir: &Path) -> Option<Vec<RawAgent>> {
    if !on_path("claude") {
        return None;
    }
    let mut child = Command::new("claude")
        .args(["agents", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout
            .by_ref()
            .take(MAX_BACKGROUND_OUTPUT as u64)
            .read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + BACKGROUND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().ok()?;
    if !status?.success() {
        return None;
    }
    parse_background(
        &String::from_utf8(output).ok()?,
        live_workers(config_dir).as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_is_claude_codes_own() {
        assert_eq!(state_of_status(Some("busy")), AgentState::Working);
        assert_eq!(state_of_status(Some("shell")), AgentState::Working);
        assert_eq!(state_of_status(Some("waiting")), AgentState::Blocked);
        assert_eq!(state_of_status(Some("idle")), AgentState::Idle);
        assert_eq!(state_of_status(Some("surprise")), AgentState::Unknown);
        assert_eq!(state_of_status(None), AgentState::Unknown);
    }

    #[test]
    fn only_the_needed_session_fields_are_read() {
        let session = parse_session(
            r#"{"pid": 42, "name": "api", "cwd": "/w/api", "kind": "interactive",
                "status": "busy", "procStart": "777", "messagingSocketPath": "/x",
                "sessionId": "secret"}"#,
        )
        .unwrap();
        assert_eq!(session.pid, Some(42));
        assert_eq!(session.name.as_deref(), Some("api"));
        assert_eq!(session.proc_start.as_deref(), Some("777"));
        assert!(parse_session("not json").is_none());
    }

    #[test]
    fn labels_fall_back_to_the_folder() {
        assert_eq!(label_of(Some("api"), Some("/w/x")), "api");
        assert_eq!(label_of(Some("  "), Some("/w/proj/")), "proj");
        assert_eq!(label_of(None, None), "claude");
    }

    #[test]
    fn this_process_is_alive_and_a_recycled_pid_is_not() {
        let pid = std::process::id();
        let start = process_start(pid).expect("own start time");
        let mut session = Session {
            pid: Some(pid),
            name: None,
            cwd: None,
            kind: None,
            status: None,
            proc_start: Some(start),
        };
        assert!(is_alive(&session, pid));
        session.proc_start = Some("1".into());
        assert!(!is_alive(&session, pid));
        session.proc_start = None;
        assert!(is_alive(&session, pid));
        assert!(!is_alive(&session, u32::MAX));
    }

    #[test]
    fn interactive_sessions_are_read_from_a_config_directory() {
        let dir = std::env::temp_dir().join(format!("luna-claude-test-{}", std::process::id()));
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let pid = std::process::id();
        let start = process_start(pid).unwrap();
        fs::write(
            sessions.join(format!("{pid}.json")),
            format!(
                r#"{{"pid":{pid},"name":"live","kind":"interactive","status":"waiting","procStart":"{start}"}}"#
            ),
        )
        .unwrap();
        fs::write(
            sessions.join("999999999.json"),
            r#"{"pid":999999999,"name":"dead","kind":"interactive","status":"busy"}"#,
        )
        .unwrap();
        fs::write(
            sessions.join("daemon.json"),
            format!(r#"{{"pid":{pid},"name":"daemon","kind":"daemon","status":"busy"}}"#),
        )
        .unwrap();

        let SourceResult::Agents(agents) = interactive(&dir) else {
            panic!("sessions directory not read");
        };
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].label, "live");
        assert_eq!(agents[0].state, AgentState::Blocked);
        assert!(matches!(
            interactive(Path::new("/nonexistent/luna")),
            SourceResult::Absent
        ));
    }

    #[test]
    fn only_background_agents_with_a_running_worker_are_shown() {
        let text = r#"[
            {"id": "aaaa1111", "name": "build", "kind": "background", "state": "blocked"},
            {"id": "bbbb2222", "name": "stale", "kind": "background", "state": "blocked"},
            {"id": "cccc3333", "name": "weird", "kind": "background", "state": "weird"},
            {"pid": 1, "name": "ui", "kind": "interactive", "status": "busy"}
        ]"#;
        let live = vec!["aaaa1111".to_owned(), "cccc3333".to_owned()];
        let agents = parse_background(text, Some(&live)).unwrap();

        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].label, "build");
        assert_eq!(agents[0].state, AgentState::Blocked);
        assert_eq!(agents[0].key, "claude-bg:aaaa1111");
        assert_eq!(agents[1].state, AgentState::Unknown);
        assert!(parse_background("{}", Some(&live)).is_none());
    }

    #[test]
    fn without_a_roster_only_working_background_agents_are_trusted() {
        let text = r#"[
            {"id": "a", "name": "run", "kind": "background", "state": "working"},
            {"id": "b", "name": "old", "kind": "background", "state": "blocked"}
        ]"#;
        let agents = parse_background(text, None).unwrap();

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].label, "run");
    }

    #[test]
    fn the_roster_lists_only_workers_whose_process_is_the_recorded_one() {
        let roster = r#"{"workers": {
            "live": {"pid": 10, "procStart": "500"},
            "recycled": {"pid": 11, "procStart": "500"},
            "gone": {"pid": 12, "procStart": "500"},
            "numeric": {"pid": 13, "procStart": 700},
            "bad": {"procStart": "1"}
        }}"#;
        let start = |pid: u32| match pid {
            10 => Some("500".to_owned()),
            11 => Some("999".to_owned()),
            13 => Some("700".to_owned()),
            _ => None,
        };
        let mut live = parse_roster(roster, start).unwrap();
        live.sort();

        assert_eq!(live, ["live", "numeric"]);
        assert!(parse_roster("[]", start).is_none());
    }

    #[test]
    fn only_terminal_devices_count_as_an_open_terminal() {
        assert!(is_terminal_path("/dev/pts/4"));
        assert!(is_terminal_path("/dev/tty1"));
        assert!(!is_terminal_path("/dev/null"));
        assert!(!is_terminal_path("pipe:[123]"));
        assert!(!is_terminal_path("socket:[9]"));
    }
}
