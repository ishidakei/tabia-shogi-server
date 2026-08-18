//! The rules: positions, moves, and how a game ends.
//!
//! This layer has nothing under it but `std` — no runtime, no I/O, no CSA
//! (invariant 1, the layering rule). The test is mechanical:
//! move legality must be testable with no socket, no tokio runtime, and no
//! protocol type. If anything here ever needs one, something has leaked
//! downward.
//!
//! Repetition is written in that vocabulary and in nothing else: positions,
//! moves, legality, and the start whose traversal it counts. So is the jishogi
//! declaration: [`declaration::holds`] answers whether a `%KACHI` holds and has
//! never heard of `#JISHOGI`.

pub mod declaration;
pub mod legality;
pub mod position;
pub mod repetition;
pub mod start_spec;
pub mod termination;

pub use legality::{Illegal, apply_move, in_check};
pub use position::{Color, Hand, HandKind, Move, NotInHand, Piece, PieceKind, Position, Square};
// `repetition::Verdict` is deliberately not flattened in here. The session layer
// has a `Verdict` of its own — what a *finished* game sends — and two bare
// `Verdict`s in scope read alike at a glance while meaning entirely different
// things. Callers spell this one `repetition::Verdict`.
pub use repetition::{PositionKey, RepetitionState};
pub use start_spec::{IllegalSetup, StartSpec};
pub use termination::Outcome;
