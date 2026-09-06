#!/usr/bin/env python3
"""Does the app spell the daemon's language?

`Data.Looper.Verb.render` is the only place a command becomes text, and the
tests in test/Main.purs pin every spelling — but they pin it against constants
a human typed while reading the daemon. That catches an accidental edit to
`render`; it does not catch the daemon changing underneath, because the oracle
and the thing under test are both on this side of the wire.

This is the other half: read BOTH sides from source and compare. The daemon is
in this repo, so the claim "we speak what it understands" is checkable rather
than merely asserted.

It earned its keep on the first run. A hand-read of engine.rs had concluded
that `t` (claim-the-past) was unimplemented, because `grep '"t"'` finds nothing
-- its arm was a char guard, `l if l.starts_with('t')`, not a string match.
That went into a commit message and a code comment as fact. This script
disagreed immediately.

**Since 2026-09-06 there are two tables and no order.** The daemon's
vocabulary is `VERBS` in `daemon/src/engine/verb.rs` — one word per line, with
the kind of argument it takes — and `dispatch` matches the whole word that
`verb::tokenize` reads out of a command. The first version of this script
had to work out which arm a command *reached*, because a `match` takes the
first arm that fits and a prefix guard like `starts_with('t')` shadowed every
later arm beginning with a t: `tone3000` had an arm, was spelled right, and
never arrived. That cannot happen now — there is no prefix to shadow with —
so the shadow check is gone and what is left is two comparisons:

  (a) every spelling `render` can produce, tokenized by the daemon's own rule
      (reproduced below, short enough to keep in step), lands on a word in
      `VERBS`, both bare and with an argument — letters for the spellings
      `render` follows with a name, since only a name can run into its word;
  (b) every word in `VERBS` has an arm in `dispatch`, and every arm is a word
      in `VERBS` — the daemon's own unit test `the_table_and_the_match_agree`
      holds the same thing from inside.

Run: make check-verbs   (or python3 tools/check-verbs.py)
Exit 0 if every verb we can send has somewhere to land.
"""

import re
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
VERB_PURS = ROOT / "client/src/Data/Looper/Verb.purs"
VERB_RS = ROOT / "daemon/src/engine/verb.rs"
DISPATCH_RS = ROOT / "daemon/src/engine/dispatch.rs"


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
    # names: `SaveTake name -> "w" <> name` — the argument is free text and
    # runs straight into the word, so these are probed with letters.
    named = set(re.findall(r'"([a-z]+)"\s*<>\s*name', body))
    return words, named


def table():
    """`VERBS` from verb.rs: word -> kind of argument, in table order."""
    src = VERB_RS.read_text()
    try:
        body = src.split("const VERBS: &[Verb] = &[")[1].split("\n];")[0]
    except IndexError:
        sys.exit("check-verbs: could not find `const VERBS` in verb.rs")
    body = re.sub(r"//[^\n]*", "", body)
    rows = re.findall(r'Verb\s*\{\s*word:\s*"([^"]+)"\s*,\s*arg:\s*Arg::(\w+)\s*\}', body)
    if not rows:
        sys.exit("check-verbs: `VERBS` in verb.rs has no rows this script can read")
    return dict(rows)


def arms():
    """Every word `dispatch`'s `match` has an arm for.

    Scoped to the body of `dispatch` and with its comments blanked — the
    prose in that file quotes the code, and a checker that reads prose as
    code is its own kind of wrong answer. An arm is `"word" =>` or
    `"a" | "b" =>`; the inner arms that read a flag match `"0"`, `"1"` and
    `""`, which is why only words made of letters count.
    """
    src = DISPATCH_RS.read_text()
    start = src.index("pub fn dispatch(")
    end = re.search(r"\n(?:pub )?fn ", src[start + 1 :])
    body = src[start : start + 1 + end.start()] if end else src[start:]
    body = re.sub(r"//[^\n]*", lambda m: " " * len(m.group(0)), body)
    words = set()
    for m in re.finditer(r'^\s*("[^"]*"(?:\s*\|\s*"[^"]*")*)\s*=>', body, re.M):
        for w in re.findall(r'"([^"]*)"', m.group(1)):
            if re.fullmatch(r"!?[a-z]+", w):
                words.add(w)
    return words


# The daemon's tokenizer, kept in step with `verb::tokenize` by (a) above
# failing the day they differ: the word is the leading run of letters (a `!`
# may open it); if that is in the table it is the command; if not, the
# longest name-verb that begins it is, and the rest is the name.
def tokenize(rest, verbs):
    m = re.match(r"!?[A-Za-z]*", rest)
    word = m.group(0)
    if word in verbs:
        return word
    names = [w for w, kind in verbs.items() if kind == "Name" and word.startswith(w)]
    return max(names, key=len) if names else None


# An argument of each kind, for the "with an argument" half of (a).
SAMPLE = {"None": "", "Flag": "1", "Number": "0.5", "Int": "3", "Text": "10", "Name": "riff"}


def main():
    (us, named), verbs = ours(), table()
    them = set(verbs)

    print(f"  app sends   : {' '.join(sorted(us))}")
    print(f"  daemon takes: {' '.join(sorted(them))}")
    print()

    # (a) Every spelling lands on its own word, bare and argued.
    lost = []
    for w in sorted(us):
        bare = tokenize(w, verbs)
        if bare is None:
            lost.append((w, "is not a word in VERBS and no name-verb begins it"))
            continue
        if bare != w:
            lost.append((w, f"reads as {bare!r}"))
            continue
        probe = w + ("riff" if w in named else SAMPLE[verbs[w]])
        hit = tokenize(probe, verbs)
        if hit != w:
            lost.append((probe, f"reads as {hit!r}, not {w!r}"))
    if lost:
        print("FAIL - the app can send verbs the daemon does not read as themselves:")
        for probe, why in lost:
            print(f"         {probe!r} {why}")
        return 1

    # (b) The table and the match name the same words.
    have = arms()
    missing = sorted(them - have)
    extra = sorted(have - them)
    if missing or extra:
        print("FAIL - verb.rs and dispatch.rs disagree:")
        for w in missing:
            print(f"         {w!r} is in VERBS and has no arm in dispatch")
        for w in extra:
            print(f"         {w!r} has an arm in dispatch and is not in VERBS")
        return 1

    print(f"PASS - all {len(us)} verbs the app can send have an arm in dispatch")
    print(f"       and reach it: the match is on the whole word, so nothing shadows")

    # Not a failure: the daemon is allowed a larger vocabulary than we drive.
    # Reported because it is the list of things the surface could grow into.
    spare = sorted(w for w in them if w not in us)
    if spare:
        print(f"       (daemon also understands, unused here: {' '.join(spare)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
