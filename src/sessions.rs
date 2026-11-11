//! Groups the process table into Claude sessions and everything else.
use crate::procs::{Kind, Proc};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Mem,
    Cpu,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            SortKey::Mem => SortKey::Cpu,
            SortKey::Cpu => SortKey::Mem,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TreeRow {
    pub pid: i32,
    pub name: Rc<str>,
    /// Box-drawing indent; empty for the root.
    pub prefix: String,
    pub mem: Option<u64>,
    pub cpu: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub pid: i32,
    /// Project folder; filled in by the caller.
    pub name: String,
    pub mem: u64,
    pub cpu: f64,
    pub count: usize,
    /// Root first, depth-first.
    pub tree: Vec<TreeRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub name: Rc<str>,
    pub pid: i32,
    pub mem: u64,
    pub cpu: f64,
    pub count: usize,
}

#[derive(Debug, Default)]
pub struct Grouped {
    pub sessions: Vec<Session>,
    pub others: Vec<Row>,
    /// Non-session processes whose stats can't be read without root.
    pub unreadable: usize,
}

const MAX_DEPTH: usize = 64;

/// Walks up from each CLI process; the session root is the topmost CLI.
fn is_root(start: usize, procs: &[Proc], by_pid: &HashMap<i32, usize>) -> bool {
    let mut at = start;
    for _ in 0..MAX_DEPTH {
        let Some(&parent) = by_pid.get(&procs[at].ppid) else {
            return true;
        };
        if parent == at {
            return true;
        }
        if procs[parent].kind == Kind::Cli {
            return false;
        }
        at = parent;
    }
    true
}

fn walk(
    at: usize,
    prefix: &str,
    procs: &[Proc],
    children: &HashMap<i32, Vec<usize>>,
    seen: &mut [bool],
    session: &mut Session,
) {
    let kids: Vec<usize> = children
        .get(&procs[at].pid)
        .map(|kids| kids.iter().copied().filter(|&k| !seen[k]).collect())
        .unwrap_or_default();
    for (i, &kid) in kids.iter().enumerate() {
        if std::mem::replace(&mut seen[kid], true) {
            continue;
        }
        let last = i + 1 == kids.len();
        add(
            kid,
            format!("{prefix}{}", if last { "└ " } else { "├ " }),
            procs,
            session,
        );
        let deeper = format!("{prefix}{}", if last { "  " } else { "│ " });
        walk(kid, &deeper, procs, children, seen, session);
    }
}

fn add(at: usize, prefix: String, procs: &[Proc], session: &mut Session) {
    let proc = &procs[at];
    session.mem += proc.mem.unwrap_or(0);
    session.cpu += proc.cpu.unwrap_or(0.0);
    session.count += 1;
    session.tree.push(TreeRow {
        pid: proc.pid,
        name: proc.name.clone(),
        prefix,
        mem: proc.mem,
        cpu: proc.cpu,
    });
}

/// Assigns each process to at most one session, so nothing is double-counted.
/// Children reparented to launchd no longer have a path to a session.
pub fn group(procs: &[Proc]) -> Grouped {
    let by_pid: HashMap<i32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    for (i, proc) in procs.iter().enumerate() {
        if proc.ppid != proc.pid {
            children.entry(proc.ppid).or_default().push(i);
        }
    }
    for kids in children.values_mut() {
        kids.sort_unstable_by_key(|&k| procs[k].pid);
    }

    let mut seen = vec![false; procs.len()];
    let mut sessions = Vec::new();
    for (i, proc) in procs.iter().enumerate() {
        if proc.kind != Kind::Cli || !is_root(i, procs, &by_pid) {
            continue;
        }
        let mut session = Session {
            pid: proc.pid,
            name: String::new(),
            mem: 0,
            cpu: 0.0,
            count: 0,
            tree: Vec::new(),
        };
        seen[i] = true;
        add(i, String::new(), procs, &mut session);
        walk(i, "", procs, &children, &mut seen, &mut session);
        sessions.push(session);
    }

    let mut others = Vec::new();
    let mut apps: HashMap<Rc<str>, Row> = HashMap::new();
    let mut unreadable = 0;
    for (i, proc) in procs.iter().enumerate() {
        if seen[i] {
            continue;
        }
        let (Some(mem), Some(cpu)) = (proc.mem, proc.cpu) else {
            unreadable += 1;
            continue;
        };
        if let Some(app) = &proc.app {
            let row = apps.entry(app.clone()).or_insert_with(|| Row {
                name: app.clone(),
                pid: proc.pid,
                mem: 0,
                cpu: 0.0,
                count: 0,
            });
            row.pid = row.pid.min(proc.pid);
            row.mem += mem;
            row.cpu += cpu;
            row.count += 1;
        } else {
            others.push(Row {
                name: proc.name.clone(),
                pid: proc.pid,
                mem,
                cpu,
                count: 1,
            });
        }
    }
    others.extend(apps.into_values());
    Grouped {
        sessions,
        others,
        unreadable,
    }
}

fn compare(key: SortKey, a: (u64, f64, i32), b: (u64, f64, i32)) -> Ordering {
    let primary = match key {
        SortKey::Mem => b.0.cmp(&a.0),
        SortKey::Cpu => b.1.total_cmp(&a.1),
    };
    primary.then(a.2.cmp(&b.2))
}

pub fn sort_sessions(sessions: &mut [Session], key: SortKey) {
    sessions.sort_by(|a, b| compare(key, (a.mem, a.cpu, a.pid), (b.mem, b.cpu, b.pid)));
}

pub fn sort_rows(rows: &mut [Row], key: SortKey) {
    rows.sort_by(|a, b| compare(key, (a.mem, a.cpu, a.pid), (b.mem, b.cpu, b.pid)));
}

/// Basename of a cwd, keeping `/` itself.
pub fn folder_name(cwd: &str) -> &str {
    match cwd.trim_end_matches('/').rsplit('/').next() {
        Some("") | None => "/",
        Some(name) => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: i32, ppid: i32, kind: Kind, mem: u64) -> Proc {
        Proc {
            pid,
            ppid,
            kind,
            name: format!("p{pid}").into(),
            app: None,
            mem: Some(mem),
            cpu: Some(1.0),
        }
    }

    const CLI: Kind = Kind::Cli;
    const OTHER: Kind = Kind::Other;

    #[test]
    fn descendants_roll_up_to_session() {
        let g = group(&[
            proc(1, 0, OTHER, 1),
            proc(10, 1, OTHER, 1), // terminal
            proc(11, 10, CLI, 100),
            proc(12, 11, OTHER, 20), // mcp server
            proc(13, 12, OTHER, 3),  // its shell
        ]);
        assert_eq!(g.sessions.len(), 1);
        let s = &g.sessions[0];
        assert_eq!((s.pid, s.mem, s.count), (11, 123, 3));
        assert_eq!(s.cpu, 3.0);
        assert_eq!(g.others.len(), 2);
    }

    #[test]
    fn subagent_belongs_to_topmost_claude() {
        let g = group(&[
            proc(1, 0, OTHER, 0),
            proc(10, 1, CLI, 100),
            proc(11, 10, OTHER, 1), // shell in between
            proc(12, 11, CLI, 50),
            proc(13, 12, OTHER, 5),
        ]);
        assert_eq!(g.sessions.len(), 1);
        assert_eq!(g.sessions[0].pid, 10);
        assert_eq!(g.sessions[0].mem, 156);
        assert_eq!(g.sessions[0].count, 4);
    }

    #[test]
    fn sibling_sessions_stay_separate_without_double_counting() {
        let g = group(&[
            proc(1, 0, OTHER, 0),
            proc(10, 1, OTHER, 0),
            proc(11, 10, CLI, 100),
            proc(12, 10, CLI, 100),
            proc(13, 11, OTHER, 7),
        ]);
        assert_eq!(g.sessions.len(), 2);
        let total: u64 = g.sessions.iter().map(|s| s.mem).sum();
        assert_eq!(total, 207);
        assert_eq!(g.sessions.iter().map(|s| s.count).sum::<usize>(), 3);
    }

    fn in_app(mut p: Proc, app: &str) -> Proc {
        p.app = Some(app.into());
        p
    }

    #[test]
    fn app_helpers_are_one_row_never_a_session() {
        let g = group(&[
            proc(1, 0, OTHER, 0),
            in_app(proc(20, 1, OTHER, 100), "Claude"),
            in_app(proc(21, 20, OTHER, 50), "Claude"),
        ]);
        assert!(g.sessions.is_empty());
        let app = g.others.iter().find(|r| &*r.name == "Claude").unwrap();
        assert_eq!((app.mem, app.count), (150, 2));
    }

    #[test]
    fn cli_child_of_desktop_app_is_a_session() {
        let g = group(&[
            proc(1, 0, OTHER, 0),
            in_app(proc(20, 1, OTHER, 100), "Claude"),
            proc(21, 20, CLI, 50),
        ]);
        assert_eq!(g.sessions.len(), 1);
        assert_eq!(g.sessions[0].pid, 21);
    }

    #[test]
    fn orphans_are_not_counted() {
        let g = group(&[
            proc(1, 0, OTHER, 0),
            proc(11, 1, CLI, 100),
            proc(12, 1, OTHER, 30), // daemon, reparented to launchd
        ]);
        assert_eq!(g.sessions[0].mem, 100);
        assert_eq!(g.sessions[0].count, 1);
    }

    #[test]
    fn unreadable_child_is_counted_but_adds_no_memory() {
        let mut child = proc(12, 11, OTHER, 0);
        child.mem = None;
        child.cpu = None;
        let g = group(&[proc(1, 0, OTHER, 0), proc(11, 1, CLI, 100), child]);
        assert_eq!((g.sessions[0].mem, g.sessions[0].count), (100, 2));
        assert_eq!(g.unreadable, 0);
    }

    #[test]
    fn unreadable_others_are_tallied() {
        let mut root_owned = proc(5, 1, OTHER, 0);
        root_owned.mem = None;
        let g = group(&[proc(1, 0, OTHER, 0), root_owned]);
        assert_eq!(g.unreadable, 1);
        assert_eq!(g.others.len(), 1);
    }

    #[test]
    fn pid_cycle_terminates() {
        let g = group(&[proc(10, 11, CLI, 1), proc(11, 10, OTHER, 1)]);
        assert!(g.sessions.len() <= 1);
    }

    #[test]
    fn tree_is_depth_first_with_glyphs() {
        let g = group(&[
            proc(1, 0, OTHER, 0),
            proc(10, 1, CLI, 1),
            proc(11, 10, OTHER, 1),
            proc(12, 11, OTHER, 1),
            proc(13, 10, OTHER, 1),
        ]);
        let rows: Vec<(i32, &str)> = g.sessions[0]
            .tree
            .iter()
            .map(|r| (r.pid, r.prefix.as_str()))
            .collect();
        assert_eq!(rows, [(10, ""), (11, "├ "), (12, "│ └ "), (13, "└ ")]);
    }

    #[test]
    fn sorts_by_key_with_stable_ties() {
        let row = |pid, mem, cpu| Row {
            name: "x".into(),
            pid,
            mem,
            cpu,
            count: 1,
        };
        let mut rows = vec![row(1, 10, 5.0), row(2, 20, 1.0), row(3, 20, 2.0)];
        sort_rows(&mut rows, SortKey::Mem);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [2, 3, 1]);
        sort_rows(&mut rows, SortKey::Cpu);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [1, 3, 2]);
    }

    #[test]
    fn folder_names() {
        assert_eq!(folder_name("/Users/me/code/agentmon"), "agentmon");
        assert_eq!(folder_name("/Users/me/x/"), "x");
        assert_eq!(folder_name("/"), "/");
    }
}
