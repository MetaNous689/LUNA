//! Agent status for the dashboard, read straight from the agents' own backends:
//! Claude Code (session files and `claude agents --json`) and the Codex
//! app-server. Nothing here needs another tool, and a backend that is not
//! installed or not running simply contributes no agents.
//!
//! A worker thread polls every two seconds so the interface never waits on a
//! socket or a process. The collector only drains what the worker published.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

mod claude;
mod codex;
mod processes;
mod websocket;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Claude Code's background agents cost a process start per query.
const BACKGROUND_INTERVAL: Duration = Duration::from_secs(15);
/// How long an agent that just finished a turn shows DONE before it is IDLE.
const DONE_WINDOW: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum AgentState {
    Working,
    Blocked,
    Done,
    Idle,
    /// The backend reported something Luna does not know. No label is drawn.
    Unknown,
}

impl AgentState {
    pub(super) fn label(self) -> Option<&'static str> {
        match self {
            Self::Working => Some("WORKING"),
            Self::Blocked => Some("BLOCKED"),
            Self::Done => Some("DONE"),
            Self::Idle => Some("IDLE"),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum AgentProvider {
    Claude,
    Codex,
}

impl AgentProvider {
    pub(super) fn tag(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AgentEntry {
    pub(super) label: String,
    pub(super) state: AgentState,
    pub(super) provider: AgentProvider,
}

impl AgentEntry {
    #[cfg(test)]
    pub(super) fn new(label: impl Into<String>, state: AgentState) -> Self {
        Self {
            label: label.into(),
            state,
            provider: AgentProvider::Claude,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AgentSnapshot {
    pub(super) agents: Vec<AgentEntry>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum AgentStatusView {
    Loading,
    /// Neither Claude Code nor Codex could be reached.
    Unavailable,
    Ready(AgentSnapshot),
    Stale(AgentSnapshot),
}

/// One agent as a backend reports it, before DONE tracking.
#[derive(Clone, Debug)]
pub(super) struct RawAgent {
    key: String,
    provider: AgentProvider,
    label: String,
    state: AgentState,
}

pub(super) enum SourceResult {
    /// The backend is not installed or not running: not an error.
    Absent,
    /// It is there but did not answer.
    Failed,
    Agents(Vec<RawAgent>),
}

/// Whether an executable of this name is on `PATH`.
pub(crate) fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|dir| fs::metadata(dir.join(program)).is_ok_and(|meta| meta.is_file()))
    })
}

/// Claude Code's config directory (`CLAUDE_CONFIG_DIR`, default `~/.claude`).
pub(crate) fn claude_config_dir() -> Option<PathBuf> {
    claude::config_dir()
}

/// Whether Codex exists on this machine: a running app-server or the CLI.
pub(crate) fn codex_present() -> bool {
    codex::socket_path().is_some_and(|path| path.exists()) || on_path("codex")
}

/// The weekly rate limits from the running Codex app-server. `None` when there
/// is no app-server socket, so the caller can ask the CLI instead.
pub(crate) fn codex_rate_limits() -> Option<Result<serde_json::Value, ()>> {
    codex::rate_limits()
}

/// Last path component, or the whole text when there is none.
pub(super) fn folder_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed)
        .chars()
        .filter(|ch| !ch.is_control())
        .take(40)
        .collect()
}

/// Turns "was working, is idle now" into DONE for a while. The backends only
/// say what an agent is doing right now, so Luna remembers the transition.
#[derive(Default)]
struct DoneTracker {
    seen: HashMap<String, Seen>,
}

struct Seen {
    last: AgentState,
    done_until: Option<Instant>,
}

impl DoneTracker {
    fn apply(&mut self, now: Instant, key: &str, raw: AgentState) -> AgentState {
        let seen = self.seen.entry(key.to_owned()).or_insert(Seen {
            last: raw,
            done_until: None,
        });
        if raw == AgentState::Idle && seen.last == AgentState::Working {
            seen.done_until = Some(now + DONE_WINDOW);
        }
        if raw != AgentState::Idle {
            seen.done_until = None;
        }
        seen.last = raw;
        match seen.done_until {
            Some(until) if raw == AgentState::Idle && now < until => AgentState::Done,
            _ => raw,
        }
    }

    fn retain(&mut self, keys: &HashSet<&str>) {
        self.seen.retain(|key, _| keys.contains(key.as_str()));
    }
}

enum Observation {
    Agents(Vec<AgentEntry>),
    /// No backend exists on this machine.
    NoBackend,
    /// A backend exists but none answered.
    Failed,
}

struct Poller {
    codex: Option<codex::CodexLink>,
    background: Vec<RawAgent>,
    background_due: Instant,
    tracker: DoneTracker,
}

impl Poller {
    fn new() -> Self {
        Self {
            codex: None,
            background: Vec::new(),
            background_due: Instant::now(),
            tracker: DoneTracker::default(),
        }
    }

    fn poll(&mut self, now: Instant) -> Observation {
        let claude_dir = claude::config_dir();
        let claude = claude_dir
            .as_deref()
            .map_or(SourceResult::Absent, claude::interactive);
        if matches!(claude, SourceResult::Agents(_)) && now >= self.background_due {
            self.background_due = now + BACKGROUND_INTERVAL;
            if let Some(agents) = claude_dir.as_deref().and_then(claude::background) {
                self.background = agents;
            }
        }
        let codex = codex::poll(&mut self.codex);

        let mut present = 0;
        let mut answered = false;
        let mut raw: Vec<RawAgent> = Vec::new();
        for source in [claude, codex] {
            match source {
                SourceResult::Absent => {}
                SourceResult::Failed => present += 1,
                SourceResult::Agents(agents) => {
                    present += 1;
                    answered = true;
                    raw.extend(agents);
                }
            }
        }
        if !answered {
            return if present == 0 {
                Observation::NoBackend
            } else {
                Observation::Failed
            };
        }
        raw.extend(self.background.iter().cloned());
        Observation::Agents(self.finish(now, raw))
    }

    fn finish(&mut self, now: Instant, raw: Vec<RawAgent>) -> Vec<AgentEntry> {
        self.tracker
            .retain(&raw.iter().map(|agent| agent.key.as_str()).collect());
        let mut agents: Vec<(String, AgentEntry)> = raw
            .into_iter()
            .map(|agent| {
                let state = self.tracker.apply(now, &agent.key, agent.state);
                (
                    agent.key,
                    AgentEntry {
                        label: agent.label,
                        state,
                        provider: agent.provider,
                    },
                )
            })
            .collect();
        // A stable order keeps rows from jumping when a status changes.
        agents.sort_by(|(left_key, left), (right_key, right)| {
            (left.provider, left.label.to_lowercase(), left_key).cmp(&(
                right.provider,
                right.label.to_lowercase(),
                right_key,
            ))
        });
        agents.into_iter().map(|(_, entry)| entry).collect()
    }
}

enum Command {
    Refresh,
}

pub(super) struct AgentStatusCollector {
    view: AgentStatusView,
    observations: Receiver<Observation>,
    commands: Option<Sender<Command>>,
    worker: Option<JoinHandle<()>>,
}

impl AgentStatusCollector {
    pub(super) fn new() -> Self {
        let (observation_tx, observations) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("luna-agents".into())
            .spawn(move || {
                let mut poller = Poller::new();
                loop {
                    if observation_tx.send(poller.poll(Instant::now())).is_err() {
                        break;
                    }
                    match command_rx.recv_timeout(POLL_INTERVAL) {
                        Ok(Command::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .ok();
        Self {
            view: AgentStatusView::Loading,
            observations,
            commands: Some(commands),
            worker,
        }
    }

    pub(super) fn view(&self) -> &AgentStatusView {
        &self.view
    }

    pub(super) fn refresh(&mut self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Refresh);
        }
    }

    /// Takes what the worker has published. `true` when the picture changed.
    pub(super) fn refresh_if_due(&mut self) -> bool {
        let mut latest = None;
        loop {
            match self.observations.try_recv() {
                Ok(observation) => latest = Some(observation),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if latest.is_none() && self.worker.is_none() {
                        return self.replace(AgentStatusView::Unavailable);
                    }
                    break;
                }
            }
        }
        match latest {
            Some(observation) => self.apply(observation),
            None => false,
        }
    }

    fn apply(&mut self, observation: Observation) -> bool {
        let next = match observation {
            Observation::Agents(agents) => AgentStatusView::Ready(AgentSnapshot { agents }),
            Observation::NoBackend => AgentStatusView::Unavailable,
            Observation::Failed => {
                match std::mem::replace(&mut self.view, AgentStatusView::Loading) {
                    AgentStatusView::Ready(snapshot) | AgentStatusView::Stale(snapshot) => {
                        AgentStatusView::Stale(snapshot)
                    }
                    _ => AgentStatusView::Unavailable,
                }
            }
        };
        self.replace(next)
    }

    fn replace(&mut self, next: AgentStatusView) -> bool {
        let changed = self.view != next;
        self.view = next;
        changed
    }
}

impl Drop for AgentStatusCollector {
    fn drop(&mut self) {
        // Closing the command channel wakes the worker out of its wait.
        self.commands = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(key: &str, label: &str, provider: AgentProvider, state: AgentState) -> RawAgent {
        RawAgent {
            key: key.into(),
            provider,
            label: label.into(),
            state,
        }
    }

    #[test]
    fn finishing_work_shows_done_for_a_while_then_idle() {
        let mut tracker = DoneTracker::default();
        let start = Instant::now();

        assert_eq!(
            tracker.apply(start, "a", AgentState::Idle),
            AgentState::Idle
        );
        assert_eq!(
            tracker.apply(start, "a", AgentState::Working),
            AgentState::Working
        );
        let finished = start + Duration::from_secs(10);
        assert_eq!(
            tracker.apply(finished, "a", AgentState::Idle),
            AgentState::Done
        );
        let later = finished + DONE_WINDOW - Duration::from_secs(1);
        assert_eq!(
            tracker.apply(later, "a", AgentState::Idle),
            AgentState::Done
        );
        let expired = finished + DONE_WINDOW + Duration::from_secs(1);
        assert_eq!(
            tracker.apply(expired, "a", AgentState::Idle),
            AgentState::Idle
        );
    }

    #[test]
    fn new_work_or_a_block_clears_done_and_idle_agents_never_start_done() {
        let mut tracker = DoneTracker::default();
        let now = Instant::now();
        tracker.apply(now, "a", AgentState::Working);
        assert_eq!(tracker.apply(now, "a", AgentState::Idle), AgentState::Done);
        assert_eq!(
            tracker.apply(now, "a", AgentState::Blocked),
            AgentState::Blocked
        );
        assert_eq!(tracker.apply(now, "a", AgentState::Idle), AgentState::Idle);
        assert_eq!(
            tracker.apply(now, "fresh", AgentState::Idle),
            AgentState::Idle
        );
    }

    #[test]
    fn agents_that_disappear_are_forgotten() {
        let mut poller = Poller::new();
        let now = Instant::now();
        poller.finish(
            now,
            vec![raw("x", "x", AgentProvider::Codex, AgentState::Working)],
        );
        poller.finish(now, Vec::new());

        assert!(poller.tracker.seen.is_empty());
    }

    #[test]
    fn rows_are_ordered_by_provider_then_name_regardless_of_status() {
        let mut poller = Poller::new();
        let now = Instant::now();
        let ordered = poller.finish(
            now,
            vec![
                raw("c2", "zeta", AgentProvider::Codex, AgentState::Idle),
                raw("a2", "Beta", AgentProvider::Claude, AgentState::Working),
                raw("c1", "alpha", AgentProvider::Codex, AgentState::Working),
                raw("a1", "alpha", AgentProvider::Claude, AgentState::Blocked),
            ],
        );

        let names: Vec<(&str, AgentProvider)> = ordered
            .iter()
            .map(|entry| (entry.label.as_str(), entry.provider))
            .collect();
        assert_eq!(
            names,
            [
                ("alpha", AgentProvider::Claude),
                ("Beta", AgentProvider::Claude),
                ("alpha", AgentProvider::Codex),
                ("zeta", AgentProvider::Codex),
            ]
        );
    }

    #[test]
    fn a_failed_poll_keeps_the_last_agents_as_stale_and_no_backend_is_unavailable() {
        let (_tx, rx) = mpsc::channel();
        let mut collector = AgentStatusCollector {
            view: AgentStatusView::Loading,
            observations: rx,
            commands: None,
            worker: None,
        };
        let one = vec![AgentEntry::new("a", AgentState::Working)];

        assert!(collector.apply(Observation::Failed));
        assert_eq!(collector.view, AgentStatusView::Unavailable);
        assert!(collector.apply(Observation::Agents(one.clone())));
        assert!(!collector.apply(Observation::Agents(one.clone())));
        assert!(collector.apply(Observation::Failed));
        assert_eq!(
            collector.view,
            AgentStatusView::Stale(AgentSnapshot { agents: one })
        );
        assert!(collector.apply(Observation::NoBackend));
        assert_eq!(collector.view, AgentStatusView::Unavailable);
    }

    #[test]
    fn folder_names_are_the_last_path_component() {
        assert_eq!(folder_name("/work/app"), "app");
        assert_eq!(folder_name("/work/app/"), "app");
        assert_eq!(folder_name("/"), "");
        assert_eq!(folder_name("plain"), "plain");
    }

    #[test]
    fn unknown_states_have_no_label() {
        assert_eq!(AgentState::Unknown.label(), None);
        assert_eq!(AgentState::Done.label(), Some("DONE"));
    }
}
