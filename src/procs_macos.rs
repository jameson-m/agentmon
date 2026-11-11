//! macOS process table.
//!
//! `KERN_PROC_ALL` gives pid/ppid/uid/comm for every process in one cheap call,
//! including root processes libproc refuses to describe. Per-process calls are
//! limited to `proc_pid_rusage` for processes we own, plus `proc_pidpath` once
//! per process image.
use super::*;
use crate::ffi;
use std::collections::HashMap;
use std::time::SystemTime;

/// The outermost `.app` bundle in an executable path, so that an app and all
/// its helpers read as one thing.
pub fn app_bundle(path: &str) -> Option<&str> {
    let end = path.find(".app/")?;
    let start = path[..end].rfind('/')? + 1;
    Some(&path[start..end])
}

pub fn ticks_to_ns(ticks: u64, numer: u32, denom: u32) -> u64 {
    if denom == 0 {
        return ticks;
    }
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

// Offsets into Darwin's `struct kinfo_proc`, which libc doesn't define.
const KP_SIZE: usize = 648;
const KP_START_SEC: usize = 0;
const KP_START_USEC: usize = 8;
const KP_STAT: usize = 36;
const KP_PID: usize = 40;
const KP_COMM: std::ops::Range<usize> = 243..243 + 17;
const KP_RUID: usize = 392;
const KP_PPID: usize = 560;
const SZOMB: u8 = 5;

fn read_i32(entry: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes(entry[offset..offset + 4].try_into().expect("4 bytes"))
}

fn read_i64(entry: &[u8], offset: usize) -> i64 {
    i64::from_ne_bytes(entry[offset..offset + 8].try_into().expect("8 bytes"))
}

/// Identifies a process image: pid reuse changes start, exec changes comm.
#[derive(PartialEq, Eq)]
struct Image {
    start: (i64, i32),
    comm: [u8; 17],
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
    buf: Vec<u8>,
    timebase: ffi::MachTimebase,
    uid: u32,
    generation: u64,
    last_wall: Option<SystemTime>,
}

fn exe_path(pid: i32) -> Option<String> {
    let mut buf = [0u8; ffi::PROC_PIDPATHINFO_MAXSIZE];
    let n = unsafe { ffi::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn rusage(pid: i32) -> Option<ffi::RusageInfoV0> {
    let mut info = ffi::RusageInfoV0::default();
    let rc = unsafe { ffi::proc_pid_rusage(pid, ffi::RUSAGE_INFO_V0, (&raw mut info).cast()) };
    (rc == 0).then_some(info)
}

/// `KERN_PROCARGS2`: argc, exec path, then argv and env, NUL separated.
fn procargs(pid: i32) -> Vec<u8> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = buf.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    buf.truncate(if rc == 0 { size } else { 0 });
    buf
}

/// The process's current working directory.
pub fn cwd(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; ffi::VNODE_PATH_INFO_SIZE];
    let n = unsafe {
        ffi::proc_pidinfo(
            pid,
            ffi::PROC_PIDVNODEPATHINFO,
            0,
            buf.as_mut_ptr().cast(),
            buf.len() as libc::c_int,
        )
    };
    let path = (n as usize == buf.len()).then(|| ffi::c_bytes(&buf[ffi::VNODE_CDIR_PATH]))?;
    (!path.is_empty()).then_some(path)
}

impl Sampler {
    pub fn new() -> Self {
        let mut timebase = ffi::MachTimebase::default();
        unsafe { ffi::mach_timebase_info(&mut timebase) };
        Self {
            meta: HashMap::new(),
            buf: vec![0; 2048 * KP_SIZE],
            timebase,
            uid: unsafe { libc::getuid() },
            generation: 0,
            last_wall: None,
        }
    }

    /// Fills `buf` with the process table, returning its length in bytes.
    fn fetch_table(&mut self) -> usize {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
        loop {
            let mut size = self.buf.len();
            let rc = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as u32,
                    self.buf.as_mut_ptr().cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if rc == 0 {
                return size;
            }
            let too_small = std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM);
            if !too_small || self.buf.len() > 256 * 1024 * KP_SIZE {
                return 0;
            }
            let grown = self.buf.len() * 2;
            self.buf.resize(grown, 0);
        }
    }

    pub fn sample(&mut self) -> Vec<Proc> {
        let size = self.fetch_table();
        let now = SystemTime::now();
        let wall_ns = self
            .last_wall
            .and_then(|last| now.duration_since(last).ok())
            .map_or(0, |d| d.as_nanos() as u64);
        let last_unix = self.last_wall.and_then(|last| {
            let d = last.duration_since(SystemTime::UNIX_EPOCH).ok()?;
            Some((d.as_secs() as i64, d.subsec_micros() as i32))
        });
        self.generation += 1;
        let generation = self.generation;
        let readable_all = self.uid == 0;
        let (numer, denom) = (self.timebase.numer, self.timebase.denom);

        let mut procs = Vec::with_capacity(size / KP_SIZE);
        for entry in self.buf[..size].as_chunks::<KP_SIZE>().0 {
            if entry[KP_STAT] == SZOMB {
                continue;
            }
            let pid = read_i32(entry, KP_PID);
            let owned = read_i32(entry, KP_RUID) as u32 == self.uid || readable_all;
            let image = Image {
                start: (
                    read_i64(entry, KP_START_SEC),
                    read_i32(entry, KP_START_USEC),
                ),
                comm: entry[KP_COMM].try_into().expect("17 bytes"),
            };
            let fresh = self.meta.get(&pid).is_none_or(|m| m.image != image);
            if fresh {
                let path = owned.then(|| exe_path(pid)).flatten();
                let mut kind = path.as_deref().map_or(Kind::Other, classify);
                let chrome_host = kind == Kind::Cli && is_chrome_host(&procargs(pid));
                if chrome_host {
                    kind = Kind::Other;
                }
                let name = match (kind, &path) {
                    (Kind::Cli, _) => "claude".to_string(),
                    _ if chrome_host => "claude-chrome-host".to_string(),
                    (_, Some(path)) => path.rsplit('/').next().unwrap_or(path).to_string(),
                    _ => ffi::c_bytes(&image.comm),
                };
                let app = path.as_deref().and_then(app_bundle).map(Rc::from);
                // A new exec of a known pid keeps its CPU baseline.
                let cpu_ns = self
                    .meta
                    .get(&pid)
                    .filter(|m| m.image.start == image.start)
                    .and_then(|m| m.cpu_ns);
                self.meta.insert(
                    pid,
                    Meta {
                        image,
                        kind,
                        name: name.into(),
                        app,
                        cpu_ns,
                        generation,
                    },
                );
            }
            let meta = self.meta.get_mut(&pid).expect("just inserted");
            meta.generation = generation;
            let mut proc = Proc {
                pid,
                ppid: read_i32(entry, KP_PPID),
                kind: meta.kind,
                name: meta.name.clone(),
                app: meta.app.clone(),
                mem: None,
                cpu: None,
            };
            if owned && let Some(usage) = rusage(pid) {
                let now_ns = ticks_to_ns(usage.user_time + usage.system_time, numer, denom);
                let started_since = last_unix.is_some_and(|last| meta.image.start >= last);
                proc.cpu = Some(cpu_percent(meta.cpu_ns, now_ns, wall_ns, started_since));
                proc.mem = Some(usage.phys_footprint);
                meta.cpu_ns = Some(now_ns);
            }
            procs.push(proc);
        }
        self.meta.retain(|_, m| m.generation == generation);
        self.last_wall = Some(now);
        procs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bundle_covers_helpers() {
        for path in [
            "/Applications/Claude.app/Contents/MacOS/Claude",
            "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper",
            "/Applications/Claude.app/Contents/Helpers/disclaimer",
        ] {
            assert_eq!(app_bundle(path), Some("Claude"));
        }
    }

    #[test]
    fn app_bundle_is_the_outermost() {
        let nested = "/Applications/Google Chrome.app/Contents/Frameworks/Helper (GPU).app/x/y";
        assert_eq!(app_bundle(nested), Some("Google Chrome"));
        assert_eq!(app_bundle("/usr/libexec/trustd"), None);
    }

    #[test]
    fn apple_silicon_timebase() {
        assert_eq!(ticks_to_ns(24_000_000, 125, 3), 1_000_000_000);
        assert_eq!(ticks_to_ns(5, 1, 1), 5);
        assert_eq!(ticks_to_ns(5, 1, 0), 5);
    }
}
