//! Luna's terminal dashboard: a black canvas, one cool accent, hairline
//! sections and measured readings only. A reading that was not measured is
//! printed as an em dash, never estimated.

use std::collections::VecDeque;
use std::time::SystemTime;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use super::agents::{AgentEntry, AgentState, AgentStatusView};
use super::services::LocalServer;
use super::system_stats::{MemoryMetrics, SystemMetrics};
use super::weekly_usage::UsageView;
use super::GpuMetrics;

/// OLED canvas: pure black, so unlit pixels stay off.
const CANVAS: Color = Color::Rgb(0, 0, 0);
const INK: Color = Color::Rgb(242, 242, 247);
const SECONDARY: Color = Color::Rgb(160, 160, 168);
const TERTIARY: Color = Color::Rgb(124, 124, 132);
const HAIRLINE: Color = Color::Rgb(46, 46, 52);
/// Card outline: barely lifted off the black canvas.
const BORDER: Color = Color::Rgb(58, 58, 66);
/// Key chip fill in the footer.
const CHIP: Color = Color::Rgb(34, 34, 40);
const ACCENT: Color = Color::Rgb(100, 210, 255);
const GREEN: Color = Color::Rgb(48, 209, 88);
const AMBER: Color = Color::Rgb(255, 214, 10);
const RED: Color = Color::Rgb(255, 69, 58);

const WIDE_MIN_WIDTH: u16 = 100;
const WIDE_MIN_HEIGHT: u16 = 30;
const COMPACT_MIN_WIDTH: u16 = 72;
const COMPACT_MIN_HEIGHT: u16 = 22;

const SPARK: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];
const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// Everything the dashboard draws, borrowed from the application state.
pub(super) struct View<'a> {
    pub(super) system: &'a SystemMetrics,
    pub(super) gpu: GpuMetrics,
    pub(super) cpu_history: &'a VecDeque<Option<f64>>,
    pub(super) gpu_history: &'a VecDeque<Option<f64>>,
    pub(super) download_history: &'a VecDeque<Option<f64>>,
    pub(super) upload_history: &'a VecDeque<Option<f64>>,
    pub(super) memory_history: &'a VecDeque<Option<f64>>,
    pub(super) codex: UsageView,
    pub(super) claude: UsageView,
    pub(super) servers: &'a [LocalServer],
    pub(super) servers_available: bool,
    pub(super) servers_stale: bool,
    pub(super) selected_server: usize,
    pub(super) pending_stop: Option<&'a LocalServer>,
    pub(super) server_notice: Option<&'a str>,
    pub(super) agents: &'a AgentStatusView,
    pub(super) clock: &'a str,
}

/// One line of a panel, or a graph that takes the space the lines leave over.
enum Row {
    Line(Line<'static>),
    Gap,
    Graph {
        values: Vec<Option<f64>>,
        scale: f64,
        color: Color,
    },
}

struct Panel {
    title: &'static str,
    meta: String,
    rows: Vec<Row>,
}

impl Panel {
    /// Height the panel needs without any graph: title, rule and its lines.
    fn preferred_height(&self) -> u16 {
        let lines = self
            .rows
            .iter()
            .filter(|row| !matches!(row, Row::Graph { .. }))
            .count();
        2 + u16::try_from(lines).unwrap_or(u16::MAX - 2)
    }
}

pub(super) fn render(frame: &mut Frame<'_>, view: &View<'_>) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(CANVAS)), area);
    if area.width < COMPACT_MIN_WIDTH || area.height < COMPACT_MIN_HEIGHT {
        let message = if area.width < COMPACT_MIN_WIDTH {
            "Terminal zu schmal · mindestens 72 Spalten"
        } else {
            "Terminal zu niedrig · mindestens 22 Zeilen"
        };
        frame.render_widget(
            Paragraph::new(Line::from(span(message, TERTIARY))).alignment(Alignment::Center),
            area,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    render_header(frame, rows[0]);
    if area.width >= WIDE_MIN_WIDTH && area.height >= WIDE_MIN_HEIGHT {
        render_wide(frame, rows[1], view);
    } else {
        render_compact(frame, rows[1], view);
    }
    render_footer(frame, rows[2], view.clock);
}

fn render_header(frame: &mut Frame<'_>, area: Rect) {
    let line = Line::from(vec![
        span(" ● ", ACCENT),
        strong("LUNA", INK),
        span("   Systemmonitor", TERTIARY),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, clock: &str) {
    let clock_width = u16::try_from(clock.chars().count() + 1).unwrap_or(6);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(clock_width)])
        .split(area);
    let keys = [
        ("q", "trennen"),
        ("r", "aktualisieren"),
        ("↑↓", "wählen"),
        ("x", "beenden"),
    ];
    let mut spans = Vec::new();
    for (index, (key, action)) in keys.into_iter().enumerate() {
        if index > 0 {
            spans.push(span("   ", TERTIARY));
        }
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(INK)
                .bg(CHIP)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(span(format!(" {action}"), TERTIARY));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), columns[0]);
    frame.render_widget(
        Paragraph::new(Line::from(strong(clock, SECONDARY))).alignment(Alignment::Right),
        columns[1],
    );
}

fn render_wide(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let servers = server_panel(view, usize::from(area.width) / 3);
    let agents_rows = agent_rows(view.agents, 24).len();
    let content = servers
        .preferred_height()
        .max(u16::try_from(agents_rows).unwrap_or(0) + 2);
    let bottom_height = content.max(9).min(area.height * 2 / 5);
    let bands = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(bottom_height)])
        .split(area);

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(38),
            Constraint::Percentage(31),
            Constraint::Percentage(31),
        ])
        .split(bands[0]);
    let middle_disks = disk_panel(view, inset(top[1]).width as usize);
    let middle = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(middle_disks.preferred_height()),
        ])
        .split(top[1]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(top[2]);

    draw_panel(
        frame,
        inset(top[0]),
        cpu_panel(view, inset(top[0]).width as usize, false),
    );
    draw_panel(
        frame,
        inset(middle[0]),
        memory_panel(view, inset(middle[0]).width as usize, false),
    );
    draw_panel(frame, inset(middle[1]), middle_disks);
    draw_panel(
        frame,
        inset(right[0]),
        gpu_panel(view, inset(right[0]).width as usize),
    );
    draw_panel(frame, inset(right[1]), network_panel(view));

    // The limits panel only exists when Claude Code or Codex is installed.
    let with_usage = usage_panel(view, 1, false).is_some();
    let shares: &[u16] = if with_usage { &[34, 33, 33] } else { &[50, 50] };
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(shares.iter().map(|share| Constraint::Percentage(*share)))
        .split(bands[1]);
    let mut slots = bottom.iter().map(|slot| inset(*slot));
    if let Some(usage) = usage_panel(view, inset(bottom[0]).width as usize, false) {
        let slot = slots.next().unwrap_or_default();
        draw_panel(frame, slot, usage);
    }
    if let Some(slot) = slots.next() {
        draw_panel(frame, slot, server_panel(view, usize::from(slot.width)));
    }
    if let Some(slot) = slots.next() {
        draw_panel(
            frame,
            slot,
            agent_panel(view.agents, usize::from(slot.width)),
        );
    }
}

fn render_compact(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let width = usize::from(area.width).saturating_sub(4);
    let mut panels: Vec<Panel> = usage_panel(view, width, true).into_iter().collect();
    // The CPU panel owns the only graph that fits a stacked layout.
    let cpu_index = panels.len();
    panels.extend([
        cpu_panel(view, width, true),
        memory_panel(view, width, true),
        gpu_panel(view, width),
        network_panel(view),
        disk_panel(view, width),
        agent_panel(view.agents, width),
        server_panel(view, width),
    ]);

    let mut heights: Vec<u16> = Vec::with_capacity(panels.len());
    let mut remaining = area.height;
    for panel in &panels {
        let take = panel.preferred_height().min(remaining);
        heights.push(take);
        remaining -= take;
    }
    if remaining > 0 {
        heights[cpu_index] += remaining;
    }

    let constraints: Vec<Constraint> = heights.iter().map(|h| Constraint::Length(*h)).collect();
    let slots = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    for (index, panel) in panels.drain(..).enumerate() {
        draw_panel(frame, inset(slots[index]), panel);
    }
}

fn cpu_panel(view: &View<'_>, width: usize, compact: bool) -> Panel {
    let cpu = &view.system.cpu;
    let load = cpu.load_percent.map(|value| value.clamp(0.0, 100.0));
    let free = load.map(|value| 100.0 - value);
    let mut rows = vec![
        Row::Line(Line::from(vec![
            strong(percent(load, 0), INK),
            span(" belegt", SECONDARY),
            span("   ", TERTIARY),
            span(percent(free, 0), SECONDARY),
            span(" frei", TERTIARY),
        ])),
        Row::Line(Line::from(bar(
            load.map(|v| v / 100.0),
            width,
            load_color(load),
        ))),
        Row::Line(Line::from(span(cpu_model(view, width), TERTIARY))),
        Row::Line(Line::from(span(cpu_summary(view), SECONDARY))),
        Row::Gap,
    ];
    if !compact {
        rows.extend(
            core_lines(&cpu.per_core_percent, width)
                .into_iter()
                .map(Row::Line),
        );
        rows.push(Row::Gap);
    }
    rows.push(Row::Graph {
        values: view.cpu_history.iter().copied().collect(),
        scale: 100.0,
        color: ACCENT,
    });
    Panel {
        title: "CPU",
        meta: String::new(),
        rows,
    }
}

fn cpu_model(view: &View<'_>, width: usize) -> String {
    view.system
        .cpu
        .model
        .as_deref()
        .map(|model| model.chars().take(width.max(1)).collect())
        .unwrap_or_else(|| "CPU".into())
}

fn cpu_summary(view: &View<'_>) -> String {
    let cpu = &view.system.cpu;
    let cores = cpu.per_core_percent.len();
    let frequency = cpu
        .frequency_mhz
        .map(|mhz| format!("{:.2} GHz", mhz as f64 / 1000.0))
        .unwrap_or_else(|| "— GHz".into());
    let temperature = cpu
        .temperature_c
        .map(|celsius| format!("{celsius:.0} °C"))
        .unwrap_or_else(|| "— °C".into());
    let mut parts = vec![format!("{cores} Kerne"), frequency, temperature];
    if let Some(watts) = cpu.power_watts {
        parts.push(format!("{watts:.0} W"));
    }
    parts.join(" · ")
}

/// Per-core load as a grid of small bars. The column count follows the width,
/// so a 16-core machine stays readable from a narrow pane to a full screen.
fn core_lines(per_core: &[Option<f64>], width: usize) -> Vec<Line<'static>> {
    if per_core.is_empty() {
        return vec![Line::from(span("Kerne nicht gemessen", TERTIARY))];
    }
    let columns = match width {
        56.. => 4,
        40..=55 => 3,
        26..=39 => 2,
        _ => 1,
    };
    let cell = width / columns;
    let bar_width = cell.saturating_sub(9);
    per_core
        .chunks(columns)
        .enumerate()
        .map(|(row, chunk)| {
            let mut spans = Vec::new();
            for (column, value) in chunk.iter().enumerate() {
                let index = row * columns + column;
                spans.push(span(format!("{index:02} "), TERTIARY));
                let fraction = value.map(|load| load.clamp(0.0, 100.0) / 100.0);
                if bar_width > 0 {
                    spans.extend(bar(
                        fraction,
                        bar_width,
                        load_color(value.map(|l| l.clamp(0.0, 100.0))),
                    ));
                    spans.push(span(" ", TERTIARY));
                }
                spans.push(span(format!("{:>4}", percent(*value, 0)), SECONDARY));
                spans.push(span(
                    " ".repeat(cell.saturating_sub(bar_width + 8)),
                    TERTIARY,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn memory_panel(view: &View<'_>, width: usize, compact: bool) -> Panel {
    let rows = match &view.system.memory {
        None => vec![Row::Line(Line::from(span("nicht gemessen", TERTIARY)))],
        Some(memory) => memory_rows(memory, view, width, compact),
    };
    Panel {
        title: "RAM",
        meta: String::new(),
        rows,
    }
}

fn memory_rows(memory: &MemoryMetrics, view: &View<'_>, width: usize, compact: bool) -> Vec<Row> {
    let total = memory.total_bytes as f64;
    let used_fraction = (total > 0.0).then(|| memory.used_bytes as f64 / total);
    let used_percent = used_fraction.map(|value| value * 100.0);
    let swap = match (memory.swap_total_bytes, memory.swap_used_bytes) {
        (Some(total), Some(used)) if total > 0 => span(
            format!(
                "swap {} / {} · {}",
                format_bytes(used),
                format_bytes(total),
                percent(Some(used as f64 / total as f64 * 100.0), 0)
            ),
            SECONDARY,
        ),
        (Some(0), _) => span("swap aus", TERTIARY),
        _ => span("swap nicht gemessen", TERTIARY),
    };
    let psi = vec![
        span("PSI ", TERTIARY),
        span("some ", TERTIARY),
        span(percent(memory.psi_some_avg10_percent, 2), SECONDARY),
        span("  full ", TERTIARY),
        span(percent(memory.psi_full_avg10_percent, 2), SECONDARY),
    ];
    let mut rows = vec![
        Row::Line(Line::from(vec![
            strong(format_bytes(memory.used_bytes), INK),
            span(" belegt", SECONDARY),
            span("   von ", TERTIARY),
            span(format_bytes(memory.total_bytes), SECONDARY),
        ])),
        Row::Line(Line::from(bar(
            used_fraction,
            width,
            load_color(used_percent),
        ))),
    ];
    if compact {
        rows.push(Row::Line(Line::from(span(
            format!(
                "frei {} · cache {} · unbelegt {}",
                format_bytes(memory.available_bytes),
                format_bytes(memory.cached_bytes),
                format_bytes(memory.free_bytes)
            ),
            SECONDARY,
        ))));
        let mut swap_and_psi = vec![swap.clone(), span("  ·  ", TERTIARY)];
        swap_and_psi.extend(psi);
        rows.push(Row::Line(Line::from(swap_and_psi)));
    } else {
        rows.push(Row::Line(Line::from(span(
            format!("frei {}", format_bytes(memory.available_bytes)),
            SECONDARY,
        ))));
        rows.push(Row::Line(Line::from(span(
            format!("cache {}", format_bytes(memory.cached_bytes)),
            SECONDARY,
        ))));
        rows.push(Row::Line(Line::from(span(
            format!("unbelegt {}", format_bytes(memory.free_bytes)),
            SECONDARY,
        ))));
        rows.push(Row::Line(Line::from(swap)));
        rows.push(Row::Line(Line::from(psi)));
        rows.push(Row::Gap);
    }
    rows.push(Row::Graph {
        values: view.memory_history.iter().copied().collect(),
        scale: 100.0,
        color: SECONDARY,
    });
    rows
}

fn disk_panel(view: &View<'_>, width: usize) -> Panel {
    let (meta, rows) = match &view.system.disks {
        None => (
            String::new(),
            vec![Row::Line(Line::from(span("nicht gemessen", TERTIARY)))],
        ),
        Some(disks) if disks.is_empty() => (
            String::new(),
            vec![Row::Line(Line::from(span("keine Volumes", TERTIARY)))],
        ),
        Some(disks) => {
            let rows = disks
                .iter()
                .map(|disk| {
                    let total = disk.total as f64;
                    let used = (total > 0.0).then(|| disk.used as f64 / total);
                    let name = mount_label(&disk.mount);
                    let bar_width = width.saturating_sub(10 + 1 + 12 + 1);
                    let mut spans = vec![span(format!("{name:<10} "), SECONDARY)];
                    spans.extend(bar(used, bar_width, load_color(used.map(|v| v * 100.0))));
                    spans.push(span(format!(" {:>12}", format_bytes(disk.used)), INK));
                    Row::Line(Line::from(spans))
                })
                .collect();
            (format!("{} Volumes · belegt", disks.len()), rows)
        }
    };
    Panel {
        title: "DATENTRÄGER",
        meta,
        rows,
    }
}

fn gpu_panel(view: &View<'_>, width: usize) -> Panel {
    let load = view.gpu.load_percent.map(|value| value.clamp(0.0, 100.0));
    let vram_used = match (view.gpu.vram_free_bytes, view.gpu.vram_total_bytes) {
        (Some(free), Some(total)) if total > 0 => Some(total.saturating_sub(free)),
        _ => None,
    };
    let vram_fraction = match (vram_used, view.gpu.vram_total_bytes) {
        (Some(used), Some(total)) if total > 0 => Some(used as f64 / total as f64),
        _ => None,
    };
    let vram_total = view
        .gpu
        .vram_total_bytes
        .map(format_bytes)
        .unwrap_or_else(|| "—".into());
    let vram_used_text = vram_used.map(format_bytes).unwrap_or_else(|| "—".into());
    Panel {
        title: "GPU",
        meta: String::new(),
        rows: vec![
            Row::Line(Line::from(vec![
                strong(percent(load, 0), INK),
                span("   Auslastung", TERTIARY),
            ])),
            Row::Line(Line::from(bar(
                load.map(|v| v / 100.0),
                width,
                load_color(load),
            ))),
            Row::Line(Line::from(vec![
                span("VRAM ", TERTIARY),
                strong(vram_used_text, INK),
                span(" belegt", SECONDARY),
                span(" / ", TERTIARY),
                span(vram_total, SECONDARY),
            ])),
            Row::Line(Line::from(bar(
                vram_fraction,
                width,
                load_color(vram_fraction.map(|v| v * 100.0)),
            ))),
            Row::Gap,
            Row::Graph {
                values: view.gpu_history.iter().copied().collect(),
                scale: 100.0,
                color: ACCENT,
            },
        ],
    }
}

fn network_panel(view: &View<'_>) -> Panel {
    let network = &view.system.network;
    let meta = network.interface.clone().unwrap_or_else(|| "—".into());
    let download_history: Vec<Option<f64>> = view.download_history.iter().copied().collect();
    let upload_history: Vec<Option<f64>> = view.upload_history.iter().copied().collect();
    let download_scale = window_scale(&download_history);
    let upload_scale = window_scale(&upload_history);
    Panel {
        title: "NETZWERK",
        meta,
        rows: vec![
            Row::Line(Line::from(vec![
                span("↓ ", ACCENT),
                strong(rate(network.download_bytes_per_second), INK),
                span("   Spitze ", TERTIARY),
                span(
                    rate(Some(network.download_peak_bytes_per_second)),
                    SECONDARY,
                ),
            ])),
            Row::Line(Line::from(vec![
                span("↑ ", SECONDARY),
                strong(rate(network.upload_bytes_per_second), INK),
                span("   Spitze ", TERTIARY),
                span(rate(Some(network.upload_peak_bytes_per_second)), SECONDARY),
            ])),
            Row::Line(Line::from(vec![
                span("Sitzung ", TERTIARY),
                span(
                    format!(
                        "↓ {}   ↑ {}",
                        format_bytes(network.download_session_bytes),
                        format_bytes(network.upload_session_bytes)
                    ),
                    SECONDARY,
                ),
            ])),
            Row::Graph {
                values: download_history,
                scale: download_scale,
                color: ACCENT,
            },
            Row::Graph {
                values: upload_history,
                scale: upload_scale,
                color: SECONDARY,
            },
        ],
    }
}

/// The weekly limits of the agents that exist on this machine. `None` when
/// there are none, so the panel is left out entirely.
fn usage_panel(view: &View<'_>, width: usize, compact: bool) -> Option<Panel> {
    let present: Vec<(&'static str, UsageView)> = [("CODEX", view.codex), ("CLAUDE", view.claude)]
        .into_iter()
        .filter(|(_, usage)| *usage != UsageView::Absent)
        .collect();
    if present.is_empty() {
        return None;
    }
    let mut rows = Vec::new();
    for (index, (label, usage)) in present.into_iter().enumerate() {
        if compact {
            rows.push(usage_summary(label, usage));
        } else {
            if index > 0 {
                rows.push(Row::Gap);
            }
            rows.extend(usage_rows(label, usage, width));
        }
    }
    Some(Panel {
        title: "WOCHENLIMIT",
        meta: String::new(),
        rows,
    })
}

/// One line per agent for the stacked layout: value, used share and reset.
fn usage_summary(label: &'static str, usage: UsageView) -> Row {
    let mut spans = vec![strong(label, TERTIARY), span("   ", TERTIARY)];
    match usage {
        UsageView::Quota {
            used_percent,
            resets_at,
            stale,
        } => {
            let free = (100.0 - used_percent).clamp(0.0, 100.0);
            spans.push(strong(
                format!("{used_percent:.1} %"),
                if stale { SECONDARY } else { INK },
            ));
            spans.push(span(" genutzt", SECONDARY));
            spans.push(span(format!(" · {free:.1} % frei"), SECONDARY));
            if let Some(resets_at) = resets_at {
                let seconds = resets_at
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs();
                spans.push(span(
                    format!(" · bis {}", super::format_reset(seconds, false)),
                    TERTIARY,
                ));
            }
            if stale {
                spans.push(span(" · veraltet", AMBER));
            }
        }
        UsageView::Loading => spans.push(span("Wochenlimit wird geladen", TERTIARY)),
        UsageView::NoWindow => spans.push(span("kein Wochenfenster verfügbar", TERTIARY)),
        UsageView::Throttled => {
            spans.push(span("gedrosselt · neuer Versuch automatisch", TERTIARY))
        }
        UsageView::Absent => {}
        UsageView::Unavailable => spans.push(span("nicht erreichbar", TERTIARY)),
    }
    Row::Line(Line::from(spans))
}

fn usage_rows(label: &'static str, usage: UsageView, width: usize) -> Vec<Row> {
    let header = |value: String, value_color: Color| {
        let unit = if value == "—" { "" } else { " genutzt" };
        Row::Line(Line::from(vec![
            strong(label, TERTIARY),
            span("   ", TERTIARY),
            strong(value, value_color),
            span(unit, SECONDARY),
        ]))
    };
    match usage {
        UsageView::Quota {
            used_percent,
            resets_at,
            stale,
        } => {
            let free = (100.0 - used_percent).clamp(0.0, 100.0);
            let mut detail = vec![span(format!("{free:.1} % frei"), SECONDARY)];
            if let Some(resets_at) = resets_at {
                let seconds = resets_at
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs();
                detail.push(span(
                    format!("   bis {}", super::format_reset(seconds, false)),
                    TERTIARY,
                ));
            }
            let mut head = vec![
                strong(label, TERTIARY),
                span("   ", TERTIARY),
                strong(
                    format!("{used_percent:.1} %"),
                    if stale { SECONDARY } else { INK },
                ),
                span(" genutzt", SECONDARY),
            ];
            if stale {
                head.push(span("   veraltet", AMBER));
            }
            vec![
                Row::Line(Line::from(head)),
                Row::Line(Line::from(bar(
                    Some(used_percent / 100.0),
                    width,
                    load_color(Some(used_percent)),
                ))),
                Row::Line(Line::from(detail)),
            ]
        }
        UsageView::Loading => vec![
            header("—".into(), TERTIARY),
            Row::Line(Line::from(bar(None, width, TERTIARY))),
            Row::Line(Line::from(span("Wochenlimit wird geladen", TERTIARY))),
        ],
        UsageView::NoWindow => vec![
            header("—".into(), TERTIARY),
            Row::Line(Line::from(bar(None, width, TERTIARY))),
            Row::Line(Line::from(span("kein Wochenfenster verfügbar", TERTIARY))),
        ],
        UsageView::Throttled => vec![
            header("—".into(), TERTIARY),
            Row::Line(Line::from(bar(None, width, TERTIARY))),
            Row::Line(Line::from(span(
                "gedrosselt · neuer Versuch automatisch",
                TERTIARY,
            ))),
        ],
        UsageView::Absent => Vec::new(),
        UsageView::Unavailable => vec![
            header("—".into(), TERTIARY),
            Row::Line(Line::from(bar(None, width, TERTIARY))),
            Row::Line(Line::from(span("nicht erreichbar", TERTIARY))),
        ],
    }
}

fn server_panel(view: &View<'_>, width: usize) -> Panel {
    let mut rows = Vec::new();
    let meta = if !view.servers_available {
        rows.push(Row::Line(Line::from(span(
            "Liste nicht verfügbar",
            TERTIARY,
        ))));
        String::new()
    } else if view.servers.is_empty() {
        rows.push(Row::Line(Line::from(span(
            "keine lokalen Server",
            TERTIARY,
        ))));
        String::new()
    } else {
        for (index, server) in view.servers.iter().enumerate() {
            let selected = index == view.selected_server;
            let name_style = if selected { INK } else { SECONDARY };
            let port = server
                .port
                .map(|port| format!(":{port}"))
                .unwrap_or_else(|| "—".into());
            let name_width = width.saturating_sub(2 + 8 + 6 + 6);
            let name: String = server.label().chars().take(name_width.max(4)).collect();
            rows.push(Row::Line(Line::from(vec![
                span(if selected { "› " } else { "  " }, ACCENT),
                if selected {
                    strong(format!("{name:<name_width$}"), name_style)
                } else {
                    span(format!("{name:<name_width$}"), name_style)
                },
                span(format!(" {port:<6}"), TERTIARY),
                span(format!(" PID {}", server.pid), TERTIARY),
            ])));
        }
        view.servers.len().to_string()
    };
    if view.servers_stale {
        rows.push(Row::Line(Line::from(span("Liste veraltet", AMBER))));
    }
    rows.push(Row::Gap);
    if let Some(pending) = view.pending_stop {
        rows.push(Row::Line(Line::from(span(
            format!(
                "Beenden {} PID {}?  y bestätigen · n abbrechen",
                pending.label(),
                pending.pid
            ),
            AMBER,
        ))));
    } else if let Some(notice) = view.server_notice {
        rows.push(Row::Line(Line::from(span(notice.to_owned(), SECONDARY))));
    }
    Panel {
        title: "SERVER",
        meta,
        rows,
    }
}

fn agent_panel(agents: &AgentStatusView, width: usize) -> Panel {
    let (meta, rows) = match agents {
        AgentStatusView::Ready(snapshot) => {
            let meta = snapshot.agents.len().to_string();
            (meta, agent_rows_for(&snapshot.agents, width))
        }
        AgentStatusView::Stale(snapshot) => {
            let mut rows = agent_rows_for(&snapshot.agents, width);
            rows.push(Row::Line(Line::from(span("veraltet", AMBER))));
            (format!("{} · veraltet", snapshot.agents.len()), rows)
        }
        AgentStatusView::Loading => (
            String::new(),
            vec![Row::Line(Line::from(span("lädt", TERTIARY)))],
        ),
        AgentStatusView::Unavailable => (
            String::new(),
            vec![Row::Line(Line::from(span(
                "Claude/Codex nicht erreichbar",
                TERTIARY,
            )))],
        ),
    };
    Panel {
        title: "AGENTEN",
        meta,
        rows,
    }
}

fn agent_rows(agents: &AgentStatusView, width: usize) -> Vec<Row> {
    match agents {
        AgentStatusView::Ready(snapshot) | AgentStatusView::Stale(snapshot) => {
            agent_rows_for(&snapshot.agents, width)
        }
        AgentStatusView::Loading | AgentStatusView::Unavailable => Vec::new(),
    }
}

fn agent_rows_for(entries: &[AgentEntry], width: usize) -> Vec<Row> {
    if entries.is_empty() {
        return vec![Row::Line(Line::from(span("keine Agenten", TERTIARY)))];
    }
    entries
        .iter()
        .map(|entry| {
            let (dot, color) = agent_mark(entry.state);
            let word = agent_word(entry.state);
            // Every status word is padded to the widest, so columns line up.
            let word_width = 7;
            // Which backend an agent runs on, shown only when there is room.
            let tag_width = if width >= 30 { 7 } else { 0 };
            let name_width = width.saturating_sub(2 + tag_width + word_width + 2).max(4);
            let name: String = entry.label.chars().take(name_width).collect();
            let mut spans = vec![
                span(format!("{dot} "), color),
                span(format!("{name:<name_width$}"), INK),
            ];
            if tag_width > 0 {
                spans.push(span(format!(" {:<6}", entry.provider.tag()), TERTIARY));
            }
            if let Some(word) = word {
                let word_color = if matches!(entry.state, AgentState::Idle) {
                    SECONDARY
                } else {
                    color
                };
                spans.push(span(format!("  {word:>word_width$}"), word_color));
            }
            Row::Line(Line::from(spans))
        })
        .collect()
}

/// The exact status words Luna shows. Unknown states get no word at all.
fn agent_word(state: AgentState) -> Option<&'static str> {
    match state {
        AgentState::Working => Some("WORKING"),
        AgentState::Blocked => Some("BLOCKED"),
        AgentState::Done => Some("DONE"),
        AgentState::Idle => Some("IDLE"),
        AgentState::Unknown => None,
    }
}

fn agent_mark(state: AgentState) -> (&'static str, Color) {
    match state {
        AgentState::Working => ("●", INK),
        AgentState::Blocked => ("●", RED),
        AgentState::Done => ("●", GREEN),
        AgentState::Idle => ("○", SECONDARY),
        AgentState::Unknown => (" ", TERTIARY),
    }
}

fn draw_panel(frame: &mut Frame<'_>, area: Rect, panel: Panel) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut title = vec![span(" ", BORDER), strong(panel.title, SECONDARY)];
    if !panel.meta.is_empty() {
        title.push(span(format!("  {}", panel.meta), TERTIARY));
    }
    title.push(span(" ", BORDER));
    // `area` is the card's content; the rounded outline sits two columns out.
    frame.render_widget(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER).bg(CANVAS))
            .title(Line::from(title)),
        Rect {
            x: area.x - 2,
            width: area.width + 4,
            ..area
        },
    );
    if area.height < 3 {
        return;
    }

    let body_end = area.y + area.height - 1;
    let fixed = panel
        .rows
        .iter()
        .filter(|row| !matches!(row, Row::Graph { .. }))
        .count() as u16;
    let graphs = panel
        .rows
        .iter()
        .filter(|row| matches!(row, Row::Graph { .. }))
        .count() as u16;
    let spare = (area.height - 2).saturating_sub(fixed);
    let per_graph = if graphs == 0 { 0 } else { spare / graphs };

    let mut y = area.y + 1;
    let mut graph_index = 0u16;
    for row in panel.rows {
        if y >= body_end {
            break;
        }
        match row {
            Row::Line(line) => {
                frame.render_widget(Paragraph::new(line), row_at(area, y));
                y += 1;
            }
            Row::Gap => y += 1,
            Row::Graph {
                values,
                scale,
                color,
            } => {
                graph_index += 1;
                let mut height = if graph_index == graphs {
                    spare - per_graph * (graphs - 1)
                } else {
                    per_graph
                };
                height = height.min(body_end - y);
                if height == 0 {
                    continue;
                }
                let lines = graph_lines(
                    &values,
                    usize::from(area.width),
                    usize::from(height),
                    scale,
                    color,
                );
                frame.render_widget(
                    Paragraph::new(lines),
                    Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height,
                    },
                );
                y += height;
            }
        }
    }
}

/// Multi-row graph in eighth-block steps. The newest sample sits at the right
/// edge; a missing sample is a dim dot on the baseline, not an invented value.
fn graph_lines(
    values: &[Option<f64>],
    width: usize,
    height: usize,
    scale: f64,
    color: Color,
) -> Vec<Line<'static>> {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let take = width.min(values.len());
    let mut samples = vec![None; width - take];
    samples.extend_from_slice(&values[values.len() - take..]);
    let total_eighths = (height * 8) as f64;
    (0..height)
        .map(|row| {
            let base = (height - 1 - row) * 8;
            let tint = ramp(color, height - 1 - row, height);
            let spans: Vec<Span<'static>> = samples
                .iter()
                .map(|sample| match sample {
                    None if row == height - 1 => span("·", HAIRLINE),
                    None => span(" ", CANVAS),
                    Some(value) => {
                        let level =
                            (value.clamp(0.0, scale) / scale * total_eighths).round() as usize;
                        match level.saturating_sub(base).min(8) {
                            0 => span(" ", CANVAS),
                            8 => span("█", tint),
                            cell => span(SPARK[cell - 1].to_string(), tint),
                        }
                    }
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// A horizontal bar in eighth-cell steps. The track is a dim dot run, and an
/// unknown fraction shows only the track.
fn bar(fraction: Option<f64>, width: usize, color: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let Some(fraction) = fraction else {
        return vec![span("·".repeat(width), HAIRLINE)];
    };
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let rest = eighths % 8;
    let mut spans = Vec::new();
    for cell in 0..full {
        spans.push(span("█", ramp(color, cell, width)));
    }
    let mut used = full;
    if rest > 0 {
        spans.push(span(PARTIAL[rest], ramp(color, full, width)));
        used += 1;
    }
    if used < width {
        spans.push(span("·".repeat(width - used), HAIRLINE));
    }
    spans
}

/// Step `index` of `steps` on a ramp from a dim tint of `color` to `color`,
/// so fills and graphs glow toward their leading edge on the black canvas.
fn ramp(color: Color, index: usize, steps: usize) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    if steps <= 1 {
        return color;
    }
    let t = 0.35 + 0.65 * index.min(steps - 1) as f64 / (steps - 1) as f64;
    let mix = |channel: u8| (f64::from(channel) * t).round() as u8;
    Color::Rgb(mix(r), mix(g), mix(b))
}

/// Load (used share): high use is the warning.
fn load_color(load_percent: Option<f64>) -> Color {
    match load_percent {
        Some(load) if load > 90.0 => RED,
        Some(load) if load > 75.0 => AMBER,
        _ => ACCENT,
    }
}

fn window_scale(values: &[Option<f64>]) -> f64 {
    values
        .iter()
        .flatten()
        .copied()
        .fold(0.0_f64, f64::max)
        .max(1024.0)
}

fn percent(value: Option<f64>, decimals: usize) -> String {
    match value {
        Some(value) => format!("{value:.decimals$} %"),
        None => "—".into(),
    }
}

fn rate(bytes_per_second: Option<f64>) -> String {
    match bytes_per_second {
        Some(value) => format!("{}/s", format_bytes(value.max(0.0) as u64)),
        None => "—".into(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= TIB {
        format!("{:.1} TiB", bytes / TIB)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

fn mount_label(mount: &str) -> String {
    if mount == "/" {
        return "root".into();
    }
    mount
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("disk")
        .chars()
        .filter(|character| !character.is_control())
        .take(10)
        .collect()
}

/// Content area of a card: inside its rounded outline, one column of padding.
fn inset(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(2),
        width: area.width.saturating_sub(4),
        ..area
    }
}

fn row_at(area: Rect, y: u16) -> Rect {
    Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    }
}

fn span(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color).bg(CANVAS))
}

fn strong(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::default()
            .fg(color)
            .bg(CANVAS)
            .add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentEntry, AgentSnapshot};
    use crate::storage::DiskSnap;
    use crate::system_stats::{CpuMetrics, MemoryMetrics};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const GIB: u64 = 1024 * 1024 * 1024;

    struct Fixture {
        system: SystemMetrics,
        gpu: GpuMetrics,
        cpu: VecDeque<Option<f64>>,
        gpu_history: VecDeque<Option<f64>>,
        download: VecDeque<Option<f64>>,
        upload: VecDeque<Option<f64>>,
        memory: VecDeque<Option<f64>>,
        codex: UsageView,
        claude: UsageView,
        agents: AgentStatusView,
        clock: String,
    }

    impl Fixture {
        fn new() -> Self {
            let mut system = SystemMetrics {
                cpu: CpuMetrics {
                    model: Some("Test CPU".into()),
                    load_percent: Some(14.0),
                    per_core_percent: (0..16).map(|core| Some(f64::from(core) * 6.0)).collect(),
                    frequency_mhz: Some(4200),
                    temperature_c: Some(61.0),
                    power_watts: None,
                },
                ..Default::default()
            };
            system.memory = Some(MemoryMetrics {
                total_bytes: 32 * GIB,
                used_bytes: 16 * GIB,
                available_bytes: 14 * GIB,
                cached_bytes: 6 * GIB,
                free_bytes: 2 * GIB,
                swap_total_bytes: Some(8 * GIB),
                swap_used_bytes: Some(GIB),
                psi_some_avg10_percent: Some(0.0),
                psi_full_avg10_percent: Some(0.5),
            });
            system.disks = Some(vec![
                disk("/", 900 * GIB, 300 * GIB),
                disk("/home", 500 * GIB, 120 * GIB),
                disk("/srv/media", 4000 * GIB, 900 * GIB),
                disk("/var/lib/docker", 200 * GIB, 60 * GIB),
            ]);
            system.network.interface = Some("enp4s0".into());
            system.network.download_bytes_per_second = Some(19_000.0);
            system.network.upload_bytes_per_second = Some(15_000.0);
            let history = |values: &[f64]| values.iter().map(|v| Some(*v)).collect::<VecDeque<_>>();
            Self {
                system,
                gpu: GpuMetrics {
                    load_percent: Some(11.0),
                    vram_free_bytes: Some(6 * GIB),
                    vram_total_bytes: Some(8 * GIB),
                },
                cpu: history(&[10.0, 14.0, 40.0, 14.0]),
                gpu_history: history(&[5.0, 11.0]),
                download: history(&[1000.0, 19_000.0]),
                upload: history(&[500.0, 15_000.0]),
                memory: history(&[50.0, 52.0]),
                codex: UsageView::Quota {
                    used_percent: 42.0,
                    resets_at: Some(
                        SystemTime::now()
                            + std::time::Duration::from_secs(4 * 86_400 + 22 * 3_600 + 1_800),
                    ),
                    stale: false,
                },
                claude: UsageView::Quota {
                    used_percent: 32.0,
                    resets_at: Some(
                        SystemTime::now()
                            + std::time::Duration::from_secs(3 * 86_400 + 20 * 3_600 + 1_800),
                    ),
                    stale: false,
                },
                agents: AgentStatusView::Ready(AgentSnapshot {
                    agents: vec![
                        AgentEntry::new("api", AgentState::Working),
                        AgentEntry::new("docs", AgentState::Idle),
                        AgentEntry::new("build", AgentState::Blocked),
                        AgentEntry::new("tests", AgentState::Done),
                    ],
                }),
                clock: "23:30".into(),
            }
        }

        fn view(&self) -> View<'_> {
            View {
                system: &self.system,
                gpu: self.gpu,
                cpu_history: &self.cpu,
                gpu_history: &self.gpu_history,
                download_history: &self.download,
                upload_history: &self.upload,
                memory_history: &self.memory,
                codex: self.codex,
                claude: self.claude,
                servers: &[],
                servers_available: true,
                servers_stale: false,
                selected_server: 0,
                pending_stop: None,
                server_notice: None,
                agents: &self.agents,
                clock: &self.clock,
            }
        }
    }

    fn disk(mount: &str, total: u64, available: u64) -> DiskSnap {
        DiskSnap {
            source: format!("/dev{mount}"),
            mount: mount.into(),
            total,
            used: total - available,
            available,
        }
    }

    fn screen(view: &View<'_>, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, view)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn contains_all(lines: &[String], needles: &[&str]) {
        let text = lines.join("\n");
        for needle in needles {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    fn relative_luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("palette uses truecolor values");
        };
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (light, dark) = {
            let (x, y) = (relative_luminance(a), relative_luminance(b));
            (x.max(y), x.min(y))
        };
        (light + 0.05) / (dark + 0.05)
    }

    #[test]
    fn wide_screen_names_every_required_reading() {
        let fixture = Fixture::new();
        let lines = screen(&fixture.view(), 120, 40);

        contains_all(
            &lines,
            &[
                "CPU", "RAM", "swap", "PSI", "GPU", "VRAM", "NETZWERK", "CODEX", "CLAUDE",
                "AGENTEN", "claude", "23:30",
            ],
        );
    }

    #[test]
    fn agent_states_use_exact_labels_and_unknown_shows_no_status() {
        let mut fixture = Fixture::new();
        fixture.agents = AgentStatusView::Ready(AgentSnapshot {
            agents: vec![
                AgentEntry::new("WORKER", AgentState::Working),
                AgentEntry::new("WAITING", AgentState::Idle),
                AgentEntry::new("STUCK", AgentState::Blocked),
                AgentEntry::new("FINISHED", AgentState::Done),
                AgentEntry::new("Mystery", AgentState::Unknown),
            ],
        });
        let lines = screen(&fixture.view(), 120, 40);
        let text = lines.join("\n");

        contains_all(&lines, &["WORKING", "IDLE", "BLOCKED", "DONE", "Mystery"]);
        assert!(!text.contains("UNKNOWN"), "unknown status leaked:\n{text}");
        let mystery = lines.iter().find(|line| line.contains("Mystery")).unwrap();
        for word in ["WORKING", "IDLE", "BLOCKED", "DONE"] {
            assert!(!mystery.contains(word));
        }
    }

    #[test]
    fn unavailable_and_loading_states_are_explicit() {
        let mut fixture = Fixture::new();
        fixture.codex = UsageView::Unavailable;
        fixture.claude = UsageView::Loading;
        fixture.agents = AgentStatusView::Unavailable;
        let lines = screen(&fixture.view(), 120, 40);

        contains_all(
            &lines,
            &[
                "nicht erreichbar",
                "Wochenlimit wird geladen",
                "Claude/Codex nicht erreichbar",
            ],
        );
    }

    #[test]
    fn swap_and_both_psi_values_stay_visible_at_supported_sizes() {
        let fixture = Fixture::new();
        for (width, height) in [(100, 30), (236, 68)] {
            let lines = screen(&fixture.view(), width, height);
            contains_all(&lines, &["swap", "some", "full"]);
        }
    }

    #[test]
    fn every_disk_volume_is_listed() {
        let fixture = Fixture::new();
        let lines = screen(&fixture.view(), 120, 40);

        contains_all(&lines, &["root", "home", "media", "docker"]);
    }

    #[test]
    fn clock_is_pinned_to_the_bottom_right_corner() {
        let fixture = Fixture::new();
        let lines = screen(&fixture.view(), 120, 40);
        let last = lines.last().unwrap().trim_end();

        assert!(last.ends_with("23:30"), "footer ends with {last:?}");
        assert!(last.contains("trennen"));
    }

    #[test]
    fn small_terminal_keeps_usage_and_core_readings() {
        let fixture = Fixture::new();
        let lines = screen(&fixture.view(), 80, 24);

        contains_all(&lines, &["CODEX", "CLAUDE", "RAM", "GPU", "CPU"]);
    }

    #[test]
    fn every_size_renders_without_panicking() {
        let fixture = Fixture::new();
        for width in (40..=240).step_by(9) {
            for height in (8..=70).step_by(6) {
                screen(&fixture.view(), width, height);
            }
        }
    }

    #[test]
    fn canvas_and_text_meet_contrast_targets() {
        for text in [INK, SECONDARY, TERTIARY, ACCENT, GREEN, AMBER, RED] {
            assert!(
                contrast(text, CANVAS) >= 4.5,
                "{text:?} is below 4.5:1 on the canvas"
            );
        }
    }

    #[test]
    fn bars_fill_in_eighth_cells_and_mark_unknown_as_track_only() {
        let text = |spans: Vec<Span<'static>>| {
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };

        assert_eq!(text(bar(Some(0.5), 4, ACCENT)), "██··");
        assert_eq!(text(bar(Some(0.0), 4, ACCENT)), "····");
        assert_eq!(text(bar(None, 4, ACCENT)), "····");
        assert_eq!(text(bar(Some(1.0), 4, ACCENT)), "████");
    }

    #[test]
    fn graphs_scale_to_the_maximum_and_mark_unknown_samples() {
        let lines = graph_lines(
            &[None, Some(0.0), Some(50.0), Some(100.0)],
            4,
            1,
            100.0,
            ACCENT,
        );
        let text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();

        assert_eq!(text, "· ▄█");
    }

    #[test]
    fn a_limit_of_an_agent_that_is_not_installed_is_not_drawn() {
        let mut fixture = Fixture::new();
        fixture.claude = UsageView::Absent;
        for (width, height) in [(120, 40), (80, 24)] {
            let lines = screen(&fixture.view(), width, height);
            let text = lines.join("\n");
            assert!(text.contains("CODEX"), "{width}x{height}:\n{text}");
            assert!(!text.contains("CLAUDE"), "{width}x{height}:\n{text}");
        }
    }

    #[test]
    fn without_any_agent_the_limits_panel_disappears_and_the_rest_stays() {
        let mut fixture = Fixture::new();
        fixture.codex = UsageView::Absent;
        fixture.claude = UsageView::Absent;
        for (width, height) in [(120, 40), (80, 24)] {
            let lines = screen(&fixture.view(), width, height);
            let text = lines.join("\n");
            assert!(!text.contains("WOCHENLIMIT"), "{width}x{height}:\n{text}");
            for needle in ["CPU", "RAM", "GPU"] {
                assert!(
                    text.contains(needle),
                    "{needle} missing at {width}x{height}"
                );
            }
        }
        let wide = screen(&fixture.view(), 120, 40).join("\n");
        assert!(wide.contains("SERVER") && wide.contains("AGENTEN"));
    }

    #[test]
    fn a_throttled_limit_says_so_instead_of_unreachable() {
        let mut fixture = Fixture::new();
        fixture.claude = UsageView::Throttled;
        let lines = screen(&fixture.view(), 120, 40);
        let text = lines.join("\n");

        contains_all(&lines, &["gedrosselt"]);
        assert!(
            !text.contains("nicht erreichbar"),
            "throttle shown as unreachable:\n{text}"
        );
    }

    #[test]
    fn usage_card_shows_free_share_reset_and_staleness() {
        let mut fixture = Fixture::new();
        fixture.claude = UsageView::Quota {
            used_percent: 32.0,
            resets_at: Some(
                SystemTime::now() + std::time::Duration::from_secs(3 * 86_400 + 20 * 3_600 + 1_800),
            ),
            stale: true,
        };
        let lines = screen(&fixture.view(), 120, 40);

        contains_all(
            &lines,
            &[
                "32.0 % genutzt",
                "68.0 % frei",
                "bis 3 T 20 Std",
                "veraltet",
            ],
        );
    }

    #[test]
    fn headline_numbers_lead_with_what_is_used() {
        let fixture = Fixture::new();
        let lines = screen(&fixture.view(), 120, 40);

        contains_all(
            &lines,
            &[
                "14 % belegt",
                "16.0 GiB belegt",
                "2.0 GiB belegt",
                "600.0 GiB",
                "42.0 % genutzt",
                "32.0 % genutzt",
            ],
        );
    }

    #[test]
    fn core_grid_uses_one_cell_per_core() {
        let per_core: Vec<Option<f64>> = (0..16).map(|core| Some(f64::from(core))).collect();

        assert_eq!(core_lines(&per_core, 33).len(), 8);
        assert_eq!(core_lines(&per_core, 80).len(), 4);
    }

    #[test]
    fn disk_rows_count_matches_volumes() {
        let fixture = Fixture::new();

        assert_eq!(disk_panel(&fixture.view(), 40).rows.len(), 4);
    }
}
