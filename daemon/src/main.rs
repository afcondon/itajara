//! itajara — the looper engine for producing-with-your-feet.
//!
//! See `docs/DESIGN-LOOPER.md` for what this is going to be. Today it is the
//! metrology: §10 of that document lists three latencies that have to be
//! measured rather than guessed, and §15 says to measure the audio round trip
//! before writing any overdub code. This is that.

mod align;
mod aggregate;
mod capture;
mod devices;
mod engine;
mod ws;
mod levels;
mod link;
mod measure;
mod wav;

use std::process::ExitCode;

const USAGE: &str = "\
itajara — looper engine for producing-with-your-feet

USAGE
  itajara devices
      List the audio devices CoreAudio can see, with channel counts and the
      sample rates each will accept. For an aggregate it also says what it is
      made of and where each member's channels landed — which is a fact about
      today, not about the aggregate's name.

  itajara sources --device <name> [--source ...]
      Where every source lands, resolved against the device's real layout.
      Opens nothing, starts no stream, changes no sample rate. Run it before
      a session on a rig with an aggregate: the failure that costs a whole
      session is the silent one, where an interface came back in a different
      order, the absolute channel numbers still parse, the meters still move,
      and every take is off the wrong input.

  itajara levels --device <name> [--seconds <n>]
      Live peak meter on every input channel, with a peak hold. Play into
      one jack at a time to find out which host channel it arrives on —
      an interface with more USB channels than physical jacks does not
      tell you this, and guessing wrong records silence.

  itajara loop --device <name> [options]
      The looper. Records, overdubs as layers, and undoes them, on the
      alignment `align` verifies. Commands on stdin:

        r  record / overdub toggle     x  multiply
        t [secs]  take from the past
        u  undo last layer             c  clear everything
        k  click on/off                p  status        q  quit
        one  one pass per trigger      f  fire it
        lev  wait for a sound          arm [db]  the level it waits for
        ch [p]  how often a pass sounds, 0 to 1
        xf [ms]  crossfade the loop wrap with what followed it
        dec [db]  how much a pass costs what is already there
        len [n]  how many bars this loop is
        s [n]  how often the newest layer sounds, in cycles
        ph [n]  which slot of those it lands on
        lq [n]  what a launch waits for, in beats (-1 a bar, 0 none)

      `len` does one of three things and says which. On an empty loop it
      sets the length, and the next recording closes itself there rather
      than waiting for a second `r`. On the first loop with no clock it
      *declares* what you played — `len4` on a four-bar phrase makes the
      bar a quarter of it, touching no audio, which is how a clockless
      session gets a loop shorter than its first take. On anything else
      it resizes, and the layers keep their own lengths inside the new one.

      `s` and `ph` are a pair: `s4` makes the newest layer sound once
      every four cycles and `ph3` puts it on the third of them, so a bar
      recorded once can be placed anywhere in a longer loop. Both are
      absolute — `s` used to multiply what was already there, which is
      the right shape for a footswitch and the wrong one for a knob.

      `one` and `lev` are per-loop modes and take `0`/`1` to set rather than
      flip: `2one1` makes loop 2 a one-shot, silent until `2f` fires it from
      the top. `2lev1` makes `2r` arm rather than record — the loop starts
      when something crosses the level, and reaches back past the crossing
      so the attack that crossed it is in the take.

      `t` is the one a pedal cannot do: you played something good and did
      not hit record, so hit it afterwards. With no loop yet it takes the
      last [secs] as the loop; with one running it claims the last complete
      cycle as a new layer.

      The first recording defines the cycle; every later one is an overdub
      of exactly that length, summed into its own layer.

      `x` multiplies: keep playing across as many cycles as you like, press
      it again, and the loop becomes that many cycles long with everything
      already there repeating underneath. Two bars into eight, in two taps.
      It starts at the beginning of the cycle you are in, not when you
      pressed, so pressing late costs nothing.

      --residual <n>    from `sweep`, for this configuration. Without it the
                        engine reads ~/.itajara/calibration.conf, which
                        `deepstar latency --write` curates from Amphora; with
                        neither it falls back to 252 and says so, because
                        the default is an assertion and not an abstention
      --max-secs <s>    longest loop, and so the arena size   (default 300)
      --loops <n>       how many loops                            (default 8)
      --layers <n>      layers per loop, which is the undo stack  (default 4)
                        Neither is capped: the arena is loops x layers x
                        --max-secs, committed only as loops fill, and the
                        daemon says what that comes to at startup, asks on a
                        terminal past a quarter of memory, and refuses past
                        all of it.
      --yes             take the footprint as read; do not ask
      --fixed-secs <s>  every loop starts, and after `c` returns, as an empty
                        tape this long: record, and it closes itself there.
                        Stands in for --max-secs unless that is given too.
      --click           metronome at loop position zero
      --monitor         pass live input to the output. Off by default: the
                        interface's own direct monitoring costs no latency
                        where this costs the round trip plus a buffer
      --mono-out        send the mix to one channel instead of a pair
      --ws              serve the app on ws://127.0.0.1:3028
      --ws-port <n>     ...on a different port
      --ring-secs <s>   how much of the past stays claimable      (default 60)
      --capture-secs <s> longest one capture may run              (default 900)
                        Costs nothing until something captures: the buffer is
                        allocated on the first one, never on startup.
      --takes-dir <p>   where `w` saves takes         (default ~/.itajara/takes)
      --link            take the bar from link-spike's /link/anchor, on 57125
      --link-port <n>   ...on a different port
      --source <n=c[,c]> a named input, repeatable: `--source board=1,2`
                        `--source di=3`. Channels count from 1. One channel is
                        a mono jack. Without any, `--in-ch` becomes one source
                        called `in`. A loop chooses with `<n>src<i>`.
                        On an aggregate prefer `--source board=AUDIO4c:1,2` —
                        which interface, and which of ITS jacks, resolved at
                        start against what CoreAudio reports. An aggregate's
                        channel order is not stable across a power cycle, and
                        an absolute number that has moved records the wrong
                        input in silence. Named, a missing interface is a
                        refusal to start instead.
      --preroll-ms <n>  how far before the tap the first loop actually
                        starts, pulled from the pre-roll           (default 0)
      --arm-db <n>      dBFS a sound must reach to start a level-armed
                        loop; `arm<n>` changes it live         (default -36)
      --selftest <s>    record one cycle of the engine's own click through a
                        loopback cable and check where it landed

  itajara align --device <name> [options]
      The self-test. Plays a click at loop position zero, records it back
      through a patch cable, and reports which position it landed at. Zero
      means the arithmetic that places recorded audio in the loop is right,
      and overdubs will stack without accumulating drift.

      This is the only part of a looper that can be verified rather than
      judged by ear. Run it whenever the audio configuration changes.

      --residual <n>    the interface's transit, from `sweep`. Resolved the
                        same three ways `loop` resolves it, so with no flag
                        this verifies the number the engine will use
      --loop-secs <s>   loop length to test against            (default 2.0)
      --cycles <n>      how many times round                   (default 4)
      --out-ch / --in-ch / --amp / --buffer / --rate  as elsewhere

  itajara map --device <name> [options]
      Click every output in turn, listening on every input. With one cable
      patched from an output jack to an input jack, exactly one pair should
      answer — which names the host channel behind BOTH jacks in one run.

      Move the cable to the next pair of jacks and run it again. Four runs
      map a four-in/four-out interface completely.

      More than one pair answering means internal routing inside the
      interface, where a click crosses no converter.

  itajara sweep --device <name> [options]
      The calibration. Measures at several buffer sizes and separates the
      two things a single reading confuses: a real converter delay, and a
      bookkeeping error in the timestamps. Only one of them moves with the
      buffer, so varying it tells them apart.

      Reports the buffer-independent residual — the interface's own round
      trip, the number recordings are compensated by — and the slope, which
      is the correction to apply to raw timestamp arithmetic.

      Same options as `measure`, minus --buffer, which it varies itself.

      --json            emit one object on stdout and no prose, so a
                        conductor can store the result. `deepstar latency`
                        is that conductor; see its docs for where the
                        number ends up and who reads it.

  itajara measure --device <name> [options]
      Measure output→input round-trip latency by clicking and listening on
      every input at once. Needs a signal path from an output back to an
      input: a cable for the interface-only figure, or out → pedalboard →
      in for the figure that applies to anything recorded wet.

      Because it listens everywhere, one run also says which input channel
      that cable arrives on — and exposes any internal monitoring path,
      where a channel hears the click having crossed no converter at all.

      --device <name>   substring of the device name, case-insensitive
      --out-ch <n>      output channel to click on   (default 0, zero-based)
      --repeats <n>     how many clicks               (default 8)
      --amp <0..1>      click amplitude               (default 0.5)
      --rate <hz>       preferred sample rate         (default 48000)
      --buffer <n>      ask for a fixed callback size (default: device's own)
                        Diagnostic: if the measured offset moves with this,
                        it is buffer accounting rather than a property of
                        the interface, and must be stored per buffer size.

  This emits a short, loud click. Take headphones off and turn amps down.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    match cmd {
        "devices" => {
            devices::list();
            ExitCode::SUCCESS
        }
        // **Does this command line still mean what it meant?**
        //
        // Takes the same `--device` and `--source` flags as `loop`, resolves
        // them, prints where every source landed, and exits — opening nothing.
        // Worth running before a session on a rig with an aggregate, because
        // the one failure that costs a whole session is the silent one: an
        // interface that came back in a different order, absolute channel
        // numbers that still parse, meters that still move, and the wrong
        // input on every take.
        "sources" => match parse_loop(&args[1..]) {
            Ok(opts) => {
                match aggregate::layout_of(&opts.device) {
                    Some(l) => print!("{}", l.describe()),
                    None => println!("{} — cannot read its layout", opts.device),
                }
                println!();
                if opts.sources.is_empty() {
                    println!("no --source given: one mono source called `in` on channel {}",
                        opts.in_ch);
                } else {
                    for src in &opts.sources {
                        println!("  {}", src.describe());
                    }
                }
                // The output is as capable of moving as an input, and moving
                // it is not silence — it is the loop and the click arriving
                // somewhere else, which sounds like a patch problem.
                let out_where = match &opts.out_on {
                    Some(d) => format!(" on {d}"),
                    None => String::new(),
                };
                println!(
                    "\n  playback → out {}{}{}",
                    opts.out_ch + 1,
                    if opts.dual { "+" } else { "" },
                    if opts.dual {
                        format!("{}{}", opts.out_ch + 2, out_where)
                    } else {
                        out_where.clone()
                    }
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        },
        "levels" => match parse_levels(&args[1..]) {
            Ok(opts) => match levels::run(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n{}", e);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("{}\n\n{}", e, USAGE);
                ExitCode::FAILURE
            }
        },
        "loop" => match parse_loop(&args[1..]) {
            Ok(opts) => match engine::run(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n{}", e);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("{}\n\n{}", e, USAGE);
                ExitCode::FAILURE
            }
        },
        "align" => match parse_align(&args[1..]) {
            Ok(opts) => match align::run(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n{}", e);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("{}\n\n{}", e, USAGE);
                ExitCode::FAILURE
            }
        },
        "map" => match parse_measure(&args[1..]) {
            Ok(opts) => match measure::map(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n{}", e);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("{}\n\n{}", e, USAGE);
                ExitCode::FAILURE
            }
        },
        "sweep" => match parse_measure(&args[1..]) {
            Ok(opts) => match measure::sweep(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n{}", e);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("{}\n\n{}", e, USAGE);
                ExitCode::FAILURE
            }
        },
        "measure" => match parse_measure(&args[1..]) {
            Ok(opts) => match measure::run(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n{}", e);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("{}\n\n{}", e, USAGE);
                ExitCode::FAILURE
            }
        },
        "help" | "-h" | "--help" => {
            print!("{}", USAGE);
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command {:?}\n\n{}", other, USAGE);
            ExitCode::FAILURE
        }
    }
}

/// `name=l`, `name=l,r`, or `name=DEVICE:l[,r]` — a source's name and the
/// input channels it reads.
///
/// One-based on the command line and zero-based inside, because the jacks on
/// the interface are numbered from one and nothing about the engine's indexing
/// is the operator's problem.
///
/// # The third form, and why it is the one to use on an aggregate
///
/// `board=17,18` says where the pedalboard is *today*. An aggregate presents
/// its members' channels end to end, and that order is a property of the
/// aggregate as it currently stands — not of its name. Rebuild it, power-cycle
/// an interface, plug something in that was not there when it was made, and 17
/// is somewhere else. Nothing then fails: the meters move, the takes have
/// audio in them, and a whole session is recorded off the wrong input.
///
/// `board=AUDIO4c:1,2` says which interface and which of *its* jacks, and is
/// resolved against what CoreAudio reports at start. If that interface is not
/// in the aggregate, the daemon refuses to start rather than recording
/// whatever is at those numbers.
fn parse_source(v: &str) -> Result<engine::Source, String> {
    let (name, chans) = v
        .split_once('=')
        .ok_or_else(|| format!("--source wants name=channel, not `{}`", v))?;
    if name.is_empty() {
        return Err(format!("--source `{}` has no name", v));
    }
    // A colon means the channels are counted on one interface rather than on
    // the whole device. Split from the right, so a device name may contain one.
    let (on, chans) = match chans.rsplit_once(':') {
        Some((dev, rest)) if !dev.is_empty() && !rest.is_empty() =>
            (Some(dev.trim().to_string()), rest),
        _ => (None, chans),
    };
    let mut ch = [0usize; engine::CHANNELS];
    let parts: Vec<&str> = chans.split(',').collect();
    if parts.is_empty() || parts.len() > engine::CHANNELS {
        return Err(format!(
            "--source {} wants one or {} channels, not {}",
            name,
            engine::CHANNELS,
            parts.len()
        ));
    }
    for (i, part) in parts.iter().enumerate() {
        let n: usize = part
            .trim()
            .parse()
            .map_err(|_| format!("--source {}: `{}` is not a channel number", name, part))?;
        if n == 0 {
            return Err(format!("--source {}: channels count from 1", name));
        }
        ch[i] = n - 1;
    }
    // A single channel is a mono jack: the same input on both sides, so
    // nothing downstream needs a special case for it.
    if parts.len() == 1 {
        ch[1] = ch[0];
    }
    Ok(engine::Source { name: name.to_string(), ch, on, available: true })
}

/// Turn every `DEVICE:channel` source into a channel of the device being
/// opened, using what CoreAudio says the aggregate holds **now**.
///
/// Reads properties only; nothing is opened, no stream is started and no
/// sample rate is touched — which matters on a machine running a DAW, where
/// interfaces appearing and disappearing is the thing that upsets one.
fn resolve_sources(opts: &mut engine::Opts) -> Result<(), String> {
    if !opts.sources.iter().any(|s| s.on.is_some()) && opts.out_on.is_none() {
        return Ok(());
    }
    let layout = aggregate::layout_of(&opts.device).ok_or_else(|| {
        format!(
            "cannot read the layout of {:?}, so a source named by interface cannot be placed",
            opts.device
        )
    })?;

    // **The output moves too, and independently.** A member's inputs and its
    // outputs shift by the same amount only by coincidence. On this rig the
    // wrong answer is not silence — it is loop playback and the click going
    // into the modular instead of to the monitors, which sounds like a patch
    // problem and is not one.
    if let Some(dev) = opts.out_on.clone() {
        if layout.is_aggregate() {
            let m = layout
                .member(&dev)
                .map_err(|e| format!("--out-ch: {}", e))?;
            // Unlike a source, the output has nowhere to degrade to: a looper
            // whose playback goes nowhere is not a looper running without one
            // interface, it is a looper you cannot hear.
            if m.absent {
                return Err(format!(
                    "--out-ch names {}, which is configured into {} and not switched on. \
                     Playback has to go somewhere you can hear it.",
                    m.name, layout.name
                ));
            }
            let want = opts.out_ch + 1;
            if want as u32 > m.out_ch {
                return Err(format!(
                    "--out-ch: {} has {} output{}, so there is no channel {}",
                    m.name,
                    m.out_ch,
                    if m.out_ch == 1 { "" } else { "s" },
                    want
                ));
            }
            opts.out_ch += (m.first_out - 1) as usize;
        } else if !layout.name.to_lowercase().contains(&dev.to_lowercase()) {
            return Err(format!(
                "--out-ch: {:?} is not an aggregate, so it has no member {:?}",
                layout.name, dev
            ));
        }
    }

    if !layout.consistent() {
        return Err(format!(
            "{}: its members add up to {} inputs and the device reports {}. The channel \
             order is not understood — probably because a member is configured in and not \
             switched on — and resolving a source against a layout that is not understood \
             is how a session gets recorded off the wrong input. Run `itajara sources \
             --device {:?}` to see it, and either switch the missing interface on or name \
             a device that is all there.",
            layout.name,
            layout.members.iter().map(|m| m.in_ch).sum::<u32>(),
            layout.in_ch,
            layout.name
        ));
    }

    for s in opts.sources.iter_mut() {
        let Some(dev) = s.on.clone() else { continue };
        // A source may name the device itself, which is how a plain interface
        // takes the same spelling as an aggregate. Then the channels are
        // already the device's own.
        if !layout.is_aggregate() {
            if layout.name.to_lowercase().contains(&dev.to_lowercase()) {
                continue;
            }
            return Err(format!(
                "--source {}: {:?} is not an aggregate, so it has no member {:?}",
                s.name, layout.name, dev
            ));
        }
        let m = layout
            .member(&dev)
            .map_err(|e| format!("--source {}: {}", s.name, e))?;
        // **An interface that is not switched on is not an error.** The source
        // keeps its place — `src<n>` counts positions and dropping one would
        // renumber the rest — but it cannot be selected, and its channels are
        // parked somewhere in bounds so nothing can read past the buffer. The
        // looper runs on what IS there, which is what makes the modular
        // optional rather than load-bearing.
        if m.absent {
            s.available = false;
            s.ch = [0; engine::CHANNELS];
            continue;
        }
        for i in 0..engine::CHANNELS {
            let want = s.ch[i] + 1;
            if want as u32 > m.in_ch {
                return Err(format!(
                    "--source {}: {} has {} input{}, so there is no channel {}",
                    s.name,
                    m.name,
                    m.in_ch,
                    if m.in_ch == 1 { "" } else { "s" },
                    want
                ));
            }
            s.ch[i] += (m.first_in - 1) as usize;
        }
    }
    Ok(())
}

fn parse_loop(args: &[String]) -> Result<engine::Opts, String> {
    let mut opts = engine::Opts::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if flag == "--yes" {
            opts.yes = true;
            i += 1;
            continue;
        }
        if flag == "--click" {
            opts.click = true;
            i += 1;
            continue;
        }
        if flag == "--ws" {
            opts.ws_port = Some(3028);
            i += 1;
            continue;
        }
        if flag == "--link" {
            opts.link_port = Some(link::DEFAULT_ANCHOR_PORT);
            i += 1;
            continue;
        }
        if flag == "--monitor" {
            opts.monitor = true;
            i += 1;
            continue;
        }
        if flag == "--mono-out" {
            opts.dual = false;
            i += 1;
            continue;
        }
        let value = args
            .get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{} needs a value", flag))?;
        match flag {
            "--device" => opts.device = value,
            "--in-ch" => opts.in_ch = value.parse().map_err(|_| "--in-ch wants an integer")?,
            // `--source name=l[,r]`, repeatable. A name because "input 3" means
            // nothing with a guitar in your hands, and a pair because a stereo
            // pedalboard is two jacks. One channel means a mono jack, which is
            // recorded to both sides and folded back by a loop set to `mono`.
            "--source" => opts.sources.push(parse_source(&value)?),
            // `--out-ch AUDIO4c:1` for the same reason `--source` takes one:
            // an aggregate's output order is no more stable than its input
            // order, and the failure is playback arriving somewhere else.
            "--out-ch" => {
                let (dev, n) = match value.rsplit_once(':') {
                    Some((d, r)) if !d.is_empty() && !r.is_empty() =>
                        (Some(d.trim().to_string()), r.to_string()),
                    _ => (None, value.clone()),
                };
                // One-based when it names an interface, because that is how
                // the jacks are numbered; bare, it stays the zero-based index
                // every existing command line uses.
                let raw: usize = n.trim().parse().map_err(|_| "--out-ch wants an integer")?;
                if dev.is_some() {
                    if raw == 0 {
                        return Err("--out-ch: an interface's channels count from 1".into());
                    }
                    opts.out_ch = raw - 1;
                } else {
                    opts.out_ch = raw;
                }
                opts.out_on = dev;
            }
            "--residual" => {
                opts.residual = value.parse().map_err(|_| "--residual wants a number")?;
                opts.residual_given = true;
            }
            "--max-secs" => {
                opts.max_secs = value.parse().map_err(|_| "--max-secs wants a number")?;
                opts.max_secs_given = true;
            }
            "--loops" => {
                opts.loops = value.parse().map_err(|_| "--loops wants an integer")?;
                if opts.loops < 1 {
                    return Err("--loops wants at least one".to_string());
                }
            }
            "--layers" => {
                opts.layers = value.parse().map_err(|_| "--layers wants an integer")?;
                if opts.layers < 1 {
                    return Err("--layers wants at least one".to_string());
                }
            }
            "--fixed-secs" => {
                let f: f64 = value.parse().map_err(|_| "--fixed-secs wants a number of seconds")?;
                if f <= 0.0 {
                    return Err("--fixed-secs wants a length greater than zero".to_string());
                }
                opts.fixed_secs = Some(f);
            }
            "--rate" => opts.sample_rate = value.parse().map_err(|_| "--rate wants an integer")?,
            "--buffer" => opts.buffer = Some(value.parse().map_err(|_| "--buffer wants an integer")?),
            "--ws-port" => {
                opts.ws_port = Some(value.parse().map_err(|_| "--ws-port wants a port number")?)
            }
            "--ring-secs" => opts.ring_secs = value.parse().map_err(|_| "--ring-secs wants a number")?,
            "--capture-secs" => opts.capture_secs = value.parse().map_err(|_| "--capture-secs wants a number")?,
            "--takes-dir" => opts.takes_dir = value.into(),
            "--link-port" => {
                opts.link_port = Some(value.parse().map_err(|_| "--link-port wants a port number")?)
            }
            "--preroll-ms" => opts.preroll_ms = value.parse().map_err(|_| "--preroll-ms wants a number")?,
            "--arm-db" => opts.arm_db = value.parse().map_err(|_| "--arm-db wants a number of dBFS")?,
            "--selftest" => {
                opts.selftest = Some(value.parse().map_err(|_| "--selftest wants a length in seconds")?)
            }
            other => return Err(format!("unknown option {:?}", other)),
        }
        i += 2;
    }
    if opts.device.is_empty() {
        return Err("loop needs --device".into());
    }
    resolve_sources(&mut opts)?;
    if opts.max_secs <= 0.0 {
        return Err("--max-secs must be positive".into());
    }
    if let Some(f) = opts.fixed_secs {
        if !opts.max_secs_given {
            opts.max_secs = f;
        }
    }
    Ok(opts)
}

fn parse_align(args: &[String]) -> Result<align::Opts, String> {
    let mut opts = align::Opts::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = args
            .get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{} needs a value", flag))?;
        match flag {
            "--device" => opts.device = value,
            "--out-ch" => opts.out_ch = value.parse().map_err(|_| "--out-ch wants an integer")?,
            "--in-ch" => opts.in_ch = value.parse().map_err(|_| "--in-ch wants an integer")?,
            "--residual" => {
                opts.residual = value.parse().map_err(|_| "--residual wants a number")?;
                opts.residual_given = true;
            }
            "--loop-secs" => {
                opts.loop_secs = value.parse().map_err(|_| "--loop-secs wants a number")?
            }
            "--cycles" => opts.cycles = value.parse().map_err(|_| "--cycles wants an integer")?,
            "--amp" => opts.amplitude = value.parse().map_err(|_| "--amp wants a number")?,
            "--rate" => opts.sample_rate = value.parse().map_err(|_| "--rate wants an integer")?,
            "--buffer" => {
                opts.buffer = Some(value.parse().map_err(|_| "--buffer wants an integer")?)
            }
            other => return Err(format!("unknown option {:?}", other)),
        }
        i += 2;
    }
    if opts.device.is_empty() {
        return Err("align needs --device".into());
    }
    if opts.loop_secs <= 0.0 {
        return Err("--loop-secs must be positive".into());
    }
    Ok(opts)
}

fn parse_levels(args: &[String]) -> Result<levels::Opts, String> {
    let mut opts = levels::Opts::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = args
            .get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{} needs a value", flag))?;
        match flag {
            "--device" => opts.device = value,
            "--seconds" => {
                opts.seconds = value.parse().map_err(|_| "--seconds wants an integer")?
            }
            "--rate" => opts.sample_rate = value.parse().map_err(|_| "--rate wants an integer")?,
            other => return Err(format!("unknown option {:?}", other)),
        }
        i += 2;
    }
    if opts.device.is_empty() {
        return Err("levels needs --device".into());
    }
    Ok(opts)
}

fn parse_measure(args: &[String]) -> Result<measure::Opts, String> {
    let mut opts = measure::Opts::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if flag == "--json" {
            opts.json = true;
            i += 1;
            continue;
        }
        let value = || {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", flag))
        };
        match flag {
            "--device" => opts.device = value()?,
            "--out-ch" => opts.out_ch = value()?.parse().map_err(|_| "--out-ch wants an integer")?,
            "--repeats" => {
                opts.repeats = value()?.parse().map_err(|_| "--repeats wants an integer")?
            }
            "--amp" => opts.amplitude = value()?.parse().map_err(|_| "--amp wants a number")?,
            "--buffer" => {
                opts.buffer =
                    Some(value()?.parse().map_err(|_| "--buffer wants an integer")?)
            }
            "--rate" => {
                opts.sample_rate = value()?.parse().map_err(|_| "--rate wants an integer")?
            }
            other => return Err(format!("unknown option {:?}", other)),
        }
        i += 2;
    }

    if opts.device.is_empty() {
        return Err("measure needs --device".into());
    }
    if !(0.0..=1.0).contains(&opts.amplitude) {
        return Err("--amp must be between 0 and 1".into());
    }
    if opts.repeats == 0 {
        return Err("--repeats must be at least 1".into());
    }
    Ok(opts)
}
