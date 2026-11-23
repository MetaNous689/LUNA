use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::json;

use super::agents::{AgentStatusCollector, AgentStatusView};
use super::services::{LocalServer, ServerManager};
use super::system_stats::{MemoryMetrics, SystemCollector, SystemMetrics};
use super::view::{self, View};
use super::weekly_usage::{Source, WeeklyUsage};
use super::{push_history_point, read_gpu, AppResult, GpuMetrics, HISTORY_LIMIT, SAMPLE_INTERVAL};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct App {
    system: SystemMetrics,
    system_collector: SystemCollector,
    gpu: GpuMetrics,
    codex: WeeklyUsage,
    claude: WeeklyUsage,
    cpu_history: VecDeque<Option<f64>>,
    gpu_history: VecDeque<Option<f64>>,
    download_history: VecDeque<Option<f64>>,
    upload_history: VecDeque<Option<f64>>,
    memory_used_history: VecDeque<Option<f64>>,
    last_history_at: Option<Instant>,
    servers: ServerManager,
    selected_server: usize,
    pending_stop: Option<LocalServer>,
    server_notice: Option<String>,
    agents: AgentStatusCollector,
}

impl App {
    fn new() -> Self {
        Self {
            system: SystemMetrics::default(),
            system_collector: SystemCollector::new(),
            gpu: GpuMetrics::default(),
            codex: WeeklyUsage::new(Source::Codex),
            claude: WeeklyUsage::new(Source::Claude),
            cpu_history: VecDeque::with_capacity(HISTORY_LIMIT),
            gpu_history: VecDeque::with_capacity(HISTORY_LIMIT),
            download_history: VecDeque::with_capacity(HISTORY_LIMIT),
            upload_history: VecDeque::with_capacity(HISTORY_LIMIT),
            memory_used_history: VecDeque::with_capacity(HISTORY_LIMIT),
            last_history_at: None,
            servers: ServerManager::default(),
            selected_server: 0,
            pending_stop: None,
            server_notice: None,
            agents: AgentStatusCollector::new(),
        }
    }

    fn sample(&mut self, refresh_disks: bool) {
        self.system = self.system_collector.sample(refresh_disks);
        self.gpu = read_gpu();
        self.record_history();
    }

    fn record_history(&mut self) {
        let now = Instant::now();
        if self
            .last_history_at
            .is_some_and(|last| now.duration_since(last) < SAMPLE_INTERVAL)
        {
            return;
        }
        self.last_history_at = Some(now);
        push_history_point(&mut self.cpu_history, self.system.cpu.load_percent);
        push_history_point(&mut self.gpu_history, self.gpu.load_percent);
        push_history_point(
            &mut self.download_history,
            self.system.network.download_bytes_per_second,
        );
        push_history_point(
            &mut self.upload_history,
            self.system.network.upload_bytes_per_second,
        );
        let memory = self.system.memory.as_ref();
        push_memory_history(&mut self.memory_used_history, memory, |m| m.used_bytes);
    }

    fn refresh_servers(&mut self) {
        let selected_pid = self
            .servers
            .servers()
            .get(self.selected_server)
            .map(|server| server.pid);
        self.servers.refresh();
        self.normalize_server_selection(selected_pid);
        self.server_notice = None;
    }

    fn refresh_servers_if_due(&mut self) -> bool {
        if !self.servers.refresh_if_due() {
            return false;
        }

        let selected_pid = self
            .servers
            .servers()
            .get(self.selected_server)
            .map(|server| server.pid);
        self.normalize_server_selection(selected_pid);
        if self.pending_stop.as_ref().is_some_and(|pending| {
            !self
                .servers
                .servers()
                .iter()
                .any(|server| server.same_instance(pending))
        }) {
            self.pending_stop = None;
            self.server_notice = Some("Dienstliste geändert · Auswahl erneuert".into());
        } else {
            self.server_notice = None;
        }
        true
    }

    fn normalize_server_selection(&mut self, selected_pid: Option<u32>) {
        let servers = self.servers.servers();
        self.selected_server = selected_pid
            .and_then(|pid| servers.iter().position(|server| server.pid == pid))
            .unwrap_or_else(|| self.selected_server.min(servers.len().saturating_sub(1)));
    }

    fn move_server_selection(&mut self, delta: isize) {
        let len = self.servers.servers().len();
        if len == 0 {
            return;
        }
        self.selected_server = if delta < 0 {
            self.selected_server.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected_server
                .saturating_add(delta as usize)
                .min(len - 1)
        };
        self.server_notice = None;
    }

    fn request_server_stop(&mut self) {
        if let Some(server) = self.servers.servers().get(self.selected_server) {
            self.pending_stop = Some(server.clone());
            self.server_notice = None;
        } else {
            self.server_notice = Some("Kein Server ausgewählt".into());
        }
    }

    fn confirm_server_stop(&mut self) {
        let Some(server) = self.pending_stop.take() else {
            return;
        };
        self.server_notice = Some(if self.servers.stop(&server).is_ok() {
            format!("Stop angefordert · {} PID {}", server.label(), server.pid)
        } else {
            format!(
                "Beenden fehlgeschlagen · {} PID {}",
                server.label(),
                server.pid
            )
        });
    }
}

fn pwa_snapshot(app: &App) -> serde_json::Value {
    let memory = app.system.memory.as_ref().map(|memory| {
        json!({
            "available_bytes": memory.available_bytes,
            "total_bytes": memory.total_bytes,
            "used_bytes": memory.used_bytes,
            "swap_total_bytes": memory.swap_total_bytes,
            "swap_used_bytes": memory.swap_used_bytes,
            "psi_some_avg10_percent": memory.psi_some_avg10_percent,
            "psi_full_avg10_percent": memory.psi_full_avg10_percent,
        })
    });
    let disk = app
        .system
        .disks
        .as_ref()
        .and_then(|disks| disks.iter().find(|disk| disk.mount == "/"))
        .map(|disk| {
            json!({
                "available_bytes": disk.available,
                "total_bytes": disk.total,
                "used_percent": disk.used_percent(),
            })
        });
    let agents = match app.agents.view() {
        AgentStatusView::Loading => ("loading", Vec::new()),
        AgentStatusView::Unavailable => ("unavailable", Vec::new()),
        AgentStatusView::Ready(snapshot) => (
            "ready",
            snapshot
                .agents
                .iter()
                .map(|agent| {
                    json!({
                        "name": agent.label,
                        "state": agent.state.label(),
                        "provider": agent.provider.tag(),
                    })
                })
                .collect(),
        ),
        AgentStatusView::Stale(_) => ("stale", Vec::new()),
    };
    let servers = if !app.servers.is_available() {
        "unavailable"
    } else if app.servers.is_stale() {
        "stale"
    } else {
        "ready"
    };

    json!({
        "cpu": {
            "load_percent": app.system.cpu.load_percent,
            "history": app.cpu_history.iter().copied().collect::<Vec<_>>(),
        },
        "memory": memory,
        "memory_history": app.memory_used_history.iter().copied().collect::<Vec<_>>(),
        "gpu": {
            "load_percent": app.gpu.load_percent,
            "vram_free_bytes": app.gpu.vram_free_bytes,
            "vram_total_bytes": app.gpu.vram_total_bytes,
            "history": app.gpu_history.iter().copied().collect::<Vec<_>>(),
        },
        "disk": disk,
        "network": {
            "download_bytes_per_second": app.system.network.download_bytes_per_second,
            "upload_bytes_per_second": app.system.network.upload_bytes_per_second,
            "download_history": app.download_history.iter().copied().collect::<Vec<_>>(),
            "upload_history": app.upload_history.iter().copied().collect::<Vec<_>>(),
        },
        "codex": app.codex.line(),
        "claude": app.claude.line(),
        "servers": {
            "status": servers,
            "items": app.servers.servers().iter().map(|server| json!({
                "name": server.label(),
                "pid": server.pid,
                "port": server.port,
            })).collect::<Vec<_>>(),
        },
        "agents": {"source": "claude+codex", "status": agents.0, "items": agents.1},
    })
}

fn push_memory_history(
    history: &mut VecDeque<Option<f64>>,
    memory: Option<&MemoryMetrics>,
    value: impl FnOnce(&MemoryMetrics) -> u64,
) {
    let percent = memory
        .filter(|memory| memory.total_bytes > 0)
        .map(|memory| value(memory) as f64 / memory.total_bytes as f64 * 100.0);
    push_history_point(history, percent);
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::cursor::Hide,
            Clear(ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), crossterm::cursor::Show, LeaveAlternateScreen);
    }
}

pub(super) fn run_tui(enable_pwa: bool) -> AppResult<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new();
    app.sample(true);
    app.codex.refresh();
    app.claude.refresh();
    app.refresh_servers();
    let pwa = enable_pwa
        .then(|| super::pwa::PwaServer::bind(pwa_snapshot(&app)).ok())
        .flatten();
    let mut next_sample = Instant::now() + SAMPLE_INTERVAL;
    let mut clock = local_clock();
    let mut next_clock_refresh = next_minute(Instant::now());
    let mut needs_redraw = true;

    loop {
        if app.codex.poll() {
            needs_redraw = true;
        }
        if app.claude.poll() {
            needs_redraw = true;
        }
        app.codex.refresh_if_due();
        app.claude.refresh_if_due();
        let servers_changed = app.refresh_servers_if_due();
        let agents_changed = app.agents.refresh_if_due();
        if servers_changed || agents_changed {
            needs_redraw = true;
        }
        let now = Instant::now();
        if now >= next_clock_refresh {
            clock = local_clock();
            next_clock_refresh = next_minute(now);
            needs_redraw = true;
        }
        if needs_redraw {
            if let Some(pwa) = &pwa {
                pwa.publish(pwa_snapshot(&app));
            }
            terminal.draw(|frame| view::render(frame, &app.view(&clock)))?;
            needs_redraw = false;
        }

        let now = Instant::now();
        let wait = next_sample
            .saturating_duration_since(now)
            .min(EVENT_POLL_INTERVAL);
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(key) => {
                    if app.pending_stop.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => app.confirm_server_stop(),
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.pending_stop = None;
                                app.server_notice = Some("Beenden abgebrochen".into());
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break;
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break;
                            }
                            KeyCode::Char('r') => {
                                app.sample(true);
                                app.codex.refresh();
                                app.claude.refresh();
                                app.refresh_servers();
                                app.agents.refresh();
                                next_sample = Instant::now() + SAMPLE_INTERVAL;
                            }
                            KeyCode::Up => app.move_server_selection(-1),
                            KeyCode::Down => app.move_server_selection(1),
                            KeyCode::Char('x') | KeyCode::Char('X') => app.request_server_stop(),
                            _ => {}
                        }
                    }
                    needs_redraw = true;
                }
                Event::Resize(_, _) => {
                    terminal.clear()?;
                    needs_redraw = true;
                }
                _ => {}
            }
        }

        if Instant::now() >= next_sample {
            app.sample(false);
            next_sample = Instant::now() + SAMPLE_INTERVAL;
            needs_redraw = true;
        }
    }
    Ok(())
}

fn local_clock() -> String {
    time::OffsetDateTime::now_local()
        .map(|now| format!("{:02}:{:02}", now.hour(), now.minute()))
        .unwrap_or_else(|_| "--:--".to_owned())
}

fn next_minute(now: Instant) -> Instant {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now + Duration::from_secs(60 - elapsed % 60)
}

impl App {
    fn view<'a>(&'a self, clock: &'a str) -> View<'a> {
        View {
            system: &self.system,
            gpu: self.gpu,
            cpu_history: &self.cpu_history,
            gpu_history: &self.gpu_history,
            download_history: &self.download_history,
            upload_history: &self.upload_history,
            memory_history: &self.memory_used_history,
            codex: self.codex.view(),
            claude: self.claude.view(),
            servers: self.servers.servers(),
            servers_available: self.servers.is_available(),
            servers_stale: self.servers.is_stale(),
            selected_server: self.selected_server,
            pending_stop: self.pending_stop.as_ref(),
            server_notice: self.server_notice.as_deref(),
            agents: self.agents.view(),
            clock,
        }
    }
}
