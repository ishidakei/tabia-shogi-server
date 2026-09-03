//! The rules: positions, moves, and how a game ends.
//!
//! This layer has nothing under it but `std` — no runtime, no I/O, no CSA.
//! Move legality must stay testable with no socket, no tokio runtime and no
//! protocol type; if anything here ever needs one, something has leaked
//! downward.

pub mod declaration;
pub mod legality;
pub mod position;
pub mod repetition;
pub mod start_spec;
pub mod termination;

pub use legality::{Illegal, apply_move, in_check};
pub use position::{Color, Hand, HandKind, Move, NotInHand, Piece, PieceKind, Position, Square};
// `repetition::Verdict` is not re-exported: the session layer has a `Verdict`
// of its own, and two bare `Verdict`s in scope read alike while meaning
// different things.
pub use repetition::{PositionKey, RepetitionState};
pub use start_spec::{IllegalSetup, StartSpec};
pub use termination::Outcome;
