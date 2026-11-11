//! GPU, power, temperature and fans. The source is per platform: a `macmon`
//! child on macOS, sysfs and `nvidia-smi` on Linux.
use crate::system::CpuUsage;
use std::sync::atomic::AtomicI32;

#[cfg(target_os = "macos")]
#[path = "macmon.rs"]
mod os;
#[cfg(target_os = "linux")]
#[path = "sensors_linux.rs"]
mod os;
pub use os::Source;

/// Child pid, for the signal handler. 0 when none.
pub static CHILD_PID: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    /// CPU load, where the source measures it better than ticks do.
    pub cpu: Option<CpuUsage>,
    pub gpu_pct: Option<f64>,
    pub temp_c: Option<f64>,
    /// Whole-system power.
    pub watts: Option<f64>,
    pub cpu_watts: Option<f64>,
    pub gpu_watts: Option<f64>,
    pub ane_watts: Option<f64>,
    pub gpu_temp_c: Option<f64>,
    /// Fastest fan. None when stopped, or on fanless Macs.
    pub fan_rpm: Option<f64>,
}
