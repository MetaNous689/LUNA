//! Filesystem usage. Rust's std has no `statvfs`, and Luna forbids `unsafe`,
//! so free space comes from one POSIX `df -P -B1` per refresh — at the storage
//! cadence that is one process every few minutes, not a per-second spawn.

use std::process::Command;

#[derive(Debug, Clone)]
pub struct DiskSnap {
    pub source: String,
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

impl DiskSnap {
    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f64 / self.total as f64 * 100.0
        }
    }
}

/// Pseudo filesystems that carry no useful "disk is filling up" signal.
const IGNORED_TYPES: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "pstore",
    "efivarfs",
    "bpf",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "binfmt_misc",
    "autofs",
    "ramfs",
    "squashfs",
    "overlay",
    "nsfs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
];

pub struct StorageProvider {
    mounts_path: std::path::PathBuf,
}

impl Default for StorageProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageProvider {
    pub fn new() -> Self {
        Self {
            mounts_path: std::path::PathBuf::from("/proc/mounts"),
        }
    }

    /// Mount points worth showing: real block/network filesystems, de-duplicated
    /// by device so a bind mount is not counted twice.
    pub fn candidate_mounts(&self) -> Vec<(String, String)> {
        let Ok(text) = std::fs::read_to_string(&self.mounts_path) else {
            return Vec::new();
        };
        parse_mounts(&text)
    }

    pub fn collect(&self, wanted: &[String]) -> Result<Vec<DiskSnap>, String> {
        let mounts = self.candidate_mounts();
        let selected: Vec<(String, String)> = if wanted.is_empty() {
            mounts
        } else {
            mounts
                .into_iter()
                .filter(|(_, m)| wanted.iter().any(|w| w == m))
                .collect()
        };
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let mut cmd = Command::new("df");
        cmd.args(["-P", "-B1"]);
        for (_, mount) in &selected {
            cmd.arg(mount);
        }
        let out = cmd.output().map_err(|e| format!("df: {e}"))?;
        // df exits non-zero when *one* path is gone but still prints the rest.
        let text = String::from_utf8_lossy(&out.stdout);
        let mut snaps = parse_df(&text);
        if snaps.is_empty() && !out.status.success() {
            return Err(format!(
                "df fehlgeschlagen: {}",
                sanitize_bounded(String::from_utf8_lossy(&out.stderr).trim(), 120)
            ));
        }
        for s in &mut snaps {
            if let Some((src, _)) = selected.iter().find(|(_, m)| *m == s.mount) {
                if s.source.is_empty() {
                    s.source = src.clone();
                }
            }
        }
        Ok(snaps)
    }
}

fn parse_mounts(text: &str) -> Vec<(String, String)> {
    let mut seen_sources: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let (source, mount, fstype) = (f[0], unescape_mount(f[1]), f[2]);
        if IGNORED_TYPES.contains(&fstype) {
            continue;
        }
        if !source.starts_with('/') && !source.contains(':') && !source.starts_with("//") {
            continue;
        }
        if seen_sources.iter().any(|s| s == source) {
            continue;
        }
        seen_sources.push(source.to_string());
        out.push((source.to_string(), mount));
    }
    out
}

/// `/proc/mounts` escapes spaces and tabs as octal.
fn unescape_mount(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 4], 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn parse_df(text: &str) -> Vec<DiskSnap> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            if f.len() < 6 {
                return None;
            }
            let total: u64 = f[1].parse().ok()?;
            let used: u64 = f[2].parse().ok()?;
            let available: u64 = f[3].parse().ok()?;
            if total == 0 {
                return None;
            }
            // The mount point is the rest of the line: it may contain spaces.
            let mount = line
                .split_ascii_whitespace()
                .skip(5)
                .collect::<Vec<_>>()
                .join(" ");
            Some(DiskSnap {
                source: sanitize_bounded(f[0], 64),
                mount: sanitize_bounded(&mount, 96),
                total,
                used,
                available,
            })
        })
        .collect()
}

/// Strip control characters and cap the length so a hostile mount name cannot
/// break the terminal.
fn sanitize_bounded(text: &str, max: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_filesystems_and_bind_duplicates_are_filtered() {
        let mounts = "\
proc /proc proc rw 0 0
/dev/nvme0n1p2 / ext4 rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/nvme0n1p2 /var ext4 rw 0 0
/dev/sda1 /mnt/games ntfs3 rw 0 0
nas:/export /mnt/nas nfs4 rw 0 0
";
        let got = parse_mounts(mounts);
        let points: Vec<&str> = got.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(points, vec!["/", "/mnt/games", "/mnt/nas"]);
    }

    #[test]
    fn mount_points_with_escaped_spaces_are_decoded() {
        let got = parse_mounts("/dev/sdb1 /mnt/my\\040disk ext4 rw 0 0\n");
        assert_eq!(got[0].1, "/mnt/my disk");
    }

    #[test]
    fn df_output_is_parsed_including_spaced_mount_points() {
        let text = "\
Filesystem      1B-blocks         Used    Available Capacity Mounted on
/dev/nvme0n1p2  500107862016 300064710656 174654382080      64% /
/dev/sda1       1000204886016 900184403968  99000000000      91% /mnt/my disk
";
        let snaps = parse_df(text);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].mount, "/");
        assert_eq!(snaps[0].total, 500_107_862_016);
        assert!((snaps[0].used_percent() - 60.0).abs() < 0.5);
        assert_eq!(snaps[1].mount, "/mnt/my disk");
        assert!(snaps[1].used_percent() > 89.0);
    }

    #[test]
    fn zero_sized_and_broken_rows_are_dropped() {
        let text = "H\n/dev/x 0 0 0 0% /empty\ngarbage\n";
        assert!(parse_df(text).is_empty());
    }

    #[test]
    fn reads_the_real_filesystem_table() {
        let p = StorageProvider::new();
        let mounts = p.candidate_mounts();
        assert!(
            mounts.iter().any(|(_, m)| m == "/"),
            "no root mount found: {mounts:?}"
        );
        let snaps = p.collect(&[]).unwrap();
        assert!(snaps.iter().any(|s| s.mount == "/"));
        assert!(snaps.iter().all(|s| s.total > 0));
    }

    #[test]
    fn an_explicit_disk_filter_narrows_the_result() {
        let p = StorageProvider::new();
        let snaps = p.collect(&["/".to_string()]).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].mount, "/");
    }
}
