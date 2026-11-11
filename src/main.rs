//! agentmon: a narrow-pane system monitor for running many Claude Code sessions.
mod app;
mod charts;
mod claude;
#[cfg(target_os = "macos")]
mod ffi;
mod palette;
mod procs;
mod sensors;
mod sessions;
mod system;
mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_INTERVAL_MS: u64 = 2000;
const MIN_INTERVAL_MS: u64 = 250;
/// Sample quickly once at startup so CPU figures appear right away.
const FIRST_DELTA: Duration = Duration::from_millis(300);
const USAGE: &str = "\
agentmon: system monitor for Claude Code sessions

Usage: agentmon [-i <ms>]

  -i, --interval <ms>  Refresh interval, at least 250 [default: 2000]
  -h, --help           Show this help
  -V, --version        Show version";

const FUSE_WINDOW: Duration = Duration::from_secs(5);
const FUSE_WINDOWS: u32 = 2;
/// Share of one core, over a whole window.
const FUSE_LIMIT: f64 = 0.8;
const LEAVE_SCREEN: &[u8] = b"\x1b[?25h\x1b[?1049l";

static RAW: AtomicBool = AtomicBool::new(false);
static SAVED_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

#[derive(Debug, PartialEq)]
enum Cli {
    Run(Duration),
    Exit(String),
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut interval = DEFAULT_INTERVAL_MS;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-i" | "--interval" => {
                let value = args.next().ok_or("-i needs milliseconds")?;
                interval = value
                    .parse()
                    .map_err(|_| format!("bad interval: {value}"))?;
                if interval < MIN_INTERVAL_MS {
                    return Err(format!("interval must be at least {MIN_INTERVAL_MS}ms"));
                }
            }
            "-h" | "--help" => return Ok(Cli::Exit(USAGE.into())),
            "-V" | "--version" => {
                return Ok(Cli::Exit(format!("agentmon {}", env!("CARGO_PKG_VERSION"))));
            }
            other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
        }
    }
    Ok(Cli::Run(Duration::from_millis(interval)))
}

fn restore_terminal() {
    if RAW.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn kill_child() {
    let child = sensors::CHILD_PID.load(Ordering::SeqCst);
    if child > 0 {
        unsafe { libc::kill(child, libc::SIGTERM) };
    }
}

/// Exits here rather than flagging the main loop: once the terminal hangs up,
/// crossterm spins in `read()` returning 0 and never gives control back.
/// Only async-signal-safe calls from here on.
extern "C" fn on_signal(signal: libc::c_int) {
    kill_child();
    if RAW.load(Ordering::SeqCst) {
        unsafe {
            if let Some(termios) = SAVED_TERMIOS.get() {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, termios);
            }
            libc::write(
                libc::STDOUT_FILENO,
                LEAVE_SCREEN.as_ptr().cast(),
                LEAVE_SCREEN.len(),
            );
        }
    }
    unsafe { libc::_exit(128 + signal) };
}

fn own_cpu_time() -> Duration {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let micros = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime))
}

/// True once `busy` has hit the limit `FUSE_WINDOWS` times running.
fn fuse_blown(strikes: &mut u32, busy: Duration) -> bool {
    *strikes = if busy >= FUSE_WINDOW.mul_f64(FUSE_LIMIT) {
        *strikes + 1
    } else {
        0
    };
    *strikes >= FUSE_WINDOWS
}

/// Covers a hangup that delivers no signal, which leaves that same spin. No tty
/// call reports it on macOS, so this watches for the spin itself: a monitor
/// pegging a core is never worth keeping alive.
fn start_fuse() {
    std::thread::spawn(|| {
        let mut strikes = 0;
        let mut last = own_cpu_time();
        loop {
            std::thread::sleep(FUSE_WINDOW);
            let now = own_cpu_time();
            if fuse_blown(&mut strikes, now.saturating_sub(last)) {
                kill_child();
                unsafe { libc::_exit(1) };
            }
            last = now;
        }
    });
}

fn install_handlers() {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        unsafe { libc::signal(signal, handler) };
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

struct Collector {
    memory: system::MemorySampler,
    cpu: system::CpuSampler,
    procs: procs::Sampler,
    sensors: sensors::Source,
    claude: claude::Reader,
}

impl Collector {
    fn tick(&mut self, app: &mut app::App) {
        let memory = self.memory.sample();
        // Always sampled, to keep the fallback's baseline current.
        let ticks = self.cpu.sample();
        let sample = self.sensors.latest();
        let cpu = sample
            .as_ref()
            .and_then(|m| m.cpu.clone())
            .or(ticks)
            .unwrap_or_default();
        let procs = self.procs.sample();
        let mut grouped = sessions::group(&procs);
        app::label_sessions(&mut grouped.sessions, procs::cwd);
        let mut meta = std::collections::HashMap::new();
        for session in &mut grouped.sessions {
            if let Some(found) = self.claude.read(session.pid) {
                if let Some(name) = &found.name {
                    session.name = name.clone();
                }
                meta.insert(session.pid, found);
            }
        }
        self.claude.retain(|pid| meta.contains_key(&pid));
        app.ingest(memory, cpu, sample, grouped, procs.len());
        app.session_meta = meta;
        app.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
    }
}

/// Applies a key. Returns true to quit.
fn on_key(app: &mut app::App, code: KeyCode, mods: KeyModifiers) -> bool {
    if code == KeyCode::Char('q')
        || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL))
    {
        return true;
    }
    if app.help {
        app.help = false;
        return false;
    }
    match code {
        KeyCode::Char('?') => app.help = true,
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(true),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(false),
        KeyCode::Enter => app.toggle_expanded(),
        _ => {}
    }
    false
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    collector: &mut Collector,
    interval: Duration,
) -> io::Result<()> {
    let mut app = app::App::default();
    collector.tick(&mut app);
    let mut next_tick = Instant::now() + FIRST_DELTA.min(interval);
    let mut dirty = true;
    loop {
        if Instant::now() >= next_tick {
            collector.tick(&mut app);
            next_tick = Instant::now() + interval;
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            dirty = false;
        }
        let timeout = next_tick.saturating_duration_since(Instant::now());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if on_key(&mut app, key.code, key.modifiers) {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Resize(..) => {
                    terminal.autoresize()?;
                    dirty = true;
                }
                _ => {}
            }
        }
    }
}

fn start(interval: Duration) -> io::Result<()> {
    install_handlers();
    palette::detect_truecolor();
    let mut termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut termios) } == 0 {
        let _ = SAVED_TERMIOS.set(termios);
    }
    enable_raw_mode()?;
    RAW.store(true, Ordering::SeqCst);
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    start_fuse();
    let mut collector = Collector {
        memory: system::MemorySampler::new(),
        cpu: system::CpuSampler::new(),
        procs: procs::Sampler::new(),
        sensors: sensors::Source::start(interval),
        claude: claude::Reader::new(),
    };
    run(&mut terminal, &mut collector, interval)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let interval = match parse_args(&args) {
        Ok(Cli::Run(interval)) => interval,
        Ok(Cli::Exit(text)) => {
            println!("{text}");
            return;
        }
        Err(message) => {
            eprintln!("agentmon: {message}");
            std::process::exit(2);
        }
    };
    if let Err(error) = start(interval) {
        restore_terminal();
        eprintln!("agentmon: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&args)
    }

    #[test]
    fn interval_flag() {
        assert_eq!(
            parse(&[]),
            Ok(Cli::Run(Duration::from_millis(DEFAULT_INTERVAL_MS)))
        );
        assert_eq!(
            parse(&["-i", "1000"]),
            Ok(Cli::Run(Duration::from_millis(1000)))
        );
        assert!(parse(&["-i"]).is_err());
        assert!(parse(&["-i", "fast"]).is_err());
        assert!(parse(&["-i", "10"]).is_err());
        assert!(parse(&["--nope"]).is_err());
    }

    #[test]
    fn fuse_needs_consecutive_pegged_windows() {
        let (idle, pegged) = (Duration::from_millis(50), FUSE_WINDOW);
        let mut strikes = 0;
        assert!(!fuse_blown(&mut strikes, pegged));
        assert!(!fuse_blown(&mut strikes, idle));
        assert!(!fuse_blown(&mut strikes, pegged));
        assert!(fuse_blown(&mut strikes, pegged));
    }

    #[test]
    fn keys() {
        let mut app = app::App::default();
        let none = KeyModifiers::NONE;
        assert!(!on_key(&mut app, KeyCode::Char('s'), none));
        assert_eq!(app.sort, sessions::SortKey::Cpu);
        assert!(!on_key(&mut app, KeyCode::Char('?'), none));
        assert!(app.help);
        assert!(!on_key(&mut app, KeyCode::Char('s'), none));
        assert!(!app.help);
        assert_eq!(app.sort, sessions::SortKey::Cpu);
        assert!(on_key(&mut app, KeyCode::Char('q'), none));
        assert!(on_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
    }
}
