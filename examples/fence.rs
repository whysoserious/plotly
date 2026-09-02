//! Does `G4` really wait? A hardware probe for the pen-fence question.
//!
//! `Driver::set_pen` fences the pen's Z move with `G4` dwells on the theory
//! that Grbl runs a dwell only once the planner buffer has emptied, so the
//! dwell's `ok` means "everything before it is *drawn*". On our machine that
//! fence did not change the hook at the end of a line — only the plot's
//! duration — which is exactly what a `G4` that answers early would look like.
//! DESIGN.org §2.5 / §15.3.
//!
//! This asks the board directly. It commands a move that takes a known number
//! of seconds and then times how long each candidate barrier takes to admit the
//! move is over:
//!   * `G4 P0.01` — the fence we ship;
//!   * polling `?` until the machine reports `Idle` — the fallback.
//!
//! A barrier that answers in milliseconds when the move takes seconds is not a
//! barrier.
//!
//! Every move is relative (`G91`) and comes straight back, so the head ends
//! where it started and nothing depends on having homed.
//!
//! Usage:
//!   cargo run --example fence -- --list-ports
//!   cargo run --example fence -- [--port /dev/ttyACM0] [--mm 40] [--feed 600]

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use plotly::plotter::serial::{find_idraw_ports, SerialTransport, DEFAULT_BAUD};
use plotly::plotter::transport::Transport;

/// Window for the `ok` of a command that only has to be *queued*.
const SHORT_WAIT: Duration = Duration::from_millis(600);
/// Window for a reply that may take as long as the move it waits for.
const LONG_WAIT: Duration = Duration::from_secs(60);
/// How long to listen for the boot banner before sending anything (§15.2).
const BANNER_WAIT: Duration = Duration::from_secs(3);
/// Gap between `?` polls. Short enough to time a barrier, long enough not to
/// flood the board with status requests.
const POLL_GAP: Duration = Duration::from_millis(10);
/// Consecutive `Idle` reports needed to believe the machine is idle. One is not
/// enough: after `ok` the state stays `Idle` for a moment before turning `Run`
/// (§15.1), and a single early poll would call a barrier instant that is not.
const IDLE_STREAK: usize = 3;

/// A barrier that answers in this fraction of the move's own duration was not
/// waiting for the move.
const REAL_BARRIER: f64 = 0.6;

fn main() -> io::Result<()> {
    let opts = match Options::parse(std::env::args().skip(1)) {
        Ok(Some(opts)) => opts,
        Ok(None) => return Ok(()),
        Err(msg) => {
            eprintln!("error: {msg}\n");
            print_usage();
            std::process::exit(2);
        }
    };

    let port = match opts
        .port
        .clone()
        .or_else(|| find_idraw_ports().into_iter().next())
    {
        Some(port) => port,
        None => {
            eprintln!("no iDraw found (VID 1A86, PID 7523/8040). Pass --port <path>.");
            std::process::exit(1);
        }
    };

    println!("Port {port} at {} baud.", opts.baud);
    let mut probe = Probe::open(&port, opts.baud)?;
    probe.settle();

    let travel_secs = f64::from(opts.mm) / (f64::from(opts.feed) / 60.0);
    println!(
        "\nTest move: {} mm at F{} — {travel_secs:.1} s of motion, then straight back.",
        opts.mm, opts.feed
    );
    println!("The pen is not touched; nothing should be drawn. Keep the carriage clear.");
    if !confirm("Run it?") {
        println!("Nothing sent.");
        return Ok(());
    }

    probe.send_ok("G21", SHORT_WAIT);
    probe.send_ok("G90", SHORT_WAIT);

    if opts.pen {
        stage_pen(&mut probe, &opts);
        if confirm("\nDisable the motors now (`$SLP`)?") {
            probe.send_ok("$SLP", SHORT_WAIT);
        }
        return Ok(());
    }

    let g4 = probe.time_barrier(&opts, Barrier::Dwell);
    let poll = probe.time_barrier(&opts, Barrier::Poll);

    println!("\n================ verdict ================");
    report("G4 P0.01", g4, travel_secs);
    report("? until Idle", poll, travel_secs);
    println!("\nThe move itself takes {travel_secs:.2} s.");
    println!(
        "A barrier under {:.2} s did not wait for it.",
        travel_secs * REAL_BARRIER
    );
    match (waited(g4, travel_secs), waited(poll, travel_secs)) {
        (true, _) => println!(
            "\n=> G4 SYNCHRONISES. The fence in Driver::set_pen is real, so the hook\n   \
             at the end of a line has another cause — the pen is still touching the\n   \
             paper when the travel starts, not cornering."
        ),
        (false, true) => println!(
            "\n=> G4 DOES NOT SYNCHRONISE, but polling `?` does. The fence has been\n   \
             decorative: it cost time and stopped nothing. Driver::set_pen should\n   \
             wait on `?` instead."
        ),
        (false, false) => println!(
            "\n=> NEITHER BARRIER WAITS. Something more basic is off — check the\n   \
             raw numbers above before changing the driver."
        ),
    }
    if confirm("\nDisable the motors now (`$SLP`)?") {
        probe.send_ok("$SLP", SHORT_WAIT);
    }
    Ok(())
}

/// Run a pen-down / draw / pen-up exactly as the driver does, sampling `?` all
/// the way through, and report what the axes actually did.
///
/// This is the question the wire log cannot answer. It shows what we send, not
/// whether XY is already moving while Z is still on its way to the paper (which
/// would draw a hook into the start of a stroke) or how long the tip stands
/// still on it (which is how a dot forms). Both are read straight off the
/// `MPos` samples.
fn stage_pen(probe: &mut Probe, opts: &Options) {
    println!("\n---- what the axes actually do ----");
    println!(
        "The pen will be lowered onto whatever is under it and drawn {} mm.",
        opts.mm
    );
    if !confirm("Run it? (put paper under the pen)") {
        return;
    }
    probe.wait_idle(LONG_WAIT);
    let start = match probe.status().as_deref().and_then(mpos) {
        Some(p) => p,
        None => {
            println!("  !! no status report; cannot sample");
            return;
        }
    };
    println!(
        "  starting at X{:.3} Y{:.3} Z{:.3}",
        start[0], start[1], start[2]
    );

    // The driver's own sequence, sent back to back so the planner holds it all
    // and the machine runs it without waiting for us — the same conditions a
    // plot runs under with `pen_fence = "off"`.
    let program = [
        format!("G1 G90 Z{:.3} F{}", opts.pen_up_z, opts.z_feed),
        format!("G1 G91 X-{} F{}", opts.mm, opts.feed),
        format!("G1 G90 Z{:.3} F{}", opts.pen_down_z, opts.z_feed),
        format!("G1 G91 X{} F{}", opts.mm, opts.feed),
        format!("G1 G90 Z{:.3} F{}", opts.pen_up_z, opts.z_feed),
    ];
    let t0 = Instant::now();
    for line in &program {
        probe.send_quiet(line);
    }
    println!(
        "  program queued in {:.0} ms; sampling…",
        t0.elapsed().as_secs_f64() * 1000.0
    );

    let mut samples: Vec<(f64, [f64; 3])> = Vec::new();
    let mut idle_streak = 0;
    while t0.elapsed() < LONG_WAIT {
        if let Some(report) = probe.status() {
            if let Some(p) = mpos(&report) {
                samples.push((t0.elapsed().as_secs_f64() * 1000.0, p));
            }
            if state_of(&report) == "Idle" && samples.len() > 4 {
                idle_streak += 1;
                if idle_streak >= IDLE_STREAK {
                    break;
                }
            } else {
                idle_streak = 0;
            }
        }
    }
    report_pen_samples(&samples, opts);
}

/// Turn the `MPos` samples into the two numbers that matter.
fn report_pen_samples(samples: &[(f64, [f64; 3])], opts: &Options) {
    println!(
        "  {} samples over {:.0} ms",
        samples.len(),
        samples.last().map_or(0.0, |s| s.0)
    );
    if samples.len() < 5 {
        println!("  !! too few samples to read anything into");
        return;
    }
    // The pen is on the paper once Z is within a hair of its down position.
    let down = f64::from(opts.pen_down_z);
    let touching = |p: &[f64; 3]| (p[2] - down).abs() < 0.05;
    let first_touch = samples.iter().find(|(_, p)| touching(p));
    // X moving while the pen is at neither height is the hook; X still while
    // it is at the down height is the dot.
    let mut moved_while_landing = 0.0_f64;
    let mut still_on_paper = 0.0_f64;
    for (a, b) in samples.iter().zip(samples.iter().skip(1)) {
        let dx = (b.1[0] - a.1[0]).abs() + (b.1[1] - a.1[1]).abs();
        let dz = (b.1[2] - a.1[2]).abs();
        if dz > 0.001 && dx > 0.001 {
            moved_while_landing += dx;
        }
        if touching(&b.1) && dx <= 0.001 {
            still_on_paper += b.0 - a.0;
        }
    }
    println!("\n  XY travelled while Z was also moving: {moved_while_landing:.3} mm");
    println!("  time with the tip on the paper and XY still: {still_on_paper:.0} ms");
    if let Some((t, p)) = first_touch {
        println!(
            "  first touch at {t:.0} ms, X{:.3} Y{:.3} Z{:.3}",
            p[0], p[1], p[2]
        );
    }
    println!(
        "\n  => {}",
        if moved_while_landing > 0.05 {
            "the axes OVERLAP: the pen is moving sideways while it changes height"
        } else {
            "the axes do NOT overlap; whatever marks the paper is not this"
        }
    );
}

/// `MPos` from a status report, if it carries one.
fn mpos(report: &str) -> Option<[f64; 3]> {
    let field = report.split('|').find(|f| f.starts_with("MPos:"))?;
    let mut it = field.trim_start_matches("MPos:").split(',');
    let mut next = || it.next()?.trim_end_matches('>').parse::<f64>().ok();
    Some([next()?, next()?, next()?])
}

/// Which barrier is under test.
#[derive(Clone, Copy)]
enum Barrier {
    /// The dwell we ship in `Driver::set_pen`.
    Dwell,
    /// Poll `?` until the machine says `Idle`.
    Poll,
}

/// What one barrier did: how long it held, and the state right after it let go.
#[derive(Clone, Copy)]
struct Timing {
    secs: f64,
    /// Machine state reported the moment the barrier returned.
    state_after: Option<&'static str>,
    /// How much longer `?` polling had to wait afterwards for a real `Idle`.
    extra_secs: f64,
}

struct Probe {
    transport: SerialTransport,
}

impl Probe {
    fn open(path: &str, baud: u32) -> io::Result<Self> {
        let transport = SerialTransport::open(path, baud, Duration::from_millis(50))?;
        Ok(Self { transport })
    }

    /// Listen out the boot banner: opening the port resets the board, and a
    /// command sent into that window is swallowed (§15.2).
    fn settle(&mut self) {
        for line in self.transport.read_lines_for(BANNER_WAIT) {
            println!("  < {line}");
        }
        let _ = self.transport.clear_input();
    }

    /// Send one line and wait for its `ok`, printing whatever comes back.
    fn send_ok(&mut self, line: &str, wait: Duration) -> Duration {
        println!("  > {line}");
        let start = Instant::now();
        if let Err(err) = self.transport.send_line(line) {
            println!("  !! send failed: {err}");
            return start.elapsed();
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.transport.read_line_for(left) {
                Ok(Some(reply)) => {
                    println!("  < {reply}");
                    if reply == "ok" || reply.starts_with("error:") || reply.starts_with("ALARM") {
                        return start.elapsed();
                    }
                }
                Ok(None) => {
                    println!("  !! no ok within {:.1} s", wait.as_secs_f64());
                    return start.elapsed();
                }
                Err(err) => {
                    println!("  !! read failed: {err}");
                    return start.elapsed();
                }
            }
        }
    }

    /// Send a line and wait for its `ok` without narrating it.
    fn send_quiet(&mut self, line: &str) {
        if self.transport.send_line(line).is_err() {
            return;
        }
        let deadline = Instant::now() + SHORT_WAIT;
        while Instant::now() < deadline {
            match self.transport.read_line_for(deadline - Instant::now()) {
                Ok(Some(r)) if r == "ok" || r.starts_with("error:") => return,
                Ok(Some(_)) => {}
                _ => return,
            }
        }
    }

    /// One `?` status report, or `None` if the board said nothing.
    fn status(&mut self) -> Option<String> {
        self.transport.write_realtime(b'?').ok()?;
        self.transport
            .read_lines_for(Duration::from_millis(80))
            .into_iter()
            .find(|l| l.starts_with('<'))
    }

    /// Poll `?` until the machine has reported `Idle` [`IDLE_STREAK`] times
    /// running. Returns how long that took.
    fn wait_idle(&mut self, timeout: Duration) -> f64 {
        let start = Instant::now();
        let mut streak = 0;
        while start.elapsed() < timeout {
            match self.status().as_deref().map(state_of) {
                Some("Idle") => {
                    streak += 1;
                    if streak >= IDLE_STREAK {
                        return start.elapsed().as_secs_f64();
                    }
                }
                _ => streak = 0,
            }
            std::thread::sleep(POLL_GAP);
        }
        start.elapsed().as_secs_f64()
    }

    /// Command the test move, hold at `barrier`, and time the hold.
    fn time_barrier(&mut self, opts: &Options, barrier: Barrier) -> Timing {
        let label = match barrier {
            Barrier::Dwell => "G4 P0.01",
            Barrier::Poll => "? until Idle",
        };
        println!("\n---- barrier under test: {label} ----");
        // Start from a standstill, so the only motion the barrier can be
        // waiting for is the one we are about to command.
        self.wait_idle(LONG_WAIT);
        let _ = self.transport.clear_input();

        self.send_ok(&format!("G1 G91 X{} F{}", opts.mm, opts.feed), SHORT_WAIT);
        let start = Instant::now();
        let (secs, state_after) = match barrier {
            Barrier::Dwell => {
                let held = self.send_ok("G4 P0.01", LONG_WAIT).as_secs_f64();
                // What the machine says the instant the dwell lets go. `Run`
                // here is the whole answer: the barrier returned mid-move.
                let state = self.status();
                if let Some(report) = &state {
                    println!("  ? {report}");
                }
                (held, state.as_deref().map(state_of).map(intern))
            }
            Barrier::Poll => {
                let held = self.wait_idle(LONG_WAIT);
                (held, Some("Idle"))
            }
        };
        // Whatever the barrier claimed, the move is not over until `?` agrees.
        let extra_secs = self.wait_idle(LONG_WAIT);
        println!(
            "  held {secs:.3} s (started {:.3} s ago), then {extra_secs:.3} s more to a real Idle",
            start.elapsed().as_secs_f64()
        );

        println!("  (returning to the start)");
        self.send_ok(&format!("G1 G91 X-{} F{}", opts.mm, opts.feed), SHORT_WAIT);
        self.wait_idle(LONG_WAIT);
        Timing {
            secs,
            state_after,
            extra_secs,
        }
    }
}

/// Did this barrier actually wait for the move?
fn waited(t: Timing, travel_secs: f64) -> bool {
    t.secs >= travel_secs * REAL_BARRIER
}

fn report(label: &str, t: Timing, travel_secs: f64) {
    let verdict = if waited(t, travel_secs) {
        "WAITED for the move"
    } else {
        "returned early — no barrier"
    };
    println!(
        "  {label:<14} held {:>7.3} s  state after: {:<6}  +{:.3} s to Idle   {verdict}",
        t.secs,
        t.state_after.unwrap_or("?"),
        t.extra_secs,
    );
}

/// The state field of a `<Idle|MPos:…>` report, without any sub-state.
fn state_of(report: &str) -> &str {
    let body = report.trim_start_matches('<');
    let field = body.split(['|', '>']).next().unwrap_or("");
    field.split(':').next().unwrap_or("")
}

/// The handful of Grbl states, as `'static` strings for the summary.
fn intern(state: &str) -> &'static str {
    match state {
        "Idle" => "Idle",
        "Run" => "Run",
        "Hold" => "Hold",
        "Jog" => "Jog",
        "Alarm" => "Alarm",
        "Home" => "Home",
        _ => "other",
    }
}

struct Options {
    port: Option<String>,
    baud: u32,
    mm: u32,
    feed: u32,
    /// Run the axis-sampling stage instead of the barrier comparison.
    pen: bool,
    pen_up_z: f32,
    pen_down_z: f32,
    z_feed: u32,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut opts = Self {
            port: None,
            baud: DEFAULT_BAUD,
            mm: 40,
            feed: 600,
            pen: false,
            pen_up_z: 0.5,
            pen_down_z: 5.0,
            z_feed: 5000,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    print_usage();
                    return Ok(None);
                }
                "--list-ports" => {
                    let ports = find_idraw_ports();
                    if ports.is_empty() {
                        println!("no iDraw ports found");
                    }
                    for port in ports {
                        println!("{port}");
                    }
                    return Ok(None);
                }
                "--pen" => opts.pen = true,
                "--pen-up-z" => opts.pen_up_z = number(args.next(), "--pen-up-z")? as f32 / 1000.0,
                "--pen-down-z" => {
                    opts.pen_down_z = number(args.next(), "--pen-down-z")? as f32 / 1000.0
                }
                "--z-feed" => opts.z_feed = number(args.next(), "--z-feed")?,
                "--port" => opts.port = Some(args.next().ok_or("--port needs a path")?),
                "--baud" => opts.baud = number(args.next(), "--baud")?,
                "--mm" => opts.mm = number(args.next(), "--mm")?,
                "--feed" => opts.feed = number(args.next(), "--feed")?,
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        if opts.mm == 0 || opts.feed == 0 {
            return Err("--mm and --feed must be above zero".to_owned());
        }
        Ok(Some(opts))
    }
}

fn number(value: Option<String>, flag: &str) -> Result<u32, String> {
    value
        .ok_or_else(|| format!("{flag} needs a number"))?
        .parse()
        .map_err(|_| format!("{flag} needs a number"))
}

fn print_usage() {
    println!(
        "Does G4 really wait? Times the pen fence against a move of known length.\n\n\
         Usage: cargo run --example fence -- [options]\n\n\
         Options:\n  \
         --port <path>   serial port (default: the first iDraw found)\n  \
         --baud <n>      baud rate (default: {DEFAULT_BAUD})\n  \
         --pen           sample the axes through a real pen-down / draw / pen-up\n  \
         --pen-up-z <n>  pen-up Z in thousandths of a mm (default: 500 = 0.5)\n  \
         --pen-down-z <n>  pen-down Z, thousandths of a mm (default: 5000 = 5.0)\n  \
         --z-feed <n>    feed for the pen's Z move (default: 5000)\n  \
         --mm <n>        length of the test move, mm (default: 40)\n  \
         --feed <n>      feed for the test move, mm/min (default: 600)\n  \
         --list-ports    list detected iDraw ports and exit\n"
    );
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    if io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes")
}
