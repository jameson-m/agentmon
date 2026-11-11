//! Linux: memory from `/proc/meminfo`, pressure from PSI, CPU from `/proc/stat`
//! and clusters from sysfs.
use super::*;
use std::fs;

/// Stall percentages over ten seconds that count as pressure.
const PSI_WARN: f64 = 5.0;
const PSI_CRITICAL: f64 = 5.0;
/// Shares of RAM still available below which pressure is assumed anyway.
const AVAILABLE_WARN: f64 = 0.10;
const AVAILABLE_CRITICAL: f64 = 0.05;

/// A `Name:   123 kB` field of meminfo, in bytes. 0 when absent.
fn field(meminfo: &str, name: &str) -> u64 {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// `avg10` of the `some` and `full` lines of `/proc/pressure/memory`.
fn parse_psi(text: &str) -> Option<(f64, f64)> {
    let avg10 = |kind: &str| {
        let line = text.lines().find(|l| l.starts_with(kind))?;
        let value = line
            .split_whitespace()
            .find_map(|f| f.strip_prefix("avg10="))?;
        value.parse::<f64>().ok()
    };
    Some((avg10("some")?, avg10("full").unwrap_or(0.0)))
}

/// Tasks stalling on memory is pressure; so is nearly nothing left to give.
fn pressure(psi: Option<(f64, f64)>, available: f64) -> Pressure {
    let (some, full) = psi.unwrap_or_default();
    if full >= PSI_CRITICAL || available < AVAILABLE_CRITICAL {
        Pressure::Critical
    } else if some >= PSI_WARN || available < AVAILABLE_WARN {
        Pressure::Warn
    } else {
        Pressure::Normal
    }
}

fn parse_meminfo(meminfo: &str, psi: Option<(f64, f64)>) -> Memory {
    let get = |name| field(meminfo, name);
    let total = get("MemTotal");
    if total == 0 {
        return Memory::default();
    }
    let available = get("MemAvailable");
    let shmem = get("Shmem");
    Memory {
        total,
        used: total.saturating_sub(available),
        compressed: get("Zswap"),
        // Shmem is counted as cache by the kernel, but can't be dropped.
        app: get("AnonPages") + shmem,
        wired: get("SUnreclaim") + get("KernelStack") + get("PageTables"),
        cached: (get("Cached") + get("Buffers") + get("SReclaimable")).saturating_sub(shmem),
        swap_total: get("SwapTotal"),
        swap_used: get("SwapTotal").saturating_sub(get("SwapFree")),
        pressure: pressure(psi, available as f64 / total as f64),
    }
}

pub struct MemorySampler;

impl MemorySampler {
    pub fn new() -> Self {
        Self
    }

    pub fn sample(&self) -> Memory {
        let psi = fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|text| parse_psi(&text));
        fs::read_to_string("/proc/meminfo")
            .map(|text| parse_meminfo(&text, psi))
            .unwrap_or_default()
    }
}

/// Per-CPU ticks from `/proc/stat`, in our order: user, system, idle, nice.
/// Interrupts and steal count as system, and waiting on I/O as idle.
fn parse_stat(stat: &str) -> Vec<Ticks> {
    stat.lines()
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .map(|line| {
            let mut fields = line
                .split_whitespace()
                .skip(1)
                .map(|f| f.parse::<u64>().unwrap_or(0));
            let mut next = || fields.next().unwrap_or(0);
            let (user, nice, system, idle) = (next(), next(), next(), next());
            let (iowait, irq, softirq, steal) = (next(), next(), next(), next());
            // Counters are compared with wrapping, so truncating is fine.
            [
                user as u32,
                (system + irq + softirq + steal) as u32,
                (idle + iowait) as u32,
                nice as u32,
            ]
        })
        .collect()
}

/// A sysfs CPU list that is one run, such as `0-15` or `3`.
fn parse_run(list: &str) -> Option<std::ops::Range<usize>> {
    let list = list.trim();
    let (first, last) = list.split_once('-').unwrap_or((list, list));
    let (first, last) = (first.parse::<usize>().ok()?, last.parse::<usize>().ok()?);
    (first <= last).then_some(first..last + 1)
}

/// Clusters from each CPU's capacity, as arm64 reports it: runs of equal
/// capacity, the fastest `P`, the slowest `E` and any between `M`.
fn clusters_from_capacity(capacity: &[u64]) -> Vec<Cluster> {
    let (Some(&max), Some(&min)) = (capacity.iter().max(), capacity.iter().min()) else {
        return Vec::new();
    };
    if max == min {
        return Vec::new();
    }
    let mut clusters: Vec<(u64, Cluster)> = Vec::new();
    for (cpu, &cap) in capacity.iter().enumerate() {
        match clusters.last_mut() {
            Some((last, cluster)) if *last == cap => cluster.cpus.end = cpu + 1,
            _ => {
                let label = match cap {
                    c if c == max => "P",
                    c if c == min => "E",
                    _ => "M",
                };
                clusters.push((
                    cap,
                    Cluster {
                        label: label.into(),
                        cpus: cpu..cpu + 1,
                    },
                ));
            }
        }
    }
    clusters.into_iter().map(|(_, cluster)| cluster).collect()
}

/// Intel hybrid chips name their core types; arm64 gives capacities. Anything
/// else is one cluster of every CPU.
fn read_topology(cpus: usize) -> Vec<Cluster> {
    let run = |kind: &str| {
        let list = fs::read_to_string(format!("/sys/devices/{kind}/cpus")).ok()?;
        parse_run(&list).filter(|run| run.end <= cpus)
    };
    if let (Some(core), Some(atom)) = (run("cpu_core"), run("cpu_atom")) {
        let mut clusters = vec![
            Cluster {
                label: "P".into(),
                cpus: core,
            },
            Cluster {
                label: "E".into(),
                cpus: atom,
            },
        ];
        clusters.sort_by_key(|c| c.cpus.start);
        return clusters;
    }
    let capacity: Vec<u64> = (0..cpus)
        .map_while(|cpu| {
            let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity");
            fs::read_to_string(path).ok()?.trim().parse().ok()
        })
        .collect();
    let clusters = clusters_from_capacity(&capacity);
    if capacity.len() == cpus && !clusters.is_empty() {
        return clusters;
    }
    vec![Cluster {
        label: "C".into(),
        cpus: 0..cpus,
    }]
}

pub struct CpuSampler {
    clusters: Vec<Cluster>,
    prev: Vec<Ticks>,
}

impl CpuSampler {
    pub fn new() -> Self {
        let prev = fs::read_to_string("/proc/stat")
            .map(|stat| parse_stat(&stat))
            .unwrap_or_default();
        Self {
            clusters: read_topology(prev.len()),
            prev,
        }
    }

    pub fn sample(&mut self) -> Option<CpuUsage> {
        let cur = parse_stat(&fs::read_to_string("/proc/stat").ok()?);
        let usage = (self.prev.len() == cur.len())
            .then(|| usage_from_ticks(&self.prev, &cur, &self.clusters));
        self.prev = cur;
        usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "\
MemTotal:       16000000 kB
MemFree:         1000000 kB
MemAvailable:    6000000 kB
Buffers:          500000 kB
Cached:          5000000 kB
SwapTotal:       2000000 kB
SwapFree:        1500000 kB
AnonPages:       7000000 kB
Shmem:           1000000 kB
KReclaimable:     600000 kB
SReclaimable:     500000 kB
SUnreclaim:       300000 kB
KernelStack:       20000 kB
PageTables:        80000 kB
Zswap:            100000 kB
";

    #[test]
    fn meminfo_maps_to_the_breakdown() {
        let kb = |n: u64| n * 1024;
        let m = parse_meminfo(MEMINFO, None);
        assert_eq!(m.total, kb(16_000_000));
        assert_eq!(m.used, kb(10_000_000));
        assert_eq!(m.app, kb(8_000_000));
        assert_eq!(m.wired, kb(400_000));
        assert_eq!(m.cached, kb(5_000_000));
        assert_eq!(m.compressed, kb(100_000));
        assert_eq!((m.swap_total, m.swap_used), (kb(2_000_000), kb(500_000)));
        assert_eq!(m.pressure, Pressure::Normal);
        assert_eq!(parse_meminfo("", None).total, 0);
    }

    #[test]
    fn pressure_from_stalls_or_scarcity() {
        let psi = parse_psi(
            "some avg10=7.50 avg60=1.00 avg300=0.10 total=1\n\
             full avg10=0.20 avg60=0.00 avg300=0.00 total=1\n",
        );
        assert_eq!(psi, Some((7.5, 0.2)));
        assert_eq!(pressure(psi, 0.5), Pressure::Warn);
        assert_eq!(pressure(Some((9.0, 6.0)), 0.5), Pressure::Critical);
        assert_eq!(pressure(None, 0.5), Pressure::Normal);
        assert_eq!(pressure(None, 0.08), Pressure::Warn);
        assert_eq!(pressure(None, 0.01), Pressure::Critical);
        assert_eq!(parse_psi(""), None);
    }

    #[test]
    fn stat_lines_become_ticks() {
        let stat = "\
cpu  100 10 50 800 40 0 0 0 0 0
cpu0 60 4 20 400 10 3 2 1 0 0
cpu1 40 6 30 400 30 0 0 0 0 0
intr 12345
";
        assert_eq!(parse_stat(stat), [[60, 26, 410, 4], [40, 30, 430, 6]]);
    }

    #[test]
    fn cpu_lists_and_capacities() {
        assert_eq!(parse_run("0-15\n"), Some(0..16));
        assert_eq!(parse_run("3"), Some(3..4));
        assert_eq!(parse_run("0-3,8-11"), None);
        let c = clusters_from_capacity(&[250, 250, 250, 250, 768, 768, 1024]);
        let got: Vec<(&str, std::ops::Range<usize>)> = c
            .iter()
            .map(|c| (c.label.as_str(), c.cpus.clone()))
            .collect();
        assert_eq!(got, [("E", 0..4), ("M", 4..6), ("P", 6..7)]);
        assert!(clusters_from_capacity(&[1024; 8]).is_empty());
    }

    #[test]
    fn samples_this_machine() {
        let mem = MemorySampler::new().sample();
        assert!(mem.total > 0 && mem.used <= mem.total);
        let mut cpu = CpuSampler::new();
        let usage = cpu.sample().expect("usage");
        let cores: usize = usage.clusters.iter().map(|c| c.cores.len()).sum();
        assert!(cores > 0);
    }
}
