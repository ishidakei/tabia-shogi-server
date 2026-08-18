//! The session layer: connection state, login, the Game Summary exchange,
//! clocks, and matchmaking.
//!
//! This layer may reach `game`, `auth`, `storage`,
//! `config`, and `csa`'s **parsed types** — not `csa`'s wire-format internals,
//! and not `web` or `services`, so that the protocol half of the server runs
//! with the web half absent.
//!
//! This is also where the two `TimeUnit` mirrors meet. `csa` may not depend on
//! `config` and `config` is given no edge to `csa`, so each layer names its own
//! three variants and the conversion between a configured [`Duration`] and a
//! counted wire unit happens here — once, in [`clock`], which is the same
//! reasoning [`csa::TimeSettings`] records for the `Time` block as a whole.
//!
//! It is also where a `game::Outcome` meets the lines it is sent as, for the
//! same reason: `game` may not name a `csa::Reason` and `csa` decides no rules,
//! so [`game_task::termination_of`] is P-7's mapping, in the one layer allowed
//! to see both.
//!
//! The layer's six pure pieces are [`clock`]'s arithmetic —
//! the numbers the game task will write — [`login`]'s decision, the rule the
//! connection task will apply to a `LOGIN`, [`matchmaker`]'s round, the
//! pairings and the `Game_ID` a scheduled round will hand to a summary,
//! [`agreement`]'s state, what each `AGREE` means to an offered pairing and
//! when the offer has outlived its timeout, [`game_task`]'s [`Game`], the
//! history a start seeds and what a move, a resignation, a declaration, and an
//! outcome do to it, and [`handler`]'s state machine, which says where every line goes and
//! is what routes to the other five. Every one of them is testable with no
//! socket and no timer, and none of them names tokio.
//!
//! Four modules carry the runtime that runs them, and they add no rule of
//! their own: [`server`] binds the listener and owns the registry, the waiting
//! pool, and the schedule the matchmaking rounds run on; [`transport`] is what a socket is wrapped
//! in and what is set on it before a session sees it (P-8); [`connection`] is
//! the three tasks one socket costs; and [`pairing`] is the per-game task, from
//! a `Game_Summary` to a termination. The division is the concurrency model —
//! one owner per piece of state, `mpsc` between them, and no lock anywhere for
//! a game task to contend on.
//!
//! [`Duration`]: std::time::Duration
//! [`csa::TimeSettings`]: crate::csa::TimeSettings

pub mod agreement;
pub mod clock;
pub mod connection;
pub mod game_task;
pub mod handler;
pub mod login;
pub mod matchmaker;
pub mod pairing;
pub mod server;
pub mod transport;

pub use agreement::{AGREEMENT_TIMEOUT, Agreed, Agreement, expired};
pub use clock::{
    KING_SHUTTLE, charged_units, effective_setup, flag_after, increment_units, setup_t_values,
    total_units, turn_allowance,
};
pub use connection::{Control, Outbound};
pub use game_task::{
    Echoing, Finished, Game, NotDeclarable, NotSuspendable, PlayedMove, Rejected, Scoring, Verdict,
    termination_of,
};
pub use handler::{
    AgreementCommand, DisconnectAnswer, Disposition, Edge, GameCommand, SessionState, on_disconnect,
};
pub use login::{ExistingSession, IDLE_TAKEOVER, LoginDecision, decide};
pub use matchmaker::{
    AccountId, EngineId, Pairing, PastResult, PreviousGame, Waiting, draw_start, mint_game_id,
    pair_round,
};
pub use pairing::{GameMessage, Player, Proposal};
pub use server::{Server, SessionId, serve};
pub use transport::{Transport, tune};
