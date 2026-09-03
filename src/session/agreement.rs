//! The agreement state: which sides have agreed, and when the offer expires.
//!
//! This module owns `AGREE` / `REJECT` and the 120-second timeout. The game
//! task waits for both agreements; on `REJECT` from either side, or on the
//! agreement timeout expiring, both sessions return to `Waiting` and the
//! pairing is discarded — neither engine is penalized and neither loses its
//! place in the pool.
//!
//! What is here is the state of one offered pairing. Sending `START` and
//! `REJECT`, returning the two sessions to the pool and noticing that the
//! timeout has expired belong to `pairing.rs`.
//!
//! Only `AGREE` is a transition: `REJECT` and expiry discard the pairing
//! whatever has been recorded, so neither is a method here. What the state
//! contributes to them is [`Agreement::silent`], which is what an expiry log
//! names.
//!
//! The echoed `<GameID>` is not consulted. The specification writes the
//! argument as optional — `AGREE[<GameID>]` — and shogi-server's
//! `AgreeCommand` and `RejectCommand` accept and ignore it.

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
/// The comparison is strictly greater — `prepared + 120 < now` is
/// `elapsed > 120` — so a pairing that has waited for precisely this long has
/// not yet expired.
///
/// The value is configurable through `[csa].agreement_timeout_seconds`, and
/// this is the default. Nothing here asks what time it is.
pub const AGREEMENT_TIMEOUT: Duration = Duration::from_secs(120);

/// What one `AGREE` means to the pairing.
///
/// [`Duplicate`](Agreed::Duplicate) is separated from
/// [`Pending`](Agreed::Pending) even though both leave the state alone,
/// because collapsing them would make a client repeating itself
/// indistinguishable from ordinary progress in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agreed {
    /// Recorded; the other side is still owed an `AGREE`.
    Pending,

    /// Both sides have now agreed: `START:<Game_ID>` goes to both, and the
    /// clock starts then — the specification (v1.2.1 section 3) has `START`
    /// simultaneously declare the start of play and start the first player's
    /// clock.
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
    /// The only stateful event in this module. A repeat from a side already
    /// recorded is [`Duplicate`](Agreed::Duplicate) and leaves the state
    /// untouched — in particular it does not consume the other side's
    /// agreement, so the pairing still starts when that side eventually
    /// agrees.
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
    /// What the expiry log names. Empty when both agreed, which is a state the
    /// timeout cannot be reached from.
    pub fn silent(&self) -> Vec<Color> {
        [Color::Black, Color::White]
            .into_iter()
            .filter(|&side| !self.agreed[slot(side)])
            .collect()
    }
}

/// Whether a pairing that has waited `elapsed` has outlived `limit`.
///
/// Strictly greater, as shogi-server's
/// `@prepared_time + WAITING_EXPIRATION < Time.now` reads: at exactly the
/// limit the pairing still stands.
///
/// The elapsed time is the caller's to measure. Checking late only delays
/// cleanup and cannot start a game that should not have started, so this is
/// never a deadline anything has to meet.
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

/// Index into [`Agreement::agreed`]: `[black, white]`.
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
        // The other order, because both having agreed must not depend on which
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
