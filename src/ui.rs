//! Rendering: boxed panels in the manner of btop. Colours come from `palette`
//! and text stays in the terminal's default foreground, so it reads on any theme.
use crate::app::{self, App, SessionLine};
use crate::charts::{self, Ramp, Segment, segment};
use crate::claude;
use crate::palette;
use crate::sessions::{self, Session, SortKey};
use crate::system::{Memory, Pressure};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use std::collections::VecDeque;

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
/// Biggest non-Claude consumers named in the memory panel.
const STRIP_CONSUMERS: usize = 3;
const MEM_WIDTH: u16 = 5;
const CPU_WIDTH: u16 = 5;
const AGE_WIDTH: u16 = 3;
const CTX_WIDTH: u16 = 4;
/// A session idle for less than this is flagged as waiting on you.
const READY_SECS: u64 = 600;
/// Gap between cluster charts.
const CLUSTER_GAP: u16 = 2;
/// Narrowest cpu panel that splits into a graph and a column of figures.
const CPU_SPLIT: u16 = 36;
/// Label and figure widths in that column.
const CPU_LABEL: u16 = 4;
const CPU_FIGURE: u16 = 5;
/// Width of a core's figure, and the gap between two columns of cores.
const CORE_FIGURE: u16 = 4;
const CORE_GAP: u16 = 2;
/// Least and full width of the figures column holding two columns of cores.
const CORES_TWO_COLUMNS: u16 = 24;
const CORES_TWO_COLUMNS_FULL: u16 = 32;
/// Least watts a full power graph stands for, so idle noise stays small.
const POWER_FLOOR: f32 = 20.0;
/// Temperatures the graph spans, in °C.
const TEMP_RANGE: (f64, f64) = (30.0, 105.0);
/// From this width, charts go on the left and lists on the right.
const WIDE: u16 = 110;
/// Rows of the full memory breakdown; shorter panels get one bar and a legend.
const MEM_DETAIL: u16 = 6;
/// Rows of memory history above it, when there's room.
const MEM_GRAPH: u16 = 4;
/// Narrowest memory panel that fits both breakdown columns.
const MEM_TWO_COLUMNS: u16 = 34;
/// Label widths of the two breakdown columns: `cache`, `Claude Code`.
const WHAT_LABEL: u16 = 6;
const WHO_LABEL: u16 = 11;
/// Narrowest memory panel that fits the long names.
const MEM_LONG_NAMES: u16 = 52;
/// Shown for missing GPU, power and temperature readings.
const NO_SENSORS: &str = if cfg!(target_os = "macos") {
    "no macmon"
} else {
    "no sensors"
};
/// The kernel's own memory, as each platform calls it.
const WIRED: &str = if cfg!(target_os = "macos") {
    "wired"
} else {
    "kern"
};
/// Shortest panel worth drawing: borders and one row.
const MIN_PANEL: u16 = 3;

pub const HELP: &str = "\
q        quit
s        cycle sort: mem, cpu
↑↓ j k   select session
Enter    expand process tree
?        this help

cpu: overall load in the title. The graph is the first cluster's history, with the last cluster's mirrored below. Beside it, the total, then every core with its own history; in a small panel, each cluster instead, with one bar per core, where 100% = all of those cores. Green under 60, amber under 85, then red. With macmon, or on Linux whatever sensors the machine has: GPU load, whole-system power and CPU temperature with their history, then power by part, GPU temperature and fan speed.

mem: used/total in the title, and the pressure level, which also colours the history. Left is what RAM is made of, and swap. Right is who has it: all Claude Code sessions together (`agents` when narrow), the biggest apps, the rest, and system (wired). Bars split used RAM by footprint; footprints include compressed and swapped pages, so the figures can add up to more than used.

sessions: ● working, ◆ went idle in the last 10 minutes, so probably waiting on you, ○ idle. `for` is how long it has been that way and `ctx` the tokens in its context, both as Claude Code reports them; without that, the mark goes by recent CPU. A name you gave the session replaces its folder. Bars are memory, relative to the largest row. The graph is CPU history, up to one core. Colours last for the session's life.

processes: everything else, with an app's helpers grouped.

MEM is phys_footprint, the Memory column of Activity Monitor, not RSS. Used is app + wired + compressed. On Linux, MEM is anonymous resident memory, used is total less available, and kern stands in for wired. CPU per row: 100% = one core.

A session is a top-level claude CLI with all its descendants by ppid. Nested claudes count under their root. Daemons reparented to launchd or init are lost.

On macOS, other users' processes can't be read without root; they are counted as n/a.";

/// Outer height of each panel, top to bottom. 0 hides it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub cpu: u16,
    pub mem: u16,
    pub sessions: u16,
    pub procs: u16,
}

impl Plan {
    fn rows(&self) -> u16 {
        self.cpu + self.mem + self.sessions + self.procs
    }
}

type Part = fn(&mut Plan) -> &mut u16;

/// Takes rows back in this order until the plan fits: graphs first, then the
/// process list, then the memory breakdown, and sessions last.
const SHRINK: [(Part, u16); 13] = [
    (|p| &mut p.cpu, 8),
    (|p| &mut p.procs, 5),
    (|p| &mut p.mem, 9),
    (|p| &mut p.cpu, 5),
    (|p| &mut p.mem, 8),
    (|p| &mut p.cpu, 4),
    (|p| &mut p.mem, 4),
    (|p| &mut p.sessions, 8),
    (|p| &mut p.cpu, 3),
    (|p| &mut p.procs, 0),
    (|p| &mut p.sessions, 3),
    (|p| &mut p.cpu, 0),
    (|p| &mut p.mem, 0),
];

/// Single-column layout. `sessions_wanted` and `procs_wanted` are list rows.
pub fn plan(height: u16, sessions_wanted: u16, procs_wanted: u16) -> Plan {
    // Sessions grow with their rows up to half the pane, then scroll.
    let sessions_full = (sessions_wanted + 2).min((height / 2).max(MIN_PANEL));
    let procs_full = if procs_wanted > 0 {
        procs_wanted + 2
    } else {
        0
    };
    let mut plan = Plan {
        cpu: 10,
        mem: 2 + MEM_GRAPH + MEM_DETAIL,
        sessions: sessions_full,
        procs: procs_full.min(7),
    };
    for (part, min) in SHRINK {
        while plan.rows() > height && *part(&mut plan) > min {
            *part(&mut plan) -= 1;
        }
    }
    for part in [|p| &mut p.cpu, |p| &mut p.mem, |p| &mut p.procs] as [Part; 3] {
        if *part(&mut plan) < MIN_PANEL {
            *part(&mut plan) = 0;
        }
    }
    if plan.rows() > height {
        plan.sessions = height;
    }

    // Spare rows: sessions in full, somewhat taller graphs, then more processes.
    let spare = |plan: &Plan| height.saturating_sub(plan.rows());
    plan.sessions += spare(&plan).min(sessions_full.saturating_sub(plan.sessions));
    if plan.cpu > 0 {
        // Enough for two columns of cores on the bigger chips.
        plan.cpu += (spare(&plan) / 2).min(6);
    }
    if plan.procs == 0 && procs_full > 0 && spare(&plan) >= MIN_PANEL {
        plan.procs = MIN_PANEL;
    }
    if plan.procs > 0 {
        plan.procs += spare(&plan).min(procs_full.saturating_sub(plan.procs));
    }
    // Whatever is left makes the cpu panel taller, rather than leaving a gap:
    // it has cores to list, and memory's history says little more when tall.
    for part in [|p| &mut p.cpu, |p| &mut p.mem] as [Part; 2] {
        if *part(&mut plan) > 0 {
            *part(&mut plan) += spare(&plan);
        }
    }
    // Nothing else to show: sessions get the rest after all.
    plan.sessions += spare(&plan);
    plan
}

/// Heights of the sessions and processes panels sharing a column.
/// Without processes to list, sessions take it all.
fn plan_lists(height: u16, sessions_wanted: u16, procs_wanted: u16) -> (u16, u16) {
    // Sessions grow with their rows up to half the column, then scroll.
    let most = if height >= 10 { height / 2 } else { height };
    let sessions = (sessions_wanted + 2).min(most);
    let procs = height - sessions;
    if procs < MIN_PANEL || procs_wanted == 0 {
        (height, 0)
    } else {
        (sessions, procs)
    }
}

pub fn fmt_mem(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= 100.0 * GIB {
        format!("{:.0}G", b / GIB)
    } else if b >= GIB {
        format!("{:.1}G", b / GIB)
    } else {
        format!("{:.0}M", b / MIB)
    }
}

/// `35.0/48.0G`, sharing the unit when both are in G.
fn fmt_used_total(used: u64, total: u64) -> String {
    let total = fmt_mem(total);
    let used = fmt_mem(used);
    let shared = total.ends_with('G') && used.ends_with('G');
    format!(
        "{}/{total}",
        if shared {
            &used[..used.len() - 1]
        } else {
            &used
        }
    )
}

fn fmt_cpu(pct: f64) -> String {
    format!("{pct:.0}%")
}

fn pressure_color(p: Pressure) -> Color {
    match p {
        Pressure::Normal => palette::GOOD,
        Pressure::Warn => palette::WARN,
        Pressure::Critical => palette::BAD,
        Pressure::Unknown => palette::NEUTRAL,
    }
}

/// Takes rows off the top of `area`.
fn take(area: &mut Rect, rows: u16) -> Rect {
    let rows = rows.min(area.height);
    let taken = Rect {
        height: rows,
        ..*area
    };
    area.y += rows;
    area.height -= rows;
    taken
}

fn memory_segments(mem: &Memory) -> Vec<Segment> {
    vec![
        segment(mem.app as f64, palette::APP),
        segment(mem.wired as f64, palette::WIRED),
        segment(mem.compressed as f64, palette::COMPRESSED),
        segment(mem.cached as f64, palette::CACHED),
    ]
}

/// `■ name size` tokens, as many as fit on `rows` lines of `width`.
fn legend_lines(items: &[(String, Color)], width: u16, rows: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for (text, color) in items {
        let need = text.chars().count() as u16 + 2;
        if used + need > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        if lines.len() >= rows as usize || need > width {
            spans.clear();
            break;
        }
        spans.push(Span::styled(charts::SWATCH, charts::style(*color)));
        spans.push(Span::raw(format!("{text} ")));
        used += need;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines.retain(|line| line.width() > 0);
    lines
}

fn draw_legend(buf: &mut Buffer, rect: Rect, items: &[(String, Color)]) {
    for (line, y) in legend_lines(items, rect.width, rect.height)
        .iter()
        .zip(rect.top()..)
    {
        buf.set_line(rect.x, y, line, rect.width);
    }
}

/// One owner of RAM. `share` is physical bytes for the
/// bar; `bytes` is the figure to print.
#[derive(Debug, PartialEq)]
pub struct Consumer {
    pub label: String,
    pub bytes: u64,
    pub share: f64,
    pub color: Color,
    pub session: bool,
}

fn others_by_mem(app: &App) -> Vec<&sessions::Row> {
    let mut others: Vec<&sessions::Row> = app.grouped.others.iter().collect();
    others.sort_by_key(|r| std::cmp::Reverse(r.mem));
    others
}

/// `Google Chrome` -> `Chrome`.
fn short_app_name(name: &str) -> &str {
    name.rsplit(' ').next().unwrap_or(name)
}

/// Sessions largest first, the biggest other consumers, the rest, then system.
/// Footprints count compressed and swapped pages at full size, so they overrun
/// physical RAM when stacked. Wired is the system slice, and the rest of used
/// memory is split between processes in proportion to footprint.
pub fn consumers(app: &App) -> Vec<Consumer> {
    let mut sessions: Vec<&Session> = app.grouped.sessions.iter().collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.mem));
    let consumer = |label: String, bytes, color, session| Consumer {
        label,
        bytes,
        share: bytes as f64,
        color,
        session,
    };
    let mut list: Vec<Consumer> = sessions
        .iter()
        .map(|s| consumer(s.name.clone(), s.mem, session_color(app, s.pid), true))
        .collect();
    let others = others_by_mem(app);
    for (row, color) in others.iter().zip(palette::CONSUMERS) {
        let label = short_app_name(&row.name).to_string();
        list.push(consumer(label, row.mem, color, false));
    }
    let rest: u64 = others.iter().skip(STRIP_CONSUMERS).map(|r| r.mem).sum();
    list.push(consumer("other".into(), rest, palette::OTHER, false));

    let footprints: f64 = list.iter().map(|c| c.share).sum();
    let wired = app.memory.wired.min(app.memory.used);
    let room = (app.memory.used - wired) as f64;
    if footprints > 0.0 {
        for c in &mut list {
            c.share *= room / footprints;
        }
    }
    list.push(consumer("system".into(), wired, palette::NEUTRAL, false));
    list
}

fn session_color(app: &App, pid: i32) -> Color {
    palette::session(app.session_slots.get(&pid).copied().unwrap_or(0))
}

/// A rounded box with its title in the top border. Returns the inside.
fn frame(buf: &mut Buffer, rect: Rect, title: &Line, color: Color) -> Rect {
    if rect.width < 2 || rect.height < 2 {
        return Rect::new(rect.x, rect.y, 0, 0);
    }
    let style = charts::style(color);
    let (right, bottom) = (rect.right() - 1, rect.bottom() - 1);
    for x in rect.left() + 1..right {
        buf[(x, rect.y)].set_symbol("─").set_style(style);
        buf[(x, bottom)].set_symbol("─").set_style(style);
    }
    for y in rect.top() + 1..bottom {
        buf[(rect.x, y)].set_symbol("│").set_style(style);
        buf[(right, y)].set_symbol("│").set_style(style);
    }
    for (x, y, corner) in [
        (rect.x, rect.y, "╭"),
        (right, rect.y, "╮"),
        (rect.x, bottom, "╰"),
        (right, bottom, "╯"),
    ] {
        buf[(x, y)].set_symbol(corner).set_style(style);
    }
    border_text(buf, rect.x + 2, rect.y, right.saturating_sub(1), title);
    Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2)
}

/// Where a panel's title stops, so right-hand labels can keep clear of it.
fn title_end(rect: Rect, title: &Line) -> u16 {
    rect.x + 2 + title.width() as u16 + 2
}

/// Writes `line` into a border from `x`, padded by a space each side, if it
/// all fits before `limit`.
fn border_text(buf: &mut Buffer, x: u16, y: u16, limit: u16, line: &Line) -> bool {
    let width = line.width() as u16 + 2;
    if width == 2 || x + width > limit {
        return false;
    }
    for x in x..x + width {
        buf[(x, y)].reset();
    }
    buf.set_line(x + 1, y, line, width - 2);
    true
}

/// `border_text`, right-aligned to end before `end` and start no sooner than `min`.
fn border_label(buf: &mut Buffer, y: u16, end: u16, min: u16, line: &Line) -> bool {
    let width = line.width() as u16 + 2;
    match end.checked_sub(width) {
        Some(start) if start >= min => border_text(buf, start, y, end + 1, line),
        _ => false,
    }
}

const HINTS: [(&str, &str); 5] = [
    ("↑↓", "select"),
    ("⏎", "tree"),
    ("s", "sort"),
    ("?", "help"),
    ("q", "quit"),
];
/// Which hints to keep as the border narrows, most wanted first.
const HINT_PRIORITY: [usize; 5] = [3, 4, 2, 1, 0];

/// As many key hints as fit in `width`, in reading order.
fn hints(width: u16) -> Line<'static> {
    let mut keep = [false; HINTS.len()];
    let mut used = 0;
    for i in HINT_PRIORITY {
        let (key, what) = HINTS[i];
        let need = (key.chars().count() + 1 + what.len()) as u16 + if used > 0 { 2 } else { 0 };
        if used + need > width {
            break;
        }
        keep[i] = true;
        used += need;
    }
    let mut spans = Vec::new();
    for (&(key, what), _) in HINTS.iter().zip(keep).filter(|(_, keep)| *keep) {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(key, BOLD));
        spans.push(Span::styled(format!(" {what}"), DIM));
    }
    Line::from(spans)
}

fn draw_hints(buf: &mut Buffer, rect: Rect) {
    if rect.height < 2 || rect.width < 8 {
        return;
    }
    let line = hints(rect.width - 6);
    border_text(buf, rect.x + 2, rect.bottom() - 1, rect.right() - 1, &line);
}

fn sensors(app: &App, gpu: bool) -> Line<'static> {
    let Some(sample) = &app.sensors else {
        return Line::styled(NO_SENSORS, DIM);
    };
    let gpu = sample
        .gpu_pct
        .filter(|_| gpu)
        .map(|g| format!("GPU {g:.0}%"));
    let temp = sample.temp_c.map(|t| format!("{t:.0}°C"));
    let watts = sample.watts.map(|w| format!("{w:.0}W"));
    let parts: Vec<String> = [gpu, temp, watts].into_iter().flatten().collect();
    Line::raw(parts.join(" · "))
}

fn draw_cpu(buf: &mut Buffer, rect: Rect, app: &App) {
    let overall = app.cpu.overall;
    let title = Line::from(vec![
        Span::styled("cpu ", BOLD),
        Span::styled(fmt_cpu(overall), BOLD.fg(palette::load(overall))),
    ]);
    let inner = frame(buf, rect, &title, palette::NEUTRAL);
    if inner.is_empty() {
        return;
    }
    let (end, min) = (rect.right() - 2, title_end(rect, &title));
    if !border_label(buf, rect.y, end, min, &sensors(app, true)) {
        border_label(buf, rect.y, end, min, &sensors(app, false));
    }

    let clusters = &app.cpu.clusters;
    let info_rows = 1 + clusters.len() as u16;
    if clusters.is_empty() || inner.height < info_rows || inner.width < CPU_SPLIT {
        draw_cpu_compact(buf, inner, app);
        return;
    }

    // Right: the total as a meter, then every core with its history, in one
    // column or two; or, without the room, each cluster with a bar per core.
    let sensor_rows = app.sensors.as_ref().map_or(0, |s| {
        u16::from(s.gpu_pct.is_some())
            + u16::from(s.watts.is_some())
            + u16::from(s.temp_c.is_some())
    });
    let cores = clusters.iter().map(|c| c.cores.len()).sum::<usize>() as u16;
    let room = inner.height.saturating_sub(1 + sensor_rows);
    let core_columns = if cores == 0 || clusters.iter().any(|c| c.cores.is_empty()) {
        0
    } else if cores <= room {
        1
    } else if cores.div_ceil(2) <= room && inner.width / 2 >= CORES_TWO_COLUMNS {
        2
    } else {
        0
    };
    let most_cores = clusters.iter().map(|c| c.cores.len()).max().unwrap_or(0);
    let info_width = match core_columns {
        0 => CPU_LABEL + most_cores.clamp(6, 16) as u16 + CPU_FIGURE,
        1 => CPU_LABEL + 16 + CPU_FIGURE,
        _ => CORES_TWO_COLUMNS_FULL,
    }
    .min(inner.width / 2);
    let middle = info_width - CPU_LABEL - CPU_FIGURE;
    let info = Rect {
        x: inner.right() - info_width,
        width: info_width,
        ..inner
    };
    let divider = info.x - 2;
    for y in inner.top()..inner.bottom() {
        buf[(divider, y)].set_symbol("│").set_fg(palette::NEUTRAL);
    }
    let mut rows = info.top()..info.bottom();
    // Label and figure; returns the space between them.
    let row = |buf: &mut Buffer, y: Option<u16>, label: &str, figure: String, style: Style| {
        let y = y?;
        buf.set_stringn(info.x, y, label, CPU_LABEL as usize, Style::new());
        put_right(buf, info.right(), y, CPU_FIGURE, &figure, style);
        Some(Rect::new(info.x + CPU_LABEL, y, middle, 1))
    };
    let figure = |pct: f64| (fmt_cpu(pct), BOLD.fg(palette::load(pct)));
    let full = [segment(1.0, palette::NEUTRAL)];
    let (text, style) = figure(overall);
    if let Some(rect) = row(buf, rows.next(), "CPU", text, style) {
        charts::meter(buf, rect, &full, 100.0 / overall.max(0.01), Ramp::Heat);
    }
    if core_columns > 0 {
        let per = cores.div_ceil(core_columns);
        let block = Rect {
            y: info.y + 1,
            height: per,
            ..info
        };
        draw_cores(buf, block, core_columns, app);
        rows.nth(usize::from(per) - 1);
    } else {
        for cluster in clusters {
            let (text, style) = figure(cluster.pct);
            if let Some(rect) = row(buf, rows.next(), &cluster.label, text, style) {
                charts::cores(buf, rect, &cluster.cores);
            }
        }
    }
    let Some(sample) = &app.sensors else {
        return draw_cpu_graph(buf, inner, divider, app);
    };
    if let Some(gpu) = sample.gpu_pct {
        let (text, style) = figure(gpu);
        if let Some(rect) = row(buf, rows.next(), "GPU", text, style) {
            charts::meter(buf, rect, &full, 100.0 / gpu.max(0.01), Ramp::Heat);
        }
    }
    if let Some(watts) = sample.watts {
        // Scaled to the most drawn so far, so the shape always uses the row.
        let peak = app
            .power_history
            .iter()
            .copied()
            .fold(POWER_FLOOR, f32::max);
        let samples: Vec<(f64, Color)> = app
            .power_history
            .iter()
            .map(|&w| (f64::from(w), palette::POWER))
            .collect();
        if let Some(rect) = row(buf, rows.next(), "PWR", format!("{watts:.0}W"), BOLD) {
            charts::graph(buf, rect, &samples, f64::from(peak), None, false);
        }
    }
    if let Some(temp) = sample.temp_c {
        let pct = |t: f64| (t - TEMP_RANGE.0) / (TEMP_RANGE.1 - TEMP_RANGE.0) * 100.0;
        let samples: Vec<(f64, Color)> = app
            .temp_history
            .iter()
            .map(|&t| (pct(f64::from(t)).clamp(0.0, 100.0), palette::NEUTRAL))
            .collect();
        let style = BOLD.fg(palette::load(pct(temp)));
        if let Some(rect) = row(buf, rows.next(), "TMP", format!("{temp:.0}°"), style) {
            charts::graph(buf, rect, &samples, 100.0, Some(palette::heat), false);
        }
    }
    // Then whatever detail there's room for, as plain text.
    let watts = |name: &str, w: Option<f64>| w.map(|w| format!("{name} {w:.1}W"));
    let split = [
        watts("cpu", sample.cpu_watts),
        watts("gpu", sample.gpu_watts),
        watts("ane", sample.ane_watts),
    ];
    let cooling = [
        sample.gpu_temp_c.map(|t| format!("gpu {t:.0}°")),
        sample.fan_rpm.map(|rpm| format!("fan {rpm:.0}rpm")),
    ];
    for parts in [&split[..], &cooling[..]] {
        let mut text = String::new();
        for part in parts.iter().flatten() {
            let gap = usize::from(!text.is_empty());
            if text.chars().count() + gap + part.chars().count() <= info.width as usize {
                text = format!("{text}{}{part}", " ".repeat(gap));
            }
        }
        if let Some(y) = rows.next().filter(|_| !text.is_empty()) {
            buf.set_stringn(info.x, y, text, info.width as usize, DIM);
        }
    }
    draw_cpu_graph(buf, inner, divider, app);
}

/// Every core as `P0 ⣀⣠⣀ 24%`, filling `columns` columns top to bottom.
fn draw_cores(buf: &mut Buffer, rect: Rect, columns: u16, app: &App) {
    let labels: Vec<String> = app
        .cpu
        .clusters
        .iter()
        .flat_map(|c| (0..c.cores.len()).map(move |i| format!("{}{i}", c.label)))
        .collect();
    let loads = app.cpu.clusters.iter().flat_map(|c| &c.cores);
    let label = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let width = (rect.width - (columns - 1) * CORE_GAP) / columns;
    for (i, (name, &pct)) in labels.iter().zip(loads).enumerate() {
        let (column, line) = (i as u16 / rect.height, i as u16 % rect.height);
        // The last column sits on the right edge, in line with the figures
        // around it; any odd cell widens the gap.
        let x = if column + 1 == columns {
            rect.right() - width
        } else {
            rect.x + column * (width + CORE_GAP)
        };
        let y = rect.y + line;
        buf.set_stringn(x, y, name, label as usize, Style::new());
        let figure = Style::new().fg(palette::load(pct));
        put_right(buf, x + width, y, CORE_FIGURE, &fmt_cpu(pct), figure);
        let graph = Rect::new(
            x + label + 1,
            y,
            width.saturating_sub(label + CORE_FIGURE + 2),
            1,
        );
        if let Some(history) = app.core_history.get(i) {
            let samples: Vec<(f64, Color)> = history
                .iter()
                .map(|&pct| (f64::from(pct), palette::NEUTRAL))
                .collect();
            charts::graph(buf, graph, &samples, 100.0, Some(palette::heat), false);
        }
    }
}

/// The first cluster's history growing up from the middle, and the last one's
/// mirrored below it, left of the figures.
fn draw_cpu_graph(buf: &mut Buffer, inner: Rect, divider: u16, app: &App) {
    let clusters = &app.cpu.clusters;
    let mut chart = Rect {
        width: divider - 1 - inner.x,
        ..inner
    };
    // A machine with one kind of core gets the whole height for it.
    let rows = if clusters.len() > 1 {
        inner.height.div_ceil(2)
    } else {
        inner.height
    };
    let upper = take(&mut chart, rows);
    let last = &clusters[clusters.len() - 1];
    for (cluster, rect, down) in [(&clusters[0], upper, false), (last, chart, true)] {
        if rect.is_empty() {
            continue;
        }
        if let Some(history) = app.cluster_history.get(&cluster.label) {
            let samples: Vec<(f64, Color)> = history
                .iter()
                .map(|&pct| (f64::from(pct), palette::NEUTRAL))
                .collect();
            charts::graph(buf, rect, &samples, 100.0, Some(palette::heat), down);
        }
        let y = if down { rect.bottom() - 1 } else { rect.y };
        buf.set_stringn(rect.x, y, &cluster.label, rect.width as usize, DIM);
    }
}

/// For short or narrow panels: each cluster as `P 17%  ▂▃▁▅` over its history.
/// Width follows core count, and the figure is padded, so nothing shifts as
/// the load changes.
fn draw_cpu_compact(buf: &mut Buffer, inner: Rect, app: &App) {
    let clusters = &app.cpu.clusters;
    let head = |label: &str| label.chars().count() as u16 + 6;
    let needs: Vec<u16> = clusters
        .iter()
        .map(|c| head(&c.label) + c.cores.len() as u16)
        .collect();
    let total: u16 = needs.iter().sum();
    let gaps = (clusters.len() as u16).saturating_sub(1) * CLUSTER_GAP;
    let room = inner.width.saturating_sub(gaps);
    if total == 0 || room == 0 {
        return;
    }
    let mut x = inner.x;
    for (i, (cluster, need)) in clusters.iter().zip(&needs).enumerate() {
        let width = if i + 1 == clusters.len() {
            inner.right().saturating_sub(x)
        } else {
            (u32::from(room) * u32::from(*need) / u32::from(total)) as u16
        };
        let line = Line::from(vec![
            Span::raw(format!("{} ", cluster.label)),
            Span::styled(
                format!("{:<4}", fmt_cpu(cluster.pct)),
                BOLD.fg(palette::load(cluster.pct)),
            ),
        ]);
        buf.set_line(x, inner.y, &line, width);
        let head = head(&cluster.label).min(width);
        let cores = Rect::new(x + head, inner.y, width - head, 1);
        charts::cores(buf, cores, &cluster.cores);
        if let Some(history) = app.cluster_history.get(&cluster.label) {
            let samples: Vec<(f64, Color)> = history
                .iter()
                .map(|&pct| (f64::from(pct), palette::NEUTRAL))
                .collect();
            let chart = Rect::new(x, inner.y + 1, width, inner.height - 1);
            charts::graph(buf, chart, &samples, 100.0, Some(palette::heat), false);
        }
        x += width + CLUSTER_GAP;
    }
}

/// One labelled bar: `app    ████░░░░ 14.2G`. Segments are fractions of the bar.
struct Meter {
    label: String,
    segments: Vec<Segment>,
    ramp: Ramp,
    value: String,
}

fn fraction(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { part / whole } else { 0.0 }
}

/// What RAM is made of, then swap.
fn what_meters(mem: &Memory) -> Vec<Meter> {
    let total = mem.total as f64;
    let free = mem
        .total
        .saturating_sub(mem.app + mem.wired + mem.compressed + mem.cached);
    let swap = fraction(mem.swap_used as f64, mem.swap_total as f64);
    [
        ("app", mem.app, palette::APP),
        (WIRED, mem.wired, palette::WIRED),
        ("comp", mem.compressed, palette::COMPRESSED),
        ("cache", mem.cached, palette::CACHED),
        ("free", free, palette::NEUTRAL),
    ]
    .into_iter()
    .map(|(label, bytes, color)| Meter {
        label: label.into(),
        segments: vec![segment(fraction(bytes as f64, total), color)],
        ramp: Ramp::Fade,
        value: fmt_mem(bytes),
    })
    .chain([Meter {
        label: "swap".into(),
        segments: vec![segment(swap, palette::NEUTRAL)],
        ramp: Ramp::Heat,
        value: fmt_mem(mem.swap_used),
    }])
    .collect()
}

/// Who has the RAM: all sessions on one bar, each in its colour, then the rest.
fn who_meters(app: &App, wide: bool) -> Vec<Meter> {
    let total = app.memory.total as f64;
    let list = consumers(app);
    let part = |c: &Consumer| segment(fraction(c.share, total), c.color);
    // Every session as one total; the sessions panel tells them apart.
    let sessions = list.iter().filter(|c| c.session);
    let share: f64 = sessions.clone().map(|c| c.share).sum();
    let mut meters = vec![Meter {
        label: if wide { "Claude Code" } else { "agents" }.into(),
        segments: vec![segment(fraction(share, total), palette::CLAUDE)],
        ramp: Ramp::Fade,
        value: fmt_mem(sessions.map(|c| c.bytes).sum()),
    }];
    meters.extend(list.iter().filter(|c| !c.session).map(|c| Meter {
        // The desktop app, named apart from the CLI sessions above.
        label: match (c.label.as_str(), wide) {
            ("Claude", true) => "Claude app".into(),
            _ => c.label.clone(),
        },
        segments: vec![part(c)],
        ramp: Ramp::Fade,
        value: fmt_mem(c.bytes),
    }));
    meters
}

fn draw_meters(buf: &mut Buffer, rect: Rect, meters: &[Meter], label: u16) {
    for (meter, y) in meters.iter().zip(rect.top()..rect.bottom()) {
        let name = label.min(rect.width);
        put_name(buf, rect.x, y, name, &meter.label, Style::new());
        put_right(buf, rect.right(), y, MEM_WIDTH, &meter.value, Style::new());
        let bar = Rect::new(
            rect.x + label + 1,
            y,
            rect.width.saturating_sub(label + MEM_WIDTH + 2),
            1,
        );
        if bar.width >= 2 {
            charts::meter(buf, bar, &meter.segments, 1.0, meter.ramp);
        }
    }
}

/// The breakdown when there's no room for meters: one bar and a legend.
fn draw_memory_compact(buf: &mut Buffer, mut area: Rect, mem: &Memory) {
    let total = mem.total as f64;
    charts::bar(buf, take(&mut area, 1), &memory_segments(mem), total, true);
    // Whole gigabytes keep the legend on one line when narrow.
    let size = |bytes: u64| {
        if area.width < 52 && bytes >= 1 << 30 {
            format!("{:.0}G", bytes as f64 / (1u64 << 30) as f64)
        } else {
            fmt_mem(bytes)
        }
    };
    let swap = fraction(mem.swap_used as f64, mem.swap_total as f64) * 100.0;
    let legend = [
        (format!("app {}", size(mem.app)), palette::APP),
        (format!("{WIRED} {}", size(mem.wired)), palette::WIRED),
        (
            format!("comp {}", size(mem.compressed)),
            palette::COMPRESSED,
        ),
        (format!("cache {}", size(mem.cached)), palette::CACHED),
        (format!("swap {}", size(mem.swap_used)), palette::load(swap)),
    ];
    draw_legend(buf, area, &legend);
}

fn draw_memory(buf: &mut Buffer, rect: Rect, app: &App) {
    let mem = &app.memory;
    let color = pressure_color(mem.pressure);
    let pct = fraction(mem.used as f64, mem.total as f64) * 100.0;
    let title = Line::from(vec![
        Span::styled("mem ", BOLD),
        Span::raw(fmt_used_total(mem.used, mem.total)),
        Span::styled(format!(" {pct:.0}%"), BOLD.fg(color)),
    ]);
    // Under pressure the whole box changes colour, to be seen from a distance.
    let border = match mem.pressure {
        Pressure::Warn | Pressure::Critical => color,
        _ => palette::NEUTRAL,
    };
    let mut inner = frame(buf, rect, &title, border);
    if inner.is_empty() {
        return;
    }
    let badge = Line::styled(
        format!(" {} ", mem.pressure.label().to_uppercase()),
        BOLD.fg(color).add_modifier(Modifier::REVERSED),
    );
    let min = title_end(rect, &title);
    border_label(buf, rect.y, rect.right() - 2, min, &badge);

    let detail = if inner.height >= MEM_DETAIL {
        MEM_DETAIL
    } else {
        inner.height.min(3)
    };
    let samples: Vec<(f64, Color)> = app
        .mem_history
        .iter()
        .map(|&(used, pressure)| (f64::from(used), pressure_color(pressure)))
        .collect();
    let chart_rows = inner.height - detail;
    let chart = take(&mut inner, chart_rows);
    charts::graph(buf, chart, &samples, 100.0, None, false);

    if detail < MEM_DETAIL {
        draw_memory_compact(buf, inner, mem);
    } else if inner.width < MEM_TWO_COLUMNS {
        draw_meters(buf, inner, &what_meters(mem), WHAT_LABEL);
    } else {
        // The right column's names are longer, so it gets the odd cells.
        let wide = inner.width >= MEM_LONG_NAMES;
        let who_label = if wide { WHO_LABEL } else { WHAT_LABEL };
        let left = (inner.width - 3 - (who_label - WHAT_LABEL)) / 2;
        let divider = inner.x + left + 1;
        for y in inner.top()..inner.bottom() {
            buf[(divider, y)].set_symbol("│").set_fg(palette::NEUTRAL);
        }
        let what = Rect {
            width: left,
            ..inner
        };
        draw_meters(buf, what, &what_meters(mem), WHAT_LABEL);
        let who = Rect {
            x: divider + 2,
            width: inner.right() - divider - 2,
            ..inner
        };
        draw_meters(buf, who, &who_meters(app, wide), who_label);
    }
}

/// Column widths of a list row. `age` and `ctx` include their leading space.
#[derive(Debug, PartialEq, Eq)]
pub struct Columns {
    pub name: u16,
    pub age: u16,
    pub ctx: u16,
    pub bar: u16,
    pub spark: u16,
}

/// `|dot| name age ctx bar mem spark cpu|`. Only session rows have an age and
/// a context size, and only when there's room.
pub fn columns(width: u16, session: bool) -> Columns {
    let spark = match width {
        56.. => 8,
        44.. => 6,
        36.. => 4,
        _ => 0,
    };
    let age = if session && width >= 44 {
        AGE_WIDTH + 1
    } else {
        0
    };
    let ctx = if session && width >= 56 {
        CTX_WIDTH + 1
    } else {
        0
    };
    let fixed = 2 + 1 + 1 + MEM_WIDTH + 1 + CPU_WIDTH;
    let room = width.saturating_sub(fixed + spark + age + ctx);
    let name = (room * 3 / 5).clamp(room.min(8), 22);
    Columns {
        name,
        age,
        ctx,
        bar: room - name,
        spark,
    }
}

/// How a session is doing, as Claude Code reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Working,
    /// Went idle recently: it's probably waiting on you.
    Ready,
    Idle,
}

/// The state, and how long it has lasted, if Claude Code says.
fn session_state(app: &App, pid: i32) -> Option<(State, Option<u64>)> {
    let meta = app.session_meta.get(&pid)?;
    let secs = meta
        .since_ms
        .map(|since| app.now_ms.saturating_sub(since) / 1000);
    let state = match meta.status? {
        claude::Status::Busy => State::Working,
        claude::Status::Idle if secs.is_some_and(|s| s < READY_SECS) => State::Ready,
        claude::Status::Idle => State::Idle,
    };
    Some((state, secs))
}

fn fmt_age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn fmt_tokens(tokens: u64) -> String {
    match tokens {
        0..1000 => tokens.to_string(),
        1000..999_500 => format!("{:.0}k", tokens as f64 / 1e3),
        _ => format!("{:.1}M", tokens as f64 / 1e6),
    }
}

struct Row<'a> {
    y: u16,
    /// Status mark; None for rows without one.
    mark: Option<(&'static str, Style)>,
    /// Session rows keep columns for these even when they're empty.
    session: bool,
    age: Option<(String, Style)>,
    ctx: Option<String>,
    name: &'a str,
    name_style: Style,
    fill: f64,
    bar_color: Color,
    mem: String,
    history: Option<&'a VecDeque<u8>>,
    /// Shown where the sparkline would be.
    note: Option<String>,
    cpu: String,
    cpu_style: Style,
}

fn put_name(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    let width = width as usize;
    if width == 0 {
        return;
    }
    if Span::raw(text).width() <= width {
        let (end, _) = buf.set_stringn(x, y, text, width, style);
        // Pad, so a selected name reads as a full-width block.
        for x in end..x + width as u16 {
            buf[(x, y)].set_style(style);
        }
    } else {
        // Keep the `#2` that tells same-folder sessions apart.
        let tail = text.rfind('#').map_or("", |i| &text[i..]);
        let room = width.saturating_sub(tail.chars().count() + 1);
        let (end, _) = buf.set_stringn(x, y, text, room, style);
        buf.set_stringn(end, y, format!("…{tail}"), width - room, style);
    }
}

fn put_right(buf: &mut Buffer, end: u16, y: u16, width: u16, text: &str, style: Style) {
    let start = end.saturating_sub(width);
    let pad = width.saturating_sub(text.chars().count() as u16);
    if start + pad < buf.area.right() {
        buf.set_stringn(start + pad, y, text, width as usize, style);
    }
}

fn draw_row(buf: &mut Buffer, area: Rect, row: Row) {
    let c = columns(area.width, row.session);
    let y = row.y;
    if let Some((glyph, style)) = row.mark {
        buf.set_stringn(area.x, y, glyph, 1, style);
    }
    let mut x = area.x + 2;
    put_name(buf, x, y, c.name, row.name, row.name_style);
    x += c.name + c.age;
    if let Some((age, style)) = row.age.filter(|_| c.age > 0) {
        put_right(buf, x, y, AGE_WIDTH, &age, style);
    }
    x += c.ctx;
    if let Some(ctx) = row.ctx.filter(|_| c.ctx > 0) {
        put_right(buf, x, y, CTX_WIDTH, &ctx, Style::new());
    }
    x += 1;
    let bar = Rect {
        x,
        y,
        width: c.bar,
        height: 1,
    };
    if !bar.is_empty() && bar.right() <= area.right() {
        charts::bar(
            buf,
            bar,
            &[segment(row.fill.clamp(0.0, 1.0), row.bar_color)],
            1.0,
            false,
        );
    }
    let end = area.right();
    let spark_end = end.saturating_sub(CPU_WIDTH + 1);
    let mem_end = spark_end.saturating_sub(c.spark);
    put_right(buf, mem_end, y, MEM_WIDTH, &row.mem, Style::new());
    put_right(buf, end, y, CPU_WIDTH, &row.cpu, row.cpu_style);
    if let Some(history) = row.history
        && c.spark > 0
    {
        let samples: Vec<(f64, Color)> = history
            .iter()
            .map(|&pct| (f64::from(pct), row.bar_color))
            .collect();
        let rect = Rect {
            x: spark_end - c.spark + 1,
            y,
            width: c.spark - 1,
            height: 1,
        };
        charts::spark(buf, rect, &samples, 100.0);
    }
    if let Some(note) = row.note
        && c.spark > note.chars().count() as u16
    {
        put_right(buf, spark_end, y, c.spark, &note, DIM);
    }
}

/// Dim when idle, bold when busy, so the eye skips the zeros.
fn cpu_style(pct: Option<f64>) -> Style {
    match pct {
        Some(pct) if pct < 0.5 => DIM,
        Some(pct) if pct >= 50.0 => BOLD,
        _ => Style::new(),
    }
}

/// Column headings in a list panel's top border, over their columns. The
/// sorted one is bold and marked.
fn list_header(buf: &mut Buffer, rect: Rect, title: &Line, sort: SortKey, session: bool) {
    if rect.width < 2 {
        return;
    }
    let c = columns(rect.width - 2, session);
    let label = |text: &str, key: SortKey| {
        if sort == key {
            Line::styled(format!("{text}▾"), BOLD)
        } else {
            Line::styled(text.to_string(), DIM)
        }
    };
    let end = rect.right() - 1;
    let min = title_end(rect, title);
    border_label(buf, rect.y, end, min, &label("cpu", SortKey::Cpu));
    let mem_end = end.saturating_sub(CPU_WIDTH + 1 + c.spark);
    border_label(buf, rect.y, mem_end, min, &label("mem", SortKey::Mem));
    let age_end = rect.x + 1 + 2 + c.name + c.age + 1;
    if c.age > 0 {
        border_label(buf, rect.y, age_end, min, &Line::styled("for", DIM));
    }
    if c.ctx > 0 {
        border_label(buf, rect.y, age_end + c.ctx, min, &Line::styled("ctx", DIM));
    }
}

fn draw_sessions(buf: &mut Buffer, rect: Rect, app: &mut App, with_hints: bool) {
    let lines = app.session_lines();
    let viewport = rect.height.saturating_sub(2) as usize;
    let (first, last) = app.selection_span(&lines);
    app.scroll = app::scroll_offset(app.scroll, first, last, viewport, lines.len());

    let app = &*app;
    let sessions = &app.grouped.sessions;
    let mut title = vec![
        Span::styled("sessions", BOLD),
        Span::raw(format!(" {}", sessions.len())),
    ];
    if lines.len() > viewport && viewport > 0 {
        let end = (app.scroll + viewport).min(lines.len());
        let range = format!(" {}-{}/{}", app.scroll + 1, end, lines.len());
        title.push(Span::styled(range, DIM));
    }
    let title = Line::from(title);
    let area = frame(buf, rect, &title, palette::NEUTRAL);
    if area.is_empty() {
        return;
    }
    list_header(buf, rect, &title, app.sort, true);
    if with_hints {
        draw_hints(buf, rect);
    }

    if sessions.is_empty() {
        let width = area.width.saturating_sub(2) as usize;
        buf.set_stringn(area.x + 2, area.y, "no claude sessions", width, DIM);
    }
    let largest = sessions.iter().map(|s| s.mem).max().unwrap_or(1).max(1) as f64;
    for (line, y) in lines.iter().skip(app.scroll).zip(area.y..area.bottom()) {
        match *line {
            SessionLine::Session(i) => {
                let s = &sessions[i];
                let color = session_color(app, s.pid);
                let history = app.session_history.get(&s.pid);
                let selected = app.selected == Some(s.pid);
                let state = session_state(app, s.pid);
                // Without word from Claude Code, go by recent CPU.
                let look = match state {
                    Some((state, _)) => state,
                    None if history.is_some_and(app::is_active) => State::Working,
                    None => State::Idle,
                };
                let (mark, age_style) = match look {
                    State::Working => (("●", charts::style(palette::GOOD)), Style::new()),
                    State::Ready => {
                        let style = BOLD.fg(palette::WARN);
                        (("◆", style), style)
                    }
                    State::Idle => (("○", DIM), DIM),
                };
                let meta = app.session_meta.get(&s.pid);
                draw_row(
                    buf,
                    area,
                    Row {
                        y,
                        mark: Some(mark),
                        session: true,
                        age: state
                            .and_then(|(_, secs)| secs)
                            .map(|secs| (fmt_age(secs), age_style)),
                        ctx: meta.and_then(|m| m.context).map(fmt_tokens),
                        name: &s.name,
                        name_style: if selected {
                            BOLD.add_modifier(Modifier::REVERSED)
                        } else {
                            Style::new()
                        },
                        fill: s.mem as f64 / largest,
                        bar_color: color,
                        mem: fmt_mem(s.mem),
                        history,
                        note: None,
                        cpu: fmt_cpu(s.cpu),
                        cpu_style: cpu_style(Some(s.cpu)),
                    },
                );
            }
            SessionLine::Tree(i, t) => {
                let s = &sessions[i];
                let r = &s.tree[t];
                draw_row(
                    buf,
                    area,
                    Row {
                        y,
                        mark: None,
                        session: true,
                        age: None,
                        ctx: None,
                        name: &format!("{}{}", r.prefix, r.name),
                        name_style: DIM,
                        fill: r.mem.unwrap_or(0) as f64 / largest,
                        bar_color: session_color(app, s.pid),
                        mem: r.mem.map_or("n/a".into(), fmt_mem),
                        history: None,
                        note: Some(r.pid.to_string()),
                        cpu: r.cpu.map_or("n/a".into(), fmt_cpu),
                        cpu_style: cpu_style(r.cpu),
                    },
                );
            }
        }
    }
}

fn draw_procs(buf: &mut Buffer, rect: Rect, app: &App, with_hints: bool) {
    let mut title = vec![Span::styled("processes", BOLD)];
    if app.grouped.unreadable > 0 {
        title.push(Span::styled(
            format!(" {} n/a", app.grouped.unreadable),
            DIM,
        ));
    }
    let title = Line::from(title);
    let area = frame(buf, rect, &title, palette::NEUTRAL);
    if area.is_empty() {
        return;
    }
    list_header(buf, rect, &title, app.sort, false);
    if with_hints {
        draw_hints(buf, rect);
    }
    let by_mem = others_by_mem(app);
    let largest = by_mem.first().map_or(1, |r| r.mem).max(1) as f64;
    let color = |pid: i32| {
        by_mem
            .iter()
            .take(STRIP_CONSUMERS)
            .zip(palette::CONSUMERS)
            .find(|(r, _)| r.pid == pid)
            .map_or(palette::OTHER, |(_, c)| c)
    };
    for (row, y) in app.grouped.others.iter().zip(area.y..area.bottom()) {
        let name = if row.count > 1 {
            format!("{} ×{}", row.name, row.count)
        } else {
            row.name.to_string()
        };
        draw_row(
            buf,
            area,
            Row {
                y,
                mark: None,
                session: false,
                age: None,
                ctx: None,
                name: &name,
                name_style: Style::new(),
                fill: row.mem as f64 / largest,
                bar_color: color(row.pid),
                mem: fmt_mem(row.mem),
                history: None,
                note: None,
                cpu: fmt_cpu(row.cpu),
                cpu_style: cpu_style(Some(row.cpu)),
            },
        );
    }
}

fn draw_help(buf: &mut Buffer, rect: Rect) {
    let inner = frame(
        buf,
        rect,
        &Line::styled("agentmon help", BOLD),
        palette::NEUTRAL,
    );
    if inner.is_empty() {
        return;
    }
    let close = Line::from(vec![
        Span::styled("any key", BOLD),
        Span::styled(" back", DIM),
    ]);
    border_text(buf, rect.x + 2, rect.bottom() - 1, rect.right() - 1, &close);
    let text = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    Paragraph::new(HELP)
        .wrap(Wrap { trim: false })
        .render(text, buf);
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let mut area = frame.area();
    if area.is_empty() {
        return;
    }
    let buf = frame.buffer_mut();
    if app.help {
        draw_help(buf, area);
        return;
    }
    let most = u16::MAX as usize / 2;
    let sessions = app.session_lines().len().clamp(1, most) as u16;
    let procs = app.grouped.others.len().min(most) as u16;
    let plan = if area.width >= WIDE {
        // Charts left, lists right.
        let mut left = Rect {
            width: area.width / 2,
            ..area
        };
        area.x += left.width;
        area.width -= left.width;
        // Half each; the odd row goes to cpu, for its cores.
        let cpu = if left.height >= 2 * MIN_PANEL + 2 {
            left.height.div_ceil(2)
        } else {
            0
        };
        draw_cpu(buf, take(&mut left, cpu), app);
        draw_memory(buf, left, app);
        let (sessions, procs) = plan_lists(area.height, sessions, procs);
        Plan {
            cpu: 0,
            mem: 0,
            sessions,
            procs,
        }
    } else {
        let plan = plan(area.height, sessions, procs);
        draw_cpu(buf, take(&mut area, plan.cpu), app);
        draw_memory(buf, take(&mut area, plan.mem), app);
        plan
    };
    let rect = take(&mut area, plan.sessions);
    draw_sessions(buf, rect, app, plan.procs == 0);
    draw_procs(buf, take(&mut area, plan.procs), app, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{Kind, Proc};
    use crate::sensors::Sample;
    use crate::sessions;
    use crate::system::{ClusterUsage, CpuUsage, Memory};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_app(n_sessions: i32) -> App {
        let mut procs = vec![];
        let mut add = |pid, ppid, kind, name: &str, mem| {
            procs.push(Proc {
                pid,
                ppid,
                kind,
                name: name.into(),
                app: None,
                mem: Some(mem),
                cpu: Some(12.0),
            });
        };
        add(1, 0, Kind::Other, "launchd", 1 << 20);
        for i in 0..n_sessions {
            let root = 100 + i * 10;
            add(root, 1, Kind::Cli, "claude", (300 + i as u64) << 20);
            add(root + 1, root, Kind::Other, "node", 90 << 20);
        }
        for i in 0..30 {
            let name = format!("a-very-long-process-name-from-some-app-{i}");
            add(5000 + i, 1, Kind::Other, &name, (i as u64) << 22);
        }
        let mut grouped = sessions::group(&procs);
        crate::app::label_sessions(&mut grouped.sessions, |pid| {
            Some(format!("/Users/me/code/project-with-a-long-name-{pid}"))
        });
        let mut app = App::default();
        for _ in 0..3 {
            app.ingest(
                Memory {
                    total: 48 << 30,
                    used: 35 << 30,
                    compressed: 9 << 30,
                    swap_total: 8 << 30,
                    swap_used: 6 << 30,
                    wired: 5 << 30,
                    app: 21 << 30,
                    cached: 8 << 30,
                    pressure: Pressure::Warn,
                },
                CpuUsage {
                    overall: 22.0,
                    clusters: vec![
                        ClusterUsage {
                            label: "P".into(),
                            pct: 17.0,
                            cores: vec![17.0; 4],
                        },
                        ClusterUsage {
                            label: "S".into(),
                            pct: 33.0,
                            cores: vec![],
                        },
                    ],
                },
                Some(Sample {
                    cpu: None,
                    gpu_pct: Some(4.0),
                    temp_c: Some(62.0),
                    watts: Some(19.0),
                    cpu_watts: Some(7.5),
                    fan_rpm: Some(1300.0),
                    ..Sample::default()
                }),
                sessions::group(&procs),
                procs.len(),
            );
            crate::app::label_sessions(&mut app.grouped.sessions, |pid| {
                Some(format!("/Users/me/code/project-with-a-long-name-{pid}"))
            });
        }
        app
    }

    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn plan_uses_every_row_at_side_pane_size() {
        let p = plan(50, 8, 200);
        assert_eq!(p.rows(), 50);
        assert_eq!(p.sessions, 10);
        assert!(p.cpu > 7 && p.mem == 12 && p.procs > 7, "{p:?}");
    }

    #[test]
    fn plan_shrinks_graphs_before_lists() {
        let p = plan(30, 8, 200);
        assert_eq!(p.rows(), 30);
        assert_eq!(p.sessions, 10);
        assert!(p.cpu < 7 && p.mem < 11, "{p:?}");
        // Still the full memory breakdown.
        assert!(p.mem >= 2 + MEM_DETAIL);
    }

    #[test]
    fn plan_short_pane_keeps_every_panel_but_smaller() {
        let p = plan(24, 8, 200);
        assert_eq!(p.rows(), 24);
        assert_eq!((p.cpu, p.mem, p.sessions, p.procs), (4, 5, 10, 5));
    }

    #[test]
    fn plan_grows_graphs_and_processes_in_a_tall_pane() {
        let p = plan(70, 5, 8);
        assert_eq!(p.rows(), 70);
        assert_eq!((p.sessions, p.procs), (7, 10));
        // The spare rows go to cpu; memory keeps its graph as it is.
        assert_eq!((p.cpu, p.mem), (41, 12));
    }

    #[test]
    fn plan_fits_at_every_height_with_no_stub_panels() {
        for h in 1..80 {
            for sessions in [1, 14, 200] {
                let p = plan(h, sessions, 300);
                assert_eq!(p.rows(), h, "sessions={sessions}: {p:?}");
                // Sessions stop at half the pane while there's more to show.
                assert!(h < 30 || p.sessions <= h / 2, "h={h}: {p:?}");
                for part in [p.cpu, p.mem, p.procs] {
                    assert!(part == 0 || part >= MIN_PANEL, "h={h}: {p:?}");
                }
                assert!(p.sessions > 0);
            }
        }
    }

    #[test]
    fn plan_tolerates_nothing_to_show() {
        let p = plan(40, 1, 0);
        assert_eq!(p.procs, 0);
        assert!(p.rows() <= 40);
    }

    #[test]
    fn lists_share_a_column() {
        assert_eq!(plan_lists(40, 8, 200), (10, 30));
        assert_eq!(plan_lists(12, 20, 200), (6, 6));
        assert_eq!(plan_lists(41, 30, 200), (20, 21));
        assert_eq!(plan_lists(6, 20, 200), (6, 0));
        assert_eq!(plan_lists(40, 8, 0), (40, 0));
    }

    #[test]
    fn hints_drop_the_least_needed_first() {
        assert_eq!(
            hints(60).to_string(),
            "↑↓ select  ⏎ tree  s sort  ? help  q quit"
        );
        assert_eq!(hints(20).to_string(), "? help  q quit");
        assert_eq!(hints(3).to_string(), "");
    }

    #[test]
    fn panel_titles_and_labels_never_collide() {
        let rect = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(rect);
        let title = Line::raw("title");
        let inner = frame(&mut buf, rect, &title, palette::NEUTRAL);
        assert_eq!(inner, Rect::new(1, 1, 18, 1));
        let min = title_end(rect, &title);
        assert!(!border_label(
            &mut buf,
            0,
            18,
            min,
            &Line::raw("too long here")
        ));
        assert!(border_label(&mut buf, 0, 18, min, &Line::raw("ok")));
        let top: String = (0..20).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(top, "╭─ title ───── ok ─╮");
        assert!(frame(&mut buf, Rect::new(0, 0, 1, 1), &title, palette::NEUTRAL).is_empty());
    }

    #[test]
    fn columns_fit_each_width() {
        for width in [40, 44, 56, 58, 70, 120] {
            for session in [false, true] {
                let c = columns(width, session);
                let used = 2 + c.name + c.age + c.ctx + 1 + c.bar + 1;
                let used = used + MEM_WIDTH + c.spark + 1 + CPU_WIDTH;
                assert!(used <= width, "{width}: {c:?}");
                assert!(c.name >= 8 && c.bar >= 4, "{width}: {c:?}");
            }
        }
        assert_eq!(columns(20, true).spark, 0);
        // Sessions and processes keep mem and cpu in line.
        assert_eq!(columns(58, true).spark, columns(58, false).spark);
        assert_eq!((columns(40, true).age, columns(50, true).ctx), (0, 0));
    }

    #[test]
    fn sessions_show_what_claude_code_reports() {
        use crate::claude::{Meta, Status};
        let mut app = sample_app(3);
        app.now_ms = 10_000_000;
        let pids: Vec<i32> = app.grouped.sessions.iter().map(|s| s.pid).collect();
        let meta = |status, ago_secs: u64, context| Meta {
            name: None,
            status: Some(status),
            since_ms: Some(app.now_ms - ago_secs * 1000),
            context,
        };
        let metas = [
            meta(Status::Busy, 75, Some(212_400)),
            meta(Status::Idle, 120, Some(1_250_000)),
            meta(Status::Idle, 7200, None),
        ];
        app.session_meta = pids.iter().copied().zip(metas).collect();
        let out = render(&mut app, 60, 50);
        let top = find(&out, "sessions 3");
        assert!(
            out[top].contains("for") && out[top].contains("ctx"),
            "{}",
            out[top]
        );
        let rows = &out[top + 1..top + 4];
        assert!(
            rows[0].starts_with("│●") && rows[0].contains(" 1m 212k "),
            "{}",
            rows[0]
        );
        assert!(
            rows[1].starts_with("│◆") && rows[1].contains(" 2m 1.2M "),
            "{}",
            rows[1]
        );
        assert!(
            rows[2].starts_with("│○") && rows[2].contains(" 2h "),
            "{}",
            rows[2]
        );
    }

    #[test]
    fn ages_and_tokens() {
        assert_eq!(fmt_age(59), "59s");
        assert_eq!(fmt_age(3599), "59m");
        assert_eq!(fmt_age(86_399), "23h");
        assert_eq!(fmt_age(86_400 * 12), "12d");
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(211_735), "212k");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn strip_never_overruns_used_memory() {
        let mut app = sample_app(4);
        app.memory.used = 1 << 30;
        app.memory.wired = 1 << 28;
        let list = consumers(&app);
        let total: f64 = list.iter().map(|c| c.share).sum();
        assert!((total - (1u64 << 30) as f64).abs() < 1.0, "{total}");
        assert_eq!(list.last().unwrap().label, "system");
        assert_eq!(list.last().unwrap().bytes, 1 << 28);
    }

    #[test]
    fn strip_lists_sessions_largest_first_then_consumers() {
        let list = consumers(&sample_app(3));
        assert!(list[..3].iter().all(|c| c.session));
        assert!(list[0].bytes >= list[1].bytes);
        assert!(!list[3].session);
        assert_eq!(list[list.len() - 2].label, "other");
    }

    #[test]
    fn short_app_names() {
        assert_eq!(short_app_name("Google Chrome"), "Chrome");
        assert_eq!(short_app_name("Finder"), "Finder");
    }

    #[test]
    fn legend_stops_at_its_rows() {
        let items: Vec<(String, Color)> = (0..5)
            .map(|i| (format!("item-{i}"), palette::APP))
            .collect();
        let lines = legend_lines(&items, 16, 2);
        assert_eq!(lines.len(), 2);
        let text = |line: &Line| line.to_string();
        assert_eq!(text(&lines[0]), "■item-0 ■item-1 ");
        assert!(legend_lines(&items, 3, 2).is_empty());
    }

    #[test]
    fn long_same_folder_names_keep_their_number() {
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        put_name(&mut buf, 0, 0, 8, "sample-project#2", Style::new());
        let text: String = (0..8).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(text, "sampl…#2");
    }

    #[test]
    fn mem_formatting() {
        assert_eq!(fmt_mem(512 << 20), "512M");
        assert_eq!(fmt_mem(3 << 29), "1.5G");
        assert_eq!(fmt_mem(120 << 30), "120G");
        assert_eq!(fmt_used_total(35 << 30, 48 << 30), "35.0/48.0G");
        assert_eq!(fmt_used_total(512 << 20, 8 << 30), "512M/8.0G");
    }

    /// A cell of `glyph` anywhere in the frame, as (x, y).
    fn find(out: &[String], text: &str) -> usize {
        out.iter()
            .position(|l| l.contains(text))
            .unwrap_or_else(|| panic!("{text}: {out:#?}"))
    }

    #[test]
    fn cores_get_a_row_each_or_share_two_columns() {
        let mut app = sample_app(4);
        app.cpu.clusters[1].cores = vec![50.0, 90.0];
        for _ in 0..2 {
            let (memory, cpu) = (app.memory.clone(), app.cpu.clone());
            let grouped = std::mem::take(&mut app.grouped);
            app.ingest(memory, cpu, app.sensors.clone(), grouped, 0);
        }
        let draw = |app: &App, height| {
            let rect = Rect::new(0, 0, 60, height);
            let mut buf = Buffer::empty(rect);
            draw_cpu(&mut buf, rect, app);
            (0..height)
                .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<String>>()
        };
        // Room for all six under the total, above the sensors.
        let out = draw(&app, 12);
        let row = find(&out, "P0 ");
        assert!(out[row - 1].contains("CPU") && out[row].ends_with(" 17%│"));
        assert!(out[row + 5].contains("│ S1 ") && out[row + 5].ends_with(" 90%│"));
        assert!(out[row + 5].contains('⣿'), "{out:#?}");
        assert!(out[row + 6].contains("GPU"));
        // Half the room: two columns.
        let out = draw(&app, 9);
        let row = find(&out, "P0 ");
        assert!(out[row].contains(" P3 ") && out[row + 2].contains(" S1 "));
        assert!(out[row + 3].contains("GPU"), "{out:#?}");
        // Less still: clusters, one bar per core.
        let out = draw(&app, 7);
        assert!(!out.iter().any(|l| l.contains("P0")), "{out:#?}");
        find(&out, "│ S ");
    }

    #[test]
    fn renders_at_side_pane_width() {
        let mut app = sample_app(4);
        let out = render(&mut app, 60, 50);
        assert!(out[0].starts_with("╭─ cpu 22% ─"), "{}", out[0]);
        assert!(out[0].contains("GPU 4% · 62°C · 19W"));
        // Mirrored cluster graphs beside the figures.
        let row = find(&out, "CPU ");
        assert!(out[row].starts_with("│P") && out[row].contains("│ CPU"));
        assert!(out[row].ends_with("22%│"), "{}", out[row]);
        assert!(out[row + 1].ends_with("17%│") && out[row + 2].ends_with("33%│"));
        // Then GPU, power and temperature, and the detail lines.
        assert!(out[row + 3].contains("GPU") && out[row + 3].ends_with("4%│"));
        assert!(out[row + 4].contains("PWR") && out[row + 4].ends_with("19W│"));
        assert!(out[row + 5].contains("TMP") && out[row + 5].ends_with("62°│"));
        find(&out, "cpu 7.5W");
        find(&out, "fan 1300rpm");
        let mem = find(&out, "mem 35.0/48.0G 73%");
        assert!(out[mem - 2].starts_with("│S"), "{}", out[mem - 2]);
        assert!(out[mem].contains(" WARN "));
        // What it's made of beside who has it.
        let row = find(&out, "app ");
        assert!(out[row].contains("│ Claude Code "), "{}", out[row]);
        let row = find(&out, "swap ");
        assert!(out[row].contains("system "), "{}", out[row]);
        let row = find(&out, "sessions 4");
        assert!(out[row].contains("mem▾") && out[row].contains("cpu"));
        find(&out, "processes");
        assert!(out[49].contains("? help  q quit"), "{}", out[49]);
    }

    #[test]
    fn wide_puts_charts_beside_lists() {
        let mut app = sample_app(4);
        let out = render(&mut app, 160, 40);
        assert!(out[0].contains("cpu 22%") && out[0].contains("sessions 4"));
        let row = find(&out, "mem 35.0/48.0G");
        assert!(out[row].contains("│  a-very-long"), "{}", out[row]);
        find(&out, "processes");
    }

    #[test]
    fn pressure_colours_the_memory_box() {
        let mut app = sample_app(1);
        let mut term = Terminal::new(TestBackend::new(60, 50)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let out = render(&mut app, 60, 50);
        let y = find(&out, "mem 35.0") as u16;
        assert_eq!(term.backend().buffer()[(0, y)].fg, palette::WARN);
        assert_eq!(term.backend().buffer()[(0, 0)].fg, palette::NEUTRAL);
    }

    #[test]
    fn nothing_is_hardcoded_to_a_theme_colour() {
        let mut app = sample_app(4);
        let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let area = *term.backend().buffer().area();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &term.backend().buffer()[(x, y)];
                for color in [cell.fg, cell.bg] {
                    assert!(
                        matches!(color, Color::Reset | Color::Indexed(16..)),
                        "({x},{y}) {color:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn narrow_width_keeps_every_number() {
        let mut app = sample_app(4);
        app.toggle_expanded();
        let out = render(&mut app, 40, 24);
        find(&out, "mem 35.0/48.0G 73%");
        assert!(out[0].contains("cpu 22%"));
        find(&out, "P 17%");
        find(&out, "swap");
        let row = find(&out, "project-");
        assert!(out[row].contains('…'));
        assert!(out[row].contains("39"), "{}", out[row]);
        find(&out, "└ node");
    }

    #[test]
    fn scrolls_to_selection_in_a_short_pane() {
        let mut app = sample_app(20);
        for _ in 0..19 {
            app.move_selection(true);
        }
        let selected = app.selected.unwrap();
        let name = app
            .grouped
            .sessions
            .iter()
            .find(|s| s.pid == selected)
            .unwrap()
            .mem;
        let out = render(&mut app, 60, 20).join("\n");
        assert!(out.contains("sessions 20"));
        assert!(out.contains(&fmt_mem(name)));
    }

    #[test]
    fn missing_macmon_and_no_sessions_render() {
        let mut app = sample_app(0);
        app.sensors = None;
        let text = render(&mut app, 60, 40).join("\n");
        assert!(text.contains(NO_SENSORS));
        assert!(text.contains("no claude sessions"));
    }

    #[test]
    fn never_panics_at_any_size() {
        for sessions in [0, 3, 25] {
            let mut app = sample_app(sessions);
            app.toggle_expanded();
            for (w, h) in [(1, 1), (5, 2), (12, 4), (20, 8), (25, 60), (200, 5), (0, 0)] {
                render(&mut app, w.max(1), h.max(1));
                app.help = true;
                render(&mut app, w.max(1), h.max(1));
                app.help = false;
            }
        }
    }

    #[test]
    fn help_explains_memory() {
        let mut app = sample_app(0);
        app.help = true;
        let text = render(&mut app, 60, 40).join("\n");
        assert!(text.contains("phys_footprint"));
    }
}
