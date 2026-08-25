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


def dispatch_body():
    """`dispatch`'s body with its comments blanked out.

    **Blanked rather than removed**, so every byte keeps its offset and the arms
    stay in the order the file has them.

    Comments have to go because this file explains itself at length, and the
    explanations quote the code. The first version of the order check reported
    `tone` as shadowed by `t` *after* the shadowing had been fixed — it was
    reading the comment that describes the fix, which quotes `starts_with('t')`
    three lines above the arm it is warning about. A checker that reads prose as
    code is its own kind of wrong answer.
    """
    src = ENGINE_RS.read_text()
    start = src.index("pub fn dispatch(")
    end = re.search(r"\n(?:pub )?fn ", src[start + 1 :])
    body = src[start : start + 1 + end.start()] if end else src[start:]
    return re.sub(r"//[^\n]*", lambda m: " " * len(m.group(0)), body)


def arms_in_order():
    """Every arm `dispatch` can land, in the order it tries them.

    Order is the whole point. A `match` takes the first arm that matches, so a
    char guard like `l if l.starts_with('t')` shadows every later arm whose word
    begins with a t — and `theirs()` above, which works with a set, cannot see
    it.
    """
    body = dispatch_body()
    arms = []
    for m in re.finditer(r'^\s+"[a-z0-9]+"(?:\s*\|\s*"[a-z0-9]+")*\s*=>', body, re.M):
        for w in re.findall(r'"([a-z0-9]+)"', m.group(0)):
            arms.append((m.start(), "exact", w))
    for m in re.finditer(r"""starts_with\(["']([a-z]+)["']\)""", body):
        arms.append((m.start(), "prefix", m.group(1)))
    arms.sort(key=lambda a: a[0])
    return arms


def lands_on(command, arms):
    """The arm `command` actually reaches, or None."""
    for _pos, kind, word in arms:
        if kind == "exact" and command == word:
            return word
        if kind == "prefix" and command.startswith(word):
            return word
    return None


def shadowed(us, arms):
    """App verbs that reach an arm other than their own.

    **The bug this exists for.** `tone3000` has an arm, is spelled correctly and
    never arrives: `t` is matched as a char and gets there first, so it was read
    as "claim the last 3000 seconds" and answered with a refusal about cycles.
    Nothing failed. The tone simply did not change.
    """
    out = []
    for w in sorted(us):
        # The bare word, and the word with an argument — a prefix guard only
        # shadows the argued form of some verbs, and the bare form of others.
        for probe in (w, w + "1"):
            hit = lands_on(probe, arms)
            if hit is not None and hit != w and not probe == hit:
                # `g1` legitimately lands on the `g1` arm; only a *different*
                # word winning is a shadow.
                if not (hit.startswith(w) or w.startswith(hit) and hit == w):
                    out.append((probe, hit))
                elif hit != w and not hit.startswith(w):
                    out.append((probe, hit))
        # Deduplicate per verb: one report is enough.
    seen, uniq = set(), []
    for probe, hit in out:
        if probe.rstrip("1") in seen:
            continue
        seen.add(probe.rstrip("1"))
        uniq.append((probe, hit))
    return uniq


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

    arms = arms_in_order()
    stolen = shadowed(us, arms)
    if stolen:
        print("FAIL - the app can send verbs that reach the wrong arm:")
        for probe, hit in stolen:
            print(f"         {probe!r} lands on {hit!r}, which is matched earlier")
        print("         (a `match` takes the first arm that fits; move yours above it)")
        return 1

    print(f"PASS - all {len(us)} verbs the app can send have an arm in dispatch")
    print(f"       and reach it: no earlier arm shadows one of them")

    # Not a failure: the daemon is allowed a larger vocabulary than we drive.
    # Reported because it is the list of things the surface could grow into.
    spare = sorted(w for w in them if w not in us and not re.fullmatch(r"[a-z]+[01]", w))
    if spare:
        print(f"       (daemon also understands, unused here: {' '.join(spare)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
