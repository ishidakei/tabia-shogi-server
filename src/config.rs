//! Configuration: the TOML the operator writes, and every startup rule that
//! needs a value out of it.
//!
//! This layer has one job and one place to do it: validation lives here and
//! only here, so no validation code runs on the hot path. A set that loads is a
//! set every game can use.
//!
//! **One outgoing edge, to `game`.** A reduction is
//! `Reduction { side: Color, amount: Duration }`, and `Color` is a `game` type,
//! so `use` lines here reach `std`, `serde`, `toml`, and `crate::game` — and
//! nothing else. In particular not `csa`, which is why [`TimeUnit`] is a mirror
//! rather than the protocol layer's own type, and not `storage`: the entries
//! [`validate()`] checks are supplied by the caller, the same arrangement that
//! keeps `auth` pure and for the same payoff — the rules are testable with no
//! file and no loader.
//!
//! The two halves of O-1's validation are split by what they need to decide.
//! What a collection entry can violate on its own — grammar, legality from
//! hirate, the reserved `sfen` base — fails at load in `storage/collections.rs`.
//! What needs a configured value fails here.
//!
//! **A [`Violation`] stops the server and a [`Warning`] does not**, and the
//! difference is whether the configuration can be served at all. A combination
//! that is merely unlikely to be what the operator meant — a matchmaking
//! interval shorter than the idle delay in front of it — is served exactly as
//! written and said out loud once, at startup.

pub mod model;
pub mod timestamp;
pub mod validate;

pub use model::{
    AuthMode, Config, Limit, MatchmakingConfig, Reduction, ServerConfig, TimeConfig, TimeUnit,
    TlsConfig,
};
pub use timestamp::FirstRound;
pub use validate::{Rule, Violation, Warning, validate, warnings};
