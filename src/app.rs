//! UI-independent state: latest data, history, selection.
use crate::claude;
use crate::sensors;
use crate::sessions::{self, Grouped, Session, SortKey};
use crate::system::{CpuUsage, Memory, Pressure};
use std::collections::{HashMap, HashSet, VecDeque};

pub const HISTORY: usize = 256;
/// Per-session and per-core CPU samples; rows draw far fewer.
pub const SESSION_HISTORY: usize = 32;
/// A session is active if it reached this much of a core in the recent window.
const ACTIVE_CPU: u8 = 5;
const ACTIVE_WINDOW: usize = 3;

#[derive(Default)]
pub struct App {
    pub sort: SortKey,
    pub help: bool,
    pub memory: Memory,
    pub cpu: CpuUsage,
    /// None once the source is missing, dead or stale.
    pub sensors: Option<sensors::Sample>,
    /// Used percent, with the pressure at that moment.
    pub mem_history: VecDeque<(u8, Pressure)>,
    /// Whole-system watts, and CPU temperature, while the sensors report them.
    pub power_history: VecDeque<f32>,
    pub temp_history: VecDeque<f32>,
    /// Per cluster, by label.
    pub cluster_history: HashMap<String, VecDeque<u8>>,
    /// Per core, in cluster order.
    pub core_history: Vec<VecDeque<u8>>,
    /// CPU percent by session root pid, pruned with the session.
    pub session_history: HashMap<i32, VecDeque<u8>>,
    /// What Claude Code reports for each session, by root pid.
    pub session_meta: HashMap<i32, claude::Meta>,
    /// Wall clock at the last tick, in epoch milliseconds.
    pub now_ms: u64,
    /// Colour slot by session root pid, held for the session's life.
    pub session_slots: HashMap<i32, usize>,
    pub grouped: Grouped,
    pub total_procs: usize,
    /// Root pid of the selected session.
    pub selected: Option<i32>,
    selected_index: usize,
    pub expanded: HashSet<i32>,
    pub scroll: usize,
}

/// One line of the sessions list.
pub enum SessionLine {
    /// Index into `grouped.sessions`.
    Session(usize),
    /// Session index, tree row index.
    Tree(usize, usize),
}

fn push<T>(history: &mut VecDeque<T>, value: T, capacity: usize) {
    if history.len() == capacity {
        history.pop_front();
    }
    history.push_back(value);
}

fn percent(value: f64) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

/// The lowest colour slot no live session holds.
fn free_slot(slots: &HashMap<i32, usize>) -> usize {
    (0..)
        .find(|slot| !slots.values().any(|s| s == slot))
        .unwrap_or(0)
}

/// Whether recent samples show the session doing anything.
pub fn is_active(history: &VecDeque<u8>) -> bool {
    history
        .iter()
        .rev()
        .take(ACTIVE_WINDOW)
        .any(|&c| c >= ACTIVE_CPU)
}

/// Names sessions by folder. Sessions sharing a folder keep separate rows,
/// numbered oldest first after the first: `webapp`, `webapp#2`.
pub fn label_sessions(sessions: &mut [Session], cwd: impl Fn(i32) -> Option<String>) {
    for s in sessions.iter_mut() {
        s.name = match cwd(s.pid) {
            Some(path) => sessions::folder_name(&path).to_string(),
            None => format!("pid {}", s.pid),
        };
    }
    let mut order: Vec<usize> = (0..sessions.len()).collect();
    order.sort_by_key(|&i| sessions[i].pid);
    let mut seen: HashMap<String, usize> = HashMap::new();
    for i in order {
        let count = seen.entry(sessions[i].name.clone()).or_default();
        *count += 1;
        if *count > 1 {
            sessions[i].name = format!("{}#{count}", sessions[i].name);
        }
    }
}

impl App {
    pub fn ingest(
        &mut self,
        memory: Memory,
        cpu: CpuUsage,
        sample: Option<sensors::Sample>,
        grouped: Grouped,
        total_procs: usize,
    ) {
        if memory.total > 0 {
            let used = percent(memory.used as f64 / memory.total as f64 * 100.0);
            push(&mut self.mem_history, (used, memory.pressure), HISTORY);
        }
        for cluster in &cpu.clusters {
            let history = self
                .cluster_history
                .entry(cluster.label.clone())
                .or_default();
            push(history, percent(cluster.pct), HISTORY);
        }
        let cores = cpu.clusters.iter().flat_map(|c| &c.cores);
        self.core_history
            .resize(cores.clone().count(), VecDeque::new());
        for (history, &pct) in self.core_history.iter_mut().zip(cores) {
            push(history, percent(pct), SESSION_HISTORY);
        }
        if let Some(sample) = &sample {
            if let Some(watts) = sample.watts {
                push(&mut self.power_history, watts as f32, HISTORY);
            }
            if let Some(temp) = sample.temp_c {
                push(&mut self.temp_history, temp as f32, HISTORY);
            }
        }
        self.memory = memory;
        self.cpu = cpu;
        self.sensors = sample;
        self.grouped = grouped;
        self.total_procs = total_procs;
        self.apply_sort();
        let pids: HashSet<i32> = self.grouped.sessions.iter().map(|s| s.pid).collect();
        self.expanded.retain(|pid| pids.contains(pid));
        self.session_history.retain(|pid, _| pids.contains(pid));
        self.session_slots.retain(|pid, _| pids.contains(pid));
        for session in &self.grouped.sessions {
            let history = self.session_history.entry(session.pid).or_default();
            push(history, percent(session.cpu), SESSION_HISTORY);
            if !self.session_slots.contains_key(&session.pid) {
                let slot = free_slot(&self.session_slots);
                self.session_slots.insert(session.pid, slot);
            }
        }
        self.sync_selection();
    }

    fn apply_sort(&mut self) {
        sessions::sort_sessions(&mut self.grouped.sessions, self.sort);
        sessions::sort_rows(&mut self.grouped.others, self.sort);
    }

    /// Follows the selected pid through re-sorts; if it exited, keeps the slot.
    fn sync_selection(&mut self) {
        let list = &self.grouped.sessions;
        if let Some(i) = list.iter().position(|s| Some(s.pid) == self.selected) {
            self.selected_index = i;
        } else {
            self.selected_index = self.selected_index.min(list.len().saturating_sub(1));
            self.selected = list.get(self.selected_index).map(|s| s.pid);
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.apply_sort();
        self.sync_selection();
    }

    pub fn move_selection(&mut self, down: bool) {
        let len = self.grouped.sessions.len();
        if len == 0 {
            return;
        }
        self.selected_index = if down {
            (self.selected_index + 1).min(len - 1)
        } else {
            self.selected_index.saturating_sub(1)
        };
        self.selected = Some(self.grouped.sessions[self.selected_index].pid);
    }

    pub fn toggle_expanded(&mut self) {
        if let Some(pid) = self.selected
            && !self.expanded.remove(&pid)
        {
            self.expanded.insert(pid);
        }
    }

    pub fn session_lines(&self) -> Vec<SessionLine> {
        let mut lines = Vec::new();
        for (i, session) in self.grouped.sessions.iter().enumerate() {
            lines.push(SessionLine::Session(i));
            if self.expanded.contains(&session.pid) {
                lines.extend((0..session.tree.len()).map(|row| SessionLine::Tree(i, row)));
            }
        }
        lines
    }

    /// (line of the selected session, last line of its block).
    pub fn selection_span(&self, lines: &[SessionLine]) -> (usize, usize) {
        let mut first = 0;
        let mut last = 0;
        for (at, line) in lines.iter().enumerate() {
            let session = match line {
                SessionLine::Session(i) | SessionLine::Tree(i, _) => *i,
            };
            if session == self.selected_index {
                if matches!(line, SessionLine::Session(_)) {
                    first = at;
                }
                last = at;
            }
        }
        (first, last)
    }
}

/// Keeps the selected line in view, then as much of its block as fits.
pub fn scroll_offset(
    prev: usize,
    first: usize,
    last: usize,
    viewport: usize,
    total: usize,
) -> usize {
    if viewport == 0 {
        return 0;
    }
    let mut top = prev.min(total.saturating_sub(viewport));
    if last >= top + viewport {
        top = last + 1 - viewport;
    }
    if first < top {
        top = first;
    }
    top
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{Kind, Proc};

    fn grouped(roots: &[(i32, u64)]) -> Grouped {
        let mut procs: Vec<Proc> = Vec::new();
        for &(pid, mem) in roots {
            for (pid, ppid) in [(pid, 1), (pid + 1, pid)] {
                procs.push(Proc {
                    pid,
                    ppid,
                    kind: if ppid == 1 { Kind::Cli } else { Kind::Other },
                    name: "x".into(),
                    app: None,
                    mem: Some(mem),
                    cpu: Some(0.0),
                });
            }
        }
        sessions::group(&procs)
    }

    fn app(roots: &[(i32, u64)]) -> App {
        let mut app = App::default();
        app.ingest(
            Memory::default(),
            CpuUsage::default(),
            None,
            grouped(roots),
            0,
        );
        app
    }

    #[test]
    fn selection_follows_pid_across_resort() {
        let mut a = app(&[(10, 5), (20, 3)]);
        a.move_selection(true);
        assert_eq!(a.selected, Some(20));
        a.ingest(
            Memory::default(),
            CpuUsage::default(),
            None,
            grouped(&[(10, 1), (20, 9)]),
            0,
        );
        assert_eq!(a.selected, Some(20));
        a.move_selection(true);
        assert_eq!(a.selected, Some(10));
    }

    #[test]
    fn selection_survives_exit_and_empties() {
        let mut a = app(&[(10, 5), (20, 3)]);
        a.move_selection(true);
        let empty = |a: &mut App, g| {
            a.ingest(Memory::default(), CpuUsage::default(), None, g, 0);
        };
        empty(&mut a, grouped(&[(10, 5)]));
        assert_eq!(a.selected, Some(10));
        empty(&mut a, grouped(&[]));
        assert_eq!(a.selected, None);
        a.move_selection(true);
        a.toggle_expanded();
        assert!(a.expanded.is_empty());
    }

    #[test]
    fn expanding_adds_tree_lines_and_exit_clears_it() {
        let mut a = app(&[(10, 5), (20, 3)]);
        assert_eq!(a.session_lines().len(), 2);
        a.toggle_expanded();
        let lines = a.session_lines();
        assert_eq!(lines.len(), 4);
        assert_eq!(a.selection_span(&lines), (0, 2));
        a.move_selection(true);
        assert_eq!(a.selection_span(&a.session_lines()), (3, 3));
        a.ingest(
            Memory::default(),
            CpuUsage::default(),
            None,
            grouped(&[(20, 3)]),
            0,
        );
        assert!(a.expanded.is_empty());
    }

    #[test]
    fn same_folder_keeps_two_labelled_rows() {
        let mut g = grouped(&[(10, 5), (20, 5), (30, 5)]);
        label_sessions(&mut g.sessions, |pid| {
            Some(if pid == 30 { "/a/other" } else { "/a/same" }.into())
        });
        let names: Vec<&str> = g.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["same", "same#2", "other"]);
    }

    #[test]
    fn missing_cwd_falls_back_to_pid() {
        let mut g = grouped(&[(10, 5)]);
        label_sessions(&mut g.sessions, |_| None);
        assert_eq!(g.sessions[0].name, "pid 10");
    }

    #[test]
    fn history_is_bounded() {
        let mut h = VecDeque::new();
        for _ in 0..HISTORY + 5 {
            push(&mut h, percent(150.0), HISTORY);
        }
        assert_eq!(h.len(), HISTORY);
        assert_eq!(h.back(), Some(&100));
    }

    #[test]
    fn session_state_is_bounded_and_pruned() {
        let mut a = app(&[(10, 5), (20, 3)]);
        for _ in 0..SESSION_HISTORY * 2 {
            a.ingest(
                Memory::default(),
                CpuUsage::default(),
                None,
                grouped(&[(10, 5), (20, 3)]),
                0,
            );
        }
        assert_eq!(a.session_history[&10].len(), SESSION_HISTORY);
        a.ingest(
            Memory::default(),
            CpuUsage::default(),
            None,
            grouped(&[(20, 3)]),
            0,
        );
        assert!(!a.session_history.contains_key(&10));
        assert!(!a.session_slots.contains_key(&10));
    }

    #[test]
    fn colour_slots_are_stable_and_reused() {
        let mut a = app(&[(10, 5), (20, 3)]);
        let (ten, twenty) = (a.session_slots[&10], a.session_slots[&20]);
        assert_ne!(ten, twenty);
        // Re-sorting moves rows but not colours.
        let ingest = |a: &mut App, g| {
            a.ingest(Memory::default(), CpuUsage::default(), None, g, 0);
        };
        ingest(&mut a, grouped(&[(10, 1), (20, 9)]));
        assert_eq!(a.session_slots[&10], ten);
        ingest(&mut a, grouped(&[(20, 9)]));
        ingest(&mut a, grouped(&[(20, 9), (30, 2)]));
        assert_eq!(a.session_slots[&20], twenty);
        assert_eq!(a.session_slots[&30], ten);
    }

    #[test]
    fn activity_looks_at_recent_samples_only() {
        let h = |v: &[u8]| v.iter().copied().collect::<VecDeque<u8>>();
        assert!(!is_active(&h(&[])));
        assert!(!is_active(&h(&[90, 1, 2, 4])));
        assert!(is_active(&h(&[0, 0, 5, 1])));
    }

    #[test]
    fn scrolling() {
        assert_eq!(scroll_offset(0, 2, 2, 5, 10), 0);
        assert_eq!(scroll_offset(0, 7, 7, 5, 10), 3);
        assert_eq!(scroll_offset(5, 2, 2, 5, 10), 2);
        // Block taller than the viewport: the selected line wins.
        assert_eq!(scroll_offset(0, 3, 20, 5, 30), 3);
        // Shrinking content doesn't strand the view.
        assert_eq!(scroll_offset(8, 0, 0, 5, 3), 0);
        assert_eq!(scroll_offset(1, 0, 0, 0, 3), 0);
    }
}
