//! The one control lane: every command, from the console and from every
//! socket, runs on one thread that owns `dispatch`.
//!
//! Made 2026-09-06 (REVIEW-daemon-debt step 7). Until then `dispatch` ran on
//! three kinds of thread with no lock between them — the console thread, a
//! serialised edit worker per connection, and a thread spawned per other
//! command — because some verbs blocked inside it, and a press must not
//! queue behind a multiply waiting for its cycle boundary. Everything is
//! atomic, so it was probably safe; "probably" was the word, and the phase
//! table (step 5a) had to leave out the pairs only a race between two of
//! those threads could make. Now there is one lane, and nothing on it
//! waits. The survey that made that possible is below.
//!
//! # Where `dispatch` blocked, or took long
//!
//! | verb | function | what | how long | class |
//! |---|---|---|---|---|
//! | `r` on a quantised first take | `commit` | slept in 5 ms polls until `origin + n·bar` | up to half a bar | (a) timed wait, decides when the phase flips |
//! | `r`, `x` on a multiply | `multiply_end` | slept in 5 ms polls until `rec_from + n·cycle` | up to half a cycle | (a) timed wait, decides when the phase flips |
//! | `r` closing any take, `x`/`r` ending a multiply | `commit`, `multiply_end` | slept 60 ms after the flip for the input to drain | 60 ms | (a′) timed wait after the flip; decides nothing about phase, but the layer cannot be shaped before it |
//! | `w` | `save_take` | one WAV per layer plus a manifest to disk | a file write per layer | (b) slow, decides nothing |
//! | `ex` | `export_set` | renders every loop (`render_loop`) and writes one WAV each | a render and a write per loop | (b) |
//! | `exl` | `export_layers` | writes every loop's layers and two manifests | a write per layer | (b) |
//! | `pk` | the `pk` arm | `mix_at` over the whole loop into up to 4000 buckets | linear in the loop; tens of ms on a long take | (b) |
//! | `t`, `x` start, `cp`, `dp`, `rvx1`, `c`, a first take's pre-roll shift, every layer draw | `take`, `multiply_start`, `copy_layers`, the arms, `flatten`, `zero_layer`, `commit` | reads or writes a layer's worth of the arena | linear in the layer, or in `--max-secs` for `zero_layer` | (c) fast in shape; state-touching, so it stays here |
//! | everything else | the arms | a few atomics | microseconds | (c) fast |
//!
//! # What moved
//!
//! **The timed waits (a) left `dispatch` for the closer's road.** A sized
//! take already closed itself: the callback armed `close_at` and a closer
//! thread committed at the frame with `late = now − at`. A quantised close
//! and a multiply end now file themselves the same way — `close_at` for the
//! boundary, and beside it (`Loop::closing`, a `Filed`) the press that
//! filed it, so the close fires with the press's own frame and lateness and
//! the layer is born on the pass the foot went down on, exactly as when the
//! press slept through the wait itself. The verb returns at once with what
//! is going to happen; the lane's tick fires the close at the frame.
//!
//! **The drain (a′) became a stage rather than a sleep.** The flip to
//! `Playing` is what stops the input writing, and it happens in the fast
//! half; the layer is shaped sixty milliseconds later, by the tick, from a
//! `Take` the fast half left on the loop. The only thing that waits for a
//! drain is a command addressed to *that* loop, which the lane holds until
//! the finish has run — a press on any other loop goes straight through.
//! That hold is the sequential form of the race the old design had: a `c`
//! or an `r` during a sleeping commit used to run beside it on another
//! thread and either could win; now the close finishes first, always.
//!
//! **The slow work (b) left the lane the other way.** The fast,
//! state-touching part of each — the guards, the name, the directory, the
//! window and bucket count — runs here; the render and the writes go to one
//! slow thread as a `Job`, and the ack comes back through the same `say` as
//! every other, so no verb goes silent (the review's fourteen).
//!
//! # The lane's shape
//!
//! One `mpsc` channel of `Command`s; the console thread and every socket
//! connection are producers, each tagging its commands with who sent them
//! so the ack goes back the way it always did (`say`). The worker blocks on
//! the channel for at most `TICK` — the closer's five milliseconds — then
//! drains what has arrived into a batch, coalesces it, runs it, and ticks:
//! fires any close whose frame has come, finishes any take whose drain is
//! due. Coalescing keeps only the last `pk` per loop in a batch — a picture
//! is superseded by the next one and nothing between is lost. `in`, `out`
//! and `rot` are *not* coalesced: each is validated against the view the
//! one before it left, `rot` is relative, and the settle in
//! `schedule_restart` already folds a moving slider into one restart, which
//! is the coalescing the ear needed.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::commit::{finish_take, fire_due, Stage};
use super::dispatch::dispatch_ack;
use super::shared::Shared;
use super::verb::tokenize;
use super::{Ack, Job};

/// The closer's interval, now the lane's: how long the worker waits for a
/// command before looking at the frame counter. A fortieth of the shortest
/// bar anyone will use, and the close it produces is quantised to the frame
/// by construction, so the poll's jitter never reaches the audio.
const TICK: Duration = Duration::from_millis(5);

/// Who sent a command, so its ack goes back the way it always did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caller {
    /// The stdin console: the ack is printed, and not carried in the snapshot.
    Console,
    /// A socket: printed as `[app]` and carried in the snapshot.
    App,
    /// The engine itself — a take that reached its length, a close nobody
    /// pressed for: printed, and carried in the snapshot, as the closer's were.
    Engine,
}

/// One command on its way to the lane.
pub(crate) struct Command {
    pub(crate) from: Caller,
    pub(crate) line: String,
}

/// A handle every producer holds: cloneable, and `send` is the whole of it.
#[derive(Clone)]
pub(crate) struct Lane {
    tx: Sender<Command>,
}

impl Lane {
    /// Start the lane: the worker that owns `dispatch`, and the slow thread
    /// that runs what it defers.
    pub(crate) fn spawn(sh: Arc<Shared>, sr: u32) -> Lane {
        Self::spawn_tapped(sh, sr, None)
    }

    /// As `spawn`, with every ack also sent down `tap`, for tests.
    pub(crate) fn spawn_tapped(sh: Arc<Shared>, sr: u32, tap: Option<Sender<String>>) -> Lane {
        let (tx, rx) = channel::<Command>();
        let (slow_tx, slow_rx) = channel::<(Caller, Job)>();
        {
            let sh = sh.clone();
            let tap = tap.clone();
            std::thread::spawn(move || {
                for (from, job) in slow_rx {
                    let ack = job(&sh);
                    say(&sh, from, &ack, tap.as_ref());
                }
            });
        }
        std::thread::spawn(move || work(sh, sr, rx, slow_tx, tap));
        Lane { tx }
    }

    /// Put a command on the lane. False if the lane is gone, which is the
    /// daemon on its way out.
    pub(crate) fn send(&self, from: Caller, line: String) -> bool {
        self.tx.send(Command { from, line }).is_ok()
    }
}

/// The worker: batches, ticks, and never waits on a verb.
fn work(
    sh: Arc<Shared>,
    sr: u32,
    rx: Receiver<Command>,
    slow: Sender<(Caller, Job)>,
    tap: Option<Sender<String>>,
) {
    loop {
        let first = match rx.recv_timeout(TICK) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some(first) = first {
            let mut batch = vec![first];
            while let Ok(c) = rx.try_recv() {
                batch.push(c);
            }
            for cmd in coalesce(batch) {
                run(&sh, sr, cmd, &slow, tap.as_ref());
            }
        }
        tick(&sh, sr, tap.as_ref());
    }
}

/// One command: hold for the drain of the loop it addresses, dispatch, and
/// say or defer the ack.
fn run(sh: &Arc<Shared>, sr: u32, cmd: Command, slow: &Sender<(Caller, Job)>, tap: Option<&Sender<String>>) {
    for li in touched(sh, &cmd.line) {
        hold_for(sh, li, sr, tap);
    }
    match dispatch_ack(sh, sr, &cmd.line, cmd.from) {
        Ack::Now(ack) => say(sh, cmd.from, &ack, tap),
        Ack::Later(job) => {
            // A closed slow thread is the daemon on its way out; the job is
            // run here rather than lost, since it was already promised.
            if let Err(e) = slow.send((cmd.from, job)) {
                let ack = (e.0).1(sh);
                say(sh, cmd.from, &ack, tap);
            }
        }
    }
}

/// The tick: fire every close whose frame has come, finish every take whose
/// drain is due. What the closer thread did, on the lane.
fn tick(sh: &Shared, sr: u32, tap: Option<&Sender<String>>) {
    let now = sh.out_frames.load(Ordering::Acquire) as i64;
    for li in 0..sh.n_loops {
        fire_due(sh, li, sr, now);
    }
    let at = Instant::now();
    for li in 0..sh.n_loops {
        if let Some(t) = sh.lp(li).take_drained(at) {
            let from = t.from;
            let ack = finish_take(sh, li, sr, t);
            say(sh, from, &ack, tap);
        }
    }
}

/// Before a command touches loop `li`: if its close's frame has come, fire
/// it; if its take is draining, wait for the drain and finish. A close
/// filed for a frame still ahead is left standing — the guards refuse what
/// would disturb it and `c` cancels it.
fn hold_for(sh: &Shared, li: usize, sr: u32, tap: Option<&Sender<String>>) {
    let now = sh.out_frames.load(Ordering::Acquire) as i64;
    fire_due(sh, li, sr, now);
    if let Some(Stage::Draining(due)) = sh.lp(li).closing_stage() {
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        if let Some(t) = sh.lp(li).take_drained(Instant::now()) {
            let from = t.from;
            let ack = finish_take(sh, li, sr, t);
            say(sh, from, &ack, tap);
        }
    }
}

/// The loops a command may change: the one its digits name, or the
/// selection; every loop for the three rig-wide verbs that read or restart
/// them all.
fn touched(sh: &Shared, line: &str) -> Vec<usize> {
    let (li, word) = parse(sh, line);
    match word {
        Some("go") | Some("ex") | Some("exl") => (0..sh.n_loops).collect(),
        _ => vec![li],
    }
}

/// The loop and the verb word of a command, read the way `dispatch` reads
/// them: digits, then the word. A line `dispatch` will refuse reads as the
/// selected loop and no word.
fn parse<'a>(sh: &Shared, line: &'a str) -> (usize, Option<&'static str>) {
    let line = match line.rsplit_once('@') {
        Some((cmd, _)) => cmd,
        None => line,
    };
    let trimmed = line.trim();
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let (li, rest) = if digits > 0 {
        match trimmed[..digits].parse::<usize>() {
            Ok(n) if n < sh.n_loops => (n, trimmed[digits..].trim()),
            _ => return (sh.sel(), None),
        }
    } else {
        (sh.sel(), trimmed)
    };
    (li, tokenize(rest).map(|(v, _)| v.word))
}

/// Keep only the last `pk` per loop in a batch; everything else in order.
fn coalesce(batch: Vec<Command>) -> Vec<Command> {
    let n = batch.len();
    let mut keep = vec![true; n];
    let mut last_pk: Vec<Option<usize>> = Vec::new();
    // Read without a `Shared`: the loop digits alone, so the predicate is
    // pure and testable. A `pk` with no digits is the selection's, which is
    // one loop whatever it is.
    for (i, c) in batch.iter().enumerate() {
        let line = c.line.rsplit_once('@').map(|(l, _)| l).unwrap_or(&c.line).trim();
        let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
        let rest = line[digits..].trim();
        let is_pk = matches!(tokenize(rest), Some((v, _)) if v.word == "pk");
        if !is_pk {
            continue;
        }
        let li = line[..digits].parse::<usize>().map(|l| l + 1).unwrap_or(0);
        if last_pk.len() <= li {
            last_pk.resize(li + 1, None);
        }
        if let Some(prev) = last_pk[li] {
            keep[prev] = false;
        }
        last_pk[li] = Some(i);
    }
    batch
        .into_iter()
        .zip(keep)
        .filter_map(|(c, k)| if k { Some(c) } else { None })
        .collect()
}

/// The ack path. Every ack, from every verb, on every road, goes through
/// here, and an empty one is nothing to say rather than something to hide:
/// the `[cmd]` line at the socket already recorded that the command
/// arrived.
fn say(sh: &Shared, from: Caller, ack: &str, tap: Option<&Sender<String>>) {
    if ack.is_empty() {
        return;
    }
    match from {
        Caller::Console => println!("  {}", ack),
        Caller::App => {
            println!("  [app] {}", ack);
            sh.note_ack(ack);
        }
        Caller::Engine => {
            println!("  {}", ack);
            sh.note_ack(ack);
        }
    }
    if let Some(t) = tap {
        let _ = t.send(ack.to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{coalesce, Caller, Command, Lane};
    use crate::engine::tests::rig;
    use crate::engine::Phase;

    fn cmds(lines: &[&str]) -> Vec<Command> {
        lines.iter().map(|l| Command { from: Caller::App, line: l.to_string() }).collect()
    }

    /// **A batch keeps every command in order and only the last picture
    /// per loop.** Edits are not coalesced — a `rot` is relative and an
    /// `in` is checked against the `out` before it — and a `pk` on one loop
    /// does not stand in for a `pk` on another.
    #[test]
    fn a_batch_coalesces_pictures_and_nothing_else() {
        let out: Vec<String> = coalesce(cmds(&[
            "0in20", "0pk16", "0in30", "0pk32", "1pk16", "0rot10", "0rot-10", "0pk64@3", "pk8",
        ]))
        .into_iter()
        .map(|c| c.line)
        .collect();
        assert_eq!(
            out,
            vec!["0in20", "0in30", "1pk16", "0rot10", "0rot-10", "0pk64@3", "pk8"]
        );
    }

    /// **Two producers, one order each.** Every command from either producer
    /// is dispatched, in the order that producer sent it: the acks of one
    /// producer's run of `vol`s read as a monotone sequence, whatever the
    /// interleaving between the two.
    #[test]
    fn commands_from_two_producers_are_dispatched_in_each_one_s_order() {
        let sh = Arc::new(rig(1000));
        let (tap, acks) = channel::<String>();
        let lane = Lane::spawn_tapped(sh.clone(), 1000, Some(tap));
        const N: usize = 40;
        let a = {
            let lane = lane.clone();
            std::thread::spawn(move || {
                for i in 1..=N {
                    assert!(lane.send(Caller::App, format!("0vol-{}", i)));
                }
            })
        };
        let b = {
            let lane = lane.clone();
            std::thread::spawn(move || {
                for i in 1..=N {
                    assert!(lane.send(Caller::Console, format!("1vol-{}", i)));
                }
            })
        };
        a.join().unwrap();
        b.join().unwrap();
        let mut got: Vec<String> = Vec::new();
        while got.len() < 2 * N {
            got.push(acks.recv_timeout(Duration::from_secs(5)).expect("an ack for every command"));
        }
        // "loop 0 plays 3.0 dB down."
        let of = |head: &str| -> Vec<f64> {
            got.iter()
                .filter(|a| a.starts_with(head))
                .map(|a| a[head.len()..].split(' ').next().unwrap().parse::<f64>().unwrap())
                .collect()
        };
        for head in ["loop 0 plays ", "loop 1 plays "] {
            let seen = of(head);
            assert_eq!(seen.len(), N, "{}", head);
            assert!(seen.windows(2).all(|w| w[0] < w[1]), "{}: {:?}", head, seen);
        }
        assert_eq!(f32::from_bits(sh.lp(0).vol.load(Ordering::Relaxed)), 10f32.powf(-(N as f32) / 20.0));
    }

    /// **A press on a draining loop waits for the drain; on any other loop
    /// it does not.** Loop 0 is closing a take; the `u` sent right behind the
    /// close finds the layer laid, where an unheld `u` would have found
    /// nothing to undo — and the `vol` on loop 1 sent between them answers
    /// before either.
    #[test]
    fn a_command_on_a_draining_loop_is_held_until_the_layer_is_laid() {
        let sh = Arc::new(rig(1000));
        let (tap, acks) = channel::<String>();
        let lane = Lane::spawn_tapped(sh.clone(), 1000, Some(tap));
        let lp = sh.lp(0);
        lp.enter(Phase::First, 0);
        lp.reached.store(100, Ordering::Release);
        sh.out_frames.store(100, Ordering::Release);
        assert!(lane.send(Caller::App, "0r".into()));
        assert!(lane.send(Caller::App, "1vol-6".into()));
        assert!(lane.send(Caller::App, "0u".into()));
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(acks.recv_timeout(Duration::from_secs(5)).expect("three acks"));
        }
        assert_eq!(
            got,
            vec![
                "loop 1 plays 6.0 dB down.".to_string(),
                "loop 0 committed: 0.100 s, 1 layer playing.".to_string(),
                "loop 0 layer 1 removed. Empty now, but still 0.100 s long, so the next take lands on the same grid — `0z` to forget the length.".to_string(),
            ]
        );
    }
}
