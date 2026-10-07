#!/usr/bin/env python3
"""What does each plug, unplug and reopen cost in CoreAudio's buffers?

On 2026-10-07 the `loop` daemon had been up 12½ days with a physical footprint of
11.2 GB, 7 GB of it swapped and therefore invisible to `ps`'s RSS. The arena was
not it (2.1 GB reserved, nearly untouched). It was 9.6 GB in CoreAudio's
`caulk::audio_buffer_resource` malloc zone — about 480k blocks of ~20 KB that
our code never allocates directly. Over 93 s of idle the block count did not
move, so the growth comes in bursts.

The first guess was the daemon's own reopens (`supervise`, engine/run.rs). The
first run of this tool said otherwise: unplugging the A4C from the "ES9 then
A4C" aggregate grew the zone by ~200 MB **without the daemon logging anything**.
An aggregate whose sub-device leaves keeps running on what remains, so
`supervise` never sees a fault — yet CoreAudio allocated anyway. So the events
worth timing are the interfaces coming and going, whether or not the daemon
reacts, and the reopens are only one kind among them.

Events are therefore taken from two places:

  ioreg       the USB devices present, polled every 0.5 s (0.03 s a poll, and
              it never touches CoreAudio): every arrival and departure
  the log     the daemon's "reopening" / "is back", when it does react

After each event the zone is sampled every STEP seconds until it has been still for
QUIET seconds, and that settled figure minus the one before the event is the
event's cost. Events that land while an earlier one is still settling are
merged into it, so unplug-and-replug in quick succession reads as one row.

**Measured as a block count, from a forked corpse.** The zone's byte size is
the wrong figure: it moved by 460 MB in minutes with nothing plugged or
unplugged, as pages were compressed and swapped. The number of live blocks in
the zone does not care where a page lives. `vmmap -forkCorpse` reads it from a
copy-on-write fork in ~4.5 s without stopping the daemon; plain `vmmap`
suspends the target for ~2 s, which is exactly the daemon's stall threshold
(8 ticks of 250 ms) and would cause the reopens it is counting.

Usage:
    tools/reopen-leak.py [--pid N] [--log PATH] [--every SECS]

Then unplug/replug the A4C or the ES-9 as you like, leaving ~20 s between
moves so each settles on its own row; Ctrl-C prints the summary.
"""

import argparse
import re
import signal
import subprocess
import sys
import time

MB = 1024 * 1024
QUIET = 15.0  # seconds of no movement before an event counts as settled
STEP = 5.0    # seconds between samples while an event settles; a sample takes ~4.5 s


def find_pid():
    out = subprocess.run(["pgrep", "-f", r"itajara loop"], capture_output=True, text=True).stdout.split()
    if len(out) != 1:
        sys.exit(f"expected one `itajara loop` process, found {out or 'none'}; pass --pid")
    return int(out[0])


def find_log(pid):
    """The daemon's stdout, which is where `supervise` writes its reopen lines."""
    out = subprocess.run(["lsof", "-p", str(pid), "-a", "-d", "1", "-Fn"], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if line.startswith("n/"):
            return line[1:]
    sys.exit("could not find the daemon's stdout file; pass --log")


def usb_devices():
    """Names of the USB devices attached now, as ioreg lists them."""
    out = subprocess.run(["ioreg", "-p", "IOUSB", "-w0"], capture_output=True, text=True).stdout
    return {m.group(1) for m in re.finditer(r"\+-o (.+?)@[0-9a-fA-F]+\s+<class IOUSBHostDevice", out)}


def sample(pid):
    """(live blocks, allocated bytes) in CoreAudio's caulk::audio_buffer_resource zone."""
    out = subprocess.run(["vmmap", "-summary", "-forkCorpse", str(pid)], capture_output=True, text=True).stdout
    # zone  vsize  resident  dirty  swapped  COUNT  ALLOCATED  frag  %frag  regions
    m = re.search(r"^caulk::audio_buffer_resource\S*\s+\S+\s+\S+\s+\S+\s+\S+\s+(\d+)\s+([\d.]+)([KMG])", out, re.M)
    if not m:
        sys.exit(f"vmmap found no caulk zone (is pid {pid} still running?)")
    return int(m.group(1)), float(m.group(2)) * {"K": 1 / 1024, "M": 1, "G": 1024}[m.group(3)] * MB


def mb(n):
    return f"{n / MB:+9.1f} MB"


def delta(a, b):
    return f"{b[0] - a[0]:+8d} blocks ({mb(b[1] - a[1]).strip()})"


def stamp():
    return time.strftime("%H:%M:%S")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--pid", type=int)
    ap.add_argument("--log")
    ap.add_argument("--every", type=float, default=15.0, help="idle sampling interval (default 15 s)")
    args = ap.parse_args()
    sys.stdout.reconfigure(line_buffering=True)  # readable when piped to a file or tee
    signal.signal(signal.SIGTERM, signal.default_int_handler)  # `kill` summarises like Ctrl-C

    pid = args.pid or find_pid()
    log_path = args.log or find_log(pid)
    log = open(log_path, errors="replace")
    log.seek(0, 2)  # only what happens from now on
    present = usb_devices()

    t0 = time.time()
    first = sample(pid)
    print(f"pid {pid}, log {log_path}")
    print(f"USB now: {', '.join(sorted(present)) or '(none)'}")
    print(f"{stamp()}  start     caulk {first[0]} blocks, {first[1] / MB:.1f} MB allocated")
    print("Unplug and replug when ready, ~20 s apart; Ctrl-C for the summary.\n")

    rows = []        # [label, before, after] for each settled event
    open_ev = None   # {"labels", "before", "moved_at"} while an event is settling
    last = first
    last_sample = time.time()
    idle = [0, 0]    # blocks and bytes that moved with no event open

    def begin(label):
        nonlocal open_ev, last, last_sample
        print(f"{stamp()}  {label}")
        if open_ev is not None:
            open_ev["labels"].append(label)
            return
        s = sample(pid)
        idle[0] += s[0] - last[0]
        idle[1] += s[1] - last[1]
        open_ev = {"labels": [label], "before": s, "moved_at": time.time()}
        last, last_sample = s, time.time()

    try:
        while True:
            now_present = usb_devices()
            for name in sorted(present - now_present):
                begin(f"UNPLUGGED {name}")
            for name in sorted(now_present - present):
                begin(f"PLUGGED   {name}")
            present = now_present

            for line in log.readlines():
                if "reopening" in line or "is back" in line:
                    begin(f"daemon:   {line.strip()}")

            now = time.time()
            if open_ev is not None and now - last_sample >= STEP:
                s = sample(pid)
                if s[0] != last[0]:
                    open_ev["moved_at"] = now
                print(f"{stamp()}    {delta(open_ev['before'], s)} since the event")
                last, last_sample = s, now
                if now - open_ev["moved_at"] >= QUIET:
                    label = " + ".join(open_ev["labels"])
                    rows.append([label, open_ev["before"], s])
                    print(f"{stamp()}  SETTLED   {label}: {delta(open_ev['before'], s)}\n")
                    open_ev = None
            elif open_ev is None and now - last_sample >= args.every:
                s = sample(pid)
                if s[0] != last[0]:
                    print(f"{stamp()}  idle      {delta(last, s)} with no event seen")
                idle[0] += s[0] - last[0]
                idle[1] += s[1] - last[1]
                last, last_sample = s, now
            time.sleep(0.5)
    except KeyboardInterrupt:
        pass

    end = sample(pid)
    print("\n" + "=" * 64)
    print(f"{(time.time() - t0) / 60:.1f} min, {len(rows)} settled event(s)")
    print(f"overall            {delta(first, end)}")
    print(f"with no event open {idle[0]:+8d} blocks ({mb(idle[1]).strip()})")
    if open_ev is not None:
        print(f"(still settling at Ctrl-C: {' + '.join(open_ev['labels'])}, {delta(open_ev['before'], end)} so far)")
    for label, before, after in rows:
        print(f"  {delta(before, after)}   {label}")


if __name__ == "__main__":
    main()
