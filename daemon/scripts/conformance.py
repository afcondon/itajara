#!/usr/bin/env python3
"""Send every verb the app can send, and record what comes back.

## What this closes

`tools/check-verbs.py` proves each verb has an arm in `dispatch` — a textual
claim about two source files. It says nothing about whether the daemon *accepts*
the command, what it does with the argument, or whether the app would ever hear
about it. This is the other end: talk to a running daemon and write down the
answer.

The interesting column is not "did it work" but **"did it say anything"**. The
daemon acks by *returning* a string from `dispatch`; a great many arms
`println!` to stdout and return unit instead, and an ack that never leaves the
daemon is invisible to the app. A refused press and a lost press then look
identical from a chair, which is a known and long-standing hole. This turns that
from a note into a table.

## Safety

The rig usually has takes in it, and the author of this file has previously
destroyed one by running tests on a live rig. So:

* **One scratch loop, and it must already be empty.** Nothing is ever addressed
  to any other loop. If the scratch loop has layers or a length, this refuses to
  run rather than clearing it for you.
* **It refuses while anything anywhere is recording.**
* **Global verbs are restored.** `k` and `m` are the metronome and input
  monitoring, which belong to the whole rig and not to a loop. Their current
  values are read first and put back after, and the bare toggling forms are sent
  in pairs so the rig ends where it started.
* Phase B — which needs actual material and so actually records — is opt-in
  behind `--with-audio`, and clears only the scratch loop when it is done.

Everything in phase A runs against an empty loop, so most verbs answer with a
refusal. **That is the point**: a refusal is an ack, it exercises the reporting
path, and it destroys nothing.

## The limitation, stated rather than discovered later

Phase A tests the *nothing-to-do* path. A verb can perfectly well `return` a
refusal on an empty loop and `println!` its success — so "acks" here means "acks
when it has nothing to do", which is weaker than it sounds. Phase B exists to
test the other side and needs `--with-audio`.

Usage:
    python3 conformance.py                  # phase A only, scratch loop 5
    python3 conformance.py --loop 4         # different scratch loop
    python3 conformance.py --with-audio     # also phase B: records ~3s, clears after
"""

import argparse
import sys
import time

from rig import Rig

WRITING = ("recordingFirst", "overdubbing", "multiplying")

# Every verb `Data.Looper.Verb.render` can produce, in the shape the app sends
# it. Transcribed from Verb.purs; `tools/check-verbs.py` is what keeps the
# spellings honest against the daemon, and this list honest against Verb.purs is
# checked by `--audit-coverage`.
#
#   (label, suffix-after-the-loop-index, needs_material)
PER_LOOP = [
    ("Record",        "r",        False),
    ("Multiply",      "x",        False),
    ("Spread 2",      "s2",       True),
    ("Rotate",        "o",        True),
    ("Dense",         "d",        True),
    ("Undo",          "u",        False),
    ("Redo",          "y",        False),
    ("ForgetLength",  "z",        False),
    ("Clear",         "c",        False),
    ("Fire",          "f",        False),
    ("ClaimPast",     "t",        True),   # records from the ring — phase B only
    ("SaveTake",      "w",        True),   # writes files — phase B only
    ("Sounding on",   "h1",       False),
    ("Sounding off",  "h0",       False),
    ("OnGrid on",     "g1",       False),
    ("OnGrid off",    "g0",       False),
    ("Reversed on",   "rev1",     False),
    ("Reversed off",  "rev0",     False),
    ("Pendulum on",   "pend1",    False),
    ("Pendulum off",  "pend0",    False),
    ("OneShot on",    "one1",     False),
    ("OneShot off",   "one0",     False),
    ("LevelArm on",   "lev1",     False),
    ("LevelArm off",  "lev0",     False),
    ("Rate",          "sp1.0",    False),
    ("Place",         "pan64",    False),
    ("Fade",          "xf0.0",    False),
    ("Decay",         "dec0.0",   False),
    ("Chance",        "ch1.0",    False),
]

# Not loop-scoped. Sent bare, exactly as the app sends them, and restored after.
GLOBAL = [
    ("Click on",      "k1"),
    ("Click off",     "k0"),
    ("ClickToggle",   "k"),
    ("Monitor on",    "m1"),
    ("Monitor off",   "m0"),
]


class Probe:
    """Sends one command and waits for the ack counter to move."""

    def __init__(self, rig):
        self.rig = rig
        snap = rig.snapshot(0.3)
        self.seq = snap["ackSeq"]

    def send(self, text, settle=0.35):
        before = self.seq
        self.rig.send(text)
        deadline = time.time() + 1.2
        while time.time() < deadline:
            snap = self.rig.snapshot(settle)
            if snap and snap["ackSeq"] != before:
                self.seq = snap["ackSeq"]
                return snap["ack"], snap
            if snap:
                last = snap
        return None, last


def loop_of(snap, i):
    return snap["loops"][i]


def refuse(msg):
    print(f"\nREFUSING TO RUN: {msg}")
    print("Nothing was sent.")
    sys.exit(2)


def preflight(rig, scratch):
    snap = rig.snapshot(0.4)
    if snap is None:
        refuse("no snapshot from the daemon — is it running on :23028?")
    if not snap["audioAlive"]:
        refuse("the daemon says its audio callbacks are not running")

    busy = [l["index"] for l in snap["loops"] if l["state"] in WRITING]
    if busy:
        refuse(f"loop(s) {busy} are recording right now")

    if scratch >= snap["nLoops"]:
        refuse(f"there are {snap['nLoops']} loops, numbered 0 to {snap['nLoops'] - 1}")

    lp = loop_of(snap, scratch)
    if lp["layers"] or lp["loopFrames"]:
        refuse(
            f"scratch loop {scratch} is not empty "
            f"({lp['layers']} layers, {lp['loopSecs']:.2f}s). "
            f"Pick an empty one with --loop, or clear it yourself if you mean to."
        )

    occupied = [(l["index"], l["layers"], l["loopSecs"])
                for l in snap["loops"] if l["layers"] or l["loopFrames"]]
    print(f"Daemon alive, {snap['nLoops']} loops, sr {snap['sampleRate']}.")
    if occupied:
        print("Loops with material in them, which will NOT be touched:")
        for i, n, secs in occupied:
            print(f"  loop {i}: {n} layer(s), {secs:.2f}s")
    print(f"Scratch loop: {scratch} (empty, confirmed).")
    return snap


def run(rows, probe, scratch=None, phase=""):
    """Send each row; return (label, sent, ack-or-None)."""
    out = []
    for label, suffix in rows:
        text = f"{scratch}{suffix}" if scratch is not None else suffix
        ack, _ = probe.send(text)
        out.append((label, text, ack))
        mark = "ack " if ack else "SILENT"
        print(f"  {mark}  {text:<10} {label:<16} {ack or ''}")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--loop", type=int, default=5, help="scratch loop (default 5)")
    ap.add_argument("--with-audio", action="store_true",
                    help="also run phase B, which records ~3s into the scratch loop")
    args = ap.parse_args()

    rig = Rig()
    before = preflight(rig, args.loop)
    probe = Probe(rig)
    results = []

    print("\n--- Phase A: every verb against an empty loop ---")
    print("    (a refusal is an ack and is the expected answer for most of these)")
    print("    NB `r` starts a take on the scratch loop; it is cleared below.")
    rows = [(lbl, sfx) for lbl, sfx, needs in PER_LOOP if not needs]
    results += run(rows, probe, scratch=args.loop)
    # `r` opened the input on the scratch loop and `c` appears in the list above,
    # so this is usually a no-op — usually is not a guarantee, and leaving a loop
    # listening because the ordering changed is exactly the trap Stop All used to
    # set. Explicit beats incidental.
    rig.send(f"{args.loop}c")
    time.sleep(0.3)

    print("\n--- Global verbs, restored afterwards ---")
    click0, mon0 = before["click"], before["monitor"]
    print(f"    click was {click0}, monitor was {mon0}")
    results += run(GLOBAL, probe)
    # Put the rig back exactly as found, whatever the toggles did.
    rig.send("k1" if click0 else "k0")
    rig.send("m1" if mon0 else "m0")
    time.sleep(0.3)

    skipped = [(lbl, sfx) for lbl, sfx, needs in PER_LOOP if needs]
    if args.with_audio:
        print("\n--- Phase B: with material (records into the scratch loop) ---")
        probe.send(f"{args.loop}r")
        time.sleep(3.0)
        probe.send(f"{args.loop}r")
        time.sleep(0.4)
        snap = rig.snapshot(0.3)
        lp = loop_of(snap, args.loop)
        print(f"    recorded {lp['loopSecs']:.2f}s, {lp['layers']} layer(s)")
        results += run(skipped, probe, scratch=args.loop)
        print("    clearing the scratch loop")
        probe.send(f"{args.loop}c")
    else:
        print("\n--- Phase B skipped (no --with-audio) ---")
        print("    These need material, and one of them writes files:")
        for lbl, sfx in skipped:
            print(f"      {sfx:<10} {lbl}")

    after = rig.snapshot(0.4)
    print("\n=== Summary ===")
    silent = [r for r in results if r[2] is None]
    print(f"  {len(results)} verbs sent, "
          f"{len(results) - len(silent)} acked, {len(silent)} silent")
    if silent:
        print("\n  SILENT — the daemon did something (or refused) and the app "
              "cannot know which:")
        for label, text, _ in silent:
            print(f"    {text:<10} {label}")

    unknown = [r for r in results if r[2] and "unknown command" in r[2]]
    if unknown:
        print("\n  UNKNOWN TO THE DAEMON:")
        for label, text, ack in unknown:
            print(f"    {text:<10} {label:<16} {ack}")

    print("\n=== Rig left as found? ===")
    ok = True
    for i in range(before["nLoops"]):
        b, a = loop_of(before, i), loop_of(after, i)
        if (b["layers"], b["loopFrames"]) != (a["layers"], a["loopFrames"]):
            print(f"  loop {i}: CHANGED  {b['layers']}L/{b['loopSecs']:.2f}s "
                  f"-> {a['layers']}L/{a['loopSecs']:.2f}s")
            ok = False
    if before["click"] != after["click"] or before["monitor"] != after["monitor"]:
        print(f"  globals: CHANGED click {before['click']}->{after['click']} "
              f"monitor {before['monitor']}->{after['monitor']}")
        ok = False
    if ok:
        print("  yes — every loop has the layers and length it started with, "
              "and the globals are back.")

    rig.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
