//! Linux process table, from `/proc`.
//!
//! Each process costs two small reads a tick: `stat` for its parent, state and
//! CPU time, and `statm` for memory. Both are world-readable, so other users'
//! processes are measured too. The executable path and arguments are read once
//! per process image, and only for processes we own.
use super::*;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::Instant;

/// `PF_KTHREAD`: a kernel thread, which has no memory of its own to show.
const KERNEL_THREAD: u64 = 0x0020_0000;

/// What one line of `/proc/<pid>/stat` says.
#[derive(Debug, PartialEq)]
struct Stat<'a> {
    comm: &'a str,
    state: char,
    ppid: i32,
    flags: u64,
    /// User plus system time, in clock ticks.
    cpu_ticks: u64,
    /// Clock ticks after boot.
    start: u64,
}

/// The name is in brackets and may itself hold spaces and brackets, so the
/// numbered fields are found from the last `)`.
fn parse_stat(line: &str) -> Option<Stat<'_>> {
    let (open, close) = (line.find('(')?, line.rfind(')')?);
    let fields: Vec<&str> = line.get(close + 1..)?.split_whitespace().collect();
    let number = |i: usize| fields.get(i)?.parse::<u64>().ok();
    Some(Stat {
        comm: line.get(open + 1..close)?,
        state: fields.first()?.chars().next()?,
        ppid: fields.get(1)?.parse().ok()?,
        flags: number(6)?,
        cpu_ticks: number(11)? + number(12)?,
        start: number(19)?,
    })
}

/// Anonymous resident pages from `statm`: resident less file and shmem pages.
/// Unlike RSS it doesn't count shared libraries once per process.
fn parse_statm(line: &str) -> Option<u64> {
    let mut fields = line.split_whitespace().skip(1);
    let resident: u64 = fields.next()?.parse().ok()?;
    let shared: u64 = fields.next()?.parse().ok()?;
    Some(resident.saturating_sub(shared))
}

/// Identifies a process image: pid reuse changes start, exec changes comm.
#[derive(PartialEq, Eq)]
struct Image {
    start: u64,
    comm: String,
}

struct Meta {
    image: Image,
    kind: Kind,
    name: Rc<str>,
    app: Option<Rc<str>>,
    cpu_ns: Option<u64>,
    generation: u64,
}

pub struct Sampler {
    meta: HashMap<i32, Meta>,
    uid: u32,
    page_size: u64,
    ticks_per_sec: u64,
    generation: u64,
    /// When the last sample was taken, and in clock ticks after boot.
    last: Option<(Instant, u64)>,
}

fn exe_path(pid: i32) -> Option<String> {
    let path = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let path = path.to_string_lossy();
    // A binary replaced by an update while it runs.
    Some(path.strip_suffix(" (deleted)").unwrap_or(&path).to_string())
}

/// The process's current working directory.
pub fn cwd(pid: i32) -> Option<String> {
    let path = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    Some(path.to_string_lossy().into_owned())
}

fn sysconf(name: libc::c_int, fallback: u64) -> u64 {
    u64::try_from(unsafe { libc::sysconf(name) })
        .ok()
        .filter(|&v| v > 0)
        .unwrap_or(fallback)
}

/// Clock ticks since boot, on the clock process start times use.
fn boot_ticks(ticks_per_sec: u64) -> u64 {
    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) };
    now.tv_sec as u64 * ticks_per_sec + now.tv_nsec as u64 * ticks_per_sec / 1_000_000_000
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            meta: HashMap::new(),
            uid: unsafe { libc::getuid() },
            page_size: sysconf(libc::_SC_PAGESIZE, 4096),
            ticks_per_sec: sysconf(libc::_SC_CLK_TCK, 100),
            generation: 0,
            last: None,
        }
    }

    pub fn sample(&mut self) -> Vec<Proc> {
        let now = (Instant::now(), boot_ticks(self.ticks_per_sec));
        let wall_ns = self
            .last
            .map_or(0, |(at, _)| now.0.duration_since(at).as_nanos() as u64);
        let last_ticks = self.last.map(|(_, ticks)| ticks);
        self.generation += 1;
        let generation = self.generation;

        let mut procs = Vec::with_capacity(self.meta.len() + 16);
        let entries = fs::read_dir("/proc").into_iter().flatten().flatten();
        for entry in entries {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            // The process may exit between any two of these reads.
            let Ok(line) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Some(stat) = parse_stat(&line) else {
                continue;
            };
            if stat.state == 'Z' || stat.flags & KERNEL_THREAD != 0 {
                continue;
            }
            let fresh = self
                .meta
                .get(&pid)
                .is_none_or(|m| m.image.start != stat.start || m.image.comm != stat.comm);
            if fresh {
                let owner = entry.metadata().map(|m| m.uid()).ok();
                let owned = self.uid == 0 || owner == Some(self.uid);
                let path = owned.then(|| exe_path(pid)).flatten();
                let mut kind = path.as_deref().map_or(Kind::Other, classify);
                let chrome_host = kind == Kind::Cli
                    && fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|a| is_chrome_host(&a));
                if chrome_host {
                    kind = Kind::Other;
                }
                let name: Rc<str> = match (kind, &path) {
                    (Kind::Cli, _) => "claude".into(),
                    _ if chrome_host => "claude-chrome-host".into(),
                    (_, Some(path)) => path.rsplit('/').next().unwrap_or(path).into(),
                    _ => stat.comm.into(),
                };
                // A new exec of a known pid keeps its CPU baseline.
                let cpu_ns = self
                    .meta
                    .get(&pid)
                    .filter(|m| m.image.start == stat.start)
                    .and_then(|m| m.cpu_ns);
                self.meta.insert(
                    pid,
                    Meta {
                        image: Image {
                            start: stat.start,
                            comm: stat.comm.to_string(),
                        },
                        kind,
                        // Processes of one program read as one thing.
                        app: (kind == Kind::Other).then(|| name.clone()),
                        name,
                        cpu_ns,
                        generation,
                    },
                );
            }
            let meta = self.meta.get_mut(&pid).expect("just inserted");
            meta.generation = generation;
            let now_ns = stat.cpu_ticks * 1_000_000_000 / self.ticks_per_sec;
            let started_since = last_ticks.is_some_and(|last| stat.start >= last);
            let cpu = cpu_percent(meta.cpu_ns, now_ns, wall_ns, started_since);
            meta.cpu_ns = Some(now_ns);
            let mem = fs::read_to_string(format!("/proc/{pid}/statm"))
                .ok()
                .and_then(|line| parse_statm(&line));
            procs.push(Proc {
                pid,
                ppid: stat.ppid,
                kind: meta.kind,
                name: meta.name.clone(),
                app: meta.app.clone(),
                mem: mem.map(|pages| pages * self.page_size),
                cpu: Some(cpu),
            });
        }
        self.meta.retain(|_, m| m.generation == generation);
        self.last = Some(now);
        procs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_survives_awkward_names() {
        let line = "42 (tmux: server (1)) S 1 42 42 0 -1 4194560 100 0 0 0 \
                    7 3 0 0 20 0 1 0 5000 1000000 200 18446744073709551615 0 0";
        assert_eq!(
            parse_stat(line),
            Some(Stat {
                comm: "tmux: server (1)",
                state: 'S',
                ppid: 1,
                flags: 4194560,
                cpu_ticks: 10,
                start: 5000,
            })
        );
        assert_eq!(parse_stat("42 (short) S 1"), None);
        assert_eq!(parse_stat(""), None);
    }

    #[test]
    fn statm_counts_anonymous_pages() {
        assert_eq!(parse_statm("5000 1200 300 10 0 900 0"), Some(900));
        assert_eq!(parse_statm("5000 100 300"), Some(0));
        assert_eq!(parse_statm("5000"), None);
    }
}
