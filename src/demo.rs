//! Invented data for `--demo`: a busy machine to look at, with nothing read
//! from this one. Also what the README's screenshot is made from.
use crate::app::{self, App};
use crate::claude::{Meta, Status};
use crate::procs::{Kind, Proc};
use crate::sensors::Sample;
use crate::sessions;
use crate::system::{ClusterUsage, CpuUsage, Memory, Pressure};
use std::collections::HashMap;

const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;
/// Ticks fed in at the start, so the graphs open with a history.
const WARMUP: u64 = 240;
/// Invented clock: the demo's "now", in epoch milliseconds.
const NOW_MS: u64 = 1_750_000_000_000;

/// A session: folder, a name the user gave it, memory in MB, typical CPU,
/// whether it is working, seconds in that state and tokens in context.
type Session = (&'static str, Option<&'static str>, u64, f64, bool, u64, u64);

const SESSIONS: [Session; 6] = [
    ("api-server", None, 1900, 85.0, true, 240, 412_000),
    (
        "web-frontend",
        Some("checkout redesign"),
        1400,
        35.0,
        true,
        75,
        188_000,
    ),
    ("data-pipeline", None, 1100, 0.5, false, 180, 96_000),
    ("docs-site", None, 760, 0.2, false, 5_400, 61_000),
    (
        "mobile-app",
        Some("fix login crash"),
        690,
        12.0,
        true,
        1_260,
        233_000,
    ),
    ("infra", None, 540, 0.1, false, 93_600, 147_000),
];

/// What a session runs under it: name and share of the session's memory.
const CHILDREN: [(&str, f64); 4] = [
    ("node", 0.22),
    ("rust-analyzer", 0.18),
    ("zsh", 0.01),
    ("cargo", 0.09),
];

/// Everything else: name, process count, memory in MB and typical CPU.
const OTHERS: [(&str, usize, u64, f64); 12] = [
    ("Google Chrome", 34, 5200, 9.0),
    ("Visual Studio Code", 12, 1800, 4.0),
    ("Slack", 6, 1100, 1.5),
    ("Docker", 8, 950, 3.0),
    ("Claude", 9, 820, 1.0),
    ("Spotify", 4, 480, 0.8),
    ("Ghostty", 1, 360, 2.5),
    ("Finder", 1, 310, 0.1),
    ("Notes", 2, 240, 0.0),
    ("postgres", 7, 210, 0.6),
    ("Mail", 3, 190, 0.1),
    ("WindowServer", 1, 170, 6.0),
];

/// A smooth, repeatable wobble in 0..=1, different for each `seed`.
fn wave(tick: u64, seed: f64) -> f64 {
    let t = tick as f64;
    let mix = (t * 0.31 + seed).sin() + (t * 0.073 + seed * 1.7).sin() * 0.8;
    ((mix / 1.8) + 1.0) / 2.0
}

#[derive(Default)]
pub struct Demo {
    tick: u64,
}

impl Demo {
    /// Advances the invented machine by one sample.
    pub fn tick(&mut self, app: &mut App) {
        let rounds = if self.tick == 0 { WARMUP } else { 1 };
        for _ in 0..rounds {
            self.tick += 1;
            self.ingest(app);
        }
    }

    fn ingest(&self, app: &mut App) {
        let tick = self.tick;
        let mut procs = Vec::new();
        let mut add = |pid: i32, ppid: i32, kind, name: &str, app: Option<&str>, mem: f64, cpu| {
            procs.push(Proc {
                pid,
                ppid,
                kind,
                name: name.into(),
                app: app.map(Into::into),
                mem: Some(mem as u64),
                cpu: Some(cpu),
            });
        };
        for (i, &(_, _, mem, cpu, busy, ..)) in SESSIONS.iter().enumerate() {
            let root = 1000 + i as i32 * 10;
            let seed = i as f64 * 2.3;
            let mem = (mem * MB) as f64 * (0.97 + 0.06 * wave(tick, seed + 9.0));
            // Working sessions surge and settle; idle ones barely stir.
            let load = if busy {
                cpu * (0.25 + 1.1 * wave(tick, seed))
            } else {
                cpu * wave(tick, seed)
            };
            add(root, 1, Kind::Cli, "claude", None, mem * 0.5, load * 0.3);
            for (n, &(name, share)) in CHILDREN.iter().enumerate() {
                let cpu = if n < 2 { load * 0.35 } else { 0.0 };
                add(
                    root + 1 + n as i32,
                    root,
                    Kind::Other,
                    name,
                    None,
                    mem * share,
                    cpu,
                );
            }
        }
        let mut pid = 5000;
        for (i, &(name, count, mem, cpu)) in OTHERS.iter().enumerate() {
            let each = (mem * MB) as f64 / count as f64;
            for n in 0..count {
                let cpu = cpu / count as f64 * 2.0 * wave(tick, i as f64 + n as f64 * 0.7);
                add(pid, 1, Kind::Other, name, Some(name), each, cpu);
                pid += 1;
            }
        }

        let mut grouped = sessions::group(&procs);
        let folder = |pid: i32| ((pid - 1000) / 10) as usize;
        app::label_sessions(&mut grouped.sessions, |pid| {
            Some(format!("/home/me/code/{}", SESSIONS.get(folder(pid))?.0))
        });
        let mut meta = HashMap::new();
        for session in &mut grouped.sessions {
            let Some(&(_, name, _, _, busy, age, context)) = SESSIONS.get(folder(session.pid))
            else {
                continue;
            };
            if let Some(name) = name {
                session.name = name.into();
            }
            meta.insert(
                session.pid,
                Meta {
                    name: name.map(Into::into),
                    status: Some(if busy { Status::Busy } else { Status::Idle }),
                    since_ms: Some(NOW_MS - age * 1000),
                    context: Some(context),
                },
            );
        }

        let cores = |count: usize, base: f64, seed: f64| -> Vec<f64> {
            (0..count)
                .map(|core| {
                    let fade = 1.0 - core as f64 / count as f64 * 0.7;
                    (base * fade * 2.0 * wave(tick, seed + core as f64 * 1.3)).clamp(0.0, 100.0)
                })
                .collect()
        };
        let cluster = |label: &str, cores: Vec<f64>| ClusterUsage {
            label: label.into(),
            pct: cores.iter().sum::<f64>() / cores.len() as f64,
            cores,
        };
        let clusters = vec![
            cluster("P", cores(8, 38.0, 0.0)),
            cluster("E", cores(4, 22.0, 5.0)),
        ];
        let overall = clusters.iter().flat_map(|c| &c.cores).sum::<f64>() / 12.0;
        let cpu = CpuUsage { overall, clusters };

        let app_bytes = 13 * GB + (GB as f64 * 1.5 * wave(tick, 20.0)) as u64;
        let (wired, compressed) = (3 * GB + 400 * MB, 2 * GB + 700 * MB);
        let memory = Memory {
            total: 32 * GB,
            used: app_bytes + wired + compressed,
            compressed,
            app: app_bytes,
            wired,
            cached: 8 * GB,
            swap_total: 4 * GB,
            swap_used: 900 * MB,
            pressure: Pressure::Normal,
        };
        let heat = overall / 100.0;
        let sample = Sample {
            cpu: None,
            gpu_pct: Some(8.0 + 30.0 * wave(tick, 30.0)),
            temp_c: Some(44.0 + 30.0 * heat),
            watts: Some(9.0 + 40.0 * heat),
            cpu_watts: Some(2.0 + 30.0 * heat),
            gpu_watts: Some(0.4 + 3.0 * wave(tick, 30.0)),
            ane_watts: Some(0.0),
            gpu_temp_c: Some(41.0 + 20.0 * heat),
            fan_rpm: Some(1200.0 + 1500.0 * heat),
        };
        app.ingest(memory, cpu, Some(sample), grouped, procs.len());
        app.session_meta = meta;
        app.now_ms = NOW_MS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_with_history_and_every_session() {
        let mut app = App::default();
        let mut demo = Demo::default();
        demo.tick(&mut app);
        assert_eq!(app.grouped.sessions.len(), SESSIONS.len());
        assert_eq!(app.mem_history.len() as u64, WARMUP);
        assert!(
            app.grouped
                .sessions
                .iter()
                .any(|s| s.name == "checkout redesign")
        );
        assert!(app.grouped.sessions.iter().any(|s| s.name == "infra"));
        assert!(app.cpu.overall > 0.0 && app.cpu.overall < 100.0);
        demo.tick(&mut app);
        assert_eq!(app.mem_history.len() as u64, WARMUP + 1);
    }
}
