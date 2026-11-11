//! Whole-system process sampling. The sampler is per platform.
use std::rc::Rc;

#[cfg(target_os = "macos")]
#[path = "procs_macos.rs"]
mod os;
#[cfg(target_os = "linux")]
#[path = "procs_linux.rs"]
mod os;
pub use os::{Sampler, cwd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A Claude Code CLI process.
    Cli,
    Other,
}

/// Classifies by executable path. The CLI's binary is typically
/// `~/.local/share/claude/versions/2.1.318` (proc_name is just the version),
/// so name alone can't identify it; `ps` shows `claude` only from argv[0].
pub fn classify(path: &str) -> Kind {
    if path.contains("/Claude.app/") {
        return Kind::Other;
    }
    let (dir, base) = path.rsplit_once('/').unwrap_or(("", path));
    let versioned =
        dir.ends_with("/claude/versions") && base.starts_with(|c: char| c.is_ascii_digit());
    if base == "claude" || versioned {
        Kind::Cli
    } else {
        Kind::Other
    }
}

/// Chrome launches `claude --chrome-native-host` for the browser extension. It's
/// the same binary but not a session.
pub fn is_chrome_host(procargs: &[u8]) -> bool {
    const FLAG: &[u8] = b"--chrome-native-host\0";
    procargs.windows(FLAG.len()).any(|w| w == FLAG)
}

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub kind: Kind,
    pub name: Rc<str>,
    /// What it groups under: the `.app` bundle on macOS, the name on Linux.
    pub app: Option<Rc<str>>,
    /// phys_footprint bytes on macOS, anonymous resident bytes on Linux.
    /// None when not readable.
    pub mem: Option<u64>,
    /// Percent of one core. None when not readable.
    pub cpu: Option<f64>,
}

/// Percent of one core used between samples. `prev` is None for a process not
/// seen last tick: if it started since then all its CPU time belongs to this
/// interval, otherwise there's no baseline yet.
pub fn cpu_percent(prev: Option<u64>, now_ns: u64, wall_ns: u64, started_since: bool) -> f64 {
    let delta = match prev {
        Some(prev) => now_ns.saturating_sub(prev),
        None if started_since => now_ns,
        None => 0,
    };
    if wall_ns == 0 {
        0.0
    } else {
        delta as f64 / wall_ns as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_cli_paths() {
        let home = "/Users/me/.local/share/claude/versions/2.1.318";
        assert_eq!(classify(home), Kind::Cli);
        assert_eq!(classify("/opt/homebrew/bin/claude"), Kind::Cli);
        assert_eq!(classify("/Users/me/.local/bin/claude"), Kind::Cli);
    }

    #[test]
    fn desktop_app_is_not_a_cli() {
        for path in [
            "/Applications/Claude.app/Contents/MacOS/Claude",
            "/Applications/Claude.app/Contents/Helpers/disclaimer",
        ] {
            assert_eq!(classify(path), Kind::Other);
        }
    }

    #[test]
    fn unrelated_paths_are_other() {
        for path in [
            "/usr/bin/zsh",
            "/Users/me/.local/share/claude/versions/notes.txt",
            "/Users/me/claude/server",
            "/tmp/claude-501/probe",
        ] {
            assert_eq!(classify(path), Kind::Other);
        }
    }

    #[test]
    fn chrome_host_is_detected_from_argv() {
        let args = b"\x02\0\0\0/a/claude\0\0\0claude\0--chrome-native-host\0PATH=/bin\0";
        assert!(is_chrome_host(args));
        assert!(!is_chrome_host(
            b"\x01\0\0\0/a/claude\0\0claude\0--resume\0"
        ));
        assert!(!is_chrome_host(b""));
    }

    #[test]
    fn cpu_delta() {
        assert_eq!(cpu_percent(Some(1_000), 2_000, 2_000, false), 50.0);
        assert_eq!(cpu_percent(Some(0), 4_000, 2_000, false), 200.0);
    }

    #[test]
    fn cpu_delta_edges() {
        assert_eq!(cpu_percent(None, 1_000, 1_000, false), 0.0);
        assert_eq!(cpu_percent(None, 500, 1_000, true), 50.0);
        assert_eq!(cpu_percent(Some(2_000), 1_000, 1_000, false), 0.0);
        assert_eq!(cpu_percent(Some(0), 1_000, 0, false), 0.0);
    }

    #[test]
    fn samples_own_process() {
        let mut sampler = Sampler::new();
        let procs = sampler.sample();
        let me = std::process::id() as i32;
        let this = procs.iter().find(|p| p.pid == me).expect("self in table");
        assert_eq!(this.ppid, unsafe { libc::getppid() });
        assert!(this.mem.unwrap() > 0);
        assert!(procs.iter().any(|p| p.pid == 1));
        assert!(cwd(me).unwrap().starts_with('/'));
    }
}
