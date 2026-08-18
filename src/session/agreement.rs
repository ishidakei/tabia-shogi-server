//! The agreement state: which sides have agreed, and when the offer expires.
//!
//! This module owns `AGREE` / `REJECT` and the 120-second timeout, and the
//! agreement rule fixes what those mean:
//!
//! > the game task waits for both agreements. On `REJECT` from either side, or
//! > on the agreement timeout expiring, both sessions return to `Waiting` and
//! > the pairing is discarded — neither engine is penalized and neither loses
//! > its place in the pool.
//!
//! What is here is the *state* of one offered pairing — pure, like [`clock`]'s
//! arithmetic, [`login`]'s decision, and [`matchmaker`]'s round. Sending
//! `START` and `REJECT`, returning the two sessions to the pool, and noticing
//! that time has passed are the connection and game wiring's, in later slices.
//! This module answers only what each event means to the pairing: start it,
//! keep waiting, or — for a repeat — log it and change nothing.
//!
//! **Only `AGREE` is a transition.** `REJECT` and expiry discard the pairing
//! whatever has been recorded, since P-3 conditions neither on how far the
//! agreement got, so neither is a method here. What the state contributes to
//! them is [`Agreement::silent`], the sides that have not agreed, which is what
//! P-3's expiry log names:
//!
//! > The agreement timeout is configured, defaults to 120 seconds, and its
//! > expiry is logged with the game ID and the silent side.
//!
//! **The echoed `<GameID>` is not consulted.** The specification writes the
//! argument as optional — `AGREE[<GameID>]` — and shogi-server's `AgreeCommand`
//! and `RejectCommand` accept and ignore it, so this server accepts `AGREE`
//! with or without a `<Game_ID>`. A handler may log a mismatch;
//! no transition depends on one, and there is nothing here for it to reach.
//!
//! [`clock`]: super::clock
//! [`login`]: super::login
//! [`matchmaker`]: super::matchmaker

use std::time::Duration;

use crate::game::Color;

/// How long an offered pairing waits for both agreements, by default.
///
/// This is shogi-server's `WAITING_EXPIRATION`, from `shogi_server/game.rb`:
///
/// ```ruby
/// WAITING_EXPIRATION = 120 # seconds
/// ...
/// @prepared_time + WAITING_EXPIRATION < Time.now
/// ```
///
/// The comparison is **strictly greater** — `prepared + 120 < now` is
/// `elapsed > 120` — so a pairing that has waited for precisely this long has
/// not yet expired. [`expired`] keeps that exactly.
///
/// P-3 makes the value configurable and this the default; the key arrives with
/// the config wiring, which is where P-3's "the agreement timeout is
/// configured" criterion lands. Nothing here asks what time it is: P-3 leaves
/// "whether expiry is checked lazily or on a precise timer" to the
/// implementation, and both callers measure their own elapsed time.
pub const AGREEMENT_TIMEOUT: Duration = Duration::from_secs(120);

/// What one `AGREE` means to the pairing.
///
/// [`Duplicate`](Agreed::Duplicate) is separated from
/// [`Pending`](Agreed::Pending) even though both leave the state alone, because
/// they are different events to a reader of the log: one side is owed an
/// agreement in the first case and nothing is owed by the sender in the second.
/// Collapsing them would make a client repeating itself indistinguishable from
/// ordinary progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agreed {
    /// Recorded; the other side is still owed an `AGREE`.
    Pending,

    /// Both sides have now agreed: `START:<Game_ID>` goes to both, and the
    /// clock starts then — the specification (v1.2.1 §3) has `START`
    /// simultaneously declare the start of play and start the first player's
    /// clock. Both are the wiring's; this is the value that authorizes them.
    Start,

    /// This side had already agreed. A command unexpected in its state is
    /// logged with the state and the command, and nothing else happens — the
    /// state does not advance — so the
    /// handler logs and sends nothing. That is tabia's deliberate divergence
    /// from shogi-server, which answers a second `AGREE` with
    /// `##[ERROR] you are in start_waiting status` and likewise changes no
    /// state.
    Duplicate,
}

/// The agreement state of one offered pairing.
///
/// Two booleans, keyed by side. The `Game_ID`, the two sessions, and the
/// summary that was sent are the caller's — this holds the one fact that
/// changes while the pairing waits, so that "are we ready to start" is answered
/// in a single place rather than recomputed beside each `AGREE`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Agreement {
    /// Indexed by [`slot`]: `[black, white]`.
    agreed: [bool; 2],
}

impl Agreement {
    /// A fresh pairing: neither side has agreed.
    pub const fn new() -> Self {
        Self { agreed: [false; 2] }
    }

    /// Records an `AGREE` from `side`, and says what it means.
    ///
    /// The only stateful event in this module. The first `AGREE` from a side is
    /// [`Pending`](Agreed::Pending), or [`Start`](Agreed::Start) when the other
    /// side has already agreed; the order the two arrive in does not matter,
    /// since the second one is what completes the pairing either way.
    ///
    /// A repeat from a side already recorded is [`Duplicate`](Agreed::Duplicate)
    /// and leaves the state untouched — in particular it does not consume the
    /// other side's agreement, so the pairing still starts when that side
    /// eventually agrees.
    ///
    /// The echoed `<GameID>` is not a parameter, per the module docs: no
    /// transition depends on it.
    pub fn agree(&mut self, side: Color) -> Agreed {
        let slot = &mut self.agreed[slot(side)];
        if *slot {
            return Agreed::Duplicate;
        }
        *slot = true;

        if self.agreed == [true; 2] {
            Agreed::Start
        } else {
            Agreed::Pending
        }
    }

    /// The sides that have not agreed, in `[black, white]` order.
    ///
    /// What P-3's expiry log names: both sides when neither answered, exactly
    /// the quiet one when a single side did, and empty when both did — a state
    /// the timeout cannot be reached from, since the second `AGREE` starts the
    /// game.
    ///
    /// A `Vec` rather than a borrowed iterator because every caller is a log
    /// line formatting at most two values, and an allocation on the expiry path
    /// costs nothing a pairing that already waited two minutes will notice.
    pub fn silent(&self) -> Vec<Color> {
        [Color::Black, Color::White]
            .into_iter()
            .filter(|&side| !self.agreed[slot(side)])
            .collect()
    }
}

/// Whether a pairing that has waited `elapsed` has outlived `limit`.
///
/// **Strictly greater**, as shogi-server's `@prepared_time + WAITING_EXPIRATION
/// < Time.now` reads: at exactly the limit the pairing still stands. `limit` is
/// a parameter rather than [`AGREEMENT_TIMEOUT`] read directly, because P-3
/// makes the timeout configured and the default is only what an unconfigured
/// instance uses.
///
/// The elapsed time is the caller's to measure, from whenever it sent the
/// summary. Lazy checking is explicitly allowed — P-3 notes it "only delays
/// cleanup and cannot start a game that should not have started" — so this is a
/// question a sweep may ask late, and never a deadline anything here has to
/// meet.
///
/// ```
/// # use std::time::Duration;
/// # use tabia_shogi_server::session::agreement::{AGREEMENT_TIMEOUT, expired};
/// assert!(!expired(AGREEMENT_TIMEOUT, AGREEMENT_TIMEOUT));
/// assert!(expired(Duration::from_secs(121), AGREEMENT_TIMEOUT));
/// ```
pub fn expired(elapsed: Duration, limit: Duration) -> bool {
    elapsed > limit
}

/// Index into [`Agreement::agreed`], so that "which slot is White's" is
/// answered in one place.
///
/// `Color`'s own index is private to [`game`](crate::game), and a second public
/// one is not this module's to add.
const fn slot(side: Color) -> usize {
    match side {
        Color::Black => 0,
        Color::White => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_timeout_is_shogi_servers_waiting_expiration() {
        assert_eq!(AGREEMENT_TIMEOUT, Duration::from_secs(120));
        assert_eq!(AGREEMENT_TIMEOUT.as_secs(), 120);
    }

    #[test]
    fn a_fresh_pairing_owes_both_sides() {
        let agreement = Agreement::new();

        assert_eq!(agreement.silent(), vec![Color::Black, Color::White]);
        assert_eq!(agreement, Agreement::default());
    }

    #[test]
    fn black_then_white_reaches_start() {
        let mut agreement = Agreement::new();

        assert_eq!(agreement.agree(Color::Black), Agreed::Pending);
        assert_eq!(agreement.agree(Color::White), Agreed::Start);
    }

    #[test]
    fn white_then_black_reaches_start() {
        // The other order, because "both have agreed" must not depend on which
        // side the summary reached first.
        let mut agreement = Agreement::new();

        assert_eq!(agreement.agree(Color::White), Agreed::Pending);
        assert_eq!(agreement.agree(Color::Black), Agreed::Start);
    }

    #[test]
    fn a_repeated_agree_is_a_duplicate_and_does_not_advance() {
        for first in [Color::Black, Color::White] {
            let mut agreement = Agreement::new();

            assert_eq!(agreement.agree(first), Agreed::Pending);
            assert_eq!(agreement.agree(first), Agreed::Duplicate);
            assert_eq!(agreement.agree(first), Agreed::Duplicate);

            // The duplicates neither started the game nor consumed the other
            // side's agreement.
            assert_eq!(agreement.silent(), vec![first.opponent()]);
            assert_eq!(agreement.agree(first.opponent()), Agreed::Start);
        }
    }

    #[test]
    fn an_agree_after_both_agreed_is_a_duplicate() {
        let mut agreement = Agreement::new();
        agreement.agree(Color::Black);
        agreement.agree(Color::White);

        assert_eq!(agreement.agree(Color::Black), Agreed::Duplicate);
        assert_eq!(agreement.agree(Color::White), Agreed::Duplicate);
    }

    #[test]
    fn silent_names_exactly_the_side_that_has_not_agreed() {
        for agreed in [Color::Black, Color::White] {
            let mut agreement = Agreement::new();
            agreement.agree(agreed);

            assert_eq!(agreement.silent(), vec![agreed.opponent()]);
        }
    }

    #[test]
    fn silent_is_empty_once_both_have_agreed() {
        let mut agreement = Agreement::new();
        agreement.agree(Color::White);
        agreement.agree(Color::Black);

        assert!(agreement.silent().is_empty());
    }

    #[test]
    fn a_pairing_at_exactly_the_limit_has_not_expired() {
        assert!(!expired(AGREEMENT_TIMEOUT, AGREEMENT_TIMEOUT));
        assert!(!expired(Duration::ZERO, AGREEMENT_TIMEOUT));
        assert!(!expired(Duration::from_secs(119), AGREEMENT_TIMEOUT));
        assert!(!expired(
            AGREEMENT_TIMEOUT - Duration::from_nanos(1),
            AGREEMENT_TIMEOUT
        ));
    }

    #[test]
    fn anything_past_the_limit_has_expired() {
        assert!(expired(
            AGREEMENT_TIMEOUT + Duration::from_nanos(1),
            AGREEMENT_TIMEOUT
        ));
        assert!(expired(Duration::from_secs(121), AGREEMENT_TIMEOUT));
        assert!(expired(Duration::from_secs(3600), AGREEMENT_TIMEOUT));
    }

    #[test]
    fn the_limit_is_the_callers_and_not_the_default() {
        // A configured timeout other than the default decides on its own terms.
        let limit = Duration::from_secs(5);

        assert!(!expired(limit, limit));
        assert!(expired(Duration::from_secs(6), limit));
        assert!(!expired(Duration::from_secs(60), Duration::from_secs(600)));
    }
}
