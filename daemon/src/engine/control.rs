//! The console: stdin, read a line at a time and put on the lane.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1). The
//! closer that lived beside it — the thread that ended a take of a known
//! length at its frame — is the lane's tick since step 7 (`lane.rs`).

use std::io::BufRead;

use super::lane::{Caller, Lane};
use super::shared::Shared;

/// Returns true only if the user actually asked to quit.
///
/// EOF is not a quit. Run headless — from a launcher, or with output
/// redirected — and `lines()` returns immediately, which must not be allowed to
/// take the audio engine and the socket down with it.
pub(crate) fn control_loop(sh: &Shared, lane: &Lane) -> bool {
    println!("Commands:  r = record/overdub toggle   x = multiply   t [secs] = take");
    println!("           s [n] = spread one in n   o = move it one slot   d = dense again");
    println!("           u = undo a layer   z = forget the length   c = both");
    println!("           w [name] = save the take (one file per layer + manifest)");
    println!("           g = follow the grid (the first loop's cycle) / free");
    println!(
        "           a leading digit picks the loop: 3r records loop 3, 3s2 spreads it,\n\
         \x20          a bare 3 selects it. {} loops, 0 to {}.",
        sh.n_loops,
        sh.n_loops - 1
    );
    println!("           k = click   m = input monitoring");
    println!("           l = levels   p = status + waveforms   q = quit\n");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return false };
        if line.trim() == "q" {
            return true;
        }
        // The lane says the ack, on the console's behalf. A closed lane is
        // the daemon on its way out.
        if !lane.send(Caller::Console, line) {
            return false;
        }
    }
    false
}
