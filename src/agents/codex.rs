//! Codex agents, read from the Codex app-server itself.
//!
//! The Codex CLI keeps one app-server daemon per user and publishes its control
//! socket at `$CODEX_HOME/app-server-control/app-server-control.sock`. Luna
//! connects like any other client, lists the loaded threads and reads each
//! thread's status. Only metadata is requested: no turns, no prompts.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use super::processes::{self, process_start};
use super::websocket::WebSocket;
use super::{AgentProvider, AgentState, RawAgent, SourceResult};

const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// Notifications that arrive between a request and its answer are skipped, but
/// not forever.
const MAX_SKIPPED_MESSAGES: usize = 256;
const MAX_THREADS: usize = 200;

pub fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
    Some(home.join("app-server-control/app-server-control.sock"))
}

pub struct CodexLink {
    socket: WebSocket<UnixStream>,
    next_id: u64,
    path: PathBuf,
    /// The app-server process behind the socket, with its start time.
    daemon: Option<(u32, String)>,
}

pub enum Connect {
    Absent,
    Failed,
}

impl CodexLink {
    pub fn connect(path: &Path) -> Result<Self, Connect> {
        // Follow Codex's own symlink, then insist on a socket owned by us.
        let meta = std::fs::metadata(path).map_err(|_| Connect::Absent)?;
        let own_uid = std::fs::metadata("/proc/self")
            .map(|meta| meta.uid())
            .map_err(|_| Connect::Failed)?;
        if !meta.file_type().is_socket() || meta.uid() != own_uid {
            return Err(Connect::Failed);
        }
        let stream = UnixStream::connect(path).map_err(|_| Connect::Absent)?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
            .map_err(|_| Connect::Failed)?;
        let socket = WebSocket::handshake(stream).map_err(|_| Connect::Failed)?;
        let mut link = Self {
            socket,
            next_id: 1,
            path: path.to_path_buf(),
            daemon: None,
        };
        link.call(
            "initialize",
            json!({"clientInfo": {
                "name": "luna", "title": "Luna", "version": env!("CARGO_PKG_VERSION")
            }}),
        )
        .map_err(|()| Connect::Failed)?;
        link.socket
            .send_text(r#"{"method":"initialized","params":{}}"#)
            .map_err(|_| Connect::Failed)?;
        Ok(link)
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, ()> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"id": id, "method": method, "params": params});
        self.socket
            .send_text(&request.to_string())
            .map_err(|_| ())?;
        for _ in 0..MAX_SKIPPED_MESSAGES {
            let text = self.socket.read_text().map_err(|_| ())?;
            let message: Value = serde_json::from_str(&text).map_err(|_| ())?;
            if message.get("id").and_then(Value::as_u64) != Some(id)
                || message.get("method").is_some()
            {
                continue;
            }
            if message.get("error").is_some() {
                return Err(());
            }
            return message.get("result").cloned().ok_or(());
        }
        Err(())
    }

    /// Every loaded thread with its current status.
    pub fn agents(&mut self) -> Result<Vec<RawAgent>, ()> {
        let mut ids: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.call(
                "thread/loaded/list",
                json!({"cursor": cursor, "limit": 100}),
            )?;
            ids.extend(
                page.get("data")
                    .and_then(Value::as_array)
                    .ok_or(())?
                    .iter()
                    .filter_map(|id| id.as_str().map(str::to_owned)),
            );
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() || ids.len() >= MAX_THREADS {
                break;
            }
        }
        ids.truncate(MAX_THREADS);
        let mut found: Vec<(RawAgent, Option<String>)> = Vec::new();
        for id in ids {
            let result = self.call(
                "thread/read",
                json!({"threadId": id, "includeTurns": false}),
            )?;
            let thread = result.get("thread").ok_or(())?;
            if let Some(agent) = parse_thread(thread) {
                let dir = thread
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|dir| !dir.trim().is_empty())
                    .map(processes::normalize);
                found.push((agent, dir));
            }
        }
        if found
            .iter()
            .any(|(agent, dir)| agent.state == AgentState::Idle && dir.is_some())
        {
            if let Some(daemon) = self.daemon_pid() {
                mark_background_work(&mut found, &processes::child_dirs(daemon, "bwrap"));
            }
        }
        Ok(found.into_iter().map(|(agent, _)| agent).collect())
    }

    /// The app-server process that listens on the control socket.
    fn daemon_pid(&mut self) -> Option<u32> {
        if let Some((pid, start)) = &self.daemon {
            if process_start(*pid).as_deref() == Some(start) {
                return Some(*pid);
            }
        }
        let socket = fs::canonicalize(&self.path).ok()?;
        let table = fs::read_to_string("/proc/net/unix").ok()?;
        let inode = processes::socket_inode(&table, socket.to_str()?)?;
        let pid = processes::listener_pid(inode)?;
        self.daemon = Some((pid, process_start(pid)?));
        Some(pid)
    }
}

/// The account's rate limits, asked of the running app-server. `None` when
/// there is no app-server to ask.
pub fn rate_limits() -> Option<Result<Value, ()>> {
    let path = socket_path()?;
    match CodexLink::connect(&path) {
        Err(Connect::Absent) => None,
        Err(Connect::Failed) => Some(Err(())),
        Ok(mut link) => Some(link.call("account/rateLimits/read", json!({}))),
    }
}

pub fn poll(link: &mut Option<CodexLink>) -> SourceResult {
    if link.is_none() {
        let Some(path) = socket_path() else {
            return SourceResult::Absent;
        };
        match CodexLink::connect(&path) {
            Ok(connected) => *link = Some(connected),
            Err(Connect::Absent) => return SourceResult::Absent,
            Err(Connect::Failed) => return SourceResult::Failed,
        }
    }
    match link.as_mut().map(CodexLink::agents) {
        Some(Ok(agents)) => SourceResult::Agents(agents),
        _ => {
            *link = None;
            SourceResult::Failed
        }
    }
}

/// Codex's own status vocabulary: `active` is a running turn, and an active
/// thread that waits on an approval or on user input is blocked.
fn state_of(status: &Value) -> AgentState {
    match status.get("type").and_then(Value::as_str) {
        Some("active") => {
            let blocked = status
                .get("activeFlags")
                .and_then(Value::as_array)
                .is_some_and(|flags| {
                    flags
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|flag| matches!(flag, "waitingOnApproval" | "waitingOnUserInput"))
                });
            if blocked {
                AgentState::Blocked
            } else {
                AgentState::Working
            }
        }
        Some("idle") => AgentState::Idle,
        _ => AgentState::Unknown,
    }
}

/// Codex calls a thread idle once its turn ended, even while a command it
/// started in the background still runs. Such a command runs in a `bwrap`
/// sandbox directly under the app-server, in the thread's working folder.
///
/// An idle thread is shown as working only when such a sandbox runs in its
/// folder and no other loaded thread shares that folder, so the command cannot
/// belong to someone else. Otherwise the thread stays idle: Luna would rather
/// miss background work than claim it falsely.
fn mark_background_work(agents: &mut [(RawAgent, Option<String>)], busy_dirs: &[String]) {
    for index in 0..agents.len() {
        let Some(dir) = agents[index].1.clone() else {
            continue;
        };
        if agents[index].0.state != AgentState::Idle || !busy_dirs.contains(&dir) {
            continue;
        }
        let sharing = agents
            .iter()
            .filter(|(_, other)| other.as_deref() == Some(dir.as_str()))
            .count();
        if sharing == 1 {
            agents[index].0.state = AgentState::Working;
        }
    }
}

fn parse_thread(thread: &Value) -> Option<RawAgent> {
    let id = thread.get("id")?.as_str()?;
    let state = state_of(thread.get("status")?);
    // A thread that is not loaded is not a live agent.
    if thread.get("status")?.get("type")?.as_str() == Some("notLoaded") {
        return None;
    }
    // An idle thread with no name and no first message is an empty draft that
    // nobody has used. Only its length is checked, never its text.
    let has_conversation = thread
        .get("preview")
        .and_then(Value::as_str)
        .is_some_and(|preview| !preview.trim().is_empty());
    let has_name = thread
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.trim().is_empty());
    if state == AgentState::Idle && !has_conversation && !has_name {
        return None;
    }
    let text = |key: &str| {
        thread
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let label = text("name")
        .or_else(|| text("agentNickname"))
        .map(str::to_owned)
        .or_else(|| text("cwd").map(super::folder_name))
        .unwrap_or_else(|| "codex".into());
    Some(RawAgent {
        key: format!("codex:{id}"),
        provider: AgentProvider::Codex,
        label,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_statuses_follow_codex_vocabulary() {
        assert_eq!(state_of(&json!({"type": "idle"})), AgentState::Idle);
        assert_eq!(
            state_of(&json!({"type": "active", "activeFlags": []})),
            AgentState::Working
        );
        assert_eq!(
            state_of(&json!({"type": "active", "activeFlags": ["waitingOnApproval"]})),
            AgentState::Blocked
        );
        assert_eq!(
            state_of(&json!({"type": "active", "activeFlags": ["waitingOnUserInput"]})),
            AgentState::Blocked
        );
        assert_eq!(
            state_of(&json!({"type": "systemError"})),
            AgentState::Unknown
        );
    }

    #[test]
    fn an_idle_thread_with_a_sandboxed_command_in_its_folder_is_working() {
        let agent = |key: &str, state| RawAgent {
            key: key.into(),
            provider: AgentProvider::Codex,
            label: key.into(),
            state,
        };
        let dir = |text: &str| Some(text.to_owned());
        let busy = vec!["/work/app".to_owned()];

        let mut alone = vec![(agent("a", AgentState::Idle), dir("/work/app"))];
        mark_background_work(&mut alone, &busy);
        assert_eq!(alone[0].0.state, AgentState::Working);

        // Another thread in the same folder could own the command: stay idle.
        let mut shared = vec![
            (agent("a", AgentState::Idle), dir("/work/app")),
            (agent("b", AgentState::Working), dir("/work/app")),
        ];
        mark_background_work(&mut shared, &busy);
        assert_eq!(shared[0].0.state, AgentState::Idle);

        // A different folder, an unknown folder, or no command at all.
        let mut other = vec![
            (agent("a", AgentState::Idle), dir("/work/other")),
            (agent("b", AgentState::Idle), None),
        ];
        mark_background_work(&mut other, &busy);
        assert!(other.iter().all(|(a, _)| a.state == AgentState::Idle));
        let mut none = vec![(agent("a", AgentState::Idle), dir("/work/app"))];
        mark_background_work(&mut none, &[]);
        assert_eq!(none[0].0.state, AgentState::Idle);
    }

    #[test]
    fn labels_prefer_name_then_nickname_then_folder() {
        let named = parse_thread(&json!({
            "id": "t1", "name": " Refactor ", "cwd": "/work/app", "status": {"type": "idle"}
        }))
        .unwrap();
        assert_eq!(named.label, "Refactor");

        let nick = parse_thread(&json!({
            "id": "t2", "name": null, "agentNickname": "Scout", "cwd": "/work/app",
            "preview": "hi", "status": {"type": "idle"}
        }))
        .unwrap();
        assert_eq!(nick.label, "Scout");

        let folder = parse_thread(&json!({
            "id": "t3", "name": "", "preview": "hi", "cwd": "/work/app/",
            "status": {"type": "idle"}
        }))
        .unwrap();
        assert_eq!(folder.label, "app");
        assert_eq!(folder.key, "codex:t3");
    }

    #[test]
    fn unloaded_and_malformed_threads_are_not_live_agents() {
        assert!(parse_thread(&json!({"id": "t", "status": {"type": "notLoaded"}})).is_none());
        assert!(parse_thread(&json!({"status": {"type": "idle"}})).is_none());
        assert!(parse_thread(&json!({"id": "t"})).is_none());
    }

    #[test]
    fn an_idle_empty_draft_is_not_an_agent_in_use() {
        let draft = json!({"id": "t", "name": null, "preview": "", "cwd": "/w",
            "status": {"type": "idle"}});
        assert!(parse_thread(&draft).is_none());

        let mut active = draft.clone();
        active["status"] = json!({"type": "active", "activeFlags": []});
        assert!(parse_thread(&active).is_some());

        let mut named = draft.clone();
        named["name"] = json!("kept");
        assert!(parse_thread(&named).is_some());

        let mut used = draft;
        used["preview"] = json!("first message");
        assert!(parse_thread(&used).is_some());
    }

    #[test]
    fn prompts_are_never_used_as_a_label() {
        let agent = parse_thread(&json!({
            "id": "t", "preview": "secret prompt text", "status": {"type": "idle"}
        }))
        .unwrap();
        assert_eq!(agent.label, "codex");
    }
}
