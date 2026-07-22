//! Hardware spike for a real iDraw 2.0 / DrawCore board (plan step 0.7).
//!
//! Answers the open questions of DESIGN.org §15: does the firmware behave like
//! "plain Grbl" (realtime commands, `G21`/`G90` absolute mm, planner on the
//! board) or must we follow the reference driver (`G91` relative, step units,
//! axis transform and planning on the host — §2.3)?
//!
//! It is deliberately a separate binary, not part of the TUI: it is interactive,
//! runs on stdout, and only moves the machine after you confirm each stage.
//!
//! Usage:
//!   cargo run --example spike -- --list-ports
//!   cargo run --example spike -- [--port /dev/ttyACM0] [--stage passive,pen,...]
//!
//! Everything (commands, raw responses, your observations) is appended to
//! `./spike-report.md`; raw wire traffic also lands in `./spike.log` at TRACE.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use plotly::plotter::serial::{find_idraw_ports, SerialTransport, DEFAULT_BAUD};
use plotly::plotter::transport::Transport;

const REPORT_PATH: &str = "spike-report.md";
const LOG_PATH: &str = "spike.log";

/// Window we wait for the answer to a plain query.
const SHORT_WAIT: Duration = Duration::from_millis(600);
/// Window we wait after a realtime byte (which may answer with nothing).
const REALTIME_WAIT: Duration = Duration::from_millis(400);
/// Window for a reply that may follow a firmware restart (banner is slow).
const BANNER_WAIT: Duration = Duration::from_secs(3);
/// Upper bound for a homing cycle.
const HOMING_TIMEOUT: Duration = Duration::from_secs(90);
/// Upper bound for a short commanded move.
const MOVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Stages, in the order they must run.
const STAGES: [&str; 7] = ["passive", "pen", "home", "move", "abs", "realtime", "draw"];

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

    init_tracing();

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

    let mut spike = Spike::open(&port, opts.baud)?;
    spike.rec.header(&port, opts.baud);
    spike.settle();

    for stage in STAGES
        .iter()
        .filter(|s| opts.stages.contains(&s.to_string()))
    {
        match *stage {
            "passive" => stage_passive(&mut spike),
            "pen" => stage_pen(&mut spike),
            "home" => stage_home(&mut spike),
            "move" => stage_move(&mut spike),
            "abs" => stage_abs(&mut spike),
            "realtime" => stage_realtime(&mut spike),
            "draw" => stage_draw(&mut spike),
            _ => unreachable!(),
        }
    }

    if confirm("Disable the motors now (`$SLP`)?") {
        spike.probe("$SLP", SHORT_WAIT);
    }
    spike.rec.section("Done");
    println!("\nReport written to {REPORT_PATH}, raw wire trace in {LOG_PATH}.");
    println!("Paste {REPORT_PATH} back into the chat so we can turn it into DESIGN.org §15 facts.");
    Ok(())
}

// ---------------------------------------------------------------- stages ---

/// Queries only: nothing here may move the machine.
fn stage_passive(s: &mut Spike) {
    s.rec.section("Stage 1 — passive probes (no motion)");

    s.rec
        .note("Handshake as the reference driver does it: `$B`, drain, `v`.");
    s.probe("$B", BANNER_WAIT);
    let _ = s.transport.clear_input();
    let version = s.probe("v", SHORT_WAIT);
    s.rec
        .note(&format!("version reply: {}", join(&version, "(none)")));

    s.rec.note("Identity / state queries.");
    for cmd in ["V", "$QP", "$QT", "$I", "$G", "$#", "$N"] {
        s.probe(cmd, SHORT_WAIT);
    }

    s.rec
        .note("Firmware settings — steps/mm, feed and accel limits (§10, §15).");
    s.probe("$$", Duration::from_secs(2));

    s.rec
        .note("Error format: an unknown `$` word and a bogus G-code word.");
    s.probe("$NOPE", SHORT_WAIT);
    s.probe("G999", SHORT_WAIT);

    s.rec
        .note("Modal words we would like to rely on. These must not move anything.");
    for cmd in ["G21", "G90", "G91", "G90.1", "G54", "M5"] {
        s.probe(cmd, SHORT_WAIT);
    }

    s.rec
        .note("Realtime bytes while idle — the biggest ❓ of the project (§2.2).");
    for _ in 0..3 {
        s.realtime(b'?', "? (status)", REALTIME_WAIT);
    }
    s.realtime(b'!', "! (feed hold)", REALTIME_WAIT);
    s.realtime(b'?', "? (status after feed hold)", REALTIME_WAIT);
    s.realtime(b'~', "~ (cycle start)", REALTIME_WAIT);
    s.realtime(b'?', "? (status after cycle start)", REALTIME_WAIT);
    s.realtime(0x85, "0x85 (jog cancel)", REALTIME_WAIT);

    s.rec
        .ask("Did anything move, click or beep during this stage? (expected: no)");
}

/// Pen (Z axis) only — the carriage must not travel in XY.
fn stage_pen(s: &mut Spike) {
    s.rec.section("Stage 2 — pen up / down (Z axis)");
    if !s.stage_gate("The pen holder will move up and down. Nothing should travel in XY.") {
        return;
    }

    s.probe("$QP", SHORT_WAIT);
    s.rec
        .note("Pen up: `G1 G90 Z0.5 F5000` then the XY feed line (§2.2).");
    s.move_and_settle("G1 G90 Z0.5 F5000");
    s.probe("G1 F2000", SHORT_WAIT);
    s.probe("$QP", SHORT_WAIT);
    s.rec.ask("Is the pen UP now? What does $QP report?");

    s.rec.note("Pen down: `G1 G90 Z5 F5000`.");
    s.move_and_settle("G1 G90 Z5 F5000");
    s.probe("G1 F2000", SHORT_WAIT);
    s.probe("$QP", SHORT_WAIT);
    s.rec
        .ask("Is the pen DOWN now (larger Z = lower)? What does $QP report?");

    s.rec
        .note("Toggle via `$TP<up>,<down>` (reference sets the feed first).");
    s.probe("G90 G1 F5000", SHORT_WAIT);
    s.move_and_settle("$TP0.5,5.0");
    s.rec.ask("Did $TP toggle the pen? In which direction?");
    s.move_and_settle("$TP0.5,5.0");
    s.move_and_settle("G1 G90 Z0.5 F5000");
    s.rec
        .ask("Anything unexpected (Z travel too small/large, buzzing)?");
}

/// Homing — establishes the only absolute reference the machine has.
fn stage_home(s: &mut Spike) {
    s.rec.section("Stage 3 — homing (`$H`)");
    if !s.stage_gate("The carriage will run into the endstops. Keep the bed clear and your hand on the power switch.")
    {
        return;
    }

    let started = Instant::now();
    s.homing(HOMING_TIMEOUT);
    s.rec.note(&format!(
        "homing took {:.1} s",
        started.elapsed().as_secs_f32()
    ));

    s.realtime(b'?', "? (status after homing)", REALTIME_WAIT);
    s.probe("$G", SHORT_WAIT);
    s.rec
        .ask("Which physical corner did it home to (front/back, left/right, seen from the front)?");
    s.rec
        .ask("Did it home both axes, and in which order? Any ALARM afterwards?");
}

/// Relative moves — measures the logical↔wire transform and the unit scale.
fn stage_move(s: &mut Spike) {
    s.rec
        .section("Stage 4 — relative moves, axis transform and scale");
    s.rec.note(
        "Goal: what does one unit on the wire mean, and which physical direction \
         does wire X / wire Y move (§2.3: the reference swaps and mirrors both).",
    );
    if !s.stage_gate(
        "Motors will be released so you can push the carriage to the MIDDLE of the bed, \
         then it makes small moves (1 mm, then 10 mm).",
    ) {
        return;
    }

    s.probe("$SLP", SHORT_WAIT);
    if !confirm("Carriage pushed to the middle of the bed, pen up, bed clear — continue?") {
        s.rec.note("stage aborted by operator");
        return;
    }

    s.rec
        .note("The pen stays UP for the whole stage — this measures directions, it does not draw.");

    s.rec.note("Small probe first: one unit on wire X.");
    s.move_and_settle("G91 G1 X1 F1000");
    s.rec
        .ask("How far did it move for `X1` (mm, by ruler) and in which physical direction?");

    s.rec.note("Now ten units, then back.");
    s.move_and_settle("G91 G1 X10 F1000");
    s.rec
        .ask("How far for `X10` (mm)? Same direction as before?");
    s.move_and_settle("G91 G1 X-11 F1000");
    s.rec.ask("Did `X-11` return it to the starting point?");

    s.rec.note("Same for wire Y.");
    s.move_and_settle("G91 G1 Y10 F1000");
    s.rec
        .ask("How far for `Y10` (mm) and in which physical direction?");
    s.move_and_settle("G91 G1 Y-10 F1000");
    s.rec.ask("Did `Y-10` return it to the starting point?");

    s.rec
        .note("Both axes at once — tells CoreXY apart from a plain cartesian mapping.");
    s.move_and_settle("G91 G1 X10 Y10 F1000");
    s.rec
        .ask("Did `X10 Y10` move diagonally, or straight along one physical axis?");
    s.move_and_settle("G91 G1 X-10 Y-10 F1000");
}

/// The same question as `move`, but answered by a drawing instead of a ruler.
fn stage_draw(s: &mut Spike) {
    s.rec
        .section("Stage 7 — draw a 40 mm square (orientation and scale by eye)");
    s.rec.note(
        "Relative moves, so it draws wherever the carriage is parked; after `$SLP` and a \
         push by hand `MPos` is stale, which does not matter for `G91`.",
    );
    if !s.stage_gate("The pen goes DOWN and draws a 40 mm square with a diagonal.") {
        return;
    }

    let settings = s.probe("$$", Duration::from_secs(2));
    let dirs = interior_dirs(setting(&settings, "$23").unwrap_or(0.0) as u8);

    s.probe("$SLP", SHORT_WAIT);
    if !confirm("Pen mounted, paper under the carriage, carriage pushed onto it — continue?") {
        s.rec.note("stage aborted by operator");
        return;
    }
    let z_down = ask("Pen-down Z value [5]:").parse::<f32>().unwrap_or(5.0);
    s.rec.note(&format!("drawing with pen-down Z{z_down}"));

    let side = 40.0;
    let (sx, sy) = (dirs[0] * side, dirs[1] * side);

    s.move_and_settle("G1 G90 Z0.5 F5000");
    s.move_and_settle(&format!("G1 G90 Z{z_down:.3} F5000"));
    for step in [
        format!("G91 G1 X{sx:.3} F2000"),
        format!("G91 G1 Y{sy:.3} F2000"),
        format!("G91 G1 X{:.3} F2000", -sx),
        format!("G91 G1 Y{:.3} F2000", -sy),
        format!("G91 G1 X{sx:.3} Y{sy:.3} F2000"),
    ] {
        s.move_and_settle(&step);
    }
    s.move_and_settle("G1 G90 Z0.5 F5000");
    s.move_and_settle(&format!("G91 G1 X{:.3} Y{:.3} F2000", -sx, -sy));

    s.rec
        .ask("Which way did the FIRST side go, seen from the front (right/left/away/towards you)?");
    s.rec
        .ask("Measure the square with a ruler: how many mm per side?");
    s.rec.ask(
        "Is it a closed square, and where does the diagonal end relative to the start corner?",
    );
}

/// Absolute millimetres — the "plain Grbl" bet of §2.2.
fn stage_abs(s: &mut Spike) {
    s.rec
        .section("Stage 5 — `G21` + `G90` absolute millimetres");
    s.rec.note(
        "Discriminator: the same absolute target sent twice. If the firmware is truly \
         absolute, the second command must NOT move the head.",
    );
    s.rec.note(
        "Targets are derived from `?` and the homing direction mask `$23`, so every \
         move steps INTO the work area and never back into a limit switch.",
    );
    if !s.stage_gate("Two 2 mm absolute moves away from the current corner, then back.") {
        return;
    }

    let settings = s.probe("$$", Duration::from_secs(2));
    let mask = setting(&settings, "$23").unwrap_or(0.0) as u8;
    let dirs = interior_dirs(mask);
    s.rec.note(&format!(
        "homing direction mask $23={mask} => work area lies at X{:+}, Y{:+} from home",
        dirs[0], dirs[1]
    ));

    s.probe("G21", SHORT_WAIT);
    s.probe("G90", SHORT_WAIT);
    s.probe("$G", SHORT_WAIT);
    let Some(start) = s.position("before the absolute moves") else {
        s.rec
            .note("=> no MPos in the status report; stage cannot run safely");
        return;
    };

    let target = [start[0] + 2.0 * dirs[0], start[1] + 2.0 * dirs[1]];
    let cmd = format!("G1 X{:.3} Y{:.3} F500", target[0], target[1]);

    s.probe(&cmd, SHORT_WAIT);
    let first = s.position("after the first move");

    s.rec
        .note("Exactly the same line again — an absolute firmware must not move now.");
    s.probe(&cmd, SHORT_WAIT);
    let second = s.position("after the repeat");

    s.rec.note(&verdict(start, target, first, second));

    s.rec.note("Back to where the stage started.");
    s.probe(
        &format!("G1 X{:.3} Y{:.3} F500", start[0], start[1]),
        MOVE_TIMEOUT,
    );
    s.position("after returning");
}

/// Read the absolute-vs-relative answer out of the three measured positions.
fn verdict(
    start: [f32; 3],
    target: [f32; 2],
    first: Option<[f32; 3]>,
    second: Option<[f32; 3]>,
) -> String {
    let (Some(first), Some(second)) = (first, second) else {
        return "=> inconclusive: no position report".to_owned();
    };
    let hit = |p: [f32; 3]| (p[0] - target[0]).abs() < 0.05 && (p[1] - target[1]).abs() < 0.05;
    let moved = (second[0] - first[0]).abs() > 0.05 || (second[1] - first[1]).abs() > 0.05;

    if hit(first) && !moved {
        format!(
            "=> ABSOLUTE mm CONFIRMED: {:?} -> {:?}, the repeat stayed put",
            [start[0], start[1]],
            [first[0], first[1]]
        )
    } else if moved {
        format!(
            "=> NOT absolute: the repeat moved again ({:?} -> {:?}) — relative semantics",
            [first[0], first[1]],
            [second[0], second[1]]
        )
    } else {
        format!(
            "=> unexpected: target {target:?}, reached {:?} and stayed there",
            [first[0], first[1]]
        )
    }
}

/// Value of a `$<n>=<value>` line in a `$$` dump.
fn setting(lines: &[String], key: &str) -> Option<f32> {
    let line = lines.iter().find(|l| l.starts_with(&format!("{key}=")))?;
    line.split_once('=')?.1.trim().parse().ok()
}

/// Direction leading away from the home switch, per axis, from the `$23` mask.
///
/// A set bit means that axis homes toward negative, so its work area is on the
/// positive side, and vice versa.
fn interior_dirs(mask: u8) -> [f32; 2] {
    [
        if mask & 1 != 0 { 1.0 } else { -1.0 },
        if mask & 2 != 0 { 1.0 } else { -1.0 },
    ]
}

/// Machine position from a `<…|MPos:x,y,z|…>` status report.
fn parse_mpos(report: &str) -> Option<[f32; 3]> {
    let field = report.split('|').find(|f| f.starts_with("MPos:"))?;
    let mut axes = field.trim_start_matches("MPos:").split(',');
    let mut next = || axes.next()?.trim_end_matches('>').parse::<f32>().ok();
    Some([next()?, next()?, next()?])
}

/// Realtime control while the machine is actually moving.
fn stage_realtime(s: &mut Spike) {
    s.rec.section("Stage 6 — realtime control during a move");
    s.rec
        .note("Everything here is ❓: `?`, `!`, `~`, `0x85`, `$J=`, `0x18` (§2.2).");
    if !s.stage_gate(
        "A long, slow move is started and then interrupted. Have the power switch within reach.",
    ) {
        return;
    }

    let dir = ask("Which sign on wire X has at least 40 mm of room — type + or -:");
    let sign = if dir.trim().starts_with('-') { "-" } else { "" };

    s.rec
        .note("Long slow move; status is polled while it runs.");
    if let Err(err) = s.transport.send_line(&format!("G91 G1 X{sign}40 F300")) {
        s.rec.line(&format!("  !! send failed: {err}"));
        return;
    }
    s.rec.line(&format!("\n$ G91 G1 X{sign}40 F300"));
    for _ in 0..5 {
        s.realtime(b'?', "? (status while moving)", Duration::from_millis(300));
    }

    s.realtime(b'!', "! (feed hold while moving)", REALTIME_WAIT);
    s.rec.ask("Did `!` stop the head mid-move?");
    s.realtime(b'?', "? (status while held)", REALTIME_WAIT);
    s.realtime(b'~', "~ (cycle start)", REALTIME_WAIT);
    s.rec.ask("Did `~` resume the move?");
    s.drain("remainder of the move", Duration::from_secs(10));

    s.rec.note("Jog command and jog cancel.");
    s.probe(&format!("$J=G91 X{sign}10 F1000"), SHORT_WAIT);
    s.rec
        .ask("Did `$J=` jog the head, or answer with an error?");
    s.realtime(0x85, "0x85 (jog cancel)", REALTIME_WAIT);

    s.rec
        .note("Soft reset — the abort path behind panic STOP (§9).");
    s.realtime(0x18, "0x18 (soft reset)", Duration::from_secs(3));
    s.rec
        .ask("What did `0x18` do (stop, banner, motors released, ALARM)?");
    s.realtime(b'?', "? (status after soft reset)", REALTIME_WAIT);
    s.probe("$X", SHORT_WAIT);
    s.probe("v", SHORT_WAIT);
    s.rec
        .ask("Does the board still take commands after the soft reset, or does it need `$H`?");
}

// ----------------------------------------------------------------- spike ---

/// Serial link plus the report it writes.
struct Spike {
    transport: SerialTransport,
    rec: Recorder,
}

impl Spike {
    fn open(port: &str, baud: u32) -> io::Result<Self> {
        let transport = SerialTransport::open(port, baud, Duration::from_millis(50))?;
        Ok(Self {
            transport,
            rec: Recorder::open()?,
        })
    }

    /// Send one line and record what comes back, up to `wait`.
    ///
    /// Returns as soon as the board answers `ok`/`error:`/`ALARM`, so probing
    /// never sits out the whole window waiting for a reply that already came.
    fn probe(&mut self, cmd: &str, wait: Duration) -> Vec<String> {
        self.rec.line(&format!("\n$ {cmd}"));
        if let Err(err) = self.transport.send_line(cmd) {
            self.rec.line(&format!("  !! send failed: {err}"));
            return Vec::new();
        }
        let deadline = Instant::now() + wait;
        let mut lines = Vec::new();
        loop {
            let chunk = self.transport.read_lines_for(Duration::from_millis(100));
            let done = chunk.iter().any(|l| is_terminal(l));
            lines.extend(chunk);
            if done || Instant::now() >= deadline {
                break;
            }
        }
        self.record_lines(&lines);
        lines
    }

    /// Send a motion command and wait for the machine to actually stop.
    ///
    /// `ok` only means "queued" (§15.1), so the position after it is not the
    /// end of the move — the operator must not be asked what happened until
    /// the status report says `Idle`.
    fn move_and_settle(&mut self, cmd: &str) -> Option<[f32; 3]> {
        self.probe(cmd, SHORT_WAIT);
        let report = self.wait_idle(MOVE_TIMEOUT)?;
        self.rec.line(&format!("  < {report}"));
        parse_mpos(&report)
    }

    /// Wait out a possible reset before the first command is sent.
    ///
    /// Opening the port can reboot the board (the kernel raises DTR before we
    /// can drop it), and the boot banner takes ~700 ms — a command sent into
    /// that window is swallowed. Anything the TUI does must wait it out too.
    fn settle(&mut self) {
        self.rec
            .note("Listening before sending anything: a `Grbl …` banner here means opening the port rebooted the board.");
        let lines = self.transport.read_lines_for(BANNER_WAIT);
        self.record_lines(&lines);
        if lines.iter().any(|l| l.starts_with("Grbl")) {
            self.rec.note(
                "=> the port open DID reset the board; the first command must wait for the banner.",
            );
        }
    }

    /// Send `$H` and follow the cycle with `?` polls until it ends.
    ///
    /// A homing cycle answers `ok` only when it finishes, so the state has to
    /// come from status reports; a banner means the board rebooted instead.
    fn homing(&mut self, timeout: Duration) {
        self.rec.line("\n$ $H");
        if let Err(err) = self.transport.send_line("$H") {
            self.rec.line(&format!("  !! send failed: {err}"));
            return;
        }
        let deadline = Instant::now() + timeout;
        let mut last_state = String::new();
        while Instant::now() < deadline {
            let lines = self.transport.read_lines_for(Duration::from_millis(600));
            let done = lines.iter().any(|l| is_terminal(l));
            let rebooted = lines.iter().any(|l| l.starts_with("Grbl"));
            for line in &lines {
                self.rec.line(&format!("  < {line}"));
            }
            if rebooted {
                self.rec
                    .note("=> the board REBOOTED instead of homing (no `ok`, no ALARM).");
                return;
            }
            if done {
                return;
            }
            // Follow the cycle: report only when the machine state changes.
            if self.transport.write_realtime(b'?').is_ok() {
                for line in self.transport.read_lines_for(Duration::from_millis(200)) {
                    let state = report_state(&line);
                    if state != last_state {
                        self.rec.line(&format!("  < {line}"));
                        last_state = state;
                    }
                }
            }
        }
        self.rec.note("=> homing timed out");
    }

    /// Wait for the machine to go `Idle`, then report where it ended up.
    fn position(&mut self, label: &str) -> Option<[f32; 3]> {
        self.rec.line(&format!("\n$ [realtime 0x3f] ? ({label})"));
        let report = self.wait_idle(MOVE_TIMEOUT)?;
        self.rec.line(&format!("  < {report}"));
        parse_mpos(&report)
    }

    /// Poll `?` until the machine reports `Idle`; yields the last report seen.
    fn wait_idle(&mut self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        let mut last: Option<String> = None;
        while Instant::now() < deadline {
            if self.transport.write_realtime(b'?').is_err() {
                break;
            }
            for line in self.transport.read_lines_for(Duration::from_millis(200)) {
                if line.starts_with('<') {
                    last = Some(line);
                }
            }
            if last.as_deref().map(report_state).as_deref() == Some("Idle") {
                return last;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if last.is_none() {
            self.rec.line("  (no status report)");
        }
        last
    }

    /// Write one realtime byte (no terminator) and record the reply.
    fn realtime(&mut self, byte: u8, label: &str, wait: Duration) -> Vec<String> {
        self.rec
            .line(&format!("\n$ [realtime {byte:#04x}] {label}"));
        if let Err(err) = self.transport.write_realtime(byte) {
            self.rec.line(&format!("  !! send failed: {err}"));
            return Vec::new();
        }
        let lines = self.transport.read_lines_for(wait);
        self.record_lines(&lines);
        lines
    }

    /// Record whatever the board says on its own within `wait`.
    fn drain(&mut self, label: &str, wait: Duration) -> Vec<String> {
        self.rec.line(&format!("\n$ [waiting: {label}]"));
        let lines = self.transport.read_lines_for(wait);
        self.record_lines(&lines);
        lines
    }

    fn record_lines(&mut self, lines: &[String]) {
        if lines.is_empty() {
            self.rec.line("  (no response)");
        }
        for line in lines {
            self.rec.line(&format!("  < {line}"));
        }
    }

    /// Ask before a stage that moves the machine; records the decision.
    fn stage_gate(&mut self, warning: &str) -> bool {
        println!("\n!! {warning}");
        let go = confirm("Run this stage?");
        if !go {
            self.rec.note("stage skipped by operator");
        }
        go
    }
}

/// The machine state of a `<Idle|MPos:…>` report, or `""` for anything else.
fn report_state(line: &str) -> String {
    line.trim_start_matches('<')
        .split(['|', '>'])
        .next()
        .filter(|_| line.starts_with('<'))
        .unwrap_or_default()
        .to_owned()
}

/// `true` for a reply that ends a command exchange.
fn is_terminal(line: &str) -> bool {
    let l = line.trim();
    l == "ok" || l.starts_with("error:") || l.starts_with("ALARM")
}

fn join(lines: &[String], empty: &str) -> String {
    if lines.is_empty() {
        empty.to_owned()
    } else {
        lines.join(" | ")
    }
}

// -------------------------------------------------------------- recorder ---

/// Appends the whole session to `spike-report.md` and echoes it to stdout.
struct Recorder {
    file: File,
}

impl Recorder {
    fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(REPORT_PATH)?;
        Ok(Self { file })
    }

    fn header(&mut self, port: &str, baud: u32) {
        self.line(&format!(
            "\n# iDraw hardware spike (step 0.7) — {port} @ {baud}"
        ));
        self.line("Answers go into DESIGN.org §15. `$` = sent, `<` = received, `>` = operator.");
    }

    fn section(&mut self, title: &str) {
        self.line(&format!("\n## {title}"));
    }

    /// A short explanation of what the next probes are for.
    fn note(&mut self, text: &str) {
        self.line(&format!("\n-- {text}"));
    }

    /// Ask the operator what physically happened and record the answer.
    fn ask(&mut self, question: &str) {
        let answer = ask(question);
        self.line(&format!("? {question}"));
        self.line(&format!("> {answer}"));
    }

    fn line(&mut self, text: &str) {
        println!("{text}");
        let _ = writeln!(self.file, "{text}");
        let _ = self.file.flush();
    }
}

// ----------------------------------------------------------------- input ---

/// Read one line from the operator, showing `question` first.
fn ask(question: &str) -> String {
    print!("{question} ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    match io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => "(no answer)".to_owned(),
        Ok(_) => answer.trim().to_owned(),
    }
}

/// Yes/no prompt; anything but `y`/`yes` means no.
fn confirm(question: &str) -> bool {
    let answer = ask(&format!("{question} [y/N]"));
    matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes")
}

// ------------------------------------------------------------------- CLI ---

struct Options {
    port: Option<String>,
    baud: u32,
    stages: Vec<String>,
}

impl Options {
    /// Hand-rolled parsing: the spike is a throwaway tool, not the TUI's CLI.
    fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut opts = Options {
            port: None,
            baud: DEFAULT_BAUD,
            stages: STAGES.iter().map(|s| s.to_string()).collect(),
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    return Ok(None);
                }
                "--list-ports" => {
                    let ports = find_idraw_ports();
                    if ports.is_empty() {
                        println!("no iDraw ports found (VID 1A86, PID 7523/8040)");
                    }
                    for port in ports {
                        println!("{port}");
                    }
                    return Ok(None);
                }
                "--port" => opts.port = Some(args.next().ok_or("--port needs a value")?),
                "--baud" => {
                    let value = args.next().ok_or("--baud needs a value")?;
                    opts.baud = value.parse().map_err(|_| format!("bad baud: {value}"))?;
                }
                "--stage" => {
                    let value = args.next().ok_or("--stage needs a value")?;
                    let stages: Vec<String> =
                        value.split(',').map(|s| s.trim().to_owned()).collect();
                    if let Some(bad) = stages.iter().find(|s| !STAGES.contains(&s.as_str())) {
                        return Err(format!("unknown stage: {bad}"));
                    }
                    opts.stages = stages;
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Some(opts))
    }
}

fn print_usage() {
    println!(
        "iDraw hardware spike (step 0.7)\n\n\
         Usage: cargo run --example spike -- [options]\n\n\
         Options:\n  \
         --port <path>     serial port (default: first iDraw found)\n  \
         --baud <n>        line speed (default: {DEFAULT_BAUD})\n  \
         --stage <list>    comma-separated subset of: {}\n  \
         --list-ports      list detected iDraw ports and exit\n  \
         -h, --help        this text\n",
        STAGES.join(", ")
    );
}

/// TRACE-level wire log to `spike.log`, mirroring what the TUI logs (§5).
fn init_tracing() {
    let Ok(file) = File::create(LOG_PATH) else {
        eprintln!("warning: cannot write {LOG_PATH}; continuing without a wire log");
        return;
    };
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
}
