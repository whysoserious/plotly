# plotly

A terminal UI (Rust) that drives an **iDraw 2.0** pen plotter running the
**DrawCore** firmware — a Grbl-style G-code dialect. Load an SVG (or a short
piece of text), jog the head to the corner of your sheet, press <kbd>Enter</kbd>,
and watch the plot progress with a live time/ink estimate. A job can be paused,
stopped, and resumed after a crash or a power cut.

The design document — hardware protocol, measurements, open questions — is
[`DESIGN.org`](DESIGN.org); it is the source of truth for *why* things work the
way they do. This file only covers how to build and run the thing.

- Language: Rust, edition 2021, no async (blocking `serialport` + threads).
- TUI: `ratatui` + `crossterm`. Logs go to a **file**, never to stdout.
- Target hardware: iDraw 2.0 only (USB CH340, `1A86:7523` / `1A86:8040`,
  115200 8N1).

---

## 1. Requirements

**Toolchain.** Stable Rust, pinned by [`rust-toolchain.toml`](rust-toolchain.toml)
(channel `stable`, with `rustfmt` and `clippy`). With `rustup` installed, the
right toolchain and components are fetched automatically on the first cargo
command. Verified on rustc 1.97.1.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if you have no rustup yet
```

**System libraries (Linux).** `serialport` enumerates USB devices through
libudev, so the dev package and `pkg-config` are needed to *build*:

```bash
sudo apt install pkg-config libudev-dev        # Debian / Ubuntu
```

**Serial port access.** The board shows up as `/dev/ttyACM0` (it enumerates as
CDC-ACM, not `ttyUSB*`), owned by `root:dialout`. Your user must be in the
`dialout` group, otherwise opening the port fails with "permission denied":

```bash
sudo usermod -aG dialout "$USER"   # then log out and back in
id -nG | tr ' ' '\n' | grep dialout
```

No hardware is needed for anything except actually plotting — see `--simulate`.

## 2. Build

```bash
cargo build                  # debug binary at target/debug/plotly
cargo build --release        # optimised, target/release/plotly
```

No build script of its own, no codegen, no vendored assets: `cargo build` is the
whole build. `Cargo.lock` is committed on purpose (this is an application), so
builds are reproducible.

Long plots are worth the release build — an A0 drawing runs for hours and the
worker thread is the one feeding the machine.

## 3. Run

The binary is `plotly`; during development `cargo run -- <args>` is equivalent
(note the `--` separating cargo's arguments from the program's). plotly is a
full-screen TUI, so it needs a real terminal — piping its output or running it
without a TTY fails once the pre-flight checks are done.

### Without hardware — start here

```bash
cargo run -- --simulate paper-sizes.svg
```

`--simulate` swaps the serial link for an in-process `MockTransport` that answers
every command like the real board does (it even reports the same `$$` dump, so
the same profile resolves). Nothing moves, and the status bar says
`SIMULATION — nothing moves` so a silent plotter is never a mystery.
`--simulate` also wins over `--port`, so a dry run cannot accidentally open real
hardware.

### With the plotter

```bash
cargo run -- drawing.svg                        # auto-detect the port
cargo run -- --port /dev/ttyACM0 drawing.svg    # force it
cargo run --release -- example.svg              # long plots: use the release build
```

Startup order — everything that can fail does so on a normal terminal, before
the TUI takes over the screen:

1. the SVG (or `--text`) is parsed into polylines in millimetres;
2. the port is resolved (`--simulate` → mock, `--port` → verbatim, otherwise the
   first device matching the iDraw VID:PID);
3. the board is greeted: listen for the boot banner, `$B`, then `v` for the
   firmware version;
4. `$$` is read and the machine profile is settled (§4 below);
5. the firmware settings the config asks for are written to the board;
6. if an unfinished job from a previous run matches this drawing, a resume
   prompt appears (<kbd>Enter</kbd> resumes, <kbd>n</kbd>/<kbd>Esc</kbd>
   declines);
7. the TUI opens: status bar on top, toolpath canvas in the middle, log tail at
   the bottom.

**Where the drawing lands.** The drawing's top-left corner is placed at the
head's *current* position — you jog to the corner of your sheet and start from
there. Press <kbd>f</kbd> first to trace the bounding box with the pen up; if
the drawing would run off the field, the log says so instead of letting the
carriage find out.

### What plotly does with an SVG

- **Geometry only.** Paths, and the basic shapes/`use`/transforms `usvg`
  resolves into paths. `<text>` and raster images are **skipped** (no fonts are
  loaded, so usvg logs a font warning when it meets text); convert text to
  paths in your editor first, or use `--text`.
- **Fill vs stroke is ignored** — the pen draws outlines. Each subpath becomes
  one pen-down stroke, and curves are flattened to polylines at a 0.1 mm
  tolerance.
- **Units.** `mm`/`in` in the SVG are honoured; a unitless document is read at
  the CSS default of 96 px per inch.
- **Order is the file's order.** plotly deliberately does no reordering,
  joining or deduplication — that is `vpype`'s job before the file gets here
  (DESIGN.org §14, step 5.2), and the "Strokes" panel (<kbd>s</kbd>) shows
  exactly what the file contains.
- **Size.** A drawing that fits keeps its true size; one larger than the field
  (minus a 5 mm margin per side) is scaled down to fit. It is never enlarged.

### Writing text instead of an SVG

```bash
cargo run -- --simulate --text "PLOTLY A0" --text-height 12
```

The built-in single-stroke font covers `0-9`, `A-Z`, space and `- . : / +`
(lowercase is upper-cased). Unknown characters leave a blank of one advance.
`--text-height` is the cap height in millimetres (default 10).

### Resuming an interrupted plot

Resume is **automatic and interactive**: every run scans the job directory, and
if an unfinished job is found a prompt appears before anything moves —
`Resume job N (XX%)?`, <kbd>Enter</kbd> resumes, <kbd>n</kbd>/<kbd>Esc</kbd>
starts fresh.

```bash
cargo run -- drawing.svg        # offers the unfinished job made from drawing.svg
cargo run --                    # started bare: offers any unfinished job
```

Started on a file or some text, only a job made from that same source is
offered, so coming back to a different drawing never lands on someone else's
leftovers. A resumed plan keeps its **original absolute coordinates** — it is
not re-placed under the head, since that would tear the drawing in two. So if
the machine lost its position (power cut, `$SLP`), press <kbd>h</kbd> to home
before accepting the resume.

Jobs live in `~/.local/share/plotly/jobs/<job_id>/` (`plan.jsonl` — one op per
line in absolute mm, `meta.json`, `progress.json`). An `ok` from Grbl means
"queued", not "drawn", so the committed index backs off by the planner depth
(15 blocks) and a few ops may be redrawn rather than skipped; repeated absolute
moves are idempotent, so that costs nothing on paper.

> **Note:** the `--resume` / `--resume-overlap` flags are defined in the CLI but
> are not wired to anything yet — the startup prompt is the only way to resume
> today, and the overlap is always 0. They parse and they are listed in `--help`;
> they just have no effect.

### Full CLI

```
Usage: plotly [OPTIONS] [SVG_FILE]

Arguments:
  [SVG_FILE]  SVG file to load and draw

Options:
      --text <STRING>       Write this text with the built-in single-stroke font instead of an SVG
      --text-height <MM>    Cap height for --text, in millimetres [default: 10]
      --port <PATH>         Force the serial port path; default is auto-detect (CH340 1A86:7523/8040)
      --baud <N>            Serial baud rate (firmware is fixed at 115200; rarely needed) [default: 115200]
      --profile <NAME>      Machine profile (idraw-a0, idraw-a1, idraw-a2, idraw-a3, idraw-a4,
                            idraw-xlx, idraw-b6, idraw-minikit). Default: read from the machine
      --simulate            Use the in-process MockTransport instead of real hardware
      --log-file <PATH>     Log file path. Default: a new file for every run in ./logs/,
                            named after the start time and the drawing
      --log-level <LEVEL>   Log level for the file; overridden by -v/-vv and --no-log. The
                            default records every line on the wire, so a plot can be
                            debugged afterwards [default: trace]
                            [possible values: off, info, debug, trace]
  -v...                     Raise verbosity to at least -v = debug, -vv = trace
      --no-log              Disable logging entirely (wins over --log-level and -v)
      --resume[=<JOB_ID>]   Resume an interrupted job: --resume (latest) or --resume=<JOB_ID>
      --resume-overlap <K>  Repeat the last K pen-down segments when resuming (for ink continuity)
  -h, --help                Print help
  -V, --version             Print version
```

Started bare (`cargo run -- --simulate`), plotly is still useful: jog, pen, home
and the raw G-code console all work with nothing loaded.

## 4. Keys

Input is **modal**: in navigation mode single keys are commands, while the raw
G-code console is open the same keys are text (typing `M3 S100` must not fire
STOP on the `S`). <kbd>?</kbd> shows this list in the app.

### Navigation

| Key | What it does |
|---|---|
| arrows | jog XY by the current step (up = up the page) |
| <kbd>+</kbd> / <kbd>-</kbd> | jog step: 0.1 / 1 / 5 / 10 mm |
| <kbd>Enter</kbd> | draw the loaded SVG — or carry on from a cutoff's stop, or resume at the resume prompt |
| <kbd>f</kbd> | trace the drawing's outline, pen up — where will it land? |
| <kbd>t</kbd> | safety timer: off / 1 / 5 / 15 min (stop + pen up) |
| <kbd>m</kbd> | distance stop: off / 50 / 100 / 500 cm (finishes the shape, pen up) |
| <kbd>M</kbd> | distance stop at any number of cm — type it, <kbd>Enter</kbd> arms it |
| <kbd>[</kbd> / <kbd>PgUp</kbd> | pen up |
| <kbd>]</kbd> / <kbd>PgDn</kbd> | pen down |
| <kbd>.</kbd> / <kbd>,</kbd> | press the pen harder / lighter (Z by 0.05 mm) |
| <kbd>space</kbd> | toggle the pen |
| <kbd>h</kbd> | home the machine (lifts the pen first, then `$H`) |
| <kbd>d</kbd> | release the motors (`$SLP`) — position unknown afterwards |
| <kbd>Esc</kbd> / <kbd>r</kbd> | pause after the current shape / resume |
| <kbd>S</kbd> | stop the plot, pen up |
| <kbd>Ctrl-C</kbd> / <kbd>Ctrl-X</kbd> | panic: pen up + soft reset (works in the console too) |
| <kbd>s</kbd> | show/hide the stroke list |
| <kbd>c</kbd> | raw G-code console |
| <kbd>?</kbd> / <kbd>F1</kbd> | this list |
| <kbd>q</kbd> | quit |

### Distance stop (<kbd>M</kbd>)

A line for the centimetres to stop after, in the row the console uses. Typed
digits (and `.` or `,`) are text here, so <kbd>S</kbd> and <kbd>q</kbd> do
nothing until the prompt is closed; <kbd>Ctrl-C</kbd> still panics.

| Key | What it does |
|---|---|
| <kbd>Enter</kbd> | arm the cutoff at that many cm — on an empty line, switch it off |
| <kbd>Backspace</kbd> | delete a character |
| <kbd>Esc</kbd> | close the prompt, leaving whatever was armed |

**The shape is always finished first.** Spending the budget only arms the
stop; what makes it take is the pen coming up at the end of the shape being
drawn, so the plot never leaves half a line and a blob on the paper. Two things
follow: the travel counter reads *past* the distance that was armed (the status
bar says `stopping after this shape` while it draws it out), and a budget spent
inside the **last** shape stops nothing — there is no shape left to cut short,
so the plot ends as a finished plot, motors released and all.

**<kbd>Enter</kbd> after a cutoff carries the plot on** from the op it stopped
at, with the drawing exactly where it was and **no homing** — the head is still
where the plan left it, so the plot picks up from that point even if you jogged
away to look at the ink. The status bar says so while it stands
(`stopped at 50% - enter carries on, S starts over`), and <kbd>S</kbd> is how
you throw the run away instead: it drops the stop point, so the next
<kbd>Enter</kbd> lays the drawing down under the pen again. A job picked up
*after a restart* is the other case — there the position is only a number in a
file, so that one does home first.

Armed before a plot, the cutoff counts from the start of the drawing and is
re-armed by every <kbd>Enter</kbd> until it fires. Typed **while a plot runs**
it takes effect at once and counts from there — the status note says
`distance stop … from here` — which is how you let a running plot draw another
20 cm and then stop with the pen up. A cutoff that fires disarms itself and
names itself in the status bar.

The safety timer (<kbd>t</kbd>) does *not* wait for a shape: it bounds how long
the machine runs unattended, and a shape that takes ten more minutes is exactly
what it is there to cut short.

### Console (<kbd>c</kbd>)

| Key | What it does |
|---|---|
| <kbd>Enter</kbd> | send the line |
| <kbd>Backspace</kbd> | delete a character |
| <kbd>Esc</kbd> | close the console |
| <kbd>Ctrl-C</kbd> / <kbd>Ctrl-X</kbd> | panic: pen up + reset |

Higher Z is **lower** on this machine: pen down is `Z=5`, pen up `Z=0.5`.

## 5. Configuration

Optional TOML, read from the per-OS config directory:

```
~/.config/plotly/config.toml          # Linux ($XDG_CONFIG_HOME/plotly/)
```

[`default.conf`](default.conf) in this repo is the **annotated** version of
every knob — what it does, what it costs in plot time, and which ones are worth
touching. It is documentation: plotly does not read it. Copy the lines you want
into `config.toml`.

```toml
[profiles.idraw-a0]
pen_down_z = 5.4      # this pen needs a little more
draw_feed = 1500      # and a slower hand
```

Sections are keyed by profile name (`--profile idraw-a3` reads
`[profiles.idraw-a3]`). A misspelt key is a **startup error**, not a shrug — a
silently dropped `pen_dwon_z` would plot at the wrong pen height.

How a profile is settled:

1. the built-in table for the name (`idraw-a0` by default) gives field size,
   feeds and pen heights;
2. **without** `--profile`, the board's own `$$` report overrides field size and
   speed/acceleration limits — plotly asks the machine how big it is and
   believes it. Naming a profile explicitly suppresses this;
3. your `config.toml` has the last word;
4. four of those keys are written *back* to the board's EEPROM — but only when
   your config names them explicitly and the board reports a different value:
   `accel_mm_s2` → `$120`+`$121`, `junction_deviation_mm` → `$11`,
   `z_max_feed` → `$112`, `z_accel_mm_s2` → `$122`. A failed write is a warning,
   not a fatal error: the drawing still runs, just with the machine's own idea
   of how hard to stop.

The resolved values are logged at startup, so what is in force can be checked
rather than remembered:

```bash
grep "machine profile" "$(ls -t logs/*.log | head -1)"
```

Built-in profiles: `idraw-a0` (841×1189), `idraw-a1` (864×594), `idraw-a2`
(594×432), `idraw-a3` (430×297), `idraw-a4` (300×210), `idraw-xlx` (595×218),
`idraw-b6` (190×140), `idraw-minikit` (160×101.6) — millimetres.

## 6. Logs and state on disk

| What | Where |
|---|---|
| Log files | one per run: `./logs/<YYYY-MM-DD HH.MM.SS> <drawing>.log` (`--log-file` for another path). Level `trace` by default — every G-code line and every reply, microsecond timestamps, nothing dropped — so a big plot writes a few hundred MB; `--log-level info` for a small log, `--no-log` for none. The first lines record the version, the command line, the working directory and the drawing's full path, size and modification time. The path is printed when plotly exits. `logs/` is git-ignored. |
| Live log tail | bottom panel of the TUI (last 1000 lines, `info` and up) |
| Job directories | `~/.local/share/plotly/jobs/<job_id>/` |
| Config | `~/.config/plotly/config.toml` |

Logs never go to stdout — the TUI owns the terminal. A panic and a termination
signal both restore the terminal and lift the pen on the way out.

## 7. Checks and tests

The full set, exactly as CI-worthy as it gets here:

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

`cargo test` runs the unit tests inside the modules plus the integration suites
in [`tests/`](tests), all of them against the mock — **no hardware required**:

| Suite | Covers |
|---|---|
| `tests/connect.rs` | the whole `--simulate` startup path through the public API |
| `tests/pen.rs` | the exact wire traffic of a pen move (`F` is modal, so order matters) |
| `tests/machine.rs` | homing, motor release and jog, from key press to bytes |
| `tests/console.rs` | modal input: `M3 S100` reaches the line buffer, `S` does not stop |
| `tests/svg.rs` | loading a real SVG into millimetre polylines |
| `tests/worker.rs` | the worker draws a plan in order, one progress event per op |
| `tests/stop.rs` | stop, pause/resume and panic abort mid-plan |
| `tests/progress.rs` | `progress.json` checkpoints; pause → leave → come back → finish |
| `tests/signals.rs` | a termination signal exits gracefully, pen up, motors released |

Run one suite or one test:

```bash
cargo test --test progress
cargo test resume_from_an_index_sends_only_the_remaining_ops_once
```

## 8. Hardware probes (`examples/`)

Two interactive binaries that talk to a real board on stdout, deliberately kept
out of the TUI. Each stage asks before it moves anything, and both print their
own `--help`.

```bash
cargo run --example spike -- --help
cargo run --example spike -- --list-ports
cargo run --example spike -- --port /dev/ttyACM0 --stage passive,pen
```

`spike` is the protocol spike of `DESIGN.org` §15 — it is what established that
this firmware really is Grbl 1.1h. Stages, in order:
`passive`, `pen`, `home`, `move`, `abs`, `realtime`, `draw` (a 40 mm square, to
check orientation and scale by eye); `--stage` takes any comma-separated subset.
Commands, raw responses and your own observations are appended to
`spike-report.md`; raw wire traffic lands in `spike.log` at TRACE.

```bash
cargo run --example fence -- --help
cargo run --example fence -- --port /dev/ttyACM0 --mm 40 --feed 600
```

`fence` asks the board whether a barrier really waits: it commands a move of
known duration and times `G4 P0.01` against polling `?` until `Idle`. A barrier
that answers in milliseconds when the move takes seconds is not a barrier — this
is how the pen fence came to be `"off"` by default (§2.5). The same binary
carries the print probes that found the clean-feed boundary: `--ladder` (one
stroke at F300..F12000), `--zladder` (six pen-down heights), `--rampladder`
(six lead-in lengths), `--dip` and `--pen` (axis sampling through a real
pen-down / draw / pen-up), and `--zcycle` (hundreds of hatch strokes with a pen
cycle each, rows alternating the Z feed or, with `--stall-ms`, a forced idle:
does the pen axis lose its depth?). Every move is relative and comes straight back, so
nothing depends on having homed.

## 9. Repository layout

```
src/
  main.rs          thin binary; everything else is the `plotly` library crate
  lib.rs           run(): args → logging → SVG → port → handshake → profile → TUI
  cli.rs           clap definition (§13)
  config.rs        config.toml: per-profile overrides
  profiles.rs      machine profiles, $$ merge, firmware write-back (§10)
  geometry.rs      points, polylines, placement, axis transform
  plan/            SVG → polylines → Plan (ops in absolute mm) + time estimate
  plotter/         transport (serial / mock), handshake, driver, worker thread
  job.rs           job directory: plan.jsonl, meta.json, progress.json (§6)
  keys.rs          modal key map → actions (§8)
  app.rs           application state and the event loop
  ui/, tui.rs      ratatui rendering: status / canvas / strokes / log, terminal guard
  logging.rs       file appender + in-TUI log ring + panic hook (§5)
  fonts.rs         built-in single-stroke pen font (§7)
tests/             integration suites, all on the mock
examples/          spike.rs, fence.rs — interactive hardware probes
default.conf       annotated configuration (documentation, not read by plotly)
DESIGN.org         the design document and the work plan (source of truth)
prints/            photographs of test prints, so a claim about paper can be checked
inkscape extensions/   reference material (read-only): the iDraw/AxiDraw Python drivers
```

Sample drawings in the repo root: `paper-sizes.svg` (nested A-series
rectangles, good for checking scale and placement), `hook-test.svg` (four
labelled blocks of strokes that isolate where a hook at a line's end comes
from, §2.5), `example.svg` (6312 shapes — a ~2 h A0 plot, and the drawing every
timing figure in `DESIGN.org` and `default.conf` is measured on).

`inkscape extensions/` is reference material only — the Python drivers the
DrawCore protocol was reverse-engineered from. Nothing in the build touches it.

## 10. Conventions and status

- **Where things stand** is `DESIGN.org` §14: the work plan, step by step, with
  `[X]` against what is done. Phases 0–3 are complete — skeleton and logging,
  port/handshake/manual control, SVG → plan → draw with stop/pause/resume, and
  the on-disk job with crash recovery. Open: the size/preview UI and system
  outline fonts of text mode (steps 4.2, 4.3), and the rest of the UX parity
  list (5.4 — copies/layers, go-to XY, mm/inch, a job queue). Path optimisation
  (5.2) was dropped on purpose: `vpype` does reorder and hatch-fill before the
  SVG gets here. Open hardware questions live in §15.3.
- **Language.** Everything inside the application is English — code, comments,
  UI strings, log lines and commit messages. `DESIGN.org` and `CLAUDE.md` are
  written in Polish, being notes to the author.
- **`inkscape extensions/` is read-only reference.** Nothing in the build reads
  it.

## 11. License

MIT.
