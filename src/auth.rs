//! Tokens: issued by `web`, verified by `session`, owned by neither.
//!
//! This layer has **no dependencies at all**.
//! `auth` is pure, like `game` — no storage, no runtime, no I/O — and its
//! `use` lines reach `std` and the three cryptography crates only.
//!
//! Purity is achieved by having the caller supply the stored hash rather than
//! having `auth` fetch it: `session/login.rs` asks `storage` for the row and
//! verifies, `web` generates and hands the hash to `storage`. The fetch is the
//! caller's; the cryptography is this module's.
//!
//! That split keeps two properties at once. There is exactly **one**
//! implementation of hashing and comparison — two is how a revoked token keeps
//! working on one path — and the module stays testable with no database, which
//! is what a security-critical module most needs.
//!
//! The hand-written [`Debug`](std::fmt::Debug) that keeps a token out of the
//! logs lives here, next to the type it protects.

pub mod token;

pub use token::{Token, TokenHash};
