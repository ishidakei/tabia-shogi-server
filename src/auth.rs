//! Credentials: tokens, issued by `web` and verified by `session` and owned by
//! neither, and the session cookie's signing key.
//!
//! `auth` is pure — no storage, no runtime, no I/O — and its `use` lines reach
//! `std` and the three cryptography crates only. The caller supplies the
//! stored hash rather than `auth` fetching it, so there is exactly one
//! implementation of hashing and comparison and the module stays testable with
//! no database.
//!
//! [`cookie`] is the same shape: the session store one layer up holds the
//! records and asks this module whether a cookie is one this server wrote.

pub mod cookie;
pub mod token;

pub use cookie::{CookieKey, opaque_id};
pub use token::{Token, TokenHash};
