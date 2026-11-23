//! A narrow look at the process tree, for what an agent backend does not say.
//!
//! The Codex app-server reports a thread as idle as soon as its turn ends, even
//! while a command the agent started in the background is still running, and
//! offers no way to ask. Such a command runs under the app-server process, so
//! this module finds that process and lists the working folders of the sandbox
//! helpers (`bwrap`) directly below it.
//!
//! Only a process's parent, name, start time and working folder are read, never
//! its arguments or its environment.

use std::fs;
use std::path::PathBuf;

/// Start time of a process in clock ticks since boot, `/proc/<pid>/stat` field
/// 22. It tells a live process from a recycled process id.
pub(super) fn process_start(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    after_name.split_whitespace().nth(19).map(str::to_owned)
}

/// `(parent, name)` of a process.
fn parent_and_name(pid: u32) -> Option<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let name = stat.get(stat.find('(')? + 1..stat.rfind(')')?)?.to_owned();
    let parent = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((parent, name))
}

fn all_pids() -> impl Iterator<Item = u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
}

/// The inode of the listening Unix socket at `path`, from `/proc/net/unix`.
pub(super) fn socket_inode(table: &str, path: &str) -> Option<u64> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Num RefCount Protocol Flags Type St Inode Path; a listener has the
        // accept flag 00010000 set.
        let listening = u32::from_str_radix(fields.get(3)?, 16).ok()? & 0x0001_0000 != 0;
        (listening && fields.get(7).copied() == Some(path))
            .then(|| fields.get(6)?.parse().ok())
            .flatten()
    })
}

/// The oldest process of this user that holds the socket with this inode: the
/// server, not a child that inherited the descriptor.
pub(super) fn listener_pid(inode: u64) -> Option<u32> {
    let wanted = format!("socket:[{inode}]");
    all_pids()
        .filter(|pid| {
            fs::read_dir(format!("/proc/{pid}/fd"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|fd| fs::read_link(fd.path()).is_ok_and(|t| t.to_string_lossy() == wanted))
        })
        .filter_map(|pid| Some((process_start(pid)?.parse::<u64>().ok()?, pid)))
        .min()
        .map(|(_, pid)| pid)
}

/// Working folders of the direct children of `parent` that are called `name`.
pub(super) fn child_dirs(parent: u32, name: &str) -> Vec<String> {
    all_pids()
        .filter(|pid| parent_and_name(*pid).is_some_and(|(p, n)| p == parent && n == name))
        .filter_map(|pid| fs::read_link(format!("/proc/{pid}/cwd")).ok())
        .map(normalize)
        .collect()
}

/// A folder as a comparable string: no trailing slash.
pub(super) fn normalize(path: impl Into<PathBuf>) -> String {
    let text = path.into().to_string_lossy().into_owned();
    let trimmed = text.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn the_listening_socket_is_found_in_the_unix_table() {
        let table = "\
Num       RefCount Protocol Flags    Type St Inode Path
ffff0001: 00000002 00000000 00010000 0001 01 418382 /tmp/codex-daemon-1000/abc
ffff0002: 00000003 00000000 00000000 0001 03 418999 /tmp/codex-daemon-1000/abc
ffff0003: 00000002 00000000 00010000 0001 01 777 /run/user/1000/other.sock
";
        assert_eq!(
            socket_inode(table, "/tmp/codex-daemon-1000/abc"),
            Some(418382)
        );
        assert_eq!(socket_inode(table, "/run/user/1000/other.sock"), Some(777));
        assert_eq!(socket_inode(table, "/tmp/missing"), None);
        assert_eq!(socket_inode("garbage", "/x"), None);
    }

    #[test]
    fn children_are_listed_by_name_with_their_working_folder() {
        let dir = std::env::temp_dir();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(&dir)
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep");
        let found = child_dirs(std::process::id(), "sleep");
        let other = child_dirs(std::process::id(), "no-such-process");
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            found.contains(&normalize(fs::canonicalize(&dir).unwrap())),
            "{found:?}"
        );
        assert!(other.is_empty());
    }

    #[test]
    fn this_process_holds_a_socket_it_listens_on() {
        let path = std::env::temp_dir().join(format!("luna-sock-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let table = fs::read_to_string("/proc/net/unix").unwrap();
        let inode = socket_inode(&table, path.to_str().unwrap()).expect("listener in table");
        let owner = listener_pid(inode);
        drop(listener);
        let _ = fs::remove_file(&path);

        assert_eq!(owner, Some(std::process::id()));
    }

    #[test]
    fn folders_compare_without_a_trailing_slash() {
        assert_eq!(normalize("/work/app/"), "/work/app");
        assert_eq!(normalize("/work/app"), "/work/app");
        assert_eq!(normalize("/"), "/");
    }
}
