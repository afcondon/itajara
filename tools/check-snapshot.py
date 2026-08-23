#!/usr/bin/env python3
"""Hold the snapshot's PureScript types to what the daemon actually sends.

`Foreign.LooperSocket` declares three record types — `LooperState`, `LoopState`,
`LayerShape` — and `latestImpl` coerces the parsed JSON straight into them with
NOTHING checking. A field the daemon does not send is therefore not a decode
error; it is `undefined`, and it detonates at the first place PureScript touches
it.

That is not hypothetical. `ws.rs` had two serialisers for one `LayerShape`: the
per-loop one sent six fields, the top-level one sent three. The app compares
successive snapshots to decide whether to redraw, that comparison reaches `env`,
PureScript's array equality opens with `xs.length`, and so a missing field threw
a TypeError ten times a second. The display froze while the socket, the
commands and the audio all stayed perfectly healthy — the worst shape a fault
can have, because every instinct says look at the transport (2026-08-23).

The check is deliberately made against a LIVE daemon rather than by parsing the
`format!` strings out of `ws.rs`. Parsing Rust string literals would be a second
model of the wire that can itself drift; a running daemon IS the wire. This is
the same reasoning as `check-verbs.py` reading both sources instead of trusting
either — an oracle that shares an assumption with the thing it checks passes on
the day that assumption breaks.

    tools/check-snapshot.py            # against ws://127.0.0.1:23028
    tools/check-snapshot.py --url ws://127.0.0.1:3028

Exit 0 if every declared field is sent, 1 otherwise.

SAFETY: `LayerShape` can only be checked when a layer exists, so this records
about a second on loop 0 and clears it. It refuses to run if loop 0 already has
layers or anything is recording — the same guard `conformance.py` uses, and for
the same reason: a take you had just played was destroyed once already.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PURS = ROOT / "src" / "Foreign" / "LooperSocket.purs"
DEFAULT_URL = "ws://127.0.0.1:23028"

# One entry per PLACE the type appears on the wire, which is not the same as one
# entry per type — and getting that wrong made the first version of this tool
# useless. `LayerShape` describes two objects: the selected loop's shapes
# repeated at the top level, and each loop's own. Those were serialised by two
# different pieces of Rust, which is the entire bug this exists to catch, and a
# check that looked only at the per-loop copy passed happily while the top-level
# one was missing three fields. Verified by breaking the daemon on purpose: with
# one location the check said OK, with both it fails.
#
# So: every location, named, and a type that appears in N places is checked N
# times. If a new array of these ever appears in the snapshot, add it here.
LOCATIONS = [
    ("LooperState", "top level", lambda s: s),
    ("LayerShape", "top-level shapes[0]", lambda s: s["shapes"][0]),
    ("LoopState", "loops[0]", lambda s: s["loops"][0]),
    ("LayerShape", "loops[0].shapes[0]", lambda s: s["loops"][0]["shapes"][0]),
]


def declared_fields(src: str, tname: str):
    """The field names of `type <tname> = { ... }`, doc comments stripped."""
    m = re.search(r"^type " + tname + r" =\n(.*?)^\s*\}\s*$", src, re.S | re.M)
    if not m:
        sys.exit(f"check-snapshot: no `type {tname}` in {PURS}")
    body = re.sub(r"^\s*--.*$", "", m.group(1), flags=re.M)
    return sorted(set(re.findall(r"^\s*[{,]\s*([a-zA-Z][A-Za-z0-9_']*)\s*::", body, re.M)))


# Node drives the socket: it has a WebSocket client built in, and this way the
# check has no Python dependency to install.
DRIVER = r"""
// Via the environment, not argv: `node -e` does not shift argv the way a script
// file does, and getting that wrong fails as "Invalid URL" rather than as
// anything that names the real problem.
const url = process.env.ITAJARA_URL;
const ws = new WebSocket(url);
let last = null;
ws.onerror = () => { console.error("cannot reach " + url); process.exit(2); };
ws.onmessage = (e) => { last = JSON.parse(e.data); };
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
ws.onopen = async () => {
  await wait(300);
  if (!last) { console.error("connected but no snapshot"); process.exit(2); }
  // Refuse to touch a loop that has something in it.
  const l0 = last.loops[0];
  if (l0.layers > 0) { console.error("GUARD: loop 0 has " + l0.layers + " layer(s) — refusing"); process.exit(3); }
  if (last.loops.some((l) => l.recording || l.armed)) { console.error("GUARD: something is recording — refusing"); process.exit(3); }
  if (!last.audioAlive) { console.error("GUARD: audio is not running — refusing"); process.exit(3); }
  ws.send("0r@0"); await wait(1000);   // record
  ws.send("0r@0"); await wait(700);    // close the layer
  const out = last;
  ws.send("0c@0"); await wait(400);    // put it back as we found it
  if (last.loops[0].layers !== 0) console.error("WARNING: loop 0 not empty after clear");
  console.log(JSON.stringify(out));
  process.exit(0);
};
setTimeout(() => { console.error("timed out"); process.exit(2); }, 15000);
"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=DEFAULT_URL, help=f"daemon socket (default {DEFAULT_URL})")
    args = ap.parse_args()

    import os
    r = subprocess.run(
        ["node", "--input-type=module", "-e", DRIVER],
        capture_output=True, text=True,
        env={**os.environ, "ITAJARA_URL": args.url},
    )
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
        return 2 if r.returncode == 2 else 3

    snap = json.loads(r.stdout)
    src = PURS.read_text()

    failed = 0
    for tname, where, pick in LOCATIONS:
        declared = declared_fields(src, tname)
        try:
            on_wire = sorted(pick(snap).keys())
        except (KeyError, IndexError):
            print(f"{tname} @ {where}: could not find the matching object in the snapshot")
            failed += 1
            continue

        # Declared-but-not-sent is the dangerous direction: it is `undefined` at
        # runtime, and PureScript's types say it cannot be.
        missing = [f for f in declared if f not in on_wire]
        # Sent-but-not-declared is merely unread — worth printing, not failing.
        extra = [f for f in on_wire if f not in declared]

        print(f"{tname} @ {where}: {len(declared)} declared, {len(on_wire)} sent — "
              f"{'OK' if not missing else 'MISSING FIELDS'}")
        if missing:
            print(f"    declared but NOT SENT (undefined at runtime): {', '.join(missing)}")
            failed += 1
        if extra:
            print(f"    sent but not declared (unread, harmless):     {', '.join(extra)}")

    print()
    if failed:
        print(f"FAIL — {failed} type(s) claim fields the daemon does not send.")
        print("Either the daemon should send them (ws.rs) or the type should stop")
        print("claiming them (LooperSocket.purs). Both sides describe one wire.")
        return 1
    print("OK — every declared field is on the wire.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
