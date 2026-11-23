//! Local TCP listener discovery without retaining or displaying addresses.
//!
//! `ss` output is reduced to PID; only numeric local ports are retained, and
//! address fields are discarded. Process identity is limited to the current
//! user's `/proc/<pid>/comm`, owner and start time. Arguments, peer addresses,
//! environments and working directories are excluded.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::process::{Command, Stdio};

const PIDFD_SIGNAL_SCRIPT: &str = r#"
import os, signal, sys
fields = sys.stdin.read().splitlines()
if len(fields) != 3 or not hasattr(os, 'pidfd_open') or not hasattr(signal, 'pidfd_send_signal'):
    raise SystemExit(2)
pid, expected_start = int(fields[0]), fields[1]
expected_comm = fields[2]
fd = os.pidfd_open(pid)
try:
    with open(f'/proc/{pid}/comm', encoding='utf-8', errors='replace') as source:
        comm = source.read().rstrip('\n')
    with open(f'/proc/{pid}/stat', encoding='utf-8', errors='replace') as source:
        stat = source.read()
    closing = stat.rfind(')')
    start_time = stat[closing + 1:].split()[19] if closing >= 0 else ''
    if comm != expected_comm or start_time != expected_start:
        raise SystemExit(3)
    if os.stat(f'/proc/{pid}').st_uid != os.getuid():
        raise SystemExit(4)
    signal.pidfd_send_signal(fd, signal.SIGTERM)
finally:
    os.close(fd)
"#;

#[derive(Clone)]
pub(super) struct TcpListener {
    pub(super) pid: u32,
    pub(super) port: u16,
    comm: String,
    start_time: u64,
}

impl TcpListener {
    pub(super) fn same_process(&self, other: &Self) -> bool {
        self.pid == other.pid && self.start_time == other.start_time && self.comm == other.comm
    }

    pub(super) fn comm(&self) -> &str {
        &self.comm
    }
}

pub(super) fn list_user_tcp_listeners() -> Option<Vec<TcpListener>> {
    let output = Command::new("ss")
        .args(["-H", "-ltnp"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let uid = fs::metadata("/proc/self").ok()?.uid();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut listeners = HashMap::<u32, TcpListener>::new();
    for line in stdout.lines() {
        for (pid, port) in parse_listener_line(line) {
            let Some((comm, start_time)) = process_identity(pid, uid) else {
                continue;
            };
            if is_ignored_comm(&comm) {
                continue;
            }
            listeners.entry(pid).or_insert(TcpListener {
                pid,
                port,
                comm,
                start_time,
            });
        }
    }
    let mut listeners = listeners.into_values().collect::<Vec<_>>();
    listeners.sort_by_key(|listener| (listener.port, listener.pid));
    Some(listeners)
}

pub(super) fn process_belongs_to_user(pid: u32) -> bool {
    let Ok(current) = fs::metadata("/proc/self") else {
        return false;
    };
    let process_path = format!("/proc/{pid}");
    let Ok(process) = fs::metadata(&process_path) else {
        return false;
    };
    let Ok(comm) = fs::read_to_string(format!("{process_path}/comm")) else {
        return false;
    };
    process.uid() == current.uid() && !is_ignored_comm(comm.trim_end_matches('\n'))
}

pub(super) fn stop_listener(listener: &TcpListener) -> Result<(), ()> {
    let uid = fs::metadata("/proc/self").map_err(|_| ())?.uid();
    let Some((comm, start_time)) = process_identity(listener.pid, uid) else {
        return Err(());
    };
    if comm != listener.comm || start_time != listener.start_time || is_ignored_comm(&comm) {
        return Err(());
    }
    let listeners = list_user_tcp_listeners().ok_or(())?;
    if !listeners
        .iter()
        .any(|current| current.same_process(listener))
    {
        return Err(());
    }

    let mut child = Command::new("python3")
        .arg("-c")
        .arg(PIDFD_SIGNAL_SCRIPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let input = format!(
        "{}\n{}\n{}\n",
        listener.pid, listener.start_time, listener.comm
    );
    let write_result = child.stdin.take().ok_or(())?.write_all(input.as_bytes());
    if write_result.is_err() {
        let _ = child.wait();
        return Err(());
    }
    child
        .wait()
        .map_err(|_| ())?
        .success()
        .then_some(())
        .ok_or(())
}

fn process_identity(pid: u32, uid: u32) -> Option<(String, u64)> {
    let process_path = format!("/proc/{pid}");
    if fs::metadata(&process_path).ok()?.uid() != uid {
        return None;
    }
    let comm = fs::read_to_string(format!("{process_path}/comm"))
        .ok()?
        .trim_end_matches('\n')
        .to_owned();
    if comm.is_empty() || comm.chars().any(char::is_control) {
        return None;
    }
    let stat = fs::read_to_string(format!("{process_path}/stat")).ok()?;
    let closing = stat.rfind(')')?;
    let start_time = stat[closing + 1..]
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    Some((comm, start_time))
}

fn parse_listener_line(line: &str) -> Vec<(u32, u16)> {
    let columns = line.split_ascii_whitespace().collect::<Vec<_>>();
    let Some(port) = columns
        .get(3)
        .and_then(|local| local.rsplit(':').next())
        .and_then(|port| port.parse::<u16>().ok())
    else {
        return Vec::new();
    };

    line.match_indices("pid=")
        .filter_map(|(start, _)| {
            let value = &line[start + 4..];
            let digits = value.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0)
                .then(|| value[..digits].parse::<u32>().ok())
                .flatten()
                .filter(|pid| *pid > 0)
                .map(|pid| (pid, port))
        })
        .collect()
}

fn is_ignored_comm(comm: &str) -> bool {
    let lower = comm.to_ascii_lowercase();
    lower == "tailscale" || lower == "tailscaled" || lower.starts_with("luna") || lower == "ss"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_parser_keeps_only_pids_and_numeric_ports() {
        let listeners =
            parse_listener_line("LISTEN 0 128 *:5173 *:* users:((\"python3\",pid=1234,fd=4))");
        assert_eq!(listeners, [(1234, 5173)]);
    }

    #[test]
    fn listener_parser_handles_multiple_processes_and_ipv6_port_syntax() {
        let listeners = parse_listener_line(
            "LISTEN 0 128 [::]:3000 *:* users:((\"node\",pid=1234,fd=4),(\"node\",pid=5678,fd=5))",
        );
        assert_eq!(listeners, [(1234, 3000), (5678, 3000)]);
    }

    #[test]
    fn known_private_or_self_services_are_not_stoppable_from_the_server_panel() {
        assert!(is_ignored_comm("tailscaled"));
        assert!(is_ignored_comm("tailscale"));
        assert!(is_ignored_comm("luna-monitor"));
        assert!(is_ignored_comm("luna-monitor"));
        assert!(is_ignored_comm("luna-pwa-relay"));
        assert!(is_ignored_comm("ss"));
        assert!(!is_ignored_comm("sshd"));
        assert!(!is_ignored_comm("postgres"));
    }
}
