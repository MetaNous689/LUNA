//! Narrow host telemetry for the terminal dashboard.
//!
//! Reads only aggregate CPU, memory, local block-volume and network counters.
//! It never scans processes, IP addresses, sessions, prompts or Tailscale configuration.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::storage::{DiskSnap, StorageProvider};

const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
pub struct CpuMetrics {
    pub model: Option<String>,
    pub load_percent: Option<f64>,
    pub per_core_percent: Vec<Option<f64>>,
    pub frequency_mhz: Option<u64>,
    pub temperature_c: Option<f64>,
    pub power_watts: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct MemoryMetrics {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub cached_bytes: u64,
    pub free_bytes: u64,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub psi_some_avg10_percent: Option<f64>,
    pub psi_full_avg10_percent: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct NetworkMetrics {
    pub interface: Option<String>,
    pub download_bytes_per_second: Option<f64>,
    pub upload_bytes_per_second: Option<f64>,
    pub download_session_bytes: u64,
    pub upload_session_bytes: u64,
    pub download_peak_bytes_per_second: f64,
    pub upload_peak_bytes_per_second: f64,
}

#[derive(Clone, Debug, Default)]
pub struct SystemMetrics {
    pub cpu: CpuMetrics,
    pub memory: Option<MemoryMetrics>,
    pub disks: Option<Vec<DiskSnap>>,
    pub network: NetworkMetrics,
}

#[derive(Clone, Copy, Debug, Default)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct NetCounters {
    received: u64,
    sent: u64,
}

struct NetCandidate {
    name: String,
    counters: NetCounters,
    deltas: Option<(u64, u64)>,
    rates: Option<(f64, f64)>,
}

struct PowerReading {
    path: PathBuf,
    energy_microjoules: u64,
    max_energy_microjoules: u64,
}

pub struct SystemCollector {
    cpu_model: Option<String>,
    previous_cpu: Vec<CpuTimes>,
    previous_network: BTreeMap<String, NetCounters>,
    previous_network_at: Option<Instant>,
    active_interface: Option<String>,
    network_totals: (u64, u64),
    network_peaks: (f64, f64),
    previous_power: Option<(PathBuf, u64, Instant)>,
    storage: StorageProvider,
    disks: Option<Vec<DiskSnap>>,
    last_disk_refresh: Option<Instant>,
}

impl SystemCollector {
    pub fn new() -> Self {
        Self {
            cpu_model: read_cpu_model(),
            previous_cpu: Vec::new(),
            previous_network: BTreeMap::new(),
            previous_network_at: None,
            active_interface: None,
            network_totals: (0, 0),
            network_peaks: (0.0, 0.0),
            previous_power: None,
            storage: StorageProvider::new(),
            disks: None,
            last_disk_refresh: None,
        }
    }

    pub fn sample(&mut self, refresh_disks: bool) -> SystemMetrics {
        let power_watts = self.sample_cpu_power();
        let (load_percent, per_core_percent) = self.sample_cpu_load();
        let cpu = CpuMetrics {
            model: self.cpu_model.clone(),
            load_percent,
            per_core_percent,
            frequency_mhz: read_cpu_frequency_mhz(),
            temperature_c: read_cpu_temperature(),
            power_watts,
        };
        let network = self.sample_network();
        self.refresh_disks(refresh_disks);
        SystemMetrics {
            cpu,
            memory: read_memory(),
            disks: self.disks.clone(),
            network,
        }
    }

    fn sample_cpu_load(&mut self) -> (Option<f64>, Vec<Option<f64>>) {
        let Ok(text) = fs::read_to_string("/proc/stat") else {
            self.previous_cpu.clear();
            return (None, Vec::new());
        };
        let current = parse_cpu_times(&text);
        if current.is_empty() {
            self.previous_cpu.clear();
            return (None, Vec::new());
        }
        let loads = current
            .iter()
            .zip(&self.previous_cpu)
            .map(|(now, previous)| cpu_load(*previous, *now))
            .collect::<Vec<_>>();
        self.previous_cpu = current;
        let aggregate = loads.first().copied().flatten();
        (aggregate, loads.into_iter().skip(1).collect())
    }

    fn sample_cpu_power(&mut self) -> Option<f64> {
        let Some(reading) = read_package_energy() else {
            self.previous_power = None;
            return read_hwmon_cpu_power();
        };
        let now = Instant::now();
        let watts = self
            .previous_power
            .as_ref()
            .and_then(|(path, previous, at)| {
                if path != &reading.path {
                    return None;
                }
                let elapsed = now.duration_since(*at).as_secs_f64();
                if elapsed <= 0.0 {
                    return None;
                }
                let delta = reading
                    .energy_microjoules
                    .checked_sub(*previous)
                    .or_else(|| {
                        (reading.max_energy_microjoules > 0).then(|| {
                            reading
                                .max_energy_microjoules
                                .saturating_sub(*previous)
                                .saturating_add(reading.energy_microjoules)
                        })
                    })?;
                let watts = delta as f64 / 1_000_000.0 / elapsed;
                (watts.is_finite() && (0.0..=1000.0).contains(&watts)).then_some(watts)
            });
        self.previous_power = Some((reading.path, reading.energy_microjoules, now));
        watts
    }

    fn sample_network(&mut self) -> NetworkMetrics {
        let current = read_network_counters();
        let now = Instant::now();
        let elapsed = self
            .previous_network_at
            .map(|previous| now.duration_since(previous).as_secs_f64())
            .filter(|elapsed| *elapsed > 0.0);
        let mut candidates = Vec::<NetCandidate>::new();
        for (name, counters) in &current {
            if !is_physical_interface(name) {
                continue;
            }
            let previous = self.previous_network.get(name).copied();
            let deltas = previous.and_then(|previous| {
                Some((
                    counters.received.checked_sub(previous.received)?,
                    counters.sent.checked_sub(previous.sent)?,
                ))
            });
            let rates = deltas.zip(elapsed).map(|((received, sent), elapsed)| {
                (received as f64 / elapsed, sent as f64 / elapsed)
            });
            candidates.push(NetCandidate {
                name: name.clone(),
                counters: *counters,
                deltas,
                rates,
            });
        }
        self.previous_network = current;
        self.previous_network_at = Some(now);

        if candidates.is_empty() {
            self.active_interface = None;
            self.network_totals = (0, 0);
            self.network_peaks = (0.0, 0.0);
            return NetworkMetrics::default();
        }

        let best = candidates
            .iter()
            .filter_map(|candidate| {
                candidate
                    .rates
                    .map(|(received, sent)| (candidate.name.as_str(), received + sent))
            })
            .max_by(|left, right| left.1.total_cmp(&right.1));
        let selected_name = best
            .filter(|(_, rate)| *rate > 0.0)
            .map(|(name, _)| name.to_owned())
            .or_else(|| {
                self.active_interface
                    .as_ref()
                    .filter(|name| candidates.iter().any(|candidate| &candidate.name == *name))
                    .cloned()
            })
            .or_else(|| {
                candidates
                    .iter()
                    .max_by_key(|candidate| {
                        candidate
                            .counters
                            .received
                            .saturating_add(candidate.counters.sent)
                    })
                    .map(|candidate| candidate.name.clone())
            });
        let Some(selected_name) = selected_name else {
            return NetworkMetrics::default();
        };
        let Some(selected) = candidates
            .iter()
            .find(|candidate| candidate.name == selected_name)
        else {
            return NetworkMetrics::default();
        };

        if self.active_interface.as_deref() != Some(selected_name.as_str()) {
            self.active_interface = Some(selected_name.clone());
            self.network_totals = (0, 0);
            self.network_peaks = (0.0, 0.0);
        }
        let (download_rate, upload_rate) = selected.rates.unwrap_or((0.0, 0.0));
        if let Some((received, sent)) = selected.deltas {
            self.network_totals.0 = self.network_totals.0.saturating_add(received);
            self.network_totals.1 = self.network_totals.1.saturating_add(sent);
            self.network_peaks.0 = self.network_peaks.0.max(download_rate);
            self.network_peaks.1 = self.network_peaks.1.max(upload_rate);
        }

        NetworkMetrics {
            interface: Some(selected_name),
            download_bytes_per_second: selected.rates.map(|(download, _)| download),
            upload_bytes_per_second: selected.rates.map(|(_, upload)| upload),
            download_session_bytes: self.network_totals.0,
            upload_session_bytes: self.network_totals.1,
            download_peak_bytes_per_second: self.network_peaks.0,
            upload_peak_bytes_per_second: self.network_peaks.1,
        }
    }

    fn refresh_disks(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && self
                .last_disk_refresh
                .is_some_and(|last| now.duration_since(last) < DISK_REFRESH_INTERVAL)
        {
            return;
        }
        self.last_disk_refresh = Some(now);

        let wanted = self
            .storage
            .candidate_mounts()
            .into_iter()
            .filter(|(source, _)| source.starts_with("/dev/"))
            .map(|(_, mount)| mount)
            .collect::<Vec<_>>();
        if wanted.is_empty() {
            self.disks = Some(Vec::new());
            return;
        }
        let Ok(mut disks) = self.storage.collect(&wanted) else {
            self.disks = None;
            return;
        };
        disks.sort_by(|left, right| {
            (left.mount != "/")
                .cmp(&(right.mount != "/"))
                .then_with(|| left.mount.cmp(&right.mount))
        });
        self.disks = Some(disks);
    }
}

fn parse_cpu_times(text: &str) -> Vec<CpuTimes> {
    text.lines()
        .take_while(|line| line.starts_with("cpu"))
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let name = fields.next()?;
            if name != "cpu"
                && !name.strip_prefix("cpu").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                })
            {
                return None;
            }
            let values = fields
                .map(str::parse::<u64>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            if values.len() < 4 {
                return None;
            }
            let total = values.iter().take(8).copied().sum();
            let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
            Some(CpuTimes { total, idle })
        })
        .collect()
}

fn cpu_load(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total = current.total.checked_sub(previous.total)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    (total > 0)
        .then(|| (total.saturating_sub(idle) as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
}

fn read_memory() -> Option<MemoryMetrics> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let pressure = fs::read_to_string("/proc/pressure/memory").ok();
    parse_memory(&text, pressure.as_deref())
}

fn parse_memory(text: &str, pressure: Option<&str>) -> Option<MemoryMetrics> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(kib) = value
            .split_ascii_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        values.insert(key, kib.saturating_mul(1024));
    }
    let total_bytes = *values.get("MemTotal")?;
    let available_bytes = (*values.get("MemAvailable")?).min(total_bytes);
    let swap = values
        .get("SwapTotal")
        .zip(values.get("SwapFree"))
        .and_then(|(&total, &free)| total.checked_sub(free).map(|used| (total, used)));
    Some(MemoryMetrics {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(available_bytes),
        available_bytes,
        cached_bytes: *values.get("Cached").unwrap_or(&0),
        free_bytes: *values.get("MemFree").unwrap_or(&0),
        swap_total_bytes: swap.map(|(total, _)| total),
        swap_used_bytes: swap.map(|(_, used)| used),
        psi_some_avg10_percent: pressure.and_then(|text| parse_memory_pressure(text, "some")),
        psi_full_avg10_percent: pressure.and_then(|text| parse_memory_pressure(text, "full")),
    })
}

fn parse_memory_pressure(text: &str, kind: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        if fields.next()? != kind {
            return None;
        }
        let value = fields
            .find_map(|field| field.strip_prefix("avg10="))?
            .parse::<f64>()
            .ok()?;
        (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
    })
}

fn read_cpu_model() -> Option<String> {
    let text = fs::read_to_string("/proc/cpuinfo").ok()?;
    let model = text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        matches!(key.trim(), "model name" | "Hardware")
            .then(|| value.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
    })?;
    let model = model
        .chars()
        .filter(|ch| !ch.is_control())
        .take(48)
        .collect::<String>();
    (!model.is_empty()).then_some(model)
}

fn read_cpu_frequency_mhz() -> Option<u64> {
    let root = fs::read_dir("/sys/devices/system/cpu").ok()?;
    let mut frequencies = Vec::new();
    for entry in root.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.strip_prefix("cpu").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            continue;
        }
        if let Ok(value) = fs::read_to_string(entry.path().join("cpufreq/scaling_cur_freq")) {
            if let Ok(khz) = value.trim().parse::<u64>() {
                frequencies.push(khz / 1000);
            }
        }
    }
    if frequencies.is_empty() {
        let text = fs::read_to_string("/proc/cpuinfo").ok()?;
        frequencies.extend(text.lines().filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "cpu MHz")
                .then(|| value.trim().parse::<f64>().ok())
                .flatten()
                .filter(|mhz| mhz.is_finite() && *mhz > 0.0)
                .map(|mhz| mhz.round() as u64)
        }));
    }
    frequencies.into_iter().max()
}

fn read_cpu_temperature() -> Option<f64> {
    let root = fs::read_dir("/sys/class/hwmon").ok()?;
    let mut readings = Vec::new();
    for entry in root.flatten() {
        let path = entry.path();
        let Ok(name) = fs::read_to_string(path.join("name")) else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if !matches!(name.as_str(), "k10temp" | "zenpower" | "coretemp") {
            continue;
        }
        let Ok(sensors) = fs::read_dir(path) else {
            continue;
        };
        for sensor in sensors.flatten() {
            let file = sensor.file_name();
            let file = file.to_string_lossy();
            if file.starts_with("temp") && file.ends_with("_input") {
                if let Ok(value) = fs::read_to_string(sensor.path()) {
                    if let Ok(millidegrees) = value.trim().parse::<f64>() {
                        let degrees = millidegrees / 1000.0;
                        if (0.0..=150.0).contains(&degrees) {
                            readings.push(degrees);
                        }
                    }
                }
            }
        }
    }
    readings.into_iter().reduce(f64::max)
}

fn read_hwmon_cpu_power() -> Option<f64> {
    let root = fs::read_dir("/sys/class/hwmon").ok()?;
    let mut readings = Vec::new();
    for entry in root.flatten() {
        let path = entry.path();
        let Ok(name) = fs::read_to_string(path.join("name")) else {
            continue;
        };
        if !matches!(
            name.trim().to_ascii_lowercase().as_str(),
            "k10temp" | "zenpower"
        ) {
            continue;
        }
        for sensor in ["power1_average", "power1_input"] {
            if let Some(microwatts) = read_u64(&path.join(sensor)) {
                let watts = microwatts as f64 / 1_000_000.0;
                if watts.is_finite() && (0.0..=1000.0).contains(&watts) {
                    readings.push(watts);
                    break;
                }
            }
        }
    }
    readings.into_iter().reduce(f64::max)
}

fn read_package_energy() -> Option<PowerReading> {
    let root = fs::read_dir("/sys/class/powercap").ok()?;
    for entry in root.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains("rapl") || name.matches(':').count() > 1 {
            continue;
        }
        let label = fs::read_to_string(path.join("name")).unwrap_or_default();
        let label = label.to_ascii_lowercase();
        let is_package = label.contains("package")
            || label.contains("cpu")
            || name.starts_with("amd-rapl")
            || name.starts_with("intel-rapl");
        if !is_package {
            continue;
        }
        let Some(energy_microjoules) = read_u64(&path.join("energy_uj")) else {
            continue;
        };
        let Some(max_energy_microjoules) = read_u64(&path.join("max_energy_range_uj")) else {
            continue;
        };
        return Some(PowerReading {
            path,
            energy_microjoules,
            max_energy_microjoules,
        });
    }
    None
}

fn read_u64(path: &std::path::Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_network_counters() -> BTreeMap<String, NetCounters> {
    let Ok(text) = fs::read_to_string("/proc/net/dev") else {
        return BTreeMap::new();
    };
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, fields) = line.split_once(':')?;
            let name = name.trim();
            if name.starts_with("tailscale") || !is_physical_interface(name) {
                return None;
            }
            let values = fields
                .split_ascii_whitespace()
                .map(str::parse::<u64>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            Some((
                name.to_owned(),
                NetCounters {
                    received: *values.first()?,
                    sent: *values.get(8)?,
                },
            ))
        })
        .collect()
}

fn is_physical_interface(name: &str) -> bool {
    if name == "lo" {
        return false;
    }
    let kind_path = PathBuf::from("/sys/class/net").join(name).join("type");
    if let Ok(kind) = fs::read_to_string(kind_path) {
        return kind.trim() == "1";
    }
    !["tailscale", "tun", "tap", "wg", "veth", "docker", "br-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: &str =
        "MemTotal: 32768 kB\nMemAvailable: 16384 kB\nMemFree: 8192 kB\nCached: 4096 kB\n";

    #[test]
    fn memory_swap_and_pressure_use_independent_kernel_counters() {
        let memory = parse_memory(
            &format!("{RAM}SwapTotal: 8192 kB\nSwapFree: 2048 kB\n"),
            Some("full avg60=1.00 avg10=0.25 total=123\nsome total=456 avg10=2.34 avg300=3.00\n"),
        )
        .unwrap();
        assert_eq!(memory.total_bytes, 32768 * 1024);
        assert_eq!(memory.used_bytes, 16384 * 1024);
        assert_eq!(memory.swap_total_bytes, Some(8192 * 1024));
        assert_eq!(memory.swap_used_bytes, Some(6144 * 1024));
        assert_eq!(memory.psi_some_avg10_percent, Some(2.34));
        assert_eq!(memory.psi_full_avg10_percent, Some(0.25));
    }

    #[test]
    fn disabled_swap_is_measured_zero_and_missing_swap_is_unavailable() {
        let zero = parse_memory(&format!("{RAM}SwapTotal: 0 kB\nSwapFree: 0 kB\n"), None).unwrap();
        assert_eq!(
            (zero.swap_total_bytes, zero.swap_used_bytes),
            (Some(0), Some(0))
        );
        for swap in [
            "",
            "SwapTotal: 8192 kB\n",
            "SwapTotal: 4 kB\nSwapFree: 5 kB\n",
            "SwapTotal: bad kB\nSwapFree: 0 kB\n",
        ] {
            let memory = parse_memory(&format!("{RAM}{swap}"), None).unwrap();
            assert_eq!(
                (memory.swap_total_bytes, memory.swap_used_bytes),
                (None, None)
            );
            assert_eq!(memory.available_bytes, 16384 * 1024);
            assert_eq!(memory.psi_some_avg10_percent, None);
        }
    }

    #[test]
    fn pressure_rejects_invalid_values_without_inventing_zero() {
        for invalid in ["NaN", "inf", "-1.00", "100.01", "invalid"] {
            let text = format!("some avg10={invalid} avg60=0.00\nfull avg10=0.00\n");
            let memory = parse_memory(RAM, Some(&text)).unwrap();
            assert_eq!(memory.psi_some_avg10_percent, None);
            assert_eq!(memory.psi_full_avg10_percent, Some(0.0));
        }
        assert_eq!(parse_memory_pressure("some avg60=12.34", "some"), None);
        assert_eq!(
            parse_memory_pressure("some avg10=100.00", "some"),
            Some(100.0)
        );
        assert_eq!(parse_memory_pressure("some avg10=0.00", "full"), None);
    }
}
