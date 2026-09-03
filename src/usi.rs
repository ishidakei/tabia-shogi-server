//! The USI protocol layer: the notation and the command vocabulary a shogi
//! engine speaking USI is driven with.
//!
//! [`csa`](crate::csa)'s counterpart on the other side of the server: that
//! layer faces the clients that connect over the listener, this one faces an
//! engine process the server itself runs. Both are edge encodings, so `game`
//! sees neither.
//!
//! - [`notation`] is a move's USI spelling, both ways, and the `position` line
//!   a game is fed.
//! - [`engine`] is the command vocabulary — the handshake, the `go` line's
//!   clock, and the `bestmove` answer — as values rather than as strings built
//!   at a call site.
//!
//! Nothing here runs a process or reads a socket; the half that does is
//! `session::bridge`.
//!
//! The reference is the USI specification as the shogi engine world uses it,
//! which is also the vocabulary `position startpos moves ...` collections are
//! written in, so the spelling a collection file uses and the spelling an
//! engine is fed are one spelling.

pub mod engine;
pub mod notation;

pub use engine::{
    BestMove, Clock, GameOver, ISREADY, QUIT, READYOK, STOP, Term, USI, USINEWGAME, USIOK, go_line,
    id_name, parse_bestmove, setoption_line,
};
pub use notation::{IllegibleMove, parse_move, position_line, render_move};
