//! The vocabulary as data, and the one place a command is read into a word.
//!
//! Added 2026-09-06 (REVIEW-daemon-debt step 2). Until then `dispatch`
//! matched the text after the loop prefix by `starts_with`, in file order,
//! and the order decided what a command meant: `tone3000` was read as `t`
//! (claim the past) until `tone` was moved above it, `sp0.5` was a sparse
//! multiply until `sp` was moved above `s`, and `exl` had to sit above `ex`
//! or `exlriff` exported the set as "lriff". Three bugs from one cause, and
//! a checker that could only ever catch the shapes of it someone had already
//! thought of.
//!
//! The grammar was regular all along:
//!
//! ```text
//!   [loop digits] word [arg] [@late]
//! ```
//!
//! `dispatch` still takes the loop digits and the `@late` off the ends, as
//! it always did. What is left comes here, and the rule is in three parts:
//!
//! 1. The **word** is the leading run of ASCII letters (a `!` may open it —
//!    `!lose` is the console's fault injection and no part of the wire), and
//!    the **argument** is whatever follows, trimmed. `sp0.5` is `sp` + `0.5`;
//!    `tone3000` is `tone` + `3000`; `lw2:1000:625000` is `lw` + the rest.
//! 2. If that word is in `VERBS`, that is the command — whole word, no
//!    ordering. `size13` is `size`, which is not a word, and is refused as
//!    such rather than multiplied.
//! 3. Only if it is not: the longest `Name` verb (a verb whose argument is
//!    free text — `ex`, `exl`, `w`) that begins the letters run is the
//!    command and everything after it is the name. `exlriff` is `exl` +
//!    `riff` because `exl` is longer than `ex`, and `wriff` is `w` + `riff`.
//!    Free text is the only reason a word can run into its argument, so
//!    this is the only place a prefix is ever consulted.
//!
//! What the argument *means* is still each arm's business — they parse
//! their own numbers and say their own refusals, as before. The kind here
//! does two small things beyond documenting: it names the `Name` verbs for
//! rule 3, and it says whether a word may carry an argument at all (a
//! `None` verb takes nothing, a `Flag` takes `0`, `1` or nothing), so that
//! `x1` and `g5` stay the unknown commands they were when `x` and `g` were
//! exact arms.
//!
//! Kept as a `const` slice on purpose: step 5 maps these words onto Glassbox
//! events, and `tools/check-verbs.py` reads the words out of this file by
//! path. Every word here has an arm in `dispatch` and every arm has a word
//! here — `the_table_and_the_match_agree` below holds that from inside, the
//! script from outside.

/// What a verb's argument is, for the reader and for the two rules above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arg {
    /// Nothing follows the word: `r`, `x`, `go`.
    None,
    /// Optionally `0` or `1`; bare means toggle (or on, for `mono`).
    Flag,
    /// A real number: seconds, hertz, decibels, a probability.
    Number,
    /// A whole number: frames, a count, a slot, a MIDI-range pan.
    Int,
    /// A shape of its own that the arm parses: `ly10`, `lw2:1000:625000`,
    /// `cp0l2`.
    Text,
    /// Free text — a take or set name. The only kind a word can run into.
    Name,
}

/// One word of the vocabulary.
pub(crate) struct Verb {
    pub word: &'static str,
    pub arg: Arg,
}

/// Every word `dispatch` answers to. One line per verb, so the table reads
/// as data from Rust and from Python alike.
pub(crate) const VERBS: &[Verb] = &[
    // The edit: window, rotation, and the picture of the loop.
    Verb { word: "win", arg: Arg::None },
    Verb { word: "in", arg: Arg::Int },
    Verb { word: "out", arg: Arg::Int },
    Verb { word: "rot", arg: Arg::Int },
    Verb { word: "pk", arg: Arg::Int },
    // Record, multiply, fire, claim the past.
    Verb { word: "x", arg: Arg::None },
    Verb { word: "r", arg: Arg::None },
    Verb { word: "f", arg: Arg::None },
    Verb { word: "tone", arg: Arg::Number },
    Verb { word: "t", arg: Arg::Number },
    Verb { word: "src", arg: Arg::Int },
    Verb { word: "mono", arg: Arg::Flag },
    // Speed and the structural multiplies.
    Verb { word: "sp", arg: Arg::Number },
    Verb { word: "s", arg: Arg::Int },
    Verb { word: "g", arg: Arg::Flag },
    Verb { word: "o", arg: Arg::None },
    Verb { word: "bpm", arg: Arg::None },
    Verb { word: "go", arg: Arg::None },
    Verb { word: "play", arg: Arg::Flag },
    Verb { word: "d", arg: Arg::None },
    Verb { word: "z", arg: Arg::None },
    // Layers, and the next take's shape.
    Verb { word: "ly", arg: Arg::Text },
    Verb { word: "lw", arg: Arg::Text },
    Verb { word: "dp", arg: Arg::Int },
    Verb { word: "lq", arg: Arg::Int },
    Verb { word: "fix", arg: Arg::Number },
    Verb { word: "len", arg: Arg::Int },
    Verb { word: "ph", arg: Arg::Int },
    Verb { word: "cp", arg: Arg::Text },
    // To and from disk.
    Verb { word: "exl", arg: Arg::Name },
    Verb { word: "ex", arg: Arg::Name },
    Verb { word: "w", arg: Arg::Name },
    // Undo, redo, and the console's fault injection.
    Verb { word: "y", arg: Arg::None },
    Verb { word: "u", arg: Arg::None },
    Verb { word: "!lose", arg: Arg::None },
    // The tape.
    Verb { word: "rev", arg: Arg::Flag },
    Verb { word: "blank", arg: Arg::Number },
    Verb { word: "rvx", arg: Arg::Flag },
    Verb { word: "fb", arg: Arg::Number },
    Verb { word: "pend", arg: Arg::Flag },
    Verb { word: "one", arg: Arg::Flag },
    Verb { word: "lev", arg: Arg::Flag },
    // The resolutions.
    Verb { word: "dec", arg: Arg::Number },
    Verb { word: "xf", arg: Arg::Number },
    Verb { word: "ch", arg: Arg::Number },
    Verb { word: "arm", arg: Arg::Number },
    Verb { word: "vol", arg: Arg::Number },
    Verb { word: "pan", arg: Arg::Int },
    Verb { word: "h", arg: Arg::Flag },
    Verb { word: "c", arg: Arg::None },
    // Rig-wide, and the console's readouts.
    Verb { word: "k", arg: Arg::Flag },
    Verb { word: "m", arg: Arg::Flag },
    Verb { word: "l", arg: Arg::None },
    Verb { word: "p", arg: Arg::None },
];

impl Arg {
    /// Whether this kind of verb may be followed by `arg` at all. The arms
    /// judge the argument's *content*; this only keeps a bare word bare.
    pub(crate) fn admits(self, arg: &str) -> bool {
        match self {
            Arg::None => arg.is_empty(),
            Arg::Flag => matches!(arg, "" | "0" | "1"),
            _ => true,
        }
    }
}

/// The word in the table, if it is one.
pub(crate) fn lookup(word: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|v| v.word == word)
}

/// How far the word reaches into `rest`: a `!` may open it, then letters.
fn word_len(rest: &str) -> usize {
    let b = rest.as_bytes();
    let bang = usize::from(b.first() == Some(&b'!'));
    bang + b[bang..].iter().take_while(|c| c.is_ascii_alphabetic()).count()
}

/// The command in `rest` — the text after the loop prefix and before the
/// `@late` — as a verb and its argument, by the three rules above. `None`
/// is not a command.
pub(crate) fn tokenize(rest: &str) -> Option<(&'static Verb, &str)> {
    let (word, arg) = rest.split_at(word_len(rest));
    if let Some(v) = lookup(word) {
        return Some((v, arg.trim()));
    }
    // Not a word: a name may have run into its verb. Longest first, so
    // `exl…` is never read as `ex` + `l…`.
    VERBS
        .iter()
        .filter(|v| v.arg == Arg::Name && word.starts_with(v.word))
        .max_by_key(|v| v.word.len())
        .map(|v| (v, rest[v.word.len()..].trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_of(rest: &str) -> Option<(&'static str, &str)> {
        tokenize(rest).map(|(v, a)| (v.word, a))
    }

    /// The three live bugs the prefix guards had, each now the reading a
    /// reader would give it.
    #[test]
    fn a_word_is_matched_whole_and_a_prefix_decides_nothing() {
        assert_eq!(word_of("tone3000"), Some(("tone", "3000")));
        assert_eq!(word_of("t3000"), Some(("t", "3000")));
        assert_eq!(word_of("t"), Some(("t", "")));
        assert_eq!(word_of("sp0.5"), Some(("sp", "0.5")));
        assert_eq!(word_of("s4"), Some(("s", "4")));
        assert_eq!(word_of("s"), Some(("s", "")));
        assert_eq!(word_of("src2"), Some(("src", "2")));
        assert_eq!(word_of("cp0l2"), Some(("cp", "0l2")));
        assert_eq!(word_of("c"), Some(("c", "")));
        assert_eq!(word_of("lw2:1000:625000"), Some(("lw", "2:1000:625000")));
        assert_eq!(word_of("in-20"), Some(("in", "-20")));
        assert_eq!(word_of("win"), Some(("win", "")));
        assert_eq!(word_of("!lose"), Some(("!lose", "")));
        assert_eq!(word_of("pan 100"), Some(("pan", "100")), "the argument is trimmed");
    }

    /// Not a word is not a command — however many words it begins with.
    #[test]
    fn what_is_not_a_word_is_not_a_command() {
        assert_eq!(word_of("size13"), None, "was a sparse multiply by nothing");
        assert_eq!(word_of("tx"), None, "was an eight-second claim");
        assert_eq!(word_of("spread"), None);
        assert_eq!(word_of(""), None);
        assert_eq!(word_of("4"), None);
        assert_eq!(word_of("!"), None);
    }

    /// A name runs into its verb, and the longest name-verb wins.
    #[test]
    fn a_name_is_read_after_the_longest_name_verb() {
        assert_eq!(word_of("exlriff"), Some(("exl", "riff")));
        assert_eq!(word_of("exriff"), Some(("ex", "riff")));
        assert_eq!(word_of("exriff2"), Some(("ex", "riff2")));
        assert_eq!(word_of("wriff"), Some(("w", "riff")));
        assert_eq!(word_of("w2026"), Some(("w", "2026")));
        assert_eq!(word_of("ex"), Some(("ex", "")));
        assert_eq!(word_of("exl"), Some(("exl", "")));
        assert_eq!(word_of("w"), Some(("w", "")));
        // A name that spells a word is that word; there is no reading a
        // grammar this flat gives it otherwise.
        assert_eq!(word_of("win"), Some(("win", "")));
    }

    /// A bare word stays bare, and a flag is a flag.
    #[test]
    fn a_word_that_takes_nothing_admits_nothing() {
        let kind = |w: &str| lookup(w).unwrap().arg;
        assert!(kind("x").admits(""));
        assert!(!kind("x").admits("1"));
        assert!(!kind("win").admits("5"));
        assert!(kind("g").admits("") && kind("g").admits("0") && kind("g").admits("1"));
        assert!(!kind("g").admits("5"));
        assert!(kind("sp").admits("0.5"));
        assert!(kind("w").admits(""));
    }

    /// The table names each word once, and names exactly the words the
    /// `match` in `dispatch` has an arm for. Read from the source rather
    /// than by dispatching, because `play1` reaches for Link over UDP and a
    /// test must not start the rig's transport.
    #[test]
    fn the_table_and_the_match_agree() {
        let words: Vec<&str> = VERBS.iter().map(|v| v.word).collect();
        for (i, w) in words.iter().enumerate() {
            assert!(!words[..i].contains(w), "`{}` is in the table twice", w);
        }
        let src = include_str!("dispatch.rs");
        let mut arms: Vec<String> = Vec::new();
        for line in src.lines() {
            let line = line.trim_start();
            // An arm is `"word" => …` or `"a" | "b" => …`; the words that
            // matter are the ones made of letters (inner arms match `"0"`,
            // `"1"`, `""`).
            if !line.starts_with('"') || !line.contains("=>") {
                continue;
            }
            let head = line.split("=>").next().unwrap();
            for q in head.split('|') {
                let w = q.trim().trim_matches('"');
                if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphabetic() || c == '!') {
                    arms.push(w.to_string());
                }
            }
        }
        for w in &words {
            assert!(arms.iter().any(|a| a == w), "`{}` is in the table and has no arm", w);
        }
        for a in &arms {
            assert!(words.contains(&a.as_str()), "`{}` has an arm and is not in the table", a);
        }
    }
}
