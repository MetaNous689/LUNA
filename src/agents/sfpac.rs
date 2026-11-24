//! Read-only SFPAC slot order. Agent status still comes from Codex and Claude.
//!
//! The registry is optional: Luna remains useful when SFPAC is stopped or not
//! installed. This client asks for snapshots only and never attaches to a PTY.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

const MAX_FRAME: usize = 2 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_millis(400);
const MAX_SKIPPED_EVENTS: usize = 32;

/// SFPAC order keys use the same identities the native backends expose to Luna.
pub(super) fn current_order() -> Option<HashMap<String, usize>> {
    for socket in socket_candidates() {
        if let Some(order) = read_order(&socket) {
            return Some(order);
        }
    }
    None
}

fn socket_candidates() -> Vec<PathBuf> {
    if let Some(directory) = std::env::var_os("SFPAC_SOCKET_DIR") {
        return vec![PathBuf::from(directory).join("runtime.sock")];
    }

    let uid = fs::metadata("/proc/self")
        .ok()
        .map(|metadata| metadata.uid());
    let Some(uid) = uid else { return Vec::new() };
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    let parent = if is_owned_directory(&runtime, uid) {
        runtime
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        return Vec::new();
    };

    let mut sockets = Vec::new();
    let Ok(entries) = fs::read_dir(parent) else {
        return sockets;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(suffix) = name.strip_prefix("sfpac-") else {
            continue;
        };
        if suffix.len() != 10 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let directory = entry.path();
        if !is_owned_directory(&directory, uid) {
            continue;
        }
        let socket = directory.join("runtime.sock");
        if is_owned_socket(&socket, uid) {
            sockets.push(socket);
        }
    }
    sockets.sort();
    sockets
}

fn is_owned_directory(path: &Path, uid: u32) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_dir() && metadata.uid() == uid)
}

fn is_owned_socket(path: &Path, uid: u32) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_socket()
            && metadata.uid() == uid
            && metadata.mode() & 0o777 == 0o600
    })
}

fn read_order(path: &Path) -> Option<HashMap<String, usize>> {
    let uid = fs::metadata("/proc/self").ok()?.uid();
    if !is_owned_directory(path.parent()?, uid) || !is_owned_socket(path, uid) {
        return None;
    }

    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let hello = request(
        &mut stream,
        &mut reader,
        1,
        "hello",
        json!({"read_only": true}),
    )?;
    if hello.get("registry_protocol_revision")?.as_u64()? < 3 {
        return None;
    }
    let snapshot = request(&mut stream, &mut reader, 2, "snapshot", json!({}))?;
    let agents = snapshot.get("agents")?.as_array()?;
    let groups = snapshot
        .get("project_groups")
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .enumerate()
                .filter_map(|(index, group)| group.as_str().map(|group| (group.to_owned(), index)))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut ordered: Vec<(usize, usize, &Value)> = agents
        .iter()
        .enumerate()
        .map(|(slot_index, agent)| {
            let group = project_group(agent, home.as_deref());
            (
                groups.get(&group).copied().unwrap_or(groups.len()),
                slot_index,
                agent,
            )
        })
        .collect();
    // SFPAC displays project groups first, then keeps persisted slot order
    // within each group. Snapshot rows themselves are only slot-ordered.
    ordered.sort_by_key(|(group_rank, slot_index, _)| (*group_rank, *slot_index));

    let mut order = HashMap::new();
    for (index, (_, _, agent)) in ordered.into_iter().enumerate() {
        if let Some(thread) = agent.get("codex_thread_id").and_then(Value::as_str) {
            order.entry(format!("codex:{thread}")).or_insert(index);
        }
        if let Some(pid) = agent.get("claude_process_pid").and_then(Value::as_u64) {
            if let Ok(pid) = u32::try_from(pid) {
                order.entry(format!("claude:{pid}")).or_insert(index);
            }
        }
    }
    Some(order)
}

fn project_group(agent: &Value, home: Option<&Path>) -> String {
    if let Some(group) = agent
        .get("project_group")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|group| !group.is_empty())
    {
        return group.to_owned();
    }

    let path = ["repo_root", "cwd", "worktree"]
        .into_iter()
        .filter_map(|field| agent.get(field).and_then(Value::as_str))
        .find(|path| !path.is_empty());
    let Some(path) = path else {
        return "Ungrouped".to_owned();
    };
    if home.is_some_and(|home| home == Path::new(path)) {
        return "Ungrouped".to_owned();
    }
    Path::new(path.trim_end_matches('/'))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Ungrouped".to_owned())
}

fn request(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Option<Value> {
    let mut frame =
        serde_json::to_vec(&json!({"id": id, "method": method, "params": params})).ok()?;
    frame.push(b'\n');
    if frame.len() > MAX_FRAME {
        return None;
    }
    stream.write_all(&frame).ok()?;

    for _ in 0..MAX_SKIPPED_EVENTS {
        let mut response = Vec::new();
        let length = reader
            .by_ref()
            .take((MAX_FRAME + 1) as u64)
            .read_until(b'\n', &mut response)
            .ok()?;
        if length == 0 || length > MAX_FRAME || response.last() != Some(&b'\n') {
            return None;
        }
        let response: Value = serde_json::from_slice(&response).ok()?;
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if response.get("error").is_some() {
            return None;
        }
        return response.get("result").cloned();
    }
    None
}
