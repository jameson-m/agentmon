//! Long-lived `macmon pipe` child, read from a background thread.
use super::{CHILD_PID, Sample};
use crate::system::{ClusterUsage, CpuUsage};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RESPAWN_AFTER: Duration = Duration::from_secs(30);
const CANDIDATES: [&str; 2] = ["macmon", "/opt/homebrew/bin/macmon"];

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct Raw {
    cpu_active_ratio: Option<f64>,
    cpu_usage_pct: Option<f64>,
    cpu_tiers: Vec<RawTier>,
    ecpu_usage: Option<(f64, f64)>,
    pcpu_usage: Option<(f64, f64)>,
    gpu_usage: Option<(f64, f64)>,
    gpu_active_ratio: Option<f64>,
    all_power: Option<f64>,
    sys_power: Option<f64>,
    cpu_power: Option<f64>,
    gpu_power: Option<f64>,
    ane_power: Option<f64>,
    fans: Vec<RawFan>,
    temp: Option<RawTemp>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct RawFan {
    rpm: f64,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct RawTier {
    label: String,
    active_ratio: f64,
    cores: Vec<RawCore>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct RawCore {
    active_ratio: f64,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct RawTemp {
    cpu_temp_avg: Option<f64>,
    gpu_temp_avg: Option<f64>,
}

fn ratio_pct(r: f64) -> f64 {
    (r * 100.0).clamp(0.0, 100.0)
}

/// Parses one line of `macmon pipe`. `ecpu`/`pcpu` are macmon's fixed names for
/// its two tiers, so tier labels are preferred: on chips with Super cores they are P and S.
pub fn parse_line(line: &str) -> Option<Sample> {
    let raw: Raw = serde_json::from_str(line).ok()?;
    let clusters: Vec<ClusterUsage> = if raw.cpu_tiers.is_empty() {
        [("E", raw.ecpu_usage), ("P", raw.pcpu_usage)]
            .into_iter()
            .filter_map(|(label, usage)| {
                Some(ClusterUsage {
                    label: label.into(),
                    pct: ratio_pct(usage?.1),
                    cores: Vec::new(),
                })
            })
            .collect()
    } else {
        raw.cpu_tiers
            .iter()
            .map(|t| ClusterUsage {
                label: t.label.clone(),
                pct: ratio_pct(t.active_ratio),
                cores: t.cores.iter().map(|c| ratio_pct(c.active_ratio)).collect(),
            })
            .collect()
    };
    let overall = raw.cpu_active_ratio.or(raw.cpu_usage_pct);
    Some(Sample {
        cpu: overall.map(|o| CpuUsage {
            overall: ratio_pct(o),
            clusters,
        }),
        gpu_pct: raw
            .gpu_active_ratio
            .or(raw.gpu_usage.map(|u| u.1))
            .map(ratio_pct),
        temp_c: raw
            .temp
            .as_ref()
            .and_then(|t| t.cpu_temp_avg)
            .filter(|t| *t > 0.0),
        watts: raw.sys_power.or(raw.all_power).filter(|w| *w > 0.0),
        cpu_watts: raw.cpu_power,
        gpu_watts: raw.gpu_power,
        ane_watts: raw.ane_power,
        gpu_temp_c: raw
            .temp
            .as_ref()
            .and_then(|t| t.gpu_temp_avg)
            .filter(|t| *t > 0.0),
        fan_rpm: raw
            .fans
            .iter()
            .map(|f| f.rpm)
            .reduce(f64::max)
            .filter(|rpm| *rpm > 0.0),
    })
}

#[derive(Default)]
struct Latest {
    sample: Option<(Sample, Instant)>,
    exited: bool,
}

pub struct Source {
    interval: Duration,
    child: Option<Child>,
    shared: Arc<Mutex<Latest>>,
    last_spawn: Option<Instant>,
}

impl Source {
    pub fn start(interval: Duration) -> Self {
        let mut m = Self {
            interval,
            child: None,
            shared: Arc::default(),
            last_spawn: None,
        };
        m.spawn();
        m
    }

    fn spawn(&mut self) {
        self.reap();
        self.last_spawn = Some(Instant::now());
        let ms = self.interval.as_millis().to_string();
        let Some(mut child) = CANDIDATES.iter().find_map(|bin| {
            Command::new(bin)
                .args(["pipe", "-i", &ms])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        }) else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
        // A fresh slot, so a stale thread can't report on the new child.
        let shared: Arc<Mutex<Latest>> = Arc::default();
        self.shared = shared.clone();
        self.child = Some(child);
        std::thread::spawn(move || read_loop(stdout, shared));
    }

    fn reap(&mut self) {
        CHILD_PID.store(0, Ordering::SeqCst);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The newest sample if fresh. Restarts a dead child, rate limited.
    pub fn latest(&mut self) -> Option<Sample> {
        let (sample, exited) = {
            let guard = self.shared.lock().ok()?;
            (guard.sample.clone(), guard.exited)
        };
        if exited || self.child.is_none() {
            let due = self.last_spawn.is_none_or(|t| t.elapsed() >= RESPAWN_AFTER);
            if due {
                self.spawn();
            }
            return None;
        }
        let max_age = self.interval * 3 + Duration::from_secs(2);
        sample
            .filter(|(_, at)| at.elapsed() <= max_age)
            .map(|(s, _)| s)
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.reap();
    }
}

fn read_loop(stdout: std::process::ChildStdout, shared: Arc<Mutex<Latest>>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::with_capacity(8192);
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(n) if n > 0 => {
                if let (Some(sample), Ok(mut guard)) = (parse_line(&line), shared.lock()) {
                    guard.sample = Some((sample, Instant::now()));
                }
            }
            _ => break,
        }
    }
    if let Ok(mut guard) = shared.lock() {
        guard.exited = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"all_power":12.0,"ane_power":0.0,"cpu_active_ratio":0.25,"cpu_power":6.0,
      "cpu_scaled_ratio":0.2,"cpu_tiers":[
        {"active_ratio":0.125,"cores":[{"active_ratio":0.4,"core_id":0,"die_id":0,"freq_mhz":2000,"scaled_ratio":0.2}],"freq_mhz":1500,"label":"P","scaled_ratio":0.1},
        {"active_ratio":0.5,"cores":[],"freq_mhz":4000,"label":"S","scaled_ratio":0.3}],
      "cpu_usage_pct":0.2,"ecpu_usage":[2000,0.4],"fans":[{"max_rpm":6000,"name":"fan0","rpm":1800}],
      "gpu_active_ratio":0.25,"gpu_usage":[1000,0.1],"memory":{"ram_total":17179869184,"ram_usage":8589934592,"swap_total":0,"swap_usage":0},
      "pcpu_usage":[4000,0.9],"sys_power":30.0,"temp":{"cpu_temp_avg":60.0,"gpu_temp_avg":50.0},"timestamp":"2025-01-01T00:00:00+00:00"}"#;

    #[test]
    fn parses_current_shape() {
        let s = parse_line(SAMPLE).unwrap();
        let cpu = s.cpu.unwrap();
        assert_eq!(cpu.overall, 25.0);
        assert_eq!(cpu.clusters[0].label, "P");
        assert_eq!(cpu.clusters[0].pct, 12.5);
        assert_eq!(cpu.clusters[0].cores, [40.0]);
        assert_eq!(cpu.clusters[1].label, "S");
        assert_eq!(cpu.clusters[1].pct, 50.0);
        assert_eq!(s.gpu_pct, Some(25.0));
        assert_eq!(s.temp_c, Some(60.0));
        assert_eq!(s.watts, Some(30.0));
    }

    #[test]
    fn older_shape_without_tiers() {
        let s = parse_line(
            r#"{"ecpu_usage":[2000,0.25],"pcpu_usage":[3000,0.5],"cpu_usage_pct":0.4,"all_power":5.0}"#,
        )
        .unwrap();
        let cpu = s.cpu.unwrap();
        assert_eq!(cpu.overall, 40.0);
        assert_eq!(cpu.clusters[0].label, "E");
        assert_eq!(cpu.clusters[1].pct, 50.0);
        assert_eq!(s.watts, Some(5.0));
    }

    #[test]
    fn tolerates_missing_and_garbage() {
        assert_eq!(parse_line("{}"), Some(Sample::default()));
        assert_eq!(parse_line("not json"), None);
        assert_eq!(parse_line(r#"{"cpu_tiers":"nope"}"#), None);
    }

    #[test]
    fn zero_sensors_read_as_missing() {
        let s = parse_line(r#"{"temp":{"cpu_temp_avg":0.0},"sys_power":0.0}"#).unwrap();
        assert_eq!(s.temp_c, None);
        assert_eq!(s.watts, None);
    }
}
