//! Memory, pressure and per-cluster CPU. The samplers are per platform.
#[cfg(target_os = "macos")]
#[path = "system_macos.rs"]
mod os;
#[cfg(target_os = "linux")]
#[path = "system_linux.rs"]
mod os;
pub use os::{CpuSampler, MemorySampler};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pressure {
    Normal,
    Warn,
    Critical,
    #[default]
    Unknown,
}

impl Pressure {
    pub fn label(self) -> &'static str {
        match self {
            Pressure::Normal => "Normal",
            Pressure::Warn => "Warn",
            Pressure::Critical => "Critical",
            Pressure::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Memory {
    pub total: u64,
    /// macOS: Activity Monitor's "Memory Used", app + wired + compressed.
    /// Linux: total less `MemAvailable`.
    pub used: u64,
    /// Linux: the zswap pool.
    pub compressed: u64,
    /// Anonymous memory, less purgeable on macOS, plus shmem on Linux.
    pub app: u64,
    /// Linux: the kernel's own unreclaimable memory.
    pub wired: u64,
    /// File-backed plus purgeable: reclaimable on demand.
    pub cached: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub pressure: Pressure,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClusterUsage {
    pub label: String,
    /// 0..=100
    pub pct: f64,
    /// 0..=100 per core.
    pub cores: Vec<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuUsage {
    /// 0..=100 across all cores.
    pub overall: f64,
    pub clusters: Vec<ClusterUsage>,
}

/// A contiguous run of logical CPUs.
#[derive(Clone, Debug, PartialEq)]
pub struct Cluster {
    pub label: String,
    pub cpus: std::ops::Range<usize>,
}

/// Per CPU: user, system, idle, nice.
pub type Ticks = [u32; CPU_STATES];
pub const CPU_STATES: usize = 4;

const IDLE: usize = 2;

fn busy_total(prev: &Ticks, cur: &Ticks) -> (u64, u64) {
    let mut busy = 0u64;
    let mut total = 0u64;
    for state in 0..CPU_STATES {
        let delta = u64::from(cur[state].wrapping_sub(prev[state]));
        total += delta;
        if state != IDLE {
            busy += delta;
        }
    }
    (busy, total)
}

fn pct(busy: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        busy as f64 / total as f64 * 100.0
    }
}

/// CPU usage between two tick snapshots. Counters wrap at 32 bits.
pub fn usage_from_ticks(prev: &[Ticks], cur: &[Ticks], clusters: &[Cluster]) -> CpuUsage {
    let per_cpu: Vec<(u64, u64)> = prev
        .iter()
        .zip(cur)
        .map(|(p, c)| busy_total(p, c))
        .collect();
    let sum = |cpus: &[(u64, u64)]| {
        cpus.iter()
            .fold((0, 0), |(b, t), (cb, ct)| (b + cb, t + ct))
    };
    let (busy, total) = sum(&per_cpu);
    CpuUsage {
        overall: pct(busy, total),
        clusters: clusters
            .iter()
            .filter_map(|c| {
                let cores = per_cpu.get(c.cpus.clone())?;
                let (b, t) = sum(cores);
                Some(ClusterUsage {
                    label: c.label.clone(),
                    pct: pct(b, t),
                    cores: cores.iter().map(|&(b, t)| pct(b, t)).collect(),
                })
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_give_per_cluster_usage() {
        let cluster = |label: &str, cpus| Cluster {
            label: label.into(),
            cpus,
        };
        let clusters = [cluster("P", 0..1), cluster("S", 1..2)];
        let prev = [[0, 0, 0, 0], [0, 0, 0, 0]];
        let cur = [[25, 0, 75, 0], [50, 25, 0, 25]];
        let u = usage_from_ticks(&prev, &cur, &clusters);
        assert_eq!(u.overall, 62.5);
        assert_eq!(u.clusters[0].pct, 25.0);
        assert_eq!(u.clusters[1].pct, 100.0);
        assert_eq!(u.clusters[0].cores, [25.0]);
    }

    #[test]
    fn ticks_survive_counter_wrap() {
        let u = usage_from_ticks(&[[u32::MAX, 0, 0, 0]], &[[9, 0, 10, 0]], &[]);
        assert_eq!(u.overall, 50.0);
    }

    #[test]
    fn no_elapsed_ticks_is_zero_not_nan() {
        let u = usage_from_ticks(&[[1, 1, 1, 1]], &[[1, 1, 1, 1]], &[]);
        assert_eq!(u.overall, 0.0);
    }
}
