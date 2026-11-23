use std::collections::VecDeque;
use std::error::Error;
use std::fs;
use std::path::Path;

mod agents;
mod claude_usage;
mod dashboard;
mod listeners;
mod pwa;
mod services;
mod storage;
mod system_stats;
mod view;
mod weekly_usage;

const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const HISTORY_LIMIT: usize = 60;

type AppResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, Default)]
struct GpuMetrics {
    load_percent: Option<f64>,
    vram_free_bytes: Option<u64>,
    vram_total_bytes: Option<u64>,
}

fn main() {
    if let Err(error) = entry() {
        eprintln!("luna-monitor: {error}");
        std::process::exit(1);
    }
}

fn entry() -> AppResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    let enable_pwa = match args.as_slice() {
        [] => true,
        [option] if option == "--no-pwa" => false,
        _ => {
            return Err(
                "Luna nimmt keine manuellen Messwerte entgegen; nutze --help für Details".into(),
            )
        }
    };
    dashboard::run_tui(enable_pwa)
}

fn print_help() {
    println!(
        "Luna system monitor\n\
         \n\
         Usage:\n\
           luna\n\
           luna-monitor --no-pwa   Terminal ohne lokalen Webserver\n\
         \n\
         Readings:\n\
           CPU, RAM, local disk usage, physical network throughput, GPU/VRAM,\n\
         and the current CODEX and CLAUDE weekly quotas.\n\
         \n\
         CPU, memory and network use aggregate Linux counters. The server panel\n\
           reduces local TCP listeners to category, port and PID, and includes\n\
         matching user services. It excludes Tailscale, addresses, arguments,\n\
         Codex sessions, prompts and configuration. Stopping requires confirmation.\n\
         The agent panel reads Claude Code and Codex status from their own backends: session files and `claude agents --json` for Claude Code, the Codex app-server socket for Codex. Names and states only; never prompts or histories.\n\
         Press q or Esc to quit, r to refresh, ↑/↓ to select, x to stop, y to confirm."
    );
}

fn push_history_point(history: &mut VecDeque<Option<f64>>, value: Option<f64>) {
    if history.len() == HISTORY_LIMIT {
        history.pop_front();
    }
    history.push_back(value);
}

fn format_reset(seconds: u64, compact: bool) -> String {
    let seconds = seconds.saturating_add(59) / 60 * 60;
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if compact {
        if days > 0 {
            format!("{days}T{hours}h")
        } else if hours > 0 {
            format!("{hours}h{minutes}m")
        } else {
            format!("{minutes}m")
        }
    } else if days > 0 {
        format!("{days} T {hours} Std")
    } else if hours > 0 {
        format!("{hours} Std {minutes} Min")
    } else {
        format!("{minutes} Min")
    }
}

fn read_gpu() -> GpuMetrics {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return GpuMetrics::default();
    };

    let mut loads = Vec::new();
    let mut vram_free_total = 0_u64;
    let mut vram_capacity_total = 0_u64;
    let mut has_vram = false;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_drm_card(&name) {
            continue;
        }
        let device = entry.path().join("device");
        if let Some(load) =
            read_integer(&device.join("gpu_busy_percent")).filter(|load| *load <= 100)
        {
            loads.push(load as f64);
        }

        let used = read_integer(&device.join("mem_info_vram_used"));
        let total = read_integer(&device.join("mem_info_vram_total"));
        if let (Some(used), Some(total)) = (used, total) {
            if total > 0 && used <= total {
                vram_free_total = vram_free_total.saturating_add(total - used);
                vram_capacity_total = vram_capacity_total.saturating_add(total);
                has_vram = true;
            }
        }
    }

    GpuMetrics {
        load_percent: loads.into_iter().reduce(f64::max),
        vram_free_bytes: has_vram.then_some(vram_free_total),
        vram_total_bytes: has_vram.then_some(vram_capacity_total),
    }
}

fn is_drm_card(name: &str) -> bool {
    name.strip_prefix("card").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn read_integer(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_filter_excludes_connectors_and_non_numeric_names() {
        assert!(is_drm_card("card0"));
        assert!(!is_drm_card("card0-eDP-1"));
        assert!(!is_drm_card("renderD128"));
    }

    #[test]
    fn history_keeps_only_the_latest_minute_of_samples() {
        let mut history = VecDeque::new();
        for value in 0..=HISTORY_LIMIT {
            push_history_point(&mut history, Some(value as f64));
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.front(), Some(&Some(1.0)));
        assert_eq!(history.back(), Some(&Some(HISTORY_LIMIT as f64)));
    }
}
