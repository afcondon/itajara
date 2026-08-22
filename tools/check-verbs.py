#!/usr/bin/env python3
"""Does the app spell the daemon's language?

`Data.Looper.Verb.render` is the only place a command becomes text, and the
tests in test/Main.purs pin every spelling — but they pin it against constants
a human typed while reading engine.rs. That catches an accidental edit to
`render`; it does not catch the daemon changing underneath, because the oracle
and the thing under test are both on this side of the wire.

This is the other half: read BOTH sides from source and compare. The daemon is
in this repo, so the claim "we speak what it understands" is checkable rather
than merely asserted.

It earned its keep on the first run. A hand-read of engine.rs had concluded
that `t` (claim-the-past) was unimplemented, because `grep '"t"'` finds nothing
-- its arm is a char guard, `l if l.starts_with('t')`, not a string match. That
went into a commit message and a code comment as fact. This script disagreed
immediately.

Run: make check-verbs   (or python3 tools/check-verbs.py)
Exit 0 if every verb we can send has somewhere to land.
"""

import re
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
VERB_PURS = ROOT / "src/Data/Looper/Verb.purs"
ENGINE_RS = ROOT / "itajara/src/engine.rs"


def ours():
    """Every literal spelling in `render`, whatever shape it is written in."""
    src = VERB_PURS.read_text()
    try:
        body = src.split("render = case _ of")[1].split("\n-- |")[0]
    except IndexError:
        sys.exit("check-verbs: could not find `render = case _ of` in Verb.purs")
    words = set()
    # bare and numeric: `Record -> "r"`, `Rate n -> "sp" <> show n`
    words |= set(re.findall(r'->\s*"([a-z]+)"', body))
    # flags: `Sounding on -> flag "h" on`
    words |= set(re.findall(r'flag\s+"([a-z]+)"', body))
    return words


def theirs():
    """Every verb `dispatch` can land, from its arms and prefix guards.

    Scoped to the body of `dispatch` — taking the rest of the file would sweep
    in unrelated `match` arms from every function after it and make this agree
    with anything.
    """
    src = ENGINE_RS.read_text()
    start = src.index("pub fn dispatch(")
    # The next top-level `fn`/`pub fn` after dispatch ends its body.
    end = re.search(r"\n(?:pub )?fn ", src[start + 1 :])
    body = src[start : start + 1 + end.start()] if end else src[start:]

    words = set()
    # string arms, including alternations: `"g" | "g1" | "g0" =>`
    for arm in re.finditer(r'^\s+"[a-z0-9]+"(?:\s*\|\s*"[a-z0-9]+")*\s*=>', body, re.M):
        words |= set(re.findall(r'"([a-z0-9]+)"', arm.group(0)))
    # prefix guards, both spellings: starts_with("sp") and starts_with('t')
    words |= set(re.findall(r"""starts_with\(["']([a-z]+)["']\)""", body))
    return words


def main():
    us, them = ours(), theirs()

    # A flag's base word covers its 0/1 forms; the daemon lists all three.
    landable = set(them)
    for w in them:
        if re.fullmatch(r"[a-z]+[01]", w):
            landable.add(w[:-1])

    unlandable = sorted(w for w in us if w not in landable)

    print(f"  app sends   : {' '.join(sorted(us))}")
    print(f"  daemon takes: {' '.join(sorted(them))}")
    print()

    if unlandable:
        print("FAIL - the app can send verbs the daemon has no arm for:")
        for w in unlandable:
            print(f"         {w!r} — no match arm and no prefix guard in dispatch")
        return 1

    print(f"PASS - all {len(us)} verbs the app can send have an arm in dispatch")

    # Not a failure: the daemon is allowed a larger vocabulary than we drive.
    # Reported because it is the list of things the surface could grow into.
    spare = sorted(w for w in them if w not in us and not re.fullmatch(r"[a-z]+[01]", w))
    if spare:
        print(f"       (daemon also understands, unused here: {' '.join(spare)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
