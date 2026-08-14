//! itajara — the looper engine for producing-with-your-feet.
//!
//! See `docs/DESIGN-LOOPER.md` for what this is going to be. Today it is the
//! metrology: §10 of that document lists three latencies that have to be
//! measured rather than guessed, and §15 says to measure the audio round trip
//! before writing any overdub code. This is that.

mod align;
mod devices;
mod engine;
mod ws;
mod levels;
mod measure;

use std::process::ExitCode;

const USAGE: &str = "\
itajara — looper engine for producing-with-your-feet

USAGE
  itajara devices
      List the audio devices CoreAudio can see, with channel counts and the
      sample rates each will accept.

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

      --residual <n>    from `sweep`, for this configuration  (default 252)
      --max-secs <s>    longest loop, and so the arena size   (default 30)
      --click           metronome at loop position zero
      --monitor         pass live input to the output. Off by default: the
                        interface's own direct monitoring costs no latency
                        where this costs the round trip plus a buffer
      --mono-out        send the mix to one channel instead of a pair
      --ws              serve the app on ws://127.0.0.1:3028
      --ws-port <n>     ...on a different port
      --ring-secs <s>   how much of the past stays claimable      (default 60)
      --preroll-ms <n>  how far before the tap the first loop actually
                        starts, pulled from the pre-roll           (default 0)
      --selftest <s>    record one cycle of the engine's own click through a
                        loopback cable and check where it landed

  itajara align --device <name> [options]
      The self-test. Plays a click at loop position zero, records it back
      through a patch cable, and reports which position it landed at. Zero
      means the arithmetic that places recorded audio in the loop is right,
      and overdubs will stack without accumulating drift.

      This is the only part of a looper that can be verified rather than
      judged by ear. Run it whenever the audio configuration changes.

      --residual <n>    the interface's transit, from `sweep`  (default 252)
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

fn parse_loop(args: &[String]) -> Result<engine::Opts, String> {
    let mut opts = engine::Opts::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
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
            "--out-ch" => opts.out_ch = value.parse().map_err(|_| "--out-ch wants an integer")?,
            "--residual" => opts.residual = value.parse().map_err(|_| "--residual wants a number")?,
            "--max-secs" => opts.max_secs = value.parse().map_err(|_| "--max-secs wants a number")?,
            "--rate" => opts.sample_rate = value.parse().map_err(|_| "--rate wants an integer")?,
            "--buffer" => opts.buffer = Some(value.parse().map_err(|_| "--buffer wants an integer")?),
            "--ws-port" => {
                opts.ws_port = Some(value.parse().map_err(|_| "--ws-port wants a port number")?)
            }
            "--ring-secs" => opts.ring_secs = value.parse().map_err(|_| "--ring-secs wants a number")?,
            "--preroll-ms" => opts.preroll_ms = value.parse().map_err(|_| "--preroll-ms wants a number")?,
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
    if opts.max_secs <= 0.0 {
        return Err("--max-secs must be positive".into());
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
                opts.residual = value.parse().map_err(|_| "--residual wants a number")?
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
