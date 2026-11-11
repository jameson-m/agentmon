# agentmon

A graphical terminal system monitor for macOS on Apple Silicon and for Linux, built to sit in a narrow side pane next to Claude Code sessions, and to be readable from across the desk. Needs no sudo. Works from about 40 columns, on light and dark terminals.

## What you see

Four boxed panels, top to bottom:

- **cpu**: overall load in the title. On the left, the first cluster's history grows up from the middle and the last cluster's is mirrored below it. On the right, the total as a meter, every core (`P0`, `S0`, …) with its own history, in one column or two, then GPU load, power and temperature with their history, and power by part, GPU temperature and fan speed. Without room for every core, each cluster gets a row with one bar per core. Short or narrow panels put each cluster's figures over its own graph instead.
- **mem**: used and total in the title, with the pressure level; under pressure the whole box turns amber or red. A history graph coloured by the pressure at each moment, then two columns of meters: what RAM is made of (app, wired, compressed, cached, free, swap) and who has it (all Claude Code sessions as one total, the biggest apps, the rest, and system).
- **sessions**: ● working, ◆ went idle in the last 10 minutes, so probably waiting on you, ○ idle. Then how long it has been that way, the tokens in its context, a memory bar and CPU history. Colours match the mem panel and last for the session's life.
- **processes**: everything else, with an app's helpers grouped.

Column headings sit in the list borders, with the sorted one marked ▾; keys are in the bottom border. Graphs shrink first when the pane is short, then the process list, then the memory breakdown folds to one bar. From 110 columns, graphs go on the left and lists on the right. Colours are fixed 256-colour mid-tones with at least 3:1 contrast on both white and black; text uses your terminal's foreground.

## Install

A prebuilt binary, for macOS on Apple Silicon and Linux on x86_64 or arm64, into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/jameson-m/agentmon/main/install.sh | sh
```

Or from source, with a recent stable Rust:

```sh
cargo install --git https://github.com/jameson-m/agentmon
```

[macmon](https://github.com/vladkens/macmon) is optional. With it you get temperature, watts and GPU; without it (or if it dies) CPU, including per-core bars, falls back to host statistics, and agentmon retries macmon every 30s.

On Linux nothing else is needed: everything comes from `/proc` and `/sys`. An NVIDIA GPU is read through `nvidia-smi` if it's installed.

## Usage

```sh
agentmon [-i <ms>]   # refresh interval, default 2000, minimum 250
```

| Key | Action |
| --- | --- |
| `q` | quit |
| `s` | cycle sort: mem, cpu |
| `↑` `↓` or `j` `k` | select session |
| `Enter` | expand process tree |
| `?` | help |

## How it measures

- **Process memory** is `phys_footprint` from `proc_pid_rusage`, the Memory column of Activity Monitor, not RSS.
- **Used memory** is Activity Monitor's Memory Used: app (internal minus purgeable) + wired + compressed, from `host_statistics64`.
- **Pressure** is `kern.memorystatus_vm_pressure_level`: Normal, Warn or Critical.
- **Row CPU** is the cputime delta between ticks, where 100% is one core. Overall and cluster figures are 0-100% of those cores. Green is under 60, amber under 85, then red.
- **Who has the RAM** shows wired as system and splits the rest of used memory by footprint. Footprints include compressed and swapped pages at full size, so stacked directly they'd exceed physical RAM. Labels print real footprints and can add up to more than used.

### On Linux

- **Process memory** is anonymous resident memory (`statm` resident less shared), so shared libraries aren't counted once per process. Processes of the same program share a row.
- **Used memory** is total less `MemAvailable`. The breakdown is app (anonymous + shmem), kern (unreclaimable slab, kernel stacks and page tables, in place of wired), comp (the zswap pool) and cache.
- **Pressure** is Warn when tasks stalled on memory for 5% of the last ten seconds (`/proc/pressure/memory`) or under 10% of RAM is available, and Critical when all tasks stalled that much or under 5% is available.
- **Cores** are `P` and `E` on Intel hybrid chips and on arm64 big.LITTLE (`M` for a middle tier), and `C` everywhere else.
- **Sensors**: CPU temperature and fans from hwmon; power from the battery while discharging, else RAPL, which most kernels only let root read; GPU load, temperature and power from an AMD card's sysfs files or from `nvidia-smi`. Whatever is missing is left out.

## What counts as a session

A session is the topmost `claude` CLI process in an ancestry, plus all its descendants by ppid: subagents, MCP servers, shells and build tools. Its name is the one you gave it in Claude Code, or else the basename of its cwd; sessions sharing a folder keep separate rows, numbered `name`, `name#2` oldest first. Status, its age and context size come from Claude Code's own files: `~/.claude/sessions/<pid>.json` and the end of the session's transcript. Where those are missing, a session counts as working if it reached 5% of a core in its last three samples.

- Children reparented to launchd or init (daemons) can't be traced back and show under PROCESSES.
- The Claude desktop app is one row, like any app, never a session.
- `claude --chrome-native-host` isn't a session.
- CLI installs that run under `node` aren't detected; only native `claude` binaries are.

On macOS, other users' processes can't be read without root. They count towards a session's process total, but add no memory or CPU, and outside sessions they're tallied as `n/a`.

## Development

```sh
cargo test
cargo clippy --all-targets
scripts/build-linux.sh   # static Linux binaries into dist/, from macOS or Linux
```

Tagging `v*` builds the binaries and publishes a release.

## License

MIT
