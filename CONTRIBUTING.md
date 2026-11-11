# Contributing

Bug reports, fixes and small features are welcome. For anything larger, open an issue first so we can agree on the shape before you spend time on it.

## Reporting a bug

Open an issue with:

- your OS and version, chip (for example M3 Pro, or Ryzen on Ubuntu 24.04) and terminal;
- `agentmon --version`;
- what you saw and what you expected, with a screenshot if it's about layout or colour;
- your terminal's size if the layout is wrong (`tput cols; tput lines`).

Check the screenshot for anything private first: session names are your folder names.

## Setting up

You need a recent stable Rust. Nothing else.

```sh
git clone https://github.com/jameson-m/agentmon
cd agentmon
cargo run -- --demo     # invented data, no Claude sessions needed
cargo run               # your machine
```

## Before you open a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same three on macOS and Linux. If you changed platform code and only have one of them, say which you tested; you can at least type-check the other:

```sh
rustup target add x86_64-unknown-linux-gnu    # or aarch64-apple-darwin
cargo clippy --all-targets --target x86_64-unknown-linux-gnu
```

A pull request should:

- do one thing, with a short description of why;
- come with tests for new behaviour: parsing and layout are plain functions and easy to test (see the `tests` module at the bottom of each file);
- keep working in a narrow pane (about 40 columns) and on both light and dark terminals;
- update the README and the in-app help (`HELP` in `src/ui.rs`) if it changes what users see;
- use invented names and paths in tests and screenshots, never ones from your own machine.

## Where things are

| File | What it holds |
| --- | --- |
| `src/main.rs` | arguments, the event loop, keys |
| `src/app.rs` | state: history, selection, filter |
| `src/ui.rs` | all drawing and layout |
| `src/charts.rs`, `src/palette.rs` | graphs, meters and colours |
| `src/sessions.rs` | grouping processes into Claude sessions |
| `src/claude.rs` | reading Claude Code's session files |
| `src/system*.rs`, `src/procs*.rs`, `src/sensors*.rs`, `src/macmon.rs` | measurement, with one file per platform (`_macos`, `_linux`) |
| `src/demo.rs` | the invented data behind `--demo` |

Platform code stays in the `_macos` and `_linux` files; everything else should build unchanged on both.

If you change how it looks, regenerate the screenshot with `scripts/screenshot.py` (needs tmux and Chrome).

## Releases

Maintainer only: bump `version` in `Cargo.toml`, tag `vX.Y.Z` and push the tag. CI builds the binaries and publishes the release; `scripts/formula.sh X.Y.Z` then produces the Homebrew formula for the tap.

## Licence

By contributing you agree that your work is released under the MIT licence, like the rest of the project.
