//! Linux sensors, read from sysfs each tick: CPU temperature and fans from
//! hwmon, power from the battery or RAPL, and an AMD GPU from its driver. An
//! NVIDIA GPU is read from a long-lived `nvidia-smi` child.
use super::{CHILD_PID, Sample};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// hwmon drivers that report the CPU's own temperature, best first.
const CPU_SENSORS: [&str; 7] = [
    "coretemp",
    "k10temp",
    "zenpower",
    "cpu_thermal",
    "cpu-thermal",
    "soc_thermal",
    "acpitz",
];
const NVIDIA_QUERY: &str = "--query-gpu=utilization.gpu,temperature.gpu,power.draw";

fn read_number(path: &Path) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn entries(dir: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// An energy counter in microjoules that wraps at `max`.
struct Rapl {
    energy: PathBuf,
    max: f64,
    /// The whole platform, rather than one CPU package.
    platform: bool,
    last: Option<(f64, Instant)>,
}

/// Watts between two readings of a wrapping microjoule counter.
fn watts_between(prev: f64, now: f64, max: f64, elapsed: Duration) -> Option<f64> {
    let joules = if now >= prev {
        now - prev
    } else {
        now + max - prev
    } / 1e6;
    let secs = elapsed.as_secs_f64();
    (secs > 0.0 && joules >= 0.0).then(|| joules / secs)
}

impl Rapl {
    fn watts(&mut self) -> Option<f64> {
        let now = (read_number(&self.energy)?, Instant::now());
        let prev = self.last.replace(now)?;
        watts_between(prev.0, now.0, self.max, now.1.duration_since(prev.1))
    }
}

/// One line of `nvidia-smi --format=csv,noheader,nounits`: load, °C, watts.
/// A field the card doesn't support reads `[N/A]`.
fn parse_nvidia(line: &str) -> Option<[Option<f64>; 3]> {
    let mut fields = line.split(',').map(|f| f.trim().parse::<f64>().ok());
    let values = [fields.next()?, fields.next()?, fields.next()?];
    values.iter().any(Option::is_some).then_some(values)
}

type Shared = Arc<Mutex<Option<([Option<f64>; 3], Instant)>>>;

struct Nvidia {
    child: Child,
    latest: Shared,
}

impl Nvidia {
    fn start(interval: Duration) -> Option<Self> {
        let ms = interval.as_millis().to_string();
        let mut child = Command::new("nvidia-smi")
            .args([NVIDIA_QUERY, "--format=csv,noheader,nounits", "-lms", &ms])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
        let latest = Shared::default();
        let shared = latest.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let (Some(values), Ok(mut guard)) = (parse_nvidia(&line), shared.lock()) {
                    *guard = Some((values, Instant::now()));
                }
            }
        });
        Some(Self { child, latest })
    }
}

impl Drop for Nvidia {
    fn drop(&mut self) {
        CHILD_PID.store(0, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Source {
    interval: Duration,
    cpu_temp: Option<PathBuf>,
    fans: Vec<PathBuf>,
    batteries: Vec<PathBuf>,
    rapl: Vec<Rapl>,
    /// An AMD card's device directory, and its hwmon directory.
    amd: Option<(PathBuf, Option<PathBuf>)>,
    nvidia: Option<Nvidia>,
}

/// The CPU's temperature file: a known hwmon driver, or else a thermal zone.
fn find_cpu_temp(hwmons: &[PathBuf]) -> Option<PathBuf> {
    let name = |dir: &PathBuf| fs::read_to_string(dir.join("name")).unwrap_or_default();
    CPU_SENSORS
        .iter()
        .find_map(|sensor| {
            let dir = hwmons.iter().find(|dir| name(dir).trim() == *sensor)?;
            let file = dir.join("temp1_input");
            file.exists().then_some(file)
        })
        .or_else(|| {
            let zone = PathBuf::from("/sys/class/thermal/thermal_zone0/temp");
            zone.exists().then_some(zone)
        })
}

impl Source {
    pub fn start(interval: Duration) -> Self {
        let hwmons = entries("/sys/class/hwmon");
        let fans = hwmons
            .iter()
            .flat_map(|dir| entries(&dir.to_string_lossy()))
            .filter(|file| {
                let name = file_name(file);
                name.starts_with("fan") && name.ends_with("_input")
            })
            .collect();
        let batteries = entries("/sys/class/power_supply")
            .into_iter()
            .filter(|dir| file_name(dir).starts_with("BAT"))
            .collect();
        // Top-level zones only: the ones under them are parts of a package.
        let rapl = entries("/sys/class/powercap")
            .into_iter()
            .filter(|dir| file_name(dir).matches(':').count() == 1)
            .filter_map(|dir| {
                let name = fs::read_to_string(dir.join("name")).ok()?;
                let name = name.trim();
                (name.starts_with("package") || name == "psys").then(|| Rapl {
                    energy: dir.join("energy_uj"),
                    max: read_number(&dir.join("max_energy_range_uj")).unwrap_or(0.0),
                    platform: name == "psys",
                    last: None,
                })
            })
            .collect();
        let amd = entries("/sys/class/drm").into_iter().find_map(|card| {
            let device = card.join("device");
            device.join("gpu_busy_percent").exists().then(|| {
                let hwmon = entries(&device.join("hwmon").to_string_lossy());
                (device, hwmon.into_iter().next())
            })
        });
        Self {
            interval,
            cpu_temp: find_cpu_temp(&hwmons),
            fans,
            batteries,
            rapl,
            amd,
            nvidia: Nvidia::start(interval),
        }
    }

    /// Watts leaving the battery. None on mains, where it reports the charge.
    fn battery_watts(&self) -> Option<f64> {
        let total: f64 = self
            .batteries
            .iter()
            .filter(|dir| {
                fs::read_to_string(dir.join("status")).is_ok_and(|s| s.trim() == "Discharging")
            })
            .filter_map(|dir| {
                read_number(&dir.join("power_now"))
                    .map(|uw| uw / 1e6)
                    .or_else(|| {
                        let amps = read_number(&dir.join("current_now"))? / 1e6;
                        let volts = read_number(&dir.join("voltage_now"))? / 1e6;
                        Some(amps * volts)
                    })
            })
            .sum();
        (total > 0.0).then_some(total)
    }

    /// What can be read right now. None when this machine offers nothing.
    pub fn latest(&mut self) -> Option<Sample> {
        let mut sample = Sample {
            temp_c: self
                .cpu_temp
                .as_deref()
                .and_then(read_number)
                .map(|milli| milli / 1000.0)
                .filter(|t| *t > 0.0),
            fan_rpm: self
                .fans
                .iter()
                .filter_map(|fan| read_number(fan))
                .reduce(f64::max)
                .filter(|rpm| *rpm > 0.0),
            ..Sample::default()
        };
        // Readable only by root on most kernels, so often absent.
        let (mut packages, mut platform) = (None, None);
        for zone in &mut self.rapl {
            let slot = if zone.platform {
                &mut platform
            } else {
                &mut packages
            };
            if let Some(watts) = zone.watts() {
                *slot = Some(slot.unwrap_or(0.0) + watts);
            }
        }
        sample.cpu_watts = packages;
        sample.watts = self.battery_watts().or(platform).or(packages);

        if let Some((device, hwmon)) = &self.amd {
            sample.gpu_pct = read_number(&device.join("gpu_busy_percent"));
            if let Some(hwmon) = hwmon {
                sample.gpu_temp_c = read_number(&hwmon.join("temp1_input")).map(|m| m / 1000.0);
                sample.gpu_watts = read_number(&hwmon.join("power1_average")).map(|uw| uw / 1e6);
            }
        }
        let max_age = self.interval * 3 + Duration::from_secs(2);
        let nvidia = self
            .nvidia
            .as_ref()
            .and_then(|n| *n.latest.lock().ok()?)
            .filter(|(_, at)| at.elapsed() <= max_age);
        if let Some(([load, temp, watts], _)) = nvidia {
            sample.gpu_pct = load.or(sample.gpu_pct);
            sample.gpu_temp_c = temp.or(sample.gpu_temp_c);
            sample.gpu_watts = watts.or(sample.gpu_watts);
        }
        (sample != Sample::default()).then_some(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_lines() {
        assert_eq!(
            parse_nvidia("37, 61, 142.50\n"),
            Some([Some(37.0), Some(61.0), Some(142.5)])
        );
        assert_eq!(
            parse_nvidia("12, 48, [N/A]"),
            Some([Some(12.0), Some(48.0), None])
        );
        assert_eq!(parse_nvidia("[N/A], [N/A], [N/A]"), None);
        assert_eq!(parse_nvidia("No devices were found"), None);
    }

    #[test]
    fn rapl_counter_wraps() {
        let two = Duration::from_secs(2);
        assert_eq!(watts_between(1e6, 21e6, 100e6, two), Some(10.0));
        assert_eq!(watts_between(95e6, 5e6, 100e6, two), Some(5.0));
        assert_eq!(watts_between(1e6, 2e6, 100e6, Duration::ZERO), None);
    }

    #[test]
    fn starts_whatever_this_machine_has() {
        let mut source = Source::start(Duration::from_secs(1));
        let _ = source.latest();
    }
}
