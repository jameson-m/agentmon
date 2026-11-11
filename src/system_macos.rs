//! macOS: memory, pressure and per-cluster CPU from sysctl and host statistics.
use super::*;
use crate::ffi;

/// Maps `kern.memorystatus_vm_pressure_level`.
pub fn pressure_level(level: i32) -> Pressure {
    match level {
        1 => Pressure::Normal,
        2 => Pressure::Warn,
        4 => Pressure::Critical,
        _ => Pressure::Unknown,
    }
}

/// Activity Monitor's formula, in pages.
pub fn used_pages(internal: u64, purgeable: u64, wired: u64, compressor: u64) -> u64 {
    internal.saturating_sub(purgeable) + wired + compressor
}

pub struct MemorySampler {
    host: u32,
    page_size: u64,
    total: u64,
}

impl MemorySampler {
    pub fn new() -> Self {
        Self {
            host: unsafe { ffi::mach_host_self() },
            page_size: ffi::sysctl_read::<u64>("hw.pagesize")
                .or_else(|| ffi::sysctl_read::<u32>("hw.pagesize").map(u64::from))
                .unwrap_or(16384),
            total: ffi::sysctl_read("hw.memsize").unwrap_or(0),
        }
    }

    pub fn sample(&self) -> Memory {
        let mut mem = Memory {
            total: self.total,
            ..Default::default()
        };
        if let Some(level) = ffi::sysctl_read::<i32>("kern.memorystatus_vm_pressure_level") {
            mem.pressure = pressure_level(level);
        }
        if let Some(swap) = ffi::sysctl_read::<ffi::XswUsage>("vm.swapusage") {
            mem.swap_total = swap.total;
            mem.swap_used = swap.used;
        }
        let mut vm = ffi::VmStatistics64::default();
        let mut count = ffi::HOST_VM_INFO64_COUNT;
        let rc = unsafe {
            ffi::host_statistics64(
                self.host,
                ffi::HOST_VM_INFO64,
                (&raw mut vm).cast(),
                &mut count,
            )
        };
        if rc == ffi::KERN_SUCCESS {
            let bytes = |pages: u32| u64::from(pages) * self.page_size;
            mem.compressed = bytes(vm.compressor_page_count);
            mem.wired = bytes(vm.wire_count);
            mem.app = bytes(vm.internal_page_count).saturating_sub(bytes(vm.purgeable_count));
            mem.cached = bytes(vm.external_page_count) + bytes(vm.purgeable_count);
            mem.used = used_pages(
                vm.internal_page_count.into(),
                vm.purgeable_count.into(),
                vm.wire_count.into(),
                vm.compressor_page_count.into(),
            ) * self.page_size;
        }
        mem
    }
}

/// `Performance` -> `P`, `Super` -> `S`, `Efficiency` -> `E`.
pub fn cluster_label(name: &str) -> String {
    name.chars().next().map_or("?".into(), |c| c.to_string())
}

/// Logical CPUs are numbered from the slowest perflevel up, and perflevel0 is
/// the fastest, so walk perflevels in reverse.
pub fn clusters_from_levels(levels_fast_first: &[(String, usize)]) -> Vec<Cluster> {
    let mut start = 0;
    levels_fast_first
        .iter()
        .rev()
        .map(|(name, n)| {
            let cpus = start..start + n;
            start += n;
            Cluster {
                label: cluster_label(name),
                cpus,
            }
        })
        .collect()
}

fn read_topology() -> Vec<Cluster> {
    let levels: i32 = ffi::sysctl_read("hw.nperflevels").unwrap_or(0);
    let levels: Vec<(String, usize)> = (0..levels)
        .filter_map(|i| {
            let n: i32 = ffi::sysctl_read(&format!("hw.perflevel{i}.logicalcpu"))?;
            let name = ffi::sysctl_string(&format!("hw.perflevel{i}.name"))?;
            Some((name, n as usize))
        })
        .collect();
    clusters_from_levels(&levels)
}

/// Tick-based fallback for when macmon isn't available.
pub struct CpuSampler {
    host: u32,
    clusters: Vec<Cluster>,
    prev: Vec<Ticks>,
}

impl CpuSampler {
    pub fn new() -> Self {
        Self {
            host: unsafe { ffi::mach_host_self() },
            clusters: read_topology(),
            prev: Vec::new(),
        }
    }

    fn ticks(&self) -> Option<Vec<Ticks>> {
        let mut cpus = 0u32;
        let mut info = std::ptr::null_mut();
        let mut words = 0u32;
        let rc = unsafe {
            ffi::host_processor_info(
                self.host,
                ffi::PROCESSOR_CPU_LOAD_INFO,
                &mut cpus,
                &mut info,
                &mut words,
            )
        };
        if rc != ffi::KERN_SUCCESS || info.is_null() {
            return None;
        }
        let ticks = unsafe { std::slice::from_raw_parts(info as *const u32, words as usize) }
            .as_chunks::<{ CPU_STATES }>()
            .0
            .to_vec();
        unsafe {
            ffi::vm_deallocate(
                ffi::mach_task_self_,
                info as usize,
                words as usize * size_of::<i32>(),
            );
        }
        Some(ticks)
    }

    pub fn sample(&mut self) -> Option<CpuUsage> {
        let cur = self.ticks()?;
        let usage = (self.prev.len() == cur.len())
            .then(|| usage_from_ticks(&self.prev, &cur, &self.clusters));
        self.prev = cur;
        usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_levels() {
        assert_eq!(pressure_level(1), Pressure::Normal);
        assert_eq!(pressure_level(2), Pressure::Warn);
        assert_eq!(pressure_level(4), Pressure::Critical);
        assert_eq!(pressure_level(3), Pressure::Unknown);
        assert_eq!(pressure_level(0), Pressure::Unknown);
    }

    #[test]
    fn used_excludes_purgeable_and_file_cache() {
        assert_eq!(used_pages(100, 10, 20, 5), 115);
        assert_eq!(used_pages(5, 10, 20, 5), 25);
    }

    #[test]
    fn perflevels_map_slowest_first_with_real_names() {
        let c = clusters_from_levels(&[("Super".into(), 6), ("Performance".into(), 12)]);
        assert_eq!(
            c,
            [
                Cluster {
                    label: "P".into(),
                    cpus: 0..12
                },
                Cluster {
                    label: "S".into(),
                    cpus: 12..18
                },
            ]
        );
    }
}
