//! The state of one game: the history its setup seeds, the events that carry a
//! game to a termination, how each of them settles against a clock, and what a
//! finished game sends.
//!
//! This module owns the per-game state — the `Game` itself, and the snapshots
//! published from it. The *task* is the half that reads commands,
//! measures what elapsed between them, and writes lines; it arrives with the
//! runtime wiring, and takes [`Game::apply`]'s `charged` from
//! [`clock::charged_units`]. What is here is the state it will own, on the same
//! terms as the four session pieces before it: no tokio, no socket, and nothing
//! that asks what time it is.
//!
//! **The flag is decided here, not in the task.** A `Game` is already the single
//! ordering authority for P-4 — a move from the side not to move is refused
//! here — and P-5's timeout is a game rule rather than a transport fact. A task
//! that decided it as well would be a second authority on when a move counts,
//! and the two could disagree on the same input. What the task still owns is
//! *measurement*: what elapsed, and the deadline that makes a player who sends
//! nothing at all flag. [`Game::allowance`] is the number it arms that deadline
//! with, and [`Game::expired`] is where the measurement it wakes with is
//! judged — by the same predicate an arrival is judged by, so the two paths
//! cannot part company.
//!
//! **`csa` joins the layer here.** `game/` depends on nothing (invariant 1), so
//! a `game::Outcome` cannot name the `#RESIGN` it maps to; `csa` decides no
//! rules, so it takes a termination's reason and result as inputs. The layer
//! that may see both is this one, and [`termination_of`] is P-7's table written
//! once in it — the same reasoning [`session`](super) records for the two
//! `TimeUnit` mirrors.
//!
//! **Invariant 5 is why [`Game::new`] seeds the history rather than the task.**
//! Setup moves are game history: they belong in `moves`, and repetition (P-6)
//! and `Max_Moves` (P-2) count them. A game whose
//! first entry is the first *real* move is a game those two slices would
//! silently get wrong, so there is no moment at which a `Game` exists without
//! them.
//!
//! **Invariant 2 is why nothing here asks what the position is.** A start is a
//! [`StartSpec`] decoded once; hirate is one value of it, reached by a replay
//! that ran zero times, and no branch distinguishes that case.
//!
//! [`clock::charged_units`]: super::clock::charged_units

use crate::config::TimeConfig;
use crate::csa::{GameResult, Reason};
use crate::game::{
    Color, Illegal, IllegalSetup, Move, Outcome, Position, RepetitionState, StartSpec, apply_move,
};
use crate::game::{declaration, repetition};

use super::clock::{effective_setup, increment_units, setup_t_values, total_units, turn_allowance};

/// One entry of a game's history — a setup move or a played one.
///
/// **A typed [`Move`], where the design writes `csa: String`.** A deliberate
/// divergence: invariant 3 keeps CSA spellings at
/// the codec, and this layer reads parsed types only. That string is
/// the *record's* projection of this entry, rendered at C-7's edge from the move
/// and the position it was played in — which is also where the notation the
/// codec relayed is already available. Storing it here would put a second
/// spelling of one move in the layer least able to check the two agree.
///
/// Every field is a fact about the entry rather than an invariant between them,
/// so they are public, on [`csa::MoveEcho`]'s terms.
///
/// [`csa::MoveEcho`]: crate::csa::MoveEcho
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlayedMove {
    /// Which ply this is: **1-based across the whole history**, setup included,
    /// so that the *n*th entry of a game is the *n*th move of the record and of
    /// the `Max_Moves` count alike. Part 3 numbers nothing; this is the
    /// numbering that makes invariant 5 readable.
    pub ply: u32,

    /// The move itself.
    pub mv: Move,

    /// The value written on the wire, which equals the value deducted
    /// (invariant 4). For a setup move this is [`setup_t_values`]'s entry — the
    /// increment, or increment plus reduction on the reduced side's first move
    /// — and for a played move the charge the task measured.
    pub t: u32,

    /// Whether this entry came from the setup sequence rather than from a
    /// client.
    pub is_setup: bool,
}

/// The state of one game.
///
/// Part 3's `Game`, restricted to the fields the milestones so far have reached:
/// the identifier, the start as it was actually transmitted, the position play
/// began from and the position now, the history, the time settings, the two
/// clocks, and the outcome once there is one. Players, engine names, the two
/// category tags, and the timestamps arrive with the milestones that read them
/// (C-5, C-7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    id: String,
    start: StartSpec,
    initial: Position,
    position: Position,
    moves: Vec<PlayedMove>,
    /// The settings both clocks were seeded from, kept because settlement needs
    /// the increment and the flag needs the byoyomi on **every** event, for the
    /// life of the game. `TimeConfig` is `Copy + Eq`, so this costs one field
    /// and keeps both numbers derived from the one value the game was created
    /// with; a pair of pre-converted counts beside it would be the same fact
    /// written twice.
    time: TimeConfig,
    remaining: [u32; 2],
    /// Every position this game has held, counted (P-6). Seeded from the
    /// **transmitted** start's traversal in [`Game::new`], so a game never
    /// exists without it — the same reasoning invariant 5 gives for `moves`,
    /// and for the same failure: a repetition counted from the first real move
    /// is one the clients would be told about too late, or not at all.
    repetition: RepetitionState,
    /// `Max_Moves`: the absolute ply limit, **setup entries included** (P-2,
    /// PRD A2). `None` is the specification's "no restriction if omitted", and
    /// it is the only spelling of absence here — shogi-server's `@max_moves > 0`
    /// guard is how Ruby says the same thing, and a zero sentinel beside the
    /// `Option` would be one absence written twice.
    max_moves: Option<u32>,
    outcome: Option<Outcome>,
}

impl Game {
    /// A game about to start, from the pairing's start and time settings.
    ///
    /// **The substitution is applied once, here.** [`effective_setup`] decides
    /// whether the sequence transmitted is the authored one or the king shuttle,
    /// and what this stores is the result of that decision. The summary the task
    /// encodes, the history below, and the T-values all read the one stored
    /// value, so they cannot disagree about how many moves are on the wire.
    ///
    /// The stored start is then decoded — one path, whatever the entry was — and
    /// its setup moves become the first entries of the history, each carrying
    /// its [`setup_t_values`] entry. Both clocks start at the configured
    /// allowance and then **settle** each of those values, exactly as a played
    /// move settles: the value is written, so it is deducted (invariant 4), and
    /// the turn's increment is credited against it.
    ///
    /// **The repetition count begins here too**, at the transmitted start rather
    /// than at the first real move (P-6). The start's own position is one
    /// occurrence, and every position the setup sequence passes through is
    /// recorded exactly as a live move records — one traversal, so what the
    /// history holds and what the count saw cannot disagree. A dummy buoy
    /// therefore holds **two** occurrences of hirate before the first real move,
    /// which is a consequence of the rule and not a case anything branches on
    /// (invariant 2).
    ///
    /// The two cancel, which is the whole point of the convention. A symmetric
    /// setup under an increment therefore reaches `START` with both clocks at
    /// the full allowance (P-2's worked example), and the `T602` form leaves the
    /// reduced side at `total − reduction` with its opponent full — the numbers
    /// a client following `remaining + increment − T` computes for itself.
    ///
    /// **`max_moves` is taken, not computed** (P-2). It is the whole game's ply
    /// ceiling, the setup entries included, so the seeding below already spends
    /// part of it — which is exactly PRD A2's "a game whose opening sequence is
    /// *n* plies has `Max_Moves − n` plies left to play", with no arithmetic
    /// anywhere to get wrong. `None` is "no restriction if omitted".
    ///
    /// # Errors
    ///
    /// [`IllegalSetup`] if the transmitted sequence is not legal from hirate.
    /// O-1 rejects such an entry at load time, so reaching this at runtime means
    /// an entry got past the loader — but the game is refused rather than
    /// started from a position no replay produced.
    pub fn new(
        id: String,
        spec: &StartSpec,
        cfg: &TimeConfig,
        max_moves: Option<u32>,
    ) -> Result<Self, IllegalSetup> {
        let setup = effective_setup(spec, cfg);
        let start = match spec {
            StartSpec::Buoy { .. } => StartSpec::Buoy {
                setup: setup.to_vec(),
            },
            StartSpec::Board(position) => StartSpec::Board(position.clone()),
        };
        let traversal = start.traversal()?;
        let (transmitted, traversed) = traversal
            .split_first()
            .unwrap_or_else(|| unreachable!("a traversal begins at the transmitted start"));

        let mut repetition = RepetitionState::new();
        repetition.count_start(transmitted);
        for position in traversed {
            // The verdict is deliberately dropped. A setup sequence that repeats
            // a position four times is an entry O-1 refuses at load, and ending
            // a game that has not started — before `START`, with no client owed
            // a termination — would be an invented behavior rather than a
            // missing one. What matters here is that the positions are
            // *counted*.
            let _ = repetition.record(position);
        }

        let increment = increment_units(cfg);
        let mut moves = Vec::with_capacity(setup.len());
        let mut remaining = [total_units(cfg); 2];
        for (index, (&mv, t)) in setup
            .iter()
            .zip(setup_t_values(setup.len(), cfg))
            .enumerate()
        {
            let clock = &mut remaining[slot(setup_mover(index))];
            *clock = settled(*clock, increment, t);
            moves.push(PlayedMove {
                ply: next_ply(&moves),
                mv,
                t,
                is_setup: true,
            });
        }

        let initial = traversal
            .last()
            .unwrap_or_else(|| unreachable!("a traversal begins at the transmitted start"))
            .clone();

        Ok(Self {
            id,
            start,
            position: initial.clone(),
            initial,
            moves,
            time: *cfg,
            remaining,
            repetition,
            max_moves,
            outcome: None,
        })
    }

    /// The `Game_ID` this game is known by.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The start as it was transmitted — the effective one, with the
    /// substitution already applied.
    pub const fn start(&self) -> &StartSpec {
        &self.start
    }

    /// The position play began from: [`start`](Self::start) with its setup
    /// replayed.
    pub const fn initial(&self) -> &Position {
        &self.initial
    }

    /// The position now.
    pub const fn position(&self) -> &Position {
        &self.position
    }

    /// The side to move, which is the position's and never tracked beside it.
    pub const fn side_to_move(&self) -> Color {
        self.position.side_to_move()
    }

    /// The history, setup moves included (invariant 5).
    pub fn moves(&self) -> &[PlayedMove] {
        &self.moves
    }

    /// What `side` has left, as a count of `Time_Unit`s.
    ///
    /// Units rather than a [`Duration`](std::time::Duration): every value ever
    /// added to or taken from a clock is a written T-value, and those are unit
    /// counts. Converting once in [`clock`](super::clock) and never again is
    /// that module's contract, and holding the remainder in units is what keeps
    /// it to one conversion.
    pub const fn remaining(&self, side: Color) -> u32 {
        self.remaining[slot(side)]
    }

    /// How the game ended, or `None` while it is still being played.
    pub const fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }

    /// What `side` may spend on this turn, or `None` if nothing bounds it.
    ///
    /// [`clock::turn_allowance`] of what that side holds — remaining plus
    /// byoyomi plus increment. The runtime wiring arms its deadline with this,
    /// which is why it is public and why it is asked *before* anything has
    /// arrived: a player who never sends a line must flag too, and nothing about
    /// this number waits for one.
    ///
    /// [`clock::turn_allowance`]: super::clock::turn_allowance
    pub fn allowance(&self, side: Color) -> Option<u32> {
        turn_allowance(self.remaining(side), &self.time)
    }

    /// Applies a move from `side`, charged `charged` units.
    ///
    /// P-4's rules, in the order it states them, with P-5's flag inserted where
    /// shogi-server puts it (`Game#handle_one_move`: the wrong side first, then
    /// the timeout, then the arrived input). A legal move is appended to the
    /// history with the next ply, the mover's clock settles the charge, and the
    /// caller relays `<move>,T<charged>` — the value written is the value
    /// deducted (invariant 4), which is why one number is both.
    ///
    /// Validation is [`game::apply_move`] against the position, whatever that
    /// position is: correctness from a buoy start is the same code path as from
    /// hirate, with no branch that could be right in one case and wrong in the
    /// other.
    ///
    /// **A legal move can end the game** (P-6). The position it produced is
    /// recorded, and the fourth occurrence of a key ends the game where it
    /// stands: [`Outcome::Repetition`] for a plain repetition, and
    /// [`Outcome::PerpetualCheck`] against whichever side the streak rule names
    /// as the checker — which is *not* always the mover, since the side escaping
    /// a perpetual check is the one whose move closes the cycle. The move itself
    /// stays applied, recorded, settled, and returned: the game ended **with**
    /// it, not instead of it, which is also what lets P-7 echo it with the
    /// consumption time it was actually charged. A caller relays the move as it
    /// always did and then asks [`outcome`](Self::outcome).
    ///
    /// **`Max_Moves` is decided last, and only where nothing else decided**
    /// (P-2). shogi-server checks it after both repetition rules
    /// (`board.rb#handle_one_move`: oute_kaihimore → uchifuzume →
    /// oute_sennichite → sennichite → max_moves), so a move that both reaches
    /// the limit and completes a fourth occurrence ends `#SENNICHITE` or
    /// `#OUTE_SENNICHITE` rather than `#MAX_MOVES`. The predicate is
    /// [`reached_max_moves`](Self::reached_max_moves) over the whole history,
    /// setup entries included (invariant 5) — which is the entirety of what
    /// makes PRD A2's "the limit applies to the whole transmitted game" true.
    ///
    /// # Errors
    ///
    /// - [`Rejected::Finished`] — the game already ended. A caller bug; nothing
    ///   changes.
    /// - [`Rejected::NotToMove`] — a move from the side not to move. P-4 makes
    ///   this a protocol error rather than an illegal move, so **no state
    ///   changes at all**: not the position, not the history, and not the
    ///   clocks — in particular the sender is not charged for a move the game
    ///   never saw. This stays ahead of the flag, as it is in the reference: the
    ///   player whose clock is *not* running cannot cause it to fall, however
    ///   far over the allowance the other side is.
    /// - [`Rejected::Timeout`] — the charge reached the side to move's
    ///   allowance, so the flag fell before this move existed:
    ///   [`Outcome::Timeout`] against `side`. The move is **not recorded** and
    ///   **nothing is settled** — the reference never reaches `process_time` on
    ///   this path, and a clock movement no client was told of is invariant 4
    ///   broken. The unrecorded move is the same rule an illegal one follows,
    ///   for the same reason: it was never played on the board.
    /// - [`Rejected::Illegal`] — the move is illegal, and the game is over:
    ///   [`Outcome::IllegalMove`] against `side`, whom the termination scores
    ///   `#LOSE`. The charge is still deducted, because the termination echoes
    ///   the received move with it (P-7) and a written value is a deducted one.
    ///
    /// [`game::apply_move`]: crate::game::apply_move
    pub fn apply(&mut self, side: Color, mv: Move, charged: u32) -> Result<PlayedMove, Rejected> {
        if let Some(outcome) = self.outcome {
            return Err(Finished { outcome }.into());
        }

        let to_move = self.side_to_move();
        if side != to_move {
            return Err(Rejected::NotToMove { side, to_move });
        }

        if let Some(allowance) = self.flagged(side, charged) {
            self.outcome = Some(Outcome::Timeout { by: side });
            return Err(Rejected::Timeout {
                by: side,
                charged,
                allowance,
            });
        }

        match apply_move(&self.position, mv) {
            Ok(position) => {
                self.position = position;
                self.settle(side, charged);
                let played = PlayedMove {
                    ply: next_ply(&self.moves),
                    mv,
                    t: charged,
                    is_setup: false,
                };
                self.moves.push(played);

                // `repetition::Verdict`, spelled out: this module's own
                // `Verdict` is what a *finished* game sends, and the two would
                // read alike at a glance.
                self.outcome = match self.repetition.record(&self.position) {
                    repetition::Verdict::None => None,
                    repetition::Verdict::Draw => Some(Outcome::Repetition),
                    repetition::Verdict::PerpetualCheck { loser } => {
                        Some(Outcome::PerpetualCheck { by: loser })
                    }
                };

                if self.outcome.is_none() && self.reached_max_moves() {
                    self.outcome = Some(Outcome::MaxMoves);
                }

                Ok(played)
            }
            Err(illegal) => {
                self.outcome = Some(Outcome::IllegalMove { by: side });
                self.settle(side, charged);
                Err(Rejected::Illegal(illegal))
            }
        }
    }

    /// Records `%TORYO` from `side`, charged `charged` units.
    ///
    /// The game ends [`Outcome::Resignation`] against `side`, and the returned
    /// [`Verdict`] is what the task sends with it: the specification's own
    /// sequence, `%TORYO,T<charged>` then `#RESIGN` then the result. The echoed
    /// text is the client's line, relayed verbatim by the codec — invariant 3
    /// keeps that spelling out of here.
    ///
    /// The charge settles for the same reason [`apply`](Self::apply) settles one
    /// on an illegal move: it is written, so it is deducted.
    ///
    /// **A late `%TORYO` is a flag, not a resignation.** The reference decides
    /// the timeout before it reads what arrived (`Game#handle_one_move`), and a
    /// declaration is not exempt: if `side` is the side to move and `charged`
    /// reached its allowance, the outcome is [`Outcome::Timeout`] against it,
    /// nothing is settled, and the returned [`Verdict`] describes *that*
    /// termination. A caller renders what it is handed and is right either way;
    /// one that names the outcome itself must read
    /// [`outcome`](Self::outcome) instead.
    ///
    /// **No turn check otherwise.** P-4's protocol error is about a *move*
    /// arriving from the side not to move; nothing pins whether a client may
    /// resign while its opponent is thinking, and refusing it here would be
    /// inventing a rule that costs a resigning engine its termination. Nor can
    /// such a resignation flag: that side's clock is not running, so no
    /// allowance applies to it.
    ///
    /// # Errors
    ///
    /// [`Finished`] if the game already ended; nothing changes.
    pub fn resign(&mut self, side: Color, charged: u32) -> Result<Verdict, Finished> {
        if let Some(outcome) = self.outcome {
            return Err(Finished { outcome });
        }

        let outcome = match self.flagged(side, charged) {
            Some(_) => Outcome::Timeout { by: side },
            None => {
                self.settle(side, charged);
                Outcome::Resignation { by: side }
            }
        };
        self.outcome = Some(outcome);

        Ok(termination_of(outcome))
    }

    /// Records `%KACHI` from `side`, charged `charged` units.
    ///
    /// The declaration is adjudicated against **this game's position**, whatever
    /// start produced it, by [`declaration::holds`] — the four conditions of the
    /// `Declaration:Jishogi 1.1` rule announced in `Game_Summary`. A declaration
    /// that holds ends the game [`Outcome::Declaration`] with `valid: true`, a
    /// win for `side`; one that does not ends it with `valid: false`, which is an
    /// illegal action by the declarer and a loss for it. Both are terminations:
    /// the reference makes the failed declaration itself the losing act rather
    /// than an ignored line, so there is no path here that leaves the game
    /// running.
    ///
    /// **Nothing is recorded and nothing is settled.** No move was played, so the
    /// ply count does not advance and `Max_Moves` is not approached by declaring;
    /// and the echo carries no `,T` (the reference writes a bare `%KACHI`), so
    /// deducting a charge would move a clock no client was ever told about —
    /// invariant 4 in the direction a resignation reads it forwards. A
    /// resignation's T-value is written, so it is deducted; a declaration's is
    /// not written, so it is not deducted.
    ///
    /// **A late `%KACHI` is a flag, not a declaration**, on exactly
    /// [`resign`](Self::resign)'s terms: the reference decides the timeout before
    /// it reads what arrived, so a declaration charged past the allowance is
    /// [`Outcome::Timeout`] against the declarer and is never adjudicated.
    ///
    /// # Errors
    ///
    /// - [`NotDeclarable::Finished`] — the game already ended; nothing changes.
    /// - [`NotDeclarable::NotToMove`] — a declaration from the side not to move.
    ///   The reference reaches `good_kachi?` only from the current player, and
    ///   the 27-point rule is a claim about the position on one's **own** turn,
    ///   so there is nothing here to adjudicate. Answered as P-4 answers a move
    ///   from the wrong side — a protocol error that alters no state — rather
    ///   than as a failed declaration, which would end the game against a player
    ///   whose claim was never judged. This is where a declaration and a
    ///   resignation differ: giving up is available at any time, and claiming a
    ///   win is not.
    pub fn declare(&mut self, side: Color, charged: u32) -> Result<Verdict, NotDeclarable> {
        if let Some(outcome) = self.outcome {
            return Err(Finished { outcome }.into());
        }

        let to_move = self.side_to_move();
        if side != to_move {
            return Err(NotDeclarable::NotToMove { side, to_move });
        }

        let outcome = match self.flagged(side, charged) {
            Some(_) => Outcome::Timeout { by: side },
            None => Outcome::Declaration {
                by: side,
                valid: declaration::holds(&self.position, side),
            },
        };
        self.outcome = Some(outcome);

        Ok(termination_of(outcome))
    }

    /// Records `%CHUDAN` from `side`, charged `charged` units.
    ///
    /// **Suspension is not supported**, and that is a route rather than a
    /// silence: the reference reaches it through its own code. `command.rb`
    /// classes `%CHUDAN` as an ordinary special move (`/^%[^%]/` →
    /// `SpecialCommand`), `board.rb`'s `handle_one_move` matches it against
    /// `%KACHI` and `%TORYO`, falls through to `:illegal`, and `game.rb` ends
    /// the game against the sender (`GameResultIllegalMoveWin`). So an in-game
    /// `%CHUDAN` is an illegal move by whoever sent it and loses:
    /// [`Outcome::IllegalMove`] against `side`, which carries P-7's row for an
    /// illegal move unchanged — the same reason line, the same two results, and
    /// the same timed echo of the line received. Nothing here is new.
    ///
    /// The charge settles for [`apply`](Self::apply)'s reason: the echo writes
    /// it, so it is deducted (invariant 4). Nothing is recorded — no move was
    /// played, so the ply count does not advance and `Max_Moves` is not
    /// approached by asking for a suspension.
    ///
    /// **A late `%CHUDAN` is a flag**, exactly as a late `%TORYO` or a late
    /// `%KACHI` is: the reference checks the timeout before it adjudicates
    /// anything, so the outcome is [`Outcome::Timeout`] against `side` and
    /// nothing is settled.
    ///
    /// # Errors
    ///
    /// - [`NotSuspendable::Finished`] — the game already ended; nothing changes.
    /// - [`NotSuspendable::NotToMove`] — a `%CHUDAN` from the side not to move.
    ///   Answered as P-4 answers a move from the wrong side — a protocol error
    ///   that alters no state — on [`declare`](Self::declare)'s terms: the
    ///   reference reads a special move only from the current player, so there
    ///   is nothing to adjudicate, and ending the game against a player whose
    ///   line was never adjudicated is precisely what that path avoids.
    pub fn suspend(&mut self, side: Color, charged: u32) -> Result<Verdict, NotSuspendable> {
        if let Some(outcome) = self.outcome {
            return Err(Finished { outcome }.into());
        }

        let to_move = self.side_to_move();
        if side != to_move {
            return Err(NotSuspendable::NotToMove { side, to_move });
        }

        let outcome = match self.flagged(side, charged) {
            Some(_) => Outcome::Timeout { by: side },
            None => {
                self.settle(side, charged);
                Outcome::IllegalMove { by: side }
            }
        };
        self.outcome = Some(outcome);

        Ok(termination_of(outcome))
    }

    /// The armed deadline fired with `charged` units elapsed and nothing
    /// received.
    ///
    /// P-5's other timeout path, and the only one that reaches a **silent**
    /// player: the arrival path above cannot fire, because a client that stops
    /// sending never produces an arrival to measure. What the task supplies is
    /// the measurement — [`clock::charged_units`] of what has elapsed since the
    /// previous relay — and the verdict is the same `flagged` predicate a move
    /// goes through, so a timer and an arrival cannot reach different answers
    /// on the same numbers.
    ///
    /// `None` where the game already ended, or where the charge does **not**
    /// reach the side to move's allowance. The second is a turn still open, not
    /// an error: a caller rearms its deadline rather than terminating, and the
    /// verdict rather than the timer decides. [`clock::flag_after`] makes it
    /// unreachable for a timer that fires no earlier than the instant it was
    /// armed for.
    ///
    /// Nothing is settled and no move is recorded — there is nothing to record,
    /// and a clock movement no client was told of is invariant 4 broken.
    ///
    /// [`clock::charged_units`]: super::clock::charged_units
    /// [`clock::flag_after`]: super::clock::flag_after
    pub fn expired(&mut self, charged: u32) -> Option<Verdict> {
        if self.outcome.is_some() {
            return None;
        }

        let side = self.side_to_move();
        self.flagged(side, charged)?;

        let outcome = Outcome::Timeout { by: side };
        self.outcome = Some(outcome);

        Some(termination_of(outcome))
    }

    /// The allowance `side` just exceeded, or `None` if it did not — or could
    /// not, because it is not the side whose clock is running.
    ///
    /// One predicate for both events, so that a move and a `%TORYO` cannot reach
    /// different verdicts on the same numbers. Returning the allowance rather
    /// than a `bool` because every caller reports it.
    fn flagged(&self, side: Color, charged: u32) -> Option<u32> {
        if side != self.side_to_move() {
            return None;
        }

        self.allowance(side)
            .filter(|&allowance| charged >= allowance)
    }

    /// Whether the history has reached `Max_Moves`, setup entries included.
    ///
    /// `>=` rather than `==`, as in the reference
    /// (`@max_moves > 0 && @move_count >= @max_moves`): the two differ only
    /// where a `Game` was hand-built over a setup that already meets the limit,
    /// and there the game ends on the first real move rather than never. O-1
    /// keeps a configured collection away from that case, and the reading is the
    /// one `config/validate.rs`'s own table gives it — "otherwise the game ends
    /// `#MAX_MOVES` at move one".
    ///
    /// `None` never reaches it: no limit is no restriction, not a limit of zero.
    fn reached_max_moves(&self) -> bool {
        self.max_moves
            .is_some_and(|max| self.moves.len() >= usize::try_from(max).unwrap_or(usize::MAX))
    }

    /// Settles a charged event against `side`'s clock: credit, deduct, floor.
    ///
    /// `TimeClock#process_time`, which is one operation rather than two:
    ///
    /// ```ruby
    /// player.mytime += @fischer
    /// player.mytime -= t
    /// if (player.mytime < 0) then player.mytime = 0 end
    /// ```
    ///
    /// The credit is what makes invariant 4 hold across a whole game and not
    /// merely across one move: a client applies `remaining + increment − T` to
    /// what it was told, and this is the same arithmetic on the same numbers.
    ///
    /// The floor diverges from an unclamped client ledger in exactly one case —
    /// a configured reduction larger than `Total_Time`, which is a configuration
    /// with no sensible reading — and the clamp settles it rather than O-1
    /// gaining a rule for it. Saturating rather than wrapping for the reason it
    /// always was: a wrapped clock reads as a side with four billion units left.
    fn settle(&mut self, side: Color, charged: u32) {
        let increment = increment_units(&self.time);
        let clock = &mut self.remaining[slot(side)];
        *clock = settled(*clock, increment, charged);
    }
}

/// One settlement, as arithmetic alone: `clamp₀(remaining + increment − charged)`.
///
/// A free function because [`Game::new`] settles its seeded setup moves before a
/// `Game` exists to call a method on, and the seeding and the live path settling
/// differently is precisely how the clocks would drift from the clients'.
const fn settled(remaining: u32, increment: u32, charged: u32) -> u32 {
    remaining.saturating_add(increment).saturating_sub(charged)
}

/// What a finished game sends, derived from its [`Outcome`].
///
/// P-7's table as one value: the reason line, the scoring the two result lines
/// are read from, and whether a move or declaration was received to echo before
/// them. The fields are private because the three are one mapping's output —
/// a hand-built value could pair `#SENNICHITE` with a loser, and P-7 asks for
/// one termination path precisely so that cannot happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    reason: Reason,
    scoring: Scoring,
    echo: Echoing,
}

impl Verdict {
    /// The reason line: exactly one status per outcome.
    pub const fn reason(self) -> Reason {
        self.reason
    }

    /// How the game is scored, before it is read from either side.
    pub const fn scoring(self) -> Scoring {
        self.scoring
    }

    /// What precedes the reason line, if anything: the received line with its
    /// consumption time, the received line bare, or nothing at all.
    ///
    /// The choice belongs here rather than at the call site that renders it, for
    /// P-7's reason: one termination path, so the shape cannot come out right in
    /// one case and wrong in another.
    pub const fn echoing(self) -> Echoing {
        self.echo
    }

    /// Whether a move or declaration was received **and accepted**, and so
    /// precedes the reason line as an echo of either shape.
    ///
    /// False for exactly `#CENSORED` and `#TIME_UP`. The first means no move or
    /// declaration was received at all.
    /// `#TIME_UP` is the second for a reason of its own — one may well
    /// have been received, and neither of its two paths *accepted* it. The
    /// deadline path has nothing in hand to echo, and the arrival path
    /// deliberately drops what it holds, because the flag fell before that line
    /// existed: no clock moved, so an echo would announce a deduction that
    /// never happened (invariant 4). It is shogi-server's own shape too
    /// (`game_result.rb`, `GameResultTimeoutWin#process`: `"#TIME_UP\n#WIN\n"`,
    /// with nothing before it).
    ///
    /// The echo itself is built by the task, from the line the client sent.
    pub const fn is_echoed(self) -> bool {
        !matches!(self.echo, Echoing::None)
    }

    /// The result line `side` receives.
    ///
    /// Total, because every outcome is scored: the two sides receive opposite
    /// results where the game has a loser and the same one where it is drawn.
    /// A termination a caller has to handle the *absence* of a result for no
    /// longer exists — `#CHUDAN` was the one such case, and it went with the
    /// decision that this server does not suspend games (see
    /// [`Game::suspend`]).
    pub fn result(self, side: Color) -> GameResult {
        match self.scoring {
            Scoring::Loser(loser) if side == loser => GameResult::Lose,
            Scoring::Loser(_) => GameResult::Win,
            Scoring::Draw => GameResult::Draw,
        }
    }
}

/// What a termination writes before its reason line.
///
/// Three cases rather than a `bool` beside a time, because a `%KACHI` is echoed
/// **bare**: shogi-server's `game_result.rb` writes `"%KACHI\n#JISHOGI\n#WIN\n"`
/// and its three companions with no `,T` anywhere in them, unlike every other
/// echoed termination. Nothing is deducted for a declaration either, so the wire
/// and the clock agree (invariant 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echoing {
    /// Nothing precedes the reason line — `#CENSORED` and `#TIME_UP`.
    None,

    /// The received line with the time it was charged, `<line>,T<charged>`.
    Timed,

    /// The received line alone. `%KACHI` only.
    Bare,
}

/// How a finished game is scored.
///
/// Two cases rather than a result per side, so that "the two clients receive
/// opposite results" is a property of the type instead of a pair a caller has
/// to keep consistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scoring {
    /// The named side lost; the other won.
    Loser(Color),

    /// Both sides drew — `#SENNICHITE` and `#MAX_MOVES`.
    Draw,
}

/// P-7's mapping from an outcome to what the game sends, in one place.
///
/// Every variant maps to exactly one reason, and the result line is derived per
/// side from the [`Scoring`] rather than chosen at the two call sites. Two
/// entries are worth reading twice:
///
/// - [`Outcome::Declaration`] splits on validity. A `%KACHI` that holds is
///   `#JISHOGI` and a win for the declarer; one that does not is an illegal
///   action by the declarer, ends the game against them, and says `#ILLEGAL_MOVE`
///   — the same status an illegal move gets, per the `Declaration` rule
///   announced in `Game_Summary`. Both echo [`Echoing::Bare`]: the declaration
///   is the one termination the reference writes with no `,T`.
/// - [`Outcome::MaxMoves`] is a **draw**, following shogi-server's
///   `GameResultMaxMovesDraw`. The specification lists the status without fixing
///   the result, so the reference implementation governs (P-7).
/// - [`Outcome::Timeout`] echoes **nothing**, unlike every other variant that
///   can have a line in hand. See [`Verdict::is_echoed`] for why the two
///   timeout paths agree on that.
///
/// There is no `#CHUDAN` row, because there is no outcome that reaches one: an
/// in-game `%CHUDAN` is adjudicated an illegal move and takes
/// [`Outcome::IllegalMove`]'s row unchanged ([`Game::suspend`]).
pub const fn termination_of(outcome: Outcome) -> Verdict {
    let (reason, scoring, echo) = match outcome {
        Outcome::Resignation { by } => (Reason::Resign, Scoring::Loser(by), Echoing::Timed),
        Outcome::Declaration { by, valid: true } => (
            Reason::Jishogi,
            Scoring::Loser(by.opponent()),
            Echoing::Bare,
        ),
        Outcome::Declaration { by, valid: false } => {
            (Reason::IllegalMove, Scoring::Loser(by), Echoing::Bare)
        }
        Outcome::IllegalMove { by } => (Reason::IllegalMove, Scoring::Loser(by), Echoing::Timed),
        Outcome::Timeout { by } => (Reason::TimeUp, Scoring::Loser(by), Echoing::None),
        Outcome::Repetition => (Reason::Sennichite, Scoring::Draw, Echoing::Timed),
        Outcome::PerpetualCheck { by } => {
            (Reason::OuteSennichite, Scoring::Loser(by), Echoing::Timed)
        }
        Outcome::MaxMoves => (Reason::MaxMoves, Scoring::Draw, Echoing::Timed),
        Outcome::Disconnected { by } => (Reason::Censored, Scoring::Loser(by), Echoing::None),
    };

    Verdict {
        reason,
        scoring,
        echo,
    }
}

/// The game has already ended, and was offered another event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the game already ended ({outcome:?})")]
pub struct Finished {
    /// How it ended — the outcome already recorded, unchanged by the event that
    /// arrived late.
    pub outcome: Outcome,
}

/// Why a `%KACHI` was not adjudicated.
///
/// Neither variant is a *failed* declaration: a declaration that is judged and
/// does not hold ends the game against the declarer and comes back as an
/// [`Outcome::Declaration`], not as an error. These two are the cases where
/// there was nothing to judge, which is why they may not collapse into it — one
/// of them would otherwise end a game against a player whose claim was never
/// read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotDeclarable {
    /// The game had already ended. Nothing changed.
    #[error(transparent)]
    Finished(#[from] Finished),

    /// A declaration from the side not to move: answered as P-4 answers a move
    /// from the wrong side, altering no state. See [`Game::declare`].
    #[error("a declaration from {side:?} arrived with {to_move:?} to move")]
    NotToMove {
        /// The side that declared.
        side: Color,
        /// The side whose turn it actually is.
        to_move: Color,
    },
}

/// Why a `%CHUDAN` was not adjudicated.
///
/// [`NotDeclarable`]'s pair, for [`Game::suspend`], and separate from it for the
/// reason the two are separate methods: an error that says "a declaration"
/// would misreport which line arrived. Neither variant is a *refused*
/// suspension — a suspension is always refused, and the refusal is a
/// termination rather than an error. These two are the cases where there was
/// nothing to adjudicate at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotSuspendable {
    /// The game had already ended. Nothing changed.
    #[error(transparent)]
    Finished(#[from] Finished),

    /// A `%CHUDAN` from the side not to move: answered as P-4 answers a move
    /// from the wrong side, altering no state. See [`Game::suspend`].
    #[error("a %CHUDAN from {side:?} arrived with {to_move:?} to move")]
    NotToMove {
        /// The side that sent it.
        side: Color,
        /// The side whose turn it actually is.
        to_move: Color,
    },
}

/// Why a move was not applied.
///
/// The four classes are separated because they are four different events to the
/// task: one is a caller bug, one is P-4's protocol error, and two end the game
/// with different reason lines. Collapsing any two would make a client's illegal
/// move indistinguishable from a server mistake, or a loss on time from a loss
/// on the rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Rejected {
    /// The game had already ended. Nothing changed.
    #[error(transparent)]
    Finished(#[from] Finished),

    /// A move from the side not to move: P-4's protocol error, which "alters no
    /// state" and is **not** `#ILLEGAL_MOVE`. The project decision behind that
    /// is on P-7's capture list, since shogi-server's source never emits
    /// `#ILLEGAL_ACTION` and so does not settle it.
    #[error("a move from {side:?} arrived with {to_move:?} to move")]
    NotToMove {
        /// The side that sent it.
        side: Color,
        /// The side whose turn it actually is.
        to_move: Color,
    },

    /// The flag fell before this move existed: [`Outcome::Timeout`] against the
    /// side to move, which is `by`. P-5's termination, and **not** an illegal
    /// move — it scores `#TIME_UP` rather than `#ILLEGAL_MOVE`, so collapsing
    /// the two would send the wrong reason line.
    ///
    /// The move is not recorded and no clock moves; the numbers are here because
    /// every caller logs them.
    #[error("{by:?} was charged {charged} against an allowance of {allowance}")]
    Timeout {
        /// The side whose allowance ran out: the side to move.
        by: Color,
        /// What the arriving input was charged.
        charged: u32,
        /// What it was measured against — remaining plus byoyomi plus increment.
        allowance: u32,
    },

    /// The move is illegal, and the game is over: [`Outcome::IllegalMove`]
    /// against the mover. The only variant here that changes state.
    #[error("illegal move: {0}")]
    Illegal(#[from] Illegal),
}

/// The ply number the next entry takes: 1-based over the whole history, setup
/// included.
///
/// Saturating rather than casting: `Max_Moves` keeps a real game some four
/// billion plies short of the ceiling, and a wrapped ply would number the record
/// from zero again rather than fail.
fn next_ply(moves: &[PlayedMove]) -> u32 {
    u32::try_from(moves.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

/// Which side played setup move `index`.
///
/// The parity `config::validate` establishes and [`setup_t_values`] places the
/// reduction by: a sequence legal from hirate alternates strictly from Black, so
/// even indices are Black's and odd ones White's. Asking the move itself is not
/// an alternative — a [`Move`] carries no side, because which side plays it is a
/// property of the position it is applied to.
const fn setup_mover(index: usize) -> Color {
    if index.is_multiple_of(2) {
        Color::Black
    } else {
        Color::White
    }
}

/// Index into a per-side array: `[black, white]`.
///
/// `Color`'s own index is private to [`game`](crate::game), on the same terms as
/// [`agreement`](super::agreement)'s copy of this.
const fn slot(side: Color) -> usize {
    match side {
        Color::Black => 0,
        Color::White => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::config::{Reduction, TimeUnit};
    use crate::csa::{MoveEcho, Termination};
    use crate::game::{HandKind, Piece, PieceKind, Square};
    use crate::session::clock::KING_SHUTTLE;

    /// The allowance every configuration below starts from, in its own unit.
    /// Larger than the example reduction, so that a reduced clock is a visible
    /// subtraction rather than a saturation.
    const TOTAL: u32 = 1800;

    /// A symmetric configuration: no increment, no floor, truncation. Each test
    /// turns on exactly the keys it is about.
    fn config() -> TimeConfig {
        TimeConfig {
            unit: TimeUnit::Second,
            total: TimeUnit::Second.duration(TOTAL),
            byoyomi: None,
            increment: None,
            least_time_per_move: Duration::ZERO,
            roundup: false,
            reduction: None,
        }
    }

    /// The asymmetric worked example's `[time]` table: `1sec`, `Increment:2`, and 600 units off
    /// White's allowance — the configuration that produces the `T602` shape.
    fn asymmetric_example() -> TimeConfig {
        TimeConfig {
            increment: Some(Duration::from_secs(2)),
            reduction: Some(Reduction {
                side: Color::White,
                amount: Duration::from_secs(600),
            }),
            ..config()
        }
    }

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    fn board(from: (u8, u8), to: (u8, u8)) -> Move {
        Move::Board {
            from: sq(from.0, from.1),
            to: sq(to.0, to.1),
            promote: false,
        }
    }

    fn buoy(setup: &[Move]) -> StartSpec {
        StartSpec::Buoy {
            setup: setup.to_vec(),
        }
    }

    /// An unlimited game: `Max_Moves` omitted, which is most of these tests.
    fn game(spec: &StartSpec, cfg: &TimeConfig) -> Game {
        limited(spec, cfg, None)
    }

    /// The same, under a stated `Max_Moves`.
    fn limited(spec: &StartSpec, cfg: &TimeConfig, max_moves: Option<u32>) -> Game {
        Game::new("20260814-tabia-1-3".to_owned(), spec, cfg, max_moves)
            .unwrap_or_else(|error| panic!("{spec:?} failed to start: {error}"))
    }

    /// The published-collection example of O-1, `7g7f 3c3d 2g2f`.
    fn collection_example() -> Vec<Move> {
        vec![
            board((7, 7), (7, 6)),
            board((3, 3), (3, 4)),
            board((2, 7), (2, 6)),
        ]
    }

    fn t_values(game: &Game) -> Vec<u32> {
        game.moves().iter().map(|played| played.t).collect()
    }

    fn plies(game: &Game) -> Vec<u32> {
        game.moves().iter().map(|played| played.ply).collect()
    }

    /// Both clocks as the server holds them, `[black, white]`.
    fn game_clocks(game: &Game) -> [u32; 2] {
        [game.remaining(Color::Black), game.remaining(Color::White)]
    }

    /// Both clocks as a **client** computes them: P-5's rule,
    /// `remaining + increment − T`, folded over the T-values that side was sent.
    ///
    /// Deliberately not `settled`: the identity is worth nothing if it is
    /// asserted against the same expression that produced it. The fold is signed
    /// and unclamped, so the clamp is a divergence it would catch rather than
    /// reproduce, and nothing below folds a ledger that goes negative.
    ///
    /// Every entry's mover is its index's parity, for the reason
    /// [`setup_mover`] states — which holds through the played moves too,
    /// because a buoy history alternates strictly from Black at ply 1. A written
    /// board need not, and no test folds one.
    fn client_ledger(game: &Game, cfg: &TimeConfig) -> [u32; 2] {
        let increment = i64::from(increment_units(cfg));
        let mut ledger = [i64::from(total_units(cfg)); 2];

        for (index, played) in game.moves().iter().enumerate() {
            let clock = &mut ledger[slot(setup_mover(index))];
            *clock += increment - i64::from(played.t);
        }

        ledger.map(|units| u32::try_from(units).expect("no test folds a ledger below zero"))
    }

    /// A declaration-ready position for `declarer`, holding `hand_pawns` pawns.
    ///
    /// The entered king on the enemy home rank, ten pieces beside it worth
    /// eighteen points, and one point per pawn in hand — so `hand_pawns` alone
    /// decides whether the declaration holds, at either threshold. The same
    /// layout [`declaration`]'s own tests are built on; what these tests are
    /// about is what a `Game` does with the verdict, not how it is reached.
    fn entered(declarer: Color, hand_pawns: u8) -> Position {
        // `1` is the enemy's home rank and `9` the declarer's own, so one
        // layout serves both colors.
        let rank = |from_enemy_home: u8| match declarer {
            Color::Black => from_enemy_home,
            Color::White => 10 - from_enemy_home,
        };

        let mut position = Position::hirate();
        for file in 1..=9 {
            for rank in 1..=9 {
                position.set_piece_at(sq(file, rank), None);
            }
        }
        position.set_side_to_move(declarer);

        let mut place = |file, from_enemy_home, kind, color| {
            position.set_piece_at(sq(file, rank(from_enemy_home)), Some(Piece { kind, color }));
        };
        place(5, 1, PieceKind::King, declarer);
        place(1, 1, PieceKind::Rook, declarer);
        place(2, 1, PieceKind::Bishop, declarer);
        place(3, 1, PieceKind::Gold, declarer);
        place(4, 1, PieceKind::Gold, declarer);
        place(6, 1, PieceKind::Silver, declarer);
        place(7, 1, PieceKind::Silver, declarer);
        place(8, 1, PieceKind::Knight, declarer);
        place(9, 1, PieceKind::Knight, declarer);
        place(1, 2, PieceKind::Lance, declarer);
        place(2, 2, PieceKind::Lance, declarer);
        place(5, 9, PieceKind::King, declarer.opponent());

        for _ in 0..hand_pawns {
            position.hand_mut(declarer).add(HandKind::Pawn);
        }

        position
    }

    /// The lines one client receives for a termination that echoes `text`, with
    /// the shape the **verdict** chooses — which is the rule
    /// [`Task::terminate`](super::pairing) applies on the wire.
    ///
    /// [`Task::terminate`]: super::pairing
    fn echoed_lines(verdict: Verdict, text: &str, charged: u32, side: Color) -> Vec<String> {
        let result = verdict.result(side);
        let termination = match verdict.echoing() {
            Echoing::Timed => Termination::with_echo(
                MoveEcho {
                    text,
                    consumed: charged,
                },
                verdict.reason(),
                result,
            ),
            Echoing::Bare => Termination::with_bare_echo(text, verdict.reason(), result),
            Echoing::None => Termination::without_echo(verdict.reason(), result),
        };

        termination.lines().map(|line| line.to_string()).collect()
    }

    /// The three lines one client receives, as text: the composition the task
    /// performs, so that a verdict is checked against the wire rather than
    /// against itself.
    fn lines(verdict: Verdict, echo: Option<MoveEcho<'_>>, side: Color) -> Vec<String> {
        let result = verdict.result(side);
        let termination = match echo {
            Some(echo) => Termination::with_echo(echo, verdict.reason(), result),
            None => Termination::without_echo(verdict.reason(), result),
        };

        termination.lines().map(|line| line.to_string()).collect()
    }

    #[test]
    fn a_hirate_start_under_symmetric_time_begins_empty_with_two_full_clocks() {
        let spec = buoy(&[]);
        let game = game(&spec, &config());

        assert!(game.moves().is_empty());
        assert_eq!(game.remaining(Color::Black), TOTAL);
        assert_eq!(game.remaining(Color::White), TOTAL);
        assert_eq!(game.position(), &Position::hirate());
        assert_eq!(game.initial(), game.position());
        assert_eq!(game.start(), &spec);
        assert_eq!(game.side_to_move(), Color::Black);
        assert_eq!(game.outcome(), None);
        assert_eq!(game.id(), "20260814-tabia-1-3");
    }

    #[test]
    fn the_example_configuration_starts_with_the_shuttle_seeded_and_only_the_reduction_taken() {
        // A hirate entry carrying a reduction: the shuttle is what the wire
        // shows, so it is what the history holds and what the clocks settle
        // against.
        let game = game(&buoy(&[]), &asymmetric_example());

        assert_eq!(
            game.start(),
            &StartSpec::Buoy {
                setup: KING_SHUTTLE.to_vec()
            }
        );
        let seeded: Vec<Move> = game.moves().iter().map(|played| played.mv).collect();
        assert_eq!(seeded, KING_SHUTTLE.to_vec());
        assert_eq!(t_values(&game), vec![2, 602, 2, 2]);
        assert_eq!(plies(&game), vec![1, 2, 3, 4]);
        assert!(game.moves().iter().all(|played| played.is_setup));

        // Every T-value cancels against the increment it was written to cancel
        // against, so what is left of the four is the reduction alone: White at
        // `total − 600`, Black untouched at `START`.
        assert_eq!(game.remaining(Color::White), TOTAL - 600);
        assert_eq!(game.remaining(Color::Black), TOTAL);
        assert_eq!(
            client_ledger(&game, &asymmetric_example()),
            game_clocks(&game)
        );

        // The shuttle returns exactly to hirate, so play begins there.
        assert_eq!(game.position(), &Position::hirate());
        assert_eq!(game.side_to_move(), Color::Black);
    }

    #[test]
    fn an_authored_setup_becomes_the_history_and_the_position_it_decodes_to() {
        let cfg = TimeConfig {
            increment: Some(Duration::from_secs(2)),
            ..config()
        };
        let spec = buoy(&collection_example());
        let game = game(&spec, &cfg);

        assert_eq!(game.start(), &spec);
        let seeded: Vec<Move> = game.moves().iter().map(|played| played.mv).collect();
        assert_eq!(seeded, collection_example());
        assert_eq!(t_values(&game), vec![2, 2, 2]);
        assert_eq!(plies(&game), vec![1, 2, 3]);
        assert!(game.moves().iter().all(|played| played.is_setup));

        // With no reduction each move is charged exactly the increment it is
        // given back, so both clocks reach `START` at the full allowance —
        // P-2's worked example, and what every client applying
        // `remaining + increment − T` computes for itself.
        assert_eq!(game.remaining(Color::Black), TOTAL);
        assert_eq!(game.remaining(Color::White), TOTAL);
        assert_eq!(client_ledger(&game, &cfg), game_clocks(&game));

        assert_eq!(game.position(), &spec.decode().expect("legal from hirate"));
        // Three plies, so White is to move — the parity falls out of the replay.
        assert_eq!(game.side_to_move(), Color::White);
        assert_eq!(
            game.position().piece_at(sq(7, 6)).map(|piece| piece.kind),
            Some(PieceKind::Pawn)
        );
    }

    #[test]
    fn a_written_board_starts_from_it_with_no_history() {
        let mut written = Position::hirate();
        written.set_piece_at(sq(1, 1), None);
        written.hand_mut(Color::White).add(HandKind::Lance);
        let spec = StartSpec::Board(written.clone());

        let game = game(&spec, &config());

        assert!(game.moves().is_empty());
        assert_eq!(game.position(), &written);
        assert_eq!(game.remaining(Color::Black), TOTAL);
        assert_eq!(game.remaining(Color::White), TOTAL);
    }

    #[test]
    fn a_setup_the_replay_refuses_is_reported_rather_than_started_from() {
        // 7g7f twice: the second has no pawn to move.
        let spec = buoy(&[
            board((7, 7), (7, 6)),
            board((3, 3), (3, 4)),
            board((7, 7), (7, 6)),
        ]);

        let rejection = Game::new("g".to_owned(), &spec, &config(), None)
            .expect_err("the third entry is illegal");

        assert_eq!(rejection.index, 2);
    }

    #[test]
    fn a_reduction_larger_than_the_allowance_clamps_rather_than_wrapping() {
        // Not a configuration O-1 rejects, and a wrapped clock would read as a
        // side with four billion units left. The clamp is the one case where
        // the server's clock and an unclamped client ledger part company, and
        // the configuration has no sensible reading anyway.
        let cfg = TimeConfig {
            total: TimeUnit::Second.duration(60),
            ..asymmetric_example()
        };
        let game = game(&buoy(&[]), &cfg);

        assert_eq!(game.remaining(Color::White), 0);
        assert_eq!(game.remaining(Color::Black), 60);
    }

    #[test]
    fn a_legal_move_continues_the_ply_count_and_settles_only_the_movers_clock() {
        let mut game = game(&buoy(&[]), &asymmetric_example());
        let (black, white) = (game.remaining(Color::Black), game.remaining(Color::White));

        let played = game
            .apply(Color::Black, board((7, 7), (7, 6)), 12)
            .expect("7g7f is legal from hirate");

        // The setup seeded four plies, so the first real move is the fifth.
        assert_eq!(
            played,
            PlayedMove {
                ply: 5,
                mv: board((7, 7), (7, 6)),
                t: 12,
                is_setup: false,
            }
        );
        assert_eq!(game.moves().last(), Some(&played));
        assert_eq!(game.moves().len(), 5);

        // Twelve charged against a two-unit increment: the mover is ten worse
        // off, not twelve, and the opponent is untouched.
        assert_eq!(game.remaining(Color::Black), black + 2 - 12);
        assert_eq!(game.remaining(Color::White), white);
        assert_eq!(
            client_ledger(&game, &asymmetric_example()),
            game_clocks(&game)
        );
        assert_eq!(game.side_to_move(), Color::White);
        assert_eq!(
            game.position().piece_at(sq(7, 6)).map(|piece| piece.kind),
            Some(PieceKind::Pawn)
        );
        assert_eq!(game.outcome(), None);

        // The position play began from is not disturbed by play.
        assert_eq!(game.initial(), &Position::hirate());
    }

    #[test]
    fn a_move_from_the_side_not_to_move_changes_nothing_at_all() {
        let mut game = game(&buoy(&collection_example()), &config());
        let before = game.clone();

        // Three setup plies, so White is to move.
        let rejection = game
            .apply(Color::Black, board((2, 6), (2, 5)), 12)
            .expect_err("Black is not to move");

        assert_eq!(
            rejection,
            Rejected::NotToMove {
                side: Color::Black,
                to_move: Color::White,
            }
        );
        // Position, history, and clocks: bit for bit, including the charge the
        // sender is not made to pay.
        assert_eq!(game, before);
    }

    #[test]
    fn an_illegal_move_ends_the_game_against_the_mover() {
        let mut game = game(&buoy(&[]), &config());

        // No pawn on 7f to move, and Black is to move.
        let rejection = game
            .apply(Color::Black, board((7, 6), (7, 5)), 12)
            .expect_err("7f is empty");

        assert!(
            matches!(rejection, Rejected::Illegal(Illegal::EmptySquare { .. })),
            "{rejection:?}"
        );
        assert_eq!(
            game.outcome(),
            Some(Outcome::IllegalMove { by: Color::Black })
        );
        // The echo carries the charge, so the charge is deducted.
        assert_eq!(game.remaining(Color::Black), TOTAL - 12);
        assert_eq!(game.remaining(Color::White), TOTAL);
        // The illegal move is not history: it was never made on the board.
        assert!(game.moves().is_empty());
        assert_eq!(game.position(), &Position::hirate());
    }

    #[test]
    fn a_finished_game_accepts_no_further_move_or_resignation() {
        let mut game = game(&buoy(&[]), &config());
        game.apply(Color::Black, board((7, 6), (7, 5)), 12)
            .expect_err("7f is empty");
        let finished = Finished {
            outcome: Outcome::IllegalMove { by: Color::Black },
        };
        let after = game.clone();

        assert_eq!(
            game.apply(Color::White, board((3, 3), (3, 4)), 1),
            Err(Rejected::Finished(finished))
        );
        assert_eq!(game.resign(Color::White, 1), Err(finished));
        assert_eq!(game, after);
    }

    #[test]
    fn a_resignation_sends_the_specifications_own_three_lines() {
        let mut game = game(&buoy(&[]), &config());

        let verdict = game
            .resign(Color::Black, 3)
            .expect("the game is still being played");

        assert_eq!(
            game.outcome(),
            Some(Outcome::Resignation { by: Color::Black })
        );
        assert_eq!(game.remaining(Color::Black), TOTAL - 3);
        assert_eq!(game.remaining(Color::White), TOTAL);

        let echo = MoveEcho {
            text: "%TORYO",
            consumed: 3,
        };
        assert_eq!(
            lines(verdict, Some(echo), Color::Black),
            ["%TORYO,T3", "#RESIGN", "#LOSE"]
        );
        assert_eq!(
            lines(verdict, Some(echo), Color::White),
            ["%TORYO,T3", "#RESIGN", "#WIN"]
        );
    }

    #[test]
    fn a_resignation_is_accepted_from_the_side_not_to_move() {
        // Black is to move; White gives up while it is not its turn.
        let mut game = game(&buoy(&[]), &config());

        let verdict = game
            .resign(Color::White, 0)
            .expect("resigning is not a move");

        assert_eq!(
            game.outcome(),
            Some(Outcome::Resignation { by: Color::White })
        );
        assert_eq!(verdict.result(Color::White), GameResult::Lose);
    }

    #[test]
    fn every_outcome_maps_to_its_reason_and_its_per_side_results() {
        // P-7's table, variant by variant. The loser is written as the side the
        // status is against, and the winner falls out of it.
        let cases: [(Outcome, Reason, Scoring); 9] = [
            (
                Outcome::Resignation { by: Color::Black },
                Reason::Resign,
                Scoring::Loser(Color::Black),
            ),
            (
                Outcome::Declaration {
                    by: Color::Black,
                    valid: true,
                },
                Reason::Jishogi,
                Scoring::Loser(Color::White),
            ),
            (
                Outcome::Declaration {
                    by: Color::White,
                    valid: false,
                },
                Reason::IllegalMove,
                Scoring::Loser(Color::White),
            ),
            (
                Outcome::IllegalMove { by: Color::White },
                Reason::IllegalMove,
                Scoring::Loser(Color::White),
            ),
            (
                Outcome::Timeout { by: Color::Black },
                Reason::TimeUp,
                Scoring::Loser(Color::Black),
            ),
            (Outcome::Repetition, Reason::Sennichite, Scoring::Draw),
            (
                Outcome::PerpetualCheck { by: Color::White },
                Reason::OuteSennichite,
                Scoring::Loser(Color::White),
            ),
            (Outcome::MaxMoves, Reason::MaxMoves, Scoring::Draw),
            (
                Outcome::Disconnected { by: Color::Black },
                Reason::Censored,
                Scoring::Loser(Color::Black),
            ),
        ];

        for (outcome, reason, scoring) in cases {
            let verdict = termination_of(outcome);

            assert_eq!(verdict.reason(), reason, "{outcome:?}");
            assert_eq!(verdict.scoring(), scoring, "{outcome:?}");

            match scoring {
                Scoring::Loser(loser) => {
                    assert_eq!(verdict.result(loser), GameResult::Lose, "{outcome:?}");
                    assert_eq!(
                        verdict.result(loser.opponent()),
                        GameResult::Win,
                        "{outcome:?}"
                    );
                }
                Scoring::Draw => {
                    for side in [Color::Black, Color::White] {
                        assert_eq!(verdict.result(side), GameResult::Draw, "{outcome:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_valid_declaration_wins_and_an_invalid_one_loses_for_the_declarer() {
        for by in [Color::Black, Color::White] {
            let valid = termination_of(Outcome::Declaration { by, valid: true });
            assert_eq!(valid.reason(), Reason::Jishogi);
            assert_eq!(valid.result(by), GameResult::Win);
            assert_eq!(valid.result(by.opponent()), GameResult::Lose);

            let invalid = termination_of(Outcome::Declaration { by, valid: false });
            assert_eq!(invalid.reason(), Reason::IllegalMove);
            assert_eq!(invalid.result(by), GameResult::Lose);
            assert_eq!(invalid.result(by.opponent()), GameResult::Win);
        }
    }

    #[test]
    fn max_moves_is_a_draw_for_both_sides() {
        // shogi-server's GameResultMaxMovesDraw, quoted by P-7: the
        // specification lists the status without fixing the result.
        let verdict = termination_of(Outcome::MaxMoves);

        assert_eq!(verdict.reason(), Reason::MaxMoves);
        assert_eq!(lines(verdict, None, Color::Black), ["#MAX_MOVES", "#DRAW"]);
    }

    #[test]
    fn only_the_outcomes_with_nothing_accepted_omit_the_echo() {
        // `#CENSORED` received nothing; `#TIME_UP` accepted nothing — a T-value
        // there would announce a deduction that never happened, on either of
        // its two paths.
        for outcome in [
            Outcome::Disconnected { by: Color::Black },
            Outcome::Timeout { by: Color::Black },
            Outcome::Timeout { by: Color::White },
        ] {
            assert!(!termination_of(outcome).is_echoed(), "{outcome:?}");
        }

        for outcome in [
            Outcome::Resignation { by: Color::Black },
            Outcome::Declaration {
                by: Color::Black,
                valid: true,
            },
            Outcome::Declaration {
                by: Color::Black,
                valid: false,
            },
            Outcome::IllegalMove { by: Color::Black },
            Outcome::Repetition,
            Outcome::PerpetualCheck { by: Color::Black },
            Outcome::MaxMoves,
        ] {
            assert!(termination_of(outcome).is_echoed(), "{outcome:?}");
        }
    }

    #[test]
    fn a_timeout_sends_the_two_lines_shogi_server_sends_and_no_echo() {
        // `GameResultTimeoutWin#process`: `"#TIME_UP\n#WIN\n"` and
        // `"#TIME_UP\n#LOSE\n"`, with nothing before either.
        let verdict = termination_of(Outcome::Timeout { by: Color::White });

        assert_eq!(lines(verdict, None, Color::White), ["#TIME_UP", "#LOSE"]);
        assert_eq!(lines(verdict, None, Color::Black), ["#TIME_UP", "#WIN"]);

        // And the same two even when the task holds a line it could echo: the
        // verdict, not the caller, decides that there is nothing to write.
        assert!(!verdict.is_echoed());
    }

    /// `%CHUDAN` is not supported, and the reference's own route says what that
    /// means: `board.rb`'s `handle_one_move` falls through to `:illegal` and the
    /// game ends against the sender. So the sender loses `#ILLEGAL_MOVE`, with
    /// the line echoed and its charge deducted, exactly as an illegal move is.
    #[test]
    fn a_chudan_from_the_side_to_move_ends_the_game_against_it_as_an_illegal_move() {
        let mut game = game(&buoy(&[]), &config());

        let verdict = game
            .suspend(Color::Black, 12)
            .expect("Black is to move and in time");

        assert_eq!(
            game.outcome(),
            Some(Outcome::IllegalMove { by: Color::Black })
        );
        assert_eq!(verdict.reason(), Reason::IllegalMove);
        assert_eq!(verdict.result(Color::Black), GameResult::Lose);
        assert_eq!(verdict.result(Color::White), GameResult::Win);
        assert_eq!(
            echoed_lines(verdict, "%CHUDAN", 12, Color::Black),
            ["%CHUDAN,T12", "#ILLEGAL_MOVE", "#LOSE"]
        );
        assert_eq!(
            echoed_lines(verdict, "%CHUDAN", 12, Color::White),
            ["%CHUDAN,T12", "#ILLEGAL_MOVE", "#WIN"]
        );

        // Written, so deducted (invariant 4) — and nothing is recorded, since
        // no move was played.
        assert_eq!(game.remaining(Color::Black), TOTAL - 12);
        assert!(game.moves().is_empty());
    }

    /// The wire is the illegal move's, with nothing added: one verdict, reached
    /// two ways. What differs between the two games is the line each client
    /// sent, and that is the client's text rather than anything this server
    /// spells.
    #[test]
    fn a_chudan_and_an_illegal_move_end_the_game_with_the_same_verdict() {
        let mut suspended = game(&buoy(&[]), &config());
        let suspension = suspended
            .suspend(Color::Black, 12)
            .expect("Black is to move and in time");

        let mut played = game(&buoy(&[]), &config());
        played
            .apply(Color::Black, board((7, 6), (7, 5)), 12)
            .expect_err("7f is empty");
        let illegal = termination_of(played.outcome().expect("the game ended"));

        assert_eq!(suspension, illegal);
        assert_eq!(suspended.outcome(), played.outcome());
        assert_eq!(game_clocks(&suspended), game_clocks(&played));
    }

    /// P-4's answer to a line from the side not to move, which alters no state
    /// and sends nothing. The reference reads a special move only from the
    /// current player, so there is nothing here to adjudicate.
    #[test]
    fn a_chudan_from_the_side_not_to_move_changes_nothing_at_all() {
        let mut game = game(&buoy(&collection_example()), &config());
        let before = game.clone();

        let rejection = game
            .suspend(Color::Black, 12)
            .expect_err("Black is not to move");

        assert_eq!(
            rejection,
            NotSuspendable::NotToMove {
                side: Color::Black,
                to_move: Color::White,
            }
        );
        assert_eq!(game, before);
        assert_eq!(game.outcome(), None);
    }

    /// The reference checks the timeout before it adjudicates anything, so a
    /// `%CHUDAN` charged past the allowance is a flag and is never adjudicated.
    #[test]
    fn a_chudan_that_arrived_after_the_flag_fell_is_a_time_up() {
        let mut game = game(&buoy(&[]), &config());
        let before = game.clone();

        let verdict = game
            .suspend(Color::Black, TOTAL)
            .expect("the game had not ended");

        assert_eq!(game.outcome(), Some(Outcome::Timeout { by: Color::Black }));
        assert_eq!(verdict.reason(), Reason::TimeUp);
        assert_eq!(verdict.result(Color::Black), GameResult::Lose);
        assert!(!verdict.is_echoed(), "a flag echoes nothing");
        assert_eq!(game_clocks(&game), game_clocks(&before));
    }

    #[test]
    fn a_finished_game_accepts_no_chudan() {
        let mut game = game(&buoy(&[]), &config());
        game.resign(Color::Black, 3).expect("still playing");
        let after = game.clone();

        assert_eq!(
            game.suspend(Color::Black, 1),
            Err(NotSuspendable::Finished(Finished {
                outcome: Outcome::Resignation { by: Color::Black },
            }))
        );
        assert_eq!(game, after);
    }

    #[test]
    fn a_disconnection_sends_two_lines_and_scores_against_the_side_that_went_away() {
        let verdict = termination_of(Outcome::Disconnected { by: Color::White });

        assert_eq!(lines(verdict, None, Color::White), ["#CENSORED", "#LOSE"]);
        assert_eq!(lines(verdict, None, Color::Black), ["#CENSORED", "#WIN"]);
    }

    #[test]
    fn an_illegal_move_termination_echoes_the_received_move_with_its_time() {
        let mut game = game(&buoy(&[]), &config());
        game.apply(Color::Black, board((7, 6), (7, 5)), 12)
            .expect_err("7f is empty");
        let verdict = termination_of(game.outcome().expect("the game ended"));

        let echo = MoveEcho {
            text: "+7675FU",
            consumed: 12,
        };
        assert_eq!(
            lines(verdict, Some(echo), Color::Black),
            ["+7675FU,T12", "#ILLEGAL_MOVE", "#LOSE"]
        );
    }

    #[test]
    fn a_game_played_from_a_buoy_start_numbers_its_plies_through_the_setup() {
        let mut game = game(&buoy(&collection_example()), &config());

        // White is to move after three setup plies.
        game.apply(Color::White, board((8, 3), (8, 4)), 5)
            .expect("8c8d is legal");
        game.apply(Color::Black, board((2, 6), (2, 5)), 7)
            .expect("2f2e is legal");

        assert_eq!(plies(&game), vec![1, 2, 3, 4, 5]);
        assert_eq!(
            game.moves()
                .iter()
                .map(|played| played.is_setup)
                .collect::<Vec<_>>(),
            vec![true, true, true, false, false]
        );
        assert_eq!(game.remaining(Color::Black), TOTAL - 7);
        assert_eq!(game.remaining(Color::White), TOTAL - 5);
    }

    #[test]
    fn the_client_rule_reaches_the_servers_clocks_after_every_settlement() {
        // P-5's completion criterion, as an executable fact: "a client applying
        // `remaining + increment − T` with no server-specific code reaches the
        // correct remaining time for both sides". Setup entries and played ones
        // alike, after each event rather than only at the end.
        let cfg = TimeConfig {
            increment: Some(Duration::from_secs(2)),
            ..config()
        };
        let mut game = game(&buoy(&collection_example()), &cfg);

        assert_eq!(client_ledger(&game, &cfg), game_clocks(&game));

        // White to move after three setup plies; the charges differ from the
        // increment in both directions, so a lost credit would show.
        for (side, mv, charged) in [
            (Color::White, board((8, 3), (8, 4)), 5),
            (Color::Black, board((2, 6), (2, 5)), 1),
            (Color::White, board((8, 4), (8, 5)), 0),
            (Color::Black, board((2, 5), (2, 4)), 30),
        ] {
            game.apply(side, mv, charged).expect("a legal continuation");

            assert_eq!(
                client_ledger(&game, &cfg),
                game_clocks(&game),
                "after {side:?} was charged {charged}"
            );
        }

        // And the numbers themselves: Black spent 31 over two moves and was
        // credited 4, White spent 5 and was credited 4.
        assert_eq!(game.remaining(Color::Black), TOTAL - 27);
        assert_eq!(game.remaining(Color::White), TOTAL - 1);
    }

    #[test]
    fn over_consumption_clamps_to_zero_and_the_game_goes_on_under_the_byoyomi() {
        // A short total with byoyomi behind it: the clock empties, but the
        // allowance is `remaining + byoyomi + increment`, so play continues.
        let cfg = TimeConfig {
            total: TimeUnit::Second.duration(10),
            byoyomi: Some(Duration::from_secs(30)),
            increment: Some(Duration::from_secs(2)),
            ..config()
        };
        let mut game = game(&buoy(&[]), &cfg);

        assert_eq!(game.allowance(Color::Black), Some(42));
        game.apply(Color::Black, board((7, 7), (7, 6)), 20)
            .expect("20 is inside an allowance of 42");
        // 10 + 2 − 20 is negative, and the floor settles it.
        assert_eq!(game.remaining(Color::Black), 0);

        // On byoyomi thereafter: the allowance is what byoyomi and the
        // increment supply, and a move inside it still plays.
        assert_eq!(game.allowance(Color::Black), Some(32));
        game.apply(Color::White, board((3, 3), (3, 4)), 1)
            .expect("White is well inside its own allowance");
        game.apply(Color::Black, board((8, 8), (2, 2)), 31)
            .expect("31 is inside an allowance of 32");

        assert_eq!(game.remaining(Color::Black), 0);
        assert_eq!(game.outcome(), None);
        // White's clock rose above the total it started with, which is what a
        // Fischer increment does.
        assert_eq!(game.remaining(Color::White), 11);
    }

    #[test]
    fn an_arrival_at_the_allowance_flags_and_leaves_the_game_untouched() {
        let mut game = game(&buoy(&[]), &config());
        let before = game.clone();

        // No byoyomi and no increment, so the allowance is the clock itself —
        // and consuming it exactly is a flag, not a move.
        assert_eq!(game.allowance(Color::Black), Some(TOTAL));
        let rejection = game
            .apply(Color::Black, board((7, 7), (7, 6)), TOTAL)
            .expect_err("the allowance was consumed exactly");

        assert_eq!(
            rejection,
            Rejected::Timeout {
                by: Color::Black,
                charged: TOTAL,
                allowance: TOTAL,
            }
        );
        assert_eq!(game.outcome(), Some(Outcome::Timeout { by: Color::Black }));
        assert_eq!(
            termination_of(Outcome::Timeout { by: Color::Black }).reason(),
            Reason::TimeUp
        );

        // The move never existed: no history entry, no position change, and
        // neither clock moved — nothing was credited and nothing deducted.
        assert!(game.moves().is_empty());
        assert_eq!(game.position(), before.position());
        assert_eq!(game_clocks(&game), game_clocks(&before));

        // And the game is over, on the same terms as any other termination.
        assert_eq!(
            game.apply(Color::White, board((3, 3), (3, 4)), 1),
            Err(Rejected::Finished(Finished {
                outcome: Outcome::Timeout { by: Color::Black },
            }))
        );
    }

    #[test]
    fn one_unit_short_of_the_allowance_is_an_ordinary_move() {
        let mut game = game(&buoy(&[]), &config());

        game.apply(Color::Black, board((7, 7), (7, 6)), TOTAL - 1)
            .expect("one unit short of the allowance is a move");

        assert_eq!(game.outcome(), None);
        assert_eq!(game.remaining(Color::Black), 1);
        assert_eq!(game.moves().len(), 1);
    }

    #[test]
    fn a_move_from_the_side_not_to_move_is_a_protocol_error_however_large_its_charge() {
        // The wrong-side check stays ahead of the flag, as it is in the
        // reference: the player whose clock is not running cannot make it fall.
        let mut game = game(&buoy(&collection_example()), &config());
        let before = game.clone();

        let rejection = game
            .apply(Color::Black, board((2, 6), (2, 5)), u32::MAX)
            .expect_err("Black is not to move");

        assert_eq!(
            rejection,
            Rejected::NotToMove {
                side: Color::Black,
                to_move: Color::White,
            }
        );
        assert_eq!(game, before);
    }

    #[test]
    fn an_untimed_configuration_never_flags() {
        // `total = 0` with no byoyomi and no increment: shogi-server's guard
        // makes this an untimed server, not one that flags on move one.
        let cfg = TimeConfig {
            total: Duration::ZERO,
            ..config()
        };
        let mut game = game(&buoy(&[]), &cfg);

        assert_eq!(game.allowance(Color::Black), None);
        game.apply(Color::Black, board((7, 7), (7, 6)), u32::MAX)
            .expect("nothing bounds a turn here");

        assert_eq!(game.outcome(), None);
        assert_eq!(game.remaining(Color::Black), 0);

        // Nor does a resignation from such a game become a timeout.
        let verdict = game.resign(Color::White, u32::MAX).expect("still playing");
        assert_eq!(verdict.reason(), Reason::Resign);
    }

    #[test]
    fn a_toryo_from_the_side_to_move_that_arrived_too_late_is_a_flag() {
        let mut game = game(&buoy(&[]), &config());
        let before = game.clone();

        let verdict = game
            .resign(Color::Black, TOTAL)
            .expect("the game had not ended");

        // The reference decides the timeout before it reads what arrived, and a
        // declaration is not exempt.
        assert_eq!(game.outcome(), Some(Outcome::Timeout { by: Color::Black }));
        assert_eq!(verdict.reason(), Reason::TimeUp);
        assert_eq!(verdict.result(Color::Black), GameResult::Lose);
        assert_eq!(verdict.result(Color::White), GameResult::Win);
        // Nothing settled: the charge is not deducted after the flag.
        assert_eq!(game_clocks(&game), game_clocks(&before));
    }

    #[test]
    fn a_toryo_from_the_side_not_to_move_resigns_and_never_flags() {
        // Black is to move, so White's clock is not running: no allowance
        // applies to it, whatever the line was charged.
        let mut game = game(&buoy(&[]), &config());

        let verdict = game
            .resign(Color::White, u32::MAX)
            .expect("the game had not ended");

        assert_eq!(
            game.outcome(),
            Some(Outcome::Resignation { by: Color::White })
        );
        assert_eq!(verdict.reason(), Reason::Resign);
        // Settled, not skipped: a resignation's T-value is written, so it is
        // deducted — and the floor is what an outsized one meets.
        assert_eq!(game.remaining(Color::White), 0);
        assert_eq!(game.remaining(Color::Black), TOTAL);
    }

    /// P-7 and the reference's own strings: `%KACHI`, `#JISHOGI`, the result —
    /// the echo bare, with no consumption time.
    #[test]
    fn a_declaration_that_holds_wins_with_the_reference_exchange() {
        let mut game = game(&StartSpec::Board(entered(Color::Black, 10)), &config());

        let verdict = game
            .declare(Color::Black, 3)
            .expect("the game is still being played");

        assert_eq!(
            game.outcome(),
            Some(Outcome::Declaration {
                by: Color::Black,
                valid: true,
            })
        );
        assert_eq!(verdict.reason(), Reason::Jishogi);
        assert_eq!(verdict.result(Color::Black), GameResult::Win);
        assert_eq!(verdict.result(Color::White), GameResult::Lose);

        assert_eq!(
            echoed_lines(verdict, "%KACHI", 3, Color::Black),
            ["%KACHI", "#JISHOGI", "#WIN"]
        );
        assert_eq!(
            echoed_lines(verdict, "%KACHI", 3, Color::White),
            ["%KACHI", "#JISHOGI", "#LOSE"]
        );
    }

    /// The declaration is the losing act, not an ignored line.
    #[test]
    fn a_declaration_that_does_not_hold_ends_the_game_against_the_declarer() {
        // Hirate: no king has entered anything.
        let mut game = game(&buoy(&[]), &config());

        let verdict = game
            .declare(Color::Black, 3)
            .expect("the game is still being played");

        assert_eq!(
            game.outcome(),
            Some(Outcome::Declaration {
                by: Color::Black,
                valid: false,
            })
        );
        assert_eq!(verdict.reason(), Reason::IllegalMove);
        assert_eq!(
            echoed_lines(verdict, "%KACHI", 3, Color::Black),
            ["%KACHI", "#ILLEGAL_MOVE", "#LOSE"]
        );
        assert_eq!(
            echoed_lines(verdict, "%KACHI", 3, Color::White),
            ["%KACHI", "#ILLEGAL_MOVE", "#WIN"]
        );
    }

    /// The two thresholds, through a whole game rather than through the
    /// adjudicator alone: the same twenty-seven points wins for gote and is a
    /// point short for sente.
    #[test]
    fn the_declarers_own_threshold_decides_the_game() {
        for (declarer, hand_pawns, valid) in [
            (Color::Black, 10, true),
            (Color::Black, 9, false),
            (Color::White, 9, true),
            (Color::White, 8, false),
        ] {
            let start = StartSpec::Board(entered(declarer, hand_pawns));
            let mut game = game(&start, &config());

            game.declare(declarer, 1).expect("still playing");

            assert_eq!(
                game.outcome(),
                Some(Outcome::Declaration {
                    by: declarer,
                    valid,
                }),
                "{declarer:?} holding {hand_pawns} pawns"
            );
        }
    }

    /// A declaration advances no move count and moves no clock. The echo carries
    /// no `,T`, so a deducted charge would be a clock movement no client was told
    /// of — the same rule an accepted resignation reads the other way round.
    #[test]
    fn a_declaration_records_no_move_and_settles_no_clock() {
        let cfg = TimeConfig {
            increment: Some(Duration::from_secs(2)),
            ..config()
        };
        let mut game = limited(&buoy(&collection_example()), &cfg, Some(6));
        let before = game.clone();

        // Three setup plies, so White holds the turn.
        game.declare(Color::White, 30).expect("still playing");

        assert_eq!(game.moves(), before.moves());
        assert_eq!(game.position(), before.position());
        assert_eq!(game_clocks(&game), game_clocks(&before));
        assert_eq!(client_ledger(&game, &cfg), game_clocks(&game));
    }

    /// P-5 ahead of the adjudication, exactly as it is ahead of a resignation:
    /// the flag fell before the declaration existed, so nothing is judged.
    #[test]
    fn a_kachi_from_the_side_to_move_that_arrived_too_late_is_a_flag() {
        // A position that would win outright, so what decides this is the clock
        // and not the board.
        let mut game = game(&StartSpec::Board(entered(Color::Black, 10)), &config());
        let before = game.clone();

        let verdict = game
            .declare(Color::Black, TOTAL)
            .expect("the game had not ended");

        assert_eq!(game.outcome(), Some(Outcome::Timeout { by: Color::Black }));
        assert_eq!(verdict.reason(), Reason::TimeUp);
        assert_eq!(verdict.result(Color::Black), GameResult::Lose);
        assert_eq!(verdict.result(Color::White), GameResult::Win);
        assert!(!verdict.is_echoed(), "a flag echoes nothing");
        assert_eq!(game_clocks(&game), game_clocks(&before));
    }

    /// A claim about the position on one's own turn: out of turn there is
    /// nothing to judge, and P-4's answer to a move from the wrong side is the
    /// one that applies. Not a failed declaration — that would end the game
    /// against a player whose claim was never read.
    #[test]
    fn a_declaration_from_the_side_not_to_move_changes_nothing_at_all() {
        let mut game = game(&buoy(&collection_example()), &config());
        let before = game.clone();

        let rejection = game
            .declare(Color::Black, 12)
            .expect_err("Black is not to move");

        assert_eq!(
            rejection,
            NotDeclarable::NotToMove {
                side: Color::Black,
                to_move: Color::White,
            }
        );
        assert_eq!(game, before);
        assert_eq!(game.outcome(), None);
    }

    /// The board a declaration is judged against is the one the **setup**
    /// built, hirate being one value of that path rather than a case of its own.
    ///
    /// The setup is what makes this test possible at all: after O-1's published
    /// three-ply entry it is White's turn, so White's declaration is judged and
    /// Black's is out of turn. From the hirate the same buoy is replayed from,
    /// both answers would be the other way round.
    #[test]
    fn a_declaration_is_judged_against_the_position_the_setup_built() {
        let start = buoy(&collection_example());
        let mut game = game(&start, &config());
        assert_ne!(game.position(), &Position::hirate());

        assert_eq!(
            game.declare(Color::Black, 1),
            Err(NotDeclarable::NotToMove {
                side: Color::Black,
                to_move: Color::White,
            })
        );

        let verdict = game.declare(Color::White, 1).expect("White is to move");

        assert_eq!(
            game.outcome(),
            Some(Outcome::Declaration {
                by: Color::White,
                valid: false,
            })
        );
        assert_eq!(verdict.reason(), Reason::IllegalMove);
        assert_eq!(verdict.result(Color::White), GameResult::Lose);
    }

    #[test]
    fn a_finished_game_accepts_no_declaration() {
        let mut game = game(&buoy(&[]), &config());
        game.resign(Color::Black, 3).expect("still playing");
        let after = game.clone();

        assert_eq!(
            game.declare(Color::Black, 1),
            Err(NotDeclarable::Finished(Finished {
                outcome: Outcome::Resignation { by: Color::Black },
            }))
        );
        assert_eq!(game, after);
    }

    /// And a declaration ends the game for every later event too: it consumes
    /// the game the way any other termination does.
    #[test]
    fn nothing_reaches_a_game_a_declaration_ended() {
        let mut game = game(&buoy(&[]), &config());
        let verdict = game.declare(Color::Black, 1).expect("still playing");
        assert_eq!(verdict.reason(), Reason::IllegalMove);
        let after = game.clone();

        let finished = Finished {
            outcome: Outcome::Declaration {
                by: Color::Black,
                valid: false,
            },
        };
        assert_eq!(
            game.apply(Color::White, board((3, 3), (3, 4)), 1),
            Err(Rejected::Finished(finished))
        );
        assert_eq!(game.resign(Color::White, 1), Err(finished));
        assert_eq!(game.declare(Color::White, 1), Err(finished.into()));
        assert_eq!(game.suspend(Color::White, 1), Err(finished.into()));
        assert_eq!(game.expired(u32::MAX), None);
        assert_eq!(game, after);
    }

    /// An untimed game cannot turn a declaration into a flag, whatever the
    /// measurement — the same guard a resignation has.
    #[test]
    fn an_untimed_configuration_never_flags_a_declaration() {
        let cfg = TimeConfig {
            total: Duration::ZERO,
            ..config()
        };
        let mut game = game(&StartSpec::Board(entered(Color::Black, 10)), &cfg);

        let verdict = game.declare(Color::Black, u32::MAX).expect("still playing");

        assert_eq!(verdict.reason(), Reason::Jishogi);
    }

    #[test]
    fn a_resignation_settles_the_increment_like_any_other_charged_event() {
        let cfg = TimeConfig {
            increment: Some(Duration::from_secs(2)),
            ..config()
        };
        let mut game = game(&buoy(&[]), &cfg);

        game.resign(Color::Black, 3).expect("still playing");

        assert_eq!(game.remaining(Color::Black), TOTAL + 2 - 3);
    }

    #[test]
    fn an_expiry_that_reaches_the_allowance_flags_the_side_to_move_and_settles_nothing() {
        let mut game = game(&buoy(&[]), &config());
        let before = game.clone();

        // Black is to move, so Black is who a deadline was armed for.
        let verdict = game
            .expired(TOTAL)
            .expect("the measured charge reaches the allowance");

        assert_eq!(verdict.reason(), Reason::TimeUp);
        assert_eq!(verdict.result(Color::Black), GameResult::Lose);
        assert_eq!(verdict.result(Color::White), GameResult::Win);
        assert!(!verdict.is_echoed());
        assert_eq!(game.outcome(), Some(Outcome::Timeout { by: Color::Black }));

        // Nothing was received, so nothing is recorded and no clock moved.
        assert_eq!(game.moves(), before.moves());
        assert_eq!(game.position(), before.position());
        assert_eq!(game_clocks(&game), game_clocks(&before));
    }

    #[test]
    fn an_expiry_short_of_the_allowance_leaves_the_turn_open() {
        // The verdict decides, not the timer: a caller that woke early rearms
        // rather than terminating, and the game is untouched.
        let mut game = game(&buoy(&[]), &config());
        let before = game.clone();

        assert_eq!(game.expired(TOTAL - 1), None);
        assert_eq!(game, before);

        // And the same numbers one unit later do flag.
        assert!(game.expired(TOTAL).is_some());
    }

    #[test]
    fn an_expiry_flags_whichever_side_the_position_says_is_to_move() {
        // Three setup plies, so White holds the turn and White's deadline is
        // the one that was armed.
        let mut game = game(&buoy(&collection_example()), &config());

        game.expired(TOTAL).expect("White's allowance is reached");

        assert_eq!(game.outcome(), Some(Outcome::Timeout { by: Color::White }));
    }

    #[test]
    fn an_untimed_game_never_expires_however_long_the_measurement() {
        // No deadline is armed at all where `allowance` is `None`, and the
        // predicate says so too — so a caller that armed one by mistake still
        // cannot end the game on it.
        let cfg = TimeConfig {
            total: Duration::ZERO,
            ..config()
        };
        let mut game = game(&buoy(&[]), &cfg);

        assert_eq!(game.allowance(Color::Black), None);
        assert_eq!(game.expired(u32::MAX), None);
        assert_eq!(game.outcome(), None);
    }

    #[test]
    fn a_finished_game_does_not_expire_a_second_time() {
        let mut game = game(&buoy(&[]), &config());
        game.resign(Color::Black, 3).expect("still playing");

        assert_eq!(game.expired(u32::MAX), None);
        assert_eq!(
            game.outcome(),
            Some(Outcome::Resignation { by: Color::Black })
        );
    }

    /// One whole king shuttle as a live sequence: the mover of each move and the
    /// move itself, alternating from Black.
    fn live_shuttle() -> Vec<(Color, Move)> {
        KING_SHUTTLE
            .into_iter()
            .enumerate()
            .map(|(index, mv)| (setup_mover(index), mv))
            .collect()
    }

    /// Plays `count` shuttles, charging a different number of units for every
    /// move, and answers the game's outcome after each.
    ///
    /// The varying charge is the point of the helper: a clock that differed at a
    /// recurrence would prevent the recurrence if a clock were part of a
    /// position's identity. P-6 says it is not, and every one of these games
    /// reaches its verdict with both clocks at values they have never held
    /// before.
    fn play_shuttles(game: &mut Game, count: usize) -> Vec<Option<Outcome>> {
        let mut outcomes = Vec::new();
        for cycle in 0..count {
            for (index, (side, mv)) in live_shuttle().into_iter().enumerate() {
                let charged = (cycle * 4 + index) as u32 + 1;
                game.apply(side, mv, charged)
                    .unwrap_or_else(|error| panic!("{mv:?} was rejected: {error}"));
                outcomes.push(game.outcome());
            }
        }

        outcomes
    }

    #[test]
    fn a_dummy_buoy_start_holds_two_occurrences_and_two_live_shuttles_finish_the_four() {
        // The transmitted shuttle returns exactly to hirate, so hirate occurs at
        // ply 0 and again at ply 4 before a client has moved at all (P-6). Two
        // live shuttles are then all that is left.
        let mut game = game(&buoy(&KING_SHUTTLE), &config());
        assert_eq!(game.moves().len(), 4);
        assert_eq!(game.position(), &Position::hirate());

        let outcomes = play_shuttles(&mut game, 2);

        assert_eq!(outcomes.len(), 8);
        assert!(
            outcomes[..7].iter().all(Option::is_none),
            "the game ended early: {outcomes:?}"
        );
        assert_eq!(outcomes[7], Some(Outcome::Repetition));

        // The repeating move is played, recorded, and charged: the game ended
        // *with* it. Four setup plies and eight live ones.
        assert_eq!(plies(&game), (1..=12).collect::<Vec<_>>());
        assert_eq!(game.moves().last().map(|played| played.t), Some(8));
        assert_eq!(game.position(), &Position::hirate());

        // And P-7's three lines, the echo being the relay of that move.
        let verdict = termination_of(Outcome::Repetition);
        let echo = MoveEcho {
            text: "-5251OU",
            consumed: 8,
        };
        for side in [Color::Black, Color::White] {
            assert_eq!(
                lines(verdict, Some(echo), side),
                ["-5251OU,T8", "#SENNICHITE", "#DRAW"]
            );
        }
    }

    #[test]
    fn a_start_with_no_setup_needs_a_third_shuttle_for_the_same_four_occurrences() {
        // The contrast that makes the setup traversal visible: the same live
        // moves, from a start whose sequence passes through nothing.
        let mut game = game(&buoy(&[]), &config());

        let outcomes = play_shuttles(&mut game, 2);
        assert!(
            outcomes.iter().all(Option::is_none),
            "eight live plies were enough: {outcomes:?}"
        );

        let outcomes = play_shuttles(&mut game, 1);
        assert_eq!(outcomes[3], Some(Outcome::Repetition));
        assert_eq!(game.moves().len(), 12);
    }

    #[test]
    fn a_repetition_reached_at_different_plies_and_clocks_still_ends_the_game() {
        // Ply and clock take no part in identity. The three recurrences of
        // hirate below fall at plies 4, 8 and 12, with both clocks at a
        // different value each time — and the game ends all the same.
        let mut game = game(&buoy(&[]), &config());
        let mut seen = Vec::new();

        for _ in 0..3 {
            play_shuttles(&mut game, 1);
            seen.push((
                game.moves().len(),
                game.remaining(Color::Black),
                game.remaining(Color::White),
            ));
        }

        assert_eq!(game.outcome(), Some(Outcome::Repetition));
        assert_eq!(seen.len(), 3);
        for (index, state) in seen.iter().enumerate() {
            assert!(
                !seen[..index].contains(state),
                "{state:?} was seen twice, so the recurrence was not a fresh one"
            );
        }
    }

    /// The perpetual-check board: a Black rook on 6h, a White king on 5a, and
    /// Black's own king out of the way on 9i, with Black to move.
    ///
    /// The same scenario `game/repetition.rs` builds, reduced to one color: what
    /// is under test here is the mapping from the rule's verdict to an
    /// [`Outcome`], not the rule.
    fn perpetual_start() -> StartSpec {
        let mut position = Position::hirate();
        for file in 1..=9 {
            for rank in 1..=9 {
                position.set_piece_at(sq(file, rank), None);
            }
        }
        for (file, rank, kind, color) in [
            (6, 8, PieceKind::Rook, Color::Black),
            (9, 9, PieceKind::King, Color::Black),
            (5, 1, PieceKind::King, Color::White),
        ] {
            position.set_piece_at(sq(file, rank), Some(Piece { kind, color }));
        }
        position.set_side_to_move(Color::Black);

        StartSpec::Board(position)
    }

    #[test]
    fn a_perpetual_check_ends_the_game_against_the_checker() {
        // The rook checks on every Black move while the White king shuttles out
        // of each check; the thirteenth ply is the fourth occurrence of the
        // checked position, with Black's streak count at the threshold.
        let mut game = game(&perpetual_start(), &config());

        let mut line = vec![board((6, 8), (5, 8))];
        for _ in 0..3 {
            line.extend([
                board((5, 1), (4, 1)),
                board((5, 8), (4, 8)),
                board((4, 1), (5, 1)),
                board((4, 8), (5, 8)),
            ]);
        }

        for (index, mv) in line.iter().enumerate() {
            let side = setup_mover(index);
            game.apply(side, *mv, 1)
                .unwrap_or_else(|error| panic!("{mv:?} at ply {index} was rejected: {error}"));
        }

        assert_eq!(game.moves().len(), 13);
        assert_eq!(
            game.outcome(),
            Some(Outcome::PerpetualCheck { by: Color::Black })
        );

        // The checker loses, and the escaping side wins.
        let verdict = termination_of(Outcome::PerpetualCheck { by: Color::Black });
        let echo = MoveEcho {
            text: "+4858HI",
            consumed: 1,
        };
        assert_eq!(
            lines(verdict, Some(echo), Color::Black),
            ["+4858HI,T1", "#OUTE_SENNICHITE", "#LOSE"]
        );
        assert_eq!(
            lines(verdict, Some(echo), Color::White),
            ["+4858HI,T1", "#OUTE_SENNICHITE", "#WIN"]
        );
    }

    /// Eight plies of an ordinary opening, alternating from Black and repeating
    /// no position: what a `Max_Moves` test needs is a game that ends for that
    /// reason and for no other.
    fn quiet_opening() -> Vec<(Color, Move)> {
        [
            board((7, 7), (7, 6)),
            board((3, 3), (3, 4)),
            board((2, 7), (2, 6)),
            board((8, 3), (8, 4)),
            board((2, 6), (2, 5)),
            board((8, 4), (8, 5)),
            board((6, 9), (7, 8)),
            board((4, 1), (3, 2)),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, mv)| (setup_mover(index), mv))
        .collect()
    }

    /// Plays `moves` in order, answering the game's outcome after each.
    fn play(game: &mut Game, moves: &[(Color, Move)]) -> Vec<Option<Outcome>> {
        moves
            .iter()
            .map(|&(side, mv)| {
                game.apply(side, mv, 1)
                    .unwrap_or_else(|error| panic!("{mv:?} was rejected: {error}"));
                game.outcome()
            })
            .collect()
    }

    #[test]
    fn the_move_that_reaches_max_moves_ends_the_game_and_the_one_before_it_does_not() {
        // Six plies of real play under a limit of six: the sixth is the move the
        // limit is reached on, and the game ends *with* it.
        let mut game = limited(&buoy(&[]), &config(), Some(6));
        let line = quiet_opening();

        let outcomes = play(&mut game, &line[..6]);

        assert!(
            outcomes[..5].iter().all(Option::is_none),
            "the game ended early: {outcomes:?}"
        );
        assert_eq!(outcomes[5], Some(Outcome::MaxMoves));

        // The reaching move is played, recorded and charged, exactly as a
        // repetition's is: P-7 echoes it before the two lines that follow.
        assert_eq!(plies(&game), (1..=6).collect::<Vec<_>>());
        assert_eq!(game.moves().last().map(|played| played.t), Some(1));
        let verdict = termination_of(Outcome::MaxMoves);
        let echo = MoveEcho {
            text: "-8485FU",
            consumed: 1,
        };
        for side in [Color::Black, Color::White] {
            assert_eq!(
                lines(verdict, Some(echo), side),
                ["-8485FU,T1", "#MAX_MOVES", "#DRAW"]
            );
        }
    }

    #[test]
    fn an_omitted_max_moves_restricts_nothing() {
        // The specification's "no restriction if omitted", against the very
        // sequence a limit of six ends above.
        let mut game = game(&buoy(&[]), &config());

        let outcomes = play(&mut game, &quiet_opening());

        assert!(
            outcomes.iter().all(Option::is_none),
            "an unlimited game ended: {outcomes:?}"
        );
        assert_eq!(game.moves().len(), 8);
    }

    #[test]
    fn a_setup_sequence_spends_the_limit_it_is_counted_under() {
        // PRD A2: the limit applies to the whole transmitted game, so a
        // three-ply setup under a limit of six leaves three plies to play.
        let mut game = limited(&buoy(&collection_example()), &config(), Some(6));
        assert_eq!(game.moves().len(), 3);

        // White is to move after three setup plies, which is where the quiet
        // opening's fourth entry picks up.
        let outcomes = play(&mut game, &quiet_opening()[3..6]);

        assert_eq!(outcomes, vec![None, None, Some(Outcome::MaxMoves)]);
        assert_eq!(game.moves().len(), 6);
        assert!(
            game.moves()[..3].iter().all(|played| played.is_setup),
            "the setup entries are what made the difference"
        );
    }

    #[test]
    fn a_repetition_outranks_the_limit_on_the_move_that_reaches_both() {
        // shogi-server checks sennichite before max_moves
        // (`board.rb#handle_one_move`), so the twelfth ply — the fourth
        // occurrence of hirate and the limit alike — is `#SENNICHITE`.
        let mut game = limited(&buoy(&KING_SHUTTLE), &config(), Some(12));

        play_shuttles(&mut game, 2);

        assert_eq!(game.moves().len(), 12);
        assert_eq!(game.outcome(), Some(Outcome::Repetition));
    }

    #[test]
    fn a_limit_one_ply_short_of_a_repetition_ends_the_same_game_at_the_limit() {
        // The contrast that shows the precedence above is about one move and not
        // about the two rules' order in general: the same game under a limit of
        // eleven never reaches its fourth occurrence.
        let mut game = limited(&buoy(&KING_SHUTTLE), &config(), Some(11));

        play_shuttles(&mut game, 1);
        assert_eq!(game.outcome(), None);
        play(&mut game, &live_shuttle()[..3]);

        assert_eq!(game.moves().len(), 11);
        assert_eq!(game.outcome(), Some(Outcome::MaxMoves));
    }

    #[test]
    fn a_game_that_reached_the_limit_accepts_no_further_event() {
        let mut game = limited(&buoy(&[]), &config(), Some(6));
        play(&mut game, &quiet_opening()[..6]);
        let after = game.clone();

        let (side, mv) = quiet_opening()[6];
        assert_eq!(
            game.apply(side, mv, 1),
            Err(Rejected::Finished(Finished {
                outcome: Outcome::MaxMoves,
            }))
        );
        assert_eq!(
            game.resign(side, 1),
            Err(Finished {
                outcome: Outcome::MaxMoves,
            })
        );
        assert_eq!(game.expired(u32::MAX), None);
        assert_eq!(game, after);
    }

    #[test]
    fn a_setup_that_already_meets_the_limit_ends_on_the_first_real_move() {
        // O-1 keeps a configured collection away from this, and the reading is
        // `config/validate.rs`'s own: "otherwise the game ends `#MAX_MOVES` at
        // move one". It needs no special case — `>=` over the whole history is
        // the whole of it.
        let mut game = limited(&buoy(&collection_example()), &config(), Some(2));
        assert_eq!(game.moves().len(), 3);
        assert_eq!(
            game.outcome(),
            None,
            "a game that has not started has no outcome"
        );

        let (side, mv) = quiet_opening()[3];
        game.apply(side, mv, 1).expect("8c8d is legal here");

        assert_eq!(game.outcome(), Some(Outcome::MaxMoves));
    }

    #[test]
    fn the_allowance_a_game_reports_is_the_clock_plus_byoyomi_and_increment() {
        // What the wiring slice arms its deadline with, asked of the game
        // rather than of the configuration — the point being that it moves as
        // the clock does.
        let cfg = TimeConfig {
            byoyomi: Some(Duration::from_secs(30)),
            increment: Some(Duration::from_secs(2)),
            ..config()
        };
        let mut game = game(&buoy(&[]), &cfg);

        assert_eq!(game.allowance(Color::Black), Some(TOTAL + 32));
        game.apply(Color::Black, board((7, 7), (7, 6)), 100)
            .expect("a legal move well inside the allowance");

        assert_eq!(game.allowance(Color::Black), Some(TOTAL - 98 + 32));
        assert_eq!(game.allowance(Color::White), Some(TOTAL + 32));
    }
}
