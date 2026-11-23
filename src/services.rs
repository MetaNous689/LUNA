//! Narrow access to explicitly named, user-managed server services.
//!
//! The collector asks systemd only for matching unit IDs, active states, and
//! main PIDs, then merges those rows with sanitized local TCP listener data.
//! It never reads process arguments, peer addresses, environments, or paths.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::listeners::{self, TcpListener};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const UNIT_PATTERNS: [&str; 8] = [
    "*codex*.service",
    "*Codex*.service",
    "*claude*.service",
    "*Claude*.service",
    "*server*.service",
    "*Server*.service",
    "*daemon*.service",
    "*Daemon*.service",
];

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ServerKind {
    Codex,
    Claude,
    Other,
}

/// What a server is called on screen: the unit name without its `.service`
/// suffix and instance part, or the process name for a bare listener.
fn display_name(raw: &str) -> String {
    let stem = raw.strip_suffix(".service").unwrap_or(raw);
    let stem = stem.split('@').next().unwrap_or(stem);
    stem.chars()
        .filter(|ch| !ch.is_control())
        .take(40)
        .collect()
}

#[derive(Clone)]
pub(super) struct LocalServer {
    kind: ServerKind,
    name: String,
    pub(super) pid: u32,
    pub(super) port: Option<u16>,
    unit: Option<String>,
    listener: Option<TcpListener>,
}

impl LocalServer {
    pub(super) fn label(&self) -> &str {
        &self.name
    }

    pub(super) fn same_instance(&self, other: &Self) -> bool {
        self.pid == other.pid
            && self.unit == other.unit
            && match (&self.listener, &other.listener) {
                (Some(left), Some(right)) => left.same_process(right),
                (None, None) => true,
                _ => false,
            }
    }
}

#[derive(Default)]
pub(super) struct ServerManager {
    available: bool,
    stale: bool,
    servers: Vec<LocalServer>,
    last_refresh: Option<Instant>,
}

impl ServerManager {
    pub(super) fn refresh(&mut self) {
        self.last_refresh = Some(Instant::now());
        self.apply_refresh(list_servers());
    }

    fn apply_refresh(&mut self, result: Option<Vec<LocalServer>>) {
        match result {
            Some(servers) => {
                self.available = true;
                self.stale = false;
                self.servers = servers;
            }
            None if self.available => {
                self.stale = true;
            }
            None => self.stale = false,
        }
    }

    pub(super) fn refresh_if_due(&mut self) -> bool {
        if self
            .last_refresh
            .is_none_or(|last| last.elapsed() >= REFRESH_INTERVAL)
        {
            self.refresh();
            true
        } else {
            false
        }
    }

    pub(super) fn is_available(&self) -> bool {
        self.available
    }

    pub(super) fn is_stale(&self) -> bool {
        self.stale
    }

    pub(super) fn servers(&self) -> &[LocalServer] {
        &self.servers
    }

    pub(super) fn stop(&self, server: &LocalServer) -> Result<(), ()> {
        if let Some(unit) = &server.unit {
            if !is_allowed_unit(unit) || kind_for_unit(unit) != Some(server.kind) {
                return Err(());
            }
            stop_user_service(unit, server.pid)
        } else if let Some(listener) = &server.listener {
            if kind_for_process_name(listener.comm()) != server.kind {
                return Err(());
            }
            listeners::stop_listener(listener)
        } else {
            Err(())
        }
    }
}

fn stop_user_service(unit: &str, pid: u32) -> Result<(), ()> {
    let details = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--no-pager",
            "--property=ActiveState",
            "--property=MainPID",
            unit,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| ())?;
    if !details.status.success() {
        return Err(());
    }
    let mut active = false;
    let mut current_pid = None;
    for line in String::from_utf8_lossy(&details.stdout).lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "ActiveState" => active = value == "active",
            "MainPID" => current_pid = value.parse::<u32>().ok(),
            _ => {}
        }
    }
    if !active || current_pid != Some(pid) {
        return Err(());
    }

    let status = Command::new("systemctl")
        .args(["--user", "--no-block", "stop", unit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| ())?;
    status.success().then_some(()).ok_or(())
}

fn list_servers() -> Option<Vec<LocalServer>> {
    let services = list_user_services();
    let listeners = listeners::list_user_tcp_listeners();
    if services.is_none() && listeners.is_none() {
        return None;
    }

    let mut by_pid = HashMap::<u32, LocalServer>::new();
    for server in services.into_iter().flatten() {
        by_pid.entry(server.pid).or_insert(server);
    }
    for listener in listeners.into_iter().flatten() {
        if let Some(server) = by_pid.get_mut(&listener.pid) {
            server.port.get_or_insert(listener.port);
            if server.listener.is_none() {
                server.listener = Some(listener);
            }
        } else {
            let server = LocalServer {
                kind: kind_for_process_name(listener.comm()),
                name: display_name(listener.comm()),
                pid: listener.pid,
                port: Some(listener.port),
                unit: None,
                listener: Some(listener),
            };
            by_pid.insert(server.pid, server);
        }
    }

    let mut servers = by_pid.into_values().collect::<Vec<_>>();
    servers.sort_by_key(|server| (server.kind, server.pid));
    Some(servers)
}

fn list_user_services() -> Option<Vec<LocalServer>> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--no-pager",
            "--property=Id",
            "--property=ActiveState",
            "--property=MainPID",
        ])
        .args(UNIT_PATTERNS)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let servers = parse_user_services(&String::from_utf8_lossy(&output.stdout));
    Some(
        servers
            .into_iter()
            .filter(|server| listeners::process_belongs_to_user(server.pid))
            .collect(),
    )
}

fn parse_user_services(output: &str) -> Vec<LocalServer> {
    let mut servers = Vec::new();
    let mut seen = HashSet::new();
    let mut unit = None;
    let mut active = false;
    let mut pid = None;

    let mut flush = |unit: &mut Option<String>, active: &mut bool, pid: &mut Option<u32>| {
        if let Some(unit_id) = unit.take() {
            if *active {
                if let (Some(kind), Some(pid)) = (kind_for_unit(&unit_id), *pid) {
                    if pid > 0 && seen.insert(unit_id.clone()) {
                        servers.push(LocalServer {
                            kind,
                            name: display_name(&unit_id),
                            pid,
                            port: None,
                            unit: Some(unit_id),
                            listener: None,
                        });
                    }
                }
            }
        }
        *active = false;
        *pid = None;
    };

    for line in output.lines() {
        if line.is_empty() {
            flush(&mut unit, &mut active, &mut pid);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Id" => {
                flush(&mut unit, &mut active, &mut pid);
                if is_allowed_unit(value) {
                    unit = Some(value.to_owned());
                }
            }
            "ActiveState" => active = value == "active",
            "MainPID" => pid = value.parse::<u32>().ok(),
            _ => {}
        }
    }
    flush(&mut unit, &mut active, &mut pid);
    servers.sort_by_key(|server| (server.kind, server.pid));
    servers
}

fn is_allowed_unit(unit: &str) -> bool {
    unit.len() <= 255
        && unit.ends_with(".service")
        && unit.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@' | ':' | '\\')
        })
}

fn kind_for_unit(unit: &str) -> Option<ServerKind> {
    if !is_allowed_unit(unit) {
        return None;
    }
    let lower = unit.to_ascii_lowercase();
    if lower.contains("tailscale") || lower.starts_with("luna") {
        None
    } else if lower.contains("codex") {
        Some(ServerKind::Codex)
    } else if lower.contains("claude") {
        Some(ServerKind::Claude)
    } else if lower.contains("server") || lower.contains("daemon") {
        Some(ServerKind::Other)
    } else {
        None
    }
}

fn kind_for_process_name(name: &str) -> ServerKind {
    let lower = name.to_ascii_lowercase();
    if lower.contains("codex") {
        ServerKind::Codex
    } else if lower.contains("claude") {
        ServerKind::Claude
    } else {
        ServerKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_keeps_only_active_matching_units_and_exposes_categories() {
        let output = concat!(
            "Id=codex-app-daemon.service\nActiveState=active\nMainPID=1234\n\n",
            "Id=claude@private-instance.service\nActiveState=active\nMainPID=2345\n\n",
            "Id=media-worker-server.service\nActiveState=active\nMainPID=3456\n\n",
            "Id=editor.service\nActiveState=active\nMainPID=4567\n\n",
            "Id=api-server.service\nActiveState=inactive\nMainPID=0\n\n",
            "Id=stale-daemon.service\nActiveState=active\nMainPID=0\n\n",
            "Description=private description\nWorkingDirectory=/private/path\n\n",
        );
        let servers = parse_user_services(output);

        assert_eq!(servers.len(), 3);
        assert_eq!(
            servers
                .iter()
                .map(|server| (server.label(), server.pid))
                .collect::<Vec<_>>(),
            [
                ("codex-app-daemon", 1234),
                ("claude", 2345),
                ("media-worker-server", 3456)
            ]
        );
        assert!(servers
            .iter()
            .all(|server| !server.label().contains("private-instance")));
    }

    #[test]
    fn parser_deduplicates_units_and_rejects_unsafe_ids() {
        let output = concat!(
            "Id=app-codex.service\nActiveState=active\nMainPID=1234\n\n",
            "Id=app-codex.service\nActiveState=active\nMainPID=1234\n\n",
            "Id=codex;touch-x.service\nActiveState=active\nMainPID=5678\n\n",
        );
        assert_eq!(parse_user_services(output).len(), 1);
    }

    #[test]
    fn stop_unit_allowlist_is_limited_to_user_service_names() {
        assert!(is_allowed_unit("codex-app.service"));
        assert!(is_allowed_unit("claude@instance.service"));
        assert!(!is_allowed_unit("../../codex.service"));
        assert!(!is_allowed_unit("codex-app.socket"));
        assert!(!is_allowed_unit("codex app.service"));
        assert!(kind_for_unit("other-user.service").is_none());
        assert!(kind_for_unit("tailscale-daemon.service").is_none());
        assert!(kind_for_unit("luna-pwa-relay.service").is_none());
        assert!(kind_for_unit("luna-monitor.service").is_none());
        assert!(matches!(
            kind_for_unit("Claude@private-instance.service"),
            Some(ServerKind::Claude)
        ));
    }

    #[test]
    fn failed_refresh_retains_the_last_server_snapshot() {
        let mut manager = ServerManager::default();
        manager.apply_refresh(Some(vec![LocalServer {
            kind: ServerKind::Codex,
            name: "codex".into(),
            pid: 1234,
            port: None,
            unit: None,
            listener: None,
        }]));

        manager.apply_refresh(None);

        assert!(manager.is_available());
        assert!(manager.is_stale());
        assert_eq!(manager.servers().len(), 1);
        assert_eq!(manager.servers()[0].label(), "codex");
    }
}
