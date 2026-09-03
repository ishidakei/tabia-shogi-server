//! The session layer: connection state, login, the Game Summary exchange,
//! clocks, and matchmaking.
//!
//! This layer may reach `game`, `auth`, `storage`, `config` and `csa`'s parsed
//! types — not `csa`'s wire-format internals, and not `web` or `services`, so
//! that the protocol half of the server runs with the web half absent.
//!
//! It is where the two `TimeUnit` mirrors meet, since `csa` may not depend on
//! `config` and `config` is given no edge to `csa`: the conversion between a
//! configured `Duration` and a counted wire unit happens once, in [`clock`].
//! It is where a `game::Outcome` meets the lines it is sent as, for the same
//! reason — [`game_task::termination_of`] is that mapping.
//!
//! The layer's pure pieces are [`clock`]'s arithmetic, [`login`]'s decision,
//! [`matchmaker`]'s round, [`agreement`]'s state, [`game_task`]'s [`Game`] and
//! [`handler`]'s state machine. Every one of them is testable with no socket
//! and no timer, and none of them names tokio.
//!
//! [`stamp`] is the layer's only wall clock, and not on the clock path: a
//! `Game_ID`'s date and a record's two timestamps are identifiers and header
//! lines, while every measured duration in this crate is a monotonic
//! `Instant`.
//!
//! Five modules carry the runtime and add no rule of their own: [`server`]
//! binds the listener and owns the registry, the waiting pool and the
//! schedule; [`transport`] is what a socket is wrapped in; [`connection`] is
//! the three tasks one socket costs; [`pairing`] is the per-game task; and
//! [`bridge`] is what a registered USI engine costs — a process at one end and
//! an ordinary CSA client at the other. The division is the concurrency model:
//! one owner per piece of state, `mpsc` between them, and no lock anywhere for
//! a game task to contend on.

pub mod agreement;
pub mod bridge;
pub mod clock;
pub mod connection;
pub mod game_task;
pub mod handler;
pub mod login;
pub mod matchmaker;
pub mod pairing;
pub mod presets;
pub mod server;
pub mod tags;
pub mod transport;

pub use agreement::{AGREEMENT_TIMEOUT, Agreed, Agreement, expired};
pub use clock::{
    KING_SHUTTLE, charged_units, effective_setup, flag_after, increment_units, setup_t_values,
    total_units, turn_allowance,
};
pub use connection::{Control, Outbound};
pub use game_task::{
    Echoing, Finished, Game, NotDeclarable, NotSuspendable, PlayedMove, Rejected, Scoring, Verdict,
    record_ending, termination_of,
};
pub use handler::{
    AgreementCommand, DisconnectAnswer, Disposition, Edge, GameCommand, SessionState, on_disconnect,
};
pub use login::{ExistingSession, IDLE_TAKEOVER, LoginDecision, decide};
pub use matchmaker::{
    AccountId, EngineId, Pairing, PastResult, PositionStats, PreviousGame, Waiting, mint_game_id,
    pair_round, select_start,
};
pub use pairing::{GameMessage, Player, Proposal};
// Renamed on the way out: `State` and `plan` say nothing on their own at this
// level.
pub use presets::{
    Kind as PresetKind, MAX_PLAYING as MAX_PLAYING_PRESETS, Plan as PresetPlan, Presets,
    Standing as PresetStanding, State as PresetState, plan as plan_presets,
};
pub use server::{Server, SessionId, serve};
// The wall-clock stamps a record header and a `Game_ID` carry. The module
// itself is `crate::stamp`, because a backup filename needs the same calendar
// and the storage layer may not name this one.
pub use crate::stamp::{rfc3339, stamp, utc_date};
pub use tags::{start_category, time_category};
pub use transport::{Transport, tune};
