//! Configuration: the TOML the operator writes, and every startup rule that
//! needs a value out of it.
//!
//! Validation lives here and only here, so no validation code runs on the hot
//! path: a set that loads is a set every game can use.
//!
//! One outgoing edge, to `game`. `use` lines here reach `std`, `serde`, `toml`
//! and `crate::game`, and nothing else — in particular not `csa`, which is why
//! [`TimeUnit`] is a mirror rather than the protocol layer's own type, and not
//! `storage`: the entries [`validate()`] checks are supplied by the caller.
//!
//! What a collection entry can violate on its own — grammar, legality from
//! hirate, the reserved `sfen` base — fails at load in
//! `storage/collections.rs`. What needs a configured value fails here.
//!
//! A [`Violation`] stops the server and a [`Warning`] does not: a combination
//! that is merely unlikely to be what the operator meant is served exactly as
//! written and said out loud once, at startup.

pub mod model;
pub mod timestamp;
pub mod validate;

pub use model::{
    AccountsConfig, AuthMode, Config, CsaConfig, Lifecycle, Limit, MatchmakingConfig, OauthConfig,
    PresetEngine, PresetEngines, Protocol, RatingsConfig, Reduction, TimeConfig, TimeUnit,
    TlsConfig, UsiOptionValue, UsiOptions, WebConfig,
};
pub use timestamp::FirstRound;
pub use validate::{Rule, Violation, Warning, validate, warnings};
